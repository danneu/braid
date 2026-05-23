# Document the `-f` flag in `btrfs replace start`

## Context

`/verify-issue` (finding #5: "No explicit handling of replace target already
containing a btrfs filesystem") concluded that the finding's prescribed fix
(add `blkdiscard`/`wipefs` and a user confirmation before `btrfs replace
start`) is wrong for braid's architecture, but caught a real docs gap: the
`-f` flag in `cli/src/cmd.rs` for `BtrfsReplaceStart` is passed
unconditionally and silently, with no inline rationale. A reader reaching
that line cold cannot tell whether braid is being cavalier about data
safety or whether the safety story is handled upstream.

The safety story has two parts: braid's identity and authorization
checks upstream of the `-f` flag, and btrfs-progs' own target preparation
downstream of it.

Upstream of `-f`, the planner classifies the new disk into one of two
prep paths:

- `ReplaceTargetPrep::FreshLuks` (`cli/src/replace.rs:247-289`): braid
  runs `cryptsetup luksFormat` first, so the mapper's encrypted view is
  random by construction -- any prior filesystem signature is no longer
  visible through the mapper.
- `ReplaceTargetPrep::ExistingLuks` (`cli/src/replace.rs:290-326`): the
  operator may supply a LUKS device that is not previously a pool
  member (precedent: `tests/cli/replace-new-already-luks.py:81`
  preformats `disk4` as LUKS, never adds it to the pool, then uses it
  as the `--new` target). braid captures the target's current LUKS UUID
  via `cryptsetup luksUUID` at planning, rejects it if any live pool
  device already carries that UUID (`verify_replace_execute_live_pool_uuid`
  at `cli/src/replace.rs:945`), re-probes the by-id form right before
  `ensure_luks_open` (`probe_existing_luks_new_target_uuid` at
  `cli/src/replace.rs:989`, catches hot-swap-at-the-slot), and
  re-verifies UUID + backing path on the open mapper
  (`verify_existing_luks_open_mapper_target` at `cli/src/replace.rs:1022`).
  Authority to consume the disk comes from the explicit `--new <name>=<by-id>`
  argument plus passphrase plus `--yes`/prompt, not from prior pool
  membership.

The `-f` flag itself skips btrfs's pre-mkfs filesystem-signature check
(`test_dev_for_mkfs` -> `check_overwrite` in
`reference/btrfs-progs/mkfs/common.c:1096-1168`).

Downstream of `-f`, btrfs-progs still runs `btrfs_prepare_device` on
the target before issuing `BTRFS_IOC_DEV_REPLACE`
(`reference/btrfs-progs/cmds/replace.c:307`, implementation at
`reference/btrfs-progs/common/device-utils.c:229`): it zeros the
device start, every superblock-mirror region, and the device tail;
optionally discards; and then calls `btrfs_wipe_existing_sb` to wipe
any remaining signatures. Only after this preparation does the kernel
ioctl run the replace scrub-copy from source to target. That
preparation is why braid does not need its own `scan-forget` +
`wipefs --types btrfs` ladder for the replace primitive -- the ladder
that `add`'s returned-disk path uses (`cli/src/add.rs:777-799`,
motivated by Decision 012.e) exists for `btrfs device add`, which does
not call `btrfs_prepare_device` on the target.

The intended outcome of this plan: a single inline comment block above
the `-f` argv push that makes that reasoning visible at the callsite, in
the same style as the existing `-r` rationale four lines above.

## Scope

In scope:

- One inline comment in `cli/src/cmd.rs` immediately above the `-f` entry
  in the `BtrfsReplaceStart` argv block.

Explicitly out of scope (and deliberately rejected from the finding's
recommendation):

- Adding `wipefs`/`blkdiscard` to the replace work plan.
- Adding any new user confirmation gate; `--yes` and the dry-run preview
  already cover intent.
- Touching `-B` in the same block. Same-block consistency is tempting but
  `-B` was not what the finding flagged, and CLAUDE.md says to default to
  no comments. Skip.
- Touching `docs/design/decisions/012-intent-cli.md`'s "Replace safety
  constraints" section. It documents intent-level constraints (which
  `--old` values are valid, which paths are allowed), not flag-level
  rationale, and a new bullet there would diverge from the section's
  current voice.
- Touching `docs/commands/replace.md` or any new `docs/internals/`
  page. There is no replace internals doc today (`ls
  docs/internals/btrfs/` returns only `balance-profiles.md`,
  `balance-soft.md`, `enospc-vs-hang.md`, `luks-sector-size.md`), and the
  cmd.rs callsite is the right home for code-level rationale.

## Critical file

- `cli/src/cmd.rs:754-777` -- the `BtrfsReplaceStart` argv block. The
  comment goes between line 770 (`"-r".into(),` with the existing
  multi-line `-r` rationale ending just above) and line 771 (`"-f".into(),`).

## Proposed comment

Match the style of the existing `-r` comment block (line-comments, full
sentences, `--` instead of em-dash per global CLAUDE.md ASCII guidance,
no `reference/...:lineno` refs per the project's "drop rust line-number
refs from comments" convention):

```rust
// -f: skip btrfs's pre-mkfs filesystem-signature check on the
// target mapper. The target is either freshly luksFormat'd by
// braid (FreshLuks prep -- the encrypted view is random by
// construction) or an operator-supplied LUKS device whose probed
// UUID has been cleared as not-already-in-the-pool, re-probed at
// the by-id form right before LUKS open, and re-verified against
// the open mapper's backing path. Authority to consume any prior
// content comes from the explicit `--new` argument plus passphrase
// plus `--yes`/confirmation, not from prior pool membership.
// btrfs-progs still runs `btrfs_prepare_device` on the target
// (zero superblock-mirror regions and device ends, wipe existing
// signatures, optional discard) before issuing
// BTRFS_IOC_DEV_REPLACE, so the scan-forget + wipefs ladder used
// by `add`'s returned-disk path (see Decision 012.e) is not
// needed for the replace primitive.
```

## Verification

Behavior-preserving comment-only change. No new tests; no test changes.

- `cargo check -p braid-cli` (via `cd cli && cargo check` if needed) --
  confirm the comment doesn't accidentally break the macro/argv block.
- Visual review: open `cli/src/cmd.rs` at the `BtrfsReplaceStart` block
  and confirm the `-r` and `-f` comments read as a matched pair, both
  explain WHY braid uses the flag, and the comment lands above `-f`
  rather than between unrelated argv entries.
- `git diff cli/src/cmd.rs` -- diff should be a single contiguous
  hunk inside the `BtrfsReplaceStart` arm of `to_argv`. Nothing else
  should change.
