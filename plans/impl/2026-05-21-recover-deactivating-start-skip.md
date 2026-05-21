# Plan: Recover subtest for the deactivating snapshot rule

## Context

`docs/decisions/026-pool-lock-rust-owned.md` (the "Snapshot Rule On
`systemctl start`" section) says every mount-producing mutator must
take its `braid-online.service` `ActiveState` snapshot inside the
pool-lock window, and `mark_online` must skip `systemctl start` when
that snapshot saw `deactivating` -- otherwise the start queues behind
an in-flight stop and can deadlock the lifecycle unit.

Three dispatch arms take this snapshot under the pool lock:

- `Commands::Add`         -- `cli/src/main.rs:510-513`
- `Commands::Unlock`      -- `cli/src/main.rs:680-681`
- `Commands::Recover`     -- `cli/src/main.rs:958-959`

Today only the add arm is regression-tested for the deactivating
gate: `tests/module/mark-online-skips-start-while-deactivating.py`.
The recover arm has only the happy-path coverage in
`tests/module/systemd-lifecycle.py:295-337` (snapshot saw `inactive`,
expect `active` afterward).

These two tests form a layered defense, and the new subtest fits one
specific layer. Specifically:

- `tests/module/systemd-lifecycle.py:295-337` already covers
  **recover marker presence**: recover collects a snapshot, threads
  it into `mark_online` via `run_with_online_marker`, and the
  `Inactive` branch of `mark_online` calls `systemctl_start`. If
  recover passed `None` to `mark_online` (i.e. stopped collecting the
  snapshot at all), that happy-path test would fail because the
  `if let Some(snap) = snap` short-circuit at
  `cli/src/online_state.rs:286` would skip the start.

- This new subtest covers the **`Deactivating` gate value** for
  recover: with a real stop in progress, recover's snapshot must
  observe `Deactivating` and `mark_online`'s `Deactivating` arm at
  `cli/src/online_state.rs:301-307` must fall through without calling
  `systemctl_start`. Today this branch is only exercised by the add
  command.

(The unlock arm has the same gap, but it is not naturally testable
with the existing drop-in: the drop-in replaces ExecStop with
`sleep 10`, so the pool stays mounted during "deactivating" and
`braid unlock` short-circuits on already-mounted. Out of scope for
this plan.)

Framing note: as analyzed in the verify-issue pass, this test does
not pin "snapshot is taken after lock" literally -- it holds the
unit in `deactivating` *before* invoking recover, so the snapshot
value would be the same regardless of whether it's taken before or
after lock acquire. Pinning the literal lock-ordering would need a
more complex external-lock-holder choreography that the project does
not need today. The narrower deactivating-value coverage is the
realistic regression risk.

## Approach

Append a new subtest "braid recover succeeds without re-starting"
to `tests/module/mark-online-skips-start-while-deactivating.py`,
placed BEFORE the existing final "Cleanup" subtest. Re-use the
slow-ExecStop drop-in already installed by the file's first subtest.

Precedent for multi-command coverage in one file:
`tests/module/pool-lock-precedes-state-read.py` (five subtests, one
invariant, one VM).

The recover subtest mirrors the add subtest's structure, with these
deltas:

- Re-activate `braid-online.service` between the two subtests
  (the add subtest leaves it inactive).
- Inject a `pending-op.json` whose journal is a live-pool reconcile:
  `op.op = "Add"`, `op.phase = "PostAddBalanceRaid1"`, and
  `pre_membership == target_membership == current pool.json`. This is
  the same shape used by `tests/module/systemd-lifecycle.py:309-318`.
- Run `braid recover --passphrase-stdin` (passphrase is unused: when
  the pool is already mounted, `plan_open_pool` returns `Ok(None)`
  at `cli/src/mount.rs:215-223`, so recover skips the
  `InitialOpenPool` action at `cli/src/recover.rs:1407-1409` and
  the credential-resolution block at `cli/src/recover.rs:960-969` is
  short-circuited by the early-return at `:951-954`. The existing
  unit test `recover_skips_mount_when_already_mounted`
  (`cli/src/recover.rs:12625`) pins this path).
- Assert the regression signature using final-state checks (no
  journal cursors -- the NixOS python driver lacks a first-class
  cursor API, and `journalctl --since=...` against an interleaving
  stop+recover would be racy).

## Files to modify

- `tests/module/mark-online-skips-start-while-deactivating.py` -- add
  one subtest plus an `import json` and a one-line coverage note in
  the preamble. No `.nix` change (existing fixture already has
  disk1+disk2; the pool is 2-disk after the add subtest, which is the
  state the new subtest assumes).

## Existing code reused

- `cli/src/mount.rs:215-223` -- `plan_open_pool` returns `Ok(None)`
  when the pool is already mounted.
- `cli/src/recover.rs:1407-1409` -- recover skips the
  `InitialOpenPool` action when `open_plan` is `None`; combined with
  the early-return at `cli/src/recover.rs:951-954`, the credential is
  never resolved. Pinned by the unit test
  `recover_skips_mount_when_already_mounted` at
  `cli/src/recover.rs:12625`.
- `cli/src/main.rs:949-998` -- the `Commands::Recover` dispatch arm
  that takes the snapshot at `:958-959` and routes through
  `run_with_online_marker`.
- `cli/src/online_state.rs:246-251` (`snapshot`) and `:285-307`
  (`mark_online`'s `Deactivating` arm -- a falls-through no-op that
  the test depends on).
- `tests/module/systemd-lifecycle.py:295-337` -- working
  `PostAddBalanceRaid1` journal template.
- `tests/module/mark-online-skips-start-while-deactivating.py`
  existing subtests "Install slow ExecStop drop-in", "Unlock pool",
  "Hold braid-online in deactivating", "braid add succeeds...",
  "Cleanup".

## Implementation steps

1. Update the preamble of
   `tests/module/mark-online-skips-start-while-deactivating.py` so
   the test conventions still match. Keep the existing intent
   sentence (it's already plural-ready -- "A mount-producing
   mutator..."). Extend the "Scenario" section with one sentence:

   > Covered mutators: `braid add` (LUKS-format + mount path) and
   > `braid recover` (already-mounted skip path: `plan_open_pool`
   > returns `None`, so `InitialOpenPool` is not pushed).

2. Add `import json` next to the existing `import shlex`.

3. Insert a new subtest immediately before the existing "Cleanup"
   subtest:

   ```python
   with subtest("braid recover succeeds without re-starting braid-online"):
       # Re-activate braid-online so we can put it back into deactivating.
       machine.succeed("systemctl start braid-online.service")
       machine.wait_until_succeeds(
           "systemctl is-active --quiet braid-online.service", timeout=30
       )

       # Inject a live-pool reconcile journal: PostAddBalanceRaid1 with
       # matching membership. Mirrors tests/module/systemd-lifecycle.py
       # subtest 8.
       pool_json_raw = machine.succeed("cat /var/lib/braid/pool.json")
       pool_membership = json.loads(pool_json_raw)
       journal = {
           "started_at": "2026-01-01T00:00:00Z",
           "op": {
               "op": "Add",
               "phase": "PostAddBalanceRaid1",
               "targets": {},
           },
           "pre_membership": pool_membership,
           "target_membership": pool_membership,
       }
       journal_json = json.dumps(journal)
       machine.succeed(
           f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
           f"{journal_json}\n"
           f"JOURNAL_EOF"
       )

       # Hold braid-online in deactivating (slow ExecStop drop-in is still
       # installed from the first subtest).
       stop_pid = machine.succeed(
           "nohup systemctl stop braid-online.service "
           ">/tmp/recover-stop.log 2>&1 & echo $!"
       ).strip()
       machine.wait_until_succeeds(
           "test \"$(systemctl show -P ActiveState braid-online.service)\" "
           "= deactivating",
           timeout=10,
       )

       # Recover with snapshot=deactivating must succeed and must NOT
       # queue a systemctl start that fires after the stop drains.
       machine.succeed(
           f"printf %s\\\\n {pq} | braid recover --passphrase-stdin"
       )
       machine.wait_until_fails(f"kill -0 {stop_pid} 2>/dev/null", timeout=30)
       machine.succeed("mountpoint -q /mnt/storage")
       machine.fail("systemctl is-active --quiet braid-online.service")
       machine.fail("test -f /var/lib/braid/pending-op.json")
   ```

   The trailing `pending-op.json` assertion is recover-specific (the
   add subtest has no journal to clear). It proves the dispatch arm
   reached completion rather than failing silently before
   `mark_online` could matter.

4. Leave the existing "Cleanup" subtest unchanged. Its `braid lock`
   already handles the post-recover state (pool mounted, service
   inactive -- same as the post-add state).

## Verification

- `just test-vm mark-online-skips-start-while-deactivating` --
  the new subtest must pass alongside the existing one.
- Optional sanity check that the test would catch the realistic
  regression (snapshot value gate broken): temporarily replace
  `cli/src/main.rs:958-959` with a hardcoded `Inactive` snapshot --

  ```rust
  let online_snapshot = (!args.dry_run && config.systemd_lifecycle())
      .then(|| braid_cli::online_state::OnlineSnapshot {
          online_state: braid_cli::online_state::UnitActiveState::Inactive,
      });
  ```

  With this regression, recover's `mark_online` call would hit the
  `Inactive` arm at `cli/src/online_state.rs:289-295` and invoke
  `systemctl_start`. That start queues behind the in-progress stop,
  fires once the sleep ExecStop drains, and leaves
  `braid-online.service` active -- which trips the new subtest's
  `machine.fail("systemctl is-active --quiet braid-online.service")`
  assertion. Revert before committing.

  (Note: passing `None` to `run_with_online_marker` would NOT cause
  this subtest to fail -- `mark_online`'s
  `if let Some(snap) = snap` short-circuit at
  `cli/src/online_state.rs:286` makes it a no-op, so the unit would
  still finish inactive and the assertion would pass for the wrong
  reason. That regression is covered by
  `tests/module/systemd-lifecycle.py:295-337` instead.)

- `just test-rust` -- should be unaffected; no Rust changes.

## Out of scope

- Unlock-arm coverage for the same invariant. Not naturally testable
  with the existing drop-in (see Context). Tracking only if a
  realistic refactor risk surfaces later.
- A structural fix that binds the snapshot to the lock guard via
  type (e.g. `snapshot(&PoolLockGuard, ...)`). Larger refactor,
  doesn't simplify the test surface, and over-engineered relative to
  the project's style.
- Helper extraction for the three near-duplicate snapshot setups at
  `main.rs:510-513`, `:680-681`, `:958-959`. Cosmetic, doesn't
  affect the snapshot-after-lock invariant (which is enforced by the
  match block sitting under `acquire_per_policy` at `:489`).
