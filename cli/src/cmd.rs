use crate::types::MountPoint;
use std::os::unix::process::ExitStatusExt;
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
    BtrfsScrubResume {
        mount_point: MountPoint,
    },
    BtrfsScrubStart {
        mount_point: MountPoint,
    },
    BtrfsScrubCancel {
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
        force: bool,
    },
    BtrfsDeviceRemove {
        device: String,
        mount_point: MountPoint,
    },
    BtrfsDeviceScan {
        device: String,
    },
    BtrfsDeviceScanAll,
    /// `btrfs device scan --forget <dev>...` -- pool-scoped forget of the
    /// kernel's btrfs scan registry. The no-arg form is kernel-global
    /// (forgets every stale scan entry on the host); always pass the
    /// explicit close-set of mapper paths the same code path is about
    /// to destroy.
    BtrfsDeviceScanForget {
        devices: Vec<String>,
    },
    /// `wipefs --all --types btrfs <device>` -- deliberately narrow stale
    /// btrfs-signature wipe used only for verified returned add targets.
    WipefsBtrfs {
        device: String,
    },
    BtrfsBalanceRaid1 {
        mount_point: MountPoint,
    },
    BtrfsBalanceRaid1Soft {
        mount_point: MountPoint,
    },
    BtrfsBalanceSingle {
        mount_point: MountPoint,
    },
    /// `btrfs balance resume <mp>` -- continue a paused balance using the
    /// convert filters the kernel already persisted in the chunk tree's
    /// `BALANCE_ITEM`. Used by `cmd_recover` to drain a balance that a
    /// forced shutdown left paused (skip_balance prevents kernel
    /// auto-resume on mount). Idempotent only in the sense that the
    /// kernel rejects with non-zero when no balance is in progress; the
    /// caller must check `BalanceReport::Paused` before invoking this.
    BtrfsBalanceResume {
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
    /// Run the canonical `braid-beep-probe` wrapper. The path is read at
    /// runtime from `/etc/braid/notifier-config.json` (written by the NixOS
    /// monitor module). Used by `braid doctor --beep` to play the alert test beep
    /// -- the same code path the alert service uses, so a successful run is
    /// both a notifier-health check and a preview of the real alert beep.
    BraidBeepProbe {
        path: String,
    },
    /// `systemctl is-active <unit>` — exits non-zero (3) when the unit is
    /// inactive/failed but still prints the status word to stdout. The TUI
    /// fan probe parses stdout regardless of exit code.
    SystemctlIsActive {
        unit: String,
    },
    /// `upsc <name>` — NUT status query. Emits `key: value` lines (see
    /// `reference/nut/clients/upsc.c:141`) on stdout; non-zero exit when the
    /// upsd daemon is unreachable or the UPS name is unknown. braid uses
    /// this for preflight-on-battery and `braid ups status`.
    UpscQuery {
        name: String,
    },
}

#[derive(Debug)]
pub struct CmdArgs {
    /// Program to invoke. `String` (not `&'static str`) so variants like
    /// `BraidBeepProbe` can carry a runtime-resolved Nix store path read
    /// from `/etc/braid/notifier-config.json`.
    pub program: String,
    pub args: Vec<String>,
}

impl CmdArgs {
    /// Render as a shell-safe command string using proper quoting.
    pub fn to_shell_string(&self) -> String {
        let argv: Vec<&str> = std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(|s| s.as_str()))
            .collect();
        shell_words::join(&argv)
    }
}

/// A step in a dry-run plan.
#[derive(Debug, Clone)]
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
                program: "lsblk".to_owned(),
                args: vec![
                    "--json".into(),
                    "--bytes".into(),
                    "--output".into(),
                    "NAME,TYPE,SIZE,MODEL,SERIAL,UUID,ROTA,TRAN".into(),
                ],
            },
            CmdRequest::BtrfsFilesystemShow { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec!["filesystem".into(), "show".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsFilesystemShowTarget { target } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec!["filesystem".into(), "show".into(), target.clone()],
            },
            CmdRequest::CryptsetupStatus { mapper } => CmdArgs {
                program: "cryptsetup".to_owned(),
                args: vec!["status".into(), mapper.clone()],
            },
            CmdRequest::CryptsetupLuksUuid { device } => CmdArgs {
                program: "cryptsetup".to_owned(),
                args: vec!["luksUUID".into(), device.clone()],
            },
            CmdRequest::BtrfsFilesystemDfJson { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec![
                    "--format".into(),
                    "json".into(),
                    "filesystem".into(),
                    "df".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsFilesystemUsageRaw { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec![
                    "filesystem".into(),
                    "usage".into(),
                    "--raw".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsScrubStatus { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec![
                    "scrub".into(),
                    "status".into(),
                    "--raw".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsScrubResume { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec![
                    "scrub".into(),
                    "resume".into(),
                    "-B".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsScrubStart { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec![
                    "scrub".into(),
                    "start".into(),
                    "-B".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsScrubCancel { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec!["scrub".into(), "cancel".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsScrubStatusPerDevice { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec![
                    "scrub".into(),
                    "status".into(),
                    "-d".into(),
                    "-R".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsDeviceStats { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec!["device".into(), "stats".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsDeviceStatsJson { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec![
                    "--format".into(),
                    "json".into(),
                    "device".into(),
                    "stats".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsBalanceStatus { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec!["balance".into(), "status".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsDeviceUsageRaw { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
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
                    program: "lsblk".to_owned(),
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
                program: "cryptsetup".to_owned(),
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
                program: "cryptsetup".to_owned(),
                args: vec!["isLuks".into(), device.clone()],
            },
            CmdRequest::CryptsetupClose { mapper } => CmdArgs {
                program: "cryptsetup".to_owned(),
                args: vec!["close".into(), mapper.clone()],
            },
            CmdRequest::BtrfsDeviceAdd {
                device,
                mount_point,
                force,
            } => {
                let mut args = vec!["device".into(), "add".into(), "--enqueue".into()];
                if *force {
                    args.push("-f".into());
                }
                args.push(device.clone());
                args.push(mount_point.0.clone());
                CmdArgs {
                    program: "btrfs".to_owned(),
                    args,
                }
            }
            CmdRequest::BtrfsDeviceRemove {
                device,
                mount_point,
            } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec![
                    "device".into(),
                    "remove".into(),
                    "--enqueue".into(),
                    device.clone(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsDeviceScan { device } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec!["device".into(), "scan".into(), device.clone()],
            },
            CmdRequest::BtrfsDeviceScanAll => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec!["device".into(), "scan".into()],
            },
            CmdRequest::BtrfsDeviceScanForget { devices } => {
                let mut args: Vec<String> = vec!["device".into(), "scan".into(), "--forget".into()];
                args.extend(devices.iter().cloned());
                CmdArgs {
                    program: "btrfs".to_owned(),
                    args,
                }
            }
            CmdRequest::WipefsBtrfs { device } => CmdArgs {
                program: "wipefs".to_owned(),
                args: vec![
                    "--all".into(),
                    "--types".into(),
                    "btrfs".into(),
                    device.clone(),
                ],
            },
            CmdRequest::BtrfsBalanceRaid1 { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
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
                program: "btrfs".to_owned(),
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
                program: "btrfs".to_owned(),
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
            CmdRequest::BtrfsBalanceResume { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec!["balance".into(), "resume".into(), mount_point.0.clone()],
            },
            CmdRequest::MkfsBtrfs { device } => CmdArgs {
                program: "mkfs.btrfs".to_owned(),
                args: vec![
                    "-d".into(),
                    "single".into(),
                    "-m".into(),
                    "dup".into(),
                    device.clone(),
                ],
            },
            CmdRequest::MkfsBtrfsRaid1 { devices } => {
                let mut args = vec!["-d".into(), "raid1".into(), "-m".into(), "raid1".into()];
                args.extend(devices.iter().cloned());
                CmdArgs {
                    program: "mkfs.btrfs".to_owned(),
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
                    program: "mount".to_owned(),
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
                    program: "mount".to_owned(),
                    args,
                }
            }
            CmdRequest::Umount { mount_point } => CmdArgs {
                program: "umount".to_owned(),
                args: vec![mount_point.0.clone()],
            },
            CmdRequest::MountpointCheck { path } => CmdArgs {
                program: "mountpoint".to_owned(),
                args: vec!["-q".into(), path.0.clone()],
            },
            CmdRequest::BtrfsReplaceStart {
                devid,
                target_device,
                mount_point,
            } => CmdArgs {
                program: "btrfs".to_owned(),
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
                program: "btrfs".to_owned(),
                // -1: print one snapshot and return immediately. Without this,
                // `btrfs replace status` loops with sleep(1) on the STARTED
                // state until the kernel reports FINISHED — see
                // reference/btrfs-progs/cmds/replace.c:451-505. Every braid
                // caller (idle, progress, recover) assumes a single-shot
                // read; without -1 they all block until the replace finishes,
                // which breaks the autosuspend integration in idle.rs.
                args: vec![
                    "replace".into(),
                    "status".into(),
                    "-1".into(),
                    mount_point.0.clone(),
                ],
            },
            CmdRequest::BtrfsFilesystemResize { devid, mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
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
                program: "cryptsetup".to_owned(),
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
                    program: "cryptsetup".to_owned(),
                    args,
                }
            }
            CmdRequest::CryptsetupTestPassphrase { device } => CmdArgs {
                program: "cryptsetup".to_owned(),
                args: vec![
                    "open".into(),
                    "--test-passphrase".into(),
                    "--key-file=-".into(),
                    device.clone(),
                ],
            },
            CmdRequest::SmartctlHealthJson { device } => CmdArgs {
                program: "smartctl".to_owned(),
                args: vec!["-H".into(), "-A".into(), device.clone(), "--json".into()],
            },
            CmdRequest::CryptsetupLuksDump { device } => CmdArgs {
                program: "cryptsetup".to_owned(),
                args: vec![
                    "luksDump".into(),
                    "--dump-json-metadata".into(),
                    device.clone(),
                ],
            },
            CmdRequest::CryptsetupLuksDumpText { device } => CmdArgs {
                program: "cryptsetup".to_owned(),
                args: vec!["luksDump".into(), device.clone()],
            },
            CmdRequest::CryptsetupLuksOpenKeyFile {
                device,
                mapper,
                key_file_path,
            } => CmdArgs {
                program: "cryptsetup".to_owned(),
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
                program: "cryptsetup".to_owned(),
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
                program: "cryptsetup".to_owned(),
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
                program: "btrfs".to_owned(),
                args: vec!["filesystem".into(), "usage".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsFilesystemDf { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec!["filesystem".into(), "df".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsDeviceUsage { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec!["device".into(), "usage".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsSubvolumeList { mount_point } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec!["subvolume".into(), "list".into(), mount_point.0.clone()],
            },
            CmdRequest::BtrfsSubvolumeShow { path } => CmdArgs {
                program: "btrfs".to_owned(),
                args: vec!["subvolume".into(), "show".into(), path.clone()],
            },
            CmdRequest::BraidBeepProbe { path } => CmdArgs {
                program: path.clone(),
                args: vec![],
            },
            CmdRequest::SystemctlIsActive { unit } => CmdArgs {
                program: "systemctl".to_owned(),
                args: vec!["is-active".into(), unit.clone()],
            },
            CmdRequest::UpscQuery { name } => CmdArgs {
                program: "upsc".to_owned(),
                args: vec![name.clone()],
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

fn signal_name(sig: i32) -> &'static str {
    match sig {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        _ => "unknown",
    }
}

fn output_to_raw(
    cmd_str: String,
    output: std::process::Output,
) -> Result<RawCommandOutput, CmdError> {
    let exit_status = match output.status.code() {
        Some(code) => code,
        None => {
            let sig = output.status.signal().unwrap_or(0);
            let name = signal_name(sig);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let detail = if stderr.is_empty() {
                format!("{cmd_str}: killed by signal {sig} ({name})")
            } else {
                format!("{cmd_str}: killed by signal {sig} ({name}): {stderr}")
            };
            return Err(CmdError::Failed(detail));
        }
    };

    Ok(RawCommandOutput {
        cmd: cmd_str,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit_status,
    })
}

pub struct RealRunner;

impl RealRunner {
    fn exec(cmd: &CmdArgs) -> Result<RawCommandOutput, CmdError> {
        let cmd_str = format!("{} {}", cmd.program, cmd.args.join(" "));
        // Force POSIX locale so error strings (strerror, %m) are always English —
        // braid matches stderr substrings for ENOSPC, device-busy, etc.
        let output = std::process::Command::new(&cmd.program)
            .args(&cmd.args)
            .env("LC_ALL", "C")
            .output()
            .map_err(|e| CmdError::Failed(format!("{cmd_str}: {e}")))?;

        output_to_raw(cmd_str, output)
    }

    fn exec_with_stdin(cmd: &CmdArgs, stdin_bytes: &[u8]) -> Result<RawCommandOutput, CmdError> {
        use std::io::Write;
        use std::process::Stdio;

        let cmd_str = format!("{} {}", cmd.program, cmd.args.join(" "));
        // Force POSIX locale so error strings (strerror, %m) are always English —
        // braid matches stderr substrings for ENOSPC, device-busy, etc.
        let mut child = std::process::Command::new(&cmd.program)
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

        output_to_raw(cmd_str, output)
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

#[derive(Clone)]
pub struct MockRunner {
    outputs: std::collections::HashMap<String, RawCommandOutput>,
    output_sequences: std::sync::Arc<
        std::sync::Mutex<
            std::collections::HashMap<String, std::collections::VecDeque<RawCommandOutput>>,
        >,
    >,
    stdin_expectations: std::collections::HashMap<String, Vec<u8>>,
    requests: std::sync::Arc<std::sync::Mutex<Vec<CmdRequest>>>,
}

impl Default for MockRunner {
    fn default() -> Self {
        Self {
            outputs: std::collections::HashMap::new(),
            output_sequences: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            stdin_expectations: std::collections::HashMap::new(),
            requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

impl MockRunner {
    pub fn with_output(mut self, request: CmdRequest, output: RawCommandOutput) -> Self {
        self.outputs.insert(format!("{request:?}"), output);
        self
    }

    pub fn with_output_sequence(self, request: CmdRequest, outputs: Vec<RawCommandOutput>) -> Self {
        self.output_sequences
            .lock()
            .expect("mock runner output sequence map poisoned")
            .insert(
                format!("{request:?}"),
                std::collections::VecDeque::from(outputs),
            );
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

    pub fn requests(&self) -> Vec<CmdRequest> {
        self.requests
            .lock()
            .expect("mock runner request log poisoned")
            .clone()
    }

    /// Test helper: stub `cryptsetup luksDump <device>` (text form) to
    /// emit a minimal LUKS2 header so `probe_config_disk`'s gateway
    /// version check passes. Use this alongside any existing
    /// `CryptsetupLuksUuid` mock that should reach `PresentLuks`.
    pub fn with_luks_dump_text_luks2(self, device: &str) -> Self {
        self.with_output(
            CmdRequest::CryptsetupLuksDumpText {
                device: device.into(),
            },
            RawCommandOutput {
                cmd: "cryptsetup luksDump".into(),
                stdout: "LUKS header information\nVersion:       \t2\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    /// Test helper: stub LUKS2 luksDump output for every device in the
    /// slice. Convenience wrapper around `with_luks_dump_text_luks2`.
    pub fn with_luks_dump_text_luks2_for(self, devices: &[&str]) -> Self {
        devices
            .iter()
            .fold(self, |acc, d| acc.with_luks_dump_text_luks2(d))
    }

    /// Test helper: stub `cryptsetup status <mapper>` to report the mapper
    /// as inactive (closed). Use for the common "mapper not yet opened"
    /// scenario. `probe_config_disk` uses this to set `mapper_open=false`
    /// without error.
    pub fn with_mapper_closed(self, mapper: &str) -> Self {
        self.with_output(
            CmdRequest::CryptsetupStatus {
                mapper: mapper.into(),
            },
            RawCommandOutput {
                cmd: format!("cryptsetup status {mapper}"),
                stdout: String::new(),
                stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
                exit_status: 4,
            },
        )
    }

    /// Test helper: stub every mapper in the slice as inactive.
    pub fn with_mappers_closed(self, mappers: &[&str]) -> Self {
        mappers
            .iter()
            .fold(self, |acc, m| acc.with_mapper_closed(m))
    }

    /// Test helper: stub `cryptsetup status <mapper>` as active with
    /// `underlying` as the backing device, AND stub
    /// `cryptsetup luksUUID <underlying>` to return `uuid`. Together these
    /// satisfy `probe_config_disk`'s mapper-backing verification for an
    /// already-open mapper whose backing LUKS UUID matches the configured
    /// disk.
    pub fn with_mapper_open(self, mapper: &str, underlying: &str, uuid: &str) -> Self {
        self.with_output(
            CmdRequest::CryptsetupStatus {
                mapper: mapper.into(),
            },
            RawCommandOutput {
                cmd: format!("cryptsetup status {mapper}"),
                stdout: format!(
                    "/dev/mapper/{mapper} is active and is in use.\n\
                     \ttype:    LUKS2\n\
                     \tcipher:  aes-xts-plain64\n\
                     \tdevice:  {underlying}\n\
                     \tsector size:  512\n"
                ),
                stderr: String::new(),
                exit_status: 0,
            },
        )
        .with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: underlying.into(),
            },
            RawCommandOutput {
                cmd: format!("cryptsetup luksUUID {underlying}"),
                stdout: format!("{uuid}\n"),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }
}

impl CommandRunner for MockRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        self.requests
            .lock()
            .expect("mock runner request log poisoned")
            .push(request.clone());
        let key = format!("{request:?}");
        let output = {
            let mut sequences = self
                .output_sequences
                .lock()
                .expect("mock runner output sequence map poisoned");
            let next = sequences.get_mut(&key).and_then(|queue| queue.pop_front());
            if sequences
                .get(&key)
                .is_some_and(std::collections::VecDeque::is_empty)
            {
                sequences.remove(&key);
            }
            next
        }
        .or_else(|| self.outputs.get(&key).cloned())
        .ok_or(CmdError::MissingMock)?;
        if let CmdRequest::CryptsetupLuksHeaderBackup { backup_path, .. } = request
            && output.exit_status == 0
        {
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
        self.requests
            .lock()
            .expect("mock runner request log poisoned")
            .push(request.clone());
        if let Some(expected) = self.stdin_expectations.get(&key) {
            assert_eq!(stdin, expected.as_slice(), "stdin mismatch for {key}");
        }
        let output = {
            let mut sequences = self
                .output_sequences
                .lock()
                .expect("mock runner output sequence map poisoned");
            let next = sequences.get_mut(&key).and_then(|queue| queue.pop_front());
            if sequences
                .get(&key)
                .is_some_and(std::collections::VecDeque::is_empty)
            {
                sequences.remove(&key);
            }
            next
        }
        .or_else(|| self.outputs.get(&key).cloned())
        .ok_or(CmdError::MissingMock)?;
        Ok(output)
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
    fn mock_runner_requests_records_run_and_run_with_stdin_in_order() {
        let req1 = CmdRequest::LsblkJson;
        let req2 = CmdRequest::CryptsetupTestPassphrase {
            device: "/dev/x".to_owned(),
        };
        let mock = MockRunner::default()
            .with_output(
                req1.clone(),
                RawCommandOutput {
                    cmd: "lsblk --json".to_owned(),
                    stdout: "{\"blockdevices\":[]}".to_owned(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_output_stdin(
                req2.clone(),
                b"secret".to_vec(),
                RawCommandOutput {
                    cmd: "cryptsetup open --test-passphrase /dev/x".to_owned(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            );

        mock.run(&req1).expect("seeded run mock");
        mock.run_with_stdin(&req2, b"secret")
            .expect("seeded run_with_stdin mock");

        assert_eq!(mock.requests(), vec![req1, req2]);
    }

    #[test]
    fn mock_runner_requests_records_missing_mock_calls_too() {
        let req = CmdRequest::BtrfsDeviceScanAll;
        let mock = MockRunner::default();

        let err = mock.run(&req).expect_err("missing mock should error");

        assert!(matches!(err, CmdError::MissingMock));
        assert_eq!(mock.requests(), vec![req]);
    }

    #[test]
    #[should_panic(expected = "stdin mismatch")]
    fn mock_runner_run_with_stdin_panics_on_stdin_mismatch_unchanged() {
        let req = CmdRequest::CryptsetupTestPassphrase {
            device: "/dev/x".to_owned(),
        };
        let mock = MockRunner::default().with_output_stdin(
            req.clone(),
            b"secret".to_vec(),
            RawCommandOutput {
                cmd: "cryptsetup open --test-passphrase /dev/x".to_owned(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        );

        let _ = mock.run_with_stdin(&req, b"wrong");
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
    // Intent: BtrfsDeviceScanForget emits `btrfs device scan --forget
    // <dev>...`, never the no-arg form that forgets every scanned btrfs
    // device on the host.
    // Why: the no-arg form is kernel-global (volumes.c
    // btrfs_free_stale_devices with devt=0). Pool-scoped forget MUST pass
    // explicit device paths. A regression to the bare form is not caught
    // by typed-CmdRequest inspection in lock.rs (an accidental to_args
    // that dropped `devices` would still typecheck), so pin it here at
    // the argv layer.
    // Scenario: lock builds [/dev/mapper/braid-aaa, /dev/mapper/braid-bbb];
    // to_argv() appends both after --forget.
    fn btrfs_device_scan_forget_generates_scoped_argv() {
        let cmd = CmdRequest::BtrfsDeviceScanForget {
            devices: vec![
                "/dev/mapper/braid-aaa".into(),
                "/dev/mapper/braid-bbb".into(),
            ],
        }
        .to_argv();
        assert_eq!(cmd.program, "btrfs");
        assert_eq!(
            cmd.args,
            vec![
                "device",
                "scan",
                "--forget",
                "/dev/mapper/braid-aaa",
                "/dev/mapper/braid-bbb",
            ]
        );
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
    // Intent: BtrfsBalanceResume generates `btrfs balance resume <mp>` with no
    // convert filters of its own.
    // Why: the kernel reuses the convert filters persisted in the chunk tree
    // BALANCE_ITEM by the original balance start. Adding our own -dconvert /
    // -mconvert here would either be ignored (best case) or conflict with the
    // stored filters (worst case). recover relies on this command picking up
    // exactly where the interrupted balance left off.
    // Scenario: forced shutdown during a post-add RAID1 conversion leaves a
    // paused balance; `cmd_recover` issues `btrfs balance resume` to drain it.
    fn btrfs_balance_resume_generates_correct_argv() {
        let cmd = CmdRequest::BtrfsBalanceResume {
            mount_point: MountPoint("/mnt/storage".to_owned()),
        }
        .to_argv();
        assert_eq!(cmd.program, "btrfs");
        assert_eq!(cmd.args, vec!["balance", "resume", "/mnt/storage"]);
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
    // Intent: Lock the `-1` flag into BtrfsReplaceStatus's argv so the cmd
    // helper always asks btrfs for a single status snapshot.
    // Why: Without `-1`, btrfs replace status loops with sleep(1) on the
    // STARTED state until the kernel reports FINISHED -- see
    // reference/btrfs-progs/cmds/replace.c:451-505. The remaining braid
    // callers (progress, recover) would block for the entire duration of
    // an in-flight replace. `braid idle` used to be one of those callers;
    // it now reads /sys/fs/btrfs/<fsid>/exclusive_operation instead, but
    // this contract still matters for the rest.
    // Scenario: a future refactor strips `-1` from the args (e.g. while
    // adding a continuous-poll variant). This test fails immediately.
    fn btrfs_replace_status_includes_minus_one() {
        let cmd = CmdRequest::BtrfsReplaceStatus {
            mount_point: MountPoint("/mnt/storage".to_owned()),
        }
        .to_argv();
        assert_eq!(cmd.program, "btrfs");
        assert_eq!(
            cmd.args,
            vec!["replace", "status", "-1", "/mnt/storage"],
            "BtrfsReplaceStatus must pass `-1` to avoid blocking until the \
             replace finishes — see reference/btrfs-progs/cmds/replace.c:451-505",
        );
    }

    #[test]
    // Intent: MkfsBtrfsRaid1 generates correct argv with -d raid1 -m raid1 and all devices.
    // Why: incorrect mkfs arguments could create a single-profile filesystem
    // instead of RAID1; -f is intentionally absent so mkfs.btrfs's libblkid
    // signature check remains the final backstop against existing filesystems.
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
     * Why: implicit profiles make braid's storage intent ambiguous and ignore upstream guidance;
     * -f is intentionally absent so mkfs.btrfs's own signature check remains active.
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
            vec!["-d", "single", "-m", "dup", "/dev/mapper/braid-disk1"]
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
            force: false,
        }
        .to_argv()
        .to_shell_string();
        assert_eq!(
            s,
            "btrfs device add --enqueue /dev/mapper/braid-aaa /mnt/storage"
        );
    }

    #[test]
    fn btrfs_device_add_force_renders_f_flag() {
        let s = CmdRequest::BtrfsDeviceAdd {
            device: "/dev/mapper/braid-aaa".to_owned(),
            mount_point: MountPoint("/mnt/storage".to_owned()),
            force: true,
        }
        .to_argv()
        .to_shell_string();
        assert_eq!(
            s,
            "btrfs device add --enqueue -f /dev/mapper/braid-aaa /mnt/storage"
        );
    }

    #[test]
    fn wipefs_btrfs_renders_narrow_signature_wipe() {
        let s = CmdRequest::WipefsBtrfs {
            device: "/dev/mapper/braid-aaa".to_owned(),
        }
        .to_argv()
        .to_shell_string();
        assert_eq!(s, "wipefs --all --types btrfs /dev/mapper/braid-aaa");
    }

    #[test]
    // Intent: to_shell_string quotes args containing shell-significant characters.
    // Why: --key-file=- contains = which shell_words may quote; paths with spaces
    // must be quoted to remain copy-pasteable.
    // Scenario: cryptsetup luksFormat with key-file flag.
    fn to_shell_string_quotes_special_chars() {
        let s = CmdArgs {
            program: "cryptsetup".to_owned(),
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

    // Intent: output_to_raw returns CmdError::Failed with signal number and name
    //   when the child was killed by a signal.
    // Why: Without this, signal kills silently become exit_status=-1, producing
    //   confusing "failed (exit -1)" messages with no indication of what happened.
    // Scenario: OOM-killer sends SIGKILL to cryptsetup during luksOpen — braid
    //   must report the signal, not a mysterious -1.
    #[test]
    fn output_to_raw_signal_killed_returns_error() {
        use std::process::ExitStatus;

        let status = ExitStatus::from_raw(libc::SIGKILL);
        let output = std::process::Output {
            status,
            stdout: Vec::new(),
            stderr: b"partial output".to_vec(),
        };
        let result = output_to_raw("cryptsetup luksOpen /dev/sda".into(), output);
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("signal 9"), "expected signal 9 in: {msg}");
        assert!(msg.contains("SIGKILL"), "expected SIGKILL in: {msg}");
        assert!(msg.contains("partial output"), "expected stderr in: {msg}");
    }

    // Intent: output_to_raw returns Ok(RawCommandOutput) for normal exits.
    // Why: Refactoring exec()/exec_with_stdin() to use output_to_raw must not
    //   change behavior for the normal (non-signal) path.
    // Scenario: Any normal command execution — exit 0 or non-zero exit code.
    #[test]
    fn output_to_raw_normal_exit_returns_output() {
        use std::process::ExitStatus;

        let status = ExitStatus::from_raw(5 << 8);
        let output = std::process::Output {
            status,
            stdout: b"some stdout".to_vec(),
            stderr: b"some stderr".to_vec(),
        };
        let raw = output_to_raw("test cmd".into(), output).unwrap();
        assert_eq!(raw.exit_status, 5);
        assert_eq!(raw.stdout, "some stdout");
        assert_eq!(raw.stderr, "some stderr");
    }

    // Intent: signal_name returns correct POSIX names for common signals and
    //   "unknown" for unrecognized values.
    // Why: Wrong signal names in error messages would mislead debugging.
    // Scenario: User sees "killed by signal 9 (SIGKILL)" — the name must match.
    #[test]
    fn signal_name_maps_known_signals() {
        assert_eq!(signal_name(libc::SIGKILL), "SIGKILL");
        assert_eq!(signal_name(libc::SIGTERM), "SIGTERM");
        assert_eq!(signal_name(libc::SIGPIPE), "SIGPIPE");
        assert_eq!(signal_name(999), "unknown");
    }

    // --- cryptsetup keyfile-size asymmetry pins -------------------------------
    //
    // The stdin-fed passphrase variants (LuksOpen / TestPassphrase / LuksFormat)
    // and the file-fed keyfile variants (LuksOpenKeyFile / TestKeyFile /
    // LuksAddKeyFile) intentionally disagree on --keyfile-size. The passphrase
    // variants must NOT pass it; the keyfile variants must pass 4096. Tests
    // below pin the full argv for each variant so a future "normalize the
    // asymmetry" patch fails immediately with a pointer to this comment.
    //
    // Why the asymmetry: --keyfile-size N, combined with --key-file=- on piped
    // stdin, makes cryptsetup's non-interactive branch (see
    // reference/cryptsetup/src/utils_password.c:296-302 and
    // reference/cryptsetup/lib/utils.c:314-317) demand exactly N bytes and fail
    // with "Cannot read requested amount of data" otherwise. User passphrases
    // are variable-length strings (braid feeds passphrase.as_bytes() unpadded
    // from cli/src/luks.rs), so pinning any N breaks unlock. The keyfile side
    // feeds a fixed 4096-byte binary blob written by LuksAddKeyFile
    // (--new-keyfile-size 4096), so pinning matches the enrollment contract
    // and makes a truncated or grown key file fail fast.

    #[test]
    // Intent: CryptsetupLuksOpen (passphrase-via-stdin) must NOT carry
    // --keyfile-size. Pin full argv.
    // Why: see the block comment above -- --keyfile-size on piped stdin would
    // break every passphrase unlock. The keyfile companion test
    // (cryptsetup_luks_open_key_file_sets_keyfile_size_4096) pins the inverse
    // for the file-fed variant.
    // Scenario: a future cleanup PR copies --keyfile-size 4096 from the
    // keyfile variant into this argv "for consistency". This test fails and
    // points at the block comment.
    fn cryptsetup_luks_open_omits_keyfile_size() {
        let cmd = CmdRequest::CryptsetupLuksOpen {
            device: "/dev/disk/by-id/disk1".to_owned(),
            mapper: "braid-disk1".to_owned(),
        }
        .to_argv();
        assert_eq!(cmd.program, "cryptsetup");
        assert_eq!(
            cmd.args,
            vec![
                "open",
                "--type",
                "luks",
                "--key-file=-",
                "--perf-no_read_workqueue",
                "--perf-no_write_workqueue",
                "/dev/disk/by-id/disk1",
                "braid-disk1",
            ]
        );
        assert!(
            !cmd.args.iter().any(|a| a == "--keyfile-size"),
            "CryptsetupLuksOpen reads a variable-length passphrase from stdin \
             and must NOT set --keyfile-size -- see block comment in cmd.rs \
             tests module above cryptsetup_luks_open_omits_keyfile_size"
        );
    }

    #[test]
    // Intent: CryptsetupTestPassphrase (passphrase-via-stdin) must NOT carry
    // --keyfile-size. Pin full argv.
    // Why: same as cryptsetup_luks_open_omits_keyfile_size -- stdin variants
    // break under --keyfile-size.
    // Scenario: a cleanup PR copies --keyfile-size 4096 from TestKeyFile. This
    // test fails immediately.
    fn cryptsetup_test_passphrase_omits_keyfile_size() {
        let cmd = CmdRequest::CryptsetupTestPassphrase {
            device: "/dev/disk/by-id/disk1".to_owned(),
        }
        .to_argv();
        assert_eq!(cmd.program, "cryptsetup");
        assert_eq!(
            cmd.args,
            vec![
                "open",
                "--test-passphrase",
                "--key-file=-",
                "/dev/disk/by-id/disk1",
            ]
        );
        assert!(
            !cmd.args.iter().any(|a| a == "--keyfile-size"),
            "CryptsetupTestPassphrase reads a variable-length passphrase from \
             stdin and must NOT set --keyfile-size"
        );
    }

    #[test]
    // Intent: CryptsetupLuksFormat (passphrase-via-stdin) must NOT carry
    // --keyfile-size. Pin full argv for the empty-extra-opts shape.
    // Why: luksFormat consumes the initial passphrase from stdin; forcing a
    // fixed read size would break first-time format the same way it would
    // break unlock.
    // Scenario: a cleanup PR normalizes all cryptsetup variants. Fails here.
    fn cryptsetup_luks_format_omits_keyfile_size() {
        let cmd = CmdRequest::CryptsetupLuksFormat {
            device: "/dev/disk/by-id/disk1".to_owned(),
            extra_opts: vec![],
        }
        .to_argv();
        assert_eq!(cmd.program, "cryptsetup");
        assert_eq!(
            cmd.args,
            vec![
                "luksFormat",
                "--type",
                "luks2",
                "--batch-mode",
                "--key-file=-",
                "/dev/disk/by-id/disk1",
            ]
        );
        assert!(
            !cmd.args.iter().any(|a| a == "--keyfile-size"),
            "CryptsetupLuksFormat reads the initial passphrase from stdin and \
             must NOT set --keyfile-size"
        );
    }

    #[test]
    // Intent: CryptsetupLuksOpenKeyFile (file-fed) MUST carry
    // --keyfile-size 4096. Pin full argv.
    // Why: the keyfile is a fixed 4096-byte binary blob written by
    // CryptsetupLuksAddKeyFile (--new-keyfile-size 4096). Pinning the read
    // length to the enrollment size means a truncated or grown keyfile fails
    // fast instead of silently deriving a different key. Dropping this flag
    // would let a tampered keyfile unlock (or silently change the effective
    // key bytes). See the block comment above for the asymmetry rationale.
    // Scenario: a refactor strips --keyfile-size "for symmetry" with the
    // passphrase variants. This test fails and names the enrollment contract.
    fn cryptsetup_luks_open_key_file_sets_keyfile_size_4096() {
        let cmd = CmdRequest::CryptsetupLuksOpenKeyFile {
            device: "/dev/disk/by-id/disk1".to_owned(),
            mapper: "braid-disk1".to_owned(),
            key_file_path: "/var/lib/braid/keyfiles/braid-disk1.key".to_owned(),
        }
        .to_argv();
        assert_eq!(cmd.program, "cryptsetup");
        assert_eq!(
            cmd.args,
            vec![
                "open",
                "--type",
                "luks",
                "--key-file",
                "/var/lib/braid/keyfiles/braid-disk1.key",
                "--keyfile-size",
                "4096",
                "--perf-no_read_workqueue",
                "--perf-no_write_workqueue",
                "/dev/disk/by-id/disk1",
                "braid-disk1",
            ]
        );
    }

    #[test]
    // Intent: CryptsetupTestKeyFile (file-fed) MUST carry --keyfile-size 4096.
    // Pin full argv.
    // Why: same reasoning as cryptsetup_luks_open_key_file_sets_keyfile_size_4096
    // -- the test-passphrase probe must read exactly the enrolled byte count.
    // Scenario: a refactor drops --keyfile-size here while leaving the real
    // open variant alone, causing a confirmation/actual mismatch where Test
    // would succeed on a truncated file but Open would refuse.
    fn cryptsetup_test_key_file_sets_keyfile_size_4096() {
        let cmd = CmdRequest::CryptsetupTestKeyFile {
            device: "/dev/disk/by-id/disk1".to_owned(),
            key_file_path: "/var/lib/braid/keyfiles/braid-disk1.key".to_owned(),
        }
        .to_argv();
        assert_eq!(cmd.program, "cryptsetup");
        assert_eq!(
            cmd.args,
            vec![
                "open",
                "--test-passphrase",
                "--key-file",
                "/var/lib/braid/keyfiles/braid-disk1.key",
                "--keyfile-size",
                "4096",
                "/dev/disk/by-id/disk1",
            ]
        );
    }

    #[test]
    // Intent: CryptsetupLuksAddKeyFile MUST carry --new-keyfile-size 4096.
    // Pin full argv.
    // Why: this call is the source-of-truth for the 4096 byte count that the
    // Open/Test keyfile variants then rely on. Changing it here without also
    // changing the reader flags would silently desynchronize enrollment vs
    // unlock. The key-slot 1 is also load-bearing -- slot 0 holds the user
    // passphrase and must not be overwritten by keyfile enrollment.
    // Scenario: someone bumps the enrollment size or drops the slot. Both
    // directions fail here.
    fn cryptsetup_luks_add_key_file_sets_new_keyfile_size_4096() {
        let cmd = CmdRequest::CryptsetupLuksAddKeyFile {
            device: "/dev/disk/by-id/disk1".to_owned(),
            key_file_path: "/var/lib/braid/keyfiles/braid-disk1.key".to_owned(),
        }
        .to_argv();
        assert_eq!(cmd.program, "cryptsetup");
        assert_eq!(
            cmd.args,
            vec![
                "luksAddKey",
                "--key-slot",
                "1",
                "--new-keyfile-size",
                "4096",
                "/dev/disk/by-id/disk1",
                "/var/lib/braid/keyfiles/braid-disk1.key",
            ]
        );
    }
}
