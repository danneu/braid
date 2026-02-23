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
