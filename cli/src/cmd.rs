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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdRequest {
    LsblkJson,
    FindmntJson {
        mount_point: String,
    },
    BtrfsFilesystemDfJson {
        mount_point: String,
    },
    BtrfsFilesystemShow {
        mount_point: String,
    },
    CryptsetupStatus {
        mapper: String,
    },
    CryptsetupLuksUuid {
        device: String,
    },
    BtrfsFilesystemUsageRaw {
        mount_point: String,
    },
    BtrfsScrubStatus {
        mount_point: String,
    },
    BtrfsScrubStatusPerDevice {
        mount_point: String,
    },
    BtrfsDeviceStats {
        mount_point: String,
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
        mount_point: String,
    },
    BtrfsDeviceRemove {
        device: String,
        mount_point: String,
    },
    BtrfsDeviceRemoveMissing {
        mount_point: String,
    },
    BtrfsDeviceScan {
        device: String,
    },
    BtrfsDeviceScanAll,
    BtrfsBalanceRaid1 {
        mount_point: String,
    },
    BtrfsBalanceSingle {
        mount_point: String,
    },
    MkfsBtrfs {
        device: String,
    },
    Mount {
        device: String,
        mount_point: String,
    },
    MountWithOptions {
        device: String,
        mount_point: String,
        options: Vec<String>,
    },
    Umount {
        mount_point: String,
    },
    MountpointCheck {
        path: String,
    },
    // Polling commands for progress monitoring
    BtrfsBalanceStatus {
        mount_point: String,
    },
    BtrfsDeviceUsageRaw {
        mount_point: String,
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
    // btrfs replace commands
    BtrfsReplaceStart {
        devid: u64,
        target_device: String,
        mount_point: String,
    },
    BtrfsReplaceStatus {
        mount_point: String,
    },
    BtrfsFilesystemResize {
        devid: u64,
        mount_point: String,
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
    fn exec(cmd: &str, args: &[&str]) -> Result<RawCommandOutput, CmdError> {
        let cmd_str = format!("{} {}", cmd, args.join(" "));
        let output = std::process::Command::new(cmd)
            .args(args)
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

    fn exec_with_stdin(
        cmd: &str,
        args: &[&str],
        stdin_bytes: &[u8],
    ) -> Result<RawCommandOutput, CmdError> {
        use std::io::Write;
        use std::process::Stdio;

        let cmd_str = format!("{} {}", cmd, args.join(" "));
        let mut child = std::process::Command::new(cmd)
            .args(args)
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
        match request {
            CmdRequest::LsblkJson => RealRunner::exec(
                "lsblk",
                &[
                    "--json",
                    "--bytes",
                    "--output",
                    "NAME,TYPE,SIZE,MODEL,SERIAL,UUID,ROTA,TRAN",
                ],
            ),
            CmdRequest::FindmntJson { mount_point } => RealRunner::exec(
                "findmnt",
                &[
                    "--json",
                    "--output",
                    "TARGET,SOURCE,FSTYPE,OPTIONS",
                    "--mountpoint",
                    mount_point,
                ],
            ),
            CmdRequest::BtrfsFilesystemShow { mount_point } => {
                RealRunner::exec("btrfs", &["filesystem", "show", mount_point])
            }
            CmdRequest::CryptsetupStatus { mapper } => {
                RealRunner::exec("cryptsetup", &["status", mapper])
            }
            CmdRequest::CryptsetupLuksUuid { device } => {
                RealRunner::exec("cryptsetup", &["luksUUID", device])
            }
            CmdRequest::BtrfsFilesystemDfJson { mount_point } => RealRunner::exec(
                "btrfs",
                &["--format", "json", "filesystem", "df", mount_point],
            ),
            CmdRequest::BtrfsFilesystemUsageRaw { mount_point } => {
                RealRunner::exec("btrfs", &["filesystem", "usage", "--raw", mount_point])
            }
            CmdRequest::BtrfsScrubStatus { mount_point } => {
                RealRunner::exec("btrfs", &["scrub", "status", mount_point])
            }
            CmdRequest::BtrfsScrubStatusPerDevice { mount_point } => {
                RealRunner::exec("btrfs", &["scrub", "status", "-d", "-R", mount_point])
            }
            CmdRequest::BtrfsDeviceStats { mount_point } => {
                RealRunner::exec("btrfs", &["device", "stats", mount_point])
            }
            CmdRequest::BtrfsBalanceStatus { mount_point } => {
                RealRunner::exec("btrfs", &["balance", "status", mount_point])
            }
            CmdRequest::BtrfsDeviceUsageRaw { mount_point } => {
                RealRunner::exec("btrfs", &["device", "usage", "--raw", mount_point])
            }
            CmdRequest::LsblkField { device, field } => {
                let field_name = match field {
                    LsblkFieldKind::Model => "MODEL",
                    LsblkFieldKind::Serial => "SERIAL",
                };
                RealRunner::exec("lsblk", &["-ndo", field_name, device])
            }
            CmdRequest::CryptsetupLuksOpen { device, mapper } => {
                // Passphrase must be piped via run_with_stdin, not here.
                // Calling run() for luksOpen is an error — return a failure.
                Err(CmdError::Failed(format!(
                    "CryptsetupLuksOpen must use run_with_stdin (device={device}, mapper={mapper})"
                )))
            }
            CmdRequest::CryptsetupIsLuks { device } => {
                RealRunner::exec("cryptsetup", &["isLuks", device])
            }
            CmdRequest::CryptsetupClose { mapper } => {
                RealRunner::exec("cryptsetup", &["close", mapper])
            }
            CmdRequest::BtrfsDeviceAdd {
                device,
                mount_point,
            } => RealRunner::exec("btrfs", &["device", "add", "-f", device, mount_point]),
            CmdRequest::BtrfsDeviceRemove {
                device,
                mount_point,
            } => RealRunner::exec("btrfs", &["device", "remove", device, mount_point]),
            CmdRequest::BtrfsDeviceRemoveMissing { mount_point } => {
                RealRunner::exec("btrfs", &["device", "remove", "missing", mount_point])
            }
            CmdRequest::BtrfsDeviceScan { device } => {
                RealRunner::exec("btrfs", &["device", "scan", device])
            }
            CmdRequest::BtrfsDeviceScanAll => RealRunner::exec("btrfs", &["device", "scan"]),
            CmdRequest::BtrfsBalanceRaid1 { mount_point } => RealRunner::exec(
                "btrfs",
                &[
                    "balance",
                    "start",
                    "-dconvert=raid1",
                    "-mconvert=raid1",
                    mount_point,
                ],
            ),
            CmdRequest::BtrfsBalanceSingle { mount_point } => RealRunner::exec(
                "btrfs",
                &[
                    "balance",
                    "start",
                    "-dconvert=single",
                    // Important: use dup for metadata when converting to single
                    "-mconvert=dup",
                    "-f",
                    mount_point,
                ],
            ),
            CmdRequest::MkfsBtrfs { device } => RealRunner::exec("mkfs.btrfs", &["-f", device]),
            CmdRequest::Mount {
                device,
                mount_point,
            } => RealRunner::exec("mount", &[device, mount_point]),
            CmdRequest::MountWithOptions {
                device,
                mount_point,
                options,
            } => {
                let mut args = Vec::new();
                let opts_str = options.join(",");
                if !options.is_empty() {
                    args.push("-o");
                    args.push(&opts_str);
                }
                args.push(device);
                args.push(mount_point);
                RealRunner::exec("mount", &args)
            }
            CmdRequest::Umount { mount_point } => RealRunner::exec("umount", &[mount_point]),
            CmdRequest::MountpointCheck { path } => RealRunner::exec("mountpoint", &["-q", path]),
            CmdRequest::BtrfsReplaceStart {
                devid,
                target_device,
                mount_point,
            } => {
                let devid_str = devid.to_string();
                RealRunner::exec(
                    "btrfs",
                    &[
                        "replace",
                        "start",
                        "-f",
                        "-B",
                        &devid_str,
                        target_device,
                        mount_point,
                    ],
                )
            }
            CmdRequest::BtrfsReplaceStatus { mount_point } => {
                RealRunner::exec("btrfs", &["replace", "status", mount_point])
            }
            CmdRequest::BtrfsFilesystemResize { devid, mount_point } => {
                let resize_arg = format!("{devid}:max");
                RealRunner::exec("btrfs", &["filesystem", "resize", &resize_arg, mount_point])
            }
            CmdRequest::CryptsetupLuksHeaderBackup {
                device,
                backup_path,
            } => RealRunner::exec(
                "cryptsetup",
                &[
                    "luksHeaderBackup",
                    "--header-backup-file",
                    backup_path,
                    device,
                ],
            ),
            CmdRequest::CryptsetupLuksFormat { device, extra_opts } => {
                // Passphrase must be piped via run_with_stdin, not here.
                let _ = (device, extra_opts);
                Err(CmdError::Failed(
                    "CryptsetupLuksFormat must use run_with_stdin".to_owned(),
                ))
            }
            CmdRequest::CryptsetupTestPassphrase { device } => {
                // Passphrase must be piped via run_with_stdin, not here.
                let _ = device;
                Err(CmdError::Failed(
                    "CryptsetupTestPassphrase must use run_with_stdin".to_owned(),
                ))
            }
            CmdRequest::SmartctlHealthJson { device } => {
                RealRunner::exec("smartctl", &["-H", "-A", device, "--json"])
            }
            CmdRequest::CryptsetupLuksDump { device } => {
                RealRunner::exec("cryptsetup", &["luksDump", "--dump-json-metadata", device])
            }
            CmdRequest::CryptsetupLuksOpenKeyFile {
                device,
                mapper,
                key_file_path,
            } => RealRunner::exec(
                "cryptsetup",
                &[
                    "open",
                    "--type",
                    "luks",
                    "--key-file",
                    key_file_path,
                    "--keyfile-size",
                    "4096",
                    "--perf-no_read_workqueue",
                    "--perf-no_write_workqueue",
                    device,
                    mapper,
                ],
            ),
            CmdRequest::CryptsetupTestKeyFile {
                device,
                key_file_path,
            } => RealRunner::exec(
                "cryptsetup",
                &[
                    "open",
                    "--test-passphrase",
                    "--key-file",
                    key_file_path,
                    "--keyfile-size",
                    "4096",
                    device,
                ],
            ),
            CmdRequest::CryptsetupLuksAddKeyFile {
                device,
                key_file_path,
            } => {
                // Passphrase must be piped via run_with_stdin, not here.
                let _ = (device, key_file_path);
                Err(CmdError::Failed(
                    "CryptsetupLuksAddKeyFile must use run_with_stdin".to_owned(),
                ))
            }
        }
    }

    fn run_with_stdin(
        &self,
        request: &CmdRequest,
        stdin: &[u8],
    ) -> Result<RawCommandOutput, CmdError> {
        match request {
            CmdRequest::CryptsetupLuksOpen { device, mapper } => RealRunner::exec_with_stdin(
                "cryptsetup",
                &[
                    "open",
                    // This just means we can universally open LUKS1 and LUKS2
                    "--type",
                    "luks",
                    "--key-file=-",
                    // Bypass dm-crypt's internal workqueues — they add 3-4x queuing
                    // overhead regardless of disk type (HDD or SSD). Requires kernel >= 5.9.
                    "--perf-no_read_workqueue",
                    "--perf-no_write_workqueue",
                    device,
                    mapper,
                ],
                stdin,
            ),
            CmdRequest::CryptsetupLuksFormat { device, extra_opts } => {
                let mut args: Vec<&str> = vec!["luksFormat", "--batch-mode", "--key-file=-"];
                for opt in extra_opts {
                    args.push(opt.as_str());
                }
                args.push(device.as_str());
                RealRunner::exec_with_stdin("cryptsetup", &args, stdin)
            }
            CmdRequest::CryptsetupTestPassphrase { device } => RealRunner::exec_with_stdin(
                "cryptsetup",
                &["open", "--test-passphrase", "--key-file=-", device],
                stdin,
            ),
            CmdRequest::CryptsetupLuksAddKeyFile {
                device,
                key_file_path,
            } => RealRunner::exec_with_stdin(
                "cryptsetup",
                &[
                    "luksAddKey",
                    "--key-slot",
                    "1",
                    "--new-keyfile-size",
                    "4096",
                    device,
                    key_file_path,
                ],
                stdin,
            ),
            _ => {
                // For non-stdin commands, delegate to run() and ignore stdin.
                self.run(request)
            }
        }
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
        self.outputs
            .get(&format!("{request:?}"))
            .cloned()
            .ok_or(CmdError::MissingMock)
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
}
