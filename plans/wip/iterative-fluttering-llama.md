# Narrow bootstrap detection to confirmed NoBtrfs

## Context

The previous plan (now implemented) catches `MountError::MountFailed` when `pre_membership` is empty and assumes "mkfs never ran." But `pending-op.json` is written *before* any irreversible disk work, and a bootstrap mount failure doesn't prove mkfs never ran — it could mean mkfs succeeded but the mount failed for another reason (e.g. missing kernel module, mount options, etc.). The detection needs to actually prove NoBtrfs on the target device before emitting the "filesystem was never created" guidance.

## What's already in place (previous plan)

- `MountError::MountFailed` variant in `cli/src/mount.rs:16` — **keep as-is**
- Bootstrap catch block in `cli/src/recover.rs:70-93` — **modify**
- Three tests in `cli/src/recover.rs` — **modify two, add one**

## Changes

### 1. `cli/src/recover.rs` — imports

Add `CmdRequest` and the btrfs probe classifier:

```rust
use crate::cmd::{CmdRequest, CommandRunner};           // add CmdRequest
use crate::parse::btrfs_filesystem_show::{classify_btrfs_probe, DeviceBtrfsProbe};
```

### 2. `cli/src/recover.rs` — bootstrap detection block (lines 70-93)

Replace the current block. After catching bootstrap + MountFailed, extract disk names from `journal.op` (must be `OpKind::Add { disks }`), probe each mapper with `BtrfsFilesystemShowTarget` + `classify_btrfs_probe`, and only emit the guidance if **all** probed disks return `NoBtrfs`.

```rust
if journal.pre_membership.disks.is_empty() {
    if let mount::MountError::MountFailed(_) = &e {
        if let journal::OpKind::Add { ref disks } = journal.op {
            let all_no_btrfs = disks.keys().all(|name| {
                let mapper = format!("/dev/mapper/{}", config::mapper_name(name).0);
                match runner.run(&CmdRequest::BtrfsFilesystemShowTarget {
                    target: mapper,
                }) {
                    Ok(raw) => matches!(classify_btrfs_probe(&raw), DeviceBtrfsProbe::NoBtrfs),
                    Err(_) => false,
                }
            });
            if all_no_btrfs {
                let disk_list: Vec<_> = union
                    .disks
                    .iter()
                    .map(|(name, m)| format!("  {} ({})", name, m.by_id))
                    .collect();
                return Err(RecoverError::Failed(format!(
                    "bootstrap add was interrupted before the filesystem was created.\n\
                     The pool does not exist yet, so there is nothing to recover.\n\n\
                     To return to a clean state:\n\
                     1. rm {}\n\
                     2. Wipe the LUKS container from each disk that was being added:\n{}\n\
                        e.g.: wipefs -a /dev/disk/by-id/<device>\n\
                     3. Re-run braid add",
                    paths.pending_op_json().display(),
                    disk_list.join("\n"),
                )));
            }
        }
    }
}
return Err(e.into());
```

Update the comment above the block to say: "Probe the target devices to confirm no btrfs superblock exists — only then is it safe to advise wiping."

### 3. Tests

**Test A (modify): `recover_bootstrap_crash_gives_actionable_instructions`**

Add a `BtrfsFilesystemShowTarget` mock after the mount-fail mock that returns NoBtrfs:

```rust
.with_output(
    CmdRequest::BtrfsFilesystemShowTarget {
        target: "/dev/mapper/braid-disk1".into(),
    },
    err_raw(
        "btrfs filesystem show",
        1,
        "not a valid btrfs filesystem on /dev/mapper/braid-disk1",
    ),
)
```

Assertions unchanged — still expects "bootstrap add was interrupted", "wipefs", etc.

**Test B (keep as-is): `recover_bootstrap_wrong_passphrase_not_masked`**

No change needed — passphrase fails before mount, so MountFailed is never reached, and the btrfs probe is never called. Still asserts "wrong passphrase" propagates.

**Test C (keep as-is): `recover_non_bootstrap_mount_failure_propagates`**

No change needed — pre_membership is non-empty, so the bootstrap branch is never entered.

**Test D (new): `recover_bootstrap_mount_fails_but_btrfs_exists_propagates_mount_error`**

```
/// Intent: when bootstrap recover's mount fails but the disk actually has a
///   btrfs superblock, the original mount error must propagate — the guidance
///   to wipe disks would be wrong.
///
/// Why it exists: mkfs may have succeeded but mount failed for another reason
///   (missing kernel module, bad options). Telling the user to wipefs would
///   destroy a valid filesystem.
///
/// Scenario: first-ever add of one disk. mkfs.btrfs succeeded, mount failed
///   for an unrelated reason. btrfs filesystem show confirms HasBtrfs. Error
///   should be the original mount error, not bootstrap guidance.
```

- Journal: bootstrap (pre={}, target={disk1})
- MockRunner: same as Test A through mount failure, but `BtrfsFilesystemShowTarget` returns exit 0 (HasBtrfs) with valid btrfs show output
- Assert: error contains "mount failed" (original error)
- Assert: error does NOT contain "bootstrap add was interrupted"
- Assert: journal NOT cleared

## Files modified

- `cli/src/recover.rs`

## Verification

`just test-rust`
