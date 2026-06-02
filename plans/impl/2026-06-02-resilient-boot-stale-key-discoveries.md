# Plan: fix stale "Key discoveries" in 003-resilient-boot.md

## Context

`docs/design/decisions/003-resilient-boot.md` is the ADR for "Resilient by
default." Its "Key discoveries" section (lines 45-53) has two subsections, and
both are stale residue from an **abandoned** architecture. Git history (commit
`adc2b5f4`) shows braid once unlocked LUKS at boot via crypttab/initrd with
`crypttabExtraOpts = [ "nofail" "x-systemd.device-timeout=10s/30s" ]` on each
data drive. That whole mechanism was replaced by CLI-driven stage-2 unlock
(`braid unlock` issues a direct `mount` call). The two subsections were never
updated:

- **"Timeout values"** (lines 51-53) -- the original finding. Claims braid waits
  10s (VM) / 30s (production) for drives to enumerate before unlock. No such wait
  exists: `cli/src/probe.rs` has zero timeout/retry/sleep logic and `braid unlock`
  probes configured members immediately. The value was a config knob of the
  deleted crypttab mechanism. The only real nearby timeout is the USB-key
  `autoUnlock.timeoutSec` (default **5s**, `modules/braid/options.nix:64-68`),
  which is unrelated to data-drive enumeration. **No forward value** -- it is a
  dead parameter, not an insight.

- **"udev SYSTEMD_READY=0 risk"** (lines 47-49) -- describes a udev quirk that can
  "block mount," says it's "Not yet hit in testing," and offers a fallback of
  "moving mount into the scan service script." The quirk itself is a *real,
  durable* upstream fact (systemd/systemd#36886), but its framing is stale:
  - The "block mount" failure only affects **systemd-initiated** mounts.
    `SYSTEMD_READY=0` gates systemd `.device`/`.mount` unit activation
    (`reference/systemd/man/systemd.device.xml`); it cannot block braid's direct
    `mount` syscall (`cli/src/mount.rs:805-827`).
  - The "scan service script" fallback **no longer exists** (no
    `btrfs-device-scan` service anywhere in `modules/`).
  - The repro tests `tests/repro/udev-missing-disk-{io,idle}.py` show the pool
    stays mounted through a member disappearing; the failure never occurs.

The udev quirk is actually *design rationale for this ADR*: "a missing drive
blocks the mount" is the exact failure "Resilient by default" must prevent, and
avoiding systemd mount units is partly *why* braid mounts from the CLI. So the
right move is **keep the knowledge, fix the framing** -- not delete it.

Intended outcome: the ADR contains no fictional values, and the durable udev
rationale survives as accurate, present-tense design justification.

## Scope

Single file: `docs/design/decisions/003-resilient-boot.md`. No code changes.

Pre-verified: no doc links to the section anchors (`#key-discoveries`,
`#timeout-values`, `#udev-...`), so deleting the section breaks no cross-link
(mdbook-linkcheck2 stays green). The genuine udev/btrfs missing-member knowledge
is independently preserved in `tests/repro/udev-missing-disk-*.py` and
`docs/internals/real-world/sata-hot-unplug.md`.

## Changes

### 1. Fold the udev rationale into the Implementation section

Note: the user already hand-edited this bullet to fix the overbroad `fileSystems`
claim. It is now titled **No boot-blocking mount units**, qualified to "data-pool
`fileSystems`", and carries a USB-key parenthetical (line 26 was likewise
qualified to "data-pool `fileSystems` entries"). Preserve all of that -- only
append the udev rationale.

Current bullet (doc line 30):

> - **No boot-blocking mount units**: The module generates no data-pool `fileSystems` or LUKS entries. The CLI (`braid unlock`) handles LUKS open + btrfs mount directly. Nothing referencing data drives can block boot. (The one build-time `fileSystems` entry is the optional `autoUnlock` USB-key mount at `/run/braid-key/mnt`, marked `noauto`/`nofail` so it never blocks boot and references the key device, not the pool.)

Replace with (data-pool qualification + USB-key parenthetical kept verbatim; udev
rationale and an accurately-scoped test pointer appended):

> - **No boot-blocking mount units**: The module generates no data-pool `fileSystems` or LUKS entries. The CLI (`braid unlock`) opens LUKS and mounts btrfs directly with a plain `mount` call, so nothing referencing data drives can block boot. Mounting outside systemd also sidesteps the `SYSTEMD_READY=0` udev quirk (systemd/systemd#36886): a missing btrfs member can mark surviving devices not-ready and stall a *systemd*-initiated mount — the exact failure resilience-by-default exists to prevent. Related coverage: `tests/repro/udev-missing-disk-{io,idle}.py` exercise udev events when a member disappears from an already-mounted pool, characterizing disappearance signals rather than the `SYSTEMD_READY=0` mount-gating path. (The one build-time `fileSystems` entry is the optional `autoUnlock` USB-key mount at `/run/braid-key/mnt`, marked `noauto`/`nofail` so it never blocks boot and references the key device, not the pool.)

Wording notes:
- The test pointer is deliberately scoped as *related* coverage. Those repro
  tests assert udev `ACTION=remove` events on an already-mounted pool after a
  hot-unplug; they do **not** exercise the `SYSTEMD_READY=0` mount-gating path,
  so the citation must not imply they prove the quirk.
- Match the file's existing em-dash style for new prose -- this file already uses
  `—` throughout, which is the documented ASCII-preference exception. Do not
  convert the file's existing em-dashes, the user's "data-pool" qualification, or
  the USB-key parenthetical.

### 2. Delete the entire "Key discoveries" section

Remove the heading plus both subsections (current lines 45-53):

```
## Key discoveries

### udev SYSTEMD_READY=0 risk

When btrfs has a missing member, udev may mark remaining devices as not ready (systemd/systemd#36886), blocking mount. Not yet hit in testing. Fallback: custom udev rule or moving mount into the scan service script.

### Timeout values

10s in VM tests (no spin-up delay). 30s in production (real drives may be slow to enumerate on a cold DAS).
```

After removal, the "### Identity enforcement" subsection is followed directly by
"## Constraint" -- ensure exactly one blank line between them.

## Out of scope (noted, not done)

- **Cold-drive spin-up wait.** The only arguably-durable idea in "Timeout values"
  is "real drives can be slow to spin up; VMs aren't." braid's code does not act
  on this (no enumeration wait). If it is a genuine operational concern, it is a
  *feature decision* ("should `braid unlock` wait for slow cold drives before
  probing?") deserving its own ADR -- not a fossilized number. Flag separately;
  do not preserve as doc text.
- **`archive/plans/test-boot-degraded.md` reference** (See section, ~line 63).
  Accurate as-is: the file was intentionally removed from the tree (commit
  `13ab4653`) and the doc correctly says it is "preserved in git history; last
  present at commit `9df91f9`." Leave unchanged.

## Verification

1. `mdbook build docs` -- must succeed (mdbook-linkcheck2 confirms no broken
   cross-links from the deletion).
2. `rg -n "Timeout values|cold DAS|Not yet hit in testing|scan service" docs/design/decisions/003-resilient-boot.md`
   -- expect no matches (stale framing gone).
3. `rg -n "SYSTEMD_READY=0|systemd/systemd#36886" docs/design/decisions/003-resilient-boot.md`
   -- expect exactly one match each, inside the rewritten Implementation bullet
   (knowledge retained, correctly framed). After deleting Key discoveries, the
   old line-49 occurrence is gone, so the only remaining hit is the new bullet.
4. `rg -n "data-pool .fileSystems|USB-key mount at .run/braid-key/mnt" docs/design/decisions/003-resilient-boot.md`
   -- expect the user's data-pool qualification (lines 26 + 30) and the USB-key
   parenthetical to survive unchanged (guard against clobbering the hand-edits).
5. Read-through: confirm "## Key discoveries" no longer exists and the
   Implementation -> Constraint -> See flow reads cleanly.

No VM/Rust tests needed -- documentation-only change with no code or module edits.

## Follow Up

- Decide separately whether `braid unlock` should wait for slow cold drives before probing; the removed ADR timeout values in `docs/design/decisions/003-resilient-boot.md` were stale residue, not current behavior.
