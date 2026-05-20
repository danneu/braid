# Plan: Online finalizer + bootstrap-journal fallback for lock

## Context

The pool-lock dispatch arms in `cli/src/main.rs` -- `Commands::Add`,
`Commands::Unlock`, and `Commands::Recover` -- call `mark_online`
only on the `Ok` branch of their respective `cmd_X` calls. This is
wrong for any `cmd_X` that mounts the pool successfully and then
returns `Err` from a later step.

The dangerous case is **bootstrap `braid add`**. In the bootstrap
branch at `cli/src/add.rs:1345-1385`:

1. `pool_bootstrap_mount[_raid1]` mounts the pool (line 1348/1352).
2. `alert::remove_acked_stats` runs (line 1357) -- can return `Err`.
3. `probe_pool` runs (line 1372) -- `Err` is logged, not propagated.
4. `membership::enrich_from_pool_state` runs (line 1374) -- can return `Err`.
5. `membership::save_membership` runs (line 1380) -- can return `Err`.
6. `journal::clear_journal` runs (line 1384) -- can return `Err`.

When 2/4/5/6 fail, `cmd_add` propagates the error and the dispatch
arm at `cli/src/main.rs:438-441` hits `std::process::exit(1)` before
`mark_online` at `cli/src/main.rs:442-444`. The pool is now mounted
but `braid-online.service` is `inactive`. This is the lifecycle hole
that ADR 026 ([`docs/decisions/026-pool-lock-rust-owned.md`](../../docs/decisions/026-pool-lock-rust-owned.md))
is meant to close. On the next `systemctl poweroff`, no `ExecStop=braid
lock --systemd-stop` fires, btrfs is SIGKILL'd, and LUKS mappers are
left to the kernel umount-rw path.

`cmd_recover` has the same shape: `execute_recover_initial_open` at
`cli/src/recover.rs:944-1056` mounts the pool, then
`execute_generic_live_pool_recovery` at `cli/src/recover.rs:1138/1144/1150/1170`
propagates `Err` from `save_membership`, `remove_acked_stats`,
`replay_post_mutation` (which includes long-running
`pool_balance_raid1_soft`), and `clear_journal`. The shutdown-safety
problem is identical.

`cmd_unlock` does not currently propagate post-mount errors (every
post-mount step at `cli/src/unlock.rs:148-176` turns failure into a
stderr warning and returns `Ok`), but the dispatch arm has the same
structural gap. Fixing it uniformly prevents a future regression.

### The second half of the hole: pool.json may be absent

Activating `braid-online.service` after a failed bootstrap-add is
necessary but not sufficient. The bootstrap-failure cases 2/4/5
above all leave `pool.json` **never written** -- `save_membership`
was either never reached or was the step that failed. When
`braid-online.service`'s `ExecStop=braid lock --systemd-stop` then
fires on shutdown, `run_systemd_stop_lock` at `cli/src/main.rs:1060-1061`
calls `load_membership_or_exit`, which calls `load_membership` at
`cli/src/membership.rs:424-491`. That function returns
`MembershipError::Io { source: NotFound, .. }` (line 434), and
`load_membership_or_exit` at `cli/src/main.rs:971-979` exits 1.
ExecStop fails, `braid-online.service` goes to `failed` state with
`TimeoutStopSec`, and the pool stays mounted / mappers stay open
through systemd's SIGKILL chain.

The bootstrap-add journal is the authoritative recovery source for
this state. `journal::write_journal` at `cli/src/add.rs:1184` writes
`pending-op.json` **before** LUKS format / open / mount. The
`Journal.target_membership` field (`cli/src/journal.rs:24`) is the
intended post-operation membership; for a bootstrap-add it is
exactly the set of disks that just got opened and mounted. That is
the right input for `cmd_lock`'s close-set classification.

### Why plain `braid lock` must stay success-gated

`cmd_lock` at `cli/src/lock.rs:491-650` accumulates `umount_error`
and `first_mapper_error` and can return `Err` after a partial
unmount (umount succeeded, LUKS close failed). The existing
`run_plain_lock` at `cli/src/main.rs:1005-1013` keeps `mark_offline`
and `coordinator_guard.mark_done()` strictly success-gated. That is
**correct, not a bug**:

- `mark_done()` writes `done\n` to `/run/braid-stop-coordinator.lock`.
  ADR 026's [Stop Coordinator + Done Protocol](../../docs/decisions/026-pool-lock-rust-owned.md#stop-coordinator--done-protocol)
  states the `done\n` signal tells the recursive ExecStop reentry it
  may exit 0 because cleanup is complete. Writing `done\n` after a
  partial `cmd_lock` failure would tell ExecStop "all good" when
  mappers may still be open.
- `mark_offline` runs `systemctl stop braid-online.service`. If the
  unit goes inactive while mappers are still open, the recovery
  guarantee from `tests/module/execstop-cleans-stale-online.py:34-38`
  -- that `systemctl stop braid-online.service` runs full cleanup on
  the next shutdown -- is lost. The safe-failure state is
  "braid-online.service stays active, operator (or next shutdown)
  retries cleanup". This is exactly what
  `execstop-cleans-stale-online.py` already pins.

So the offline side gets no finally-block. The fix is online-only.

### Intended outcome

After this change:

1. Every `unlock` / `add` / `recover` attempt -- success or failure
   -- runs `mark_online` as a finally-block under the held pool
   lock. The `is_mountpoint` gate inside `mark_online` short-circuits
   when the mount never happened.
2. `braid-online.service`'s `ExecStop` path can complete cleanup
   even when bootstrap-add failed before `save_membership` ran, by
   falling back to the bootstrap-add journal's `target_membership`.
3. The lifecycle hole closes end to end: pool mounted, mappers open,
   no `pool.json` -> shutdown still runs full cleanup.

## Approach

Three coordinated changes:

1. **Online finalizer helper** in `cli/src/online_state.rs`. Use it
   in the three online-side dispatch arms.
2. **Bootstrap-journal membership fallback** for `cmd_lock`'s
   membership input, used by both `run_plain_lock` and
   `run_systemd_stop_lock`. Plain-lock lifecycle calls stay
   success-gated -- nothing about `mark_offline` / `mark_done`
   changes.
3. **One VM test** that pins the end-to-end scenario: bootstrap-add
   fails post-mount, dispatch activates `braid-online.service`,
   `systemctl stop braid-online.service` runs full cleanup via the
   journal-fallback path.

### Step 1: Online finalizer helper in `cli/src/online_state.rs`

Add one public function next to `mark_online` (around line 290):

```rust
/// Run a pool-touching operation and always reconcile braid-online.service
/// afterward, regardless of whether the operation succeeded. The
/// `is_mountpoint` gate inside `mark_online` makes the Err-path call a
/// no-op when the operation failed before mounting; the bootstrap-add /
/// recover case where the mount succeeded but a later step (remove_acked_stats,
/// save_membership, clear_journal) returned Err is exactly where this
/// finally-block matters. Closes the lifecycle hole described in ADR 026.
pub fn run_with_online_marker<E>(
    snap: Option<&OnlineSnapshot>,
    cfg: Option<&Config>,
    ops: &dyn OnlineStateOps,
    op: impl FnOnce() -> Result<(), E>,
) -> Result<(), E> {
    let result = op();
    if let Some(cfg) = cfg {
        let _ = mark_online(snap, cfg, ops);
    }
    result
}
```

No offline counterpart. See Context above for why.

### Step 2: Use the helper in `cli/src/main.rs`

Three call sites change. Each follows the same shape: build the
operation closure, hand it to the helper, then match the returned
`Result` for exit code.

- **`Commands::Add` (lines 399-445):** wrap the `cmd_add(...)` call in
  `run_with_online_marker(online_snapshot.as_ref(), online_config.as_ref(), &online_ops, || cmd_add(...))`.
  Match the result; exit 1 on `Err`. Delete the trailing `if let Some(cfg)
  = online_config.as_ref() { let _ = mark_online(...) }` block at 442-444
  (the helper now owns it).

- **`Commands::Unlock` (lines 575-616):** wrap the `cmd_unlock(...)`
  call in `run_with_online_marker(...)`. Keep the existing
  `Err(MountError::DegradedRefused(_))` -> exit 2 special case in the
  outer match. Pass `online_snapshot.as_ref()` and a
  `(!args.dry_run).then(|| &config)` gating.

- **`Commands::Recover` (lines 858-906):** same shape as Unlock.
  Same `DegradedRefused` -> exit 2 case applies.

- **`run_plain_lock` (lines 981-1014):** **untouched** for lifecycle
  calls. `mark_offline` and `mark_done()` stay strictly success-gated
  per the rationale in Context.

- **`run_systemd_stop_lock` (lines 1016-1068):** **untouched** for
  lifecycle calls -- still must not call `mark_offline` (would
  deadlock against the in-flight stop per ADR 026). Only its
  membership-load step changes (Step 3).

### Step 3: Bootstrap-journal membership fallback for lock

Factor a `Result`-returning inner function and a thin `_or_exit`
wrapper next to `load_membership_or_exit` in `cli/src/main.rs`.
The inner function carries a typed error so the wrapper can render
each failure with its source-specific remediation text (especially
the pinned `JournalError::Parse` message from
`cli/src/journal.rs:205-208`, which `docs/luks-unlock.md` quotes).
A bare `_ =>` collapse would hide that text.

```rust
#[derive(Debug, thiserror::Error)]
pub(crate) enum LoadForLockError {
    #[error(transparent)]
    Membership(#[from] braid_cli::membership::MembershipError),
    /// Corrupt or unreadable pending-op.json. Distinct from the
    /// "no journal present" case so the operator sees the pinned
    /// `JournalError::Parse` remediation text.
    #[error(transparent)]
    Journal(#[from] braid_cli::journal::JournalError),
    /// pool.json is absent, pending-op.json is absent (Ok(None)),
    /// or the journal exists but is not a bootstrap-add (non-empty
    /// pre_membership or non-Add OpKind). The lock path has no
    /// authoritative membership source.
    #[error(
        "no pool membership available -- pool.json missing and no \
         bootstrap-add journal present"
    )]
    NoMembershipAvailable,
}

/// Load pool membership for the lock path. Falls back to the
/// bootstrap-add journal's target_membership when pool.json is
/// absent (NotFound) and a bootstrap-add journal is on disk.
/// This handles the shutdown-after-failed-bootstrap-add lifecycle
/// hole: pool is mounted, mappers are open, but save_membership
/// never ran. Without this fallback, ExecStop=braid lock would exit
/// 1 on load and leave the pool to systemd's SIGKILL chain.
/// Bootstrap detection: OpKind::Add with empty pre_membership.
/// Corrupt journals surface as Journal(_) so operators keep the
/// pinned remediation text instead of a generic "no journal" line.
fn load_membership_for_lock(
    paths: &StatePaths,
) -> Result<PoolMembership, LoadForLockError> {
    match braid_cli::membership::load_membership(paths) {
        Ok(m) => Ok(m),
        Err(braid_cli::membership::MembershipError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            // pool.json absent -- try the bootstrap-add journal.
            match braid_cli::journal::load_journal(paths)? {
                Some(j)
                    if matches!(&j.op, braid_cli::journal::OpKind::Add { .. })
                        && j.pre_membership.is_empty() =>
                {
                    eprintln!(
                        "braid: pool.json absent; recovering membership from \
                         interrupted bootstrap-add journal for shutdown cleanup"
                    );
                    Ok(j.target_membership)
                }
                // Ok(None) or non-bootstrap journal.
                _ => Err(LoadForLockError::NoMembershipAvailable),
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn load_membership_for_lock_or_exit(
    paths: &StatePaths,
    exit_code: i32,
) -> PoolMembership {
    match load_membership_for_lock(paths) {
        Ok(m) => m,
        Err(e) => {
            // Display impls already carry source-specific remediation:
            // MembershipError::Corrupt -> existing remediation text;
            // JournalError::Parse     -> pinned text quoted in docs/luks-unlock.md;
            // NoMembershipAvailable   -> the generic line.
            print_cli_error(&e.to_string());
            std::process::exit(exit_code);
        }
    }
}
```

Use this helper at:

- `run_plain_lock`, `cli/src/main.rs:1000` (replace
  `load_membership_or_exit(paths, 1)`).
- `run_systemd_stop_lock`, `cli/src/main.rs:1061` (replace
  `load_membership_or_exit(paths, 1)`).

Decision rule for "is this a bootstrap-add journal":
`OpKind::Add { .. }` with `pre_membership.is_empty()`. Empty
pre_membership is the structural signature of bootstrap (no
previous pool). Live-pool adds carry non-empty pre_membership and
should not trigger the fallback -- if pool.json is somehow missing
during a live-pool-add interrupt, that is a different recovery
shape (run `braid recover`).

`cmd_lock` handles a membership listing mappers that turn out to be
absent gracefully -- `build_close_sets_full` at `cli/src/lock.rs:736-746`
probes each one and routes through `skipped_mappers` / `notes` when
they cannot be classified. So even if `target_membership` lists a
disk whose LUKS open failed mid-bootstrap (before mkfs), the close
phase will skip it cleanly.

Do not extend the fallback to `OpKind::Remove` / `RemoveMissing` /
`Replace` -- those operations require `pool.json` to exist as a
precondition; missing `pool.json` there is a different (already
journal-recoverable) shape.

### Step 4: Unit tests in `cli/src/online_state.rs`

Reuse the existing `RecordingOnlineStateOps` (lines 314-388) and the
existing test-config helper `cfg()` (line 395). Add tests near the
bottom of the `tests` mod (after the existing `mark_online_*` /
`mark_offline_*` tests around line 559):

1. **`run_with_online_marker_calls_mark_online_on_err`** -- closure
   returns `Err`, lifecycle enabled, snapshot is `Inactive`, ops
   reports `is_mountpoint=true`. Assert: helper returns the `Err`,
   `ops.calls()` contains `"start braid-online.service"`. **Regression
   test for the cited bug.**
2. **`run_with_online_marker_calls_mark_online_on_ok`** -- closure
   returns `Ok`, same setup. Assert: helper returns `Ok`, `ops.calls()`
   contains `"start braid-online.service"`.
3. **`run_with_online_marker_skips_when_mountpoint_false`** -- closure
   returns `Err`, `ops.set_mounted(false)`. Assert: helper returns the
   `Err`, `ops.calls()` does NOT contain a start. Pins the
   `is_mountpoint` gate as load-bearing for Err-branch safety.
4. **`run_with_online_marker_skips_when_config_none`** -- closure
   returns `Err`, `cfg = None` (the dry-run shape). Assert: helper
   returns `Err`, no calls at all. Pins the dry-run bypass.

### Step 5: Unit tests for the membership fallback

Add to `cli/src/main.rs`'s existing `#[cfg(test)] mod tests` (around
lines 1109-1199 -- the parser-test block has the right harness for
testing dispatch-adjacent helpers; `load_membership_for_lock_or_exit`
is the right granularity for unit tests since it does not need
RealRunner).

Use `tempfile::tempdir` + `StatePaths` (the pattern used by existing
tests like `cli/src/unlock.rs:1364-1368` and the `isolated_paths`
helper). Test the `Result`-returning inner function
`load_membership_for_lock` directly; the `_or_exit` wrapper is
trivial and verified by inspection.

1. **`load_membership_for_lock_uses_pool_json_when_present`** --
   write a `pool.json` with two disks, no journal. Assert: returns
   the two-disk membership.
2. **`load_membership_for_lock_falls_back_to_bootstrap_journal`** --
   no `pool.json`, write a bootstrap-add journal
   (`OpKind::Add { .. }`, `pre_membership.is_empty()`,
   `target_membership` with one disk). Assert: returns the
   one-disk membership.
3. **`load_membership_for_lock_rejects_non_bootstrap_journal`** --
   no `pool.json`, write an `OpKind::Remove` journal (or
   `OpKind::Add` with non-empty `pre_membership`). Assert: returns
   `LoadForLockError::NoMembershipAvailable`.
4. **`load_membership_for_lock_rejects_when_no_pool_json_no_journal`**
   -- nothing on disk. Assert: returns
   `LoadForLockError::NoMembershipAvailable`.
5. **`load_membership_for_lock_propagates_corrupt_pool_json`** --
   `pool.json` exists but is invalid JSON. Assert: returns
   `LoadForLockError::Membership(MembershipError::Corrupt { .. })`,
   **not** the fallback. (`Display` rendering preserves the
   existing corrupt-pool-json remediation text.)
6. **`load_membership_for_lock_surfaces_corrupt_journal`** -- no
   `pool.json`, write `pending-op.json` with invalid JSON. Assert:
   returns `LoadForLockError::Journal(JournalError::Parse { .. })`,
   and that `e.to_string()` contains the pinned `Parse` remediation
   text from `cli/src/journal.rs:205-208` (`"Remove
   /var/lib/braid/pending-op.json after manual reconciliation"`).
   Pins the operator-facing remediation for the corrupt-journal
   case in the lock path. **This is the regression test for the
   reviewer's finding.**
7. **`load_membership_for_lock_surfaces_journal_io_error`** -- no
   `pool.json`, create `pending-op.json` as a directory so
   `std::fs::read_to_string` fails with a non-NotFound Io error.
   Assert: returns `LoadForLockError::Journal(JournalError::Io {
   .. })`. Pins that read failures other than NotFound are
   distinguished from "no journal present".

### Step 6: VM test pinning the end-to-end scenario

Add `tests/module/post-mount-failure-marks-online.nix` and
`tests/module/post-mount-failure-marks-online.py`, registered in
`flake.nix` `checks.aarch64-darwin` alongside the existing module
tests (see [`docs/testing.md`](../../docs/testing.md) for the
registration form).

Required preamble (per AGENTS.md "Test Conventions"):

```
# Intent: a bootstrap `braid add` whose mount succeeds but whose
#   post-mount cleanup step fails must (a) leave braid-online.service
#   active, and (b) leave the pool recoverable via systemctl stop
#   braid-online.service driven by the bootstrap-add journal.
# Why it exists: a previous bug left the pool mounted while
#   braid-online.service stayed inactive when cmd_add returned Err
#   post-mount; even after activating the service, ExecStop could
#   not unmount because pool.json was never written. Both halves of
#   the lifecycle hole must close (ADR 026).
# Scenario: single-disk fresh-pool bootstrap-add. Before `braid add`,
#   pre-create /var/lib/braid/acked-stats.json as a directory so the
#   post-mount alert::remove_acked_stats fails after pool_bootstrap_mount
#   has committed and BEFORE save_membership runs (so pool.json never
#   exists). Assert (1) the add exits non-zero, (2) the pool is mounted,
#   (3) braid-online.service is active, (4) pool.json does not exist,
#   (5) pending-op.json exists and is a bootstrap-add journal, (6)
#   `systemctl stop braid-online.service` unmounts /mnt/storage and
#   closes the LUKS mapper.
```

**Fault injection: acked-stats.json as a directory.** This is the
established precedent at `cli/src/add.rs:5641-5680`
(`cmd_add_bootstrap_acked_cleanup_failure_is_fatal`). It triggers
the failure at `add.rs:1357` (`alert::remove_acked_stats`),
**after** `pool_bootstrap_mount` has committed and **before**
`save_membership` would have written `pool.json`. This is the
maximally-dangerous post-mount failure: pool is mounted, journal is
on disk, no pool.json yet.

**Do not use `pool.json` as a directory** -- `plan_add` at
`cli/src/add.rs:1570-1578` loads `pool.json` before any LUKS or
mount work runs, so making it a directory yields a pre-mount
planning failure, not the intended post-mount cleanup failure. (The
existing `unlock_tolerates_post_mount_save_membership_failure` test
at `cli/src/unlock.rs:1364-1368` uses the pool.json-directory
pattern, but that works for unlock because unlock does not pre-load
pool.json in the same way.)

Test body (sketch):

1. Pre-create `/var/lib/braid/acked-stats.json` as a directory.
2. Run `braid add disk1=/dev/disk/by-id/virtio-disk1` with the
   passphrase on stdin. Expect non-zero exit and an error mentioning
   `AckCleanupFailed` / `bootstrap`.
3. Assert `mountpoint -q /mnt/storage` succeeds (pool is mounted
   despite the error).
4. Assert `systemctl is-active braid-online.service` returns
   `active`. (Pins the online-finalizer half of the fix.)
5. Assert `! test -e /var/lib/braid/pool.json` (file truly never
   written -- proves the journal-fallback path matters).
6. Assert `test -f /var/lib/braid/pending-op.json` and that its
   contents parse as a bootstrap-add journal.
7. Run `systemctl stop braid-online.service`.
8. Assert `! mountpoint -q /mnt/storage` (pool is unmounted by
   ExecStop).
9. Assert `! ls /dev/mapper/braid-* 2>/dev/null` (LUKS mappers
   closed by ExecStop). (Pins the journal-fallback half of the
   fix.)
10. Cleanup: `rm -rf /var/lib/braid/acked-stats.json
    /var/lib/braid/pending-op.json`.

Skip the parallel Recover scenario for now -- the online-finalizer
shape is identical and code review covers it. The journal-fallback
already has its full coverage path through this single test because
the bootstrap-add scenario exercises both halves.

### Step 7: ADR updates

`docs/decisions/026-pool-lock-rust-owned.md` currently says (lines
54-58):

> Post-success lifecycle work also lives under the Rust-held pool lock:
> - `unlock`, `add`, and `recover` call `mark_online` after a successful mount.
> - Plain `braid lock` calls `mark_offline` after successful unmount/close.

Replace the first bullet with: "After every `unlock`, `add`, and
`recover` attempt -- success or failure -- the dispatch arm runs
`mark_online` as a finally-block under the held pool lock. The
`is_mountpoint` gate inside `mark_online` short-circuits when the
operation failed before mounting; the bootstrap-add / recover case
where the mount succeeded but a later step returned `Err` is
exactly where this finally-block matters." Leave the
`mark_offline` bullet unchanged -- the success-gated shape is
intentional and load-bearing for the
`execstop-cleans-stale-online` safety net.

Add a new subsection to ADR 026 titled "Bootstrap-Journal Membership
Fallback" documenting:
- Lock-side dispatch loads pool.json normally; on NotFound it falls
  back to the bootstrap-add journal's `target_membership`.
- Detection rule: `OpKind::Add` with empty `pre_membership`.
- Why: closes the second half of the lifecycle hole -- ExecStop
  must complete cleanup even when `save_membership` never ran.
- Out of scope: other OpKind variants. Those require pool.json or
  run through `braid recover`.

Update `docs/decisions/018-systemd-lifecycle.md` "Rust dispatch as
synchronization layer" (lines 123-130) with the same wording shift
on the online finalizer. Note the new ExecStop journal-fallback
behavior in the shutdown sequence numbered list (line 139-143).

## Critical files

- `cli/src/online_state.rs` -- add one helper (`run_with_online_marker`)
  and four unit tests.
- `cli/src/main.rs` -- three dispatch arms (Add, Unlock, Recover)
  switch to the helper; `LoadForLockError` enum,
  `load_membership_for_lock` (Result-returning), and
  `load_membership_for_lock_or_exit` (thin exit wrapper) added next
  to `load_membership_or_exit`; two call sites (`run_plain_lock`,
  `run_systemd_stop_lock`) swap to the new loader. Plain-lock and
  ExecStop lifecycle calls untouched.
- `tests/module/post-mount-failure-marks-online.nix` (new)
- `tests/module/post-mount-failure-marks-online.py` (new)
- `flake.nix` -- register the new VM test in `checks.aarch64-darwin`.
- `docs/decisions/026-pool-lock-rust-owned.md` -- adjust online
  finalizer wording; add the new "Bootstrap-Journal Membership
  Fallback" subsection.
- `docs/decisions/018-systemd-lifecycle.md` -- mirror wording update
  on online finalizer; note ExecStop journal fallback.

## Existing code to reuse

- `mark_online` at `cli/src/online_state.rs:233-290` -- already has
  the `is_mountpoint` gate that makes Err-branch calls safe.
- `mark_offline` at `cli/src/online_state.rs:292-311` -- **not
  wrapped**; stays as-is.
- `OnlineSnapshot` and `snapshot` at `cli/src/online_state.rs:221-231`.
- `RecordingOnlineStateOps` at `cli/src/online_state.rs:314-388` --
  seam for the new online-marker unit tests.
- The `cfg` test helper at `cli/src/online_state.rs:395` -- builds a
  `Config` from a JSON literal.
- `membership::load_membership` at `cli/src/membership.rs:424-491`
  -- the NotFound discrimination uses
  `MembershipError::Io { source, .. }` with
  `source.kind() == ErrorKind::NotFound`, matching the existing
  fallback pattern at `cli/src/add.rs:1570-1578`.
- `journal::load_journal` at `cli/src/journal.rs:242-253` -- returns
  `Ok(None)` for missing pending-op.json; the new helper threads on
  `Ok(Some(j)) if matches!(&j.op, OpKind::Add { .. }) && j.pre_membership.is_empty()`.
- `journal::OpKind::Add` at `cli/src/journal.rs:168-171` -- the
  bootstrap signature is empty pre_membership.
- The acked-stats.json-as-directory fault-injection pattern at
  `cli/src/add.rs:5641-5680`
  (`cmd_add_bootstrap_acked_cleanup_failure_is_fatal`).
- The stale-online cleanup contract pinned by
  `tests/module/execstop-cleans-stale-online.py:34-38` -- exactly
  the safety net that the success-gated `mark_offline` /
  `mark_done()` shape preserves.
- The systemd-unit override fault-injection pattern at
  `tests/module/systemd-lifecycle.py:254-258` (for future tests if
  needed).

## Verification

1. `just test-rust` -- runs the four new online-marker unit tests
   in `cli/src/online_state.rs` and the seven new
   `load_membership_for_lock` unit tests in `cli/src/main.rs`.
   `run_with_online_marker_calls_mark_online_on_err`,
   `load_membership_for_lock_falls_back_to_bootstrap_journal`, and
   `load_membership_for_lock_surfaces_corrupt_journal` must fail
   before the fix and pass after.
2. `just test-vm post-mount-failure-marks-online` -- runs the new
   VM test. Must fail before the fix and pass after. Both
   assertions (braid-online active; ExecStop unmounts + closes
   mappers) are load-bearing; either failure indicates the fix
   regressed. Run with `--verbose` only if non-verbose output does
   not explain a failure (per AGENTS.md test verbosity rule).
3. `just test-vm systemd-lifecycle execstop-cleans-stale-online
   auto-unlock-key-present` -- the existing lifecycle suites must
   still pass. Critically: `execstop-cleans-stale-online.py` pins
   that braid-online.service stays active after an out-of-band
   unmount and ExecStop closes orphan mappers -- this contract
   would break if `mark_offline` were moved to a finally-block, so
   keeping it success-gated is verified here.
4. `just test-vm` -- full unstable-excluded VM suite passes.
5. Code review checklist:
   - The three online dispatch arms (Add, Unlock, Recover) call
     `run_with_online_marker`.
   - `run_plain_lock` lifecycle order is unchanged: `cmd_lock` ->
     `mark_done()` -> `mark_offline`, all success-gated.
   - `run_systemd_stop_lock` does NOT call `mark_offline`.
   - Both `run_plain_lock` and `run_systemd_stop_lock` use
     `load_membership_for_lock_or_exit`.
   - The fallback in `load_membership_for_lock` rejects non-bootstrap
     journals (live-pool-add with non-empty pre_membership, Remove,
     RemoveMissing, Replace).
   - Corrupt `pool.json` surfaces as `LoadForLockError::Membership(_)`
     and corrupt or unreadable `pending-op.json` surfaces as
     `LoadForLockError::Journal(_)` -- the pinned `JournalError::Parse`
     remediation text from `cli/src/journal.rs:205-208` (referenced by
     `docs/luks-unlock.md`) reaches the operator instead of being
     collapsed into `NoMembershipAvailable`.
