use std::path::Path;

use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::sys::stat::Mode;
use std::os::fd::AsRawFd;

use crate::types::Devid;

/// Mirrors `struct btrfs_ioctl_dev_info_args`, the kernel ABI behind
/// `BTRFS_IOC_DEV_INFO`, so replace preflight can read btrfs's own
/// per-device `total_bytes` authority.
#[repr(C)]
pub struct BtrfsIoctlDevInfoArgs {
    pub devid: u64,
    pub uuid: [u8; 16],
    pub bytes_used: u64,
    pub total_bytes: u64,
    pub fsid: [u8; 16],
    pub unused: [u64; 377],
    pub path: [u8; 1024],
}

impl BtrfsIoctlDevInfoArgs {
    fn for_devid(devid: Devid) -> Self {
        Self {
            devid: devid.get(),
            uuid: [0; 16],
            bytes_used: 0,
            total_bytes: 0,
            fsid: [0; 16],
            unused: [0; 377],
            path: [0; 1024],
        }
    }
}

const _: () = assert!(std::mem::size_of::<BtrfsIoctlDevInfoArgs>() == 4096);

nix::ioctl_readwrite!(btrfs_dev_info_raw, 0x94, 30, BtrfsIoctlDevInfoArgs);

/// Errors at the btrfs ioctl boundary, before replace policy translates them
/// into a fail-closed validation refusal.
#[derive(Debug, thiserror::Error)]
pub enum BtrfsIoctlError {
    #[error("open {mount}: {errno}")]
    OpenFailed { mount: String, errno: Errno },
    #[error("devid {devid} was not found in the mounted btrfs filesystem")]
    DevidNotFound { devid: Devid },
    #[error("BTRFS_IOC_DEV_INFO failed for devid {devid}: {errno}")]
    IoctlFailed { devid: Devid, errno: Errno },
}

/// Abstracts the btrfs device-info syscall so replace planning and unit tests
/// share the same source-size contract without shelling out.
pub trait BtrfsDevInfo {
    fn total_bytes(&self, mount: &Path, devid: Devid) -> Result<u64, BtrfsIoctlError>;
}

/// Production btrfs device-info reader backed by `BTRFS_IOC_DEV_INFO` on the
/// mounted filesystem path.
pub struct LinuxBtrfsDevInfo;

impl BtrfsDevInfo for LinuxBtrfsDevInfo {
    fn total_bytes(&self, mount: &Path, devid: Devid) -> Result<u64, BtrfsIoctlError> {
        let fd = open(mount, OFlag::O_RDONLY, Mode::empty()).map_err(|errno| {
            BtrfsIoctlError::OpenFailed {
                mount: mount.display().to_string(),
                errno,
            }
        })?;
        let mut args = BtrfsIoctlDevInfoArgs::for_devid(devid);
        // SAFETY: `fd` is a valid open descriptor for the btrfs mount path,
        // and `args` points to a 4096-byte `btrfs_ioctl_dev_info_args` buffer.
        let result = unsafe { btrfs_dev_info_raw(fd.as_raw_fd(), &mut args) };
        match result {
            Ok(_) => Ok(args.total_bytes),
            Err(Errno::ENODEV) => Err(BtrfsIoctlError::DevidNotFound { devid }),
            Err(errno) => Err(BtrfsIoctlError::IoctlFailed { devid, errno }),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[derive(Debug, Clone, Default)]
    pub(crate) struct MockBtrfsDevInfo {
        map: HashMap<(PathBuf, Devid), u64>,
    }

    impl MockBtrfsDevInfo {
        pub(crate) fn with_total_bytes(
            mut self,
            mount: impl Into<PathBuf>,
            devid: Devid,
            total_bytes: u64,
        ) -> Self {
            self.map.insert((mount.into(), devid), total_bytes);
            self
        }
    }

    impl BtrfsDevInfo for MockBtrfsDevInfo {
        fn total_bytes(&self, mount: &Path, devid: Devid) -> Result<u64, BtrfsIoctlError> {
            self.map
                .get(&(mount.to_path_buf(), devid))
                .copied()
                .ok_or(BtrfsIoctlError::DevidNotFound { devid })
        }
    }

    pub(crate) struct PanicBtrfsDevInfo;

    impl BtrfsDevInfo for PanicBtrfsDevInfo {
        fn total_bytes(&self, mount: &Path, devid: Devid) -> Result<u64, BtrfsIoctlError> {
            panic!(
                "planner-boundary test: BtrfsDevInfo must not be invoked; got mount={} devid={devid}",
                mount.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Intent: the Rust mirror of `btrfs_ioctl_dev_info_args` remains 4096
    //   bytes, matching the kernel ABI that is encoded into the ioctl number.
    // Why it exists: a field-shape drift would generate the wrong
    //   `BTRFS_IOC_DEV_INFO` request number.
    // Scenario: compile and unit-test the ABI guard on every Rust test run.
    fn btrfs_ioctl_dev_info_args_size_is_4kib() {
        assert_eq!(std::mem::size_of::<BtrfsIoctlDevInfoArgs>(), 4096);
    }

    #[test]
    // Intent: the mock btrfs device-info reader returns configured
    //   `(mount, devid)` sizes.
    // Why it exists: replace planning tests depend on source-size fixtures
    //   without invoking a host kernel ioctl.
    // Scenario: `/mnt/storage` devid 2 has a seeded 520093696-byte size.
    fn mock_btrfs_dev_info_returns_configured_total_bytes() {
        let dev_info = tests_support::MockBtrfsDevInfo::default().with_total_bytes(
            "/mnt/storage",
            Devid::new(2),
            520_093_696,
        );
        let got = dev_info
            .total_bytes(Path::new("/mnt/storage"), Devid::new(2))
            .expect("configured devid should resolve");
        assert_eq!(got, 520_093_696);
    }

    #[test]
    // Intent: the mock btrfs device-info reader fails like the real ioctl
    //   boundary for unknown devids.
    // Why it exists: fail-closed replace tests need a deterministic
    //   `DevidNotFound` source-size error.
    // Scenario: no size is seeded for devid 99.
    fn mock_btrfs_dev_info_reports_unconfigured_devid_not_found() {
        let dev_info = tests_support::MockBtrfsDevInfo::default();
        let err = dev_info
            .total_bytes(Path::new("/mnt/storage"), Devid::new(99))
            .expect_err("unconfigured devid should fail");
        assert!(matches!(err, BtrfsIoctlError::DevidNotFound { devid: d } if d == Devid::new(99)));
    }

    #[ignore = "requires BRAID_BTRFS_IOCTL_SMOKE_MOUNT and BRAID_BTRFS_IOCTL_SMOKE_DEVID"]
    #[test]
    // Intent: optional manual coverage for the production ioctl reader
    //   against a real mounted btrfs filesystem.
    // Why it exists: unit tests cover layout and mock behavior, while this
    //   ignored test gives maintainers a local syscall smoke check.
    // Scenario: maintainer sets mount and devid environment variables.
    fn linux_btrfs_dev_info_smoke_test_requires_mounted_btrfs() {
        let mount = std::env::var("BRAID_BTRFS_IOCTL_SMOKE_MOUNT")
            .expect("set BRAID_BTRFS_IOCTL_SMOKE_MOUNT to a mounted btrfs path");
        let devid: u64 = std::env::var("BRAID_BTRFS_IOCTL_SMOKE_DEVID")
            .expect("set BRAID_BTRFS_IOCTL_SMOKE_DEVID to a btrfs devid")
            .parse()
            .expect("BRAID_BTRFS_IOCTL_SMOKE_DEVID must be a u64");
        let total = LinuxBtrfsDevInfo
            .total_bytes(Path::new(&mount), Devid::new(devid))
            .expect("BTRFS_IOC_DEV_INFO should succeed");
        assert!(total > 0, "total_bytes should be positive");
    }
}
