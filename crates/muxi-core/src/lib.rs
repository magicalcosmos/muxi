//! Provider-neutral domain types and deterministic task state transitions.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use uuid::Uuid;

pub const CRATE_NAME: &str = "muxi-core";

pub type EventId = Uuid;
pub type TaskId = Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Draft,
    Analysis,
    Planning,
    AwaitingPlanApproval,
    Executing,
    Verifying,
    Reviewing,
    Completed,
    WaitingForUser,
    WaitingForPermission,
    WaitingForWorkspace,
    Paused,
    Cancelled,
    Failed,
    Recovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub phase: Phase,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl Task {
    pub fn new(title: impl Into<String>, now: OffsetDateTime) -> Self {
        Self {
            id: Uuid::now_v7(),
            title: title.into(),
            phase: Phase::Draft,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum DomainEvent {
    TaskCreated { task: Task },
    PhaseChanged { task_id: TaskId, phase: Phase },
    TaskRenamed { task_id: TaskId, title: String },
    RecoveryRequired { task_id: TaskId, reason: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppState {
    pub tasks: Vec<Task>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum StateError {
    #[error("task {0} was not found")]
    TaskNotFound(TaskId),
    #[error("task cannot transition from {from:?} to {to:?}")]
    InvalidTransition { from: Phase, to: Phase },
}

///
/// # Errors
///
/// Returns [`StateError`] when an event references an unknown task or requests an invalid phase transition.
pub fn reduce(mut state: AppState, event: &DomainEvent) -> Result<AppState, StateError> {
    match event {
        DomainEvent::TaskCreated { task } => state.tasks.push(task.clone()),
        DomainEvent::PhaseChanged { task_id, phase } => {
            let task = state
                .tasks
                .iter_mut()
                .find(|task| task.id == *task_id)
                .ok_or(StateError::TaskNotFound(*task_id))?;
            if !can_transition(task.phase, *phase) {
                return Err(StateError::InvalidTransition {
                    from: task.phase,
                    to: *phase,
                });
            }
            task.phase = *phase;
        }
        DomainEvent::TaskRenamed { task_id, title } => {
            let task = state
                .tasks
                .iter_mut()
                .find(|task| task.id == *task_id)
                .ok_or(StateError::TaskNotFound(*task_id))?;
            task.title.clone_from(title);
        }
        DomainEvent::RecoveryRequired { task_id, .. } => {
            let task = state
                .tasks
                .iter_mut()
                .find(|task| task.id == *task_id)
                .ok_or(StateError::TaskNotFound(*task_id))?;
            task.phase = Phase::Recovery;
        }
    }
    Ok(state)
}

fn can_transition(from: Phase, to: Phase) -> bool {
    matches!(
        (from, to),
        (Phase::Draft, Phase::Analysis)
            | (Phase::Analysis, Phase::Planning)
            | (Phase::Planning, Phase::AwaitingPlanApproval)
            | (
                Phase::AwaitingPlanApproval
                    | Phase::WaitingForPermission
                    | Phase::WaitingForUser
                    | Phase::Paused,
                Phase::Executing
            )
            | (
                Phase::Executing,
                Phase::Verifying | Phase::WaitingForPermission | Phase::WaitingForUser
            )
            | (Phase::Verifying, Phase::Reviewing)
            | (Phase::Reviewing, Phase::Completed)
            | (
                _,
                Phase::Paused | Phase::Cancelled | Phase::Failed | Phase::Recovery
            )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_lifecycle_is_deterministic() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let task = Task::new("fix bug", now);
        let id = task.id;
        let events = [
            DomainEvent::TaskCreated { task },
            DomainEvent::PhaseChanged {
                task_id: id,
                phase: Phase::Analysis,
            },
            DomainEvent::PhaseChanged {
                task_id: id,
                phase: Phase::Planning,
            },
        ];
        let state = events
            .iter()
            .try_fold(AppState::default(), reduce)
            .expect("valid lifecycle");
        assert_eq!(state.tasks[0].phase, Phase::Planning);
    }

    #[test]
    fn invalid_transition_is_rejected() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let task = Task::new("fix bug", now);
        let id = task.id;
        let state =
            reduce(AppState::default(), &DomainEvent::TaskCreated { task }).expect("created");
        let error = reduce(
            state,
            &DomainEvent::PhaseChanged {
                task_id: id,
                phase: Phase::Completed,
            },
        )
        .expect_err("cannot complete a draft");
        assert!(matches!(error, StateError::InvalidTransition { .. }));
    }
}
