# Plan: atomic preserve-first corrupt-latch sidecar and surface quarantine failures

## Context

ADR 014's "Corrupt latch recovery" section
(`docs/decisions/014-alerts.md:103-111`) promises that when
`cmd_monitor` encounters an unreadable `alert-latch.json` it
"preserves the bad bytes for forensics until an ack path that can
safely clean them up." Two implementation gaps in
`load_alert_latch_or_quarantine` (`cli/src/alert.rs:316-326`)
quietly break that promise:

1. **Sidecar clobber on repeat corruption.** Line 322 uses
   `std::fs::rename(latch.json, latch.json.corrupt)`. Unix `rename(2)`
   atomically replaces the destination, so if a `.corrupt` sidecar
   already exists from a prior quarantine, its bytes are destroyed.
   The first corruption event -- the original failure before the
   system recovered and re-corrupted -- is the most forensically
   valuable, and that is exactly the one this code can lose. A naive
   "check `dst.exists()` then `rename`" fix is still
   check-then-clobber: a concurrent writer that creates the sidecar
   between the check and the rename can still have its bytes
   replaced. The correct primitive is `link(2)` (Rust:
   `std::fs::hard_link`), which atomically fails with `EEXIST` /
   `io::ErrorKind::AlreadyExists` if the destination exists -- no
   TOCTOU window. Both paths sit under the same `state_paths.root`
   directory (`/var/lib/braid` in production,
   `cli/src/state_paths.rs:19-46`), so the cross-filesystem
   restriction of `link(2)` (EXDEV) never applies.

2. **Silent rename I/O failure.** The same line discards the rename
   `Result` with `let _ =`. A real I/O failure (permission denied on
   `/var/lib/braid`, ENOSPC, stale NFS, etc.) is invisible to the
   operator even though both call sites
   (`cli/src/monitor.rs:26`, `cli/src/monitor.rs:125`) already accept
   an `Option<String>` detail they fold into a `ComputationError`
   cause. After the silent rename failure, the caller's subsequent
   `save_alert_latch` atomic-writes a fresh latch over the still-bad
   `latch.json`, and the bad bytes are lost there too -- so the
   `let _ =` is doubly load-bearing for forensic preservation.

The proposed fix preserves the FIRST sidecar (not all sidecars via
timestamped variants) so:

- `cli/src/state_paths.rs:39-41`'s single `alert_latch_corrupt()`
  path stays canonical.
- `cli/src/ack.rs:187`'s single-file cleanup
  (`remove_alert_latch_corrupt`) keeps working unchanged -- no glob,
  no new path API, no per-corruption clutter.
- Matches ADR 014's singular "the bad bytes" wording.

The probability of double-corruption is genuinely low because
`save_alert_latch` uses `atomic_write` (`cli/src/state_io.rs:53-75`,
temp + rename), so braid itself does not produce partial writes;
the realistic vectors are external tampering, filesystem damage, or
a manual edit. Severity is medium: silent forensic loss with no
operator-visible signal.

## Approach

Replace the body of
`load_alert_latch_or_quarantine` (`cli/src/alert.rs:316-326`) so
that on a read/parse failure it delegates the quarantine move to a
small private helper, `quarantine_corrupt_latch`, that uses an
atomic two-step preserve-first primitive instead of `rename`:

1. **`std::fs::hard_link(src, dst)`** -- creates a new hard link to
   the bad-bytes inode at the sidecar path. On Unix this maps to
   `link(2)`, which atomically fails with `EEXIST`
   (`io::ErrorKind::AlreadyExists`) if `dst` already exists. There
   is no `dst.exists()` check, so no TOCTOU window.
2. **`std::fs::remove_file(src)`** -- removes the source path. The
   bad-bytes inode survives via the sidecar link.

The helper returns:

- `None` when both steps succeed (clean quarantine).
- `Some(detail)` when forensic preservation degrades:
  - `AlreadyExists` from `hard_link` -> prior sidecar preserved;
    new corrupt bytes were not separately captured. The caller's
    next `save_alert_latch` will overwrite latch.json anyway.
  - Any other `Err` from `hard_link` -> surface the error.
  - `Err` from `remove_file` after a successful `hard_link` ->
    surface a "quarantine succeeded but source removal failed"
    detail. The forensic sidecar is still intact (different inode
    reference), and the caller's next `save_alert_latch`
    atomic-writes a fresh latch over the source path regardless,
    so this failure is operationally cosmetic but worth surfacing
    so the operator knows about it.

The outer function concatenates the parse error and the quarantine
detail with `; ` and returns the combined string via the existing
`Option<String>` return, so callers at `monitor.rs:26` and
`monitor.rs:125` need no changes -- they already fold whatever
detail they get into a single `ComputationError`. The existing
`eprintln!` at `alert.rs:321` stays as-is (operator-visible stderr
on quarantine attempt).

## Critical files to modify

- **`cli/src/alert.rs:316-326`** -- replace the body of
  `load_alert_latch_or_quarantine`; add a new private function
  `quarantine_corrupt_latch(paths: &StatePaths) -> Option<String>`
  immediately below it. Keep the public signature
  `(Option<AlertState>, Option<String>)` unchanged.

- **`cli/src/alert.rs` test module** -- add two unit tests beside
  the existing `quarantine_moves_corrupt_file_aside_and_reports_detail`
  at `cli/src/alert.rs:617` (see Tests below).

- **`tests/cli/braid-monitor.py`** -- add one VM subtest after the
  existing corrupt-latch subtest at line 134 (see Tests below).

- **`docs/decisions/014-alerts.md`** -- append one sentence to the
  "Corrupt latch recovery" bullet at lines 107-111 documenting the
  preserve-first rule and the lost-evidence detail folded into the
  `ComputationError`.

## Sketch (alert.rs body replacement)

```rust
pub fn load_alert_latch_or_quarantine(paths: &StatePaths) -> (Option<AlertState>, Option<String>) {
    match load_alert_latch(paths) {
        Ok(opt) => (opt, None),
        Err(e) => {
            let parse_detail = e.to_string();
            eprintln!("warning: alert latch unreadable -- quarantining: {parse_detail}");
            let detail = match quarantine_corrupt_latch(paths) {
                Some(quarantine_detail) => format!("{parse_detail}; {quarantine_detail}"),
                None => parse_detail,
            };
            (None, Some(detail))
        }
    }
}

/// Move the unreadable latch to `alert-latch.json.corrupt` for forensics.
/// Uses hard_link + remove_file (not rename) so that an already-existing
/// sidecar is detected atomically by link(2)'s EEXIST -- no TOCTOU window
/// where a concurrent writer could replace the prior bytes. The first
/// sidecar wins; the new bad bytes are left to be overwritten by the
/// caller's next save_alert_latch. Returns `Some(detail)` whenever
/// forensic preservation degrades (prior sidecar present, hard_link I/O
/// failure, or remove_file I/O failure after a successful link); the
/// caller folds it into the ComputationError so operators see the
/// lost-evidence signal in `braid status`.
fn quarantine_corrupt_latch(paths: &StatePaths) -> Option<String> {
    let src = paths.alert_latch_json();
    let dst = paths.alert_latch_corrupt();
    match std::fs::hard_link(&src, &dst) {
        Ok(()) => match std::fs::remove_file(&src) {
            Ok(()) => None,
            Err(e) => Some(format!(
                "quarantined corrupt latch but failed to remove source: {e}"
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Some(
            "prior alert-latch.json.corrupt sidecar exists -- new corrupt bytes were not separately preserved".to_string()
        ),
        Err(e) => Some(format!("failed to quarantine corrupt latch: {e}")),
    }
}
```

## Tests

Each Rust unit test gets a contiguous block of `//` line-comment
preamble lines directly above the `#[test]` attribute (Intent /
Why it exists / Scenario), matching the canonical form in
[`docs/testing.md`](../../docs/testing.md) section "Preamble:
literal `//` line-comment form" and the existing convention in
`cli/src/ups.rs:244-291`. Do NOT use `/* ... */` block comments.
The Python VM-test preamble in `tests/cli/braid-monitor.py` uses
`#` line comments matching the existing subtest style in that file.

### Unit test 1: prior sidecar is preserved

`quarantine_preserves_first_corrupt_sidecar` in
`cli/src/alert.rs` test module:

1. `tempfile::tempdir()` → `StatePaths::custom(...)`.
2. Write bytes `b"first garbage"` to `paths.alert_latch_json()`.
3. Call `load_alert_latch_or_quarantine(&paths)`; assert
   `paths.alert_latch_corrupt()` holds `b"first garbage"`,
   `paths.alert_latch_json()` is absent (removed after the
   successful `hard_link`), and the returned detail is `Some(_)`
   carrying just the parse error (single-segment, no quarantine
   suffix because the helper returned `None`).
4. Write bytes `b"second garbage"` to `paths.alert_latch_json()`
   (simulating a second corruption between monitor cycles).
5. Call `load_alert_latch_or_quarantine(&paths)` again. Assert:
   - returned `Option<AlertState>` is `None`,
   - returned detail is `Some(s)` where
     `s.contains("prior alert-latch.json.corrupt sidecar exists")`
     AND `s` contains the parse error,
   - sidecar STILL contains `b"first garbage"` (first event
     preserved; `hard_link` returned `AlreadyExists` without
     touching the existing inode),
   - `paths.alert_latch_json()` still exists with
     `b"second garbage"` -- the helper did NOT remove it (the
     caller's next `save_alert_latch` atomic-writes over it).

This is the test that fails when the bug is reintroduced.

### Unit test 2: hard_link I/O failure folds into detail

`quarantine_link_failure_surfaces_in_detail` in
`cli/src/alert.rs` test module, gated `#[cfg(unix)]`:

1. `tempfile::tempdir()` → `StatePaths::custom(...)`.
2. Write `b"not json"` to `paths.alert_latch_json()`.
3. `std::fs::set_permissions` on the state root to mode 0o500
   (read+execute, no write). `link(2)` needs write on the parent
   directory of the destination to create the new directory entry,
   so `hard_link` will fail with EACCES.
4. Call `load_alert_latch_or_quarantine(&paths)`. Assert:
   - returned detail is `Some(s)` where
     `s.contains("failed to quarantine corrupt latch")`,
   - latch.json still exists with `b"not json"` (hard_link failed
     before remove_file was reached).
5. **Cleanup**: restore the dir to mode 0o700 before `tempdir`
   drops so the `Drop` cleanup can recurse-delete. Use a small RAII
   guard struct (`struct RestorePerms { dir: PathBuf }` whose
   `Drop` impl calls `set_permissions` back to 0o700) so a panic
   between steps still restores permissions.

Note: this is the first test in `cli/src/alert.rs` to inject a
`std::fs` failure. Existing alert tests only use `tempfile::tempdir`
+ direct writes. We are deliberately not adopting `MockFs` (used in
`cli/src/preflight.rs`, `cli/src/idle.rs`) because a single
permission-based test is much smaller than a fs-trait
abstraction for one function.

### VM subtest 3: repeat corruption preserves first sidecar end-to-end

In `tests/cli/braid-monitor.py`, add a new `with subtest(...)`
block after the existing
`with subtest("corrupt alert latch is fail-loud-quarantined (mounted)"):`
at line 134.

```
# Intent: When the alert latch becomes corrupt a second time before
#   ack, the first .corrupt sidecar must be preserved -- ADR 014
#   guarantees the bad bytes survive for forensics until ack, and
#   the first corruption event is the most valuable snapshot.
# Why it exists: Pre-fix, std::fs::rename atomically replaced the
#   .corrupt sidecar on every quarantine, silently destroying the
#   original failure event's bytes whenever a second corruption
#   occurred before braid ack.
# Scenario: Operator misses the first ALERT; meanwhile the latch
#   corrupts again (FS damage, manual edit, slow tampering). The
#   second monitor cycle must keep the first sidecar and surface
#   the lost-evidence condition in braid status's JSON output.

machine.succeed("printf 'first corruption' > /var/lib/braid/alert-latch.json")
rc = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
assert rc == "1"
first_sidecar = machine.succeed("cat /var/lib/braid/alert-latch.json.corrupt")
assert first_sidecar == "first corruption"

# Second corruption: overwrite the freshly written valid latch.
machine.succeed("printf 'second corruption' > /var/lib/braid/alert-latch.json")
rc2 = machine.succeed("set +e; braid monitor; echo $?").strip().splitlines()[-1]
assert rc2 == "1"

# Sidecar still holds the FIRST event's bytes.
preserved = machine.succeed("cat /var/lib/braid/alert-latch.json.corrupt")
assert preserved == "first corruption", \
    f"first sidecar must survive second quarantine, got {preserved!r}"

# braid status surfaces the lost-evidence condition.
status_json = machine.succeed("braid status --json")
# Assert the ComputationError detail names the prior-sidecar condition.
# (Exact substring matches the helper's emitted string.)
assert "prior alert-latch.json.corrupt sidecar exists" in status_json

# ack clears both files as before.
machine.succeed("braid ack")
machine.fail("test -f /var/lib/braid/alert-latch.json")
machine.fail("test -f /var/lib/braid/alert-latch.json.corrupt")
```

## Existing utilities reused

- `paths.alert_latch_json()` / `paths.alert_latch_corrupt()`
  (`cli/src/state_paths.rs:35-41`) -- unchanged.
- `alert::remove_alert_latch_corrupt` (`cli/src/alert.rs:341-347`)
  -- unchanged; ack cleanup at `cli/src/ack.rs:187` keeps using
  the single canonical path.
- `merge_into_latch` (`cli/src/alert.rs:360-378`) and
  `same_cause_key` (`cli/src/alert.rs:381-392`) -- unchanged;
  the existing ComputationError-collapsing behavior at
  `monitor.rs:26` and `monitor.rs:125` already folds the
  combined detail correctly.
- `atomic_write` (`cli/src/state_io.rs:53-75`) -- unchanged;
  callers still save fresh latches via the safe temp+rename
  helper.

## Out of scope

- The three other `let _ = std::fs::create_dir_all(...)` sites
  (`cli/src/pool.rs:665`, `cli/src/pool.rs:699`,
  `cli/src/mount.rs:793`) and the `let _ = std::fs::remove_file(...)`
  at `cli/src/luks.rs:469` are NOT touched -- they are defensible
  best-effort cleanups whose failure is either non-actionable or
  re-surfaced by the immediately-following command. Only
  `alert.rs:322` is load-bearing for a documented forensic contract.

- An `exists() + rename` variant of the helper was considered and
  rejected because it remains check-then-clobber: a concurrent
  writer creating the sidecar between the existence check and the
  rename would still have its bytes replaced. `hard_link(src, dst)`
  followed by `remove_file(src)` is the atomic no-clobber primitive
  -- `link(2)` returns `EEXIST` atomically -- and is the right
  shape regardless of whether the race is exploitable from
  in-tree braid today (the pool lock at `/run/braid-pool.lock`
  already serializes monitor/ack/add/remove writers, but the
  atomic primitive prevents future regressions if any code path
  later touches `.corrupt` outside the lock).

- Timestamped sidecar variants (e.g.
  `alert-latch.json.corrupt.<unix-ts>`) are explicitly rejected:
  they would require teaching `cli/src/ack.rs:187`'s cleanup to
  glob `alert-latch.json.corrupt*` and would clutter
  `/var/lib/braid` on slow-leaking FS damage. The first sidecar
  is the highest-value snapshot; subsequent events live in the
  `ComputationError` detail.

- The `MockFs` trait used in `cli/src/preflight.rs`,
  `cli/src/idle.rs` is NOT adopted into `cli/src/alert.rs`. A
  permission-based unit test is much smaller than a fs-trait
  abstraction for a single helper.

## ADR update

In `docs/decisions/014-alerts.md`, the "Corrupt latch recovery"
bullet for `cmd_monitor` (lines 107-108) gets one appended sentence:

> Quarantine uses `hard_link` + `remove_file` (not `rename`) so
> that an already-existing sidecar is detected atomically by
> `link(2)`'s `EEXIST`. If a prior
> `alert-latch.json.corrupt` sidecar exists when another
> unreadable latch is encountered, the first sidecar is preserved
> (highest-value forensic snapshot -- the original failure event
> before the system recovered and re-corrupted) and the new
> corruption is surfaced only in the `ComputationError` detail.
> Any I/O failure during quarantine is likewise folded into the
> same detail rather than silently dropped.

## Verification

1. **`just test-rust`** -- runs both new unit tests against the
   modified helper. Expect: both pass; the existing
   `quarantine_moves_corrupt_file_aside_and_reports_detail`
   continues to pass (first-quarantine path unchanged).
2. **`just test-vm braid-monitor`** -- runs the new VM subtest
   end-to-end and re-validates the existing corrupt-latch coverage
   at `tests/cli/braid-monitor.py:134-158` and the lock-contended
   case at `tests/module/alert-state-lock.py:172-204`.
3. **Manual smoke (optional)** in a NixOS VM dev shell:
   ```
   printf 'a' > /var/lib/braid/alert-latch.json && braid monitor; \
   printf 'b' > /var/lib/braid/alert-latch.json && braid monitor; \
   cat /var/lib/braid/alert-latch.json.corrupt   # expect 'a'
   braid status --json | grep "prior alert-latch.json.corrupt"
   ```
