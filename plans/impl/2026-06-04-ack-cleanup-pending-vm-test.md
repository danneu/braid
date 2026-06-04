# Plan: VM coverage for the ack-cleanup-pending cross-command contract

## Context

`docs/commands/ack.md` (the "what happens under the hood" section) promises a
cross-command contract: when `braid ack` reaches cleanup and a later step fails,
it leaves `/var/lib/braid/alert-cleanup-pending`; `braid status` then surfaces
`` ack cleanup pending -- re-run `braid ack` to resume `` as an alert cause; and
a sentinel-only `braid ack` re-enters cleanup directly and prints
`acknowledged current alerts`.

Both halves are heavily unit-tested -- `status.rs#resolve_alert_state` (and its
`resolve_alert_state_surfaces_cleanup_pending_as_computation_error` test) and the
`ack.rs#cmd_ack_impl` sentinel-only branch (`cmd_ack_mounted_sentinel_only_retry...`
and siblings). But every one of those tests runs against an `isolated_paths()`
temp dir with an injected runner; none drives the real `braid status` / `braid
ack` binaries against the production `/var/lib/braid` path. A wiring regression
in either command's sentinel handling (a path mismatch, a renderer that drops the
cause, a producer that marks the wrong file) would pass the entire unit suite.
This was shipped in `c0360184` ("fix(ack): resume cleanup after partial
failures") with unit tests only; no VM test ever followed.

This plan adds one focused VM test that drives the full produce -> surface ->
consume cycle over real files and real binaries.

## Decisions (grounded in `docs/dev/testing.md`)

- **Dedicated test file, not an extension of `braid-smartd-alert.py`.**
  testing.md §"Regression test quality": keep tests focused; do not bundle
  another phase into a test "whose failure would become ambiguous." The
  smartd-alert file's Intent is the smartd flag bridge; the cleanup-pending
  sentinel is ack's crash-recovery contract. Separate failure -> separate file.

- **Induce a real cleanup failure (deterministic failure injection), not a bare
  `touch` of the sentinel.** testing.md §"VM and command test design" prefers
  exactly this for ordering invariants: "allow the persistence step to succeed,
  force the next maintenance step to fail, then assert the persisted state is
  current and the journal still exists." This also tests "the layer where
  production failed" (ack's producer) rather than assuming the sentinel into
  existence.

- **Mounted scenario**, so ack's baseline-save persistence step (`save_acked_stats`)
  actually runs before the forced maintenance failure -- the realistic c0360184
  shape and the precise "persist before maintenance" invariant testing.md names.

- **`tests/cli/`, not `tests/repro/`.** The failure injection is deterministic
  (not timing-sensitive), and the contract deserves continuous coverage under the
  default `just test-vm` run. repro/ tests are excluded from that run.

## Failure-injection mechanism

`ack.rs#cleanup_alert_files_and_beeper` runs, in order: `stop_beeper`,
`mark_alert_cleanup_pending` (creates the sentinel), `remove_smartd_alert_flag`,
`remove_alert_latch`, `remove_alert_latch_corrupt`, `clear_alert_cleanup_pending`.
Each `remove_*` is a NotFound-tolerant `std::fs::remove_file`.

Poison the **`.corrupt` sidecar** (`alert-latch.json.corrupt`) as a **directory**:
`remove_file` on a directory returns `EISDIR` (not NotFound), so
`remove_alert_latch_corrupt` propagates the error and ack returns
`AckError::CleanupFailed` (exit 1) -- *after* the sentinel was marked and the
smartd flag + latch were removed. The sentinel survives. This is the established
idiom from `cmd_ack_mounted_sentinel_only_retry...` (`std::fs::create_dir(paths.alert_latch_corrupt())`).

Note: the poison must target `.corrupt`, not the sentinel or smartd paths --
`alert::alert_cleanup_pending` and `alert::smartd_alert_active` both require a
*regular file*, so a directory at those paths is ignored, not treated as active.

The smartd flag is the lightest realistic alert trigger that makes a mounted ack
reach cleanup (it forces past the `no active alerts` no-op into the
stats-snapshot + cleanup path).

## Files

- **Create `tests/cli/braid-ack-cleanup-pending.py`** -- the test script (below).
- **Create `tests/cli/braid-ack-cleanup-pending.nix`** -- copy
  `tests/cli/braid-smartd-alert.nix` verbatim, changing only `name` to
  `"braid-ack-cleanup-pending"` and the `testScript` path. It already provides the
  2-disk setup, `cryptsetup`/`btrfs-progs`/`jq` packages, and
  `/etc/braid/config.json` with `mount_point = "/mnt/storage"`.
- **Modify `flake.nix`** -- add, next to the `braid-smartd-alert` entry (~line 942):
  ```nix
  braid-ack-cleanup-pending = pkgs.testers.nixosTest (
    import ./tests/cli/braid-ack-cleanup-pending.nix { braid = linuxCrane.braid; }
  );
  ```
  (Match the exact `braid = ...` argument the adjacent `braid-smartd-alert`
  registration uses.) Per testing.md, an unregistered `.nix` never runs.

## Test script shape

Preamble (Intent / Why it exists / Scenario per AGENTS.md), then:

1. **Setup** -- reuse `braid-smartd-alert.py`'s pool bring-up: luksFormat + open
   both disks, `mkfs.btrfs -d raid1 -m raid1`, mount at `/mnt/storage`,
   `mkdir -p /var/lib/braid`. Assert healthy (`braid status` has no `ALERT`).

2. **Produce (ack fails mid-cleanup)** -- the persist-before-maintenance invariant:
   - `touch /var/lib/braid/smartd-alert` (alert trigger)
   - `mkdir /var/lib/braid/alert-latch.json.corrupt` (poison)
   - run ack capturing streams separately:
     `rc, _ = machine.execute("braid ack >/tmp/ack-fail.out 2>/tmp/ack-fail.err")`
   - assert `rc == 1` -- `cmd_ack` returning `CleanupFailed` is mapped to
     `std::process::exit(1)` by the `Commands::Ack` arm in `main.rs#main`. Assert
     the exact code, not `rc != 0`, so a dispatcher regression to exit 2 (or any
     other nonzero code) is caught. Deterministic here: config exists and the pool
     lock is free, so the forced `CleanupFailed` is the only error path. (braid
     precedent for exact-code VM assertions: `tests/repro/cryptsetup-close-mounted.py`.)
   - assert `test -f /var/lib/braid/alert-cleanup-pending` succeeds (sentinel produced)
   - assert `test -f /var/lib/braid/acked-stats.json` succeeds (baseline persisted
     before the failed maintenance step)
   - assert `test -f /var/lib/braid/smartd-alert` **fails** (cleanup got past the
     flag removal, proving the failure is at the later `.corrupt` step)

3. **Surface (`braid status` reports the cause)**:
   - `out = machine.succeed("braid status")`
   - assert `"ALERT" in out`
   - assert ``"ack cleanup pending -- re-run `braid ack` to resume" in out``
     (the exact `docs/commands/ack.md` string -- pins the messaging invariant)
   - assert `"SMART" not in out` (the only surfaced cause is the sentinel; the
     flag was already removed)

4. **Consume (retry clears it)**:
   - `rmdir /var/lib/braid/alert-latch.json.corrupt` (operator fixes the fault)
   - `rc, _ = machine.execute("braid ack >/tmp/ack-retry.out 2>/tmp/ack-retry.err")`
   - assert `rc == 0`
   - assert stdout (`/tmp/ack-retry.out`) `== "acknowledged current alerts\n"`
     (documented sentinel-only retry output; stdout captured separately per
     testing.md so the `systemctl stop` warning on stderr does not pollute it)
   - assert `test -f /var/lib/braid/alert-cleanup-pending` **fails** (sentinel cleared)

5. **Verify clean**:
   - `out = machine.succeed("braid status")`
   - assert `"ALERT" not in out` and `"ack cleanup pending" not in out`

   Then `machine.shutdown()`.

### Notes for the implementer

- Use `machine.execute` (not `succeed`/`fail`) for both ack invocations: the
  failing ack and the retry both emit a `warning: systemctl stop
  braid-alert.service` line on stderr (braid-alert.service is not installed in
  this VM, same as `braid-smartd-alert.py` documents). Assert on the captured
  exit code, never on stderr being empty.
- `braid status` exits 0 even when an alert is active (see `braid-smartd-alert.py`),
  so `machine.succeed("braid status")` is correct in steps 3 and 5.
- The retry's sentinel-only branch in `cmd_ack_impl` is hoisted above
  `probe_pool_alerts`, so it works whether or not the pool is still mounted; no
  unmount is needed before step 4.

## Verification

- `just test-vm braid-ack-cleanup-pending` -- the focused run; the flake attr name
  equals the test name (no `repro-` prefix, since this is a `cli/` test).
- TDD check: before writing the `.py` assertions' "expected" side, confirm the
  produce step actually leaves the sentinel and exit 1 (the test should fail for
  the right reason if, e.g., the poison path is wrong) -- i.e. run it once and
  read the failure, per braid's TDD-with-VM-tests workflow.
- No Rust, parser, fixture, or module changes -> `just test-rust`,
  `just test-parsers`, and fixture recapture are not required.
- Blast radius is test-only (one new VM test + its registration); a focused run is
  sufficient. No full-suite rerun is needed from me -- hand back for the user's
  full-suite run if desired.

## Implementation notes

- The `.nix` was not copied byte-for-byte from `braid-smartd-alert.nix`. Beyond
  `name` and the `testScript` path, the header comment block (`What`/`Why`/
  `Scenario`) was rewritten to describe the cleanup-pending contract; leaving the
  verbatim "smartd alert lifecycle" comment would misdescribe the new test. The
  Nix attribute set (disks, packages, config) is identical to the source.
