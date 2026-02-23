use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCommandOutput {
    pub cmd: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_status: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdRequest {
    LsblkJson,
    FindmntJson { mount_point: String },
    BtrfsFilesystemDfJson { mount_point: String },
    BtrfsFilesystemShow { mount_point: String },
    CryptsetupStatus { mapper: String },
    CryptsetupLuksUuid { device: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LsblkJson {
    pub blockdevices: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindmntJson {
    pub filesystems: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtrfsDfJson {
    pub filesystem_df: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdOutput {
    Lsblk(LsblkJson),
    Findmnt(FindmntJson),
    BtrfsDf(BtrfsDfJson),
    BtrfsShowRaw(String),
    CryptsetupStatusRaw(String),
    CryptsetupLuksUuidRaw(String),
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

impl CommandRunner for RealRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        // Implement in Phase 2.
        Err(CmdError::Failed(format!(
            "RealRunner not implemented for request: {request:?}"
        )))
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
        ];

        assert_eq!(all.len(), 6);
    }
}
