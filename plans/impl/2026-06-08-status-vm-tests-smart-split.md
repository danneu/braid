# Fix VM tests broken by the `btrfs_errors` + `smart` status split

## Context

Commit `efdf85b1 feat(status): split per-disk error reporting into btrfs_errors + smart`
changed the `braid status` per-disk output in two ways:

- **Human output:** the single `Errors:` line became two lines -- `btrfs:` (the
  filesystem I/O counters) and a new `SMART:` line (the drive's self-report).
  See `cli/src/status.rs#format_status_human` (the `btrfs:` / `SMART:` block) and
  `cli/src/status.rs#format_smart_human`.
- **JSON output (`--json`):** the `DiskReport.errors` field was renamed to
  `btrfs_errors` and a sibling `smart` object was added. See
  `cli/src/status.rs` -> `pub struct DiskReport` (`btrfs_errors`, `smart`; both
  snake_case, no serde rename). Note the internal human-render struct keeps its
  `errors` field name -- only the serialized JSON field renamed (documented in
  `status.rs` near the `DiskReport` -> `HumanDisk` mapping).

The commit updated the Rust unit tests but not the NixOS VM tests (they're `.py`,
not compiled with the crate, so `cargo test` couldn't catch them). Three VM tests
assert on the old format with literal substring / JSON-key checks and now fail.

**Scope (per request):** fix only the three tests broken by the `errors ->
btrfs_errors` and new `smart`/`SMART:` changes. Explicitly ignore the two unrelated
failures:
- `braid-idle` -- scrub/idle detection (`tests/cli/braid-idle.py`, "running scrub
  must make braid idle exit 1, got 0"). Unrelated to output format.
- `braid-module-disabled` -- `systemctl list-unit-files` "Connection timed out"
  (`tests/module/disabled.py`). Infra/timeout flake, unrelated.

A repo-wide grep confirms these three files are the *only* VM tests with breaking
assertions. A fourth file, `tests/cli/braid-smartd-alert.py`, still *passes* but
its two broad `"SMART" in output` checks are weakened by the new always-present
per-disk `SMART:` line -- change 4 tightens them to the actual cause string. That
is coverage hardening, not a failure fix, but it stems from the same `smart` split
and belongs with these edits.

## Changes

### 1. `tests/cli/braid-status.py`

- Phase 1 (human), the `Errors:` assertion:
  - `assert "Errors:" in output, f"Expected 'Errors:':\n{output}"`
  - -> `assert "btrfs:" in output, f"Expected 'btrfs:':\n{output}"`
  - **Add** (covers the new line): `assert "SMART:" in output, f"Expected 'SMART:':\n{output}"`
- Phase 2 (`--json`), the per-disk key check:
  - `assert "errors" in disk, f"Disk missing errors: {disk}"`
  - -> `assert "btrfs_errors" in disk, f"Disk missing btrfs_errors: {disk}"`
  - **Add**: `assert "smart" in disk, f"Disk missing smart: {disk}"`

### 2. `tests/cli/braid-status-rust.py`

- Phase 2 "Healthy RAID1 summary" (human):
  - `assert "Errors:" in output, f"Expected 'Errors:':\n{output}"`
  - -> `assert "btrfs:" in output, f"Expected 'btrfs:':\n{output}"`
  - **Add**: `assert "SMART:" in output, f"Expected 'SMART:':\n{output}"`
- Phase 2 "Healthy JSON" per-disk loop (three references to the renamed key):
  - `assert "errors" in d, f"Missing errors: {d}"` -> `assert "btrfs_errors" in d, f"Missing btrfs_errors: {d}"`
  - `assert d["errors"] is not None, f"Expected errors object: {d}"` -> `assert d["btrfs_errors"] is not None, f"Expected btrfs_errors object: {d}"`
  - `assert key in d["errors"], f"Missing errors.{key}: {d}"` -> `assert key in d["btrfs_errors"], f"Missing btrfs_errors.{key}: {d}"`
  - **Add** after the loop's status check: `assert "smart" in d, f"Missing smart: {d}"`
- No change to the Degraded phase: it asserts the substring `"disk offline -- not
  in pool"`, which still appears (now in the `btrfs:`/`SMART:` unknown-reason
  lines) -- the reason text is unchanged, only the `Errors:` label moved.

### 3. `tests/cli/braid-ack-cleanup-pending.py`

The "status reports the cleanup-pending cause" subtest checks `"SMART" not in out`
to confirm the smartd flag was removed and no SMART alert *cause* surfaces. That
literal now collides with the always-present per-disk `SMART:` detail line, so it
must target the actual alert-cause string instead (the smartd cause renders as
`  - SMART health warning` via `cli/src/alert.rs` `AlertCause::SmartdAlert`):

- `assert "SMART" not in out, (` ... `"smartd flag was removed; only the sentinel should surface, got: {out}"` `)`
- -> `assert "SMART health warning" not in out, (` ... `"smartd flag was removed; no SMART alert cause should surface, got: {out}"` `)`

This preserves the test's intent (no smartd-driven alert cause) while tolerating
the per-disk `SMART:` line. The phrase `SMART health warning` cannot appear in a
per-disk line (which renders at most `SMART:   warning (...)`), so there is no
false positive. The adjacent comment (lines ~86-90, "SMART must not appear...")
should be reworded to "no SMART alert *cause* must appear" to match.

### 4. `tests/cli/braid-smartd-alert.py` (coverage hardening, not a current failure)

Two positive assertions check `"SMART" in output` to confirm the smartd
`AlertCause::SmartdAlert` cause renders. The new always-present per-disk `SMART:`
line lets the mounted check pass even if that cause stops rendering, so both
should require the exact cause string (`  - SMART health warning`, from
`cli/src/status.rs#format_status_human`):

- "After smartd alert: status shows SMART warning" subtest (pool mounted -- the
  genuinely-masked one):
  - `assert "SMART" in output, f"Expected SMART cause, got: {output}"`
  - -> `assert "SMART health warning" in output, f"Expected SMART cause, got: {output}"`
- "Pool offline with smartd alert: status shows ALERT" subtest (pool unmounted, so
  the disk list is empty and no per-disk `SMART:` line renders today -- not masked,
  but tightened for consistency and future-proofing):
  - `assert "SMART" in output, f"Expected SMART cause, got: {output}"`
  - -> `assert "SMART health warning" in output, f"Expected SMART cause, got: {output}"`

Both are safe: each subtest already asserts `"ALERT" in output` with the smartd
flag as the sole alert source, so the `  - SMART health warning` cause is
guaranteed to render. The cause loop sits in the mount-independent ALERT banner,
so the string appears in the offline case too.

## Notes on the added `SMART:` / `smart` assertions

The new assertions are safe in the VM: present virtio disks render `SMART:
unknown` with no parenthetical, which means `DiskReport.smart` is `Some(Unknown
probe)` (a `None` smart with no absent-reason would render nothing). Since `smart`
is `Some`, it serializes, so `"smart" in disk` holds. They're included because the
request explicitly covers the new `smart` key -- leaving the line the commit added
untested would be a coverage gap. If a minimal diff is preferred, the renames
alone are sufficient to make the tests pass; the `Add` lines can be dropped.

## Out of scope

- The two unrelated failures above (`braid-idle`, `braid-module-disabled`).
- Docs/README sync for the new `btrfs:`/`SMART:` format -- that belongs to the
  feature commit, not this test fix. (Spot-check that `efdf85b1` already covered
  it; flag separately if not.)

## Verification

1. Run the affected VM tests (named args go to `_build-checks`):
   ```
   just test-vm braid-status braid-status-rust braid-ack-cleanup-pending braid-smartd-alert
   ```
   All four should build/pass. (Runs on macOS via the aarch64-darwin linux
   builder, per AGENTS.md.)
2. Optional sanity check that no other VM `.py` still references the old format:
   ```
   grep -rnE '"Errors:"|"errors"|\["errors"\]' tests/
   ```
   should return nothing after the edits.
3. The unrelated `braid-idle` and `braid-module-disabled` failures are expected to
   remain and are intentionally not addressed here.
