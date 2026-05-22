# Plan: pivot -- strengthen positional chain in both unlock dry-run render tests

## Context

A review finding flagged `plan_unlock_dry_run_render_2_closed_disks_with_key_file`
(`cli/src/unlock.rs:847-893`) as having no positional ordering assertions:
it only does `contains` / `!contains` substring checks on the rendered
dry-run preview. The finding proposed strengthening that one test with
the chain `probe notes < LUKS opens < scan < mount`.

The pivot: the same gap exists, less obviously, in the *first* sibling
test `plan_unlock_dry_run_render_2_closed_disks`
(`cli/src/unlock.rs:780-839`). Its positional chain is only
`pos_note1 < pos_note2 < pos_scan` -- it pins notes-before-scan but
leaves LUKS-open positions and post-scan mount ordering unverified, then
falls back to `contains` checks that would still pass if the planner
moved scan ahead of the LUKS opens or moved mount above scan.

Both tests claim in their preamble to defend the
"probe-context-before-steps contract" (ADR 022:
`docs/decisions/022-dry-run-preview-model.md:48` -- "Notes render first,
then steps"). The inter-section invariant ("notes before steps") is
already pinned at the renderer in `cli/src/preview.rs:326-339`. What the
per-command unlock tests uniquely guard is the intra-step-block ordering
that `compile_open_steps` produces. That is exactly what is currently
under-asserted. Strengthening only the keyfile sibling would leave the
passphrase-stdin sibling weaker than its preamble claims, so both tests
need the same chain.

Outcome: both unlock dry-run render tests pin the full positional chain
`probe note disk1 < probe note disk2 < LUKS open disk1 < LUKS open disk2
< btrfs device scan < mount -> /mnt/storage`. A planner regression that
reorders any of those steps -- in either the passphrase-stdin or
key-file mode -- fails the right test with a clear chain-violation
panic.

## Critical files

- `cli/src/unlock.rs:780-839` -- `plan_unlock_dry_run_render_2_closed_disks`
- `cli/src/unlock.rs:847-893` -- `plan_unlock_dry_run_render_2_closed_disks_with_key_file`

No other files change. ADR 022 already documents the contract; the
preview-renderer unit tests in `cli/src/preview.rs` already pin the
inter-section invariant. Nothing in `cli/src/mount.rs` (which builds the
open / scan / mount Step structs) needs to change -- the rendered titles
the assertions target (`LUKS open <by-id>`, `btrfs device scan`,
`mount -> /mnt/storage`) are already what `compile_open_steps`,
`build_scan_step`, and the mount-step builder emit.

## Step shapes the assertions target

Confirmed from `cli/src/mount.rs` and the `Step` rendering in `cli/src/cmd.rs:400`:

- LUKS open (passphrase-stdin or keyfile): title line includes the
  substring `LUKS open /dev/disk/by-id/virtio-diskN` (followed by
  ` -> braid-diskN`). Same title for both modes; only the `$ <argv>`
  line beneath differs.
- btrfs device scan: title line includes the substring
  `btrfs device scan`.
- Mount (healthy, non-degraded): title line includes the substring
  `mount -> /mnt/storage`. (Both tests use the healthy / non-degraded
  fixture, so the degraded variant `mount -> /mnt/storage (degraded)` is
  not in scope.)
- Probe notes: rendered exactly as `[ok]   disk diskN: found\n` (already
  matched by the existing test 1).

## Change pattern (applied to both tests)

Inside each test, after the existing
`let rendered = plan_unlock(&runner, &fs, &params).expect(...).preview().render();`
line, replace the per-needle `find().unwrap_or_else(...)` repetition
with a single local `pos` closure and a single chain assertion:

```rust
let pos = |needle: &str| {
    rendered
        .find(needle)
        .unwrap_or_else(|| panic!("expected {needle:?} in render, got: {rendered:?}"))
};

let p_note1  = pos("[ok]   disk disk1: found\n");
let p_note2  = pos("[ok]   disk disk2: found\n");
let p_open1  = pos("LUKS open /dev/disk/by-id/virtio-disk1");
let p_open2  = pos("LUKS open /dev/disk/by-id/virtio-disk2");
let p_scan   = pos("btrfs device scan");
let p_mount  = pos("mount -> /mnt/storage");

assert!(
    p_note1 < p_note2
        && p_note2 < p_open1
        && p_open1 < p_open2
        && p_open2 < p_scan
        && p_scan  < p_mount,
    "preview chain notes < opens < scan < mount must hold; got: {rendered:?}",
);
```

Per-test specifics on top of the shared chain:

- **`plan_unlock_dry_run_render_2_closed_disks`**: the chain block above
  fully replaces the current `pos1 / pos2 / scan_pos` block AND the
  three trailing `rendered.contains("LUKS open ...")` / `"mount"`
  asserts -- every substring those checks named is now in the chain.
  Update the intent line of the preamble to mention the full chain
  rather than "the block still carries the expected open, scan, and
  mount steps".
- **`plan_unlock_dry_run_render_2_closed_disks_with_key_file`**: insert
  the chain block immediately after the `rendered` binding. Keep the
  two existing keyfile-specific assertions:
  1. `rendered.contains("cryptsetup open --type luks --key-file /run/keys/braid.key --keyfile-size 4096")`
  2. `!rendered.contains("--key-file=-")`
  These pin the `$ <argv>` line beneath the LUKS open step and are
  the only thing distinguishing this test from its sibling. Update the
  intent line of the preamble to mention the chain.

## Design notes

- **No shared helper.** Two call sites and one local 6-line closure each
  is below the abstraction threshold ("three similar lines is better
  than a premature abstraction"). If a third sibling later covers a
  passphrase-file mode, factor out a `assert_unlock_preview_chain`
  helper at that point. Until then, inline duplication is clearer.
- **Pinning disk1 < disk2 between opens is intentional.** The existing
  test 1 already pins disk1 < disk2 on the probe notes, which couples
  to the planner's iteration order. Extending the same coupling to the
  opens preserves consistency. If a future refactor parallelises the
  opens, both tests break together with a clear chain panic, which is
  the desired signal -- not a brittle false positive.
- **Per-step title substrings (not full lines)** keep the assertions
  resilient to changes in the risk-tag column width or the mapper-name
  suffix (` -> braid-diskN`) while still uniquely identifying each
  step.

## Verification

1. `just test-rust` -- runs all Rust unit tests, including the two
   modified tests in the unlock module. Both must pass.
2. Sanity check the failure mode: temporarily reorder one element of
   `compile_open_steps` (e.g. push the scan step before the opens) and
   re-run `just test-rust` to confirm both tests now fail with the
   chain-violation panic. Revert the temporary edit. (Optional, only if
   you want to see the failure shape before committing.)

No VM tests are needed -- the change is contained to the dry-run /
render path, which is pure and exercised entirely in the Rust unit-test
lane.
