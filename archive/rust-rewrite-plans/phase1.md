# Phase 1: Scaffold + Core Types

## Goal
Stand up a compilable Rust CLI scaffold in `cli/` with strict module boundaries, typed command requests/outputs, and foundational domain types.

Success criteria:
- `cargo test` passes in `cli/`
- `nix build .#braid-rust` builds
- Existing bash-based VM tests remain unchanged and passing

---

## Task List

## 1. Create crate and wire build
- [ ] Create `cli/Cargo.toml`
- [ ] Create `cli/src/main.rs`
- [ ] Create `cli/src/lib.rs`
- [ ] Add Rust package to `flake.nix` as `braid-rust`
- [ ] Add `test-rust` target to `Makefile` (`cd cli && cargo test`)

## 2. Add core modules (stubs + compile)
- [ ] Create `cli/src/types.rs`
- [ ] Create `cli/src/config.rs`
- [ ] Create `cli/src/cmd.rs`
- [ ] Create `cli/src/parse.rs`
- [ ] Ensure `main.rs` declares 4 subcommands (`init-disk`, `plan`, `apply`, `status`) with `not yet implemented` behavior

## 3. Add initial tests
- [ ] `types.rs`: status transition tests, serde round-trip tests
- [ ] `config.rs`: valid/invalid config decode tests
- [ ] `cmd.rs`: `MockRunner` smoke tests, request coverage test

## 4. Boundary checks
- [ ] Domain code compiles without raw parsing in `main.rs`
- [ ] All external command intents modeled as `CmdRequest` variants
- [ ] `ActionType` has no formatting variant

## 5. Phase-1 exit check
- [ ] `cd cli && cargo test`
- [ ] `nix build .#braid-rust`

---

## File Skeleton

```text
cli/
  Cargo.toml
  src/
    lib.rs
    main.rs
    types.rs
    config.rs
    cmd.rs
    parse.rs
```

---

## Initial Rust Type Definitions

## `cli/Cargo.toml`
```toml
[package]
name = "braid-cli"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "braid"
path = "src/main.rs"

[dependencies]
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "1"
uuid = { version = "1", features = ["serde", "v4"] }

[dev-dependencies]
pretty_assertions = "1"
```

## `cli/src/lib.rs`
```rust
pub mod cmd;
pub mod config;
pub mod parse;
pub mod types;
```

## `cli/src/main.rs`
```rust
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "braid")]
#[command(about = "braid Rust CLI", long_about = None)]
struct Cli {
    #[arg(long, default_value = "/etc/braid/config.json")]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    InitDisk(InitDiskArgs),
    Plan(PlanArgs),
    Apply(ApplyArgs),
    Status(StatusArgs),
}

#[derive(Debug, Args)]
struct InitDiskArgs {
    by_id_path: String,
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct PlanArgs {
    #[arg(long)]
    json: bool,
    #[arg(long)]
    allow_remove_missing: bool,
    #[arg(long)]
    allow_remove_ambiguous: bool,
}

#[derive(Debug, Args)]
struct ApplyArgs {
    #[arg(long)]
    resume: bool,
    #[arg(long)]
    allow_remove_missing: bool,
    #[arg(long)]
    allow_remove_ambiguous: bool,
}

#[derive(Debug, Args)]
struct StatusArgs {
    #[arg(long)]
    verbose: bool,
    #[arg(long)]
    json: bool,
}

fn main() {
    let cli = Cli::parse();

    let _config_path = cli.config;

    match cli.command {
        Commands::InitDisk(_) => println!("not yet implemented"),
        Commands::Plan(_) => println!("not yet implemented"),
        Commands::Apply(_) => println!("not yet implemented"),
        Commands::Status(_) => println!("not yet implemented"),
    }
}
```

## `cli/src/types.rs`
```rust
use serde::{Deserialize, Serialize};
use std::fmt;
use std::marker::PhantomData;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ByIdPath(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LuksUuid(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MapperName(pub String);

impl fmt::Display for ByIdPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl ActionStatus {
    pub fn transition_to(self, next: ActionStatus) -> Result<ActionStatus, TransitionError> {
        use ActionStatus::{Completed, Failed, InProgress, Pending};
        let ok = matches!((self, next),
            (Pending, InProgress)
                | (Pending, Failed)
                | (InProgress, Completed)
                | (InProgress, Failed)
                | (Pending, Pending)
                | (InProgress, InProgress)
                | (Completed, Completed)
                | (Failed, Failed)
        );

        if ok {
            Ok(next)
        } else {
            Err(TransitionError { from: self, to: next })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActionType {
    OpenLuks,
    AddDiskBtrfsAdd,
    BalanceToRaid1,
    RemoveDiskGraceful,
    RemoveDiskMissingExplicit,
    CloseLuksMapper,
    VerifyPoolHealth,
    VerifyExpectedDiskSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    #[serde(rename = "type")]
    pub action_type: ActionType,
    pub target: String,
    pub preconditions: Vec<String>,
    pub status: ActionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedReason {
    pub code: String,
    pub disk: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confirmation {
    pub action_id: String,
    pub phrase: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PlanOutcome {
    Applicable {
        plan_id: String,
        actions: Vec<Action>,
        warnings: Vec<Warning>,
        confirmations: Vec<Confirmation>,
    },
    Blocked {
        plan_id: String,
        warnings: Vec<Warning>,
        blocked_reasons: Vec<BlockedReason>,
    },
}

pub struct Applicable;
pub struct Blocked;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan<S> {
    pub plan_id: String,
    pub actions: Vec<Action>,
    pub warnings: Vec<Warning>,
    pub confirmations: Vec<Confirmation>,
    pub blocked_reasons: Vec<BlockedReason>,
    _state: PhantomData<S>,
}

impl Plan<Applicable> {
    pub fn new_applicable(
        plan_id: String,
        actions: Vec<Action>,
        warnings: Vec<Warning>,
        confirmations: Vec<Confirmation>,
    ) -> Self {
        Self {
            plan_id,
            actions,
            warnings,
            confirmations,
            blocked_reasons: Vec::new(),
            _state: PhantomData,
        }
    }
}

impl Plan<Blocked> {
    pub fn new_blocked(
        plan_id: String,
        warnings: Vec<Warning>,
        blocked_reasons: Vec<BlockedReason>,
    ) -> Self {
        Self {
            plan_id,
            actions: Vec::new(),
            warnings,
            confirmations: Vec::new(),
            blocked_reasons,
            _state: PhantomData,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplicablePlan(pub Plan<Applicable>);

impl TryFrom<PlanOutcome> for ApplicablePlan {
    type Error = &'static str;

    fn try_from(value: PlanOutcome) -> Result<Self, Self::Error> {
        match value {
            PlanOutcome::Applicable {
                plan_id,
                actions,
                warnings,
                confirmations,
            } => Ok(ApplicablePlan(Plan::new_applicable(
                plan_id,
                actions,
                warnings,
                confirmations,
            ))),
            PlanOutcome::Blocked { .. } => Err("blocked plans are not executable"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionError {
    pub from: ActionStatus,
    pub to: ActionStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_valid_status_transition() {
        let next = ActionStatus::Pending
            .transition_to(ActionStatus::InProgress)
            .expect("pending -> in_progress should be valid");
        assert_eq!(next, ActionStatus::InProgress);
    }

    #[test]
    fn rejects_invalid_status_transition() {
        let err = ActionStatus::Completed
            .transition_to(ActionStatus::InProgress)
            .expect_err("completed -> in_progress should be invalid");
        assert_eq!(err.from, ActionStatus::Completed);
        assert_eq!(err.to, ActionStatus::InProgress);
    }

    #[test]
    fn blocks_conversion_for_blocked_plan() {
        let outcome = PlanOutcome::Blocked {
            plan_id: "p1".to_owned(),
            warnings: vec![],
            blocked_reasons: vec![BlockedReason {
                code: "X".to_owned(),
                disk: None,
                message: "blocked".to_owned(),
            }],
        };

        let res = ApplicablePlan::try_from(outcome);
        assert!(res.is_err());
    }
}
```

## `cli/src/config.rs`
```rust
use crate::types::ByIdPath;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub disks: Vec<ByIdPath>,
    #[serde(rename = "mountPoint")]
    pub mount_point: String,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: String,
        source: serde_json::Error,
    },
    #[error("config validation failed: {0}")]
    Validation(String),
}

pub fn config_read(path: &Path) -> Result<Config, ConfigError> {
    let raw = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;

    let cfg: Config = serde_json::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })?;

    validate(&cfg)?;
    Ok(cfg)
}

pub fn config_hash(raw: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    raw.hash(&mut h);
    format!("hash:{:x}", h.finish())
}

fn validate(cfg: &Config) -> Result<(), ConfigError> {
    if cfg.disks.is_empty() {
        return Err(ConfigError::Validation("disks must not be empty".to_owned()));
    }
    if cfg.mount_point.is_empty() {
        return Err(ConfigError::Validation(
            "mountPoint must not be empty".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_config() {
        let raw = r#"{"disks":["/dev/disk/by-id/a"],"mountPoint":"/mnt/storage"}"#;
        let cfg: Config = serde_json::from_str(raw).expect("config should parse");
        assert_eq!(cfg.disks.len(), 1);
        assert_eq!(cfg.mount_point, "/mnt/storage");
    }

    #[test]
    fn rejects_empty_disks() {
        let cfg = Config {
            disks: vec![],
            mount_point: "/mnt/storage".to_owned(),
        };
        let err = validate(&cfg).expect_err("empty disks should fail");
        assert!(matches!(err, ConfigError::Validation(_)));
    }
}
```

## `cli/src/cmd.rs`
```rust
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
```

## `cli/src/parse.rs`
```rust
use crate::cmd::{CmdOutput, RawCommandOutput};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("unsupported parser path for command: {0}")]
    Unsupported(String),
    #[error("invalid json: {0}")]
    InvalidJson(String),
    #[error("invalid text format: {0}")]
    InvalidText(String),
}

pub fn parse_output(raw: RawCommandOutput) -> Result<CmdOutput, ParseError> {
    // Phase 1 stub only. Implement command-specific parsers in Phase 2.
    Err(ParseError::Unsupported(raw.cmd))
}
```

---

## Notes for Phase 2
- Move `serde_json::Value` placeholders in `cmd.rs` to concrete typed structs.
- Keep all parsing in `parse.rs`; no parsing in `probe.rs`, `identity.rs`, `plan.rs`, `exec.rs`, or `status.rs`.
- Replace `config_hash()` with SHA-256 to match existing behavior contract.
