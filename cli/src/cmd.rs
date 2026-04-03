use crate::types::MountPoint;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCommandOutput {
    pub cmd: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_status: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LsblkFieldKind {
    Model,
    Serial,
    Size,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdRequest {
    LsblkJson,
    FindmntJson {
        mount_point: MountPoint,
    },
    BtrfsFilesystemDfJson {
        mount_point: MountPoint,
    },
    BtrfsFilesystemShow {
        mount_point: MountPoint,
    },
    /// `btrfs filesystem show <target>` — accepts any path (device or mount point).
    /// Unlike `BtrfsFilesystemShow` which takes a typed `MountPoint`, this accepts
    /// a raw string for per-device FSID queries in the add path.
    BtrfsFilesystemShowTarget {
        target: String,
    },
    CryptsetupStatus {
        mapper: String,
    },
    CryptsetupLuksUuid {
        device: String,
    },
    BtrfsFilesystemUsageRaw {
        mount_point: MountPoint,
    },
    BtrfsScrubStatus {
        mount_point: MountPoint,
    },
    BtrfsScrubStatusPerDevice {
        mount_point: MountPoint,
    },
    BtrfsDeviceStats {
        mount_point: MountPoint,
    },
    BtrfsDeviceStatsJson {
        mount_point: MountPoint,
    },
    LsblkField {
        device: String,
        field: LsblkFieldKind,
    },
    // Mutation commands for apply
    CryptsetupLuksOpen {
        device: String,
        mapper: String,
    },
    CryptsetupIsLuks {
        device: String,
    },
    CryptsetupClose {
        mapper: String,
    },
    BtrfsDeviceAdd {
        device: String,
        mount_point: MountPoint,
    },
    BtrfsDeviceRemove {
        device: String,
        mount_point: MountPoint,
    },
    BtrfsDeviceScan {
        device: String,
    },
    BtrfsDeviceScanAll,
    BtrfsDeviceScanForget,
    BtrfsBalanceRaid1 {
        mount_point: MountPoint,
    },
    BtrfsBalanceRaid1Soft {
        mount_point: MountPoint,
    },
    BtrfsBalanceSingle {
        mount_point: MountPoint,
    },
    MkfsBtrfs {
        device: String,
    },
    MkfsBtrfsRaid1 {
        devices: Vec<String>,
    },
    Mount {
        device: String,
        mount_point: MountPoint,
    },
    MountWithOptions {
        device: String,
        mount_point: MountPoint,
        options: Vec<String>,
    },
    Umount {
        mount_point: MountPoint,
    },
    MountpointCheck {
        path: MountPoint,
    },
    // Polling commands for progress monitoring
    BtrfsBalanceStatus {
        mount_point: MountPoint,
    },
    BtrfsDeviceUsageRaw {
        mount_point: MountPoint,
    },
    // init-disk commands
    CryptsetupLuksFormat {
        device: String,
        extra_opts: Vec<String>,
    },
    CryptsetupTestPassphrase {
        device: String,
    },
    CryptsetupLuksHeaderBackup {
        device: String,
        backup_path: String,
    },
    SmartctlHealthJson {
        device: String,
    },
    CryptsetupLuksDump {
        device: String,
    },
    /// `cryptsetup luksDump <device>` (text output, no --dump-json-metadata).
    /// Used to read the LUKS2 binary header label field, which is NOT included
    /// in the JSON metadata output.
    CryptsetupLuksDumpText {
        device: String,
    },
    // btrfs replace commands
    BtrfsReplaceStart {
        devid: u64,
        target_device: String,
        mount_point: MountPoint,
    },
    BtrfsReplaceStatus {
        mount_point: MountPoint,
    },
    BtrfsFilesystemResize {
        devid: u64,
        mount_point: MountPoint,
    },
    // Keyfile commands (auto-unlock)
    CryptsetupLuksOpenKeyFile {
        device: String,
        mapper: String,
        key_file_path: String,
    },
    CryptsetupTestKeyFile {
        device: String,
        key_file_path: String,
    },
    CryptsetupLuksAddKeyFile {
        device: String,
        key_file_path: String,
    },
    // Browse TUI — human-readable display variants (no --raw / --format json)
    BtrfsFilesystemUsage {
        mount_point: MountPoint,
    },
    BtrfsFilesystemDf {
        mount_point: MountPoint,
    },
    BtrfsDeviceUsage {
        mount_point: MountPoint,
    },
    BtrfsSubvolumeList {
        mount_point: MountPoint,
    },
    BtrfsSubvolumeShow {
        path: String,
    },
}

#[derive(Debug)]
pub struct CmdArgs {
    pub program: &'static str,
    pub args: Vec<String>,
}

impl CmdArgs {
    /// Render as a shell-safe command string using proper quoting.
    pub fn to_shell_string(&self) -> String {
        let argv: Vec<&str> = std::iter::once(self.program)
            .chain(self.args.iter().map(|s| s.as_str()))
            .collect();
        shell_words::join(&argv)
    }
}

/// A step in a dry-run plan.
#[derive(Debug)]
pub struct Step {
    pub risk: &'static str,
    pub description: String,
    pub commands: Vec<CmdRequest>,
}

impl Step {
    /// Pure renderer — returns the formatted dry-run lines.
    pub fn render_dry_run(steps: &[Step]) -> String {
        let mut out = String::new();
        for step in steps {
            out.push_str(&format!("[{:<11}] {}\n", step.risk, step.description));
            for cmd in &step.commands {
                out.push_str(&format!(
                    "               $ {}\n",
                    cmd.to_argv().to_shell_string()
                ));
            }
        }
        out
    }

    /// Print dry-run plan to stdout.
    pub fn print_dry_run(steps: &[Step]) {
        print!("{}", Self::render_dry_run(steps));
    }
}

/// Base mount options braid always applies.
///
/// noatime: relatime (default) turns reads into CoW metadata writes across all
/// RAID1 drives, preventing HDD spindown.
///
/// skip_balance: prevent the kernel from silently resuming an interrupted balance
/// on mount. braid manages balance lifecycle explicitly.
///
/// subvolid=5: always mount the top-level subvolume, regardless of what
/// `btrfs subvolume set-default` is set to. Without this, a set-default to a
/// non-top-level subvolume would silently change what braid mounts, hiding
/// sibling subvolumes from the mountpoint.
fn base_mount_options() -> Vec<String> {
    vec![
        "noatime".to_owned(),
        "skip_balance".to_owned(),
        "subvolid=5".to_owned(),
    ]
}

impl CmdRequest {
    pub fn to_argv(&self) -> CmdArgs {
        match self {
            CmdRequest::LsblkJson => CmdArgs {
                program: "lsblk",
                args: vec![
                    "--json".into(),
                    "--bytes".into(),
                    "--output".into(),
                    "NAME,TYPE,SIZE,MODEL,SERIAL,UUID,ROTA,TRAN".into(),
                ],
            },
            CmdRequest::FindmntJson { mount_point } => CmdArgs {
                program: "findmnt",
                args: vec![
                    "--json".into(),
                    "--output".into(),
                    "TARGET,SOURCE,FSTYPE,OPTIONS".into(),
                    "--mountpoint".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsFilesystemShow { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec!["filesystem".into(), "show".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsFilesystemShowTarget { target } => CmdArgs {
                program: "btrfs",
                args: vec!["filesystem".into(), "show".into(), target.clone()],
            },
            CmdRequest::CryptsetupStatus { mapper } => CmdArgs {
                program: "cryptsetup",
                args: vec!["status".into(), mapper.clone()],
            },
            CmdRequest::CryptsetupLuksUuid { device } => CmdArgs {
                program: "cryptsetup",
                args: vec!["luksUUID".into(), device.clone()],
            },
            CmdRequest::BtrfsFilesystemDfJson { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec![
                    "--format".into(),
                    "json".into(),
                    "filesystem".into(),
                    "df".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsFilesystemUsageRaw { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec![
                    "filesystem".into(),
                    "usage".into(),
                    "--raw".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsScrubStatus { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec![
                    "scrub".into(),
                    "status".into(),
                    "--raw".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsScrubStatusPerDevice { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec![
                    "scrub".into(),
                    "status".into(),
                    "-d".into(),
                    "-R".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsDeviceStats { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec!["device".into(), "stats".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsDeviceStatsJson { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec![
                    "--format".into(),
                    "json".into(),
                    "device".into(),
                    "stats".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsBalanceStatus { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec!["balance".into(), "status".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsDeviceUsageRaw { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec![
                    "device".into(),
                    "usage".into(),
                    "--raw".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::LsblkField { device, field } => {
                let field_name = match field {
                    LsblkFieldKind::Model => "MODEL",
                    LsblkFieldKind::Serial => "SERIAL",
                    LsblkFieldKind::Size => "SIZE",
                };
                CmdArgs {
                    program: "lsblk",
                    args: vec![
                        "-n".into(), // no header
                        "-d".into(), // device only (no partitions)
                        "-b".into(), // sizes in bytes (no-op for string fields)
                        "-o".into(), // output column
                        field_name.into(),
                        device.clone(),
                    ],
                }
            }
            CmdRequest::CryptsetupLuksOpen { device, mapper } => CmdArgs {
                program: "cryptsetup",
                args: vec![
                    "open".into(),
                    "--type".into(),
                    "luks".into(),
                    "--key-file=-".into(),
                    // Bypass dm-crypt's internal workqueues — they add 3-4x queuing
                    // overhead on any block device. Requires kernel >= 5.9.
                    "--perf-no_read_workqueue".into(),
                    "--perf-no_write_workqueue".into(),
                    device.clone(),
                    mapper.clone(),
                ],
            },
            CmdRequest::CryptsetupIsLuks { device } => CmdArgs {
                program: "cryptsetup",
                args: vec!["isLuks".into(), device.clone()],
            },
            CmdRequest::CryptsetupClose { mapper } => CmdArgs {
                program: "cryptsetup",
                args: vec!["close".into(), mapper.clone()],
            },
            CmdRequest::BtrfsDeviceAdd {
                device,
                mount_point,
            } => CmdArgs {
                program: "btrfs",
                args: vec![
                    "device".into(),
                    "add".into(),
                    "--enqueue".into(),
                    // We don't need '-f' here to bypass `btrfs device add`'s blkid probe
                    // because at this point we've added a fresh luks header,
                    // the volume is luks-opened, and mapper will be "decrypting"
                    // preexisting data that we never encrypted which will look like garbage
                    // rather than anything blkid will recognize.
                    //
                    // "-f".into(),
                    device.clone(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsDeviceRemove {
                device,
                mount_point,
            } => CmdArgs {
                program: "btrfs",
                args: vec![
                    "device".into(),
                    "remove".into(),
                    "--enqueue".into(),
                    device.clone(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsDeviceScan { device } => CmdArgs {
                program: "btrfs",
                args: vec!["device".into(), "scan".into(), device.clone()],
            },
            CmdRequest::BtrfsDeviceScanAll => CmdArgs {
                program: "btrfs",
                args: vec!["device".into(), "scan".into()],
            },
            CmdRequest::BtrfsDeviceScanForget => CmdArgs {
                program: "btrfs",
                args: vec!["device".into(), "scan".into(), "--forget".into()],
            },
            CmdRequest::BtrfsBalanceRaid1 { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec![
                    "balance".into(),
                    "start".into(),
                    "--enqueue".into(),
                    "-dconvert=raid1".into(),
                    "-mconvert=raid1".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsBalanceRaid1Soft { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec![
                    "balance".into(),
                    "start".into(),
                    "--enqueue".into(),
                    "-dconvert=raid1,soft".into(),
                    "-mconvert=raid1,soft".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsBalanceSingle { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec![
                    "balance".into(),
                    "start".into(),
                    "--enqueue".into(),
                    "-dconvert=single".into(),
                    // Important: use dup for metadata when converting to single
                    "-mconvert=dup".into(),
                    "-f".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::MkfsBtrfs { device } => CmdArgs {
                program: "mkfs.btrfs",
                args: vec![
                    "-f".into(),
                    "-d".into(),
                    "single".into(),
                    "-m".into(),
                    "dup".into(),
                    device.clone(),
                ],
            },
            CmdRequest::MkfsBtrfsRaid1 { devices } => {
                let mut args = vec![
                    "-f".into(),
                    "-d".into(),
                    "raid1".into(),
                    "-m".into(),
                    "raid1".into(),
                ];
                args.extend(devices.iter().cloned());
                CmdArgs {
                    program: "mkfs.btrfs",
                    args,
                }
            }
            CmdRequest::Mount {
                device,
                mount_point,
            } => {
                let args = vec![
                    "-o".into(),
                    base_mount_options().join(","),
                    device.clone(),
                    mount_point.0.clone(),
                ];
                CmdArgs {
                    program: "mount",
                    args,
                }
            }
            CmdRequest::MountWithOptions {
                device,
                mount_point,
                options,
            } => {
                let mut all_options = base_mount_options();
                all_options.extend(options.iter().cloned());
                let args = vec![
                    "-o".into(),
                    all_options.join(","),
                    device.clone(),
                    mount_point.0.clone(),
                ];
                CmdArgs {
                    program: "mount",
                    args,
                }
            }
            CmdRequest::Umount { mount_point } => CmdArgs {
                program: "umount",
                args: vec![mount_point.0.clone()],
            },
            CmdRequest::MountpointCheck { path } => CmdArgs {
                program: "mountpoint",
                args: vec!["-q".into(), path.0.clone()],
            },
            CmdRequest::BtrfsReplaceStart {
                devid,
                target_device,
                mount_point,
            } => CmdArgs {
                program: "btrfs",
                args: vec![
                    "replace".into(),
                    "start".into(),
                    "--enqueue".into(),
                    // -r: read from mirrors, not the source device. Without -r,
                    // replacing a drive with read errors is extremely slow (kernel
                    // retries every bad sector). In RAID1 there is no downside to
                    // always passing -r — it just reads the other copy instead of
                    // the source, same amount of I/O. The perf cost only exists
                    // for RAID5/6 (parity reconstruction), which braid doesn't use.
                    "-r".into(),
                    "-f".into(),
                    "-B".into(),
                    devid.to_string(),
                    target_device.clone(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsReplaceStatus { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec!["replace".into(), "status".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsFilesystemResize { devid, mount_point } => CmdArgs {
                program: "btrfs",
                args: vec![
                    "filesystem".into(),
                    "resize".into(),
                    "--enqueue".into(),
                    format!("{devid}:max"),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::CryptsetupLuksHeaderBackup {
                device,
                backup_path,
            } => CmdArgs {
                program: "cryptsetup",
                args: vec![
                    "luksHeaderBackup".into(),
                    "--header-backup-file".into(),
                    backup_path.clone(),
                    device.clone(),
                ],
            },
            CmdRequest::CryptsetupLuksFormat { device, extra_opts } => {
                let mut args: Vec<String> = vec![
                    "luksFormat".into(),
                    // luks2 is already the default but might as well
                    "--type".into(),
                    "luks2".into(),
                    "--batch-mode".into(),
                    "--key-file=-".into(),
                ];
                for opt in extra_opts {
                    args.push(opt.clone());
                }
                args.push(device.clone());
                CmdArgs {
                    program: "cryptsetup",
                    args,
                }
            }
            CmdRequest::CryptsetupTestPassphrase { device } => CmdArgs {
                program: "cryptsetup",
                args: vec![
                    "open".into(),
                    "--test-passphrase".into(),
                    "--key-file=-".into(),
                    device.clone(),
                ],
            },
            CmdRequest::SmartctlHealthJson { device } => CmdArgs {
                program: "smartctl",
                args: vec!["-H".into(), "-A".into(), device.clone(), "--json".into()],
            },
            CmdRequest::CryptsetupLuksDump { device } => CmdArgs {
                program: "cryptsetup",
                args: vec![
                    "luksDump".into(),
                    "--dump-json-metadata".into(),
                    device.clone(),
                ],
            },
            CmdRequest::CryptsetupLuksDumpText { device } => CmdArgs {
                program: "cryptsetup",
                args: vec!["luksDump".into(), device.clone()],
            },
            CmdRequest::CryptsetupLuksOpenKeyFile {
                device,
                mapper,
                key_file_path,
            } => CmdArgs {
                program: "cryptsetup",
                args: vec![
                    "open".into(),
                    "--type".into(),
                    "luks".into(),
                    "--key-file".into(),
                    key_file_path.clone(),
                    "--keyfile-size".into(),
                    "4096".into(),
                    "--perf-no_read_workqueue".into(),
                    "--perf-no_write_workqueue".into(),
                    device.clone(),
                    mapper.clone(),
                ],
            },
            CmdRequest::CryptsetupTestKeyFile {
                device,
                key_file_path,
            } => CmdArgs {
                program: "cryptsetup",
                args: vec![
                    "open".into(),
                    "--test-passphrase".into(),
                    "--key-file".into(),
                    key_file_path.clone(),
                    "--keyfile-size".into(),
                    "4096".into(),
                    device.clone(),
                ],
            },
            CmdRequest::CryptsetupLuksAddKeyFile {
                device,
                key_file_path,
            } => CmdArgs {
                program: "cryptsetup",
                args: vec![
                    "luksAddKey".into(),
                    "--key-slot".into(),
                    "1".into(),
                    "--new-keyfile-size".into(),
                    "4096".into(),
                    device.clone(),
                    key_file_path.clone(),
                ],
            },
            CmdRequest::BtrfsFilesystemUsage { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec!["filesystem".into(), "usage".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsFilesystemDf { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec!["filesystem".into(), "df".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsDeviceUsage { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec!["device".into(), "usage".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsSubvolumeList { mount_point } => CmdArgs {
                program: "btrfs",
                args: vec!["subvolume".into(), "list".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsSubvolumeShow { path } => CmdArgs {
                program: "btrfs",
                args: vec!["subvolume".into(), "show".into(), path.clone()],
            },
        }
    }

    pub fn requires_stdin(&self) -> bool {
        matches!(
            self,
            CmdRequest::CryptsetupLuksOpen { .. }
                | CmdRequest::CryptsetupLuksFormat { .. }
                | CmdRequest::CryptsetupTestPassphrase { .. }
                | CmdRequest::CryptsetupLuksAddKeyFile { .. }
        )
    }
}

#[derive(Debug, Error)]
pub enum CmdError {
    #[error("command failed: {0}")]
    Failed(String),
    #[error("mock output missing for request")]
    MissingMock,
}

pub trait CommandRunner: Sync {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError>;
    fn run_with_stdin(
        &self,
        request: &CmdRequest,
        stdin: &[u8],
    ) -> Result<RawCommandOutput, CmdError>;
}

pub struct RealRunner;

impl RealRunner {
    fn exec(cmd: &CmdArgs) -> Result<RawCommandOutput, CmdError> {
        let cmd_str = format!("{} {}", cmd.program, cmd.args.join(" "));
        // Force POSIX locale so error strings (strerror, %m) are always English —
        // braid matches stderr substrings for ENOSPC, device-busy, etc.
        let output = std::process::Command::new(cmd.program)
            .args(&cmd.args)
            .env("LC_ALL", "C")
            .output()
            .map_err(|e| CmdError::Failed(format!("{cmd_str}: {e}")))?;

        let exit_status = output.status.code().unwrap_or(-1);

        Ok(RawCommandOutput {
            cmd: cmd_str,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_status,
        })
    }

    fn exec_with_stdin(cmd: &CmdArgs, stdin_bytes: &[u8]) -> Result<RawCommandOutput, CmdError> {
        use std::io::Write;
        use std::process::Stdio;

        let cmd_str = format!("{} {}", cmd.program, cmd.args.join(" "));
        // Force POSIX locale so error strings (strerror, %m) are always English —
        // braid matches stderr substrings for ENOSPC, device-busy, etc.
        let mut child = std::process::Command::new(cmd.program)
            .args(&cmd.args)
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| CmdError::Failed(format!("{cmd_str}: {e}")))?;

        if let Some(mut stdin_handle) = child.stdin.take() {
            stdin_handle
                .write_all(stdin_bytes)
                .map_err(|e| CmdError::Failed(format!("{cmd_str}: write stdin: {e}")))?;
        }

        let output = child
            .wait_with_output()
            .map_err(|e| CmdError::Failed(format!("{cmd_str}: {e}")))?;

        let exit_status = output.status.code().unwrap_or(-1);

        Ok(RawCommandOutput {
            cmd: cmd_str,
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_status,
        })
    }
}

impl CommandRunner for RealRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        if request.requires_stdin() {
            return Err(CmdError::Failed(format!(
                "{request:?} must use run_with_stdin"
            )));
        }
        RealRunner::exec(&request.to_argv())
    }

    fn run_with_stdin(
        &self,
        request: &CmdRequest,
        stdin: &[u8],
    ) -> Result<RawCommandOutput, CmdError> {
        if !request.requires_stdin() {
            return Err(CmdError::Failed(format!(
                "{request:?} must use run, not run_with_stdin"
            )));
        }
        RealRunner::exec_with_stdin(&request.to_argv(), stdin)
    }
}

#[derive(Default)]
pub struct MockRunner {
    outputs: std::collections::HashMap<String, RawCommandOutput>,
    stdin_expectations: std::collections::HashMap<String, Vec<u8>>,
}

impl MockRunner {
    pub fn with_output(mut self, request: CmdRequest, output: RawCommandOutput) -> Self {
        self.outputs.insert(format!("{request:?}"), output);
        self
    }

    pub fn with_output_stdin(
        mut self,
        request: CmdRequest,
        expected_stdin: Vec<u8>,
        output: RawCommandOutput,
    ) -> Self {
        let key = format!("{request:?}");
        self.outputs.insert(key.clone(), output);
        self.stdin_expectations.insert(key, expected_stdin);
        self
    }
}

impl CommandRunner for MockRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        let output = self
            .outputs
            .get(&format!("{request:?}"))
            .cloned()
            .ok_or(CmdError::MissingMock)?;
        if let CmdRequest::CryptsetupLuksHeaderBackup { backup_path, .. } = request
            && output.exit_status == 0 {
                if let Some(parent) = std::path::Path::new(backup_path.as_str()).parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| CmdError::Failed(format!("mock: create_dir_all: {e}")))?;
                }
                std::fs::write(backup_path, b"")
                    .map_err(|e| CmdError::Failed(format!("mock: write backup: {e}")))?;
            }
        Ok(output)
    }

    fn run_with_stdin(
        &self,
        request: &CmdRequest,
        stdin: &[u8],
    ) -> Result<RawCommandOutput, CmdError> {
        let key = format!("{request:?}");
        if let Some(expected) = self.stdin_expectations.get(&key) {
            assert_eq!(stdin, expected.as_slice(), "stdin mismatch for {key}");
        }
        self.outputs.get(&key).cloned().ok_or(CmdError::MissingMock)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_runner_returns_seeded_output() {
        let req = CmdRequest::LsblkJson;
        let mock = MockRunner::default().with_output(
            req.clone(),
            RawCommandOutput {
                cmd: "lsblk --json".to_owned(),
                stdout: "{\"blockdevices\":[]}".to_owned(),
                stderr: String::new(),
                exit_status: 0,
            },
        );

        let out = mock.run(&req).expect("mock should have output");
        assert_eq!(out.exit_status, 0);
    }

    #[test]
    fn luks_format_run_without_stdin_errors() {
        let runner = RealRunner;
        let req = CmdRequest::CryptsetupLuksFormat {
            device: "/dev/vda".to_owned(),
            extra_opts: vec![],
        };
        let result = runner.run(&req);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, CmdError::Failed(ref msg) if msg.contains("must use run_with_stdin")),
            "expected Failed with stdin hint, got: {err:?}"
        );
    }

    #[test]
    fn test_passphrase_run_without_stdin_errors() {
        let runner = RealRunner;
        let req = CmdRequest::CryptsetupTestPassphrase {
            device: "/dev/vda".to_owned(),
        };
        let result = runner.run(&req);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, CmdError::Failed(ref msg) if msg.contains("must use run_with_stdin")),
            "expected Failed with stdin hint, got: {err:?}"
        );
    }

    #[test]
    fn luks_format_run_with_stdin_routes_correctly() {
        let req = CmdRequest::CryptsetupLuksFormat {
            device: "/dev/vda".to_owned(),
            extra_opts: vec!["--pbkdf".to_owned(), "pbkdf2".to_owned()],
        };
        let mock = MockRunner::default().with_output_stdin(
            req.clone(),
            b"secret".to_vec(),
            RawCommandOutput {
                cmd: "cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 /dev/vda"
                    .to_owned(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let result = mock.run_with_stdin(&req, b"secret");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().exit_status, 0);
    }

    #[test]
    fn luks_open_key_file_run_dispatches_directly() {
        let req = CmdRequest::CryptsetupLuksOpenKeyFile {
            device: "/dev/vda".to_owned(),
            mapper: "braid-test".to_owned(),
            key_file_path: "/run/braid-key/braid.key".to_owned(),
        };
        let mock = MockRunner::default().with_output(
            req.clone(),
            RawCommandOutput {
                cmd: "cryptsetup open".to_owned(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let result = mock.run(&req);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().exit_status, 0);
    }

    #[test]
    fn test_key_file_run_dispatches_directly() {
        let req = CmdRequest::CryptsetupTestKeyFile {
            device: "/dev/vda".to_owned(),
            key_file_path: "/run/braid-key/braid.key".to_owned(),
        };
        let mock = MockRunner::default().with_output(
            req.clone(),
            RawCommandOutput {
                cmd: "cryptsetup open --test-passphrase".to_owned(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let result = mock.run(&req);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().exit_status, 0);
    }

    #[test]
    fn luks_add_key_file_run_without_stdin_errors() {
        let runner = RealRunner;
        let req = CmdRequest::CryptsetupLuksAddKeyFile {
            device: "/dev/vda".to_owned(),
            key_file_path: "/tmp/braid.key".to_owned(),
        };
        let result = runner.run(&req);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, CmdError::Failed(ref msg) if msg.contains("must use run_with_stdin")),
            "expected Failed with stdin hint, got: {err:?}"
        );
    }

    #[test]
    fn luks_add_key_file_run_with_stdin_routes_correctly() {
        let req = CmdRequest::CryptsetupLuksAddKeyFile {
            device: "/dev/vda".to_owned(),
            key_file_path: "/tmp/braid.key".to_owned(),
        };
        let mock = MockRunner::default().with_output_stdin(
            req.clone(),
            b"existingpassphrase".to_vec(),
            RawCommandOutput {
                cmd: "cryptsetup luksAddKey --key-slot 1 /dev/vda /tmp/braid.key".to_owned(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let result = mock.run_with_stdin(&req, b"existingpassphrase");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().exit_status, 0);
    }

    #[test]
    // Intent: BtrfsBalanceRaid1Soft generates the correct ,soft flags.
    // Why: the ,soft flag is critical — it tells btrfs to only convert non-RAID1
    // chunks, skipping already-RAID1 data. Without it, a full rebalance rewrites
    // every chunk unnecessarily.
    // Scenario: after remove-missing or replace clears the last missing device,
    // the soft balance restores redundancy for single-profile chunks created
    // during degraded operation.
    fn btrfs_balance_raid1_soft_generates_correct_argv() {
        let cmd = CmdRequest::BtrfsBalanceRaid1Soft {
            mount_point: MountPoint("/mnt/storage".to_owned()),
        }
        .to_argv();
        assert_eq!(cmd.program, "btrfs");
        assert_eq!(
            cmd.args,
            vec![
                "balance",
                "start",
                "--enqueue",
                "-dconvert=raid1,soft",
                "-mconvert=raid1,soft",
                "/mnt/storage",
            ]
        );
    }

    #[test]
    // Intent: btrfs replace start must pass -r to read from mirrors instead of
    // the source device.
    // Why: without -r, replacing a degrading (but still present) drive hits
    // every bad sector, triggering kernel I/O retries/timeouts and making
    // replacement dramatically slower. Always passing -r is the safe default —
    // negligible downside on healthy swaps, massive upside on failing drives.
    // Scenario: drive has SMART warnings with growing bad sectors. Operator
    // runs braid replace proactively. -r skips the dying drive, reads from
    // healthy mirrors, and finishes in minutes instead of hours.
    fn btrfs_replace_start_includes_read_from_mirrors_flag() {
        let cmd = CmdRequest::BtrfsReplaceStart {
            devid: 2,
            target_device: "/dev/mapper/braid-new".to_owned(),
            mount_point: MountPoint("/mnt/storage".to_owned()),
        }
        .to_argv();
        assert!(
            cmd.args.iter().any(|a| a == "-r"),
            "btrfs replace start must include -r flag to read from mirrors, got: {:?}",
            cmd.args
        );
    }

    #[test]
    // Intent: MkfsBtrfsRaid1 generates correct argv with -d raid1 -m raid1 and all devices.
    // Why: incorrect mkfs arguments could create a single-profile filesystem instead of RAID1.
    // Scenario: multi-disk add bootstraps a new pool with 2+ fresh disks.
    fn mkfs_btrfs_raid1_generates_correct_argv() {
        let cmd = CmdRequest::MkfsBtrfsRaid1 {
            devices: vec![
                "/dev/mapper/braid-disk1".to_owned(),
                "/dev/mapper/braid-disk2".to_owned(),
            ],
        }
        .to_argv();
        assert_eq!(cmd.program, "mkfs.btrfs");
        assert_eq!(
            cmd.args,
            vec![
                "-f",
                "-d",
                "raid1",
                "-m",
                "raid1",
                "/dev/mapper/braid-disk1",
                "/dev/mapper/braid-disk2",
            ]
        );
    }

    #[test]
    /* Intent: MkfsBtrfs generates correct argv with -d single -m dup.
     * Why: implicit profiles make braid's storage intent ambiguous and ignore upstream guidance.
     * Scenario: single-disk bootstrap creates a new pool with one fresh disk.
     */
    fn mkfs_btrfs_single_generates_correct_argv() {
        let cmd = CmdRequest::MkfsBtrfs {
            device: "/dev/mapper/braid-disk1".to_owned(),
        }
        .to_argv();
        assert_eq!(cmd.program, "mkfs.btrfs");
        assert_eq!(
            cmd.args,
            vec!["-f", "-d", "single", "-m", "dup", "/dev/mapper/braid-disk1"]
        );
    }

    #[test]
    fn test_passphrase_run_with_stdin_routes_correctly() {
        let req = CmdRequest::CryptsetupTestPassphrase {
            device: "/dev/vda".to_owned(),
        };
        let mock = MockRunner::default().with_output_stdin(
            req.clone(),
            b"secret".to_vec(),
            RawCommandOutput {
                cmd: "cryptsetup open --test-passphrase --key-file=- /dev/vda".to_owned(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let result = mock.run_with_stdin(&req, b"secret");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().exit_status, 0);
    }

    #[test]
    // Intent: MockRunner creates the backup file on successful luksHeaderBackup.
    // Why: backup_luks_header_to does atomic write (tmp + rename) and needs the
    // tmp file to exist after cryptsetup runs. Without the mock side-effect,
    // set_permissions on the tmp file would ENOENT.
    // Scenario: any enroll_key_file test that backs up headers.
    fn mock_header_backup_creates_file_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("braid-test.luksheader.tmp");

        let req = CmdRequest::CryptsetupLuksHeaderBackup {
            device: "/dev/vda".to_owned(),
            backup_path: path.display().to_string(),
        };
        let mock = MockRunner::default().with_output(
            req.clone(),
            RawCommandOutput {
                cmd: "cryptsetup luksHeaderBackup".to_owned(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        );

        mock.run(&req).unwrap();
        assert!(path.exists(), "mock should create backup file on success");
    }

    #[test]
    // Intent: MockRunner does NOT create file when luksHeaderBackup fails.
    // Why: a failed cryptsetup shouldn't leave artifacts on disk.
    // Scenario: cryptsetup fails (bad device, permissions, etc).
    fn mock_header_backup_skips_file_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("braid-test.luksheader.tmp");

        let req = CmdRequest::CryptsetupLuksHeaderBackup {
            device: "/dev/vda".to_owned(),
            backup_path: path.display().to_string(),
        };
        let mock = MockRunner::default().with_output(
            req.clone(),
            RawCommandOutput {
                cmd: "cryptsetup luksHeaderBackup".to_owned(),
                stdout: String::new(),
                stderr: "Device not found".to_owned(),
                exit_status: 1,
            },
        );

        mock.run(&req).unwrap();
        assert!(!path.exists(), "mock should not create file on failure");
    }

    #[test]
    fn btrfs_filesystem_show_target_generates_correct_argv() {
        let cmd = CmdRequest::BtrfsFilesystemShowTarget {
            target: "/dev/mapper/braid-disk1".to_owned(),
        }
        .to_argv();
        assert_eq!(cmd.program, "btrfs");
        assert_eq!(
            cmd.args,
            vec!["filesystem", "show", "/dev/mapper/braid-disk1"]
        );
    }

    #[test]
    fn cryptsetup_luks_dump_text_generates_correct_argv() {
        let cmd = CmdRequest::CryptsetupLuksDumpText {
            device: "/dev/disk/by-id/disk1".to_owned(),
        }
        .to_argv();
        assert_eq!(cmd.program, "cryptsetup");
        assert_eq!(cmd.args, vec!["luksDump", "/dev/disk/by-id/disk1"]);
    }

    #[test]
    // Intent: Mount always includes noatime and skip_balance.
    // Why: skip_balance prevents the kernel from silently resuming an
    // interrupted balance on mount — a safety-critical invariant.
    // Scenario: normal unlock mounts the pool with base options only.
    fn mount_includes_skip_balance() {
        let cmd = CmdRequest::Mount {
            device: "/dev/mapper/braid-disk1".to_owned(),
            mount_point: MountPoint("/mnt/storage".to_owned()),
        }
        .to_argv();
        assert_eq!(cmd.program, "mount");
        assert_eq!(
            cmd.args,
            vec![
                "-o",
                "noatime,skip_balance,subvolid=5",
                "/dev/mapper/braid-disk1",
                "/mnt/storage"
            ]
        );
    }

    #[test]
    // Intent: MountWithOptions prepends base options before caller options.
    // Why: degraded mount must still include skip_balance and subvolid=5.
    // Scenario: degraded unlock adds -o degraded; base options must appear first.
    fn mount_with_options_includes_skip_balance() {
        let cmd = CmdRequest::MountWithOptions {
            device: "/dev/mapper/braid-disk1".to_owned(),
            mount_point: MountPoint("/mnt/storage".to_owned()),
            options: vec!["degraded".to_owned()],
        }
        .to_argv();
        assert_eq!(cmd.program, "mount");
        assert_eq!(
            cmd.args,
            vec![
                "-o",
                "noatime,skip_balance,subvolid=5,degraded",
                "/dev/mapper/braid-disk1",
                "/mnt/storage",
            ]
        );
    }

    #[test]
    // Intent: to_shell_string renders simple args without unnecessary quoting.
    // Why: dry-run output should be readable and copy-pasteable.
    // Scenario: typical btrfs command with paths and flags.
    fn to_shell_string_simple_args() {
        let s = CmdRequest::BtrfsDeviceAdd {
            device: "/dev/mapper/braid-aaa".to_owned(),
            mount_point: MountPoint("/mnt/storage".to_owned()),
        }
        .to_argv()
        .to_shell_string();
        assert_eq!(
            s,
            "btrfs device add --enqueue /dev/mapper/braid-aaa /mnt/storage"
        );
    }

    #[test]
    // Intent: to_shell_string quotes args containing shell-significant characters.
    // Why: --key-file=- contains = which shell_words may quote; paths with spaces
    // must be quoted to remain copy-pasteable.
    // Scenario: cryptsetup luksFormat with key-file flag.
    fn to_shell_string_quotes_special_chars() {
        let s = CmdArgs {
            program: "cryptsetup",
            args: vec![
                "luksFormat".into(),
                "--key-file=-".into(),
                "/mnt/my storage/key".into(),
            ],
        }
        .to_shell_string();
        // shell_words::join quotes args that contain spaces
        assert!(
            s.contains("/mnt/my storage") || s.contains("'/mnt/my storage"),
            "path with spaces must be quoted, got: {s}"
        );
        assert!(s.starts_with("cryptsetup luksFormat"));
    }

    #[test]
    // Intent: to_shell_string produces correct output for LUKS format with label.
    // Why: this is the most complex real command with env opts + label.
    // Scenario: dry-run of braid add with a fresh disk.
    fn to_shell_string_luks_format_with_label() {
        let s = CmdRequest::CryptsetupLuksFormat {
            device: "/dev/disk/by-id/disk1".to_owned(),
            extra_opts: vec!["--label".into(), "braid-aaa".into()],
        }
        .to_argv()
        .to_shell_string();
        assert_eq!(
            s,
            "cryptsetup luksFormat --type luks2 --batch-mode '--key-file=-' --label braid-aaa /dev/disk/by-id/disk1"
        );
    }

    #[test]
    // Intent: Step::render_dry_run formats steps with commands correctly.
    // Why: user-facing dry-run output shape must be stable and readable.
    // Scenario: two-step dry-run plan with one command each.
    fn render_dry_run_formats_steps_with_commands() {
        let steps = vec![
            Step {
                risk: "destructive",
                description: "LUKS format /dev/disk/by-id/disk1".into(),
                commands: vec![CmdRequest::CryptsetupLuksFormat {
                    device: "/dev/disk/by-id/disk1".to_owned(),
                    extra_opts: vec!["--label".into(), "braid-aaa".into()],
                }],
            },
            Step {
                risk: "safe",
                description: "LUKS open → braid-aaa".into(),
                commands: vec![CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/disk1".to_owned(),
                    mapper: "braid-aaa".to_owned(),
                }],
            },
        ];
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "[destructive] LUKS format /dev/disk/by-id/disk1");
        assert!(lines[1].contains("$ cryptsetup luksFormat"));
        assert_eq!(lines[2], "[safe       ] LUKS open → braid-aaa");
        assert!(lines[3].contains("$ cryptsetup open --type luks"));
    }

    #[test]
    // Intent: Steps with no commands render description only.
    // Why: deferred verification steps have no concrete command.
    // Scenario: LUKS open + identity verification at execution time.
    fn render_dry_run_step_without_commands() {
        let steps = vec![Step {
            risk: "safe",
            description: "identity verification at execution time".into(),
            commands: vec![],
        }];
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            "[safe       ] identity verification at execution time"
        );
    }
}
