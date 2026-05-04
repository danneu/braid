# Plan: surface luksDump failures during `braid discover`

## Context

`braid discover` exists so that a user who has lost `pool.json` can plug
their drives into a fresh box and rebuild membership from the LUKS labels
on disk. Its job is to make labeled disks **visible**, even unhealthy
ones -- that is the recovery story.

Today, `discover_from_dir` in `cli/src/discover.rs` has three silent
or untestable warning channels:

1. **luksDump parser failures** (`discover.rs:94-97`) -- `Err(_) => continue`
   silently drops a labeled disk whose `cryptsetup luksDump` rejected
   the header (`ParseError::CommandFailed`) or whose output the parser
   could not read (`MissingField` / `UnexpectedValue`). The reviewer's
   primary finding.
2. **LUKS1 inline warning** (`discover.rs:98-101`) -- `eprintln!` to
   stderr, no test pins it.
3. **Canonicalize-failure inline warning** (`discover.rs:114-117`) --
   `eprintln!` to stderr when `resolver.canonicalize` fails on a
   labeled symlink. No test pins the warning text either.

End-user impact (`cli/src/main.rs:696-727`): a silently-dropped
candidate makes `members` empty; the user sees `"no braid-labeled LUKS
devices found"`, exits 1, with no signal that one of their drives is
present-but-broken.

Sibling code in `cli/src/probe.rs:130-150` already takes the opposite
stance with an explicit comment: *"the gateway must not lie about a
configured disk's state."* Discover diverges from that, and the
divergence is the bug.

## What is **already correct** in the current file

The first plan revision asked for runner-level errors to propagate
hard. They already do:

- `discover.rs:78-83` -- `runner.run(CryptsetupIsLuks { .. })?` then
  silent skip on non-zero exit. Correct: failing isLuks just means
  "not a LUKS device," which is the expected path for every non-LUKS
  entry in `/dev/disk/by-id`.
- `discover.rs:90-92` -- `runner.run(CryptsetupLuksDumpText { .. })?`.
  Correct: a `CmdError` from luksDump means cryptsetup itself broke,
  not "this device is broken." That belongs as a hard error.
- Tests `discover_propagates_runner_error_at_isluks`
  (`discover.rs:309-355`) and
  `discover_propagates_runner_error_at_luksdump`
  (`discover.rs:357-411`) already pin both propagation paths.

So this plan does **not** touch `?` propagation or add new
runner-error tests -- that work is already merged.

## Design

### 1. Structured warning enum

Add to `cli/src/discover.rs`:

```rust
#[derive(Debug, PartialEq, Eq)]
pub enum DiscoverWarning {
    LuksDumpFailed { path: String, exit_code: i32, stderr: String },
    LuksDumpUnparseable { path: String, detail: String },
    UnsupportedLuksVersion { path: String, version: u32 },
    CannotCanonicalize { path: String, detail: String },
}
```

Implement `Display` -- this is the single source of truth for
user-facing wording, replacing both inline `eprintln!`s currently in
the loop:

```text
LuksDumpFailed         -> "skipping {path}: luksDump failed (exit {exit_code}) -- {stderr trimmed}"
LuksDumpUnparseable    -> "skipping {path}: luksDump output unparseable -- {detail}"
UnsupportedLuksVersion -> "skipping {path}: LUKS{version} (braid requires LUKS2)"
CannotCanonicalize     -> "skipping {path}: cannot canonicalize -- {detail}"
```

`detail: String` (rather than the underlying `ParseError` / `io::Error`)
keeps the `PartialEq` derive trivial and lets tests `matches!` on
variant + path without matching every nested field. The wording stays
consistent with the existing inline messages so anyone grepping logs
or screenshots before/after the patch sees the same human text.

### 2. Outcome struct

Change return type of both public entry points:

```rust
pub struct DiscoverOutcome {
    pub members: BTreeMap<String, ByIdPath>,
    pub warnings: Vec<DiscoverWarning>,
}

pub fn discover_pool_members<R: CommandRunner>(...) -> Result<DiscoverOutcome, DiscoverError>;
fn discover_from_dir<R: CommandRunner>(...) -> Result<DiscoverOutcome, DiscoverError>;
```

This forces every caller to acknowledge the warnings vec. main.rs
gets one match-arm change (below); tests gain `.warnings` for
inspection.

### 3. Loop edits in `discover_from_dir`

**Per-device parser errors -> warn-and-skip with variant split**
(replacing `discover.rs:94-97`):

```rust
let version = match parse_cryptsetup_luks_version(&dump_raw) {
    Ok(out) => out.version,
    Err(ParseError::CommandFailed { exit_code, stderr, .. }) => {
        warnings.push(DiscoverWarning::LuksDumpFailed {
            path: path_str.clone(),
            exit_code,
            stderr,
        });
        continue;
    }
    Err(e) => {
        warnings.push(DiscoverWarning::LuksDumpUnparseable {
            path: path_str.clone(),
            detail: e.to_string(),
        });
        continue;
    }
};
```

The variant split is finding 3 from the previous review:
`CommandFailed` (cryptsetup refused the header) is one warning;
`MissingField` / `UnexpectedValue` (parser-vs-tool drift) is another.
Requires `use crate::parse::ParseError;` at the top.

**LUKS1 case folded into the warning enum** (replacing
`discover.rs:98-101`):

```rust
if version != 2 {
    warnings.push(DiscoverWarning::UnsupportedLuksVersion {
        path: path_str.clone(),
        version,
    });
    continue;
}
```

**Canonicalize failure folded into the warning enum** (replacing
`discover.rs:112-118`):

```rust
let canonical = match resolver.canonicalize(&path_str) {
    Ok(c) => c,
    Err(e) => {
        warnings.push(DiscoverWarning::CannotCanonicalize {
            path: path_str.clone(),
            detail: e.to_string(),
        });
        continue;
    }
};
```

**Label parser stays as-is** (`discover.rs:103-105`). It only `Err`s on
non-zero `exit_status`, which the version parser already short-circuited
on -- so the `.ok().and_then(...)` is dead-code for the broken-luksDump
scenario. A genuine missing `Label:` line returns `Ok(label: None)`
and is silently dropped, but that's a separate defect (LUKS2 with no
Label) with different motivation. Out of scope.

### 4. Caller update in `main.rs:696-727`

Switch from matching on `members` to matching on `outcome`:

```rust
match braid_cli::discover::discover_pool_members(&runner) {
    Ok(outcome) => {
        for w in &outcome.warnings {
            eprintln!("warning: {w}");
        }
        if outcome.members.is_empty() {
            eprintln!("no braid-labeled LUKS devices found");
            std::process::exit(1);
        }
        // ... existing print loop and write-to-pool.json flow uses outcome.members
    }
    Err(e) => { ... }  // unchanged
}
```

Warnings print **before** the empty-members check, so a recovery user
who plugs in only a broken disk sees both the per-disk warning AND
the "no labeled disks" summary.

## Tests

### LabelMap extension (`discover.rs:214-291`)

The mock can already return non-zero exit for *unknown* paths
(line 273-278), but cannot return non-zero exit for a *known* path with
a *real* stderr message. Add one bundled builder:

```rust
struct LabelMap {
    labels: HashMap<String, String>,
    versions: HashMap<String, u32>,
    dump_responses: HashMap<String, RawCommandOutput>,  // new
    calls: Mutex<Vec<(String, String)>>,
}

/// Override the entire luksDump response for a path. Used to inject
/// realistic failure modes (non-zero exit + stderr) or unparseable
/// stdout. Takes precedence over the synthesized default.
fn with_dump_response(mut self, path: &str, response: RawCommandOutput) -> Self {
    self.dump_responses.insert(path.to_string(), response);
    self
}
```

Consulted in the `CryptsetupLuksDumpText` arm of `run` (lines 258-279):
if the path is in `dump_responses`, clone and return that; otherwise
fall through to the existing label-lookup default. This preserves the
existing-tests' "absent mock = predictable default" property
(consistent with the
`feedback_mockrunner_absent_mocks.md` memory) -- new override is
opt-in.

Bundled (one builder taking a full `RawCommandOutput`) rather than three
separate `with_dump_exit` / `with_dump_stdout` / `with_dump_stderr`
because the test scenarios always need to set exit + stderr together
to mean anything, and the bundled form makes the test read like the
real cryptsetup output it represents.

### Test 1 (new): `discover_warns_when_labeled_disk_fails_luksdump`

Pins the primary bug. Two LUKS-labeled symlinks in the by-id dir:

- `ata-MODERN_DISK` -- healthy, default LabelMap response (exit 0,
  synthesized LUKS2 stdout with `Label: braid-modern`).
- `ata-BROKEN_DISK` -- labeled `braid-broken` so `isLuks` returns 0,
  but `with_dump_response(broken_path, RawCommandOutput { exit_status: 1,
  stderr: "Device /dev/foo is not a valid LUKS device.\n".into(), .. })`
  forces luksDump to fail.

Assertions:

```rust
assert_eq!(outcome.members.len(), 1);
assert!(outcome.members.contains_key("modern"));
assert_eq!(outcome.warnings.len(), 1);
let w = &outcome.warnings[0];
assert!(matches!(w, DiscoverWarning::LuksDumpFailed { exit_code: 1, .. }));
let DiscoverWarning::LuksDumpFailed { path, stderr, .. } = w else { unreachable!() };
assert!(path.ends_with("ata-BROKEN_DISK"));
assert!(stderr.contains("not a valid LUKS device"));
```

**Fails before the patch** because today the broken disk silently
disappears with no warning recorded -- and the bare-`BTreeMap` return
has nowhere to record one even if we wanted to.

### Test 2 (new): `discover_warns_on_unparseable_luksdump_output`

Pins the variant split. `with_dump_response(path, RawCommandOutput {
exit_status: 0, stdout: "LUKS header information\nUUID: foo\n".into(),
stderr: String::new(), .. })` -- valid exit, but no `Version:` field.
Assert exactly one
`DiscoverWarning::LuksDumpUnparseable { path, detail }` whose detail
mentions the missing field. Distinguishes the
`MissingField` / `UnexpectedValue` path from `CommandFailed`.

### Test 3 (upgrade): `discover_skips_luks1_disk` (`discover.rs:467-503`)

Existing test asserts only membership. Add:

```rust
assert_eq!(outcome.warnings.len(), 1);
assert!(matches!(
    &outcome.warnings[0],
    DiscoverWarning::UnsupportedLuksVersion { path, version: 1 }
        if path.ends_with("ata-LEGACY_DISK")
));
```

Retroactively pins the LUKS1 warning that was previously a
side-effect-no-test-verified.

### Test 4 (upgrade): `discover_skips_entry_when_canonicalize_fails` (`discover.rs:645-672`)

Existing test asserts only membership. Add:

```rust
assert_eq!(outcome.warnings.len(), 1);
assert!(matches!(
    &outcome.warnings[0],
    DiscoverWarning::CannotCanonicalize { path, .. }
        if path.ends_with("ata-DANGLING")
));
```

Retroactively pins the canonicalize warning.

### Existing test mechanical updates

The return-type change requires every test that currently does
`let members = discover_from_dir(...)?` to update to
`let outcome = discover_from_dir(...)?` and reference
`outcome.members[..]` instead of `members[..]`. Affected sites:
`discover.rs:432-434`, `489-490`, `513-516`, and similar inside the
remaining tests in lines 514-672. Mechanical search-and-replace; no
behavioral change. Each updated test should also assert
`outcome.warnings.is_empty()` (where applicable) so a future regression
that emits a spurious warning is caught.

## Files modified

- `cli/src/discover.rs`
  - Add `DiscoverWarning` enum + `Display` impl + `DiscoverOutcome`
    struct.
  - `use crate::parse::ParseError;`
  - Change return types of `discover_pool_members` and
    `discover_from_dir`.
  - Replace 3 inline failure arms (parser err, LUKS1 eprintln,
    canonicalize eprintln) with structured warning pushes.
  - Extend `LabelMap` with `dump_responses` field +
    `with_dump_response` builder; consult in the luksDump arm.
  - Add 2 new tests (Tests 1 & 2).
  - Upgrade 2 existing tests (Tests 3 & 4) to assert the new
    warning variants.
  - Mechanical update of all other tests to consume `DiscoverOutcome`.

- `cli/src/main.rs:696-727`
  - Switch `members` to `outcome`; iterate `outcome.warnings` with
    `eprintln!("warning: {w}")` before the empty check; propagate
    `outcome.members` into the existing print loop and pool.json
    write flow.

## Verification

- `just test-rust` -- runs the full discover test suite. Tests 1 and 2
  must fail on master and pass after the patch (proves the bug is
  testable and fixed). Tests 3 and 4 should pass at every point since
  they pin existing behavior the refactor is meant to preserve. The
  pre-existing runner-Err propagation tests must continue to pass.
- `cargo build -p braid-cli` -- ensures `main.rs` compiles against
  the new `DiscoverOutcome` return shape.
- `just test-parsers` -- not affected (no parser-contract change),
  but worth running once.

No NixOS VM test is needed: this is a CLI-layer fix with no
systemd/btrfs/luks side effects.

## Out of scope

- "LUKS2 device with no `Label:` line" silent drop (different defect).
- Refactoring other CLI sites (`ack.rs`, `alert.rs`) to use structured
  warnings -- worth doing eventually, not bundled here.
- Stderr-capture infrastructure for the test harness (the structured
  return makes it unnecessary for this fix).
