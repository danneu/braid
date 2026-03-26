use std::thread;
use std::time::Duration;

use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::{mapper_name, Config};
use crate::membership::{self, PoolMembership};
use crate::preflight;
use crate::probe::Filesystem;

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Membership(#[from] membership::MembershipError),
    #[error("{0}")]
    Failed(String),
}

/// Status line tag for output.
fn tag(label: &str) -> String {
    format!("[{:<4}]", label)
}

const CLOSE_RETRY_ATTEMPTS: u32 = 3;
const CLOSE_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Close a LUKS mapper, retrying up to 3 times if the error indicates the
/// device is busy. Non-busy errors fail immediately.
fn close_mapper_with_retry<R: CommandRunner>(runner: &R, mapper: &str) -> Result<(), LockError> {
    for attempt in 1..=CLOSE_RETRY_ATTEMPTS {
        let result = runner.run(&CmdRequest::CryptsetupClose {
            mapper: mapper.to_owned(),
        })?;
        if result.exit_status == 0 {
            return Ok(());
        }
        let stderr = result.stderr.to_lowercase();
        let is_busy = stderr.contains("busy") || stderr.contains("in use");
        if !is_busy || attempt == CLOSE_RETRY_ATTEMPTS {
            return Err(LockError::Failed(format!(
                "cryptsetup close {} failed (exit {}): {}",
                mapper,
                result.exit_status,
                result.stderr.trim()
            )));
        }
        eprintln!(
            "[warn]  cryptsetup close {mapper} busy, retrying ({attempt}/{CLOSE_RETRY_ATTEMPTS})..."
        );
        thread::sleep(CLOSE_RETRY_DELAY);
    }
    unreachable!()
}

pub fn cmd_lock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
) -> Result<(), LockError> {
    let mount_point = config.mount_point();

    // 1. Check if pool is mounted
    let mp_result = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.clone(),
    })?;
    let pool_was_mounted = mp_result.exit_status == 0;

    // Preflight
    if pool_was_mounted {
        preflight::check_no_exclusive_op(runner, mount_point.as_str())
            .map_err(LockError::Failed)?;
    }

    // 2. If mounted → unmount
    if pool_was_mounted {
        let umount_result = runner.run(&CmdRequest::Umount {
            mount_point: mount_point.clone(),
        })?;
        if umount_result.exit_status != 0 {
            return Err(LockError::Failed(format!(
                "umount {mount_point} failed (exit {}): {}\n\
                 hint: a process may be using files on the mount. \
                 Run 'lsof {mount_point}' or 'fuser -vm {mount_point}' to identify it.",
                umount_result.exit_status,
                umount_result.stderr.trim(),
                mount_point = mount_point,
            )));
        }
        eprintln!("{}  {:<14}unmounted {}", tag("ok"), "pool", mount_point);

        // Clear btrfs kernel scan registry so that cryptsetup close doesn't
        // race against stale device references on multi-device pools.
        let forget_result = runner.run(&CmdRequest::BtrfsDeviceScanForget);
        match forget_result {
            Ok(r) if r.exit_status == 0 => {}
            Ok(r) => {
                eprintln!(
                    "[warn]  btrfs device scan --forget failed (exit {}): {} (continuing)",
                    r.exit_status,
                    r.stderr.trim()
                );
            }
            Err(e) => {
                eprintln!("[warn]  btrfs device scan --forget failed: {e} (continuing)");
            }
        }
    }

    // 3. Close each mapper
    let mut all_already_closed = true;
    for name in membership.disks.keys() {
        let mn = mapper_name(name);
        let mapper_path = format!("/dev/mapper/{}", mn.0);

        if fs.exists(&mapper_path) {
            close_mapper_with_retry(runner, &mn.0)?;
            eprintln!("{}  disk: {:<7}locked", tag("ok"), name);
            all_already_closed = false;
        } else {
            eprintln!("{}  disk: {:<7}already closed", tag("ok"), name);
        }
    }

    // 4. If nothing was done → short message
    if !pool_was_mounted && all_already_closed {
        eprintln!("pool already locked");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::config::Config;
    use crate::membership::PoolMembership;
    use crate::types::{ByIdPath, MountPoint};
    use std::collections::BTreeMap;

    struct MockFs {
        paths: Vec<String>,
    }

    impl MockFs {
        fn new(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.contains(&path.to_string())
        }

        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
    }

    fn ok_raw(cmd: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
            exit_status: exit_code,
        }
    }

    fn test_config() -> Config {
        Config::new(MountPoint("/mnt/storage".to_owned())).unwrap()
    }

    fn test_membership() -> PoolMembership {
        let mut disks = BTreeMap::new();
        disks.insert("aaa".to_owned(), ByIdPath("/dev/disk/by-id/a".to_owned()));
        disks.insert("bbb".to_owned(), ByIdPath("/dev/disk/by-id/b".to_owned()));
        PoolMembership { disks }
    }

    /// Build a MockRunner pre-loaded with the standard preflight outputs
    /// (mountpoint check = mounted, balance status = no balance, umount = ok,
    /// forget = ok).
    fn mounted_runner() -> MockRunner {
        MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("mountpoint -q /mnt/storage"),
            )
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                RawCommandOutput {
                    cmd: "btrfs balance status /mnt/storage".into(),
                    stdout: "No balance found on '/mnt/storage'\n".into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("umount /mnt/storage"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget,
                ok_raw("btrfs device scan --forget"),
            )
    }

    #[test]
    fn lock_happy_path_unmounts_and_closes() {
        let runner = mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        cmd_lock(&runner, &fs, &config, &membership).expect("lock should succeed");
    }

    #[test]
    fn lock_already_locked() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs = MockFs::new(&[]);
        let config = test_config();
        let membership = test_membership();

        cmd_lock(&runner, &fs, &config, &membership).expect("lock should succeed (already locked)");
    }

    #[test]
    fn lock_partial_state() {
        // Pool not mounted, only aaa mapper open
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint -q /mnt/storage", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa"]);
        let config = test_config();
        let membership = test_membership();

        cmd_lock(&runner, &fs, &config, &membership).expect("lock should succeed (partial)");
    }

    #[test]
    fn lock_umount_busy_fails() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("mountpoint -q /mnt/storage"),
            )
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                RawCommandOutput {
                    cmd: "btrfs balance status /mnt/storage".into(),
                    stdout: "No balance found on '/mnt/storage'\n".into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("umount /mnt/storage", 32, "target is busy"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock(&runner, &fs, &config, &membership).expect_err("should fail on busy");
        assert!(err.to_string().contains("target is busy"));
    }

    #[test]
    fn lock_umount_busy_includes_hint() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("mountpoint -q /mnt/storage"),
            )
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                RawCommandOutput {
                    cmd: "btrfs balance status /mnt/storage".into(),
                    stdout: "No balance found on '/mnt/storage'\n".into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("umount /mnt/storage", 32, "target is busy"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock(&runner, &fs, &config, &membership).expect_err("should fail on busy");
        let msg = err.to_string();
        assert!(
            msg.contains("lsof") && msg.contains("fuser"),
            "expected lsof/fuser hint in error, got: {msg}"
        );
    }

    #[test]
    fn lock_adds_forget_after_umount() {
        let runner = mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        // If BtrfsDeviceScanForget were not called, MockRunner would return
        // MissingMock and the test would fail.
        cmd_lock(&runner, &fs, &config, &membership).expect("lock should succeed with forget");
    }

    #[test]
    fn lock_forget_failure_is_nonfatal() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("mountpoint -q /mnt/storage"),
            )
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                RawCommandOutput {
                    cmd: "btrfs balance status /mnt/storage".into(),
                    stdout: "No balance found on '/mnt/storage'\n".into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("umount /mnt/storage"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget,
                err_raw("btrfs device scan --forget", 1, "some error"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        cmd_lock(&runner, &fs, &config, &membership)
            .expect("lock should succeed even when forget fails");
    }
}
