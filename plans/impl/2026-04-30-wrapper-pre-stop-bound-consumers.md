# Plan: pre-stop pool consumers in `braid-wrapper.sh` via `BoundBy`

## Context

`sudo braid lock` fails with `EBUSY` when long-running pool consumers (samba on caja today; future nfs/syncthing) hold open files under `/mnt/storage`. The CLI in `cli/src/lock.rs` calls `umount(2)` directly; nothing pre-stops consumers. systemd's `BindsTo` cascade fires only when systemd itself drives the stop (`systemctl stop braid-online.service`, i.e. shutdown) -- not on user-initiated `braid lock`.

The wrapper already pre-stops `braid-scrub.{timer,service,resume-trigger}` for exactly this reason (`modules/braid/braid-wrapper.sh:49-64`); samba is the next instance and the precedent is already wired. `docs/decisions/018-systemd-lifecycle.md:175` documents the contract long-running consumers must declare (`After=` + `BindsTo=braid-online.service`); the existing `~/world` commit 586c1c6 satisfies it correctly. **The bug is purely in the wrapper, not in samba.nix and not in the Rust CLI.**

systemd exposes the inverse of `BindsTo=X` as the read-only `BoundBy=` property on X. So `systemctl show -P BoundBy braid-online.service` is a single source of truth for "what must stop before lock" -- adding a new consumer requires only the `BindsTo` declaration, no wrapper edit.

Intended outcome: `sudo braid lock` succeeds whenever a bound consumer is holding the mount busy, by stopping bound consumers in the wrapper before invoking the CLI. The CLI stays the low-level pool mutator with unchanged behavior.

## Approach

Add a generic `BoundBy` pre-stop loop to `braid-wrapper.sh`, **after** the existing scrub block (lines 56-64). Keep the scrub block verbatim: it has documented ordering (timer -> trigger -> service) to prevent re-trigger races (`braid-wrapper.sh:50-55`) that do not apply to long-running consumers. The new loop skips the three scrub units to avoid cosmetic re-stop noise.

No changes to `cli/src/lock.rs`, `cli/src/unlock.rs`, `cli/src/cmd.rs`, or any Rust file. No change to `~/world/hosts/caja/modules/samba.nix` (already correct). No unlock-side change: consumers with `wantedBy=braid-online.service` already auto-activate when the wrapper starts `braid-online.service` post-unlock (`braid-wrapper.sh:89`); cascade activation is free, only cascade deactivation needs a wrapper-side helper.

### Wrapper change (`modules/braid/braid-wrapper.sh`)

Insert immediately after line 64 (end of the existing scrub `case` block), before line 66 (the `9>&-` exec into the CLI):

```bash
# Pre-stop pool consumers (samba, nfs, future) declared via
# BindsTo=braid-online.service. systemd exposes the inverse as the BoundBy=
# read-only property, making this single-source-of-truth: a new consumer
# only needs the BindsTo declaration, no wrapper edit.
#
# The scrub block above is left in place because it encodes
# timer->trigger->service ordering to prevent re-trigger races that don't
# apply to long-running consumers. We skip the three scrub units here to
# avoid cosmetic re-stop noise.
case "$subcmd" in
  lock)
    if ! $skip_fixup; then
      bound_by=$(@systemctlBin@ show -P BoundBy braid-online.service 2>/dev/null || true)
      for unit in $bound_by; do
        case "$unit" in
          braid-scrub.timer|braid-scrub.service|braid-scrub-resume-trigger.service)
            continue ;;
        esac
        ec=0
        @systemctlBin@ stop "$unit" || ec=$?
        if [ "$ec" -ne 0 ]; then
          echo "braid: WARNING: failed to stop $unit (exit $ec) -- continuing; umount may fail" >&2
        fi
      done
    fi
    ;;
esac
```

Why error reporting differs from the scrub block above: the scrub block uses `2>/dev/null || true` because its units may not exist (`autoScrub.enable = false`). The new block trusts `BoundBy` -- anything systemd reports as bound exists, so a non-zero exit is a real failure the user should see. We still attempt the lock (the consumer may have been mid-deactivation; umount may still succeed).

Edge cases:
- **`braid-online.service` not declared** (non-NixOS, no module): `systemctl show` returns empty, loop is a no-op.
- **ExecStop reentry** (`systemctl stop braid-online.service` -> `ExecStop=braid lock`): bound consumers were already deactivated by systemd's BindsTo cascade before ExecStop ran, so each `systemctl stop` is a no-op (exit 0). The post-CLI `--no-block` path at lines 94-104 is untouched.
- **Hung consumer:** systemd's default `TimeoutStopSec=90s` applies. Wrapper blocks until timeout, then warns and continues. Same cost as any systemctl stop today.

### Doc updates (`docs/decisions/018-systemd-lifecycle.md`)

Two surgical edits:

1. **"On `lock`" sequence (lines 126-130)** -- update step 1 to: "Wrapper stops `braid-scrub.timer`, then `braid-scrub-resume-trigger.service`, then `braid-scrub.service` (ordered to prevent re-trigger races); then iterates `systemctl show -P BoundBy braid-online.service` and stops each remaining bound consumer."

2. **"Long-running services holding open files" (line 175)** -- append one sentence: "The wrapper iterates `BoundBy braid-online.service` and stops these consumers before invoking `braid lock`, mirroring the cascade systemd performs on shutdown."

The "Consumer dependency contracts" scrub paragraph at line 173 is unchanged; the explicit scrub ordering remains a documented special case.

### Regression test

New single-VM test, mirroring the `cancel` node in `tests/module/scrub-lifecycle.nix:124-141`. The test must cover **both** lock paths because both go through the new wrapper block: user-initiated `braid lock` and systemd-driven `systemctl stop braid-online.service` (`storage.nix:131` declares `ExecStop=${braidWrapped}/bin/braid lock`, so ExecStop reenters the wrapper).

**`tests/module/lock-stops-bound-consumers.nix`** -- imports `../../modules/braid` and the LUKS+btrfs fixture (`./lib/initrd-fixture.nix`); declares a fake consumer:

```nix
systemd.services.dummy-pool-consumer = {
  description = "Fake long-running consumer that holds /mnt/storage busy";
  wantedBy = [ "braid-online.service" ];
  after = [ "braid-online.service" ];
  bindsTo = [ "braid-online.service" ];
  unitConfig.ConditionPathIsMountPoint = "/mnt/storage";
  serviceConfig = {
    Type = "simple";
    ExecStart = pkgs.writeShellScript "dummy-consumer" ''
      exec 3>/mnt/storage/.consumer-lock
      sleep 300
    '';
  };
};
```

Preamble (top of the `.nix` file): use the documented `/* ... */` Intent / Why it exists / Scenario form per `docs/testing.md:11-13`. The `# Test:` style in `scrub-lifecycle.nix:1-21` is grandfathered, not the standard. Nix supports `/* */` block comments.

```nix
/*
 * Intent: braid lock (user-initiated) and systemctl stop braid-online.service
 *   (shutdown / manual ExecStop) both stop bound pool consumers and unmount
 *   cleanly when a long-running consumer holds /mnt/storage busy.
 * Why it exists: regression guard for the EBUSY-on-busy-mount class of bug
 *   (samba on caja, future nfs/syncthing). User-initiated lock relies on
 *   the wrapper iterating BoundBy braid-online.service; ExecStop relies on
 *   systemd's BindsTo cascade. Both paths run through braid-wrapper.sh's
 *   pre-stop block, so both must be tested.
 * Scenario: pool unlocked with a fake consumer service holding fd 3 on
 *   /mnt/storage/.consumer-lock. Cycle 1 runs `braid lock` and asserts
 *   teardown. Cycle 2 unlocks again, runs `systemctl stop
 *   braid-online.service`, asserts the same teardown.
 */
```

**`tests/module/lock-stops-bound-consumers.py`** -- two cycles:

*Setup once:* `braid unlock`; assert pool mounted; assert `dummy-pool-consumer.service` is `active`; resolve `pid=$(systemctl show -P MainPID dummy-pool-consumer.service)`; assert `readlink /proc/$pid/fd/3` resolves under `/mnt/storage` (proves the consumer is genuinely holding the mount busy without depending on `fuser`/`lsof` being on PATH -- `commonNode` from `scrub-lifecycle.nix:73` only ships `btrfs-progs` + `cryptsetup`); assert `systemctl show -P BoundBy braid-online.service` lists `dummy-pool-consumer.service` (behavior-locks the `BoundBy` property name/shape against systemd version drift -- the wrapper depends on this).

*Cycle 1 -- user-initiated lock:*
1. `sudo braid lock` exits 0.
2. `dummy-pool-consumer.service` is `inactive`.
3. `mountpoint -q /mnt/storage` returns non-zero.
4. `test ! -e /dev/mapper/braid-disk1` (LUKS mapper closed). Use the device-node check rather than `cryptsetup status`: the latter exits non-zero for inactive mappers and would trip the NixOS test driver's auto-prepended `set -euo pipefail` (`docs/testing.md:48-60`), aborting the script even when lock behavior is correct. Matches the assertion style used by existing lifecycle tests.

*Cycle 2 -- ExecStop reentry:*
5. `sudo braid unlock`; reassert consumer active, pool mounted, fd 3 under `/mnt/storage`.
6. `systemctl stop braid-online.service` exits 0.
7. `dummy-pool-consumer.service` is `inactive`.
8. `mountpoint -q /mnt/storage` returns non-zero.
9. `test ! -e /dev/mapper/braid-disk1` (LUKS mapper closed).

Cycle 2 directly exercises the "ExecStop reentry is harmless" claim in this plan -- the wrapper's `BoundBy` loop runs a second time with consumers already deactivated by systemd's own cascade, and the post-CLI `systemctl stop --no-block braid-online.service` at `braid-wrapper.sh:101` must not deadlock against the in-progress stop.

Register in `flake.nix` checks alongside `scrub-lifecycle`:

```nix
lock-stops-bound-consumers = pkgs.testers.nixosTest (
  import ./tests/module/lock-stops-bound-consumers.nix { braid = linuxCrane.braid-cli-unwrapped; }
);
```

## Critical files

- `modules/braid/braid-wrapper.sh:49-64` -- existing scrub pre-stop block (style model)
- `modules/braid/braid-wrapper.sh:73` -- CLI invocation; new block goes immediately above
- `modules/braid/braid-wrapper.sh:94-104` -- post-CLI `--no-block` reentry handling; must not be disturbed
- `modules/braid/storage.nix:54-113` -- the three scrub units `BoundBy` returns today
- `docs/decisions/018-systemd-lifecycle.md:126-130, 175` -- doc sections to update
- `tests/module/scrub-lifecycle.nix:124-141` -- pattern for the new test's fake consumer
- `tests/module/scrub-lifecycle.py` -- pattern for the new test's Python assertions
- `flake.nix` -- new test registration

## Reused utilities

- `./lib/initrd-fixture.nix` -- the LUKS+btrfs fixture used by `scrub-lifecycle.nix:36-40`. Reuse verbatim.
- `commonNode` pattern from `scrub-lifecycle.nix:31-77` -- includes pool.json seeding and the `braid-unlock.script` override that bypasses interactive `systemd-ask-password` for VM tests. Mirror the structure rather than duplicating; the dummy-consumer node is `commonNode` + the `dummy-pool-consumer` service.

## Verification

1. `just test-rust` -- existing Rust unit tests (`lock_umount_busy_fails`, `lock_adds_forget_after_umount`, etc.) stay green; no MockRunner edits needed because the wrapper change is below the CLI's view.
2. `just test-vm scrub-lifecycle` -- existing scrub regression still passes; validates the explicit scrub ordering survived the refactor.
3. `just test-vm lock-stops-bound-consumers` -- new test passes; validates `BoundBy` iteration on a non-scrub consumer.
4. `just test-vm` -- full VM suite (no other test should regress).
5. **Pre-deploy spot-check on caja:** `systemctl show -P BoundBy braid-online.service`. Expected: `samba-smbd.service samba.target braid-scrub.timer braid-scrub.service braid-scrub-resume-trigger.service` (order may vary). Anything unexpected -> investigate before generalizing.
6. **Manual end-to-end on caja (post-deploy):** `sudo braid unlock`; from a Mac LAN client, mount `//caja/creepy`, open a file; `sudo braid lock` -> exits 0; `sudo braid unlock` -> samba returns to active via `wantedBy=braid-online.service` cascade.

## Out of scope

- `cli/src/lock.rs` / `cli/src/unlock.rs` / `cli/src/cmd.rs` -- no Rust changes.
- `~/world/hosts/caja/modules/samba.nix` -- commit 586c1c6 conforms exactly to `018-systemd-lifecycle.md:175`; leave it.
- Multi-machine VM test with a real SMB client -- introduces a new pattern (no multi-machine tests in `tests/module/` today) just for this one fix; the single-VM mock exercises the wrapper change directly and behavior-locks `BoundBy` itself.
- Replacing the existing scrub pre-stop block with a generic ordered loop -- the scrub block has tightly documented race-prevention ordering; folding it into the generic loop would require re-encoding that ordering in bash and adds risk for no behavioral gain.
