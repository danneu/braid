//! The LUKS identity of an open mapper, read from the kernel's
//! device-mapper table in sysfs.
//!
//! cryptsetup records the LUKS UUID in the dm UUID when it activates a
//! mapping, and the kernel keeps that UUID for as long as the mapping exists,
//! with or without its backing disk (see `cli/src/parse/dm_uuid.rs`). So every
//! "which LUKS container is this open mapper" question reads it here, and a
//! gone or unreadable disk cannot hide a mapper's identity. Questions about a
//! disk itself (by-id probes, format and recovery checks) still read the disk's
//! header with `cryptsetup luksUUID`.

use crate::filesystem::Filesystem;
use crate::parse::{DmUuid, parse_dm_uuid};
use crate::types::{LuksUuid, MapperName};
use std::io::ErrorKind;

/// sysfs directory holding one `dm-N` entry per device-mapper device.
const SYS_BLOCK: &str = "/sys/block";

/// Why an open mapper's LUKS identity is not proven. Every caller treats
/// each case as unverifiable; the variants keep the "no such mapping" answer
/// distinct for callers to whom absence is a normal outcome.
#[derive(Debug, thiserror::Error)]
pub(crate) enum UnprovenMapper {
    /// No device-mapper device has this name.
    #[error("no device-mapper device by that name")]
    Absent,
    /// An active device whose dm UUID is not a cryptsetup LUKS UUID.
    #[error("device-mapper UUID {dm_uuid:?} is not a cryptsetup LUKS UUID")]
    NotLuks { dm_uuid: String },
    /// The sysfs device-mapper table could not be read.
    #[error("cannot read {path}: {source}")]
    Unreadable {
        path: String,
        source: std::io::Error,
    },
}

/// The LUKS UUID cryptsetup recorded for `mapper` when it activated it.
/// Reads `/sys/block/dm-*/dm/name` to find the device, then its `dm/uuid`, so
/// a mapper's name and identity always come from the same `dm-N` entry. A
/// device that disappears between the listing and the reads is skipped, as
/// it is no longer active.
pub(crate) fn mapper_luks_uuid<F: Filesystem + ?Sized>(
    fs: &F,
    mapper: &MapperName,
) -> Result<LuksUuid, UnprovenMapper> {
    named_luks_uuid(fs, mapper.as_str())
}

/// `mapper_luks_uuid` for a device-mapper name read from the kernel, such as
/// the `/dev/mapper/<name>` source of a mount, which need not be a name braid
/// would give a mapper.
pub(crate) fn named_luks_uuid<F: Filesystem + ?Sized>(
    fs: &F,
    name: &str,
) -> Result<LuksUuid, UnprovenMapper> {
    let entries = fs
        .list_dir(SYS_BLOCK)
        .map_err(|source| UnprovenMapper::Unreadable {
            path: SYS_BLOCK.to_owned(),
            source,
        })?;
    for entry in entries.iter().filter(|entry| entry.starts_with("dm-")) {
        let Some(entry_name) = read_attr(fs, entry, "name")? else {
            continue;
        };
        if entry_name.strip_suffix('\n').unwrap_or(&entry_name) != name {
            continue;
        }
        return entry_luks_uuid(fs, entry);
    }
    Err(UnprovenMapper::Absent)
}

/// The LUKS UUID recorded by the device-mapper device whose kernel name is
/// `entry` (`dm-N`), for a device known by its `/dev/dm-N` node rather than
/// its mapper name.
pub(crate) fn entry_luks_uuid<F: Filesystem + ?Sized>(
    fs: &F,
    entry: &str,
) -> Result<LuksUuid, UnprovenMapper> {
    let Some(dm_uuid) = read_attr(fs, entry, "uuid")? else {
        return Err(UnprovenMapper::Absent);
    };
    match parse_dm_uuid(&dm_uuid) {
        DmUuid::Luks(uuid) => Ok(uuid),
        DmUuid::Other(dm_uuid) => Err(UnprovenMapper::NotLuks { dm_uuid }),
    }
}

/// Read `/sys/block/<entry>/dm/<attr>`; `None` when the device is gone.
fn read_attr<F: Filesystem + ?Sized>(
    fs: &F,
    entry: &str,
    attr: &str,
) -> Result<Option<String>, UnprovenMapper> {
    let path = format!("{SYS_BLOCK}/{entry}/dm/{attr}");
    match fs.read_to_string(&path) {
        Ok(body) => Ok(Some(body)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(source) => Err(UnprovenMapper::Unreadable { path, source }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::MockFs;

    const UUID: &str = "11111111-2222-3333-4444-555555555555";

    fn mapper(name: &str) -> MapperName {
        MapperName::from_basename(name.to_owned())
    }

    #[test]
    fn finds_the_named_mapping_among_others() {
        let fs = MockFs::storage(vec![])
            .with_raw_dm("vg-root", "LVM-abc")
            .with_luks_dm("braid-disk2", "99999999-9999-9999-9999-999999999999")
            .with_luks_dm("braid-disk1", UUID);

        assert_eq!(
            mapper_luks_uuid(&fs, &mapper("braid-disk1")).unwrap(),
            LuksUuid::parse(UUID).unwrap()
        );
    }

    #[test]
    fn dm_entry_yields_its_luks_uuid() {
        let fs = MockFs::storage(vec![])
            .with_raw_dm("vg-root", "LVM-abc")
            .with_luks_dm("braid-disk1", UUID);

        assert_eq!(
            entry_luks_uuid(&fs, "dm-1").unwrap(),
            LuksUuid::parse(UUID).unwrap()
        );
        assert!(matches!(
            entry_luks_uuid(&fs, "dm-0"),
            Err(UnprovenMapper::NotLuks { .. })
        ));
        assert!(matches!(
            entry_luks_uuid(&fs, "dm-7"),
            Err(UnprovenMapper::Absent)
        ));
    }

    #[test]
    fn unknown_name_is_absent() {
        let fs = MockFs::storage(vec![]).with_luks_dm("braid-disk1", UUID);

        assert!(matches!(
            mapper_luks_uuid(&fs, &mapper("braid-WRONG")),
            Err(UnprovenMapper::Absent)
        ));
    }

    #[test]
    fn non_luks_mapping_is_not_luks() {
        let fs = MockFs::storage(vec![]).with_raw_dm("braid-disk1", "CRYPT-PLAIN-braid-disk1");

        match mapper_luks_uuid(&fs, &mapper("braid-disk1")) {
            Err(UnprovenMapper::NotLuks { dm_uuid }) => {
                assert_eq!(dm_uuid, "CRYPT-PLAIN-braid-disk1");
            }
            other => panic!("expected NotLuks, got {other:?}"),
        }
    }

    // Intent: a sysfs read error other than NotFound fails the lookup instead
    //   of being read as "no such mapping".
    // Why it exists: absence is a normal answer to some callers (a post-commit
    //   close treats it as already closed), so an unreadable table must not
    //   masquerade as one.
    // Scenario: listing `/sys/block` fails with a permission error.
    #[test]
    fn unreadable_table_is_an_error_not_absence() {
        struct DeniedSysBlock;
        impl Filesystem for DeniedSysBlock {
            fn exists(&self, _path: &str) -> bool {
                false
            }
            fn is_block_device(&self, _path: &str) -> bool {
                false
            }
            fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
                assert_eq!(path, SYS_BLOCK);
                Err(std::io::Error::new(ErrorKind::PermissionDenied, "denied"))
            }
            fn read_to_string(&self, _path: &str) -> Result<String, std::io::Error> {
                unreachable!("nothing to read after a failed listing")
            }
            fn claim_exclusive(&self, _path: &str) -> Result<(), std::io::Error> {
                unreachable!(
                    "DeniedSysBlock: never formats a disk; claim_exclusive must never be called"
                )
            }
            fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
                unreachable!("read-only lookup")
            }
        }

        match mapper_luks_uuid(&DeniedSysBlock, &mapper("braid-disk1")) {
            Err(UnprovenMapper::Unreadable { path, source }) => {
                assert_eq!(path, SYS_BLOCK);
                assert_eq!(source.kind(), ErrorKind::PermissionDenied);
            }
            other => panic!("expected Unreadable, got {other:?}"),
        }
    }
}
