# Plan: fix false `braid doctor` SMB-inhibitor claim (delete, don't reword)

## Context

`docs/guides/sharing-and-permissions.md:170` claims:

> `braid doctor` also picks up active SMB connections as auto-suspend inhibitors -- see [Power management](power-management.md).

This is **wrong on two counts**, and the original review finding's proposed fix
(reword the sentence in place) is not the ideal shape. The ideal pivot is to
**delete the sentence entirely**.

### Why the claim is wrong

1. **Wrong command.** `braid doctor` has no SMB awareness. `cli/src/doctor.rs`
   contains zero `smb`/`samba`/`inhibit`/`ActiveConnection` code (verified via
   `rg`), and its run-list (`doctor.rs:1613-1630`) shows exactly one
   suspend-adjacent check: `check_wake_on_lan` (`doctor.rs:1335`). SMB
   suspend-blocking is entirely the **autosuspend daemon's** `Smb` idle check in
   `modules/braid/auto-suspend.nix:114-117`, gated on `services.samba.enable`.
2. **Wrong term.** In braid a "sleep inhibitor" is the logind `systemd-inhibit`
   lock held during `remove`/migration (`cli/src/inhibit.rs`,
   `docs/commands/remove.md:52`). The autosuspend `Smb` check is an
   idle-detection gate, not a sleep inhibitor.

### Why delete instead of reword

The reworded sentence would be accurate but **redundant and misplaced**:

- **Redundant.** The same file already states this correctly, and more
  completely, 19 lines down at `sharing-and-permissions.md:189-191`
  ("## Auto-suspend integration"): *"If you enable `braid.autoSuspend`, active
  SMB and NFS connections automatically block suspend..."* (covers NFS too).
- **Misplaced.** Line 170 sits at the tail of the "### Binding shares to the
  pool lifecycle" subsection (lines 150-169), which is wholly about
  `wantedBy`/`bindsTo`/`after` and `braid lock` walking `BoundBy` before
  `umount`. An auto-suspend remark is a non-sequitur there.
- **Cross-reference is not lost.** The `[Power management](power-management.md)`
  link still exists in the "Related" section at line 198 after deletion.

Intended outcome: the false attribution is removed with no information loss and
no orphaned link; auto-suspend/SMB behavior remains documented correctly in its
own section and in power-management.md.

## Change

**File:** `docs/guides/sharing-and-permissions.md` (only file modified)

Delete line 170 and one adjacent blank line so the "Binding shares to the pool
lifecycle" subsection ends cleanly on its `braid lock`/`BoundBy` paragraph,
immediately followed by the `## NFS` heading.

Before (lines 168-172):

```
... This is the same pattern braid's own scrub timer uses (see `modules/braid/storage.nix`).

`braid doctor` also picks up active SMB connections as auto-suspend inhibitors -- see [Power management](power-management.md).

## NFS
```

After:

```
... This is the same pattern braid's own scrub timer uses (see `modules/braid/storage.nix`).

## NFS
```

No code, test, or other doc changes. No edits to `doctor.rs`,
`auto-suspend.nix`, `power-management.md`, or the "Auto-suspend integration"
section (already correct).

## Verification

- `rg -n 'braid doctor' docs/guides/sharing-and-permissions.md` -- confirm the
  SMB-inhibitor line is gone and no other `braid doctor` line mentions SMB.
- `rg -n 'power-management.md' docs/guides/sharing-and-permissions.md` -- confirm
  the cross-reference still survives at the "Related" section (line ~197).
- `mdbook build docs` -- succeeds; `mdbook-linkcheck2` passes (deleting a link
  never breaks linkcheck, and the target is still referenced elsewhere).
- No Rust/VM test exercises this prose, so `just test-*` runs are not required.
