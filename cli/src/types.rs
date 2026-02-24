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

impl fmt::Display for LuksUuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for MapperName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// ---------------------------------------------------------------------------
// Action status & transitions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ActionState {
    Pending,
    InProgress,
    Completed,
    Failed { error: String },
}

impl ActionState {
    pub fn transition_to(self, next: ActionState) -> Result<ActionState, TransitionError> {
        use std::mem::discriminant;
        let ok = discriminant(&self) == discriminant(&next)
            || matches!(
                (&self, &next),
                (Self::Pending, Self::InProgress)
                    | (Self::Pending, Self::Failed { .. })
                    | (Self::InProgress, Self::Completed)
                    | (Self::InProgress, Self::Failed { .. })
                    | (Self::Failed { .. }, Self::InProgress)
            );

        if ok {
            Ok(next)
        } else {
            Err(TransitionError {
                from: self,
                to: next,
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionError {
    pub from: ActionState,
    pub to: ActionState,
}

// ---------------------------------------------------------------------------
// Action types & actions
// ---------------------------------------------------------------------------

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
#[serde(rename_all = "snake_case")]
pub enum RunCertainty {
    WillRun,
    MayRun,
}

fn default_will_run() -> RunCertainty {
    RunCertainty::WillRun
}

fn is_will_run(c: &RunCertainty) -> bool {
    matches!(c, RunCertainty::WillRun)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedCommand {
    pub command: String,
    #[serde(default = "default_will_run", skip_serializing_if = "is_will_run")]
    pub certainty: RunCertainty,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub id: String,
    #[serde(rename = "type")]
    pub action_type: ActionType,
    pub target: String,
    pub preconditions: Vec<String>,
    #[serde(flatten)]
    pub state: ActionState,
    pub commands: Vec<PlannedCommand>,
}

// ---------------------------------------------------------------------------
// Warning & blocked-reason codes (enum, not raw strings)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WarningCode {
    DiskAbsentSkipped,
    InitRequired,
    PoolDegradedMissingDevices,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BlockedReasonCode {
    IdentityAmbiguousAbsentDisk,
    AmbiguousMissing,
    InvalidByIdPath,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Warning {
    pub code: WarningCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedReason {
    pub code: BlockedReasonCode,
    pub disk: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confirmation {
    pub action_id: String,
    pub phrase: String,
}

// ---------------------------------------------------------------------------
// Plan outcome (planner output)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Typestate plan wrappers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applicable;
#[derive(Debug, Clone, PartialEq, Eq)]
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

// ---------------------------------------------------------------------------
// Plan status (for JSON report)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Applicable,
    Blocked,
}

// ---------------------------------------------------------------------------
// JSON report types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub actions_total: usize,
    pub actions_mutation: usize,
    pub actions_verify: usize,
    pub warnings_total: usize,
    pub blocked_total: usize,
    pub skipped_total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanReport {
    pub schema_version: u32,
    pub plan_id: String,
    pub mount_point: String,
    pub status: PlanStatus,
    pub warning_count: usize,
    pub warnings: Vec<Warning>,
    pub blocked_reasons: Vec<BlockedReason>,
    pub confirmations: Vec<Confirmation>,
    pub actions: Vec<Action>,
    pub summary: PlanSummary,
}

// ---------------------------------------------------------------------------
// Planner input types (produced by probe, consumed by planner)
// ---------------------------------------------------------------------------

/// What we know about the live btrfs pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolState {
    pub mounted: bool,
    pub devices: Vec<PoolDevice>,
    pub missing_count: u64,
    pub total_devices: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolDevice {
    pub mapper: MapperName,
    pub luks_uuid: LuksUuid,
    pub devid: u64,
}

/// Pre-probed state of each config disk (produced by probe, consumed by planner).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDisk {
    pub by_id_path: ByIdPath,
    pub state: ConfigDiskState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigDiskState {
    /// Device file doesn't exist (unplugged / absent).
    Absent,
    /// Device exists but is not LUKS-formatted.
    PresentNotLuks,
    /// Device exists, has LUKS header, UUID known.
    /// `mapper_open` = true if /dev/mapper/<name> is already active (crash recovery skip).
    PresentLuks { uuid: LuksUuid, mapper_open: bool },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanFlags {
    pub allow_remove_missing: bool,
    pub allow_remove_ambiguous: bool,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_valid_status_transition() {
        let next = ActionState::Pending
            .transition_to(ActionState::InProgress)
            .expect("pending -> in_progress should be valid");
        assert_eq!(next, ActionState::InProgress);
    }

    #[test]
    fn rejects_invalid_status_transition() {
        let err = ActionState::Completed
            .transition_to(ActionState::InProgress)
            .expect_err("completed -> in_progress should be invalid");
        assert_eq!(err.from, ActionState::Completed);
        assert_eq!(err.to, ActionState::InProgress);
    }

    #[test]
    fn failed_to_in_progress_allowed_for_retry() {
        let next = ActionState::Failed {
            error: "device not found".to_owned(),
        }
        .transition_to(ActionState::InProgress)
        .expect("failed -> in_progress should be valid (retry on resume)");
        assert_eq!(next, ActionState::InProgress);
    }

    #[test]
    fn blocks_conversion_for_blocked_plan() {
        let outcome = PlanOutcome::Blocked {
            plan_id: "p1".to_owned(),
            warnings: vec![],
            blocked_reasons: vec![BlockedReason {
                code: BlockedReasonCode::IdentityAmbiguousAbsentDisk,
                disk: None,
                message: "blocked".to_owned(),
            }],
        };

        let res = ApplicablePlan::try_from(outcome);
        assert!(res.is_err());
    }
}
