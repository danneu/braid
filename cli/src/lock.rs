use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::{mapper_name, Config, ConfigError};
use crate::probe::Filesystem;

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("{0}")]
    Failed(String),
}

/// Status line tag for output.
fn tag(label: &str) -> String {
    format!("[{:<4}]", label)
}

pub fn cmd_lock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
) -> Result<(), LockError> {
    let mount_point = config.mount_point();

    // 1. Check if pool is mounted
    let mp_result = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.to_owned(),
    })?;
    let pool_was_mounted = mp_result.exit_status == 0;

    // 2. If mounted → unmount
    if pool_was_mounted {
        let umount_result = runner.run(&CmdRequest::Umount {
            mount_point: mount_point.to_owned(),
        })?;
        if umount_result.exit_status != 0 {
            return Err(LockError::Failed(format!(
                "umount {mount_point} failed (exit {}): {}",
                umount_result.exit_status,
                umount_result.stderr.trim()
            )));
        }
        eprintln!("{}  {:<14}unmounted {}", tag("ok"), "pool", mount_point);
    }

    // 3. Close each mapper
    let mut all_already_closed = true;
    for key in config.disks().keys() {
        let mn = mapper_name(key);
        let mapper_path = format!("/dev/mapper/{}", mn.0);

        if fs.exists(&mapper_path) {
            let close_result = runner.run(&CmdRequest::CryptsetupClose {
                mapper: mn.0.clone(),
            })?;
            if close_result.exit_status != 0 {
                return Err(LockError::Failed(format!(
                    "cryptsetup close {} failed (exit {}): {}",
                    mn.0,
                    close_result.exit_status,
                    close_result.stderr.trim()
                )));
            }
            eprintln!("{}  disk: {:<7}locked", tag("ok"), key);
            all_already_closed = false;
        } else {
            eprintln!("{}  disk: {:<7}already closed", tag("ok"), key);
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
    use crate::config::{Config, DiskConfig};
    use crate::types::ByIdPath;
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
        let mut disks = BTreeMap::new();
        disks.insert(
            "aaa".to_owned(),
            DiskConfig {
                by_id: ByIdPath("/dev/disk/by-id/a".to_owned()),
            },
        );
        disks.insert(
            "bbb".to_owned(),
            DiskConfig {
                by_id: ByIdPath("/dev/disk/by-id/b".to_owned()),
            },
        );
        Config::new(disks, "/mnt/storage".to_owned()).unwrap()
    }

    #[test]
    fn lock_happy_path_unmounts_and_closes() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".into(),
                },
                ok_raw("mountpoint -q /mnt/storage"),
            )
            .with_output(
                CmdRequest::Umount {
                    mount_point: "/mnt/storage".into(),
                },
                ok_raw("umount /mnt/storage"),
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

        cmd_lock(&runner, &fs, &config).expect("lock should succeed");
    }

    #[test]
    fn lock_already_locked() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: "/mnt/storage".into(),
            },
            err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs = MockFs::new(&[]);
        let config = test_config();

        cmd_lock(&runner, &fs, &config).expect("lock should succeed (already locked)");
    }

    #[test]
    fn lock_partial_state() {
        // Pool not mounted, only aaa mapper open
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".into(),
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

        cmd_lock(&runner, &fs, &config).expect("lock should succeed (partial)");
    }

    #[test]
    fn lock_umount_busy_fails() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".into(),
                },
                ok_raw("mountpoint -q /mnt/storage"),
            )
            .with_output(
                CmdRequest::Umount {
                    mount_point: "/mnt/storage".into(),
                },
                err_raw("umount /mnt/storage", 32, "target is busy"),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();

        let err = cmd_lock(&runner, &fs, &config).expect_err("should fail on busy");
        assert!(err.to_string().contains("target is busy"));
    }
}
