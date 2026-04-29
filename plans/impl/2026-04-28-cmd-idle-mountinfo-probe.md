# Plan: replace findmnt subprocess in `cmd_idle` mount probe with `/proc/self/mountinfo` read

## Context

Plan review surfaced a fail-open seam inside the fail-closed autosuspend gate:

- `parse_findmnt_json` (`cli/src/parse/findmnt.rs:30-36`) returns `Ok(empty filesystems)` whenever `findmnt` exits non-zero AND stderr is empty -- the "mount point not found" convention.
- `is_btrfs_mounted` (`cli/src/idle.rs:126`) maps an empty list to `Ok(false)`.
- `cmd_idle` (`cli/src/idle.rs:72`) maps false to `IdleResult::PoolOffline`, which `cli/src/main.rs:549-552` exits with 0.
- `modules/braid/auto-suspend.nix:86` wraps it as `bash -c '! timeout -k 2 10 braid idle'`. The `timeout` is *inside* the `!` deliberately (the comment at `auto-suspend.nix:80-85` notes that an outer `timeout` would fail open by killing bash before `!` runs); a `braid idle` overrun is treated as exit 124, then `!` flips to 0, and autosuspend's `CommandActivity` (confirmed in `reference/autosuspend/src/autosuspend/checks/command.py`) treats exit 0 as "activity" and *blocks* suspend. For the bug here, what matters is that `braid idle` exit 0 (PoolOffline) inverts to non-zero, which `CommandActivity` treats as "no activity" and *allows* suspend.

Real failure shape: an ordinary non-zero `findmnt` exit with empty stderr that is **not** the "mount point not found" case it pretends to be. Concretely: a future findmnt regression that errors silently, or any other anomaly where findmnt returns non-zero without diagnostic output. Signal death (SIGKILL/OOM) is **not** in scope -- `output_to_raw` (`cli/src/cmd.rs:845-858`) converts signal-killed subprocesses into `CmdError::Failed` before the parser is reached, so that path already fails closed. The gap is purely in the "non-zero exit code with empty stderr" branch.

Result: any such silent non-zero exit silently becomes "pool offline -- safe to suspend". The downstream scrub/balance/replace probes are skipped. A scrub/balance/replace in progress can be interrupted by suspend.

Goal: remove the subprocess entirely from this safety-critical probe. Read `/proc/self/mountinfo` directly. One syscall, no fork/exec, no parser-folklore fallback. Treat any IO or parse uncertainty as suspend-blocking.

## Why `/proc/self/mountinfo` over the alternatives

- **Tighten the parser / gate `is_btrfs_mounted` on `exit_status == 0`** -- the proposer's "minimum" fix. Closes the specific known seam but keeps a subprocess in a path that has no business shelling out. Rejected as the principal fix.
- **`statfs(2)` checking magic `0x9123683E`** -- single syscall, but `statfs` reports the fs that owns the path, not whether the path is itself a mount point. If the OS root is btrfs and `/mnt/storage` exists as a plain directory, statfs reports btrfs and we false-positive into the probes. Distinguishing mount-point vs subdir requires a second `statfs` on the parent and an `st_dev` compare. Rejected.
- **`/proc/self/mountinfo`** -- explicit kernel-maintained mount table. Direct answer, no inference, no subprocess. Format stable since Linux 2.6.26 (2008). Chosen.

Exploration confirmed there is no existing `/proc`/mountinfo helper in `cli/src/` and no `nix`/`rustix`/`procfs` dep -- only `libc`. The new parser is small and self-contained.

### Corroborating evidence in `reference/coreutils`

`reference/coreutils/src/df.c` reads mount data through gnulib's `mountlist`, and on Linux `mountlist` treats `/proc/self/mountinfo` as the authoritative mount table. This is the same source-of-truth choice this plan makes for `cmd_idle`. `df.c` also carries substantial duplicate / overmount handling code, which supports the fail-closed `DuplicateTarget` policy here -- duplicates are real, not just theoretical, and a safety-critical caller should refuse to guess which entry is "current".

`reference/coreutils/NEWS` records real `df` bugs around unusual `/proc/self/mountinfo` entries, including:
- **empty source fields** (two consecutive spaces between `fstype` and `super_options`) -- valid mountinfo that naive parsers reject;
- **octal-escaped whitespace in filesystem-type fields** -- the same `\040` / `\011` / `\012` / `\134` escape scheme this plan decodes for the mount-point field.

Implication: the parser must use space-aware parsing that preserves empty fields (`str::split(' ')`, **not** `str::split_whitespace`), and the empty-source case must be exercised by a unit test. Both are pinned below.

## Files to modify

- **New** `cli/src/mount_check.rs` -- pure mountinfo parser + thin IO wrapper that goes through the existing `Filesystem` trait.
- `cli/src/lib.rs` -- register the new module.
- `cli/src/idle.rs` -- drop `FindmntJson` and `parse_findmnt_json` from `cmd_idle`'s initial mount check; rewrite the local `is_btrfs_mounted` helper to take `&F: Filesystem` instead of `&R: CommandRunner` and read `/proc/self/mountinfo` via `fs.read_to_string("/proc/self/mountinfo")`; add a `MountInfo(#[from] crate::mount_check::MountInfoError)` variant to `IdleError` (preserve the existing `Cmd`, `Parse`, `Probe`, `Exclop` variants -- `cmd_idle` still uses `probe_fsid` and `check_no_exclusive_op` after the mount check). **Signature stays `cmd_idle(runner, fs, mount_point)` -- no new parameter.**
- `docs/decisions/016-auto-suspend.md` -- add a subsection under "Decision" recording that the mount probe reads `/proc/self/mountinfo` directly via the existing `Filesystem` abstraction, that octal-escaped paths are decoded before comparison, and that any IO or parse uncertainty surfaces as `IdleError` -> exit 2 -> blocks suspend.

`cli/src/main.rs:549` requires no changes -- it already passes `&RealFilesystem` to `cmd_idle`, and `RealFilesystem::read_to_string` is `std::fs::read_to_string`.

**Scope: this plan only fixes `cmd_idle`.** `parse_findmnt_json` and the `FindmntJson` `CmdRequest` variant stay unchanged. `check_not_read_only` (`cli/src/preflight.rs:198-231`) is a fail-open preflight check (failures map to `Ok(Some(warning_text))`) and is unaffected by the lenient parser branch.

`probe_pool` (`cli/src/probe.rs:214-242`) is **also affected by the same bug** -- it consumes `parse_findmnt_json` output and maps an empty filesystems list to `Ok(PoolState { mounted: false, ... })`, which `cmd_monitor` (fail-closed per `cli/src/monitor.rs:21-23` and `docs/decisions/014-alerts.md`) surfaces as `MonitorResult::PoolOffline`. A silent non-zero `findmnt` exit there silences alerting on a genuinely-mounted pool. This is captured as an explicit follow-up below; it is **not** fixed by this plan because the right fix for `probe_pool` is a separate design decision (route `probe_pool` through the same `mount_check` module, or harden the shared parser, or both -- each has different blast radius across `add`/`remove`/`monitor`).

## Implementation sketch

### 1. `cli/src/mount_check.rs`

Parser must **fail closed**. Any line that doesn't conform to the documented mountinfo format returns `Err`, not `Ok(None)`. `Ok(None)` means "the file was well-formed and our target is genuinely not present". This distinction is what closes the bug.

Mountinfo format from `proc(5)`:

```
mount_id parent_id major:minor root mount_point options optional_fields - fstype source super_options
```

`optional_fields` is zero-or-more space-separated tokens terminated by a literal `-` field. The mount_point and source fields are octal-escaped for space (`\040`), tab (`\011`), newline (`\012`), and backslash (`\134`); we decode the mount_point before comparing to `target`.

```rust
use std::io;
use crate::probe::Filesystem;

const MOUNTINFO_PATH: &str = "/proc/self/mountinfo";

#[derive(Debug, thiserror::Error)]
pub enum MountInfoError {
    #[error("io reading mountinfo: {0}")]
    Io(#[from] io::Error),
    #[error("malformed mountinfo line: {line}")]
    Malformed { line: String },
    #[error("mountinfo contains multiple entries for target {target}")]
    DuplicateTarget { target: String },
}

/// Returns the fstype mounted at `target`, or Ok(None) if the well-formed
/// mountinfo content has no entry for `target`. Returns Err for any malformed
/// non-empty line (related or not) and for any case where multiple entries
/// match `target` -- both are anomalies the safety-critical caller must treat
/// as suspend-blocking.
pub fn fstype_at_mount(content: &str, target: &str) -> Result<Option<String>, MountInfoError> {
    let mut hit: Option<String> = None;
    for line in content.lines() {
        if line.is_empty() { continue; }
        let parsed = parse_line(line)
            .ok_or_else(|| MountInfoError::Malformed { line: line.into() })?;
        if parsed.mount_point == target {
            if hit.is_some() {
                return Err(MountInfoError::DuplicateTarget { target: target.into() });
            }
            hit = Some(parsed.fstype);
        }
    }
    Ok(hit)
}

struct ParsedLine { mount_point: String, fstype: String }

/// Validates the *full* mountinfo line shape. Returns None on any deviation:
/// missing mandatory fields, missing "-" separator, missing source /
/// super_options after fstype, or extra fields beyond super_options.
/// Lenient validation here would re-introduce the "we don't know -> allow
/// suspend" gap this fix exists to close.
fn parse_line(line: &str) -> Option<ParsedLine> {
    // split(' '), NOT split_whitespace(): empty source fields appear in real
    // mountinfo (e.g. "... - tmpfs  rw") and must parse as a present-but-empty
    // field, not be silently collapsed. coreutils df has had bugs from getting
    // this wrong; see reference/coreutils/NEWS.
    let mut fields = line.split(' ');
    for _ in 0..4 { fields.next()?; }                 // mount_id, parent_id, major:minor, root
    let mount_point = decode_octal_escapes(fields.next()?);
    fields.next()?;                                    // mount_options
    let mut saw_dash = false;
    for f in fields.by_ref() {
        if f == "-" { saw_dash = true; break; }
    }
    if !saw_dash { return None; }
    let fstype = fields.next()?.to_string();
    fields.next()?;                                    // source (may be literal "none" for tmpfs/proc)
    fields.next()?;                                    // super_options
    if fields.next().is_some() { return None; }        // no trailing junk allowed
    Some(ParsedLine { mount_point, fstype })
}

fn decode_octal_escapes(s: &str) -> String {
    // Kernel only emits \040 \011 \012 \134; decode those, leave every other
    // byte (including multi-byte UTF-8 continuation bytes) untouched.
    //
    // Operate on bytes -- not chars -- because the input may contain non-ASCII
    // UTF-8 paths (e.g. a path ending in U+00E9, two bytes 0xC3 0xA9). A naive
    // `bytes[i] as char` loop would interpret each UTF-8 continuation byte as
    // a separate Latin-1 code point and produce mojibake, causing the target
    // comparison to silently miss a mounted pool and fall through to
    // PoolOffline -- a fail-open result in the safety-critical check.
    //
    // Output is valid UTF-8 by construction: input is &str (valid UTF-8), the
    // only replacements are ASCII bytes ('\' and the three octal digits) being
    // replaced by other ASCII bytes (' ', '\t', '\n', '\\'). All non-replaced
    // bytes are passed through verbatim.
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 4 <= bytes.len() {
            match &bytes[i+1..i+4] {
                b"040" => { out.push(b' ');  i += 4; continue; }
                b"011" => { out.push(b'\t'); i += 4; continue; }
                b"012" => { out.push(b'\n'); i += 4; continue; }
                b"134" => { out.push(b'\\'); i += 4; continue; }
                _ => {}
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).expect("UTF-8 preserved by construction")
}

/// IO wrapper that goes through the existing `Filesystem` trait so tests
/// can mock `/proc/self/mountinfo` content via the same MockFs they use for
/// sysfs reads. Production paths get `RealFilesystem`, which delegates to
/// `std::fs::read_to_string`.
pub fn is_btrfs_mounted<F: Filesystem + ?Sized>(fs: &F, target: &str) -> Result<bool, MountInfoError> {
    let content = fs.read_to_string(MOUNTINFO_PATH)?;
    Ok(fstype_at_mount(&content, target)?.as_deref() == Some("btrfs"))
}
```

**Parser policy: strict on every non-empty line.** Any malformed line, whether or not it matches the target, returns `Err`. Rationale: `/proc/self/mountinfo` is kernel-formatted and has been format-stable since 2008. A malformed line is an anomaly, and a fail-closed safety gate must treat anomalies as suspend-blocking. The cost (suspend blocked until the anomaly clears) is acceptable; the alternative (silently ignoring lines we can't parse) recreates the same "we don't know -> allow suspend" pattern this fix exists to eliminate.

### 2. `cli/src/idle.rs` changes

Extend `IdleError` (preserve existing variants):

```rust
#[derive(Debug, thiserror::Error)]
pub enum IdleError {
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("exclusive-op check error: {0}")]
    Exclop(String),
    #[error("mount probe error: {0}")]
    MountInfo(#[from] crate::mount_check::MountInfoError),
}
```

(`MountInfoError` already wraps `io::Error`, so no separate `Io` variant on `IdleError`.)

`cmd_idle` signature is **unchanged** -- it stays `(runner, fs, mount_point)`. The `fs: &F` parameter is what we now use for the mountinfo read, replacing the `runner.run(FindmntJson)` call.

The local `is_btrfs_mounted` helper changes from runner-based to fs-based:

```rust
// Was: fn is_btrfs_mounted<R: CommandRunner>(runner: &R, mount_point: &MountPoint) -> Result<bool, IdleError>
// Now:
fn is_btrfs_mounted<F: Filesystem + ?Sized>(fs: &F, mount_point: &MountPoint) -> Result<bool, IdleError> {
    Ok(crate::mount_check::is_btrfs_mounted(fs, mount_point.as_str())?)
}
```

And the call site in `cmd_idle`:

```rust
// Was: if !is_btrfs_mounted(runner, mount_point)? { return Ok(IdleResult::PoolOffline); }
// Now: if !is_btrfs_mounted(fs, mount_point)? { return Ok(IdleResult::PoolOffline); }
```

Drop the `parse_findmnt_json` import from `idle.rs` (`probe_fsid` still uses it internally; that's not our concern). The `CmdRequest::FindmntJson` variant is *not* removed -- `probe_fsid` (called later in `cmd_idle`) still uses it.

`?`-propagation gives the fail-closed behavior: any IO error or parser `Err` from `mount_check` becomes `IdleError::MountInfo` -> `main.rs` exits 2 -> `! braid idle` flips to 0 -> autosuspend treats as activity -> blocks suspend.

### 3. `cli/src/main.rs`

**No changes.** `main.rs:548-549` already constructs `let fs = braid_cli::probe::RealFilesystem;` and passes `&fs` to `cmd_idle`. `RealFilesystem::read_to_string` delegates to `std::fs::read_to_string`, which reads `/proc/self/mountinfo` correctly in production.

### 4. `docs/decisions/016-auto-suspend.md`

Add a subsection (under "Decision", before "SSH always on..."):

> ### Mount probe reads `/proc/self/mountinfo` directly
>
> `braid idle`'s initial mount-presence check (`is_btrfs_mounted`) reads `/proc/self/mountinfo` via the existing `Filesystem` abstraction rather than shelling out to `findmnt`. Rationale: the mount probe is a fail-closed safety gate; any subprocess fallback path that maps "non-zero exit + empty stderr" to "no mount" reintroduces the fail-open seam this gate exists to prevent. The kernel-maintained mountinfo file gives a direct answer in one syscall, with no fork/exec.
>
> Octal-escaped mount-point fields (`\040`, `\011`, `\012`, `\134`) are decoded before comparison so configured mount paths containing whitespace match correctly.
>
> IO errors (file unreadable, EIO), malformed mountinfo lines, and ambiguous duplicate target entries all propagate as `IdleError::MountInfo`, surface as exit 2, and block suspend. "Don't know" never becomes "allow suspend".
>
> Note: `cmd_idle` continues to call `probe_fsid` after the mount check, and `probe_fsid` still uses `findmnt` internally. That call is fail-closed at its own callsite (`probe_fsid` returns `ProbeError::PoolDevice` when the target is absent, which becomes `IdleError::Probe` and exit 2). Hardening `probe_fsid` and other findmnt callers is tracked separately.

## Tests

### Unit tests in `cli/src/mount_check.rs`

Each test as a `/* Intent / Why / Scenario */` block per project convention.

- `fstype_at_mount_finds_btrfs_target` -- realistic mountinfo body, target mounted as btrfs, returns `Ok(Some("btrfs"))`.
- `fstype_at_mount_returns_none_when_target_absent` -- well-formed body without target returns `Ok(None)`.
- `fstype_at_mount_returns_other_fstype` -- target mounted as ext4 returns `Ok(Some("ext4"))` (proves we distinguish "wrong fs" from "not present").
- `fstype_at_mount_handles_optional_fields` -- target line includes a `master:N` optional field before the `-` separator; still parses correctly.
- `fstype_at_mount_handles_multiple_mounts` -- multi-line body with target as the second entry parses correctly.
- `fstype_at_mount_decodes_octal_escaped_path` -- target configured as `/mnt/storage pool`, mountinfo contains `/mnt/storage\040pool`, returns `Ok(Some("btrfs"))`. Bug-fix regression guard for the path-escape finding.
- `fstype_at_mount_errors_on_malformed_target_line` -- mountinfo line for the target is missing the `-` separator (or fstype after it); returns `Err(MountInfoError::Malformed)`. **Regression guard for the parser-fail-open finding** -- this test must fail before the fix and pass after.
- `fstype_at_mount_errors_on_malformed_unrelated_line` -- mountinfo body has the target line correctly formatted *plus* an unrelated short/malformed line; returns `Err(MountInfoError::Malformed)`. Pins the "strict on every line" policy so a future relaxation gets caught.
- `fstype_at_mount_errors_on_target_line_truncated_after_fstype` -- target line ends after `... - btrfs` with no source / super_options fields; returns `Err(MountInfoError::Malformed)`. Pins the post-fstype validation so a future shortcut returning `Some` after just the fstype field gets caught.
- `fstype_at_mount_errors_on_duplicate_target_entries` -- mountinfo body has two well-formed entries with the same `target`; returns `Err(MountInfoError::DuplicateTarget)`. Pins the fail-closed ambiguity behavior so a future "first hit wins" change gets caught.
- `decode_octal_escapes_handles_all_four_kernel_escapes` -- `\040 \011 \012 \134` decode to ` `, `\t`, `\n`, `\\` respectively; unrelated backslash sequences pass through literally.
- `fstype_at_mount_preserves_non_ascii_utf8_path` -- target configured as `/mnt/caf\u{e9}` (UTF-8 bytes `0xC3 0xA9`) appears verbatim in mountinfo (the kernel only escapes the four whitespace/backslash chars, not arbitrary non-ASCII), parser decodes through `decode_octal_escapes` without corruption, and the comparison succeeds returning `Ok(Some("btrfs"))`. **Regression guard for the UTF-8 decoder finding** -- a `bytes[i] as char` implementation would mangle the multi-byte sequence and cause the test to fail with `Ok(None)`.
- `is_btrfs_mounted_io_error_when_read_fails` -- pass a `MockFs` whose `read_to_string("/proc/self/mountinfo")` returns `Err(NotFound)`; `mount_check::is_btrfs_mounted` returns `Err(MountInfoError::Io)`, not `Ok(false)`. Regression guard for the IO-failure path.
- `fstype_at_mount_errors_on_trailing_junk_after_super_options` -- target line has an extra unexpected field after super_options; returns `Err(MountInfoError::Malformed)`. Pins the no-trailing-junk policy.
- `fstype_at_mount_accepts_empty_source_field` -- target line has two consecutive spaces between `fstype` and `super_options` (e.g. `... - tmpfs  rw,size=...`), modeling a real mountinfo entry with an empty source field. Returns `Ok(Some("tmpfs"))`, **not** `Err(Malformed)`. Pins the requirement that the parser uses `split(' ')` rather than `split_whitespace()`; corroborated by `df` bugs recorded in `reference/coreutils/NEWS` around exactly this case.

### Rewritten tests in `cli/src/idle.rs`

The existing test set (`idle_when_pool_offline`, `idle_when_all_ops_quiet`, `busy_when_*`, `error_on_probe_failure`, `short_circuits_on_first_busy`, `replace_status_failure_is_not_idle` -- map to the current set in `idle.rs` after the recent refactor; rename/adjust as needed) keeps its structure but adapts `MockFs` to also serve `/proc/self/mountinfo`.

**Critical: `findmnt_mounted` mocks are still required for the non-scrub mounted-pool tests** because `cmd_idle` calls `probe_fsid` after the mount check, and `probe_fsid` still uses `CmdRequest::FindmntJson` followed by `BtrfsFilesystemShow` to derive the fsid. Removing those mocks would break the existing fsid + sysfs path. The only `FindmntJson` mock that goes away is the one previously consumed by the now-removed initial `is_btrfs_mounted` `runner.run(FindmntJson)` call -- but since that was the *first* `FindmntJson` request and `probe_fsid`'s is the *second*, the test fixtures previously seeded one mock that served both. After the change, that single mock continues to serve `probe_fsid` only.

Extend `MockFs` to also serve `/proc/self/mountinfo`:

```rust
struct MockFs {
    expected_path: String,                  // existing -- exclop sysfs path
    body: Option<String>,                   // existing -- exclop body
    mountinfo: Option<String>,              // new -- /proc/self/mountinfo content
}

impl MockFs {
    fn with_exclop_and_mountinfo(exclop_body: &str, mountinfo: &str) -> Self {
        Self {
            expected_path: exclop_path(),
            body: Some(format!("{exclop_body}\n")),
            mountinfo: Some(mountinfo.to_string()),
        }
    }
    // ... existing constructors get a `.with_mountinfo(...)` builder
}

impl Filesystem for MockFs {
    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        if path == "/proc/self/mountinfo" {
            return self.mountinfo.clone()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "mock: no mountinfo seeded"));
        }
        // existing exclop branch unchanged
        if path == self.expected_path { /* ... */ }
        // ...
    }
    // exists, is_block_device, list_dir unchanged
}
```

Helper for canonical mountinfo bodies:

```rust
const MOUNTINFO_WITH_BTRFS: &str =
    "36 35 0:32 / /mnt/storage rw,noatime shared:1 - btrfs /dev/mapper/braid-disk1 rw\n";
const MOUNTINFO_WITHOUT_TARGET: &str =
    "26 25 0:23 / / rw,noatime shared:1 - ext4 /dev/sda1 rw\n";
```

Each existing test threads the appropriate constant into `MockFs`. `cmd_idle` is called with the **same signature** as today: `cmd_idle(&runner, &fs, &mp())`.

Add two new behavioral regression tests:

```rust
/* Intent: mountinfo IO failure must propagate as IdleError, not PoolOffline.
 * Why: a fail-closed safety gate must never let "we couldn't determine state"
 *   become "ok to suspend". The previous findmnt path mapped non-zero+empty-stderr
 *   exits to PoolOffline, which allowed suspend. The replacement must surface
 *   read failures as Err.
 * Scenario: MockFs.read_to_string("/proc/self/mountinfo") returns NotFound.
 */
#[test]
fn mountinfo_read_failure_is_not_pool_offline() {
    let runner = MockRunner::default();
    let fs = MockFs::with_no_mountinfo();   // mountinfo = None
    let result = cmd_idle(&runner, &fs, &mp());
    assert!(matches!(result, Err(IdleError::MountInfo(_))));
}

/* Intent: malformed mountinfo content for the target mount must propagate
 *   as IdleError, not PoolOffline.
 * Why: same fail-closed contract as above. The original bug was a lenient
 *   parser branch returning "no entry" for a state that wasn't actually
 *   "no entry"; the replacement must error rather than silently default.
 * Scenario: mountinfo body is well-formed except the target line is missing
 *   the "- fstype" tail.
 */
#[test]
fn mountinfo_malformed_target_line_is_not_pool_offline() {
    let runner = MockRunner::default();
    let fs = MockFs::with_mountinfo(
        "36 35 0:32 / /mnt/storage rw,noatime shared:1 garbage_no_dash_separator\n"
    );
    let result = cmd_idle(&runner, &fs, &mp());
    assert!(matches!(result, Err(IdleError::MountInfo(_))));
}
```

(`tempfile` is no longer needed for these tests since `MockFs` serves the content directly.)

## Verification

1. `just test-rust` -- new mount_check unit tests, retargeted idle tests, and the two regression tests pass.
2. `just test-vm braid-idle braid-auto-suspend replace-inhibits-suspend remove-inhibits-suspend` -- run the targeted autosuspend/idle VM tests (defined in `tests/cli/braid-idle.nix`, `tests/module/braid-auto-suspend.nix`, `tests/cli/replace-inhibits-suspend.nix`, `tests/cli/remove-inhibits-suspend.py`). Then `just test-vm` for the full suite to catch any unanticipated regression.
3. Manual sanity in a VM (or against the running NAS):
   - Pool mounted, no ops: `braid idle; echo $?` -> 0.
   - Pool unmounted: `braid idle; echo $?` -> 0.
   - Pool mounted, scrub running: `braid idle; echo $?` -> 1.
   - Inject a `MountInfoError` only via the unit test added above -- there is no production way to make `/proc/self/mountinfo` unreadable, which is the point of the fix. The unit test is the regression contract.

## Follow-ups (explicitly out of scope, must be tracked)

- **Harden `probe_pool` mount detection against the same silent `findmnt` failure.** `cmd_monitor` is fail-closed per `docs/decisions/014-alerts.md` and `cli/src/monitor.rs:21-23`, but `probe_pool` (`cli/src/probe.rs:214-242`) currently swallows non-zero + empty-stderr findmnt exits as "pool not mounted" via the same lenient parser branch. After this plan lands, `cmd_monitor` would silently report `PoolOffline` on a mounted pool with a misbehaving findmnt -- losing the ComputationError alert path. Resolution options (separate plan): (a) route `probe_pool` through `mount_check::is_btrfs_mounted_at` then continue using findmnt only for the device list (cleanest, but threads a path everywhere `probe_pool` is called), (b) replace `probe_pool`'s findmnt call with `lsblk` or `btrfs filesystem show` for device enumeration plus mountinfo for presence (larger refactor), or (c) tighten `parse_findmnt_json` itself to return `Err` on non-zero exit + empty stderr and audit `add`/`remove`/`preflight` for fallout (smallest delta but changes shared semantics). This decision belongs in its own plan.

## Other items not addressed here

- Hardening `parse_findmnt_json` itself for `add`/`remove`/`preflight`. Those are not fail-closed safety gates; tracked only as part of the `probe_pool` follow-up if option (c) is chosen.
- Adding mount-point validation in `cli/src/config.rs:45` or `modules/braid/options.nix:14` to reject paths with whitespace. Decoding octal escapes in the parser is the more robust and localized fix; rejecting whitespace at config time would be a separate UX-improvement plan.
