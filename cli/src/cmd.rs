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
    FindmntJson { mount_point: String },
    BtrfsFilesystemDfJson { mount_point: String },
    BtrfsFilesystemShow { mount_point: String },
    CryptsetupStatus { mapper: String },
    CryptsetupLuksUuid { device: String },
    BtrfsFilesystemUsageRaw { mount_point: String },
    BtrfsScrubStatus { mount_point: String },
    BtrfsDeviceStats { mount_point: String },
    LsblkField { device: String, field: LsblkFieldKind },
}

#[derive(Debug, Error)]
pub enum CmdError {
    #[error("command failed: {0}")]
    Failed(String),
    #[error("mock output missing for request")]
    MissingMock,
}

pub trait CommandRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError>;
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
}

impl CommandRunner for RealRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        match request {
            CmdRequest::LsblkJson => {
                RealRunner::exec("lsblk", &["--json", "--bytes", "--output", "NAME,TYPE,SIZE,MODEL,SERIAL,UUID"])
            }
            CmdRequest::FindmntJson { mount_point } => {
                RealRunner::exec("findmnt", &["--json", "--output", "TARGET,SOURCE,FSTYPE", "-T", mount_point])
            }
            CmdRequest::BtrfsFilesystemShow { mount_point } => {
                RealRunner::exec("btrfs", &["filesystem", "show", mount_point])
            }
            CmdRequest::CryptsetupStatus { mapper } => {
                RealRunner::exec("cryptsetup", &["status", mapper])
            }
            CmdRequest::CryptsetupLuksUuid { device } => {
                RealRunner::exec("cryptsetup", &["luksUUID", device])
            }
            CmdRequest::BtrfsFilesystemDfJson { mount_point } => {
                RealRunner::exec("btrfs", &["--format", "json", "filesystem", "df", mount_point])
            }
            CmdRequest::BtrfsFilesystemUsageRaw { mount_point } => {
                RealRunner::exec("btrfs", &["filesystem", "usage", "--raw", mount_point])
            }
            CmdRequest::BtrfsScrubStatus { mount_point } => {
                RealRunner::exec("btrfs", &["scrub", "status", mount_point])
            }
            CmdRequest::BtrfsDeviceStats { mount_point } => {
                RealRunner::exec("btrfs", &["device", "stats", mount_point])
            }
            CmdRequest::LsblkField { device, field } => {
                let field_name = match field {
                    LsblkFieldKind::Model => "MODEL",
                    LsblkFieldKind::Serial => "SERIAL",
                };
                RealRunner::exec("lsblk", &["-ndo", field_name, device])
            }
        }
    }
}

#[derive(Default)]
pub struct MockRunner {
    outputs: std::collections::HashMap<String, RawCommandOutput>,
}

impl MockRunner {
    pub fn with_output(mut self, request: CmdRequest, output: RawCommandOutput) -> Self {
        self.outputs.insert(format!("{request:?}"), output);
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
    fn cmd_request_declares_expected_commands() {
        let all = vec![
            CmdRequest::LsblkJson,
            CmdRequest::FindmntJson {
                mount_point: "/mnt/storage".to_owned(),
            },
            CmdRequest::BtrfsFilesystemDfJson {
                mount_point: "/mnt/storage".to_owned(),
            },
            CmdRequest::BtrfsFilesystemShow {
                mount_point: "/mnt/storage".to_owned(),
            },
            CmdRequest::CryptsetupStatus {
                mapper: "disk1".to_owned(),
            },
            CmdRequest::CryptsetupLuksUuid {
                device: "/dev/vda".to_owned(),
            },
            CmdRequest::BtrfsFilesystemUsageRaw {
                mount_point: "/mnt/storage".to_owned(),
            },
            CmdRequest::BtrfsScrubStatus {
                mount_point: "/mnt/storage".to_owned(),
            },
            CmdRequest::BtrfsDeviceStats {
                mount_point: "/mnt/storage".to_owned(),
            },
            CmdRequest::LsblkField {
                device: "/dev/vda".to_owned(),
                field: LsblkFieldKind::Model,
            },
        ];

        assert_eq!(all.len(), 10);
    }
}
