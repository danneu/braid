# Plan: close TOCTOU gap in `add`'s ClosedPresentLuks Pass-1 path

## Context

The deferred ClosedPresentLuks branch of `braid add` journals the planning-time
LUKS UUID without re-probing the live device before journal write. At
`cli/src/add.rs:1853` the planner caches `ClosedPresentLuksCandidate.luks_uuid`
from the probe-time `ConfigDiskState::PresentLuks.uuid`. At execute time
(`cli/src/add.rs:892-956`), Pass 1 calls `ensure_luks_open` -- which opens
whatever LUKS volume is currently at `target.by_id` and only compares the
mapper's underlying-device UUID against the *live* by-id UUID, never against
the cached planning-time UUID. `classify_braid_disk_fsid` then checks btrfs
FSID against the mounted pool but does not probe LUKS UUID. The journal is
then written keyed by the planning-time UUID, even if the disk at `by_id` was
swapped between plan and execute to a different LUKS container that happens to
share the passphrase and an FSID (e.g. block-cloned filesystem on a fresh LUKS
header).

Decision doc `docs/decisions/024-luks-uuid-identity.md` mandates "verifies live
UUIDs again at mutation boundaries where a physical disk could have been
swapped or reformatted." The proven open-boundary defense already exists in
`cli/src/replace.rs:721` -- `probe_existing_luks_new_target_uuid` runs *before*
`ensure_luks_open` and rejects the swap with the structured
`ReplaceError::NewTargetUuidMismatchAtOpen { by_id, expected, observed }`. The
recovery-side analog lives at `cli/src/recover.rs:2128-2134`. Add's primary
path is the lone surviving omission against this invariant.

Intended outcome: add an execute-time live-UUID re-probe in the
ClosedPresentLuks branch, mirroring the replace.rs pattern, so a plan-to-execute
disk swap fails closed before braid opens a foreign LUKS volume or writes
`pending-op.json`. Pin the gate with a VM test sibling to
`tests/cli/recover-replace-existing-luks-uuid-mismatch.py`.

## Approach

Mirror `replace.rs::probe_existing_luks_new_target_uuid` as a file-private
helper in `add.rs`. Add a structured `AddError::TargetUuidMismatchAtOpen`
variant matching the replace shape so canonical wording stays identical across
commands and tests can pattern-match the substring "LUKS UUID mismatch" the
same way recovery and unlock tests do today.

Rejected alternatives (briefly): extracting a shared helper to `luks.rs` --
deferred until a third caller appears; with two callers and per-caller
diagnostic strings ("probe failed: {e}", "probe parse failed: {e}") the
abstraction cost (typed `LiveLuksUuidProbeError`, per-callsite `From` impls)
outweighs the duplication. The existing `probe_mapper_uuid.rs` helper is the
close-boundary warn-and-skip variant (different return type, different
semantics) and is not a substitute.

## Changes

### 1. `cli/src/add.rs`

**Add a structured `AddError` variant** (next to `DuplicateUuid` near line 60):

```rust
#[error(
    "add target '{by_id}' LUKS UUID mismatch: expected {expected}, found {observed} -- detach the foreign disk and retry"
)]
TargetUuidMismatchAtOpen {
    by_id: ByIdPath,
    expected: LuksUuid,
    observed: String,
},
```

The doc comment should follow the pattern at `add.rs:51-57` (DuplicateUuid):
explain why the variant exists (open-boundary defense for plan-to-execute
swap), where it fires (before `ensure_luks_open` in Pass 1), and that the
wording mirrors `ReplaceError::NewTargetUuidMismatchAtOpen` so operator
remediation reads identically across commands.

**Add a file-private helper** modeled byte-for-byte on
`replace.rs:913-940`:

```rust
fn probe_closed_present_luks_target_uuid<R: CommandRunner>(
    runner: &R,
    by_id: &ByIdPath,
    expected: &LuksUuid,
) -> Result<(), AddError> {
    // probe failure -> Err(TargetUuidMismatchAtOpen { observed: "probe failed: {e}" })
    // parse failure -> Err(TargetUuidMismatchAtOpen { observed: "probe parse failed: {e}" })
    // mismatch     -> Err(TargetUuidMismatchAtOpen { observed: parsed.uuid.as_str().to_owned() })
    // match        -> Ok(())
}
```

A `///` doc comment is required (per AGENTS.md). One-to-three lines covering
intent: open-boundary re-probe for ClosedPresentLuks, mirrors the replace-side
gate, fires before `ensure_luks_open` so braid never opens a foreign LUKS
volume at this by-id.

**Wire the helper into Pass 1** in the existing for-loop at `add.rs:892-956`.
Insertion point: between the loop's `let AddTargetWork::ClosedPresentLuks(...)
= target else { continue; };` and the first `emit_status("disk {}:
unlocking...")` line (around `add.rs:896`). Placement before the "unlocking"
status line avoids an orphaned `[wait]` line followed by an immediate error.

```rust
for target in &sorted_targets {
    let AddTargetWork::ClosedPresentLuks(target) = target else {
        continue;
    };
    probe_closed_present_luks_target_uuid(
        runner,
        &target.by_id,
        &target.luks_uuid,
    )?;
    emit_status(&status_line(
        StatusTag::Wait,
        color_enabled,
        &format!("disk {}: unlocking...", target.name),
    ));
    // ... existing ensure_luks_open + classify_braid_disk_fsid as today
}
```

Safety: `LuksCleanupGuard::new` at `add.rs:866` allocates an empty `mappers`
vec; an early `?`-return from the new helper before the first
`luks_guard.track(...)` runs `Drop` over an empty vec (no-op). On later loop
iterations the guard correctly tears down anything tracked so far -- behavior
is identical to today's `?`-from-`ensure_luks_open` propagation path.

**Update the misleading planner comment** at `add.rs:1832-1838`. Replace:

```rust
// Mapper closed -- FSID verification deferred to execution time.
// The deferred path stores the probed UUID; the
// pre-write uniqueness assert covers
// ClosedPresentLuks via the same gate that runs
// when the target promotes to a
// RecoverableBraidTarget at execute time.
```

with a two-tier description:

```rust
// Mapper closed -- FSID verification deferred to execution time.
// Two-tier defense for the cached `uuid`:
//   (a) plan-time `assert_target_uuid_unique` below rejects UUID
//       collisions against in-flight targets, pool membership, and
//       the live pool.
//   (b) execute-time live-UUID re-probe before `ensure_luks_open`
//       (see Pass-1 loop) rejects plan-to-execute disk swaps so a
//       foreign disk at this by-id cannot pass through to
//       `btrfs device add`.
```

### 2. VM test: `tests/cli/braid-add-uuid-swap-rejected.py` + `.nix`

The test must reproduce the **between-plan-and-execute** swap, not a
before-plan swap. Planning runs synchronously inside the same `braid add`
invocation as execute. To open a TOCTOU window the test must pause braid
between the planner (which probes and caches U1) and Pass 1 (where the new
gate fires), perform the swap, then resume.

The natural pause point is the `Type 'yes' to continue:` confirmation
prompt at `cli/src/add.rs:813` (only emitted when `--yes` is absent). The
test invokes `braid add` *without* `--yes`, polls the output stream for the
prompt, performs the reformat, then writes `yes\n` plus the passphrase to
braid's stdin via a fifo.

The setup pattern is the returned-disk scenario from
`tests/cli/add-returned-disk-after-remove-missing.py:62`: it produces a
closed, braid-labeled disk absent from `pool.json` with a mounted pool --
exactly the state that drives the planner into the ClosedPresentLuks branch.

The `.nix` follows `tests/cli/recover-replace-existing-luks-uuid-mismatch.nix`
(4 emptyDiskImages with `serial=disk1..disk4` -- match the returned-disk test's
3-disk layout, with `cryptsetup` and `btrfs-progs` in `systemPackages`). The
check must be registered in `flake.nix` alongside the other `braid-add-*`
checks (pattern at `flake.nix:116-132`).

Preamble (intent / why / scenario) per AGENTS.md Test Conventions and the
existing siblings.

Scenario:

```
Phase 0: build 3-disk RAID1 pool (disk1, disk2, disk3) using the
         same add_cmd helper as add-returned-disk-after-remove-missing.py
         (lines 35-40 -- pbkdf2 + 1000 iterations for speed).

Phase 1: take disk3 missing -- umount /mnt/storage, cryptsetup close
         braid-disk3, mount -o degraded, then
         `braid remove-missing --missing-id <devid> --yes`.
         Verify disk3 is absent from pool.json and the pool is mounted.
         Capture U1 = cryptsetup luksUUID /dev/disk/by-id/virtio-disk3.
         Snapshot pool.json bytes for an "unchanged" assertion later.

Phase 2: stage interactive add via fifo. Drop a /tmp/swap.sh on the
         machine that:
           - mkfifo /tmp/braid-in
           - launches `braid add ... disk3=/dev/disk/by-id/virtio-disk3
             --passphrase-stdin` (no --yes) in the background, reading
             stdin from the fifo, writing combined stdout+stderr to
             /tmp/braid-out, recording its exit code to /tmp/braid-exit
           - holds the fifo open with `exec 3>/tmp/braid-in` so the
             writer side does not race against EOF
           - polls /tmp/braid-out for the substring
             "Type 'yes' to continue" (bounded retry, ~30s ceiling)
           - on prompt detection, runs:
                 printf '%s' "$PASS" | cryptsetup luksFormat \
                   --batch-mode --label=braid-disk3 --key-file=- \
                   --pbkdf pbkdf2 --pbkdf-force-iterations 1000 \
                   /dev/disk/by-id/virtio-disk3
             so the swapped LUKS volume carries (a) the same braid-disk3
             label (passes validate_braid_preconditions at add.rs:118),
             (b) the same passphrase (passes verify_credential_for_targets
             at add.rs:828), (c) a fresh UUID U2, and (d) no btrfs inside
             (does not matter -- the new gate fires before
             classify_braid_disk_fsid).
           - writes `yes\n` and the passphrase to the fifo, closes the
             writer fd, and `wait`s on braid.
         The python test invokes /tmp/swap.sh, reads U2 with
         `cryptsetup luksUUID`, and asserts U2 != U1.

Phase 3: assert the failure mode.
           - /tmp/braid-exit holds a non-zero integer.
           - /tmp/braid-out contains all of: "add target",
             "LUKS UUID mismatch", "expected <U1>", "found <U2>",
             "detach the foreign disk".
           - test -e /var/lib/braid/pending-op.json fails (the gate
             fires before journal write).
           - pool.json bytes are identical to the Phase-1 snapshot
             (disk3 still absent from membership, ordering and
             whitespace untouched).
           - `cryptsetup status braid-disk3` reports inactive (the
             gate fires before ensure_luks_open, so no mapper was
             opened; LuksCleanupGuard never tracked anything).
```

Regression value: without the new gate, the same scenario would proceed
past `ensure_luks_open`, succeed on the passphrase (verify_credential plus
ensure_luks_open both pass because the swapped disk uses the same
passphrase), then fail downstream at `classify_braid_disk_fsid` with the
`BraidLabeledNoBtrfs` "no btrfs superblock" wording -- not the canonical
"LUKS UUID mismatch" wording. The test's substring assertions on the gate's
exact phrasing therefore fail closed if the gate is removed or moved after
`ensure_luks_open`.

## Critical files

- `cli/src/add.rs` -- new variant, new helper, wire-in, comment update
- `tests/cli/braid-add-uuid-swap-rejected.py` -- new
- `tests/cli/braid-add-uuid-swap-rejected.nix` -- new
- `flake.nix` -- register the new check

## Existing code reused

- `replace.rs::probe_existing_luks_new_target_uuid` at `cli/src/replace.rs:913-940` -- reference template for the new helper
- `ReplaceError::NewTargetUuidMismatchAtOpen` at `cli/src/replace.rs:79-86` -- reference template for the new AddError variant (same struct shape, same wording with "replace target" -> "add target")
- `tests/cli/recover-replace-existing-luks-uuid-mismatch.py` -- reference template for the VM test (reformat-in-place idiom at lines 86-97, exit + stderr + state assertions at lines 149-174)
- `tests/cli/recover-replace-existing-luks-uuid-mismatch.nix` -- reference template for the test `.nix`
- `flake.nix:116-132` -- pattern for registering the new check
- `LuksCleanupGuard` at `cli/src/add.rs:216-238` -- already safe for early return before any `track()`, no change needed

## Out of scope

- `OpenRecoverable` path: mapper is kernel-pinned from plan to execute, so the
  TOCTOU window is closed by the kernel reference; no probe needed.
- `Fresh` path: `cryptsetup luksFormat --uuid <journaled_uuid>` writes the
  authoritative UUID; no probe needed.
- Recovery-side gate at `cli/src/recover.rs:2128-2134`: unchanged. It remains
  the second line of defense for crash-then-swap, complementing the new
  add-side gate which closes plan-then-swap within a single invocation.
- Extracting a shared helper into `luks.rs`: defer until a third caller (e.g.
  a future `unlock`-side open-boundary gate) appears.

## Verification

1. `just test-rust` -- new helper has no unit tests beyond what AddError's
   `thiserror` formatting test (if any) covers; existing add unit tests must
   still pass. Run the full crate to catch any incidental breakage from the
   new variant in `match` arms.
2. `just test-vm braid-add-uuid-swap-rejected` -- new VM test must pass.
3. `just test-vm braid-add-disk braid-add-during-balance braid-add-enroll braid-add-persists-before-balance braid-add-warnings` -- the existing add-side tests must still pass (the gate is path-specific to ClosedPresentLuks reformat; none of these scenarios reformat).
4. `just test-vm recover-replace-existing-luks-uuid-mismatch unlock-uuid-mismatch` -- regression check that the canonical "LUKS UUID mismatch" wording was not perturbed elsewhere.
5. End-to-end check on a live pool. The reformat must land between plan and
   execute, so run interactively:
   - Get a closed, braid-labeled disk absent from `pool.json` (e.g. via the
     returned-disk path used in `add-returned-disk-after-remove-missing.py`).
   - Capture U1 = `cryptsetup luksUUID <by-id>`.
   - In one terminal, start `braid add <name>=<by-id> --passphrase-stdin`
     (no `--yes`) and wait at the `Type 'yes' to continue:` prompt.
   - In another terminal, reformat the same `<by-id>` with the same
     passphrase and `--label=braid-<name>` to produce U2.
   - Return to the first terminal, type `yes`, press enter, then type the
     passphrase.
   - Confirm braid prints `add target '<by-id>' LUKS UUID mismatch: expected
     <U1>, found <U2> -- detach the foreign disk and retry`, exits non-zero,
     wrote no `pending-op.json`, did not open the `braid-<name>` mapper, and
     left `pool.json` byte-identical to its pre-attempt state.
