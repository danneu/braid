use crate::config::config_hash;
use crate::state_io::atomic_write;
use crate::types::PoolState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const CHECKPOINT_FILE: &str = "/var/lib/braid/op-state.json";
pub const CHECKPOINT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpKind {
    Add,
    Remove,
    RemoveMissing,
    Replace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpArgs {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_disk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_disk: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_id: Option<u64>,
}

impl OpArgs {
    pub fn add(name: &str) -> Self {
        Self {
            disk: Some(name.to_owned()),
            old_disk: None,
            new_disk: None,
            missing_id: None,
        }
    }

    pub fn remove(name: &str) -> Self {
        Self {
            disk: Some(name.to_owned()),
            old_disk: None,
            new_disk: None,
            missing_id: None,
        }
    }

    pub fn remove_missing(missing_id: Option<u64>) -> Self {
        Self {
            disk: None,
            old_disk: None,
            new_disk: None,
            missing_id,
        }
    }

    pub fn replace(old_name: &str, new_name: &str, missing_id: Option<u64>) -> Self {
        Self {
            disk: None,
            old_disk: Some(old_name.to_owned()),
            new_disk: Some(new_name.to_owned()),
            missing_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    AddBalanceRaid1,
    RemoveStart,
    RemoveMissingStart,
    ReplaceBalanceRaid1,
    ReplaceEvictDead,
    ReplaceEvictLive,
}

impl Phase {
    pub fn as_env_value(&self) -> &'static str {
        match self {
            Phase::AddBalanceRaid1 => "add-balance-raid1",
            Phase::RemoveStart => "remove-start",
            Phase::RemoveMissingStart => "remove-missing-start",
            Phase::ReplaceBalanceRaid1 => "replace-balance-raid1",
            Phase::ReplaceEvictDead => "replace-evict-dead",
            Phase::ReplaceEvictLive => "replace-evict-live",
        }
    }

    fn allowed_for_op(&self, op: &OpKind) -> bool {
        matches!(
            (op, self),
            (OpKind::Add, Phase::AddBalanceRaid1)
                | (OpKind::Remove, Phase::RemoveStart)
                | (OpKind::RemoveMissing, Phase::RemoveMissingStart)
                | (OpKind::Replace, Phase::ReplaceBalanceRaid1)
                | (OpKind::Replace, Phase::ReplaceEvictDead)
                | (OpKind::Replace, Phase::ReplaceEvictLive)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secondary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_id: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointV1 {
    pub schema_version: u32,
    pub run_id: String,
    pub op: OpKind,
    pub op_args: OpArgs,
    pub phase: Phase,
    pub created_at: String,
    pub updated_at: String,
    pub config_hash: String,
    pub args_hash: String,
    pub pool_fingerprint: PoolFingerprint,
    pub target_snapshot: TargetSnapshot,
}

/// Pool topology snapshot for checkpoint staleness detection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolFingerprint {
    pub devices: Vec<PoolFingerprintDevice>,
    pub missing_count: u32,
    pub total_devices: u32,
    pub mounted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolFingerprintDevice {
    pub devid: u64,
    pub luks_uuid: Option<String>,
    pub mapper: String,
}

impl PoolFingerprint {
    pub fn from_pool_state(pool: &PoolState) -> Self {
        let mut devices: Vec<PoolFingerprintDevice> = pool
            .devices
            .iter()
            .map(|d| PoolFingerprintDevice {
                devid: d.devid,
                luks_uuid: Some(d.luks_uuid.0.clone()),
                mapper: d.mapper.0.clone(),
            })
            .collect();
        devices.sort_by_key(|d| d.devid);
        PoolFingerprint {
            devices,
            missing_count: pool.missing_count as u32,
            total_devices: pool.total_devices as u32,
            mounted: pool.mounted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointErrorCode {
    Corrupt,
    SchemaUnsupported,
    OpMismatch,
    ArgsMismatch,
    ConfigDrift,
    TopologyDrift,
    TargetMissing,
    PhaseInvalid,
    PauseTimeout,
    TestHook,
}

impl CheckpointErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckpointErrorCode::Corrupt => "CHECKPOINT_CORRUPT",
            CheckpointErrorCode::SchemaUnsupported => "CHECKPOINT_SCHEMA_UNSUPPORTED",
            CheckpointErrorCode::OpMismatch => "CHECKPOINT_OP_MISMATCH",
            CheckpointErrorCode::ArgsMismatch => "CHECKPOINT_ARGS_MISMATCH",
            CheckpointErrorCode::ConfigDrift => "CHECKPOINT_CONFIG_DRIFT",
            CheckpointErrorCode::TopologyDrift => "CHECKPOINT_TOPOLOGY_DRIFT",
            CheckpointErrorCode::TargetMissing => "CHECKPOINT_TARGET_MISSING",
            CheckpointErrorCode::PhaseInvalid => "CHECKPOINT_PHASE_INVALID",
            CheckpointErrorCode::PauseTimeout => "CHECKPOINT_PAUSE_TIMEOUT",
            CheckpointErrorCode::TestHook => "CHECKPOINT_TEST_HOOK",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointError {
    pub code: CheckpointErrorCode,
    pub message: String,
}

impl CheckpointError {
    pub fn new(code: CheckpointErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "error[{}]: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for CheckpointError {}

pub trait Clock {
    fn now_rfc3339(&self) -> String;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_rfc3339(&self) -> String {
        use time::format_description::well_known::Iso8601;
        time::OffsetDateTime::now_utc()
            .format(&Iso8601::DEFAULT)
            .unwrap_or_else(|_| "unknown".into())
    }
}

#[cfg(test)]
pub struct FixedClock {
    pub value: String,
}

#[cfg(test)]
impl Clock for FixedClock {
    fn now_rfc3339(&self) -> String {
        self.value.clone()
    }
}

#[derive(Debug, Clone)]
pub struct InvocationCtx {
    pub op: OpKind,
    pub op_args: OpArgs,
    pub args_hash: String,
    pub config_hash: String,
}

#[derive(Debug, Clone)]
pub struct LiveCtx {
    pub pool_fingerprint: PoolFingerprint,
    pub primary_target_available: bool,
    pub secondary_target_available: Option<bool>,
}

#[derive(Debug, Clone)]
pub enum ValidationDecision {
    ResumeFrom { phase: Phase },
    Reject { error: CheckpointError },
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum ResumeGate {
    NoCheckpoint,
    ResumeFrom(CheckpointV1),
    Reject(CheckpointError),
}

/// Compute a hash for the command arguments (for staleness detection).
pub fn hash_args(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    format!("{:x}", hasher.finalize())
}

#[allow(clippy::too_many_arguments)]
pub fn new_checkpoint(
    clock: &dyn Clock,
    op: OpKind,
    op_args: OpArgs,
    phase: Phase,
    config_hash: String,
    args_hash: String,
    pool_fingerprint: PoolFingerprint,
    target_snapshot: TargetSnapshot,
) -> CheckpointV1 {
    let now = clock.now_rfc3339();
    CheckpointV1 {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        run_id: uuid::Uuid::now_v7().to_string(),
        op,
        op_args,
        phase,
        created_at: now.clone(),
        updated_at: now,
        config_hash,
        args_hash,
        pool_fingerprint,
        target_snapshot,
    }
}

pub fn update_phase(checkpoint: &mut CheckpointV1, phase: Phase, clock: &dyn Clock) {
    checkpoint.phase = phase;
    checkpoint.updated_at = clock.now_rfc3339();
}

pub fn load_checkpoint_file(path: &Path) -> Result<Option<CheckpointV1>, CheckpointError> {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(CheckpointError::new(
                CheckpointErrorCode::Corrupt,
                format!("failed to read checkpoint: {e}"),
            ));
        }
    };

    let checkpoint: CheckpointV1 = serde_json::from_str(&contents).map_err(|e| {
        CheckpointError::new(
            CheckpointErrorCode::Corrupt,
            format!("checkpoint file is not valid JSON: {e}"),
        )
    })?;

    if checkpoint.schema_version != CHECKPOINT_SCHEMA_VERSION {
        return Err(CheckpointError::new(
            CheckpointErrorCode::SchemaUnsupported,
            format!(
                "checkpoint schema_version={} is unsupported (expected {})",
                checkpoint.schema_version, CHECKPOINT_SCHEMA_VERSION
            ),
        ));
    }

    Ok(Some(checkpoint))
}

pub fn validate_resume(
    checkpoint: &CheckpointV1,
    invocation: &InvocationCtx,
    live: &LiveCtx,
) -> ValidationDecision {
    if checkpoint.op != invocation.op {
        return ValidationDecision::Reject {
            error: CheckpointError::new(
                CheckpointErrorCode::OpMismatch,
                format!(
                    "checkpoint is for '{:?}' but command is '{:?}'",
                    checkpoint.op, invocation.op
                ),
            ),
        };
    }

    if checkpoint.op_args != invocation.op_args || checkpoint.args_hash != invocation.args_hash {
        return ValidationDecision::Reject {
            error: CheckpointError::new(
                CheckpointErrorCode::ArgsMismatch,
                "checkpoint arguments do not match this invocation",
            ),
        };
    }

    if checkpoint.config_hash != invocation.config_hash {
        return ValidationDecision::Reject {
            error: CheckpointError::new(
                CheckpointErrorCode::ConfigDrift,
                "config changed since checkpoint was created",
            ),
        };
    }

    // ReplaceEvictLive legitimately changes topology (device removed) and
    // the eviction target may already be gone on resume, so skip strict
    // fingerprint and secondary_target checks for that phase.
    let is_live_evict = checkpoint.phase == Phase::ReplaceEvictLive;

    if !is_live_evict && checkpoint.pool_fingerprint != live.pool_fingerprint {
        return ValidationDecision::Reject {
            error: CheckpointError::new(
                CheckpointErrorCode::TopologyDrift,
                "pool topology changed since checkpoint was created",
            ),
        };
    }

    if !checkpoint.phase.allowed_for_op(&checkpoint.op) {
        return ValidationDecision::Reject {
            error: CheckpointError::new(
                CheckpointErrorCode::PhaseInvalid,
                "checkpoint phase is invalid for this operation",
            ),
        };
    }

    if !live.primary_target_available
        || (!is_live_evict && matches!(live.secondary_target_available, Some(false)))
    {
        return ValidationDecision::Reject {
            error: CheckpointError::new(
                CheckpointErrorCode::TargetMissing,
                "checkpoint target is not currently available",
            ),
        };
    }

    ValidationDecision::ResumeFrom {
        phase: checkpoint.phase.clone(),
    }
}

pub fn resolve_resume_gate(
    config_raw: &str,
    invocation: InvocationCtx,
    live: LiveCtx,
    checkpoint_path: &Path,
) -> ResumeGate {
    let checkpoint = match load_checkpoint_file(checkpoint_path) {
        Ok(Some(cp)) => cp,
        Ok(None) => return ResumeGate::NoCheckpoint,
        Err(e) => return ResumeGate::Reject(e),
    };

    let invocation = InvocationCtx {
        config_hash: config_hash(config_raw),
        ..invocation
    };

    match validate_resume(&checkpoint, &invocation, &live) {
        ValidationDecision::ResumeFrom { .. } => ResumeGate::ResumeFrom(checkpoint),
        ValidationDecision::Reject { error } => ResumeGate::Reject(error),
    }
}

/// Save a checkpoint atomically.
pub fn save_checkpoint_atomic(
    checkpoint: &CheckpointV1,
    checkpoint_path: &Path,
) -> Result<(), std::io::Error> {
    let json = serde_json::to_string_pretty(checkpoint).map_err(std::io::Error::other)?;
    atomic_write(checkpoint_path, json.as_bytes())?;
    Ok(())
}

/// Clear the checkpoint file on successful completion.
pub fn clear_checkpoint(checkpoint_path: &Path) {
    let _ = std::fs::remove_file(checkpoint_path);
}

pub fn maybe_fail_after_checkpoint() -> Result<(), CheckpointError> {
    if std::env::var("BRAID_TEST_FAIL_AFTER_CHECKPOINT")
        .ok()
        .as_deref()
        == Some("1")
    {
        return Err(CheckpointError::new(
            CheckpointErrorCode::TestHook,
            "simulated failure via BRAID_TEST_FAIL_AFTER_CHECKPOINT",
        ));
    }
    Ok(())
}

pub fn run_phase_hooks(phase: &Phase) -> Result<(), CheckpointError> {
    if std::env::var("BRAID_TEST_FAIL_AT_PHASE").ok().as_deref() == Some(phase.as_env_value()) {
        return Err(CheckpointError::new(
            CheckpointErrorCode::TestHook,
            format!("simulated failure at phase {}", phase.as_env_value()),
        ));
    }

    if std::env::var("BRAID_TEST_PAUSE_AT_PHASE").ok().as_deref() == Some(phase.as_env_value()) {
        let timeout_secs = std::env::var("BRAID_TEST_PAUSE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(30);
        let pause_file = std::env::var("BRAID_TEST_PAUSE_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp/braid-test-unpause"));

        let start = std::time::Instant::now();
        while !pause_file.exists() {
            if start.elapsed() >= Duration::from_secs(timeout_secs) {
                return Err(CheckpointError::new(
                    CheckpointErrorCode::PauseTimeout,
                    format!(
                        "phase {} pause timed out after {}s waiting for {}",
                        phase.as_env_value(),
                        timeout_secs,
                        pause_file.display()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LuksUuid, MapperName, PoolDevice};

    fn test_pool() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-toshiba".into()),
                luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".into()),
                devid: 1,
            }],
            missing_count: 0,
            total_devices: 1,
        }
    }

    fn valid_checkpoint() -> CheckpointV1 {
        new_checkpoint(
            &FixedClock {
                value: "2026-01-01T00:00:00Z".to_owned(),
            },
            OpKind::Add,
            OpArgs::add("disk2"),
            Phase::AddBalanceRaid1,
            "sha256:abc".to_owned(),
            "args123".to_owned(),
            PoolFingerprint::from_pool_state(&test_pool()),
            TargetSnapshot {
                primary: Some("disk2".to_owned()),
                secondary: None,
                missing_id: None,
            },
        )
    }

    #[test]
    fn fingerprint_sorts_by_devid() {
        let pool = PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName("braid-b".into()),
                    luks_uuid: LuksUuid("bbb".into()),
                    devid: 5,
                },
                PoolDevice {
                    mapper: MapperName("braid-a".into()),
                    luks_uuid: LuksUuid("aaa".into()),
                    devid: 1,
                },
            ],
            missing_count: 0,
            total_devices: 2,
        };
        let fp = PoolFingerprint::from_pool_state(&pool);
        assert_eq!(fp.devices[0].devid, 1);
        assert_eq!(fp.devices[1].devid, 5);
    }

    #[test]
    fn hash_args_deterministic() {
        let h1 = hash_args(&["add", "toshiba"]);
        let h2 = hash_args(&["add", "toshiba"]);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_args_different_for_different_args() {
        let h1 = hash_args(&["add", "toshiba"]);
        let h2 = hash_args(&["add", "ironwolf"]);
        assert_ne!(h1, h2);
    }

    #[test]
    fn update_phase_updates_timestamp() {
        let mut cp = valid_checkpoint();
        update_phase(
            &mut cp,
            Phase::ReplaceEvictDead,
            &FixedClock {
                value: "2026-01-01T00:00:30Z".to_owned(),
            },
        );
        assert_eq!(cp.updated_at, "2026-01-01T00:00:30Z");
        assert_eq!(cp.created_at, "2026-01-01T00:00:00Z");
        assert_eq!(cp.phase, Phase::ReplaceEvictDead);
    }

    #[test]
    fn validate_resume_happy_path() {
        let cp = valid_checkpoint();
        let invocation = InvocationCtx {
            op: OpKind::Add,
            op_args: OpArgs::add("disk2"),
            args_hash: "args123".to_owned(),
            config_hash: "sha256:abc".to_owned(),
        };
        let live = LiveCtx {
            pool_fingerprint: PoolFingerprint::from_pool_state(&test_pool()),
            primary_target_available: true,
            secondary_target_available: None,
        };

        let result = validate_resume(&cp, &invocation, &live);
        assert!(matches!(
            result,
            ValidationDecision::ResumeFrom {
                phase: Phase::AddBalanceRaid1
            }
        ));
    }

    #[test]
    fn validate_resume_op_mismatch() {
        let cp = valid_checkpoint();
        let invocation = InvocationCtx {
            op: OpKind::Remove,
            op_args: OpArgs::remove("disk2"),
            args_hash: "args123".to_owned(),
            config_hash: "sha256:abc".to_owned(),
        };
        let live = LiveCtx {
            pool_fingerprint: PoolFingerprint::from_pool_state(&test_pool()),
            primary_target_available: true,
            secondary_target_available: None,
        };

        let result = validate_resume(&cp, &invocation, &live);
        match result {
            ValidationDecision::Reject { error } => {
                assert_eq!(error.code, CheckpointErrorCode::OpMismatch);
                assert!(
                    error
                        .to_string()
                        .starts_with("error[CHECKPOINT_OP_MISMATCH]:")
                );
            }
            _ => panic!("expected reject"),
        }
    }

    #[test]
    fn validate_resume_args_mismatch() {
        let cp = valid_checkpoint();
        let invocation = InvocationCtx {
            op: OpKind::Add,
            op_args: OpArgs::add("disk9"),
            args_hash: "args-different".to_owned(),
            config_hash: "sha256:abc".to_owned(),
        };
        let live = LiveCtx {
            pool_fingerprint: PoolFingerprint::from_pool_state(&test_pool()),
            primary_target_available: true,
            secondary_target_available: None,
        };

        let result = validate_resume(&cp, &invocation, &live);
        match result {
            ValidationDecision::Reject { error } => {
                assert_eq!(error.code, CheckpointErrorCode::ArgsMismatch);
            }
            _ => panic!("expected reject"),
        }
    }

    #[test]
    fn validate_resume_config_drift() {
        let cp = valid_checkpoint();
        let invocation = InvocationCtx {
            op: OpKind::Add,
            op_args: OpArgs::add("disk2"),
            args_hash: "args123".to_owned(),
            config_hash: "sha256:different".to_owned(),
        };
        let live = LiveCtx {
            pool_fingerprint: PoolFingerprint::from_pool_state(&test_pool()),
            primary_target_available: true,
            secondary_target_available: None,
        };

        let result = validate_resume(&cp, &invocation, &live);
        match result {
            ValidationDecision::Reject { error } => {
                assert_eq!(error.code, CheckpointErrorCode::ConfigDrift);
            }
            _ => panic!("expected reject"),
        }
    }

    #[test]
    fn validate_resume_topology_drift() {
        let cp = valid_checkpoint();
        let mut different_pool = test_pool();
        different_pool.total_devices = 2;
        let invocation = InvocationCtx {
            op: OpKind::Add,
            op_args: OpArgs::add("disk2"),
            args_hash: "args123".to_owned(),
            config_hash: "sha256:abc".to_owned(),
        };
        let live = LiveCtx {
            pool_fingerprint: PoolFingerprint::from_pool_state(&different_pool),
            primary_target_available: true,
            secondary_target_available: None,
        };

        let result = validate_resume(&cp, &invocation, &live);
        match result {
            ValidationDecision::Reject { error } => {
                assert_eq!(error.code, CheckpointErrorCode::TopologyDrift);
            }
            _ => panic!("expected reject"),
        }
    }

    #[test]
    fn validate_resume_target_missing() {
        let cp = valid_checkpoint();
        let invocation = InvocationCtx {
            op: OpKind::Add,
            op_args: OpArgs::add("disk2"),
            args_hash: "args123".to_owned(),
            config_hash: "sha256:abc".to_owned(),
        };
        let live = LiveCtx {
            pool_fingerprint: PoolFingerprint::from_pool_state(&test_pool()),
            primary_target_available: false,
            secondary_target_available: None,
        };

        let result = validate_resume(&cp, &invocation, &live);
        match result {
            ValidationDecision::Reject { error } => {
                assert_eq!(error.code, CheckpointErrorCode::TargetMissing);
            }
            _ => panic!("expected reject"),
        }
    }

    #[test]
    fn validate_resume_phase_invalid() {
        let mut cp = valid_checkpoint();
        cp.phase = Phase::ReplaceEvictDead;
        let invocation = InvocationCtx {
            op: OpKind::Add,
            op_args: OpArgs::add("disk2"),
            args_hash: "args123".to_owned(),
            config_hash: "sha256:abc".to_owned(),
        };
        let live = LiveCtx {
            pool_fingerprint: PoolFingerprint::from_pool_state(&test_pool()),
            primary_target_available: true,
            secondary_target_available: None,
        };

        let result = validate_resume(&cp, &invocation, &live);
        match result {
            ValidationDecision::Reject { error } => {
                assert_eq!(error.code, CheckpointErrorCode::PhaseInvalid);
            }
            _ => panic!("expected reject"),
        }
    }

    #[test]
    fn load_checkpoint_rejects_schema_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cp.json");
        std::fs::write(
            &path,
            r#"{"schema_version":99,"run_id":"x","op":"add","op_args":{"disk":"disk2"},"phase":"add-balance-raid1","created_at":"x","updated_at":"x","config_hash":"c","args_hash":"a","pool_fingerprint":{"devices":[],"missing_count":0,"total_devices":0,"mounted":true},"target_snapshot":{}}"#,
        )
        .unwrap();

        let err = load_checkpoint_file(&path).unwrap_err();
        assert_eq!(err.code, CheckpointErrorCode::SchemaUnsupported);
    }

    #[test]
    fn load_checkpoint_rejects_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cp.json");
        std::fs::write(&path, "not-json").unwrap();

        let err = load_checkpoint_file(&path).unwrap_err();
        assert_eq!(err.code, CheckpointErrorCode::Corrupt);
    }

    #[test]
    // Intent: ReplaceEvictLive phase skips topology drift check on resume.
    // Why: after live eviction, the pool topology legitimately changes.
    // Scenario: live replace interrupted during eviction, operator retries.
    fn validate_resume_replace_evict_live_skips_topology_check() {
        let pool = test_pool();
        let mut cp = new_checkpoint(
            &FixedClock {
                value: "2026-01-01T00:00:00Z".to_owned(),
            },
            OpKind::Replace,
            OpArgs::replace("old", "new", None),
            Phase::ReplaceEvictLive,
            "sha256:abc".to_owned(),
            "args123".to_owned(),
            PoolFingerprint::from_pool_state(&pool),
            TargetSnapshot {
                primary: Some("new".to_owned()),
                secondary: Some("old".to_owned()),
                missing_id: None,
            },
        );
        // Change the fingerprint to simulate topology drift after eviction
        cp.pool_fingerprint.total_devices = 99;

        let invocation = InvocationCtx {
            op: OpKind::Replace,
            op_args: OpArgs::replace("old", "new", None),
            args_hash: "args123".to_owned(),
            config_hash: "sha256:abc".to_owned(),
        };
        // Live context has different topology (device already removed)
        let live = LiveCtx {
            pool_fingerprint: PoolFingerprint::from_pool_state(&pool),
            primary_target_available: true,
            secondary_target_available: Some(false), // old device may be gone
        };

        let result = validate_resume(&cp, &invocation, &live);
        assert!(
            matches!(result, ValidationDecision::ResumeFrom { .. }),
            "expected ResumeFrom despite topology drift for ReplaceEvictLive, got: {result:?}"
        );
    }

    #[test]
    // Intent: ReplaceEvictDead still enforces strict topology check.
    // Why: dead eviction doesn't change topology, so drift should reject.
    // Scenario: regression guard — relaxation only applies to live eviction.
    fn validate_resume_replace_evict_dead_strict_topology() {
        let pool = test_pool();
        let cp = new_checkpoint(
            &FixedClock {
                value: "2026-01-01T00:00:00Z".to_owned(),
            },
            OpKind::Replace,
            OpArgs::replace("old", "new", None),
            Phase::ReplaceEvictDead,
            "sha256:abc".to_owned(),
            "args123".to_owned(),
            PoolFingerprint::from_pool_state(&pool),
            TargetSnapshot {
                primary: Some("new".to_owned()),
                secondary: Some("old".to_owned()),
                missing_id: None,
            },
        );

        let mut different_pool = pool;
        different_pool.total_devices = 99;
        let invocation = InvocationCtx {
            op: OpKind::Replace,
            op_args: OpArgs::replace("old", "new", None),
            args_hash: "args123".to_owned(),
            config_hash: "sha256:abc".to_owned(),
        };
        let live = LiveCtx {
            pool_fingerprint: PoolFingerprint::from_pool_state(&different_pool),
            primary_target_available: true,
            secondary_target_available: Some(true),
        };

        let result = validate_resume(&cp, &invocation, &live);
        match result {
            ValidationDecision::Reject { error } => {
                assert_eq!(error.code, CheckpointErrorCode::TopologyDrift);
            }
            _ => panic!("expected topology drift reject for ReplaceEvictDead"),
        }
    }

    #[test]
    fn run_phase_hooks_timeout() {
        unsafe {
            std::env::set_var("BRAID_TEST_PAUSE_AT_PHASE", "add-balance-raid1");
            std::env::set_var("BRAID_TEST_PAUSE_TIMEOUT_SECS", "0");
            std::env::set_var(
                "BRAID_TEST_PAUSE_FILE",
                "/tmp/this-file-should-not-exist-braid-tests",
            );
        }

        let err = run_phase_hooks(&Phase::AddBalanceRaid1).unwrap_err();
        assert_eq!(err.code, CheckpointErrorCode::PauseTimeout);

        unsafe {
            std::env::remove_var("BRAID_TEST_PAUSE_AT_PHASE");
            std::env::remove_var("BRAID_TEST_PAUSE_TIMEOUT_SECS");
            std::env::remove_var("BRAID_TEST_PAUSE_FILE");
        }
    }
}
