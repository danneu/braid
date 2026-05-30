# Fix `ups-lb-during-remove-missing`: permit balance during shutdown lock

## Context

`braid-online.service` (`Type=oneshot`, `RemainAfterExit`, Description "Braid
storage pool online", `TimeoutStopSec=300s`) runs `ExecStop = braid lock
--systemd-stop --deadline-secs <N>` (`modules/braid/storage.nix:130-147`). When a
UPS low-battery shutdown lands during the post-`remove-missing` soft RAID1
balance:

1. `remove-missing` (under a `systemd-run` scope) is SIGKILLed during the
   shutdown transition, freeing the pool flock. The *kernel* balance keeps
   running, so `/sys/fs/btrfs/<fsid>/exclusive_operation` reads `balance`.
2. `braid lock --systemd-stop` acquires the now-free pool lock, then `plan_lock`
   -> `preflight::require_lock_preflight` (policy `RejectAnyBusy`) HARD-FAILS:
   `cannot lock: balance is in progress. Wait for it to finish first.` ExecStop
   exits 1.
3. systemd falls back to its *uncoordinated* generic shutdown, tearing down the
   dm-crypt (and the test's dm-delay) devices out from under the still-mounted,
   still-balancing btrfs. That mid-balance device yank corrupts metadata.
4. Next boot, `braid recover` replays the owed balance, btrfs detects the
   corruption, forces the FS read-only, and recover fails with EIO.

**Root cause and the real fix.** A clean `umount` already does the safe thing: the
kernel's `close_ctree()` calls `btrfs_pause_balance()`, which leaves the balance
**paused and persisted, not removed** (`reference/linux/fs/btrfs/volumes.c:4587-4598`;
`btrfs_pause_balance` at `volumes.c:4761`; close path in `disk-io.c`). `recover`
then resumes it. The only bug is that ExecStop *fails* before braid's ordered
teardown (umount -> `btrfs device scan --forget` -> `cryptsetup close`) ever runs,
so systemd's uncoordinated cascade yanks devices instead. The fix is to make
ExecStop **succeed** by permitting a running/paused balance in `--systemd-stop`
mode and letting the existing umount quiesce it.

**Bound, stated honestly.** The balance quiesce happens inside `umount(2)`'s
`close_ctree` -> `btrfs_pause_balance`, which *blocks* (`wait_event(...
!BALANCE_RUNNING)`, `volumes.c:4775`) until the balance reaches a block-group
checkpoint. That wait is bounded by systemd's `TimeoutStopSec` (300s), **not** by
`--deadline-secs`. `--deadline-secs` continues to bound only the stop-coordinator
wait and the pool-lock acquisition in `main.rs` (its original purpose). There is
no userspace way to bound the kernel pause wait, so this plan does not attempt a
deadline-bounded pause (see "Rejected" below).

## Approach

Give `--systemd-stop` a distinct exclusive-op **preflight policy** that permits a
running or paused balance and rejects every other exclusive op, then runs the
unchanged ordered teardown. No explicit `btrfs balance pause`, no execute-time
changes -- the umount quiesces+persists the balance and `recover` resumes it.
Ordinary `braid lock` stays strict. Non-balance exclusive ops stay hard errors in
both modes (no proven safe quiesce -- explicit constraint).

### 1. New preflight policy (`cli/src/preflight.rs`)

- Add a third `ExclusiveOpPolicy` variant `AllowBalanceElseReject` with a doc
  comment: `--systemd-stop` teardown -- a running or paused balance is safe to
  proceed on because the umount pauses+persists it and `recover` resumes it; every
  other exclusive op is still unsafe to unmount under.
- In `check_exclusive_op_with_policy` (L182-202): `None` already early-returns
  `Ok(None)`. Add the arm `AllowBalanceElseReject => match op { Balance |
  BalancePaused => Ok(None), _ => Err(format!("cannot lock: {op} is in progress.
  Wait for it to finish first.")) }` (reuse the exact `RejectAnyBusy` wording).
- Add `pub fn require_systemd_stop_lock_preflight<F: Filesystem + ?Sized>(fs,
  fsid) -> Result<(), String>` = `check_exclusive_op_with_policy(fs, fsid,
  AllowBalanceElseReject).map(|_| ())`, mirroring `require_lock_preflight` (L639).
- Leave `RejectAnyBusy` / `require_lock_preflight` untouched.

### 2. Lock mode threaded into planning (`cli/src/lock.rs`)

- Add `enum LockMode { User, SystemdStop }` (payload-free; `///` doc). No
  deadline -- the lock body does not use one.
- `plan_lock` gains `mode: LockMode`. At the `Snapshot::Probed` arm (preflight at
  L787-789, gated on `pool.fsid.is_some()`) and the `Snapshot::ProbeFailed` arm
  (L806): `match mode { User => require_lock_preflight(fs, fsid)?, SystemdStop =>
  require_systemd_stop_lock_preflight(fs, fsid)? }`. `Snapshot::Unmounted`
  (L818-827) does no preflight in either mode (nothing mounted).
- `cmd_lock_impl_with_notes` (L1103) gains `mode`, forwards it to `plan_lock`.
- `#[cfg(test)] cmd_lock_impl` (L1085) keeps its arity and passes `LockMode::User`
  -> its ~30 callers compile unchanged. Only the ~4 direct `plan_lock(...)` test
  calls (e.g. L1778, L1829, and the NotBtrfs-arm one near file end) gain `,
  LockMode::User`.
- Keep `pub fn cmd_lock(...)` unchanged -> passes `LockMode::User` (preserves
  dry-run + `cmd_lock_orchestrate` callers).
- Add `pub fn cmd_lock_systemd_stop(runner, fs, config, membership) -> Result<(),
  LockError>` (`///`; **no deadline param**) -> `cmd_lock_impl_with_notes(..,
  &RealSleeper, .., dry_run=false, Vec::new(), LockMode::SystemdStop)`.
- `LockPlan` and `LockPlan::execute` are **unchanged** -- once preflight permits
  the balance, the existing `umount -> btrfs device scan --forget -> cryptsetup
  close` path does all the work, and umount blocks in `close_ctree` until the
  balance pauses.

### 3. Wire the ExecStop entry (`cli/src/main.rs`)

In `run_systemd_stop_lock` (L1196): replace the `cmd_lock(&runner, &fs, &config,
&membership, false, Vec::new())` call (L1251) with
`braid_cli::lock::cmd_lock_systemd_stop(&runner, &fs, &config, &membership)`. The
coordinator wait + `acquire_with_systemd_stop_deadline(remaining)` (L1204-1240)
and their `--deadline-secs` budget are untouched. `run_dry_run_lock` and
`run_plain_lock` keep `cmd_lock` / `cmd_lock_orchestrate` => `LockMode::User`.

### 4. Strengthen the VM test assertion (`tests/module/ups-lb-during-remove-missing.py` L334-341)

The "Previous boot's braid-online.service stopped cleanly" subtest only greps for
`Stopped Braid storage pool online`, which systemd logs even when ExecStop FAILS
-- masking the stop failure so the test only blows up later at recover. Keep the
positive line and add negatives on the same `journalctl -b -1 -u
braid-online.service` text: assert `"Failed with result"` not present and
`"/FAILURE"` not present (covers `Control process exited, code=exited,
status=N/FAILURE` and `Failed with result 'exit-code'`/`'timeout'`). Behavioral
and structure-insensitive (systemd's own result markers). Before the fix it fails
at the stop subtest (root cause); after, all subtests pass. Recover is unchanged
-- keep the existing `"replaying post-remove-missing RAID1 soft balance"` assert.

### 5. Update ADR 018 (`docs/design/decisions/018-systemd-lifecycle.md`)

ADR 018 is Status `Active`; amend **in place** (additive refinement of the
shutdown contract, not a reversal -- it stays `Active`). ADR 018 today documents
the stop path's lock/coordinator wait but is **silent on exclusive-op handling**,
so the new per-mode behavior is an addition, not a rewrite of a stale claim.
Verified target locations (the file has **no** "Consequences" or "Alternatives
considered" section -- do not add or cite one):

- **`braid-online.service` ExecStop bullet (L82)** -- today reads "unmounts pool
  and closes all LUKS ... with a bounded wait below `TimeoutStopSec`." Add that
  `braid lock --systemd-stop` permits a running or paused `balance` (the unmount's
  `close_ctree` -> `btrfs_pause_balance` pauses+persists it) while every other
  exclusive op is refused.
- **"On system shutdown" steps (L144-158), at step 4 "CLI unmounts and closes
  LUKS" (L157)** -- note that a running/paused balance is permitted and quiesced +
  persisted by the unmount before LUKS close, so `braid recover` resumes it on the
  next boot. Plain `braid lock` still refuses all active exclusive ops.
- **"ExecStop bounded-wait pattern" (L172-176)** -- add one sentence distinguishing
  the two bounds: `--deadline-secs` (default 270s, asserted `< TimeoutStopSec`)
  bounds only pool-lock + stop-coordinator acquisition; the kernel balance-pause
  inside `umount` has no userspace timeout and is bounded only by the unit's
  `TimeoutStopSec` (300s).

AGENTS.md requires this update because the change alters a documented behavioral
contract (the systemd shutdown stop path).

## Files to modify

- `cli/src/preflight.rs` -- `AllowBalanceElseReject` policy +
  `require_systemd_stop_lock_preflight`.
- `cli/src/lock.rs` -- `LockMode`, `mode` on `cmd_lock_impl_with_notes`/`plan_lock`
  (mode-selected preflight), `cmd_lock_systemd_stop`, tests.
- `cli/src/main.rs` -- `run_systemd_stop_lock` calls `cmd_lock_systemd_stop`.
- `tests/module/ups-lb-during-remove-missing.py` -- strengthen stop-clean assert.
- `docs/design/decisions/018-systemd-lifecycle.md` -- amend the shutdown contract
  in place (per-mode balance handling; `--deadline-secs` vs `TimeoutStopSec`
  bound); stays `Active`.

(`cli/src/cmd.rs` and `cli/src/test_fixtures/shared.rs` are NOT touched -- no new
command and the existing fixed-body `with_excl_op` fixture suffices.)

## Reuse (don't reinvent)

- `preflight::check_exclusive_op_with_policy` + `ExclusiveOpPolicy` + `ExclusiveOp`
  -- add one policy arm; do not add a parallel classifier.
- `preflight::require_lock_preflight` as the shape to mirror for
  `require_systemd_stop_lock_preflight`.
- Lock test fixtures: `lock_fs(&[...]).with_excl_op("...")` (fixed body),
  `lock_with_fsid_probe_mocks`, `mounted_runner_with_btrfs_show`, `lock_test_config`,
  `lock_test_membership`, `LockNoopSleeper` -- exactly what the existing
  `lock_refuses_when_*` tests use.
- preflight test fixture `MockFs::with_sysfs(fsid, content)` for the policy tests.

## Unit tests (`just test-rust`)

Timing-free -- the pivot is a preflight-policy change, so tests assert
classification, not waits.

**`cli/src/preflight.rs`** (mirror existing `MockFs::with_sysfs` policy tests):

- `require_systemd_stop_lock_preflight` returns Ok for `"none"`, `"balance"`,
  `"balance paused"`.
- `require_systemd_stop_lock_preflight` returns Err naming the op for each of
  `"device add"`, `"device remove"`, `"device replace"`, `"resize"`,
  `"swap activate"` (loop), and the message contains `"cannot lock"` +
  `"in progress"`.

**`cli/src/lock.rs`** (mode wiring; use `lock_with_fsid_probe_mocks` +
`lock_fs(...).with_excl_op(...)`):

- `systemd_stop_proceeds_on_running_balance`: `.with_excl_op("balance")`; drive via
  `cmd_lock_systemd_stop` (or `plan_lock(.., LockMode::SystemdStop)` +
  `plan.execute(&runner, &fs, &LockNoopSleeper)` with the runner seeded like the
  existing success tests). Assert Ok and that umount + forget + close ran.
- `systemd_stop_proceeds_on_paused_balance`: `.with_excl_op("balance paused")`;
  assert Ok and teardown ran.
- `systemd_stop_rejects_non_balance_op`: `.with_excl_op("device remove")`; assert
  `Err` naming the op and **no** umount request (refusal before teardown).
- `user_lock_still_refuses_active_balance` / `..._paused_balance`: already covered
  by the existing `lock_refuses_when_exclusive_op_active` (L3517-3541) and
  `lock_refuses_when_balance_paused` (L3543-3566), which run via `cmd_lock_impl`
  (User). **Preserve them unchanged** -- they are the regression pins that User
  mode is untouched. The systemd-stop accept-paused test is the deliberate
  counterpoint, not a replacement.

## Verification

1. `just test-rust` -- new preflight + lock mode tests (fast; run first).
2. `just test-vm ups-lb-during-remove-missing` -- end-to-end repro. Expect:
   ExecStop succeeds (umount pauses+persists the balance, then forget + close); the
   strengthened stop-clean subtest passes; `pending-op.json` survives; `braid
   recover` completes (exit 0) and logs `replaying post-remove-missing RAID1 soft
   balance`; pool remounts RAID1.
   - To confirm the strengthened assertion has teeth, optionally stash the Rust fix
     and verify the test then fails at the *stop-clean* subtest (not only later at
     recover).
3. Spot-check no other `cmd_lock`/`plan_lock` callers broke from the `mode` param
   (dry-run preview, plain-lock orchestrate, the ~4 direct `plan_lock` test calls).

## Risks / edge cases

- **umount blocks until the balance checkpoints, bounded by `TimeoutStopSec`
  (300s), not `--deadline-secs`.** If a single block-group relocation cannot reach
  a checkpoint within 300s, systemd SIGKILLs braid lock mid-umount and the
  corruption window reopens. This is inherent to btrfs (the kernel pause wait has
  no userspace timeout) and is identical to what any pause-based approach would
  face; block-group checkpoints arrive in seconds-to-low-minutes, so 300s is ample.
  Documented, not mitigated further.
- `Snapshot::ProbeFailed` still has an fsid, so the systemd-stop policy applies
  there too; `Snapshot::Unmounted` has no mounted fs => no preflight, nothing to
  permit.
- Non-balance exclusive ops remain hard errors in `--systemd-stop` (explicit
  constraint) -- only `balance`/`balance paused` are proven safe to unmount under.
- Doc comments (AGENTS.md): add `///` to every new `pub`/`pub(crate)` item
  (`AllowBalanceElseReject` if the variant warrants it, `require_systemd_stop_lock_preflight`,
  `LockMode`, `cmd_lock_systemd_stop`); `#[cfg(test)]` helpers are exempt.

## Rejected: explicit `btrfs balance pause` with a deadline-bounded poll

An earlier draft added a `CmdRequest::BtrfsBalancePause` issued in `execute`,
followed by a poll loop bounded by `--deadline-secs`. Rejected: `btrfs balance
pause` is a blocking ioctl -- btrfs-progs calls `ioctl(... BTRFS_BALANCE_CTL_PAUSE)`
(`reference/btrfs-progs/cmds/balance.c:707`) and the kernel waits in
`btrfs_pause_balance()` until `BTRFS_FS_BALANCE_RUNNING` clears
(`volumes.c:4775`). Because `RealRunner::exec` blocks in `Command::output()`
(`cmd.rs:1294`), the poll + `elapsed() >= deadline` check only run *after* the
unbounded wait, so the deadline is fictional in production and the timeout branch
would only ever "pass" in a mock that returns instantly without pausing -- a
misleading test. The explicit pause is also redundant: the umount already performs
the identical pause+persist. The preflight-policy approach above is simpler and
matches the verified kernel behavior.

## Implementation notes

- Current `cli/src/lock.rs` had more direct `plan_lock` unit-test call sites than the plan estimated; all were updated mechanically to pass `LockMode::User` so existing user-lock coverage remains explicit.
- VM verification showed `btrfs-progs` can survive the Rust parent briefly while blocked in `BTRFS_IOC_BALANCE_V2` and keep the mount fd busy after the pool lock is released; `LockPlan` now carries a systemd-stop-only longer umount retry budget while plain user lock keeps the existing retry count.
- VM verification also showed relying on umount alone can let a fatal signal cancel the running balance before teardown completes; systemd-stop now issues `btrfs balance pause` before unmount for running balances, with the blocking wait still bounded by `TimeoutStopSec`.
