//! Whether the filesystem mounted at `braid.mountPoint` is this pool.
//!
//! `mountpoint -q` says only that something is mounted there; util-linux
//! exits 0 for any filesystem. So every caller that acts on "the pool is
//! mounted" -- the online finalizer, unlock, recover, and lock -- takes a
//! confirmed mount from here and classifies it, and a tmpfs, an ext4 disk, or
//! another btrfs at the mount point is never treated as the pool.
//!
//! The answer comes from kernel state. The mount table gives the filesystem
//! type and the device btrfs names for the mount, and sysfs gives that
//! device's LUKS UUID (`cli/src/dm_identity.rs`). One device is enough,
//! because a block device belongs to only one btrfs. The pool's own records,
//! `pool.json` plus a pending journal, say which LUKS UUIDs are its members.

use std::collections::BTreeSet;
use std::io::ErrorKind;

use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::dm_identity::{self, UnprovenMapper};
use crate::filesystem::Filesystem;
use crate::journal;
use crate::membership::{self, MembershipError};
use crate::mount_check;
use crate::online_state::{OnlineError, OnlineStateOps};
use crate::state_paths::StatePaths;
use crate::types::{LuksUuid, MountPoint};

/// sysfs directory with one entry per block device and partition, keyed by
/// kernel name.
const SYS_CLASS_BLOCK: &str = "/sys/class/block";

/// Proof that the filesystem at the mount point is this pool. Only
/// `ConfirmedMount::classify` builds one, so the online finalizer cannot
/// change ownership of, or start `braid-online.service` over, anything else.
#[derive(Debug)]
pub struct PoolMounted(());

/// Proof that `mountpoint -q` observed nothing mounted at the mount point.
/// Only `check_mountpoint` builds one, so `braid-online.service` stops only
/// over an observed unmount, never over a mounted or unknown mount state.
#[derive(Debug)]
pub struct PoolUnmounted(());

/// `mountpoint -q` confirmed that something is mounted at the mount point.
/// Only `classify` says whether it is this pool.
#[derive(Debug)]
pub struct ConfirmedMount {
    mount_point: MountPoint,
}

/// The finalizers' reading of `mountpoint -q`.
#[derive(Debug)]
pub enum Mountpoint {
    NotMounted(PoolUnmounted),
    Mounted(ConfirmedMount),
}

/// The finalizers' `mountpoint -q` check, under util-linux's exit contract
/// (`OnlineStateOps::is_mountpoint`): only exit 32 proves nothing is mounted,
/// and any other failure is an error the caller treats as unknown.
pub fn check_mountpoint(
    ops: &dyn OnlineStateOps,
    mount_point: &MountPoint,
) -> Result<Mountpoint, OnlineError> {
    Ok(if ops.is_mountpoint(mount_point)? {
        Mountpoint::Mounted(ConfirmedMount {
            mount_point: mount_point.clone(),
        })
    } else {
        Mountpoint::NotMounted(PoolUnmounted(()))
    })
}

/// The planners' `mountpoint -q` check. Only exit 0 confirms a mount; any
/// other exit counts as not mounted, because util-linux also exits 1 when the
/// mount point directory does not exist yet, and the mount step creates it.
pub fn confirmed_mount<R: CommandRunner + ?Sized>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<Option<ConfirmedMount>, CmdError> {
    let output = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.clone().into(),
    })?;
    Ok((output.exit_status == 0).then(|| ConfirmedMount {
        mount_point: mount_point.clone(),
    }))
}

/// What occupies the mount point once a mount is confirmed.
#[derive(Debug)]
pub enum MountedAt {
    Pool(PoolMounted),
    Foreign(ForeignMount),
    Unknown(UnprovenMount),
}

/// A mount braid can prove is not this pool.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ForeignMount {
    /// Not btrfs. The wording is `ProbeError::NotBtrfs`'s, the refusal every
    /// other command already gives for this mount.
    #[error("{mount_point} is mounted but fstype is {fstype}, not btrfs")]
    NotBtrfs { mount_point: String, fstype: String },
    #[error(transparent)]
    OtherBtrfs(#[from] OtherBtrfs),
}

/// A btrfs at the mount point that is not this pool: the device the kernel
/// names for it is not a LUKS mapper, or its LUKS UUID is no pool member.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{mount_point} holds a btrfs filesystem on {device} that is not this pool: {reason}")]
pub struct OtherBtrfs {
    mount_point: String,
    device: String,
    reason: String,
}

/// A mount braid can prove neither is nor is not this pool.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot tell whether the filesystem at {mount_point} is this pool: {reason}")]
pub struct UnprovenMount {
    mount_point: String,
    reason: String,
}

impl ConfirmedMount {
    /// Classify the confirmed mount. The reads stop at the first answer: the
    /// mount table, then the device's LUKS UUID in sysfs, then -- only for a
    /// btrfs on a LUKS mapper -- the pool's members. Foreign needs positive
    /// proof; a read that fails or finds nothing gives Unknown.
    pub fn classify<F: Filesystem + ?Sized>(self, fs: &F, paths: &StatePaths) -> MountedAt {
        let mount_point = self.mount_point.as_str().to_owned();
        let unknown = |reason: String| {
            MountedAt::Unknown(UnprovenMount {
                mount_point: mount_point.clone(),
                reason,
            })
        };

        let entry = match mount_check::mount_entry_at_via_fs(fs, &mount_point) {
            Ok(Some(entry)) => entry,
            Ok(None) => return unknown("mountinfo has no entry for it".to_owned()),
            Err(e) => return unknown(format!("mountinfo error: {e}")),
        };
        if entry.fstype != "btrfs" {
            return MountedAt::Foreign(ForeignMount::NotBtrfs {
                mount_point,
                fstype: entry.fstype,
            });
        }
        let other = |reason: String| {
            MountedAt::Foreign(ForeignMount::OtherBtrfs(OtherBtrfs {
                mount_point: mount_point.clone(),
                device: entry.source.clone(),
                reason,
            }))
        };

        let uuid = match source_identity(fs, &entry.source) {
            SourceIdentity::Luks(uuid) => uuid,
            SourceIdentity::NotLuks(reason) => return other(reason),
            SourceIdentity::Unproven(reason) => return unknown(reason),
        };
        let known = match known_members(paths) {
            Ok(known) => known,
            Err(reason) => return unknown(reason),
        };
        if known.is_empty() {
            return unknown(
                "braid knows no member of this pool: pool.json lists none and no operation is pending"
                    .to_owned(),
            );
        }
        if known.contains(&uuid) {
            MountedAt::Pool(PoolMounted(()))
        } else {
            other(format!(
                "its LUKS UUID {uuid} is not a member in pool.json or the pending-operation journal"
            ))
        }
    }
}

/// What sysfs says about the device a mount names.
enum SourceIdentity {
    /// A device-mapper device whose dm UUID records this LUKS UUID.
    Luks(LuksUuid),
    /// Proven not to be a LUKS mapper.
    NotLuks(String),
    /// No proof either way.
    Unproven(String),
}

/// Resolve a mount source to its LUKS identity. btrfs prints the device path
/// it registered first: `/dev/mapper/<name>` when braid's scan or mount came
/// first, or the kernel node `/dev/dm-N` when udev's scan did. A `/dev/<name>`
/// node for a block device that is not device-mapper is proven not a LUKS
/// mapper. Any other text, such as a `/dev/disk/by-*` link or btrfs's
/// `<missing disk>`, is unproven.
fn source_identity<F: Filesystem + ?Sized>(fs: &F, source: &str) -> SourceIdentity {
    let single_name = |prefix: &str| {
        source
            .strip_prefix(prefix)
            .filter(|name| !name.is_empty() && !name.contains('/'))
    };
    let lookup = if let Some(name) = single_name("/dev/mapper/") {
        dm_identity::named_luks_uuid(fs, name)
    } else if let Some(kernel_name) = single_name("/dev/") {
        if kernel_name.starts_with("dm-") {
            dm_identity::entry_luks_uuid(fs, kernel_name)
        } else if fs.exists(&format!("{SYS_CLASS_BLOCK}/{kernel_name}")) {
            return SourceIdentity::NotLuks(format!("{source} is not a device-mapper device"));
        } else {
            return SourceIdentity::Unproven(format!(
                "{source} is not a block device in {SYS_CLASS_BLOCK}"
            ));
        }
    } else {
        return SourceIdentity::Unproven(format!(
            "mountinfo names its device {source:?}, which braid cannot resolve"
        ));
    };
    match lookup {
        Ok(uuid) => SourceIdentity::Luks(uuid),
        Err(UnprovenMapper::NotLuks { dm_uuid }) => SourceIdentity::NotLuks(format!(
            "its device-mapper UUID {dm_uuid:?} is not a cryptsetup LUKS UUID"
        )),
        Err(e @ (UnprovenMapper::Absent | UnprovenMapper::Unreadable { .. })) => {
            SourceIdentity::Unproven(format!("cannot read the LUKS identity of {source}: {e}"))
        }
    }
}

/// The LUKS UUIDs that can belong to this pool's devices: `pool.json`'s
/// members plus a pending journal's pre and target members. The journal
/// covers a crash between a btrfs commit and the `pool.json` write, when a
/// device is only in the journal. A missing file adds nothing; any other read
/// error is returned as the reason the answer is unknown.
///
/// It never exits, unlike a dispatch-time `pool.json` load, so a bad record
/// cannot end a command before the command decides. An unreadable record
/// makes a mounted pool Unknown, and each caller's match arm decides what
/// that means: unlock and recover refuse it, the online finalizer skips it,
/// and lock proceeds on it. So `braid lock` still unmounts the pool, and
/// with a journal pending, `braid recover` then mounts from the journal and
/// rewrites `pool.json`.
fn known_members(paths: &StatePaths) -> Result<BTreeSet<LuksUuid>, String> {
    let mut known = BTreeSet::new();
    match membership::load_membership(paths) {
        Ok(pool) => known.extend(pool.iter().map(|(uuid, _)| uuid.clone())),
        Err(MembershipError::Io { source, .. }) if source.kind() == ErrorKind::NotFound => {}
        Err(MembershipError::Conflict(detail)) => {
            return Err(format!("pool.json conflict: {detail}"));
        }
        Err(e) => return Err(e.to_string()),
    }
    match journal::load_journal(paths) {
        Ok(Some(pending)) => known.extend(
            pending
                .pre_membership
                .iter()
                .chain(pending.target_membership.iter())
                .map(|(uuid, _)| uuid.clone()),
        ),
        Ok(None) => {}
        Err(e) => return Err(e.to_string()),
    }
    Ok(known)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::journal::{OpKind, build_journal, write_journal};
    use crate::membership::{PoolMembership, save_membership};
    use crate::online_state::{RecordingOnlineStateOps, StagedOnlineFailure};
    use crate::test_fixtures::{MockFs, disk_member, isolated_paths};
    use tempfile::TempDir;

    const UUID_A: &str = "11111111-1111-1111-1111-111111111111";
    const UUID_B: &str = "22222222-2222-2222-2222-222222222222";
    const UUID_X: &str = "99999999-9999-9999-9999-999999999999";

    fn mp() -> MountPoint {
        MountPoint::new("/mnt/storage".to_owned())
    }

    fn uuid(raw: &str) -> LuksUuid {
        LuksUuid::parse(raw).unwrap()
    }

    fn mountinfo(fstype: &str, source: &str) -> String {
        format!("36 35 0:32 / /mnt/storage rw shared:1 - {fstype} {source} rw\n")
    }

    fn members(uuids: &[&str]) -> PoolMembership {
        let mut pool = PoolMembership::empty();
        for (n, raw) in uuids.iter().enumerate() {
            let (_, member) = disk_member(
                n as u64 + 1,
                &format!("disk{}", n + 1),
                &format!("/dev/disk/by-id/virtio-disk{}", n + 1),
            );
            pool.insert(uuid(raw), member).unwrap();
        }
        pool
    }

    fn state_with_pool_json(uuids: &[&str]) -> (TempDir, StatePaths) {
        let (dir, paths) = isolated_paths();
        save_membership(&members(uuids), &paths).unwrap();
        (dir, paths)
    }

    fn classify(fs: &MockFs, paths: &StatePaths) -> MountedAt {
        ConfirmedMount { mount_point: mp() }.classify(fs, paths)
    }

    fn reason(found: MountedAt) -> String {
        match found {
            MountedAt::Pool(_) => panic!("expected a non-pool answer, got Pool"),
            MountedAt::Foreign(foreign) => foreign.to_string(),
            MountedAt::Unknown(unproven) => unproven.to_string(),
        }
    }

    // Intent: a btrfs whose named device records a pool.json member's LUKS
    //   UUID is this pool, whether btrfs names it by mapper or by kernel node.
    // Why it exists: the real pool must keep its "already mounted" exit 0 and
    //   its online finalizer; btrfs prints whichever path registered first.
    // Scenario: the pool is mounted; mountinfo shows /dev/mapper/braid-disk1
    //   on one kernel and /dev/dm-0 on another.
    #[test]
    fn btrfs_on_a_member_mapper_is_the_pool() {
        let (_dir, paths) = state_with_pool_json(&[UUID_A, UUID_B]);
        for source in ["/dev/mapper/braid-disk1", "/dev/dm-0"] {
            let fs = MockFs::storage(vec![])
                .with_mountinfo(&mountinfo("btrfs", source))
                .with_luks_dm("braid-disk1", UUID_A);
            assert!(
                matches!(classify(&fs, &paths), MountedAt::Pool(_)),
                "{source} should classify as the pool"
            );
        }
    }

    // Intent: a non-btrfs mount is Foreign with ProbeError::NotBtrfs's words,
    //   and needs no pool record to say so.
    // Why it exists: unlock, recover, the finalizer, and lock all refuse this
    //   mount; the text must match what add and status already print.
    // Scenario: an ext4 disk is mounted at /mnt/storage on a host whose
    //   pool.json is missing.
    #[test]
    fn non_btrfs_is_foreign_without_reading_the_pool_record() {
        let (_dir, paths) = isolated_paths();
        let fs = MockFs::storage(vec![]).with_mountinfo(&mountinfo("ext4", "/dev/sdb1"));

        match classify(&fs, &paths) {
            MountedAt::Foreign(foreign @ ForeignMount::NotBtrfs { .. }) => assert_eq!(
                foreign.to_string(),
                "/mnt/storage is mounted but fstype is ext4, not btrfs"
            ),
            other => panic!("expected NotBtrfs, got {other:?}"),
        }
    }

    // Intent: a btrfs that braid can prove is not this pool is Foreign.
    // Why it exists: lock would otherwise unmount another btrfs and close
    //   its LUKS mapper as an orphan, and unlock would report it as the pool.
    // Scenario: three unrelated btrfs disks at /mnt/storage -- one on a
    //   non-member LUKS mapper, one on a plain partition, one on LVM.
    #[test]
    fn btrfs_that_is_provably_not_a_member_is_foreign() {
        let (_dir, paths) = state_with_pool_json(&[UUID_A]);
        let cases = [
            (
                MockFs::storage(vec![])
                    .with_mountinfo(&mountinfo("btrfs", "/dev/mapper/backup"))
                    .with_luks_dm("backup", UUID_X),
                format!(
                    "/mnt/storage holds a btrfs filesystem on /dev/mapper/backup that is not this pool: its LUKS UUID {UUID_X} is not a member in pool.json or the pending-operation journal"
                ),
            ),
            (
                MockFs::storage(vec!["/sys/class/block/sdb1".to_owned()])
                    .with_mountinfo(&mountinfo("btrfs", "/dev/sdb1")),
                "/mnt/storage holds a btrfs filesystem on /dev/sdb1 that is not this pool: /dev/sdb1 is not a device-mapper device".to_owned(),
            ),
            (
                MockFs::storage(vec![])
                    .with_mountinfo(&mountinfo("btrfs", "/dev/mapper/vg-data"))
                    .with_raw_dm("vg-data", "LVM-abc"),
                "/mnt/storage holds a btrfs filesystem on /dev/mapper/vg-data that is not this pool: its device-mapper UUID \"LVM-abc\" is not a cryptsetup LUKS UUID".to_owned(),
            ),
        ];
        for (fs, expected) in cases {
            let found = classify(&fs, &paths);
            assert!(
                matches!(found, MountedAt::Foreign(ForeignMount::OtherBtrfs(_))),
                "expected OtherBtrfs, got {found:?}"
            );
            assert_eq!(reason(found), expected);
        }
    }

    // Intent: every read that fails or finds nothing gives Unknown, never
    //   Foreign.
    // Why it exists: lock still runs on Unknown, so a missing or corrupt
    //   pool.json cannot block shutdown cleanup; only proof refuses.
    // Scenario: the mount table, the device's sysfs entry, or the pool record
    //   cannot answer.
    #[test]
    fn missing_or_unreadable_evidence_is_unknown() {
        let member_fs = || {
            MockFs::storage(vec![])
                .with_mountinfo(&mountinfo("btrfs", "/dev/mapper/braid-disk1"))
                .with_luks_dm("braid-disk1", UUID_A)
        };
        let (_pool_dir, pool_paths) = state_with_pool_json(&[UUID_A]);
        let (_empty_dir, empty_paths) = isolated_paths();
        let (_corrupt_dir, corrupt_paths) = isolated_paths();
        std::fs::write(corrupt_paths.pool_json(), "{not json").unwrap();
        let (_journal_dir, journal_paths) = state_with_pool_json(&[UUID_A]);
        std::fs::write(journal_paths.pending_op_json(), "{not json").unwrap();

        let cases: Vec<(MockFs, &StatePaths, &str)> = vec![
            (
                MockFs::storage(vec![])
                    .with_mountinfo("26 25 0:23 / / rw shared:1 - ext4 /dev/sda1 rw\n"),
                &pool_paths,
                "mountinfo has no entry for it",
            ),
            (
                MockFs::storage(vec![]).with_mountinfo(&format!(
                    "{}{}",
                    mountinfo("btrfs", "/dev/mapper/braid-disk1"),
                    mountinfo("btrfs", "/dev/mapper/braid-disk1")
                )),
                &pool_paths,
                "mountinfo error:",
            ),
            (
                MockFs::storage(vec![]).with_mountinfo(&mountinfo("btrfs", "<missing\\040disk>")),
                &pool_paths,
                "\"<missing disk>\", which braid cannot resolve",
            ),
            (
                MockFs::storage(vec![])
                    .with_mountinfo(&mountinfo("btrfs", "/dev/disk/by-id/dm-name-braid-disk1")),
                &pool_paths,
                "which braid cannot resolve",
            ),
            (
                MockFs::storage(vec![]).with_mountinfo(&mountinfo("btrfs", "/dev/sdz")),
                &pool_paths,
                "/dev/sdz is not a block device in /sys/class/block",
            ),
            (
                MockFs::storage(vec![])
                    .with_mountinfo(&mountinfo("btrfs", "/dev/mapper/braid-disk1")),
                &pool_paths,
                "cannot read the LUKS identity of /dev/mapper/braid-disk1",
            ),
            (
                member_fs(),
                &empty_paths,
                "braid knows no member of this pool",
            ),
            (member_fs(), &corrupt_paths, "pool.json"),
            (member_fs(), &journal_paths, "pending-op.json"),
        ];
        for (fs, paths, expected) in cases {
            let found = classify(&fs, paths);
            assert!(
                matches!(found, MountedAt::Unknown(_)),
                "expected Unknown for {expected:?}, got {found:?}"
            );
            let text = reason(found);
            assert!(
                text.starts_with(
                    "cannot tell whether the filesystem at /mnt/storage is this pool: "
                ) && text.contains(expected),
                "expected {expected:?} in {text:?}"
            );
        }
    }

    // Intent: a device found only in the pending journal still proves the
    //   pool, with or without pool.json.
    // Why it exists: a single-device replace that crashes after btrfs
    //   commits leaves the filesystem's only device in the journal's target,
    //   and an interrupted bootstrap add leaves no pool.json at all. Lock and
    //   recover must still recognize the pool.
    // Scenario: pool.json lists disk A while the journal targets disk B, and
    //   separately pool.json is missing while the journal targets disk B.
    #[test]
    fn journal_members_count_as_the_pool() {
        let journal = build_journal(
            members(&[UUID_A]),
            members(&[UUID_B]),
            OpKind::Remove {
                luks_uuid: uuid(UUID_A),
                name: crate::types::DiskName::parse("disk1").unwrap(),
            },
        );
        let (_replace_dir, mid_replace) = state_with_pool_json(&[UUID_A]);
        write_journal(&mid_replace, &journal).unwrap();
        let (_bootstrap_dir, bootstrap) = isolated_paths();
        write_journal(&bootstrap, &journal).unwrap();
        let fs = MockFs::storage(vec![])
            .with_mountinfo(&mountinfo("btrfs", "/dev/mapper/braid-disk2"))
            .with_luks_dm("braid-disk2", UUID_B);

        for paths in [&mid_replace, &bootstrap] {
            assert!(matches!(classify(&fs, paths), MountedAt::Pool(_)));
        }
    }

    // Intent: the planners' check confirms a mount only on exit 0.
    // Why it exists: util-linux exits 1 when the mount point directory does
    //   not exist yet, and unlock must still mount the pool then.
    // Scenario: `mountpoint -q` exits 0, 32, and 1.
    #[test]
    fn confirmed_mount_needs_exit_zero() {
        for (exit_status, confirmed) in [(0, true), (32, false), (1, false)] {
            let runner = MockRunner::default().with_output(
                CmdRequest::MountpointCheck { path: mp().into() },
                RawCommandOutput {
                    cmd: "mountpoint".into(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status,
                },
            );
            assert_eq!(
                confirmed_mount(&runner, &mp()).unwrap().is_some(),
                confirmed,
                "exit {exit_status}"
            );
        }
    }

    // Intent: the finalizers' check keeps util-linux's exit contract: only a
    //   clean "not mounted" builds the unmounted proof.
    // Why it exists: plain lock stops braid-online.service only on that
    //   proof, so a failed check must not produce one.
    // Scenario: `mountpoint -q` reports mounted, not mounted, and fails.
    #[test]
    fn check_mountpoint_proves_unmounted_only_on_a_clean_answer() {
        let ops = RecordingOnlineStateOps::new();
        assert!(matches!(
            check_mountpoint(&ops, &mp()),
            Ok(Mountpoint::Mounted(_))
        ));
        ops.set_mounted(false);
        assert!(matches!(
            check_mountpoint(&ops, &mp()),
            Ok(Mountpoint::NotMounted(_))
        ));
        ops.set_mountpoint_err(StagedOnlineFailure::Spawn("spawn failed".into()));
        assert!(check_mountpoint(&ops, &mp()).is_err());
    }
}
