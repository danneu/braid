//! The plan-time check that the disks `add` and `replace` will format are
//! distinct whole disks.
//!
//! An operator names a target by one of its `/dev/disk/by-id/` links, and one
//! disk has several (ata-, wwn-, scsi-, ...). Two links to one disk, or a disk
//! and one of its partitions, each pass every per-disk check. But after the
//! journal, the first format holds the disk and cryptsetup refuses the second
//! one as in use, so the operator lands in recovery mode. So this check
//! compares targets by the device each link resolves to, and asks sysfs (the
//! kernel, not the link's name) whether each device is a whole disk. Once every
//! target is a whole disk, two targets overlap only by being the same device.
//! It lives apart from `cli/src/membership.rs#parse_disk_spec` because it reads
//! the host, and that parser is pure.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::Path;

use crate::filesystem::Filesystem;
use crate::luks::BackingPathResolver;
use crate::types::{ByIdPath, DiskName};

/// sysfs directory with one entry per block device and partition, keyed by
/// kernel name. A partition's entry has a `partition` file; a whole disk's
/// does not.
const SYS_CLASS_BLOCK: &str = "/sys/class/block";

/// Why a command's target specs are not distinct whole disks. Every message
/// stands alone and names the spec, so commands pass it through untagged.
#[derive(Debug, thiserror::Error)]
pub enum TargetDiskError {
    #[error("disks '{first}' and '{second}' both name {by_id}. Pass each disk once.")]
    SameLink {
        first: DiskName,
        second: DiskName,
        by_id: ByIdPath,
    },
    #[error(
        "disks '{first}' and '{second}' are the same disk -- {first_by_id} and \
         {second_by_id} both point to {device}. Pass each disk once."
    )]
    SameDevice {
        first: DiskName,
        first_by_id: ByIdPath,
        second: DiskName,
        second_by_id: ByIdPath,
        device: String,
    },
    #[error(
        "disk '{name}' ({by_id}) is the partition {device}, not a whole disk. braid \
         formats whole disks: pass the disk's own by-id link, not a partition's."
    )]
    Partition {
        name: DiskName,
        by_id: ByIdPath,
        device: String,
    },
    /// The check could not get an answer. A wrong answer strands a journal,
    /// so braid refuses instead of guessing
    /// (docs/dev/safety-heuristics.md).
    #[error("cannot prove disk '{name}' ({by_id}) is a whole disk: {reason}")]
    Unproven {
        name: DiskName,
        by_id: ByIdPath,
        reason: String,
    },
}

/// Refuse unless the specs name distinct whole disks. `add` passes every
/// spec and `replace` its new spec, before any probe, so `--dry-run` refuses
/// too and nothing is written. A link that does not exist is keyed by its own
/// text and skips the sysfs proof, so the probe's "not present" refusal still
/// owns an absent disk. Two specs on one device refuse before the proof, so
/// the operator gets the "same disk" message, not a partition or unproven one.
pub fn check_whole_disk_targets<F: Filesystem + ?Sized>(
    fs: &F,
    resolver: &dyn BackingPathResolver,
    specs: &[(DiskName, ByIdPath)],
) -> Result<(), TargetDiskError> {
    let mut by_device: HashMap<String, (&DiskName, &ByIdPath)> = HashMap::new();
    let mut present: Vec<(&DiskName, &ByIdPath, String)> = Vec::new();
    for (name, by_id) in specs {
        let device = match resolver.canonicalize(by_id.as_str()) {
            Ok(device) => Some(device),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => {
                return Err(TargetDiskError::Unproven {
                    name: name.clone(),
                    by_id: by_id.clone(),
                    reason: format!("cannot resolve the link: {e}"),
                });
            }
        };
        let key = device.clone().unwrap_or_else(|| by_id.as_str().to_owned());
        if let Some((first, first_by_id)) = by_device.get(&key) {
            return Err(if *first_by_id == by_id {
                TargetDiskError::SameLink {
                    first: (*first).clone(),
                    second: name.clone(),
                    by_id: by_id.clone(),
                }
            } else {
                TargetDiskError::SameDevice {
                    first: (*first).clone(),
                    first_by_id: (*first_by_id).clone(),
                    second: name.clone(),
                    second_by_id: by_id.clone(),
                    device: key,
                }
            });
        }
        by_device.insert(key, (name, by_id));
        if let Some(device) = device {
            present.push((name, by_id, device));
        }
    }

    for (name, by_id, device) in present {
        prove_whole_disk(fs, &device).map_err(|refusal| match refusal {
            Refusal::Partition => TargetDiskError::Partition {
                name: name.clone(),
                by_id: by_id.clone(),
                device,
            },
            Refusal::Unproven(reason) => TargetDiskError::Unproven {
                name: name.clone(),
                by_id: by_id.clone(),
                reason,
            },
        })?;
    }
    Ok(())
}

/// What `prove_whole_disk` found instead of a whole disk.
enum Refusal {
    Partition,
    Unproven(String),
}

/// Ask sysfs whether the kernel device at `device` is a whole disk: its
/// entry must exist and have no `partition` file. Any other answer refuses.
fn prove_whole_disk<F: Filesystem + ?Sized>(fs: &F, device: &str) -> Result<(), Refusal> {
    let Some(kernel_name) = Path::new(device).file_name().and_then(|n| n.to_str()) else {
        return Err(Refusal::Unproven(format!(
            "the link resolves to {device}, which names no block device"
        )));
    };
    let entry = format!("{SYS_CLASS_BLOCK}/{kernel_name}");
    if !fs.exists(&entry) {
        return Err(Refusal::Unproven(format!(
            "the link resolves to {device}, which has no entry {entry}"
        )));
    }
    let partition = format!("{entry}/partition");
    match fs.read_to_string(&partition) {
        Ok(_) => Err(Refusal::Partition),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Refusal::Unproven(format!("cannot read {partition}: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{MockBackingPathResolver, MockFs};

    fn spec(name: &str, by_id: &str) -> (DiskName, ByIdPath) {
        (
            DiskName::parse(name).unwrap(),
            ByIdPath::parse(by_id).unwrap(),
        )
    }

    fn check(
        fs: &MockFs,
        resolver: &MockBackingPathResolver,
        specs: &[(&str, &str)],
    ) -> Result<(), String> {
        let specs: Vec<_> = specs.iter().map(|(n, b)| spec(n, b)).collect();
        check_whole_disk_targets(fs, resolver, &specs).map_err(|e| e.to_string())
    }

    const ATA: &str = "/dev/disk/by-id/ata-X";
    const ATA_PART1: &str = "/dev/disk/by-id/ata-X-part1";

    #[test]
    // Intent: two links that resolve to two whole disks pass.
    // Why it exists: the check must refuse only real overlaps and partitions,
    //   or it blocks the common add of two new disks.
    // Scenario: the operator adds two fresh disks, each by its own ata- link.
    fn whole_disks_on_distinct_devices_pass() {
        let fs = MockFs::unmounted(vec![
            "/sys/class/block/sdb".into(),
            "/sys/class/block/sdc".into(),
        ]);
        let resolver = MockBackingPathResolver::default()
            .with_path(ATA, "/dev/sdb")
            .with_path("/dev/disk/by-id/ata-Y", "/dev/sdc");
        assert_eq!(
            check(
                &fs,
                &resolver,
                &[("a", ATA), ("b", "/dev/disk/by-id/ata-Y")]
            ),
            Ok(())
        );
    }

    #[test]
    // Intent: a link that does not exist skips the sysfs proof.
    // Why it exists: the probe's "not present" refusal owns an absent disk,
    //   and an "unproven" refusal there would hide the real cause.
    // Scenario: the operator names a disk that is not plugged in.
    fn absent_link_skips_the_sysfs_proof() {
        let fs = MockFs::unmounted(vec![]);
        let resolver = MockBackingPathResolver::default().with_error(ATA, ErrorKind::NotFound);
        assert_eq!(check(&fs, &resolver, &[("a", ATA)]), Ok(()));
    }

    #[test]
    // Intent: one link named twice refuses with the "both name" message, before
    //   any sysfs read.
    // Why it exists: a raw duplicate once slipped past validation and failed
    //   after the journal; the empty MockFs pins that the duplicate wins over
    //   an "unproven" refusal.
    // Scenario: the operator types `a=/dev/disk/by-id/X b=/dev/disk/by-id/X`.
    fn same_link_twice_refuses_before_any_sysfs_read() {
        let fs = MockFs::unmounted(vec![]);
        let resolver = MockBackingPathResolver::default().with_path(ATA, "/dev/sdb");
        assert_eq!(
            check(&fs, &resolver, &[("a", ATA), ("b", ATA)]),
            Err(format!(
                "disks 'a' and 'b' both name {ATA}. Pass each disk once."
            ))
        );
    }

    #[test]
    // Intent: one absent link named twice still refuses as a duplicate.
    // Why it exists: an absent link is keyed by its own text, and that key
    //   must still catch a raw duplicate, not leave it to the probe.
    // Scenario: the operator names an unplugged disk twice in one add.
    fn same_absent_link_twice_refuses() {
        let fs = MockFs::unmounted(vec![]);
        let resolver = MockBackingPathResolver::default().with_error(ATA, ErrorKind::NotFound);
        assert_eq!(
            check(&fs, &resolver, &[("a", ATA), ("b", ATA)]),
            Err(format!(
                "disks 'a' and 'b' both name {ATA}. Pass each disk once."
            ))
        );
    }

    #[test]
    // Intent: a whole disk and one of its partitions in one command refuse.
    // Why it exists: they resolve to different devices, so device equality
    //   alone misses them, and the kernel refuses the second format while the
    //   first holds the disk -- after the journal.
    // Scenario: the operator pastes both ata-X and ata-X-part1 from
    //   `ls -l /dev/disk/by-id`.
    fn whole_disk_plus_its_partition_refuses_the_partition() {
        let fs =
            MockFs::unmounted(vec!["/sys/class/block/sdb".into()]).with_sysfs_partition("sdb1");
        let resolver = MockBackingPathResolver::default()
            .with_path(ATA, "/dev/sdb")
            .with_path(ATA_PART1, "/dev/sdb1");
        assert_eq!(
            check(&fs, &resolver, &[("a", ATA), ("b", ATA_PART1)]),
            Err(format!(
                "disk 'b' ({ATA_PART1}) is the partition /dev/sdb1, not a whole disk. braid \
                 formats whole disks: pass the disk's own by-id link, not a partition's."
            ))
        );
    }

    #[test]
    // Intent: a device without a sysfs entry refuses as unproven.
    // Why it exists: without the kernel's answer braid cannot rule out a
    //   partition, and a wrong guess strands a journal.
    // Scenario: a link resolves to a device whose sysfs name differs from its
    //   /dev name, such as a cciss disk.
    fn device_without_a_sysfs_entry_refuses_as_unproven() {
        let fs = MockFs::unmounted(vec![]);
        let resolver = MockBackingPathResolver::default().with_path(ATA, "/dev/cciss/c0d0");
        assert_eq!(
            check(&fs, &resolver, &[("a", ATA)]),
            Err(format!(
                "cannot prove disk 'a' ({ATA}) is a whole disk: the link resolves to \
                 /dev/cciss/c0d0, which has no entry /sys/class/block/c0d0"
            ))
        );
    }

    #[test]
    // Intent: a partition file that fails to read for any reason but NotFound
    //   refuses as unproven.
    // Why it exists: only NotFound proves a whole disk; treating any read
    //   error as "no partition file" would admit a partition and strand a
    //   journal.
    // Scenario: a hardened host denies reads under /sys/class/block.
    fn unreadable_partition_file_refuses_as_unproven() {
        let fs = MockFs::unmounted(vec!["/sys/class/block/sdb".into()]).with_read_error(
            "/sys/class/block/sdb/partition",
            ErrorKind::PermissionDenied,
        );
        let resolver = MockBackingPathResolver::default().with_path(ATA, "/dev/sdb");
        let err = check(&fs, &resolver, &[("a", ATA)]).unwrap_err();
        assert!(
            err.starts_with(&format!(
                "cannot prove disk 'a' ({ATA}) is a whole disk: cannot read \
                 /sys/class/block/sdb/partition: "
            )),
            "got: {err}"
        );
    }

    #[test]
    // Intent: a link that fails to resolve for any reason but NotFound
    //   refuses as unproven.
    // Why it exists: without the resolved device braid cannot compare targets
    //   or ask sysfs, so it must refuse, not fall back to the raw text.
    // Scenario: the operator runs braid where it cannot read /dev/disk/by-id.
    fn unresolvable_link_refuses_as_unproven() {
        let fs = MockFs::unmounted(vec![]);
        let resolver =
            MockBackingPathResolver::default().with_error(ATA, ErrorKind::PermissionDenied);
        let err = check(&fs, &resolver, &[("a", ATA)]).unwrap_err();
        assert!(
            err.starts_with(&format!(
                "cannot prove disk 'a' ({ATA}) is a whole disk: cannot resolve the link: "
            )),
            "got: {err}"
        );
    }
}
