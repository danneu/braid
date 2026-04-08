# Enforce LUKS2 at the probe and discovery gateways; stop version-gating in the header-integrity probe

## Context

Two probe paths in braid are confused about their job, and a third (discovery) silently relies on the same conflation:

1. `cli/src/luks.rs:260-275` — `probe_luks_header` is documented as a *header integrity* probe (its callers want to know "is the header readable / corrupt / wiped?"), but its first command, `CryptsetupIsLuks`, passes `--type luks2` (`cli/src/cmd.rs:395-404`). Per `reference/cryptsetup/man/cryptsetup-isLuks.8.adoc:24` and `reference/cryptsetup/src/cryptsetup.c:2479` (`crypt_load(cd, CRYPT_LUKS2, NULL)`), this rejects LUKS1 devices. The result: a LUKS1-formatted disk that braid is configured to use surfaces the `LuksHeaderState::Unreadable` branch and prints `"LUKS header unreadable. Restore from your off-system LUKS header backup …"` (`cli/src/luks.rs:284-288`). That guidance is wrong: the header is intact, it is just the wrong version. The probe is gating on something it does not own.

2. `cli/src/probe.rs:72-110` — `probe_config_disk` is the gateway every braid command uses to classify a configured disk. It calls `cryptsetup luksUUID`, which silently accepts both LUKS1 and LUKS2 (`reference/cryptsetup/src/cryptsetup.c:2485-2510` calls `crypt_load(cd, CRYPT_LUKS, …)` — version-agnostic). braid only formats LUKS2 (`cli/src/cmd.rs:617-634`) and only supports LUKS2 semantically, but no probe in the codebase enforces it: a wrong-version disk would slip through to downstream callers (mount, status, add, replace, enroll, tui) and produce inconsistent failure modes.

3. `cli/src/discover.rs:51-57` — `discover_pool_members` uses `CryptsetupIsLuks` as its admission gate before reading the LUKS label. It currently relies on `--type luks2` to filter out LUKS1 candidates. Its results flow directly into `pool.json` via `braid discover --write` (`cli/src/main.rs:561-600`). If we drop `--type luks2` from `CryptsetupIsLuks` without compensating in discovery, a braid-labeled LUKS1 disk would be written into membership and only fail later at the gateway probe — too late to be useful guidance.

The fix is to put the version invariant at every gateway that owns it (`probe_config_disk` for runtime classification; `discover.rs` for membership construction) and let `probe_luks_header` go back to honestly answering the integrity question its callers actually care about. This satisfies `feedback_invariants_at_right_layer.md`. It does **not** widen `ConfigDiskState` (the consumers of which include destructive command paths like `add`/`replace`/`enroll`), so it does not run afoul of `feedback_no_diagnostic_refinements_in_mutation_paths.md`. It does add a single new render variant to `UnpooledDiskRender` (a TUI-only diagnostic enum), which is mechanically safe because the TUI's match arms are exhaustively checked by the compiler and `UnpooledDiskRender` has no destructive consumers.

Outcome: any wrong-version disk encountered through any braid command path produces an explicit, actionable signal:
- CLI gateway probe (`probe_config_disk`) → hard error with the disk name and version, halting `mount`/`status`/`add`/`replace`/`enroll`.
- Discovery gateway (`discover_pool_members`) → warning to stderr with the path and version, skip the disk, do not write it to `pool.json`.
- TUI → renders the disk with an explicit `LUKS{n} (unsupported)` cell instead of silently disappearing.

### Intentional tradeoff: damaged-LUKS2-metadata diagnostic regression

The strict gateway design has one knock-on effect that's worth calling out explicitly. Before this change, a configured pool member with damaged LUKS2 keyslot metadata (luksUuid succeeds, luksDump exits non-zero) would reach the unlock path; on auth failure, `probe_luks_header` was called for diagnostic enrichment and surfaced the curated `luks_header_damaged_guidance` (`cli/src/luks.rs:293-298`) — *"LUKS header metadata damaged. To attempt repair manually: cryptsetup repair --type luks2 …"*.

After this change, `probe_config_disk` calls luksDump itself and propagates `ParseError::CommandFailed` as `ProbeError::Parse`. The unlock attempt is never reached for a damaged-metadata disk, so the curated message never fires for *configured pool members*. The user sees the verbatim cryptsetup stderr (e.g., `"parse error: command cryptsetup luksDump failed (exit 1): Cannot read LUKS header metadata"`).

This is an intentional tradeoff. We considered swallowing `CommandFailed` in the gateway and falling through to `PresentLuks` so the diagnostic enrichment could still run, but rejected it because it reintroduces exactly the inconsistency this PR exists to fix: a per-call-site lottery where some paths catch wrong-version-or-damaged disks and others don't, and the gateway lying about a configured disk's state. The strict propagation is the price of "the gateway is the single source of truth". The user-facing message is less curated but accurate; `cryptsetup repair` is documented and the user can still run it themselves.

The curated `luks_header_damaged_guidance` remains reachable from the *unpooled-disk* paths — the `PresentNotLuks` branches in `mount.rs:191`, `status.rs:855`, `tui/probe.rs:206`. Those paths only fire when `parse_cryptsetup_luks_uuid` returns `CommandFailed`, which short-circuits `probe_config_disk` *before* the new luksDump call. So the curated message survives for disks braid has not yet successfully classified as LUKS — only configured pool members with mid-life metadata damage lose it.

## Files to modify

**Production code:**
- `cli/src/cmd.rs` — drop `--type luks2` from `CryptsetupIsLuks`. Add `MockRunner::with_luks_dump_text_luks2(device)` and `with_luks_dump_text_luks2_for(&[devices])` test helper methods (see "Test infrastructure addition" below).
- `cli/src/parse/cryptsetup_luks_version.rs` (new) — parse the `Version:` field from `cryptsetup luksDump` text output.
- `cli/src/parse/types.rs` — add `CryptsetupLuksVersionOutput` struct.
- `cli/src/parse/mod.rs` — register and re-export the new module.
- `cli/src/probe.rs` — add `ProbeError::UnsupportedLuksVersion`; extend `probe_config_disk` with the version check.
- `cli/src/discover.rs` — parse the version from the existing `luksDump` call; warn-and-skip on non-LUKS2.
- `cli/src/tui/model.rs` — add `UnpooledDiskRender::WrongLuksVersion(u32)`.
- `cli/src/tui/probe.rs` — pattern-match `ProbeError::UnsupportedLuksVersion` in the unpooled-disks loop and insert the new render variant.
- `cli/src/tui/view/mod.rs` — render the new variant in `unpooled_disk_status_cell`.

**Test files (broad impact — see "Test infrastructure addition" below):**
- `cli/src/probe.rs` — update existing `probe_config_disk_present_luks_*` tests to seed luksDump mocks; add 3 new tests for the version-check branches.
- `cli/src/discover.rs` — extend `LabelMap` mock to include a Version field with default 2 and a `with_version` override; add `discover_skips_luks1_disk`.
- `cli/src/luks.rs` — optional `probe_luks_header_ok_for_luks1` to lock in the version-agnostic header probe contract.
- `cli/src/mount.rs` — patch `base_two_disk_runner()` and ~10 individual tests to chain `with_luks_dump_text_luks2` for every disk that goes through `probe_config_disk`. **Replace** `unlock_keyfile_verify_fails_damaged_header_emits_repair_guidance` with `unlock_damaged_luks2_metadata_fails_at_gateway` (the original test scenario is now structurally unreachable — see "Intentional tradeoff" above). Delete the now-unused `test_keyfile_fail` helper.
- `cli/src/unlock.rs` — patch 6 tests to seed luksDump LUKS2 mocks.
- `cli/src/recover.rs` — patch 6 tests to seed luksDump LUKS2 mocks (per-test, since each builds its own runner inline).
- `cli/src/replace.rs` — extend `FailingReplaceRunner` to handle `CryptsetupLuksDumpText` returning LUKS2.
- `cli/src/enroll_key_file.rs` — patch 3 discovery tests to seed luksDump LUKS2 mocks.
- `cli/src/tui/probe.rs` — patch `unpooled_disk_present_luks_unknown_uuid_classified_as_unknown_luks` to seed a luksDump LUKS2 mock; add `unpooled_disk_wrong_luks_version_classified_correctly`.
- `cli/src/tui/view/mod.rs` — extend `unpooled_disk_status_cell_renders_each_variant` to assert the new `WrongLuksVersion` rendering.

**Fixture / golden coverage:**
- `tests/capture-tool-fixtures.py` — capture `cryptsetup luksDump /dev/vdb` (text variant) into a new fixture.
- `cli/tests/support/golden_common.rs` — register a `golden_test!` for `parse_cryptsetup_luks_version` against the new fixture (and a bonus `golden_cryptsetup_luks_label` filling a pre-existing gap).

## Changes

### 1. Drop `--type luks2` from `CryptsetupIsLuks`

`cli/src/cmd.rs:395-404`:

```rust
CmdRequest::CryptsetupIsLuks { device } => CmdArgs {
    program: "cryptsetup",
    args: vec!["isLuks".into(), device.clone()],
},
```

Delete the `// We only care about luks2` comment — it was the source of the drift.

After this change, `CryptsetupIsLuks` becomes a true "is the LUKS magic intact?" probe, version-agnostic. Production callers:
- `cli/src/luks.rs:261` (`probe_luks_header`) — now returns `Ok` for both LUKS1 and LUKS2 healthy headers, which is the contract its callers actually want.
- `cli/src/discover.rs:51` — now accepts both LUKS versions; the explicit version check we add in step 5 below prevents LUKS1 disks from being written into `pool.json`.

No CLI test snapshots the `isLuks` args (verified by grep for `args.*\["isLuks"`), so no test args need updating. Mocks in `recover.rs`, `status.rs`, `mount.rs`, `tui/probe.rs`, and `luks.rs` test modules key on the `CmdRequest::CryptsetupIsLuks { device }` enum variant, not on the rendered argv, so they remain valid.

### 2. New parser: `parse_cryptsetup_luks_version`

New file `cli/src/parse/cryptsetup_luks_version.rs`, modeled after `cryptsetup_luks_label.rs`:

```rust
use crate::cmd::RawCommandOutput;
use super::types::CryptsetupLuksVersionOutput;
use super::ParseError;

/// Parse the LUKS version from `cryptsetup luksDump` text output.
///
/// The text output begins with:
/// ```text
/// LUKS header information
/// Version:        2
/// ```
/// Both LUKS1 and LUKS2 emit a `Version:` line in this format
/// (`reference/cryptsetup/lib/setup.c:6138`,
///  `reference/cryptsetup/lib/luks2/luks2_json_metadata.c:2198`).
pub fn parse_cryptsetup_luks_version(
    raw: &RawCommandOutput,
) -> Result<CryptsetupLuksVersionOutput, ParseError> {
    if raw.exit_status != 0 {
        return Err(ParseError::CommandFailed {
            cmd: raw.cmd.clone(),
            exit_code: raw.exit_status,
            stderr: raw.stderr.clone(),
        });
    }

    let version_str = raw
        .stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Version:").map(str::trim))
        .ok_or_else(|| ParseError::MissingField {
            cmd: raw.cmd.clone(),
            field: "Version".into(),
        })?;

    let version: u32 = version_str.parse().map_err(|_| ParseError::UnexpectedValue {
        cmd: raw.cmd.clone(),
        field: "Version".into(),
        value: version_str.to_owned(),
    })?;

    Ok(CryptsetupLuksVersionOutput { version })
}
```

In `cli/src/parse/types.rs`, add:
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptsetupLuksVersionOutput {
    pub version: u32,
}
```

In `cli/src/parse/mod.rs`, register `pub mod cryptsetup_luks_version;` and re-export `pub use cryptsetup_luks_version::parse_cryptsetup_luks_version;` alongside the existing `cryptsetup_luks_label` re-export.

### 3. New `ProbeError::UnsupportedLuksVersion` variant

`cli/src/probe.rs:56-66`, add:

```rust
#[error("disk '{name}' is LUKS{version}; braid requires LUKS2. \
         To use this disk with braid, back up its data and re-add it \
         (braid will reformat it as LUKS2).")]
UnsupportedLuksVersion { name: String, version: u32 },
```

The `name` field is the configured disk name (so the error tells the user *which* disk is wrong without forcing the caller to wrap the error).

### 4. Wire the version check into `probe_config_disk`

`cli/src/probe.rs:72-110`, after `parse_cryptsetup_luks_uuid` succeeds and before constructing `PresentLuks`:

```rust
let dump_raw = runner.run(&CmdRequest::CryptsetupLuksDumpText {
    device: by_id.0.clone(),
})?;
let version = parse_cryptsetup_luks_version(&dump_raw)?.version;
if version != 2 {
    return Err(ProbeError::UnsupportedLuksVersion {
        name: name.to_owned(),
        version,
    });
}
```

`CryptsetupLuksDumpText` already exists (`cli/src/cmd.rs:142-145, 656-658`) and is read-only/idempotent. No new `CmdRequest` variant needed. `?` propagates `CmdError` and `ParseError` into `ProbeError::Cmd`/`ProbeError::Parse` via the existing `#[from]` impls.

> **Test impact:** adding this `runner.run()` call inside `probe_config_disk` forces every test that mocks `CryptsetupLuksUuid` as success to *also* mock `CryptsetupLuksDumpText` for the same device — otherwise `MockRunner::run` returns `MissingMock` and the test fails with `ProbeError::Cmd(MissingMock)`. This affected ~30 tests across `mount.rs`, `unlock.rs`, `recover.rs`, `replace.rs`, `enroll_key_file.rs`, and `tui/probe.rs`. The "Test infrastructure addition" section below explains how we minimized the test churn with a shared `MockRunner` helper rather than the per-file local helper originally planned.

### 5. Wire the version check into `discover.rs` (new gateway enforcement)

`cli/src/discover.rs:59-67` already calls `cryptsetup luksDump` once for label extraction. Reuse the same `RawCommandOutput` to also parse the version — single command, two parses. Replace the existing label-only block:

```rust
// Read LUKS label + version via luksDump text output
let dump_raw = match runner.run(&CmdRequest::CryptsetupLuksDumpText {
    device: path_str.clone(),
}) {
    Ok(raw) => raw,
    Err(_) => continue,
};

let version = match parse_cryptsetup_luks_version(&dump_raw) {
    Ok(out) => out.version,
    Err(_) => continue,
};
if version != 2 {
    eprintln!(
        "warning: skipping {path_str}: LUKS{version} (braid requires LUKS2)"
    );
    continue;
}

let label = parse_cryptsetup_luks_label(&dump_raw)
    .ok()
    .and_then(|out| out.label);
```

Rationale for the order (version check before label check): if the label is `braid-disk1` but the disk is LUKS1, the warning specifically tells the user *which* on-disk path was wrong-version, which is the actionable signal. If the label is unrelated (not a braid disk at all), the version check still skips it silently — same outcome as today.

The warning uses `eprintln!` consistent with other library modules in `cli/src/` (e.g., `add.rs`, `mount.rs`, `replace.rs`). `discover_pool_members` is only called from `main.rs::Commands::Discover`, so the stderr surface is appropriate.

### 6. New TUI render variant + explicit surfacing

In `cli/src/tui/model.rs:77-91`, add a variant:

```rust
/// `probe_config_disk` returned `ProbeError::UnsupportedLuksVersion`.
/// The disk is on-disk LUKS but the wrong version (LUKS1 only — braid
/// requires LUKS2). Recovery: back up data, re-add via `braid add`.
WrongLuksVersion(u32),
```

`UnpooledDiskRender` derives `Copy`; `u32` is `Copy`, so the derive still holds.

In `cli/src/tui/probe.rs:185-188`, replace the catch-all skip with a typed match:

```rust
let probed = match probe_config_disk(runner, fs, disk_name, &by_id) {
    Ok(p) => p,
    Err(probe::ProbeError::UnsupportedLuksVersion { version, .. }) => {
        unpooled_disks.insert(
            disk_name.clone(),
            UnpooledDiskRender::WrongLuksVersion(version),
        );
        continue;
    }
    Err(_) => continue, // other probe errors → skip (degrade gracefully)
};
```

In `cli/src/tui/view/mod.rs:338-351`, add a render arm:

```rust
UnpooledDiskRender::WrongLuksVersion(v) => Span::styled(
    format!("LUKS{v} (unsupported)"),
    Style::default().fg(Color::Red),
),
```

The compiler will require this match arm because the `unpooled_disk_status_cell` match is exhaustive — that's the safety net per `feedback_dont_overclaim_refactor_benefits.md` (the variant addition gives real exhaustiveness enforcement, not vibes).

### 7. Caller-side audit (no code changes expected)

All other `probe_config_disk` callers (`mount.rs:178`, `enroll_key_file.rs:43`, `replace.rs:110`, `status.rs:362/474`) propagate `ProbeError` via `?`. None pattern-match on specific variants. The new `UnsupportedLuksVersion` variant flows through naturally:

- `mount`, `enroll`, `replace`, `add`, `status` (CLI text + JSON) bail with the `UnsupportedLuksVersion` Display message. **This is intentional** — strict invariant enforcement at the gateway means a wrong-version configured disk halts all braid mutation operations on the pool until the user resolves it. The error message tells them how (`back up its data and re-add it`).
- `tui/probe.rs:185` is updated above to surface the wrong-version case explicitly.

Confirm by reading each caller. No diffs expected outside the files listed in "Files to modify".

## Reused functions / patterns

- `parse_cryptsetup_luks_label` (`cli/src/parse/cryptsetup_luks_label.rs:18-46`) — exact template for the new single-field text parser.
- `CryptsetupLuksDumpText` request (`cli/src/cmd.rs:656-658`) — reused as-is in both `probe.rs` and `discover.rs`. No new `CmdRequest` variant.
- `parse_cryptsetup_luks_version` and `parse_cryptsetup_luks_label` operate on the same `RawCommandOutput`, so `discover.rs` runs `luksDump` once and parses twice.
- `MockRunner::with_output` chaining and `ok_raw`/`err_raw` helpers (`cli/src/probe.rs:283-303`) — used by all new probe tests.
- `LabelMap` test runner (`cli/src/discover.rs:155-200`) — extended to also return a version field, since discover tests already use it for the existing `CryptsetupLuksDumpText` mock.
- `cli/tests/support/golden_common.rs` `golden_test!` macro — register the new parser fixture.
- The text format `Version:       \t<n>\n` is confirmed in cryptsetup source for both versions (`reference/cryptsetup/lib/setup.c:6138`, `reference/cryptsetup/lib/luks2/luks2_json_metadata.c:2198`).

## Test infrastructure addition

The original plan called for a local `luks_dump_luks2_ok(device) -> (CmdRequest, RawCommandOutput)` helper inside `mount.rs`'s test mod, mirroring the existing `luks_uuid_ok` helper. In practice this didn't compose well: the failing tests live in 6 different files and many don't use `luks_uuid_ok` at all (they construct `CryptsetupLuksUuid` mocks inline). Adding a per-file helper would have meant 6 copies plus per-test boilerplate (`let (req, out) = ...; .with_output(req, out)` for each disk).

Instead, we added two chain methods directly on `MockRunner` in `cli/src/cmd.rs`:

```rust
impl MockRunner {
    pub fn with_luks_dump_text_luks2(self, device: &str) -> Self { ... }
    pub fn with_luks_dump_text_luks2_for(self, devices: &[&str]) -> Self { ... }
}
```

The first chains a single LUKS2 luksDump mock; the second is a vararg-style wrapper for the common 2-or-3-disk pool case. Each failing test gained one line per disk (or one `_for(&[...])` call) appended to its existing runner setup.

This is a pragmatic deviation from the plan and worth noting because:
1. It puts test-only helper methods on the production `MockRunner` type. That's already the case for `with_output` / `with_output_stdin`, so we're consistent with existing precedent — but the surface is now slightly larger.
2. It's keyed on the exact device path, so tests that override the dump for a specific device (e.g., to return `Damaged`) can do so by calling `with_output(CryptsetupLuksDumpText { device }, …)` afterward — `MockRunner` stores outputs in a `HashMap` so the override wins.
3. The original local `luks_dump_luks2_ok` helper proposed in the test plan was added and then removed; the same goes for a local helper added to `mount.rs` during exploration. The shared `MockRunner` method is the only path to seeding dump mocks now, and the duplicate insertions in `base_two_disk_runner()` (introduced by an early `replace_all` and corrected during review) are gone.

## Tests

### New: `cli/src/parse/cryptsetup_luks_version.rs` test module

Mirrors the existing `cryptsetup_luks_label.rs` test module. Each test uses inline string literals (per the parser-module guidelines in `cli/src/parse/mod.rs:14-16`):

1. `parses_luks2_version` — typical `LUKS header information\nVersion:       \t2\n…` → `Ok(CryptsetupLuksVersionOutput { version: 2 })`.
2. `parses_luks1_version` — same shape with `Version:       \t1` → `Ok(CryptsetupLuksVersionOutput { version: 1 })`.
3. `errors_on_command_failure` — non-zero exit → `Err(ParseError::CommandFailed { .. })`.
4. `errors_on_missing_version_field` — empty/garbled stdout (no `Version:` line) → `Err(ParseError::MissingField { field: "Version", .. })`.
5. `errors_on_non_integer_version` — `Version: foo` → `Err(ParseError::UnexpectedValue { .. })`.

### New: `cli/src/probe.rs` `tests` mod

The **primary failure-layer test** per `feedback_test_at_failure_layer.md`:

1. `probe_config_disk_luks1_returns_unsupported_version` — Mock: `CryptsetupLuksUuid` ok with valid UUID; `CryptsetupLuksDumpText` ok with `LUKS header information\nVersion:       \t1\n…`. MockFs: by_id path exists. Assert: `Err(ProbeError::UnsupportedLuksVersion { name: "toshiba", version: 1 })`. **This test is the canary**: it fails if either step 1 (drop `--type luks2`) is reverted alone *or* the version check in step 4 is removed, because the gateway is the sole runtime enforcement point.
2. `probe_config_disk_luksdump_failure_propagates_as_cmd_error` — `CryptsetupLuksUuid` ok, `CryptsetupLuksDumpText` runner error → `ProbeError::Cmd(_)`.
3. `probe_config_disk_luksdump_garbled_propagates_as_parse_error` — `CryptsetupLuksUuid` ok, `CryptsetupLuksDumpText` exit 0 with no `Version:` line → `ProbeError::Parse(_)`.

### Updated: existing `probe_config_disk` tests in `cli/src/probe.rs`

`probe_config_disk_present_luks_closed` (lines 377-399) and `probe_config_disk_present_luks_open` (lines 402-424) currently mock only `CryptsetupLuksUuid`. They need to add a `CryptsetupLuksDumpText` mock returning a LUKS2-shaped `Version:       \t2` so the new code path passes through. No assertion changes.

Other existing tests (`probe_config_disk_absent`, `probe_config_disk_present_not_luks`, `probe_config_disk_cmd_spawn_fails`, `probe_config_disk_garbled_uuid_output`) short-circuit before the new `luksDump` call and need no changes.

### New + updated: `cli/src/discover.rs` tests

1. **Updated** existing `LabelMap` mock (`discover.rs:155-200`) so its `CryptsetupLuksDumpText` response includes a `Version:\t2\n` line by default, plus a new `with_version(path, version)` builder method for per-path overrides.
2. **New: `discover_skips_luks1_disk`** (the failure-layer test for the discovery side) — set up two braid-labeled disks and use `with_version(luks1_path, 1)` to mark one as LUKS1. Assert that `discover_pool_members` returns only the LUKS2 disk in the membership map. The plan originally called this `discover_skips_luks1_disk_with_warning`, but verifying the eprintln warning in unit tests is awkward (it goes to test stderr); the behavior assertion is the contract, so we dropped `_with_warning` from the name.

### New: `cli/src/tui/probe.rs` test

1. `unpooled_disk_wrong_luks_version_classified_correctly` — set up a `MockRunner` (using `one_disk_mounted_pool_runner()` as the base) where the second declared disk has a luksUuid success but a luksDump returning `Version: 1`. Assert that `unpooled_disks["ironwolf"] == UnpooledDiskRender::WrongLuksVersion(1)` (not skipped, not Missing). Uses the existing `StubFs` and `one_disk_mounted_pool_runner` helpers; named to match the existing `unpooled_disk_present_not_luks_*` test family.

Also patch the existing `unpooled_disk_present_luks_unknown_uuid_classified_as_unknown_luks` test, which mocked `CryptsetupLuksUuid` as success but had no `CryptsetupLuksDumpText` mock — after this PR, it needs the LUKS2 dump mock seeded so the gateway probe reaches `PresentLuks`.

### Replaced: `cli/src/mount.rs` damaged-header diagnostic test

The original test `unlock_keyfile_verify_fails_damaged_header_emits_repair_guidance` (mount.rs ~line 2184) overrode the disk1 luksDump mock to fail, expecting `probe_luks_header`'s `Damaged` enrichment to surface the curated `cryptsetup repair` guidance. Per the "Intentional tradeoff" section above, that scenario is now structurally unreachable for configured pool members: the gateway catches the luksDump failure first. We deleted the test and added a replacement that pins the new gateway behavior:

- **`unlock_damaged_luks2_metadata_fails_at_gateway`** — mocks disk1 with a healthy luksUuid but a failing luksDump (via `luks_dump_text_fail`). Asserts the resulting error contains `"luksDump"` and `"Cannot read LUKS header metadata"` (verbatim cryptsetup stderr) and does *not* contain `"wrong keyfile"` (the gateway must reject before keyfile verification runs) or any reference to local backup directories.

The replacement test serves the same regression purpose at the new boundary: it would fail if a future PR re-introduced a `CommandFailed` swallow in `probe_config_disk`, which is exactly the regression the strict-gateway design exists to prevent.

### Updated: `cli/src/tui/view/mod.rs` `unpooled_disk_status_cell_renders_each_variant`

The test at `cli/src/tui/view/mod.rs:1384-1410` is the single point that pins the user-facing vocabulary for every `UnpooledDiskRender` variant. Add the new variant to the `unpooled_disks` HashMap and assert its rendered text:

```rust
pool.unpooled_disks = HashMap::from([
    ("alpha".to_owned(), UnpooledDiskRender::Missing),
    ("bravo".to_owned(), UnpooledDiskRender::UnknownLuks),
    ("charlie".to_owned(), UnpooledDiskRender::LuksHeaderUnreadable),
    ("delta".to_owned(), UnpooledDiskRender::LuksHeaderDamaged),
    ("echo".to_owned(), UnpooledDiskRender::WrongLuksVersion(1)),
]);
// …
assert_eq!(cell("echo"), "LUKS1 (unsupported)");
```

The existing `"names not in unpooled_disks must return None"` assertion needs a new placeholder name (e.g., `"foxtrot"`) since `"echo"` is now claimed.

### Optional: `cli/src/luks.rs` add `probe_luks_header_ok_for_luks1`

Mock `CryptsetupIsLuks` exit 0 + `CryptsetupLuksDumpText` exit 0 with LUKS1-shaped output → assert `LuksHeaderState::Ok`. This documents that the header probe is now version-agnostic and locks in the contract so a future "let me re-add `--type luks2`" PR fails fast.

### New: golden parser fixture + golden test

This addresses a pre-existing parser-coverage gap (text-form `cryptsetup luksDump` has no golden fixture today, only the JSON form does) and ensures the new parser is covered in both stable and unstable lanes per the "Parser Compatibility" section of `AGENTS.md`.

1. **Capture** — extend `tests/capture-tool-fixtures.py` (around line 90-94, next to the existing JSON capture) to also write the text form:
   ```python
   # 8c. cryptsetup luksDump (text)
   machine.succeed(
       f"cryptsetup luksDump /dev/vdb"
       f" > {FIXTURE_DIR}/cryptsetup-luks-dump.txt"
   )
   ```
2. **Register golden test** — add to `cli/tests/support/golden_common.rs` (next to `golden_cryptsetup_luks_uuid` around line 191):
   ```rust
   golden_test!(
       golden_cryptsetup_luks_version,
       "cryptsetup-luks-dump.txt",
       "cryptsetup luksDump",
       parse::cryptsetup_luks_version::parse_cryptsetup_luks_version,
       |out: parse::types::CryptsetupLuksVersionOutput| {
           assert_eq!(out.version, 2, "captured fixture must be LUKS2");
       }
   );
   ```
3. **(Bonus, fills a pre-existing gap)** While we're touching `golden_common.rs`, also register `parse_cryptsetup_luks_label` against the same fixture:
   ```rust
   golden_test!(
       golden_cryptsetup_luks_label,
       "cryptsetup-luks-dump.txt",
       "cryptsetup luksDump",
       parse::cryptsetup_luks_label::parse_cryptsetup_luks_label,
       |out: parse::types::CryptsetupLuksLabelOutput| {
           // Capture script formats with `cryptsetup luksFormat`, no --label flag.
           assert!(out.label.is_none(), "captured fixture has no Label set");
       }
   );
   ```

### Other test files

- `cli/src/recover.rs:2030-2045`, `cli/src/status.rs:3309/3339/3379`, `cli/src/mount.rs:1976/1989/2273`, `cli/src/tui/probe.rs:681/742` — these mock `CryptsetupIsLuks` by `CmdRequest` variant, not by args. Run `cargo test` (`just test-rust`) to confirm they still pass.
- No other VM tests touch LUKS version handling (verified by grep). The tests in `tests/cli/braid-*.py` that already invoke `cryptsetup luksDump` don't depend on the version field's formatting.

## Verification

1. **Unit tests** — `just test-rust`. Confirms the new parser, the new probe variant, the updated probe/discover/tui tests, and that all existing mocks still pass.
2. **Fixture capture (REQUIRED FOLLOW-UP — flag this in the implementation PR per `feedback_flag_required_followups.md`)**:
   - `just capture-fixtures` — refreshes `cli/tests/fixtures/nixos-25.11/cryptsetup-luks-dump.txt`.
   - `just capture-fixtures-unstable` — same for the unstable lane.
   - Both must be run because the new fixture is referenced by golden tests in both lanes; they will SKIP without the file present, which masks coverage.
3. **Golden parser tests** — `just test-rust` (stable) and `just test-rust-unstable` (forecast). The new `golden_cryptsetup_luks_version` (and bonus `golden_cryptsetup_luks_label`) entries must pass on both lanes.
4. **Parser canary** — `just test-parsers`. Sanity check that the existing parser surface still matches live tool output (no regression expected; this PR adds a parser, doesn't modify existing ones).
5. **VM smoke** — `just test-vm hello-world` to confirm the build still cleanly assembles a NixOS image. No targeted VM test is added because no VM scenario produces a LUKS1 disk (and creating one just for this test would need its own infrastructure that doesn't otherwise exist).
6. **Manual error message inspection** — instantiate `ProbeError::UnsupportedLuksVersion { name: "toshiba".into(), version: 1 }` in a scratch test, eyeball the rendered Display output during review to confirm the message reads naturally.

## Out of scope / explicit non-goals

- No new `ConfigDiskState::PresentLuksWrongVersion` variant. The hard-error-from-probe path was the user's explicit choice over the soft-variant path; this avoids the enum-widening concern from `feedback_no_diagnostic_refinements_in_mutation_paths.md`.
- No graceful "wrong version" rendering in `status` text/JSON output. The hard error from `probe_config_disk` will halt the entire `status` call when any one declared disk is wrong-version. Acceptable tradeoff: the error message is precise, actionable, and points at the right disk. Partial-status rendering would require changing the gateway return type to `Vec<Result<ConfigDisk, ProbeError>>`, which is a larger refactor and out of scope.
- No automatic LUKS1→LUKS2 conversion via `cryptsetup convert`. braid does not own that workflow; user is told to back up and re-add.
- No new `CmdRequest` variants. We reuse `CryptsetupLuksDumpText`.
- No backwards-compatibility shim for any pre-existing LUKS1 pool. braid is unreleased per `AGENTS.md` ("No backwards compatibility"); the error-and-instruct path is the supported recovery.

## Follow-ups (not in this PR)

- **Dead-code cleanup of `Damaged` arms in `mount.rs`.** With the strict gateway, the `LuksHeaderState::Damaged` arms in `mount.rs:388` (`open_disks_with_passphrase`'s post-verify enrichment) and `mount.rs:493` (the keyfile equivalent) are no longer reachable for *configured pool members*: a damaged-metadata disk fails `probe_config_disk` before the unlock attempt even runs. They're still reachable from the *unpooled* paths (`mount.rs:191` `PresentNotLuks` branch) via `MissingReason::LuksHeaderDamaged`, so they aren't entirely dead — but the post-verify enrichment branches in `open_disks_with_passphrase` / `open_disks_with_keyfile` could be collapsed in a follow-up. Out of scope here because the audit needs care (`probe_luks_header` is also called from `status.rs` and `tui/probe.rs` for unpooled disks, and those paths still want the full classification).

## Implementation notes

Implementation diverged from the initial plan in one important place: the first attempt treated `cryptsetup luksDump` exit-non-zero in `probe_config_disk` as a non-fatal case and let the disk fall through as `PresentLuks`, with the idea that later command paths would use `probe_luks_header` to refine damaged-vs-unreadable guidance. That did not work.

In practice, once `probe_config_disk` is the configured-disk gateway, it must not lie about disk health. A disk whose `luksUUID` succeeds but whose `luksDump` exits non-zero is not a healthy `PresentLuks` disk, and allowing it through caused downstream command paths to treat damaged metadata as usable until a later failure.

The working implementation therefore changed the gateway rule to:

- `probe_config_disk` runs `cryptsetup luksDump` after `luksUUID`.
- `parse_cryptsetup_luks_version` success with `version != 2` returns `ProbeError::UnsupportedLuksVersion`.
- `cryptsetup luksDump` command failure or parse failure is propagated as a hard gateway error (`ProbeError::Cmd` / `ProbeError::Parse`), not deferred.

This also changed the expected command-path behavior for configured disks with damaged metadata:

- `mount` / `unlock` now fail before passphrase or keyfile verification for that disk.
- The old “verify failed, then enrich with `probe_luks_header`” path still exists for cases that explicitly probe header integrity, but it is no longer the normal configured-disk path for damaged metadata.
- Tests that originally tried to preserve the old damaged-header enrichment flow for configured disks were updated to assert the new gateway failure instead.

The plan’s broader structure still held: version enforcement moved to the owning gateways (`probe_config_disk` and `discover_pool_members`), `probe_luks_header` became version-agnostic again, and the TUI gained an explicit unsupported-version render path.
