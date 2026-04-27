# Plan: fix silent latched-cause loss on corrupt `alert-latch.json`

## Context

`cli/src/alert.rs:243-246` currently swallows every failure mode of
`alert-latch.json` into `Option::None`:

```rust
pub fn load_alert_latch(paths: &StatePaths) -> Option<AlertState> {
    let contents = std::fs::read_to_string(paths.alert_latch_json()).ok()?;
    serde_json::from_str(&contents).ok()
}
```

The four callers (`monitor.rs:104`, `monitor.rs:114`, `status.rs:530`,
`ack.rs:16`) all treat `None` as "no latch -> no alert". This conflates three
distinct outcomes:

1. ENOENT -- the normal "no active alerts" case.
2. I/O error -- filesystem trouble.
3. JSON parse error -- the file is corrupt.

The bug bites in (3). When the latch file is unparseable, `cmd_monitor`
sees `existing_latch = None`, calls `merge_into_latch(None, &live_causes)`
(`alert.rs:276`), and writes the result back via `save_alert_latch`
(`monitor.rs:121`). The corrupt file is overwritten with a fresh slate
that contains only currently-live causes. **Any previously latched cause
that is no longer live is gone forever**, silently violating the "latched
until ack" invariant. Status sees the same `None` and reports "no alert",
so nobody notices.

`save_alert_latch` writes via `atomic_write` (`state_io.rs:53-75`), so
braid itself is unlikely to corrupt the file. The realistic vectors are
external tampering, manual edits, filesystem damage, or a future refactor
that drops atomicity. Severity is Medium because likelihood is low but
the failure mode is silent and the on-disk state is invariant-load-bearing.

There is already an in-tree precedent for the right shape:
`cli/src/membership.rs:81-101` distinguishes `MembershipError::NotFound`
from `MembershipError::Corrupt(path, detail)`. The fix below mirrors that
pattern, adapted for the fact that "absent" is a normal state for the
alert latch (unlike pool membership).

## Approach

Three changes:

1. **`load_alert_latch` becomes `Result<Option<AlertState>, LatchLoadError>`.**
   - `Ok(None)` -- file absent (normal: no active alerts).
   - `Ok(Some(state))` -- parsed.
   - `Err(LatchLoadError::Read(io::Error))` -- I/O failure.
   - `Err(LatchLoadError::Parse(serde_json::Error))` -- corrupt JSON.
2. **Add `load_alert_latch_or_quarantine`** as the monitor-side helper
   that does the side-effecting recovery (move corrupt file aside, return
   a detail string so the caller can plant a `ComputationError` cause).
3. **Each caller decides its own policy** for how to react to the typed
   error. Status surfaces it (read-only). Ack acknowledges it. Monitor
   quarantines and fail-louds it into the new latch.

This keeps the shared helper pure (`load_alert_latch` has no side
effects), and concentrates per-caller "fail-closed" policy at each call
site -- consistent with the prior feedback that caller-specific gating
belongs at call sites, not in shared helpers.

## Files to change

### `cli/src/alert.rs`

Replace `load_alert_latch` (lines 243-246) and add a new error type +
quarantine helper.

```rust
#[derive(Debug, thiserror::Error)]
pub enum LatchLoadError {
    #[error("read alert latch: {0}")]
    Read(#[from] std::io::Error),
    #[error("parse alert latch: {0}")]
    Parse(#[from] serde_json::Error),
}

pub fn load_alert_latch(paths: &StatePaths) -> Result<Option<AlertState>, LatchLoadError> {
    // Read bytes (not String) so invalid UTF-8 in a corrupt file surfaces as
    // a Parse error via serde_json, not as an io::Error::InvalidData wrapped
    // in Read. The Read/Parse split should reflect "filesystem failed" vs
    // "on-disk content is wrong", and non-UTF-8 bytes are the latter.
    let bytes = match std::fs::read(paths.alert_latch_json()) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(LatchLoadError::Read(e)),
    };
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// Variant for mutation paths: on read/parse failure, move the bad file
/// aside to `alert-latch.json.corrupt` (best-effort) and return a detail
/// string so the caller can emit a loud `ComputationError` cause.
/// On success behaves identically to `load_alert_latch`.
pub fn load_alert_latch_or_quarantine(
    paths: &StatePaths,
) -> (Option<AlertState>, Option<String>) {
    match load_alert_latch(paths) {
        Ok(opt) => (opt, None),
        Err(e) => {
            let detail = e.to_string();
            eprintln!("warning: alert latch unreadable -- quarantining: {detail}");
            let from = paths.alert_latch_json();
            let to = paths.alert_latch_corrupt();
            let _ = std::fs::rename(&from, &to);
            (None, Some(detail))
        }
    }
}
```

The `eprintln!` uses `--` (double hyphen) per the project CLI output style
rule.

### `cli/src/state_paths.rs`

Add `alert_latch_corrupt()` returning `<root>/alert-latch.json.corrupt`,
and extend the existing `production_resolves_expected_paths` /
`custom_resolves_under_given_root` tests to assert the new path.

### `cli/src/monitor.rs`

Replace both `load_alert_latch` call sites (lines 104, 114) with
`load_alert_latch_or_quarantine`. When the helper reports a quarantine
detail, surface it as a `ComputationError` cause through `merge_into_latch`.

**Important constraint on the error path** (lines 90-111): `merge_into_latch`
treats every `ComputationError` variant as the same key
(`alert.rs:296-307`'s `same_cause_key`), so a vector containing two distinct
`ComputationError`s collapses to whichever one is appended last. If the
latch is corrupt AND `compute_alert_state_with_devid_map` also fails, the
naive "prepend one, keep the other" approach silently drops the
latch-corruption detail. Combine both failures into a **single**
`ComputationError` whose `detail` concatenates them before calling
`merge_into_latch`.

Sketch of the happy-path block (replacing lines 113-124):

```rust
// 9. Load existing latch (quarantine corrupt file if needed)
let (existing_latch, latch_corrupt_detail) = alert::load_alert_latch_or_quarantine(paths);

// 9b. If latch was corrupt, surface it as a loud cause
let mut live_causes = live_causes;
if let Some(detail) = latch_corrupt_detail {
    live_causes.insert(
        0,
        AlertCause::ComputationError {
            detail: format!("previous alert latch was unreadable -- quarantined; {detail}"),
        },
    );
}

// 10. Merge: existing latch + live causes
let merged = merge_into_latch(existing_latch.as_ref(), &live_causes);
```

Sketch of the fail-closed block (replacing lines 100-110), combining details
into one cause:

```rust
let (existing_latch, latch_corrupt_detail) =
    alert::load_alert_latch_or_quarantine(paths);
let detail = match latch_corrupt_detail {
    Some(latch_detail) => format!(
        "alert computation failed: {e}; additionally, previous alert latch was unreadable -- quarantined; {latch_detail}"
    ),
    None => format!("alert computation failed: {e}"),
};
let error_causes = vec![AlertCause::ComputationError { detail }];
let merged = merge_into_latch(existing_latch.as_ref(), &error_causes);
if let Err(write_err) = alert::save_alert_latch(&merged, paths) {
    eprintln!("warning: failed to write alert latch: {write_err}");
}
return Err(MonitorError::UnmappedDevice(e));
```

### `cli/src/status.rs`

`resolve_alert_state` (lines 529-554) becomes:

```rust
pub(crate) fn resolve_alert_state(paths: &StatePaths) -> AlertState {
    let smartd_active = alert::smartd_alert_active(paths);

    let latch = match alert::load_alert_latch(paths) {
        Ok(opt) => opt,
        Err(e) => {
            // Fail loud: don't pretend "no alert" when we can't read the latch.
            // Status is read-only -- do not quarantine here; that's monitor's job.
            let mut causes = vec![AlertCause::ComputationError {
                detail: format!("alert latch unreadable -- {e}"),
            }];
            if smartd_active {
                causes.push(AlertCause::SmartdAlert);
            }
            return AlertState { active: true, causes };
        }
    };

    // ... existing match branches unchanged ...
}
```

The deliberate non-symmetry with monitor: status MUST stay read-only
(per the established design principle that read commands don't mutate
state), so it never moves the file aside.

### `cli/src/ack.rs`

`cmd_ack` (line 16) needs a three-way classification: parseable latch,
absent latch, and unreadable latch. Critically, an unreadable latch must
count as an active alert for gating purposes -- otherwise
`ack_offline` (lines 81-94) will see `has_alert = (latch_count > 0 ||
smartd_active) == false` and return `PoolNotMounted`, leaving the
corrupt file on disk. The user has no way to clear a corrupt latch with
the pool offline.

```rust
let latch = alert::load_alert_latch(paths);
let (latch_count, latch_corrupt) = match &latch {
    Ok(Some(s)) => (s.causes.len(), false),
    Ok(None) => (0, false),
    Err(e) => {
        eprintln!("warning: alert latch unreadable -- acknowledging anyway: {e}");
        (0, true)
    }
};
```

Then propagate `latch_corrupt` through to `ack_offline`:

```rust
fn ack_offline(latch_count: usize, latch_corrupt: bool, paths: &StatePaths)
    -> Result<(), AckError>
{
    let smartd_active = alert::smartd_alert_active(paths);
    let has_alert = latch_count > 0 || smartd_active || latch_corrupt;
    if !has_alert {
        return Err(AckError::PoolNotMounted);
    }
    alert::remove_alert_latch(paths)?;
    alert::remove_alert_latch_corrupt(paths)?;
    alert::remove_smartd_alert_flag(paths)?;
    stop_beeper();
    println!("acknowledged current alerts");
    Ok(())
}
```

The mounted-pool success path also calls `remove_alert_latch_corrupt`
after `remove_alert_latch` (`ack.rs:66`) so ack truly clears the slate.

Add `remove_alert_latch_corrupt` next to `remove_alert_latch`
(`alert.rs:253-259`) using the same NotFound-tolerant pattern.

## Tests

### Unit tests in `cli/src/alert.rs` (mod tests)

Each test gets the literal `/* Intent / Why it exists / Scenario */`
block-comment preamble per AGENTS.md.

1. **`load_alert_latch_absent_returns_ok_none`** -- tempdir with no
   latch file; assert `load_alert_latch_at(...)` returns `Ok(None)`.
   *(Add a path-taking variant `load_alert_latch_at(path)` mirroring
   `load_acked_stats_at` for testability.)*
2. **`load_alert_latch_corrupt_returns_parse_err`** -- write `"not json"`
   to the latch path; assert `matches!(result, Err(LatchLoadError::Parse(_)))`.
   This is the test that fails when the bug is reintroduced -- the
   original code returns `None` (which would be `Ok(None)` after the
   refactor), so the typed assertion fails. Asserts on the variant per
   the prior feedback that propagation tests bind to typed shape, not
   message substrings.
3. **`load_alert_latch_valid_returns_ok_some`** -- write a serialized
   `AlertState` and assert round-trip.
4. **`quarantine_moves_corrupt_file_aside_and_reports_detail`** -- write
   garbage to latch path, call `load_alert_latch_or_quarantine`, assert:
   - returns `(None, Some(detail))` with non-empty detail
   - latch path no longer exists
   - corrupt path now exists with the original garbage contents

### Unit test in `cli/src/status.rs`

5. **`resolve_alert_state_surfaces_corrupt_latch_as_computation_error`**
   -- write garbage to latch path, call `resolve_alert_state`, assert
   returned `AlertState { active: true, causes }` contains exactly one
   `AlertCause::ComputationError { detail }` (plus `SmartdAlert` if the
   smartd flag is present, but the test should keep that flag absent
   for focus). Assert on variant + payload, not substrings.

### VM subtests in `tests/cli/braid-monitor.py`

CLI-contract grounding (verified against `cli/src/main.rs`):
- `Commands::Status` exits 0 unless `cmd_status` returns Err -- the
  presence of an alert does NOT change the exit code (`main.rs:414-428`).
- `Commands::Monitor` exits **1** on `MonitorResult::Alert(_)` and 2 on
  `Err(_)` (`main.rs:597-603`).
- `Commands::Ack` exits non-zero on `AckError`.

6. **`with subtest("corrupt alert latch is fail-loud-quarantined (mounted)"):`**
   - while the pool is mounted, write `b"not json"` to
     `/var/lib/braid/alert-latch.json`
   - run `braid monitor`; assert exit code 1 (because `cmd_monitor`
     returns `MonitorResult::Alert(...)` once the latch-corruption
     `ComputationError` is folded in)
   - assert `/var/lib/braid/alert-latch.json.corrupt` exists with the
     original garbage bytes
   - run `braid status`; assert exit 0 (status does not exit non-zero on
     alerts) AND assert the human/JSON output names the corruption
     cause -- e.g. `--json` output contains a `ComputationError` cause
     whose detail mentions "alert latch was unreadable"
   - run `braid ack`; assert exit 0; assert both
     `alert-latch.json` and `alert-latch.json.corrupt` are gone

7. **`with subtest("corrupt alert latch can be acknowledged with pool offline"):`**
   - unmount the pool
   - write `b"not json"` to `/var/lib/braid/alert-latch.json`
   - run `braid ack`; assert exit 0 (NOT `PoolNotMounted`); assert both
     `alert-latch.json` and `alert-latch.json.corrupt` are gone

Both subtests reuse the existing pool fixture; neither bundles other
concerns. Both stay inside the already-registered `braid-monitor.py`
check, so no flake.nix change is needed.

## Out of scope

- Atomic-write hardening: `atomic_write` already fsyncs and renames; the
  bug at hand is corrupt-input recovery, not corrupt-write prevention.
- Migrating other "silently swallow on corrupt" paths
  (`load_acked_stats_at` at `alert.rs:68-74` does the same thing for
  acked stats). That has different semantics -- losing acked stats
  re-fires already-acked alerts, which is annoying but not invariant
  violating. A separate plan if desired.
- Any change to `MonitorError`. The latch-corruption signal flows through
  the existing `ComputationError` cause channel, not as a new error
  variant -- monitor still returns `MonitorResult::Alert(...)` to its
  caller, and the cause shows up in status.

## Verification

- `cargo test -p braid-cli alert::tests::load_alert_latch` runs the four
  new unit tests in alert.rs.
- `cargo test -p braid-cli status::` runs the new status test.
- `cargo test -p braid-cli` for the full Rust suite.
- `just test-vm braid-monitor` for the VM subtest. Do not autonomously
  run `just test-all`; the user drives full-suite re-runs (per prior
  feedback).
- Manual sanity check: revert the `load_alert_latch` change but leave
  the new tests in place. Test #2 must fail (`Ok(None)` vs
  `Err(Parse(_))`); test #5 must fail (returns inactive empty state); VM
  test must fail (no `.corrupt` sidecar produced). This proves each test
  is at the failure layer, not a tautology.

## Decision doc update

Append a short subsection to `docs/decisions/014-alerts.md` documenting
the corrupt-latch recovery policy this plan establishes:

- `load_alert_latch` is typed (`Result<Option<AlertState>, LatchLoadError>`),
  distinguishing absent / read-failed / parse-failed.
- `cmd_status` is the read-only surface: it surfaces a corrupt latch as
  a `ComputationError` cause but never moves the file.
- `cmd_monitor` is the only path that quarantines a corrupt latch
  (rename to `alert-latch.json.corrupt`) and writes a fresh latch with a
  loud `ComputationError`.
- `cmd_ack` treats a corrupt latch as an active alert for gating
  purposes and clears both the live and `.corrupt` sidecar files.

This makes the new invariant discoverable in the same place readers go
for the broader alert design.

## Files touched

- `cli/src/alert.rs` -- new `LatchLoadError`, rewrite `load_alert_latch`
  (read bytes + `from_slice`), add `load_alert_latch_or_quarantine`,
  add `remove_alert_latch_corrupt`, add 4 unit tests + path-taking
  variant.
- `cli/src/state_paths.rs` -- add `alert_latch_corrupt()`, extend
  existing path tests.
- `cli/src/monitor.rs` -- update both call sites to use
  `load_alert_latch_or_quarantine`; combine compute-error and
  latch-corruption details into a single `ComputationError` on the
  fail-closed path.
- `cli/src/status.rs` -- update `resolve_alert_state` to surface
  corruption; add 1 unit test.
- `cli/src/ack.rs` -- update `cmd_ack` latch read; thread `latch_corrupt`
  into `ack_offline` gating; clean up `.corrupt` sidecar in both ack
  paths.
- `tests/cli/braid-monitor.py` -- add mounted + offline corrupt-latch
  subtests.
- `docs/decisions/014-alerts.md` -- append corrupt-latch recovery
  subsection.
