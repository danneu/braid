# recover: honor the planned mapper close set in the remount cycle

## Context

`braid lock` was just fixed (commit `4fcb6eb fix(lock): honor planned mapper
close set`) so the close set is computed once in `plan_lock` and then
consumed by both `btrfs device scan --forget` and the per-mapper close loop;
`fs.exists` is used only as a disappearance guard between plan and execute.

`braid recover` has the same class of bug in its remount cycle. Today:

- `plan_recover` already computes the planned close set:
  `cycle_close_names` at `cli/src/recover.rs:1106` (union members whose
  mapper currently exists, plus the cycle's reopen targets).
- Dry-run rendering correctly consumes those planned names in
  `RecoverWorkAction::RemountCycle::render_into` at
  `cli/src/recover.rs:319` -- so the preview shows the right
  `btrfs device scan --forget` argv and per-mapper `cryptsetup close`
  list.
- Real execution throws the planned names away. The dispatch arm at
  `cli/src/recover.rs:444` destructures `RemountCycle { .. }` and calls
  `relock_and_remount(...)` without forwarding `close_names`.
- `relock_and_remount` then re-derives both lists from live state:
  - the forget argv from `membership.disks.keys()` filtered by `fs.exists`
    at `cli/src/recover.rs:2838`,
  - the close loop from `membership.disks.keys()` filtered by `fs.exists`
    at `cli/src/recover.rs:2864`.

That re-derivation can disagree with what the dry-run promised in two
ways:

1. A mapper that opened between plan and execute (e.g. a sibling tool,
   an operator, or a racing automation) is closed and forgotten by the
   recover cycle even though the plan never mentioned it. This is the
   exact race the lock fix was protecting against.
2. The two membership choices differ. `mount_membership_for_recover`
   (`cli/src/recover.rs:3014`) returns `pre_membership`,
   `target_membership`, or `union` depending on journal phase, while the
   plan computed `cycle_close_names` from the `union`. Today execution
   can iterate a different set of disks than the dry-run preview.

Goal: make recover's remount-cycle execution consume the same planned
`close_names` that the dry-run preview rendered. `btrfs device scan
--forget` and the close loop must use one planned set; `fs.exists` only
filters mappers that disappeared between plan and execute.

## Approach

Mirror the lock fix exactly. Plumb the already-computed
`cycle_close_names` from the work-plan action into `relock_and_remount`,
and have that helper iterate the planned set instead of
`membership.disks.keys()`.

This keeps the surface area small: no new structs, no new helpers, no
plan-time changes. The plan side already does the right thing -- only
execution is broken.

## Code changes (all in `cli/src/recover.rs`)

### 1. Add `close_names: &[String]` to `relock_and_remount`

Function definition at `cli/src/recover.rs:2786`:

```rust
fn relock_and_remount<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    allow_degraded: bool,
    credential: &OpenCredential,
    close_names: &[String],
) -> Result<(), RecoverError>
```

Rationale: `&[String]` matches the lock fix's plumbing of bare disk
names, and avoids a clone at the callsite. The function still needs
`membership`, because the re-open phase calls
`mount::plan_open_pool(..., membership, ...)` which needs the
journal-phase-correct membership.

### 2. Source the forget argv from the planned set

Replace the body at `cli/src/recover.rs:2838-2843`:

```rust
let forget_devs: Vec<String> = close_names
    .iter()
    .map(|name| format!("/dev/mapper/{}", config::mapper_name(name).0))
    .filter(|p| fs.exists(p))
    .collect();
```

Comment update right above (`recover.rs:2829`): the close set is now
the planned set, not "membership mapper paths derived from
`membership.disks.keys()`".

### 3. Source the close loop from the planned set

Replace `for name in membership.disks.keys() { ... }` at
`cli/src/recover.rs:2864` with `for name in close_names { ... }`. Inside
the loop, keep the existing `if !fs.exists(&mapper_path) { continue; }`
guard (`recover.rs:2867`) -- it now serves only as a disappearance
guard.

Diagnostics inside the loop (`disk {name}: locking...` /
`disk {name}: locked` / error reporting) are unchanged. Recover's
current behavior already silently skips disks whose mapper does not
exist; preserve that. Do not add an "already closed" line for
membership disks not in the planned set -- `lock.rs` prints one because
its loop still iterates `membership.disks.keys()` for per-disk
reporting; recover's loop iterates the planned set directly, so there
is no membership disk to print about. Adding a second loop just to
emit messages would be noise.

### 4. Forward the planned set at the only callsite

In `RecoverWorkAction::RemountCycle::execute` at
`cli/src/recover.rs:444`, change the destructure and call:

```rust
RecoverWorkAction::RemountCycle { close_names, .. } => {
    if state.just_mounted {
        let recovery_mount_membership =
            mount_membership_for_recover(&plan.journal, &plan.union).clone();
        let cred = state.credential.as_ref().expect(
            "just_mounted implies open_plan was Some and credential was resolved",
        );
        relock_and_remount(
            runner,
            fs,
            params.config,
            &recovery_mount_membership,
            params.allow_degraded,
            cred,
            close_names,
        )?;
    }
    Ok(false)
}
```

### 5. Update the one direct-call test

Only one test calls `relock_and_remount` directly:
`recover_remount_cycle_mount_failure_closes_reopened_mappers`, with the
call at `cli/src/recover.rs:8872`. Add `close_names` as the final
argument -- supply `membership.disks.keys().cloned().collect::<Vec<_>>()`
(or an explicit `vec!["disk1".into(), "disk2".into()]`) so the existing
forget-argv and close-argv assertions still hold; the test fixture
models the union case where every membership mapper is open and
planned to be closed.

All other recover tests (including
`recover_remount_cycle_umount_failure_aborts_before_pool_json` at
`cli/src/recover.rs:8579`) drive `cmd_recover`, not `relock_and_remount`
directly. They are unaffected by the signature change because the
work-plan action carries `close_names` and the dispatch arm in step 4
forwards it.

## Behavioral change

- Mappers that appear between plan and execute are no longer closed or
  forgotten by the recover cycle. They are not in the planned
  `close_names`, so the cycle will not touch them. This matches the
  dry-run preview's promise and matches the lock fix.
- Mappers in `close_names` whose `/dev/mapper/<name>` has disappeared
  between plan and execute are still skipped via the `fs.exists` guard,
  and are also filtered out of the forget argv. No change from today.
- Membership disks not in `close_names` (e.g. a journal-phase mismatch
  where `mount_membership_for_recover` returns a wider membership than
  the plan-time union) are silently skipped. This is the same posture
  the dry-run already takes.
- No diagnostic regressions: the existing per-disk
  `disk {name}: locking...` / `disk {name}: locked` and error
  propagation remain intact for every name in `close_names`.

## Tests

Add two focused unit tests in the existing `tests` mod in
`cli/src/recover.rs` (around the existing `relock_and_remount`
direct-call tests near `cli/src/recover.rs:8757`). Both reuse the
existing `MockFs` (`recover.rs:3063`) and `MockRunner` infrastructure
and assert against `runner.requests()` (exposed at
`cli/src/cmd.rs:1008`).

### Test A: execution does not close or forget a mapper absent from `close_names`

```
// Intent: relock_and_remount honors the planned close_names and does
//   not close or forget a membership mapper that appeared in
//   /dev/mapper between plan and execute.
// Why it exists: closing a mapper not in the plan reopens the
//   cryptsetup-close-btrfs-held race because the forget argv is
//   plan-derived; it also breaks the dry-run -> execute contract.
// Scenario: plan_recover would have computed close_names =
//   [disk1, disk2]. Between plan and execute, /dev/mapper/braid-extra
//   appears (membership union also lists 'extra' for the test, to
//   prove the new code does NOT fall back to membership.disks.keys()).
//   Execute must not issue CryptsetupClose for braid-extra and the
//   BtrfsDeviceScanForget argv must not contain /dev/mapper/braid-extra.
```

Assertions:

- `runner.requests()` contains no
  `CmdRequest::CryptsetupClose { mapper: "braid-extra" }`.
- The `CmdRequest::BtrfsDeviceScanForget` request's `devices` does not
  contain `/dev/mapper/braid-extra`; it contains exactly
  `/dev/mapper/braid-disk1` and `/dev/mapper/braid-disk2`.
- The `CmdRequest::CryptsetupClose` requests cover `braid-disk1` and
  `braid-disk2` and only those.

The `MockFs` seeds `/dev/mapper/braid-disk1`, `/dev/mapper/braid-disk2`,
and `/dev/mapper/braid-extra`. The membership passed in lists all
three; `close_names` lists only `disk1` and `disk2`. The runner is
prebuilt so the umount, forget, both closes, both reopens, scan, and
mount succeed; `cryptsetup` UUID/dump/passphrase mocks come from the
existing helpers used by `recover_remount_cycle_mount_failure_closes_reopened_mappers`.

### Test B: planned close target whose mapper disappeared between plan and execute

```
// Intent: relock_and_remount uses fs.exists only as a disappearance
//   guard -- if a planned close target's mapper is gone at execute
//   time, neither cryptsetup close nor the forget argv references it.
// Why it exists: a previously-open mapper can vanish between plan and
//   execute (operator, sibling tool, racing recovery). The cycle must
//   degrade gracefully without spurious errors.
// Scenario: close_names = [disk1, disk2]; only /dev/mapper/braid-disk1
//   exists at execute time. The forget argv contains exactly
//   /dev/mapper/braid-disk1 and CryptsetupClose runs only for disk1.
```

Assertions:

- `runner.requests()` contains exactly one
  `CmdRequest::CryptsetupClose { mapper: "braid-disk1" }` and no
  `CryptsetupClose` for `braid-disk2`.
- The single `CmdRequest::BtrfsDeviceScanForget`'s `devices` is exactly
  `["/dev/mapper/braid-disk1"]`.
- `relock_and_remount` returns `Ok(())` (cycle succeeds end to end with
  the surviving mapper).

Use the same helpers as test A; just seed `MockFs` with only the
`disk1` mapper at start and skip the `cryptsetup close` mock for
`disk2`.

### Tests expected to remain green (unchanged behavior)

- `plan_recover_dry_run_includes_remount_cycle_when_not_mounted`
  (`cli/src/recover.rs:11708`) -- already validates the planned forget
  argv and the planned per-mapper close list in the rendered preview.
  No code change is needed because the plan side was already correct.
- `plan_recover_dry_run_omits_remount_cycle_when_already_mounted`
  (`cli/src/recover.rs:11760`).
- `recover_remount_cycle_mount_failure_closes_reopened_mappers`
  (`cli/src/recover.rs:8757`, direct `relock_and_remount` call at
  `:8872`) -- updated only to pass `close_names`; existing
  `expect_err`/forget-argv/close-argv assertions still hold since the
  test fixture already models "every membership mapper is open and
  planned".
- `recover_remount_cycle_umount_failure_aborts_before_pool_json`
  (`cli/src/recover.rs:8579`) -- drives `cmd_recover`, not
  `relock_and_remount` directly; no change needed.
- All other existing recover tests in `cli/src/recover.rs` -- they go
  through `RecoverWorkPlan::execute`, which receives `close_names` from
  the action and forwards it untouched.
- Lock-side tests in `cli/src/lock.rs` -- untouched.

## Reference patterns being mirrored

- Planning side already exists; do not duplicate. Reuse
  `cycle_close_names` computed at `cli/src/recover.rs:1106-1117`.
- Lock fix planning shape:
  `cli/src/lock.rs:427-432` (build `open_mappers` once).
- Lock fix forget argv:
  `cli/src/lock.rs:238-244` (planned set + `fs.exists` guard).
- Lock fix close loop with disappearance guard:
  `cli/src/lock.rs:282-300`.
- Lock fix unit-test shape (the closest analog to test A):
  `cli/src/lock.rs:766-809`,
  `execute_does_not_close_membership_mapper_absent_from_plan`.

`mount::close_opened_mappers` (`cli/src/mount.rs:671`) is *not*
applicable here -- it is used post-failure to roll back mappers opened
inside one call, and operates on `MapperName`, not on planned bare
names. Refactoring it to subsume both lock and recover is out of
scope; lock did not do it either, so doing it now would expand the
diff without unifying anything that lives in only one production
caller per file.

## Critical files

- `cli/src/recover.rs` -- the only production file changed.
- `cli/src/lock.rs` -- read for pattern parity; no changes.
- `cli/src/mount.rs` -- read to confirm `plan_open_pool`/
  `execute_unlock_and_mount` semantics inside `relock_and_remount`; no
  changes.

## Verification

- `just test-rust` -- runs the new and updated unit tests in the
  `cli/src/recover.rs` `tests` mod, plus all other Rust unit tests.
  This is the primary signal.
- `just test-vm recover` (and any narrower `recover-*` checks visible
  in `flake.nix`'s `checks`) -- exercises the real recover path against
  a NixOS VM. Run before commit to catch any integration-level breakage
  in the journal/membership wiring.
- `cargo build -p braid-cli` -- catches the signature-change ripple at
  the two test callsites if they were missed.

No fixture-refresh event is triggered; no nixpkgs-pinned tool is
touched.
