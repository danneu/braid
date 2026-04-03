use crate::cmd::{CmdError, CmdRequest, CommandRunner, Step};
use crate::config::{mapper_name, Config};
use crate::luks::{self, LuksError};
use crate::membership::PoolMembership;
use crate::probe::{self, Filesystem, ProbeError};
use crate::types::{ByIdPath, ConfigDiskState, MountPoint};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("{0}")]
    Probe(#[from] ProbeError),
    #[error("{0}")]
    Luks(#[from] LuksError),
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Failed(String),
    #[error("{0}")]
    MountFailed(String),
    #[error("{0}")]
    DegradedRefused(String),
}

/// Credential source for opening LUKS devices.
pub enum Credential<'a> {
    Passphrase {
        passphrase_stdin: bool,
        passphrase_file: Option<&'a std::path::Path>,
    },
    KeyFile(&'a std::path::Path),
}

/// Status line tag for output.
fn tag(label: &str) -> String {
    format!("[{:<4}]", label)
}

/// Result of the read-only probe + validate phase.
pub struct OpenPlan {
    /// Disks that need LUKS open (name, by_id pairs).
    pub to_unlock: Vec<(String, ByIdPath)>,
    /// At least one mapper was already open.
    pub any_open: bool,
    /// At least one membership disk was absent/damaged.
    pub any_missing_member: bool,
    /// Device path to use for mount (e.g. "/dev/mapper/braid-disk1").
    pub mount_device: String,
}

/// Probe membership disks, validate UUIDs, check degraded policy.
/// Returns the same errors that open_and_mount_pool() would.
/// No mutations — safe for dry-run.
///
/// Returns `Ok(None)` when pool is already mounted.
pub fn plan_open_pool<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    allow_degraded: bool,
    command_hint: &str,
) -> Result<Option<OpenPlan>, MountError> {
    let mount_point = config.mount_point();

    // 1. If pool already mounted → None
    let mp_result = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.clone(),
    })?;
    if mp_result.exit_status == 0 {
        eprintln!("pool already mounted at {mount_point}");
        return Ok(None);
    }

    // 2. Probe each membership disk
    let mut to_unlock = Vec::new();
    let mut any_open = false;
    let mut any_missing_member = false;

    for (name, member) in &membership.disks {
        let probed = probe::probe_config_disk(runner, fs, name, &member.by_id)?;
        match &probed.state {
            ConfigDiskState::Absent => {
                eprintln!("{}  disk: {:<10}not found (unplugged?)", tag("skip"), name);
                any_missing_member = true;
            }
            ConfigDiskState::PresentNotLuks => {
                eprintln!("{}  disk: {:<10}LUKS header damaged", tag("skip"), name);
                any_missing_member = true;
            }
            ConfigDiskState::PresentLuks { uuid, mapper_open } => {
                if let Some(expected) = &member.luks_uuid {
                    if expected != uuid {
                        return Err(MountError::Failed(format!(
                            "disk '{}' LUKS UUID mismatch at {}:\n  \
                             expected  {}\n  \
                             found     {}",
                            name, member.by_id, expected, uuid
                        )));
                    }
                }

                if *mapper_open {
                    eprintln!("{}  disk: {:<10}already open", tag("ok"), name);
                    any_open = true;
                } else {
                    eprintln!("{}  disk: {:<10}found", tag("ok"), name);
                    to_unlock.push((name.clone(), member.by_id.clone()));
                }
            }
        }
    }

    // 3. If no disks to unlock AND none already open → error
    if to_unlock.is_empty() && !any_open {
        return Err(MountError::Failed("no unlockable disks found".into()));
    }

    // 4. Degraded check (before any mutations)
    if any_missing_member && !allow_degraded {
        return Err(MountError::DegradedRefused(format!(
            "pool has missing devices — refusing to mount degraded\n\
             new writes would have ZERO redundancy (single-profile chunks)\n\
             hint: braid {} --allow-degraded",
            command_hint
        )));
    }

    // 5. Compute mount device
    let mount_key = to_unlock
        .first()
        .map(|(k, _)| k.as_str())
        .or_else(|| membership.disks.keys().next().map(|k| k.as_str()))
        .unwrap_or("unknown");
    let mount_device = format!("/dev/mapper/{}", mapper_name(mount_key).0);

    Ok(Some(OpenPlan {
        to_unlock,
        any_open,
        any_missing_member,
        mount_device,
    }))
}

/// Compile dry-run steps from a validated OpenPlan.
pub fn compile_open_steps(
    plan: &OpenPlan,
    mount_point: &MountPoint,
    key_file: Option<&Path>,
) -> Vec<Step> {
    let mut steps = Vec::new();

    for (name, by_id) in &plan.to_unlock {
        let mn = mapper_name(name);
        if let Some(kf) = key_file {
            steps.push(Step {
                risk: "safe",
                description: format!("LUKS open {} → {}", by_id, mn),
                commands: vec![CmdRequest::CryptsetupLuksOpenKeyFile {
                    device: by_id.0.clone(),
                    mapper: mn.0.clone(),
                    key_file_path: kf.display().to_string(),
                }],
            });
        } else {
            steps.push(Step {
                risk: "safe",
                description: format!("LUKS open {} → {}", by_id, mn),
                commands: vec![CmdRequest::CryptsetupLuksOpen {
                    device: by_id.0.clone(),
                    mapper: mn.0.clone(),
                }],
            });
        }
    }

    steps.push(Step {
        risk: "safe",
        description: "btrfs device scan".into(),
        commands: vec![CmdRequest::BtrfsDeviceScanAll],
    });

    if plan.any_missing_member {
        steps.push(Step {
            risk: "safe",
            description: format!("mount → {} (degraded)", mount_point),
            commands: vec![CmdRequest::MountWithOptions {
                device: plan.mount_device.clone(),
                mount_point: mount_point.clone(),
                options: vec!["degraded".to_owned()],
            }],
        });
    } else {
        steps.push(Step {
            risk: "safe",
            description: format!("mount → {}", mount_point),
            commands: vec![CmdRequest::Mount {
                device: plan.mount_device.clone(),
                mount_point: mount_point.clone(),
            }],
        });
    }

    steps
}

/// Open LUKS devices from a membership set and mount the btrfs pool.
///
/// Steps: plan (probe + validate), verify credentials, open LUKS, btrfs
/// device scan, mkdir + mount.
///
/// Returns `Ok(true)` if the mount was performed, `Ok(false)` if already mounted.
/// `command_hint` is used in the `--allow-degraded` hint message (e.g. "unlock" or "recover").
pub fn open_and_mount_pool<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    credential: Credential<'_>,
    allow_degraded: bool,
    command_hint: &str,
) -> Result<bool, MountError> {
    let mount_point = config.mount_point();

    // 1. Plan: probe + validate (read-only)
    let plan = match plan_open_pool(runner, fs, config, membership, allow_degraded, command_hint)? {
        Some(p) => p,
        None => return Ok(false), // already mounted
    };

    // 2. If disks need opening → verify credential, then open each disk
    if !plan.to_unlock.is_empty() {
        match &credential {
            Credential::KeyFile(kf) => {
                let (ref first_name, ref first_by_id) = plan.to_unlock[0];
                let ok = luks::verify_key_file(runner, &first_by_id.0, kf)?;
                if !ok {
                    return Err(MountError::Failed(format!(
                        "wrong keyfile (verified against {})",
                        first_name
                    )));
                }

                for (name, by_id) in &plan.to_unlock {
                    luks::ensure_luks_open_with_key_file(runner, fs, name, by_id, kf).map_err(
                        |_| {
                            MountError::Failed(format!(
                                "failed to open disk '{}': keyfile was verified against \
                                 '{}' but rejected here (single-passphrase invariant \
                                 may be violated by external LUKS manipulation)",
                                name, first_name
                            ))
                        },
                    )?;
                    eprintln!("{}  disk: {:<10}unlocked", tag("ok"), name);
                }
            }
            Credential::Passphrase {
                passphrase_stdin,
                passphrase_file,
            } => {
                let passphrase = luks::read_passphrase(*passphrase_file, *passphrase_stdin)?;

                let (ref first_name, ref first_by_id) = plan.to_unlock[0];
                let ok = luks::verify_passphrase(runner, &first_by_id.0, &passphrase)?;
                if !ok {
                    return Err(MountError::Failed(format!(
                        "wrong passphrase (verified against {})",
                        first_name
                    )));
                }

                for (name, by_id) in &plan.to_unlock {
                    luks::ensure_luks_open(runner, fs, name, by_id, &passphrase).map_err(|_| {
                        MountError::Failed(format!(
                            "failed to open disk '{}': passphrase was verified \
                                 against '{}' but rejected here (single-passphrase \
                                 invariant may be violated by external LUKS \
                                 manipulation)",
                            name, first_name
                        ))
                    })?;
                    eprintln!("{}  disk: {:<10}unlocked", tag("ok"), name);
                }
            }
        }
    }

    // 3. btrfs device scan
    let scan = runner.run(&CmdRequest::BtrfsDeviceScanAll)?;
    if scan.exit_status != 0 {
        return Err(MountError::Failed(format!(
            "btrfs device scan failed (exit {}): {}",
            scan.exit_status,
            scan.stderr.trim()
        )));
    }

    // 4. mkdir + mount
    let _ = std::fs::create_dir_all(mount_point);

    let mount_result = if plan.any_missing_member {
        runner.run(&CmdRequest::MountWithOptions {
            device: plan.mount_device.clone(),
            mount_point: mount_point.clone(),
            options: vec!["degraded".to_owned()],
        })?
    } else {
        runner.run(&CmdRequest::Mount {
            device: plan.mount_device.clone(),
            mount_point: mount_point.clone(),
        })?
    };

    if mount_result.exit_status != 0 {
        return Err(MountError::MountFailed(format!(
            "mount failed (exit {}): {}",
            mount_result.exit_status,
            mount_result.stderr.trim()
        )));
    }

    eprintln!("{}  {:<10}mounted {}", tag("ok"), "pool", mount_point);

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::config::Config;
    use crate::membership::{DiskMember, PoolMembership};
    use crate::types::{ByIdPath, LuksUuid, MountPoint};
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

        fn read_to_string(&self, _path: &str) -> Result<String, std::io::Error> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
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

    fn two_disk_membership() -> PoolMembership {
        let mut disks = BTreeMap::new();
        for (name, path) in [
            ("disk1", "/dev/disk/by-id/virtio-disk1"),
            ("disk2", "/dev/disk/by-id/virtio-disk2"),
        ] {
            disks.insert(
                name.to_owned(),
                DiskMember::from_by_id(ByIdPath(path.to_owned())),
            );
        }
        PoolMembership { disks }
    }

    fn three_disk_membership() -> PoolMembership {
        let mut disks = BTreeMap::new();
        for (name, path) in [
            ("disk1", "/dev/disk/by-id/virtio-disk1"),
            ("disk2", "/dev/disk/by-id/virtio-disk2"),
            ("disk3", "/dev/disk/by-id/virtio-disk3"),
        ] {
            disks.insert(
                name.to_owned(),
                DiskMember::from_by_id(ByIdPath(path.to_owned())),
            );
        }
        PoolMembership { disks }
    }

    fn luks_uuid_ok(device: &str, uuid: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksUuid {
                device: device.into(),
            },
            RawCommandOutput {
                cmd: "cryptsetup luksUUID".into(),
                stdout: format!("{uuid}\n"),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    /// Intent: When the pool is already mounted, open_and_mount_pool should
    /// return Ok(false) without issuing any LUKS commands.
    ///
    /// Why: Callers use the return value to decide post-mount actions
    /// (e.g. unlock refreshes metadata, recover continues to rebuild).
    ///
    /// Scenario: Pool was previously unlocked and is still mounted. A
    /// redundant mount attempt should be a no-op.
    #[test]
    fn mount_already_mounted_returns_false() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = MockFs::new(&[]);

        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            ok_raw("mountpoint"),
        );

        let result = open_and_mount_pool(
            &runner,
            &fs,
            &config,
            &membership,
            Credential::Passphrase {
                passphrase_stdin: false,
                passphrase_file: None,
            },
            false,
            "unlock",
        );

        assert_eq!(result.unwrap(), false);
    }

    /// Intent: Two healthy disks with LUKS closed should be opened, scanned,
    /// and mounted successfully.
    ///
    /// Why: This is the core happy path that both unlock and recover rely on.
    ///
    /// Scenario: 2-disk RAID1, both present, both LUKS-closed. Passphrase
    /// provided via file. All commands succeed.
    #[test]
    fn mount_two_disk_happy_path() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "aaaaaaaa-1111-2222-3333-444444444444",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("mount"),
            );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = open_and_mount_pool(
            &runner,
            &fs,
            &config,
            &membership,
            Credential::Passphrase {
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
            },
            false,
            "unlock",
        );

        assert_eq!(result.unwrap(), true);
    }

    /// Intent: When a disk is absent and --allow-degraded is passed, the pool
    /// should mount with the degraded option.
    ///
    /// Why: Recovery after interrupted remove may leave a disk absent. The
    /// pool must still be mountable.
    ///
    /// Scenario: 3-disk RAID1, disk3 absent. allow_degraded=true. Mount uses
    /// MountWithOptions with "degraded".
    #[test]
    fn mount_degraded_with_flag() {
        let config = test_config();
        let membership = three_disk_membership();
        // disk3 is absent — not in fs paths
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "aaaaaaaa-1111-2222-3333-444444444444",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            .with_output(
                CmdRequest::MountWithOptions {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                    options: vec!["degraded".to_owned()],
                },
                ok_raw("mount -o degraded"),
            );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = open_and_mount_pool(
            &runner,
            &fs,
            &config,
            &membership,
            Credential::Passphrase {
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
            },
            true,
            "unlock",
        );

        assert_eq!(result.unwrap(), true);
    }

    /// Intent: When a disk is absent and --allow-degraded is NOT passed, the
    /// mount must be refused with a clear error including the command hint.
    ///
    /// Why: Principle 1 requires explicit opt-in for degraded mounts.
    ///
    /// Scenario: 3-disk RAID1, disk3 absent, allow_degraded=false. The error
    /// must mention "braid recover --allow-degraded" when command_hint is "recover".
    #[test]
    fn mount_degraded_refused() {
        let config = test_config();
        let membership = three_disk_membership();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "aaaaaaaa-1111-2222-3333-444444444444",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"));
        // No mount mock — should never reach mount

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = open_and_mount_pool(
            &runner,
            &fs,
            &config,
            &membership,
            Credential::Passphrase {
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
            },
            false,
            "recover",
        );

        let err = result.expect_err("should refuse degraded mount");
        assert!(
            matches!(&err, MountError::DegradedRefused(_)),
            "expected DegradedRefused, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("braid recover --allow-degraded"),
            "hint should reference 'braid recover --allow-degraded', got: {msg}"
        );
    }

    /// Intent: When a passphrase is verified against disk1 but rejected by
    /// disk2, the error must name both disks.
    ///
    /// Why: The single-passphrase invariant (Principle 4) may be violated by
    /// external LUKS manipulation. The error must help the user identify which
    /// disk is different.
    ///
    /// Scenario: 2-disk RAID1, passphrase verified on disk1, disk2 rejects it.
    #[test]
    fn mount_passphrase_mismatch_names_disk() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "aaaaaaaa-1111-2222-3333-444444444444",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                err_raw(
                    "cryptsetup open",
                    5,
                    "No key available with this passphrase.",
                ),
            );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            tmp.as_file().write_all(b"testpass").unwrap();
        }

        let result = open_and_mount_pool(
            &runner,
            &fs,
            &config,
            &membership,
            Credential::Passphrase {
                passphrase_stdin: false,
                passphrase_file: Some(tmp.path()),
            },
            false,
            "unlock",
        );

        let err = result.expect_err("should fail when disk2 rejects passphrase");
        let msg = err.to_string();
        assert!(
            msg.contains("disk2"),
            "error should name the failing disk, got: {msg}"
        );
        assert!(
            msg.contains("disk1"),
            "error should name the verification disk, got: {msg}"
        );
    }

    /// Intent: When all disks are absent and none are already open, the helper
    /// must return a clear error.
    ///
    /// Why: Cannot mount what doesn't exist.
    ///
    /// Scenario: 2-disk pool, both disks unplugged.
    #[test]
    fn mount_no_unlockable_disks() {
        let config = test_config();
        let membership = two_disk_membership();
        let fs = MockFs::new(&[]); // no devices present

        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("mountpoint", 1, ""),
        );

        let result = open_and_mount_pool(
            &runner,
            &fs,
            &config,
            &membership,
            Credential::Passphrase {
                passphrase_stdin: false,
                passphrase_file: None,
            },
            false,
            "unlock",
        );

        let err = result.expect_err("should fail with no unlockable disks");
        let msg = err.to_string();
        assert!(
            msg.contains("no unlockable disks"),
            "expected 'no unlockable disks', got: {msg}"
        );
    }

    /// Intent: When all LUKS mappers are already open, the helper should skip
    /// passphrase prompting and proceed directly to scan + mount.
    ///
    /// Why: Idempotency. User may have partially recovered manually before
    /// running braid recover.
    ///
    /// Scenario: 2-disk pool, both mappers already open, pool not yet mounted.
    #[test]
    fn mount_skip_already_open() {
        let config = test_config();
        let membership = two_disk_membership();
        // Devices exist and mappers exist
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "aaaaaaaa-1111-2222-3333-444444444444",
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out)
            // No passphrase or LUKS open mocks — should not be called
            .with_output(CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw("mount"),
            );

        let result = open_and_mount_pool(
            &runner,
            &fs,
            &config,
            &membership,
            Credential::Passphrase {
                passphrase_stdin: false,
                passphrase_file: None,
            },
            false,
            "unlock",
        );

        assert_eq!(result.unwrap(), true);
    }

    /// Intent: When a disk's probed LUKS UUID doesn't match pool.json's stored
    /// UUID, unlock must fatally error before attempting to open the device.
    ///
    /// Why: A UUID mismatch means the physical drive has been swapped,
    /// reformatted, or corrupted. Proceeding would mount the wrong data.
    ///
    /// Scenario: 2-disk RAID1. disk1 has a stored luks_uuid from a prior
    /// unlock, but the device now reports a different UUID (drive was swapped).
    /// Both LUKS devices are closed.
    #[test]
    fn mount_luks_uuid_mismatch_closed() {
        let config = test_config();
        let mut membership = two_disk_membership();
        membership.disks.get_mut("disk1").unwrap().luks_uuid =
            Some(LuksUuid("aaaaaaaa-1111-2222-3333-444444444444".into()));

        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "ffffffff-ffff-ffff-ffff-ffffffffffff", // different from stored
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out);

        let result = open_and_mount_pool(
            &runner,
            &fs,
            &config,
            &membership,
            Credential::Passphrase {
                passphrase_stdin: false,
                passphrase_file: None,
            },
            false,
            "unlock",
        );

        let err = result.expect_err("should fail on LUKS UUID mismatch");
        let msg = err.to_string();
        assert!(
            msg.contains("disk1"),
            "error should name the disk, got: {msg}"
        );
        assert!(
            msg.contains("aaaaaaaa"),
            "error should show expected UUID, got: {msg}"
        );
        assert!(
            msg.contains("ffffffff"),
            "error should show found UUID, got: {msg}"
        );
    }

    /// Intent: UUID mismatch must be caught even when the LUKS mapper is
    /// already open (e.g. from a previous partial unlock or manual intervention).
    ///
    /// Why: The check must fire in both PresentLuks branches — mapper_open
    /// status doesn't make a swapped drive safe.
    ///
    /// Scenario: Same as mount_luks_uuid_mismatch_closed, but disk1's mapper
    /// is already open.
    #[test]
    fn mount_luks_uuid_mismatch_already_open() {
        let config = test_config();
        let mut membership = two_disk_membership();
        membership.disks.get_mut("disk1").unwrap().luks_uuid =
            Some(LuksUuid("aaaaaaaa-1111-2222-3333-444444444444".into()));

        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1", // mapper already open
        ]);

        let (uuid1_req, uuid1_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk1",
            "ffffffff-ffff-ffff-ffff-ffffffffffff", // different from stored
        );
        let (uuid2_req, uuid2_out) = luks_uuid_ok(
            "/dev/disk/by-id/virtio-disk2",
            "bbbbbbbb-1111-2222-3333-444444444444",
        );

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(uuid1_req, uuid1_out)
            .with_output(uuid2_req, uuid2_out);

        let result = open_and_mount_pool(
            &runner,
            &fs,
            &config,
            &membership,
            Credential::Passphrase {
                passphrase_stdin: false,
                passphrase_file: None,
            },
            false,
            "unlock",
        );

        let err = result.expect_err("should fail on LUKS UUID mismatch even with open mapper");
        let msg = err.to_string();
        assert!(
            msg.contains("disk1"),
            "error should name the disk, got: {msg}"
        );
        assert!(
            msg.contains("aaaaaaaa"),
            "error should show expected UUID, got: {msg}"
        );
        assert!(
            msg.contains("ffffffff"),
            "error should show found UUID, got: {msg}"
        );
    }
}
