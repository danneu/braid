use std::thread;
use std::time::Duration;

use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::{mapper_name, name_from_mapper, Config};
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
    #[error("device busy: {0}")]
    DeviceBusy(String),
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
        if !is_busy {
            return Err(LockError::Failed(format!(
                "cryptsetup close {} failed (exit {}): {}",
                mapper,
                result.exit_status,
                result.stderr.trim()
            )));
        }
        if attempt == CLOSE_RETRY_ATTEMPTS {
            return Err(LockError::DeviceBusy(format!(
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
    let mut umount_error: Option<LockError> = None;
    if pool_was_mounted {
        let umount_result = runner.run(&CmdRequest::Umount {
            mount_point: mount_point.clone(),
        })?;
        if umount_result.exit_status != 0 {
            let err = LockError::Failed(format!(
                "umount {mount_point} failed (exit {}): {}\n\
                 hint: a process may be using files on the mount. \
                 Run 'lsof {mount_point}' or 'fuser -vm {mount_point}' to identify it.",
                umount_result.exit_status,
                umount_result.stderr.trim(),
                mount_point = mount_point,
            ));
            eprintln!("[FAIL]  {err}");
            eprintln!("[warn]  attempting to close LUKS mappers despite umount failure...");
            umount_error = Some(err);
        } else {
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
    }

    // 3. Close each mapper
    let mut all_already_closed = true;
    for name in membership.disks.keys() {
        let mn = mapper_name(name);
        let mapper_path = format!("/dev/mapper/{}", mn.0);

        if fs.exists(&mapper_path) {
            match close_mapper_with_retry(runner, &mn.0) {
                Ok(()) => {
                    eprintln!("{}  disk: {:<7}locked", tag("ok"), name);
                }
                Err(LockError::DeviceBusy(msg)) if umount_error.is_some() => {
                    eprintln!(
                        "[warn]  disk: {:<7}close failed (umount was stuck): {}",
                        name, msg
                    );
                }
                Err(e) => return Err(e),
            }
            all_already_closed = false;
        } else {
            eprintln!("{}  disk: {:<7}already closed", tag("ok"), name);
        }
    }

    // 3b. Scan for orphaned braid-* mappers not in membership
    match fs.list_dir("/dev/mapper") {
        Ok(entries) => {
            for entry in entries {
                let Some(disk_name) = name_from_mapper(&entry) else {
                    continue;
                };
                if membership.disks.contains_key(disk_name) {
                    continue;
                }
                if fs.exists(&format!("/dev/mapper/{entry}")) {
                    eprintln!(
                        "[warn]  orphaned mapper {entry} (not in pool.json — likely a prior crash)"
                    );
                    match close_mapper_with_retry(runner, &entry) {
                        Ok(()) => {
                            eprintln!("{}  disk: {:<7}locked (orphan)", tag("ok"), disk_name);
                        }
                        Err(LockError::DeviceBusy(msg)) if umount_error.is_some() => {
                            eprintln!(
                                "[warn]  disk: {:<7}orphan close failed (umount was stuck): {}",
                                disk_name, msg
                            );
                        }
                        Err(e) => return Err(e),
                    }
                    all_already_closed = false;
                }
            }
        }
        Err(e) => {
            eprintln!("[warn]  could not scan /dev/mapper for orphans: {e} (skipping)");
        }
    }

    // 4. Return deferred umount error, if any
    if let Some(err) = umount_error {
        return Err(err);
    }

    // 5. If nothing was done → short message
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
    use crate::membership::{DiskMember, PoolMembership};
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

        fn list_dir(&self, dir: &str) -> Result<Vec<String>, std::io::Error> {
            let prefix = if dir.ends_with('/') {
                dir.to_string()
            } else {
                format!("{dir}/")
            };
            let entries: Vec<String> = self
                .paths
                .iter()
                .filter_map(|p| p.strip_prefix(&prefix).map(|s| s.to_string()))
                .filter(|s| !s.contains('/'))
                .collect();
            Ok(entries)
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
        disks.insert(
            "aaa".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/a".to_owned())),
        );
        disks.insert(
            "bbb".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/b".to_owned())),
        );
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

    // Intent: lock fails when umount reports the mount is busy.
    // Why it exists: a busy mount means the pool cannot be cleanly locked;
    //   reporting success would be a lie.
    // Scenario: a process holds a file open on /mnt/storage; umount returns
    //   "target is busy". lock still attempts mapper close (best-effort), but
    //   ultimately returns the umount error.
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
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                err_raw(
                    "cryptsetup close braid-aaa",
                    5,
                    "Device braid-aaa is still in use.",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                err_raw(
                    "cryptsetup close braid-bbb",
                    5,
                    "Device braid-bbb is still in use.",
                ),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock(&runner, &fs, &config, &membership).expect_err("should fail on busy");
        assert!(err.to_string().contains("target is busy"));
    }

    // Intent: the umount-busy error message includes actionable diagnostic hints.
    // Why it exists: users need to know how to find the blocking process so
    //   they can kill it and retry lock.
    // Scenario: umount fails with "target is busy"; the error message suggests
    //   running lsof or fuser to identify the holder.
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
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                err_raw(
                    "cryptsetup close braid-aaa",
                    5,
                    "Device braid-aaa is still in use.",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                err_raw(
                    "cryptsetup close braid-bbb",
                    5,
                    "Device braid-bbb is still in use.",
                ),
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

    // Intent: orphaned braid-* mappers from prior crashes are cleaned up
    //   during lock.
    // Why it exists: a crash between cryptsetup open and journal/pool.json
    //   write leaves a mapper outside pool.json that the membership loop
    //   won't close.
    // Scenario: power loss during `braid add` after LUKS open but before
    //   pool.json write; next `braid lock` must still close the orphan.
    #[test]
    fn lock_closes_orphaned_mapper() {
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
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-ccc".into(),
                },
                ok_raw("cryptsetup close braid-ccc"),
            );
        // ccc is not in membership but exists as a mapper → orphan
        let fs = MockFs::new(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
        let config = test_config();
        let membership = test_membership();

        cmd_lock(&runner, &fs, &config, &membership).expect("lock should close orphan too");
    }

    // Intent: I/O errors scanning /dev/mapper don't prevent closing known
    //   mappers.
    // Why it exists: /dev/mapper may be unreadable in degraded environments;
    //   the safety-net scan shouldn't break the primary lock path.
    // Scenario: containerized environment where /dev/mapper has restricted
    //   permissions; lock must still close membership-known mappers.
    #[test]
    fn lock_orphan_scan_failure_is_nonfatal() {
        struct FailListDirFs;
        impl Filesystem for FailListDirFs {
            fn exists(&self, path: &str) -> bool {
                path == "/dev/mapper/braid-aaa" || path == "/dev/mapper/braid-bbb"
            }
            fn is_block_device(&self, _path: &str) -> bool {
                false
            }
            fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "permission denied",
                ))
            }
        }

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
        let config = test_config();
        let membership = test_membership();

        cmd_lock(&runner, &FailListDirFs, &config, &membership)
            .expect("lock should succeed despite list_dir failure");
    }

    /// Build a MockRunner pre-loaded with a failed-umount scenario
    /// (mountpoint check = mounted, balance status = no balance, umount = busy).
    /// No BtrfsDeviceScanForget — forget is gated on successful unmount.
    fn umount_failed_runner() -> MockRunner {
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
                err_raw("umount /mnt/storage", 32, "target is busy"),
            )
    }

    // Intent: when umount fails, lock still attempts to close LUKS mappers
    //   and returns the umount error (not a mapper error).
    // Why it exists: the original code returned immediately on umount failure,
    //   leaving all LUKS mappers open — a security gap during shutdown.
    // Scenario: umount fails with "target is busy"; both mapper closes succeed
    //   anyway (e.g. kernel released references between umount and close).
    //   The function still fails with the umount error because the mount is
    //   in an inconsistent state.
    #[test]
    fn lock_umount_fails_but_mappers_close_successfully() {
        let runner = umount_failed_runner()
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

        let err = cmd_lock(&runner, &fs, &config, &membership)
            .expect_err("should fail — umount error is the root cause");
        let msg = err.to_string();
        assert!(
            msg.contains("umount") && msg.contains("target is busy"),
            "expected umount error, got: {msg}"
        );
    }

    // Intent: busy mapper close errors are suppressed (as warnings) when
    //   umount already failed, and the umount error is returned.
    // Why it exists: busy mapper close after a stuck umount is expected —
    //   the filesystem still holds the devices. Surfacing the mapper error
    //   instead of the umount error would obscure the root cause.
    // Scenario: umount fails; both mapper closes fail with "in use" (DeviceBusy).
    //   The returned error is the umount error, not a mapper close error.
    #[test]
    fn lock_umount_fails_busy_mapper_is_warning() {
        let runner = umount_failed_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                err_raw(
                    "cryptsetup close braid-aaa",
                    5,
                    "Device braid-aaa is still in use.",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                err_raw(
                    "cryptsetup close braid-bbb",
                    5,
                    "Device braid-bbb is still in use.",
                ),
            );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock(&runner, &fs, &config, &membership)
            .expect_err("should fail with umount error");
        let msg = err.to_string();
        assert!(
            msg.contains("umount") && msg.contains("target is busy"),
            "expected umount error (not mapper error), got: {msg}"
        );
    }

    // Intent: unexpected (non-busy) mapper close errors remain fatal even when
    //   umount already failed — only DeviceBusy is suppressed.
    // Why it exists: suppressing all mapper close errors after umount failure
    //   would hide real problems like permission errors or missing devices.
    //   Only the expected busy/in-use errors should be downgraded to warnings.
    // Scenario: umount fails; mapper aaa close fails with "Device is not
    //   active." (not a busy error). The non-busy error is returned immediately.
    #[test]
    fn lock_umount_fails_unexpected_mapper_error_is_fatal() {
        let runner = umount_failed_runner().with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-aaa".into(),
            },
            err_raw("cryptsetup close braid-aaa", 5, "Device is not active."),
        );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock(&runner, &fs, &config, &membership)
            .expect_err("should fail with mapper error");
        let msg = err.to_string();
        assert!(
            msg.contains("braid-aaa") && msg.contains("not active"),
            "expected mapper error (not umount error), got: {msg}"
        );
    }

    // Intent: mapper close errors remain fatal when umount succeeded.
    // Why it exists: regression guard — the umount-failure fix must not
    //   accidentally suppress mapper close errors on the normal path.
    // Scenario: umount succeeds; aaa mapper close fails with a non-busy error.
    //   The mapper error is returned.
    #[test]
    fn lock_mapper_close_fatal_when_umount_succeeded() {
        let runner = mounted_runner().with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-aaa".into(),
            },
            err_raw("cryptsetup close braid-aaa", 5, "Device is not active."),
        );
        let fs = MockFs::new(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = test_config();
        let membership = test_membership();

        let err =
            cmd_lock(&runner, &fs, &config, &membership).expect_err("should fail on mapper close");
        let msg = err.to_string();
        assert!(
            msg.contains("braid-aaa"),
            "expected mapper error, got: {msg}"
        );
    }

    // Intent: busy orphan mapper close errors are suppressed when umount
    //   already failed, same as for membership mappers.
    // Why it exists: the membership and orphan close loops are separate code
    //   paths; a bug in orphan handling could slip through even if the
    //   membership tests pass.
    // Scenario: umount fails; membership mappers close ok; orphan mapper
    //   close fails with "in use" (DeviceBusy). The returned error is the
    //   umount error.
    #[test]
    fn lock_umount_fails_orphan_busy_is_warning() {
        let runner = umount_failed_runner()
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
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-ccc".into(),
                },
                err_raw(
                    "cryptsetup close braid-ccc",
                    5,
                    "Device braid-ccc is still in use.",
                ),
            );
        let fs = MockFs::new(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock(&runner, &fs, &config, &membership)
            .expect_err("should fail with umount error");
        let msg = err.to_string();
        assert!(
            msg.contains("umount") && msg.contains("target is busy"),
            "expected umount error (not orphan error), got: {msg}"
        );
    }

    // Intent: unexpected (non-busy) orphan mapper close errors remain fatal
    //   even when umount already failed.
    // Why it exists: the orphan branch must have the same precise suppression
    //   as the membership branch — only DeviceBusy is suppressed.
    // Scenario: umount fails; membership mappers close ok; orphan mapper
    //   close fails with "Device is not active." (non-busy). The orphan
    //   error is returned immediately.
    #[test]
    fn lock_umount_fails_orphan_unexpected_error_is_fatal() {
        let runner = umount_failed_runner()
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
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-ccc".into(),
                },
                err_raw("cryptsetup close braid-ccc", 5, "Device is not active."),
            );
        let fs = MockFs::new(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
        let config = test_config();
        let membership = test_membership();

        let err = cmd_lock(&runner, &fs, &config, &membership)
            .expect_err("should fail with orphan error");
        let msg = err.to_string();
        assert!(
            msg.contains("braid-ccc") && msg.contains("not active"),
            "expected orphan mapper error (not umount error), got: {msg}"
        );
    }

    // Intent: if an orphan mapper is detected but can't be closed, lock must
    //   fail rather than silently leaving LUKS open.
    // Why it exists: a stray open LUKS mapper is a security concern —
    //   reporting success while leaving it open is worse than failing.
    // Scenario: orphan mapper is held open by a leaked process; lock must
    //   surface the failure.
    #[test]
    fn lock_orphan_close_failure_is_fatal() {
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
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-orphan".into(),
                },
                err_raw("cryptsetup close braid-orphan", 5, "Device is not active."),
            );
        let fs = MockFs::new(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-orphan",
        ]);
        let config = test_config();
        let membership = test_membership();

        let err =
            cmd_lock(&runner, &fs, &config, &membership).expect_err("should fail on orphan close");
        assert!(
            err.to_string().contains("braid-orphan"),
            "error should mention the orphan mapper, got: {err}"
        );
    }
}
