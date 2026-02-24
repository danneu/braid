use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::Config;
use crate::parse::{parse_cryptsetup_luks_uuid, ParseError};
use crate::plan::mapper_name_for_by_id;
use crate::probe::{probe_pool, Filesystem, ProbeError};
use crate::types::*;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum InitDiskError {
    #[error("{0}")]
    Validation(String),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Entry point: reads env vars, delegates to cmd_init_disk_with.
pub fn cmd_init_disk<R: CommandRunner, F: Filesystem>(
    runner: &R,
    fs: &F,
    config: &Config,
    by_id_path: &str,
    force: bool,
) -> Result<(), InitDiskError> {
    let passphrase = std::env::var("BRAID_PASSPHRASE").unwrap_or_default();
    if passphrase.is_empty() {
        return Err(InitDiskError::Validation(
            "BRAID_PASSPHRASE must be set".to_owned(),
        ));
    }

    let confirm = std::env::var("BRAID_CONFIRM").unwrap_or_default();

    let luks_opts_raw = std::env::var("BRAID_LUKS_OPTS").unwrap_or_default();
    let luks_extra_opts = if luks_opts_raw.is_empty() {
        vec![]
    } else {
        shell_words::split(&luks_opts_raw).map_err(|e| {
            InitDiskError::Validation(format!("failed to parse BRAID_LUKS_OPTS: {e}"))
        })?
    };

    cmd_init_disk_with(
        runner,
        fs,
        config,
        by_id_path,
        force,
        &passphrase,
        &confirm,
        &luks_extra_opts,
    )
}

/// Inner implementation with explicit parameters (testable without env vars).
pub(crate) fn cmd_init_disk_with<R: CommandRunner, F: Filesystem>(
    runner: &R,
    fs: &F,
    config: &Config,
    by_id_path: &str,
    force: bool,
    passphrase: &str,
    confirm: &str,
    luks_extra_opts: &[String],
) -> Result<(), InitDiskError> {
    // 1. Block device check
    if !fs.is_block_device(by_id_path) {
        return Err(InitDiskError::Validation(format!(
            "Device not found or not a block device: {by_id_path}"
        )));
    }

    // 2. Declared check
    if !config.disks.iter().any(|d| d.0 == by_id_path) {
        return Err(InitDiskError::Validation(format!(
            "Disk {by_id_path} is not declared in config"
        )));
    }

    // 3. Pool membership check (fail-closed)
    let mountpoint_result = runner.run(&CmdRequest::MountpointCheck {
        path: config.mount_point.clone(),
    });
    let is_mounted = matches!(mountpoint_result, Ok(ref out) if out.exit_status == 0);

    if is_mounted {
        match probe_pool(runner, &config.mount_point) {
            Ok(pool) if pool.mounted => {
                // Check if target is LUKS
                let is_luks_result = runner.run(&CmdRequest::CryptsetupIsLuks {
                    device: by_id_path.to_owned(),
                });
                if matches!(is_luks_result, Ok(ref out) if out.exit_status == 0) {
                    // Get target UUID
                    let uuid_raw = runner.run(&CmdRequest::CryptsetupLuksUuid {
                        device: by_id_path.to_owned(),
                    })?;
                    let uuid_out = parse_cryptsetup_luks_uuid(&uuid_raw)?;

                    // Check if any pool device has matching UUID
                    if pool
                        .devices
                        .iter()
                        .any(|d| d.luks_uuid == uuid_out.uuid)
                    {
                        return Err(InitDiskError::Validation(format!(
                            "Disk {by_id_path} is currently part of the mounted pool"
                        )));
                    }
                }
            }
            Err(ProbeError::NotBtrfs { .. }) => {
                // Not a btrfs pool, skip membership check
            }
            Err(other) => {
                // Fail-closed: cannot verify pool membership
                return Err(InitDiskError::Probe(other));
            }
            _ => {}
        }
    }

    // 4. LUKS header probe
    let is_luks_result = runner.run(&CmdRequest::CryptsetupIsLuks {
        device: by_id_path.to_owned(),
    });
    let is_already_luks = matches!(is_luks_result, Ok(ref out) if out.exit_status == 0);

    if is_already_luks {
        if !force {
            return Err(InitDiskError::Validation(format!(
                "Disk {by_id_path} already has a LUKS header. Use --force to re-format"
            )));
        }
        if confirm != "reformat this disk" {
            return Err(InitDiskError::Validation(
                "--force requires BRAID_CONFIRM='reformat this disk'".to_owned(),
            ));
        }
    }

    // 5b. Target mapper guard
    if let Some(mn) = mapper_name_for_by_id(&ByIdPath(by_id_path.to_owned())) {
        let mapper_path = format!("/dev/mapper/{}", mn.0);
        if fs.exists(&mapper_path) {
            return Err(InitDiskError::Validation(format!(
                "close mapper {} before reformatting {by_id_path}",
                mn.0
            )));
        }
    }

    // 6. Single-passphrase check
    match find_passphrase_target(runner, fs, config, by_id_path)? {
        Some(member) => {
            let test_result = runner.run_with_stdin(
                &CmdRequest::CryptsetupTestPassphrase {
                    device: member.clone(),
                },
                passphrase.as_bytes(),
            );
            match test_result {
                Ok(out) if out.exit_status != 0 => {
                    return Err(InitDiskError::Validation(format!(
                        "Passphrase does not match existing pool member {member}"
                    )));
                }
                Err(e) => {
                    return Err(InitDiskError::Cmd(e));
                }
                _ => {}
            }
        }
        None => {
            // No existing member to verify against (first disk scenario)
        }
    }

    // 7. Format
    println!("Formatting {by_id_path} with LUKS...");
    let format_result = runner.run_with_stdin(
        &CmdRequest::CryptsetupLuksFormat {
            device: by_id_path.to_owned(),
            extra_opts: luks_extra_opts.to_vec(),
        },
        passphrase.as_bytes(),
    );

    match format_result {
        Ok(out) if out.exit_status != 0 => {
            return Err(InitDiskError::Validation(format!(
                "luksFormat failed (exit {}): {}",
                out.exit_status, out.stderr
            )));
        }
        Err(e) => return Err(InitDiskError::Cmd(e)),
        _ => {}
    }

    // 8. Success
    println!("LUKS format complete: {by_id_path}");
    println!("Next step: run 'braid apply' to open and add this disk to the pool.");

    Ok(())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Find an existing LUKS device to test the passphrase against.
///
/// Two-pass search (excludes the target disk):
/// 1. Open mapper: config disk whose mapper is active
/// 2. LUKS-formatted: config disk that is a block device with LUKS header
///
/// Fail-closed: if candidates exist but all checks error, returns Err.
fn find_passphrase_target<R: CommandRunner, F: Filesystem>(
    runner: &R,
    fs: &F,
    config: &Config,
    exclude_path: &str,
) -> Result<Option<String>, InitDiskError> {
    let mut had_candidate = false;
    let mut errors: Vec<String> = Vec::new();

    // Pass 1: open mapper
    for disk in &config.disks {
        if disk.0 == exclude_path {
            continue;
        }

        let mapper_name = match mapper_name_for_by_id(disk) {
            Some(mn) => mn,
            None => continue,
        };

        let mapper_path = format!("/dev/mapper/{}", mapper_name.0);
        if !fs.exists(&mapper_path) {
            continue;
        }

        had_candidate = true;

        let status_result = runner.run(&CmdRequest::CryptsetupStatus {
            mapper: mapper_name.0.clone(),
        });
        match status_result {
            Ok(out) if out.exit_status == 0 => {
                // Active mapper found — use the by-id path for test-passphrase
                return Ok(Some(disk.0.clone()));
            }
            Ok(_) => {
                // Not active, continue
            }
            Err(e) => {
                errors.push(format!("status {}: {e}", mapper_name.0));
            }
        }
    }

    // Pass 2: LUKS-formatted disk
    for disk in &config.disks {
        if disk.0 == exclude_path {
            continue;
        }

        if !fs.is_block_device(&disk.0) {
            continue;
        }

        had_candidate = true;

        let is_luks = runner.run(&CmdRequest::CryptsetupIsLuks {
            device: disk.0.clone(),
        });
        match is_luks {
            Ok(out) if out.exit_status == 0 => {
                return Ok(Some(disk.0.clone()));
            }
            Ok(_) => {
                // Not LUKS, continue
            }
            Err(e) => {
                errors.push(format!("isLuks {}: {e}", disk.0));
            }
        }
    }

    // Fail-closed rule
    if had_candidate && !errors.is_empty() {
        return Err(InitDiskError::Validation(format!(
            "cannot verify passphrase: all candidate checks failed: {}",
            errors.join("; ")
        )));
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};

    struct MockFs {
        paths: Vec<String>,
        block_devices: Vec<String>,
    }

    impl MockFs {
        fn new(paths: &[&str], block_devices: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                block_devices: block_devices.iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.contains(&path.to_string())
        }

        fn is_block_device(&self, path: &str) -> bool {
            self.block_devices.contains(&path.to_string())
        }
    }

    fn ok_raw(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
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

    fn config_2disk() -> Config {
        Config {
            disks: vec![
                ByIdPath("/dev/disk/by-id/virtio-disk1".to_owned()),
                ByIdPath("/dev/disk/by-id/virtio-disk2".to_owned()),
            ],
            mount_point: "/mnt/storage".to_owned(),
        }
    }

    fn config_1disk() -> Config {
        Config {
            disks: vec![ByIdPath("/dev/disk/by-id/virtio-disk1".to_owned())],
            mount_point: "/mnt/storage".to_owned(),
        }
    }

    // ======================================================================
    // Safety gate tests
    // ======================================================================

    #[test]
    fn init_disk_device_not_found() {
        let runner = MockRunner::default();
        let fs = MockFs::new(&[], &[]);
        let config = config_1disk();

        let err = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk1",
            false,
            "pass",
            "",
            &[],
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("not found or not a block device"),
            "got: {err}"
        );
    }

    #[test]
    fn init_disk_not_declared() {
        let runner = MockRunner::default();
        let fs = MockFs::new(&[], &["/dev/disk/by-id/virtio-disk2"]);
        let config = config_1disk(); // only disk1

        let err = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk2",
            false,
            "pass",
            "",
            &[],
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("not declared in config"),
            "got: {err}"
        );
    }

    #[test]
    fn init_disk_in_pool_refuses() {
        // Pool is mounted with disk1 whose UUID matches the target
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                ok_raw("mountpoint", ""),
            )
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: "/mnt/storage".to_owned(),
                },
                ok_raw(
                    "findmnt",
                    r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/virtio-disk1","fstype":"btrfs"}]}"#,
                ),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: "/mnt/storage".to_owned(),
                },
                ok_raw(
                    "btrfs filesystem show",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 1 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/virtio-disk1\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "virtio-disk1".to_owned(),
                },
                ok_raw(
                    "cryptsetup status virtio-disk1",
                    "/dev/mapper/virtio-disk1 is active and is in use.\n\
                     \ttype:    LUKS2\n\
                     \tdevice:  /dev/vda\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".to_owned(),
                },
                ok_raw("cryptsetup luksUUID", "11111111-1111-1111-1111-111111111111\n"),
            )
            // isLuks for pool membership check
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                ok_raw("cryptsetup isLuks", ""),
            )
            // luksUUID for target
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                ok_raw(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
                ),
            );

        let fs = MockFs::new(&[], &["/dev/disk/by-id/virtio-disk1"]);
        let config = config_2disk();

        let err = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk1",
            true,
            "pass",
            "reformat this disk",
            &[],
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("currently part of the mounted pool"),
            "got: {err}"
        );
    }

    #[test]
    fn init_disk_luks_without_force() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                ok_raw("cryptsetup isLuks", ""),
            );

        let fs = MockFs::new(&[], &["/dev/disk/by-id/virtio-disk1"]);
        let config = config_1disk();

        let err = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk1",
            false,
            "pass",
            "",
            &[],
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("already has a LUKS header"),
            "got: {err}"
        );
    }

    #[test]
    fn init_disk_force_wrong_confirm() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                ok_raw("cryptsetup isLuks", ""),
            );

        let fs = MockFs::new(&[], &["/dev/disk/by-id/virtio-disk1"]);
        let config = config_1disk();

        let err = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk1",
            true,
            "pass",
            "wrong",
            &[],
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("BRAID_CONFIRM"),
            "got: {err}"
        );
    }

    #[test]
    fn init_disk_force_correct_confirm() {
        // Force reformat with correct confirm — should proceed to format
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                ok_raw("cryptsetup isLuks", ""),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksFormat {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                    extra_opts: vec![],
                },
                b"pass".to_vec(),
                ok_raw("cryptsetup luksFormat", ""),
            );

        let fs = MockFs::new(&[], &["/dev/disk/by-id/virtio-disk1"]);
        let config = config_1disk();

        let result = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk1",
            true,
            "pass",
            "reformat this disk",
            &[],
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn init_disk_passphrase_mismatch() {
        // Existing member exists, test-passphrase fails
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk2".to_owned(),
                },
                err_raw("cryptsetup isLuks", 4, "not LUKS"),
            )
            // find_passphrase_target: disk1 has LUKS header
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                ok_raw("cryptsetup isLuks", ""),
            )
            // test-passphrase fails
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                b"wrongpass".to_vec(),
                err_raw("cryptsetup open --test-passphrase", 2, "No key available"),
            );

        let fs = MockFs::new(
            &[],
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ],
        );
        let config = config_2disk();

        let err = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk2",
            false,
            "wrongpass",
            "",
            &[],
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("does not match"),
            "got: {err}"
        );
    }

    #[test]
    fn init_disk_passphrase_match() {
        // Existing member, passphrase matches → proceeds to format
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk2".to_owned(),
                },
                err_raw("cryptsetup isLuks", 4, "not LUKS"),
            )
            // find_passphrase_target: disk1 has LUKS header
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                ok_raw("cryptsetup isLuks", ""),
            )
            // test-passphrase succeeds
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                b"pass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase", ""),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksFormat {
                    device: "/dev/disk/by-id/virtio-disk2".to_owned(),
                    extra_opts: vec![],
                },
                b"pass".to_vec(),
                ok_raw("cryptsetup luksFormat", ""),
            );

        let fs = MockFs::new(
            &[],
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ],
        );
        let config = config_2disk();

        let result = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk2",
            false,
            "pass",
            "",
            &[],
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    // ======================================================================
    // Target mapper guard
    // ======================================================================

    #[test]
    fn init_disk_force_target_mapper_active_refuses() {
        // --force but target's mapper is open
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                ok_raw("cryptsetup isLuks", ""),
            );

        let fs = MockFs::new(
            &["/dev/mapper/virtio-disk1"],
            &["/dev/disk/by-id/virtio-disk1"],
        );
        let config = config_1disk();

        let err = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk1",
            true,
            "pass",
            "reformat this disk",
            &[],
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("close mapper"),
            "got: {err}"
        );
    }

    #[test]
    fn init_disk_force_target_mapper_closed_proceeds() {
        // --force, target mapper not active → proceeds
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                ok_raw("cryptsetup isLuks", ""),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksFormat {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                    extra_opts: vec![],
                },
                b"pass".to_vec(),
                ok_raw("cryptsetup luksFormat", ""),
            );

        // mapper does NOT exist
        let fs = MockFs::new(&[], &["/dev/disk/by-id/virtio-disk1"]);
        let config = config_1disk();

        let result = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk1",
            true,
            "pass",
            "reformat this disk",
            &[],
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    // ======================================================================
    // Env parsing
    // ======================================================================

    #[test]
    fn init_disk_luks_opts_unbalanced_quotes_error() {
        // Test that shell_words::split fails on unbalanced quotes and the error propagates
        let result = shell_words::split("--pbkdf 'unclosed");
        assert!(result.is_err(), "expected parse error for unbalanced quotes");
    }

    // ======================================================================
    // Happy path tests
    // ======================================================================

    #[test]
    fn init_disk_fresh_no_existing_member() {
        // First disk, no members to verify against → format succeeds
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                err_raw("cryptsetup isLuks", 4, "not LUKS"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksFormat {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                    extra_opts: vec!["--pbkdf".to_owned(), "pbkdf2".to_owned()],
                },
                b"pass".to_vec(),
                ok_raw("cryptsetup luksFormat", ""),
            );

        let fs = MockFs::new(&[], &["/dev/disk/by-id/virtio-disk1"]);
        let config = config_1disk();

        let result = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk1",
            false,
            "pass",
            "",
            &["--pbkdf".to_owned(), "pbkdf2".to_owned()],
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn init_disk_with_existing_member() {
        // Second disk, passphrase matches open mapper → format succeeds
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk2".to_owned(),
                },
                err_raw("cryptsetup isLuks", 4, "not LUKS"),
            )
            // find_passphrase_target: disk1 mapper exists and active
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "virtio-disk1".to_owned(),
                },
                ok_raw(
                    "cryptsetup status",
                    "/dev/mapper/virtio-disk1 is active and is in use.\n\tdevice:  /dev/vda\n",
                ),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                b"pass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase", ""),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksFormat {
                    device: "/dev/disk/by-id/virtio-disk2".to_owned(),
                    extra_opts: vec![],
                },
                b"pass".to_vec(),
                ok_raw("cryptsetup luksFormat", ""),
            );

        let fs = MockFs::new(
            &["/dev/mapper/virtio-disk1"],
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ],
        );
        let config = config_2disk();

        let result = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk2",
            false,
            "pass",
            "",
            &[],
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn init_disk_non_luks_target_in_pool_not_checked() {
        // Target is not LUKS → pool membership check skipped
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                ok_raw("mountpoint", ""),
            )
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: "/mnt/storage".to_owned(),
                },
                ok_raw(
                    "findmnt",
                    r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/virtio-disk1","fstype":"btrfs"}]}"#,
                ),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: "/mnt/storage".to_owned(),
                },
                ok_raw(
                    "btrfs filesystem show",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 1 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/virtio-disk1\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "virtio-disk1".to_owned(),
                },
                ok_raw(
                    "cryptsetup status virtio-disk1",
                    "/dev/mapper/virtio-disk1 is active and is in use.\n\
                     \ttype:    LUKS2\n\
                     \tdevice:  /dev/vda\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".to_owned(),
                },
                ok_raw("cryptsetup luksUUID", "11111111-1111-1111-1111-111111111111\n"),
            )
            // target isLuks fails (not LUKS)
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk2".to_owned(),
                },
                err_raw("cryptsetup isLuks", 4, "not LUKS"),
            )
            // find_passphrase_target: disk1 mapper is active
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                b"pass".to_vec(),
                ok_raw("cryptsetup open --test-passphrase", ""),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksFormat {
                    device: "/dev/disk/by-id/virtio-disk2".to_owned(),
                    extra_opts: vec![],
                },
                b"pass".to_vec(),
                ok_raw("cryptsetup luksFormat", ""),
            );

        let fs = MockFs::new(
            &["/dev/mapper/virtio-disk1"],
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ],
        );
        let config = config_2disk();

        let result = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk2",
            false,
            "pass",
            "",
            &[],
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    // ======================================================================
    // Pool membership check (fail-closed)
    // ======================================================================

    #[test]
    fn init_disk_pool_probe_error_is_fatal() {
        // MountpointCheck succeeds + probe_pool returns non-NotBtrfs error → fatal
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                ok_raw("mountpoint", ""),
            )
            // FindmntJson returns something that makes probe_pool error out
            // (command error / missing mock for FindmntJson → CmdError → ProbeError::Cmd)
            ;

        let fs = MockFs::new(&[], &["/dev/disk/by-id/virtio-disk1"]);
        let config = config_1disk();

        let err = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk1",
            false,
            "pass",
            "",
            &[],
        )
        .unwrap_err();

        assert!(
            matches!(err, InitDiskError::Probe(_)),
            "expected Probe error, got: {err:?}"
        );
    }

    #[test]
    fn init_disk_not_btrfs_skips_membership() {
        // MountpointCheck succeeds + probe_pool returns NotBtrfs → no membership error
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                ok_raw("mountpoint", ""),
            )
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: "/mnt/storage".to_owned(),
                },
                ok_raw(
                    "findmnt",
                    r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/sda1","fstype":"ext4"}]}"#,
                ),
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                err_raw("cryptsetup isLuks", 4, "not LUKS"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksFormat {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                    extra_opts: vec![],
                },
                b"pass".to_vec(),
                ok_raw("cryptsetup luksFormat", ""),
            );

        let fs = MockFs::new(&[], &["/dev/disk/by-id/virtio-disk1"]);
        let config = config_1disk();

        let result = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk1",
            false,
            "pass",
            "",
            &[],
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    #[test]
    fn init_disk_not_mounted_skips_membership() {
        // MountpointCheck fails → membership check skipped entirely
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: "/mnt/storage".to_owned(),
                },
                err_raw("mountpoint", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                err_raw("cryptsetup isLuks", 4, "not LUKS"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksFormat {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                    extra_opts: vec![],
                },
                b"pass".to_vec(),
                ok_raw("cryptsetup luksFormat", ""),
            );

        let fs = MockFs::new(&[], &["/dev/disk/by-id/virtio-disk1"]);
        let config = config_1disk();

        let result = cmd_init_disk_with(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk1",
            false,
            "pass",
            "",
            &[],
        );
        assert!(result.is_ok(), "got: {result:?}");
    }

    // ======================================================================
    // Passphrase target search (fail-closed)
    // ======================================================================

    #[test]
    fn find_target_prefers_open_mapper() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "virtio-disk1".to_owned(),
                },
                ok_raw(
                    "cryptsetup status",
                    "/dev/mapper/virtio-disk1 is active.\n\tdevice:  /dev/vda\n",
                ),
            );

        let fs = MockFs::new(
            &["/dev/mapper/virtio-disk1"],
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ],
        );
        let config = config_2disk();

        let result = find_passphrase_target(&runner, &fs, &config, "/dev/disk/by-id/virtio-disk2")
            .unwrap();
        assert_eq!(
            result,
            Some("/dev/disk/by-id/virtio-disk1".to_owned())
        );
    }

    #[test]
    fn find_target_falls_back_to_luks_disk() {
        // No open mapper, but another config disk has LUKS header
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk1".to_owned(),
                },
                ok_raw("cryptsetup isLuks", ""),
            );

        let fs = MockFs::new(
            &[],
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ],
        );
        let config = config_2disk();

        let result = find_passphrase_target(&runner, &fs, &config, "/dev/disk/by-id/virtio-disk2")
            .unwrap();
        assert_eq!(
            result,
            Some("/dev/disk/by-id/virtio-disk1".to_owned())
        );
    }

    #[test]
    fn find_target_excludes_self() {
        // Only config disk is the target → returns Ok(None)
        let runner = MockRunner::default();

        let fs = MockFs::new(&[], &["/dev/disk/by-id/virtio-disk1"]);
        let config = config_1disk();

        let result = find_passphrase_target(&runner, &fs, &config, "/dev/disk/by-id/virtio-disk1")
            .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn find_target_none_when_no_members() {
        // No disks exist
        let runner = MockRunner::default();

        let fs = MockFs::new(&[], &[]);
        let config = Config {
            disks: vec![],
            mount_point: "/mnt/storage".to_owned(),
        };

        let result =
            find_passphrase_target(&runner, &fs, &config, "/dev/disk/by-id/virtio-disk1")
                .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn find_target_all_candidates_error() {
        // Mapper exists but CryptsetupStatus errors for the candidate → returns Err
        let runner = MockRunner::default();
        // No output for CryptsetupStatus → MissingMock error

        let fs = MockFs::new(
            &["/dev/mapper/virtio-disk1"],
            &["/dev/disk/by-id/virtio-disk2"],
        );
        let config = config_2disk();

        let err = find_passphrase_target(
            &runner,
            &fs,
            &config,
            "/dev/disk/by-id/virtio-disk2",
        )
        .unwrap_err();

        assert!(
            err.to_string().contains("all candidate checks failed"),
            "got: {err}"
        );
    }
}
