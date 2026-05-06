# Drop `lock_forget_devices`, route close decisions through `LockPlan.open_mappers`

## Context

`plan_lock` (cli/src/lock.rs:434-439) builds `open_mappers` -- the
membership mapper names filtered by `fs.exists` -- and then hands it
only to `compile_lock_steps`. The set is discarded before the plan
returns. `LockPlan::execute` later calls
`lock_forget_devices(fs, membership, orphan_mappers)`
(cli/src/lock.rs:262), which re-derives that same set by re-walking
`membership.disks.keys()` and re-applying `fs.exists`
(cli/src/lock.rs:89-107). The membership close loop at
cli/src/lock.rs:296-335 *also* re-derives the close decision live
from `fs.exists`. Three independent answers, one question: "which
membership mappers are we about to close?"

The deeper problem is not just redundancy. With the close loop using
live `fs.exists` and the forget call using a precomputed set, a mapper
that *newly appears* between plan and execute would slip past the
forget call and still get closed by the loop -- reviving the
cryptsetup-close-btrfs-held race that
`btrfs device scan --forget` exists to prevent
(see `tests/repro/cryptsetup-close-btrfs-held.py`). A naive "consume
`open_mappers` only in the forget build" refactor preserves that gap.

The fix: make `LockPlan.open_mappers` the planned close decision for
the membership close loop too, so the forget set and the close set
have one source. `fs.exists` survives only as a disappearance guard
right before `cryptsetup close` (a real TOCTOU concern: a mapper
listed in the plan can vanish if something else closed it in the
window between plan and execute, and `cryptsetup close` on a missing
mapper is a noisy no-op).

This aligns with the dry-run-preview-as-source-of-truth principle in
[`docs/decisions/022-dry-run-preview-model.md`](docs/decisions/022-dry-run-preview-model.md):
the work `execute` does should match what the preview promised, not
deviate based on live state at execute time.

A more sweeping pivot considered ("merge `open_mappers` and
`orphan_mappers` into a single `LockPlan.close_set` and iterate it in
the close loops") is rejected because the membership close loop emits
a per-disk `[ok] disk {name}: already closed` diagnostic for disks
that were closed at plan time (cli/src/lock.rs:329-334). Driving the
close loop off a unified close-set drops that diagnostic for the
partial-state case (e.g. 2-of-3 disks open at plan time -- the third
disk silently disappears from the output) unless we re-introduce a
membership iteration anyway. The `open_mappers` + `orphan_mappers`
pair preserves both the planning channel and the per-disk diagnostic.

## Fix

All edits are in `cli/src/lock.rs`.

### 1. Carry `open_mappers` in `LockPlan`

Add a `pub open_mappers: Vec<String>` field next to the existing
`orphan_mappers` field (cli/src/lock.rs:170-176). Bare mapper names
(no `/dev/mapper/` prefix), already filtered by `fs.exists` at plan
time -- mirroring `orphan_mappers` exactly. Refresh the struct's doc
comment to name both planned sets and to record the new invariant:
the membership close decision and the forget set are driven by
`open_mappers`; `fs.exists` at execute time is a disappearance guard
only.

### 2. Populate it in `plan_lock`

`open_mappers` is already computed at cli/src/lock.rs:434-439 and
handed to `compile_lock_steps`. Thread it into the returned `LockPlan`
literal (cli/src/lock.rs:462-468). No change to the compute itself;
no change to `compile_lock_steps`.

### 3. Drive the membership close loop off `open_mappers`

Rewrite cli/src/lock.rs:296-335 so the close decision is plan-driven.
The loop still iterates `membership.disks.keys()` so per-disk
diagnostics ("already closed", "locked", "already closed (vanished)")
remain visible for every membership disk -- but `fs.exists` no longer
gates the close vs already-closed branch. Sketch:

```rust
let open_set: std::collections::HashSet<&str> =
    self.open_mappers.iter().map(String::as_str).collect();

let mut all_already_closed = true;
for name in membership.disks.keys() {
    let mn = mapper_name(name);
    if !open_set.contains(mn.0.as_str()) {
        // Plan-time: this disk was already closed. Honor the plan;
        // don't close even if it has reappeared since.
        eprint!("{}", line(StatusTag::Ok,
            &format!("disk {name}: already closed")));
        continue;
    }
    let mapper_path = format!("/dev/mapper/{}", mn.0);
    if !fs.exists(&mapper_path) {
        // Plan-time it was open; vanished by execute time. Treat as
        // already closed to keep `cryptsetup close` from emitting a
        // noisy "no such mapper" failure.
        eprint!("{}", line(StatusTag::Ok,
            &format!("disk {name}: already closed")));
        continue;
    }
    eprint!("{}", line(StatusTag::Wait,
        &format!("disk {name}: locking...")));
    match close_mapper_with_retry(runner, sleeper, &mn.0, color_enabled) {
        Ok(()) => { /* ok line as today */ }
        Err(CloseMapperError::DeviceBusy(msg)) if umount_error.is_some() => {
            /* warn line as today */
        }
        Err(e) => { /* fail line + first_mapper_error capture as today */ }
    }
    all_already_closed = false;
}
```

Notes on equivalence:

- The "already closed" message body is unchanged -- both the
  not-in-plan and vanished-since-plan branches use the same wording so
  the test surface keeps matching.
- `all_already_closed` is set to false only when we actually invoke
  `close_mapper_with_retry`. Matches today's "did the lock do real
  membership work?" flag semantics.
- Error-handling shape (DeviceBusy-when-umount-stuck warn,
  first_mapper_error capture) is unchanged; only the gate on entering
  the close branch moves from `fs.exists` to `open_set.contains`.

### 4. Inline the forget-list build using the same planned set

Replace the `lock_forget_devices` call at cli/src/lock.rs:262 with a
single chained iterator over `self.open_mappers ∪ self.orphan_mappers`,
prefixed with `/dev/mapper/`, filtered by `fs.exists` for the
disappearance guard:

```rust
let forget_devs: Vec<String> = self
    .open_mappers
    .iter()
    .chain(self.orphan_mappers.iter())
    .map(|m| format!("/dev/mapper/{m}"))
    .filter(|p| fs.exists(p))
    .collect();
```

The surrounding `if !forget_devs.is_empty()` guard and the existing
match on the runner result are unchanged. Forget set and close set
are now driven by the same `open_mappers ∪ orphan_mappers`, with the
identical disappearance guard. The "newly-appeared mapper closed
without forget" race is closed: a mapper not in `open_mappers` is
neither forget'd nor closed.

### 5. Delete `lock_forget_devices`

Remove the function and its doc comment (cli/src/lock.rs:85-107).
After step 4 it has no callers.

### Orphan close loop: untouched

`self.orphan_mappers` already drives the orphan close loop iteration
(cli/src/lock.rs:342-384), and `fs.exists` already plays only the
disappearance-guard role there (cli/src/lock.rs:344). Symmetric with
the new membership shape; no change needed.

## Behavior change to flag

A mapper that newly appears in `/dev/mapper` between plan and execute
is no longer closed by `LockPlan::execute`. Today's behavior is to
close it (live-fs.exists wins). After this change, `execute` honors
the plan and leaves the new mapper alone -- which is the safe,
race-free outcome and matches what the dry-run preview promised. The
window is microseconds inside `cmd_lock_impl`, so this is observable
only via constructed unit tests, but it is the load-bearing behavior
change.

## Test impact

### New unit test (required)

The reviewer asked for explicit coverage of the planned-set contract.
Add a focused test next to `lock_happy_path_unmounts_and_closes`:

```rust
/*
 * Intent: `LockPlan::execute` honors the planned `open_mappers` and
 *   does NOT close a membership mapper that appeared in /dev/mapper
 *   only after planning.
 * Why it exists: closing a mapper that wasn't in the plan reopens the
 *   cryptsetup-close-btrfs-held race -- the forget call's argv is
 *   plan-derived, so an unplanned close would race against a stale
 *   btrfs scan reference. This pins "execute follows the plan, not
 *   live state" for the membership close loop.
 * Scenario: plan_lock runs against a fs where braid-aaa is closed
 *   (open_mappers empty); between plan and execute braid-aaa
 *   reappears; execute must NOT issue CryptsetupClose for braid-aaa.
 */
#[test]
fn execute_does_not_close_membership_mapper_absent_from_plan() {
    // plan_lock with mounted pool, no mappers present -> open_mappers empty
    let runner = with_fsid_probe_mocks(MockRunner::default()
        .with_output(
            CmdRequest::MountpointCheck { path: MountPoint("/mnt/storage".into()) },
            ok_raw("mountpoint -q /mnt/storage"),
        )
        .with_output(
            CmdRequest::Umount { mount_point: MountPoint("/mnt/storage".into()) },
            ok_raw("umount /mnt/storage"),
        ));
    let plan_fs = MockFs::new(&[]); // no mappers at plan time
    let config = test_config();
    let membership = test_membership();

    let plan = plan_lock(&runner, &plan_fs, &config, &membership)
        .expect("plan_lock should succeed");
    assert!(plan.open_mappers.is_empty(),
        "precondition: plan should record no membership opens");

    // Execute against a fs where braid-aaa has appeared since planning.
    let execute_fs = MockFs::new(&["/dev/mapper/braid-aaa"]);
    let recording = RecordingRunner::new(runner);
    plan.execute(&recording, &execute_fs, &NoopSleeper, &membership)
        .expect("execute should succeed without closing the unplanned mapper");

    assert!(recording.close_calls().is_empty(),
        "execute must not close mappers absent from open_mappers; got {:?}",
        recording.close_calls());
    // The forget list also derives from open_mappers ∪ orphan_mappers,
    // so the absence of any forget call is the right pin: an empty-argv
    // forget would be kernel-global (see
    // dry_run_lock_forget_step_omitted_when_no_mappers at
    // cli/src/lock.rs:2168) and must never be issued.
    assert!(recording.forget_calls().is_empty(),
        "execute must not invoke forget when open_mappers is empty; got {:?}",
        recording.forget_calls());
}
```

Test scaffolding (`MockFs`, `RecordingRunner`, `with_fsid_probe_mocks`,
`NoopSleeper`, `test_config`, `test_membership`, `ok_raw`,
`mounted_runner`) is already in `cli/src/lock.rs`. The test only adds
new logic; no fixture changes.

### Existing unit tests

- `lock_forget_includes_orphan_mappers` (cli/src/lock.rs:2242-2293)
  is the load-bearing forget-list contract test. The new inline
  forget build produces an identical argv for that scenario
  (`open_mappers = [braid-aaa, braid-bbb]`, `orphan_mappers =
  [braid-ccc]`, all present in fs); test stays green.
- `lock_happy_path_unmounts_and_closes` (lock.rs:739-760),
  `lock_already_locked` (lock.rs:762-776), `lock_partial_state`
  (lock.rs:778-805): exercise the membership close loop. Each test
  runs `plan_lock` then execute via `cmd_lock_impl`; the same fs is
  used for both, so `open_mappers` matches `fs.exists` and behavior
  is unchanged.
- `lock_closes_orphaned_mapper` (lock.rs:1077-1126) and the dry-run
  preview tests: unaffected.
- All other lock unit tests at lock.rs:806+ that go through
  `cmd_lock_impl`: unaffected for the same reason
  (`open_mappers` matches `fs.exists` because the fs is unchanged
  between plan and execute).

### VM tests

- `tests/cli/braid-lock.py`, `tests/cli/braid-lock-orphan.py`: real-run
  output and forget-call scope are unchanged in the steady-state cases
  these tests exercise; stay green.
- The cryptsetup-close-btrfs-held repro
  (`tests/repro/cryptsetup-close-btrfs-held.py`): the race this exists
  to test is unchanged; the planned-set contract makes the bug class
  *less* likely, but the test path itself is untouched.

### `LockPlan` literal construction

Only one `LockPlan { ... }` literal exists -- in `plan_lock` itself
(cli/src/lock.rs:462). No tests construct `LockPlan` directly, so the
new field requires no test-side updates beyond the new dedicated test.

## Files

- `cli/src/lock.rs`
  - add `open_mappers: Vec<String>` field to `LockPlan` and refresh its
    doc comment (lines 170-176);
  - thread `open_mappers` into the `LockPlan` literal in `plan_lock`
    (lines 462-468);
  - rewrite the membership close loop (lines 296-335) to gate on
    `open_set.contains(...)` with `fs.exists` as a disappearance guard;
  - inline the forget-list build at `LockPlan::execute` (line 262),
    replacing the `lock_forget_devices` call;
  - delete `lock_forget_devices` and its doc comment (lines 85-107);
  - add the `execute_does_not_close_membership_mapper_absent_from_plan`
    unit test next to `lock_happy_path_unmounts_and_closes`.

No NixOS module changes, no doc/decision changes, no VM-test changes.

## Out of scope

- Unifying with `relock_and_remount` in cli/src/recover.rs:2837-2842,
  which has the same forget-list shape inlined. There is no plan
  layer there, so no analogous redundancy to consolidate.
- Merging `open_mappers` and `orphan_mappers` into a single
  `LockPlan.close_set` field. Rejected above (drops the "already
  closed" diagnostic in the partial-state case).

## Verification

1. `just test-rust` -- all lock unit tests must stay green, including
   the new
   `execute_does_not_close_membership_mapper_absent_from_plan` and the
   `lock_forget_includes_orphan_mappers` regression guard for the
   forget-list contract.
2. `just test-vm braid-lock braid-lock-orphan` -- the no-op-dry-run VM
   test plus the orphan VM test must stay green.
