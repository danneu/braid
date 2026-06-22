# Plan: close the e2e gap for ENOSPC baseline invalidation on a real geometry change

## Context

`braid monitor` raises a proactive "ENOSPC risk" Warning (exit 3) when a RAID1
pool is one disk-loss away from being unable to allocate RAID1 chunk pairs. When
the operator runs `braid ack` on an at-risk pool, braid writes a suppression
baseline (`enospc-ack.json`) holding the signed `margin` plus a `PoolKey` (btrfs
FS UUID + sorted per-device `(devid, device_size)` pairs). Each tick the monitor
builds a live `PoolKey`; on a mismatch it discards the baseline and fires armed
(guard "F1"). `device_size` is in the key *specifically* to catch a same-devid
`braid replace`/resize -- the source devid is preserved but the chunk-pair
geometry the predicate depends on changes. This rationale is stated in code at
`cli/src/alert.rs#PoolKey`.

The discard LOGIC is exhaustively unit-tested across all three axes (changed
devid set, FS UUID, `device_size`) in
`cli/src/monitor.rs#cmd_monitor_stale_baseline_key_mismatch_fires_and_clears` --
but that test hand-builds the stale `PoolKey` (the `changed-device-size` case
literally seeds `(Devid::new(1), 50 * GIB)`). It stubs the seam where REAL device
geometry flows through the probe (`btrfs device usage --raw`), the parser, and
`cli/src/alert.rs#live_pool_key` into the field the key keys on. The one VM e2e
that exists, `tests/cli/braid-monitor-enospc.{nix,py}`, drives the full lifecycle
(fill -> exit 3 -> advisory routing -> ack -> suppressed re-run -> degraded) but
never exercises a geometry change. So no test proves a real `braid replace` trips
F1 end to end. The `device_size` axis is the newest/subtlest part of the key and
has zero real-hardware coverage.

This plan closes that gap with a focused sibling VM test, and fixes a stale
preamble in the existing test (see "Side fix").

## Verdict: build it (low cost, named gap, validates a design invariant)

Add one focused sibling VM test that drives a REAL `braid replace` onto a LARGER
target and asserts the monitor discards the stale baseline and re-fires (exit 3).
The marginal cost is small (a near-copy of the proven monitor harness plus one
extra disk), and it is the *only* test that ties the design rationale ("device_size
is in the key to catch a `braid replace`") to its end-to-end consequence. An
honest "don't build it" analysis and what would flip it are in the last section.

## Recommended vehicle: `braid replace` onto a larger target

Replace `disk1` (512 MiB) with a 1024 MiB `disk3` on an at-risk 2x512 MiB pool.

### Why this vehicle is feasible (and `braid add` is not)

- **A live `braid replace` runs NO rebalance**, so it has no allocator pressure
  and cannot ENOSPC on a full pool. `cli/src/replace.rs#ReplacePlan` emits the
  steps `btrfs replace start` -> (live-only) `cryptsetup close` ->
  `btrfs filesystem resize <devid>:max`. The soft-balance step is gated behind
  `restore_raid1_after_commit`, which `cli/src/replace.rs` derives from
  `cli/src/pool.rs#should_restore_raid1(will_clear_last_missing, ...)` -- true
  ONLY for a missing-path replace that clears the last missing device. A live
  replace skips it. `btrfs replace start` copies chunks from the healthy mirror
  (no new chunk allocation); `btrfs filesystem resize <devid>:max` is a
  metadata-only grow. Contrast `braid add`, whose post-add hard rebalance
  (`btrfs balance start -dconvert=raid1 -mconvert=raid1`) ENOSPCs on a full pool
  -- the ruled-out dead end.
- **The "smaller target" lead from the brief is infeasible.** btrfs rejects a
  target smaller than the source device
  (`reference/btrfs-progs/cmds/replace.c#cmd_replace_start`), and braid enforces
  it (covered by `tests/cli/replace-rejects-smaller-target.{nix,py}`). Only
  same-or-larger works, and same-size would not change `device_size`. So the
  feasible direction is the OPPOSITE of the brief's lead: replace onto a LARGER
  disk.
- **The replace mechanics are already proven** by
  `tests/cli/replace-larger-disk.{nix,py}` (512 -> 1024, btrfs reports the new
  ~1006 MiB size). This plan reuses that exact invocation; what is new is doing
  it on a *filled, at-risk* pool and asserting the *monitor's* reaction.

### Why a larger target keeps the pool at-risk (size math)

The threshold is `min(1 GiB, sum_of_device_sizes / 10)` -- based on the SUM, applied
to each device (`cli/src/capacity.rs#evaluate_enospc_risk`, via
`enospc_risk_threshold`). For a 2-device pool,
`margin = min over devices of (unallocated - threshold)`; at-risk is `margin < 0`.
Growing one device RAISES the shared threshold, which DEEPENS the untouched
device's deficit -- so a larger-disk replace makes the pool *more* clearly at-risk,
not less. (Sizes are approximate: a 512 MiB raw disk yields ~496 MiB of
btrfs `device_size` after the LUKS header, a 1024 MiB raw disk ~1006 MiB. The test
gates on `braid status`, not exact bytes, exactly like the parent test, so it is
robust to overhead.)

| Quantity                    | Before replace (G1) | After replace disk1->disk3 (G2) |
|-----------------------------|---------------------|---------------------------------|
| devid 1 `device_size`       | ~496 MiB            | ~1006 MiB (grown)               |
| devid 2 `device_size`       | ~496 MiB            | ~496 MiB (untouched)            |
| sum / threshold             | ~992 / ~99 MiB      | ~1502 / ~150 MiB                |
| devid 1 unallocated         | ~90 MiB (filled)    | ~600 MiB (grew, data unchanged) |
| devid 2 unallocated         | ~90 MiB (filled)    | ~90 MiB (untouched)             |
| devid 1 margin term         | ~ -9 MiB            | ~ +450 MiB (now healthy)        |
| devid 2 margin term         | ~ -9 MiB            | ~ -60 MiB (deeper deficit)      |
| pool margin = min           | ~ -9 MiB (at-risk)  | ~ -60 MiB (STILL at-risk)       |

### The crucial isolation property

After the replace, the margin worsens by only ~51 MiB (driven by the ~99 -> ~150
MiB threshold rise). That is far below `ENOSPC_WORSEN_STEP` (512 MiB,
`cli/src/capacity.rs`). So the key-match re-fire branch in
`cli/src/monitor.rs#evaluate_enospc_for_monitor` (fire iff
`margin < baseline_margin - WORSEN_STEP`) would SUPPRESS. The only path that can
fire exit 3 is the confirmed-key-mismatch branch (`device_size` of devid 1
changed). The test therefore isolates F1 cleanly: pair a "still suppressed before
the change (exit 0)" assertion with a "fired after the change (exit 3)" assertion,
and the geometry change is provably the sole cause. (A 1024 MiB target raises the
threshold by only ~51 MiB; the worsening cannot approach the 512 MiB step, so
there is no confound. This margin holds even if the size estimates are off 2-3x.)

This also isolates the `device_size` AXIS specifically: the devid set `{1,2}` and
the FS UUID are unchanged, so the only differing field in the `PoolKey` is devid
1's `device_size`.

## Recommendation: a sibling test, not an extension

Add `tests/cli/braid-monitor-enospc-geometry.{nix,py}` rather than extending
`braid-monitor-enospc`.

- **Repo convention is one focused scenario per VM test.** The replace family
  alone is six siblings (`replace-live-disk`, `replace-dead-disk`,
  `replace-larger-disk`, `replace-2disk-pool`, `replace-sequential`,
  `replace-rejects-smaller-target`); the monitor family is four
  (`braid-monitor`, `braid-monitor-enospc`, `monitor-hot-unplug`,
  `monitor-lifecycle`). A geometry-invalidation sibling fits the grain.
- **Extending entangles with the degraded subtest in two places.** The parent's
  final subtest closes a mapper by hardcoded name (`cryptsetup close braid-disk2`,
  `mount -o degraded /dev/mapper/braid-disk1`) and asserts `enospc-ack.json`
  "survives untouched". A replace changes mapper names AND the F1 discard REMOVES
  `enospc-ack.json` -- so an in-place insertion would force both a mapper-name
  rewrite and a re-ack to restore the degraded subtest's precondition. That
  coupling is fragile and muddies single-responsibility.
- **Cost of a sibling is modest:** the `.nix` is a near-copy of the parent plus
  one disk; the `.py` reuses the dd fill loop and ack; one `flake.nix` line. One
  extra VM boot is marginal against the repo's existing check suite, and aligns
  with `AGENTS.md`'s "reach for the ideal, robust, simple, most correct solution
  regardless of scope cost."

## Files and changes

### New: `tests/cli/braid-monitor-enospc-geometry.nix`

Near-copy of `tests/cli/braid-monitor-enospc.nix`. Key deltas:

- `name = "braid-monitor-enospc-geometry";`
- Keep `diskNames = [ "disk1" "disk2" ]` (the initrd fixture formats only those;
  `disk3` is left raw as the replace target -- the same arrangement
  `replace-larger-disk.nix` relies on).
- Add a third image to `virtualisation.emptyDiskImages`:
  `{ size = 1024; driveConfig.deviceExtraOpts.serial = "disk3"; }`.
- Keep the rest verbatim: the `../module/lib/initrd-fixture.nix` import, the
  `braid.monitor` block (`enable`/`beep = false`/`alertCommand`), the `pool.json`
  tmpfiles seed for disk1+disk2, the `braid-unlock` script override, `memorySize`,
  and the `btrfs-progs`/`cryptsetup`/`jq` packages.
- Write a fresh Intent/Why/Scenario preamble (see `.py` below; mirror it).

### New: `tests/cli/braid-monitor-enospc-geometry.py`

Preamble (Intent / Why it exists / Scenario) per `docs/dev/testing.md`, then:

```
import json
start_all(); machine.wait_for_unit("multi-user.target", timeout=120)
passphrase = "testpassphrase"

def replace_cmd(old, new):
    # Fast pbkdf so the VM's LUKS format of the new disk stays quick (mirrors
    # replace-larger-disk.py).
    return (f"printf '%s\\n' '{passphrase}' | "
            f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
            f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
            f"--old {old} --new {new}=/dev/disk/by-id/virtio-{new} --passphrase-stdin --yes")
```

Subtests and assertions:

1. **Stop the monitor timer** -- `systemctl stop braid-monitor.timer` (deterministic
   manual driving, as the parent does).
2. **Unlock the pool** -- `systemctl start braid-pool.target`; assert
   `mountpoint -q /mnt/storage`.
3. **Fill below the ENOSPC threshold** -- reuse the parent's loop verbatim
   (`for i in range(14): dd ... bs=1M count=50; sync; break when "ENOSPC risk" in
   braid status`); assert `"ENOSPC risk" in braid status`.
4. **At-risk monitor latches EnospcRisk** -- `braid monitor` exits `3`; assert
   `test -f /var/lib/braid/alert-latch.json`. This step is REQUIRED before ack:
   `cli/src/ack.rs#cmd_ack_impl` writes the baseline only when the latch carries
   an `EnospcRisk` cause -- on an empty latch it prints "no active alerts" and
   writes nothing, so acking a never-monitored pool would leave no
   `enospc-ack.json` and the suppression step below would fire armed instead of
   exiting 0. Mirrors the parent's monitor-then-ack ordering.
5. **Ack writes the keyed baseline (geometry G1)** -- `braid ack`; assert
   `machine.fail("test -f /var/lib/braid/alert-latch.json")` (ack cleared the
   latch) and `test -f /var/lib/braid/enospc-ack.json` (baseline written).
6. **Acked-but-unchanged pool stays suppressed** -- `braid monitor` exits `0`;
   assert no `alert-latch.json`. (Establishes the suppression is live; with the
   margin unchanged, exit 0 is the only correct result.)
7. **`braid replace disk1 -> disk3`** -- FIRST record the pre-replace FS UUID,
   disk1's devid, and disk1's reported size (from `btrfs filesystem show
   /mnt/storage` / `btrfs device usage --raw`; reuse the
   `get_device_size_mib(mapper)` helper pattern from `replace-larger-disk.py`).
   Then `machine.succeed(replace_cmd("disk1", "disk3"))`; assert
   `/dev/mapper/braid-disk3` in `btrfs fi show /mnt/storage`.
8. **Replace changed ONLY the device_size axis** -- assert the FS UUID is
   unchanged, `braid-disk3` carries disk1's old devid (btrfs replace preserves
   the source devid), and disk3's reported size is significantly larger than
   disk1's old size (ratio > 1.5, as `replace-larger-disk.py` checks). This pins
   the plan's central claim: the only differing `PoolKey` field is devid 1's
   `device_size`, not the devid set or FS UUID -- so the discard in step 10
   exercises the same-devid geometry axis, not a devid-set or identity change.
9. **Pool is STILL at-risk** -- assert `"ENOSPC risk" in braid status` (the
   threshold rose; disk2 stays below it).
10. **Geometry change discards the baseline and re-fires** -- `braid monitor`
   exits `3`; assert `machine.fail("test -f /var/lib/braid/enospc-ack.json")`
   (confirmed mismatch removed it); assert `test -f .../alert-latch.json`; parse
   `braid status --json` and assert `"enospc_risk" in [c["type"] for c in
   report["alert_causes"]]`.

Subtests 6 + 10 together are the test's spine: identical pool, only the geometry
differs, suppression flips to firing. Because the worsening is below the worsen
step, exit 3 can ONLY come from the key-mismatch discard; subtest 8 pins that the
mismatch is on the `device_size` axis (same devid, same FS UUID).

Do NOT assert on the monitor's stderr wording ("pool key no longer matches");
that is structure-sensitive. The exit code + file-state + cause-type assertions
are behavioral and sufficient.

### `flake.nix` -- register the check

After the `braid-monitor-enospc` block (around `flake.nix` line 994), mirror the
pattern:

```
braid-monitor-enospc-geometry = pkgs.testers.nixosTest (
  import ./tests/cli/braid-monitor-enospc-geometry.nix {
    braid = linuxCrane.braid;
  }
);
```

The name has no `repro-` prefix, so it lands in `checks` (run by `just check`).

### Side fix: the stale preamble in `tests/cli/braid-monitor-enospc.{nix,py}`

The parent `.py` preamble currently claims coverage the body does not provide:
"a `braid add` topology change invalidates the baseline so a still-at-risk pool
re-fires" and "disk3 held raw for the add subtest" -- but there is no add subtest
and the `.nix` has only two disks. (The body's own NOTE already concedes F1 is
not driven e2e because `braid add` ENOSPCs/relieves.) As part of this change:

- Rewrite the `.py` preamble to describe only what that test does, and
  cross-reference `braid-monitor-enospc-geometry` for the geometry/F1 e2e.
- Update the in-body NOTE to say the `device_size` axis is now exercised e2e in
  the sibling via `braid replace`, while `braid add` remains unusable as a vehicle.
- Bring the `.nix` preamble's See/cross-reference in line.

No behavioral change to the parent test; preamble/comment only.

## Verification

- **Run the new check** (checks build on the linux-builder; attr system is
  `aarch64-darwin` per `docs/dev/testing.md`):
  `nix build .#checks.aarch64-darwin.braid-monitor-enospc-geometry -L`
  Expect green: subtest 6 exit 0, subtest 10 exit 3 with `enospc-ack.json` gone
  and a fresh latch carrying `enospc_risk`.
- **TDD teeth-check (prove the test pins the seam, not a tautology).** The F1
  behavior already exists, so the test should pass immediately; to confirm it has
  teeth, temporarily weaken `cli/src/alert.rs#live_pool_key` to zero the
  `device_size` in each pair (key on devid only). Re-run: the new check must go
  RED -- subtest 10 now exits 0, because the live key matches the baseline on
  devid + FS UUID and the ~51 MiB worsening is below the worsen step, so the
  stale baseline wrongly suppresses. Revert the weakening.

  This mutation is NOT uniquely caught by the new VM, and the recipe does not
  claim it is: the unit test `cli/src/alert.rs#live_pool_key_requires_fs_uuid_and_sorts`
  asserts the exact `device_size` value in the key, so it also goes RED (defense
  in depth). (The hand-built
  `cli/src/monitor.rs#cmd_monitor_stale_baseline_key_mismatch_fires_and_clears`
  does NOT fail under it -- its stale keys differ from a zeroed live key
  regardless -- which is precisely why a hand-built key cannot stand in for the
  real flow.) The teeth-check only confirms the new VM is sensitive to the real
  `device_size` path end to end. The failure modes the VM alone guards -- a probe
  that caches geometry, or parser drift on real post-replace output -- are the
  ones the stub/fixture tests cannot see, and they do not reduce to a one-line
  mutation.
- **Regression-suite sanity:** `just test-rust` (unit) and
  `nix build .#checks.aarch64-darwin.braid-monitor-enospc -L` (parent still green
  after the preamble edit).
- **Confirm the replace truly succeeds on a full pool** (the one residual risk):
  the first real run of subtest 7 settles it. If `braid replace` were to ENOSPC
  here, see the fallback below.

## Alternative considered: don't build it

What is already covered: the discard logic on all three axes (the unit test); the
`device_size` parser extraction (`parse_btrfs_device_usage` fixtures);
`live_pool_key` construction (unit-tested); the full monitor lifecycle e2e (the
parent). The only uncovered seam is the integration: that a real btrfs op moves
the reported `device_size` and braid's probe/parser/key path picks it up to trip
F1.

Honest case against building: that seam is low-probability and partly guarded
elsewhere (parser drift is caught by the fixture-refresh discipline in
`docs/dev/parser-compatibility.md`; `replace-larger-disk` already proves the
device grows in `btrfs fi show`). One could argue the composition is "very likely
correct" and skip the VM cost.

Why I still recommend building: it is the ONLY test connecting a real `braid
replace` to the F1 firing it was designed to trigger (`cli/src/alert.rs#PoolKey`
justifies `device_size`'s existence by exactly this scenario). A regression that
silently broke the flow -- a probe that cached geometry, a parser drift on
post-replace output, or someone "simplifying" the key to devid-only -- would pass
every existing test yet leave a stale baseline suppressing a genuinely-changed,
still-at-risk pool. The cost is low and the gap is specific and named.

What would change the calculus toward not building: if the first run shows
`braid replace` ENOSPCs on the at-risk pool (judged unlikely -- a live replace
does no balance, and an at-risk pool still has ~50-90 MiB unallocated per device,
ample for any incidental system chunk). In that case the only remaining vehicle is
the raw-resize fallback, which is a strictly weaker, non-braid trigger -- at which
point relying on the unit + fixture coverage becomes the more defensible call.

### Fallback vehicle (only if `braid replace` cannot stay at-risk)

A raw, in-place `btrfs filesystem resize 1:<smaller> /mnt/storage` that shaves a
few MiB off devid 1's top (where no chunks are allocated) changes `device_size`
with no new device and no rebalance, so the probe -> key -> discard wiring still
fires. It is NOT a braid subcommand (braid exposes no resize), so it validates
only "the probe sees a geometry change," not "`braid replace` trips F1" -- it does
not exercise the design rationale. Use it only as a degraded fallback, and say so
in the preamble.

## Commit (when implemented)

`test(monitor): cover enospc baseline invalidation across a braid replace geometry change`

## Implementation notes

- The new VM uses a skewed 3-member fixture (4096 MiB, 512 MiB, 4096 MiB) with
  an 8192 MiB replacement instead of the planned 2-member filled pool. Real VM
  runs showed 2-member at-risk pools starved live `braid replace` even after
  scaling from 512 MiB to 1024 MiB source disks. The skewed member keeps the
  pool at risk without exhausting the large source disk's replacement workspace.
  A raw `btrfs filesystem resize` fallback changed `btrfs fi show`, but not the
  `btrfs device usage --raw` `Device size` field that `PoolKey` consumes, so it
  would not test F1.
