//! Deterministic provider-event normalization and session attention state.

use std::{collections::HashSet, fmt};

use serde::{Deserialize, Serialize};

use crate::session_state::{
    AttentionAcknowledgement, AttentionEvent, AttentionEventId, AttentionReason, ProviderActivity,
    SessionLifecycle, SessionStatus,
};

const MAX_PROVIDER_CONVERSATION_ID_LENGTH: usize = 200;
const MAX_TURN_FAILURE_CATEGORY_LENGTH: usize = 64;
const MAX_TRACKED_ACTIVE_SUBAGENTS: usize = 256;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProviderConversationId(String);

impl ProviderConversationId {
    /// Validates a provider-owned conversation identifier before it crosses the runtime seam.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is empty, oversized, or contains unsafe characters.
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        Self::try_from(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ProviderConversationId {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() || value.len() > MAX_PROVIDER_CONVERSATION_ID_LENGTH {
            return Err(format!(
                "provider conversation ID must be between 1 and {MAX_PROVIDER_CONVERSATION_ID_LENGTH} characters"
            ));
        }
        if !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        {
            return Err("provider conversation ID contains unsupported characters".into());
        }
        Ok(Self(value))
    }
}

impl From<ProviderConversationId> for String {
    fn from(value: ProviderConversationId) -> Self {
        value.0
    }
}

impl fmt::Display for ProviderConversationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TurnFailureCategory(String);

impl TurnFailureCategory {
    /// Creates a bounded machine-readable category. Raw provider diagnostics are not accepted.
    ///
    /// # Errors
    ///
    /// Returns an error unless the value is a short lowercase identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        Self::try_from(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TurnFailureCategory {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() || value.len() > MAX_TURN_FAILURE_CATEGORY_LENGTH {
            return Err(format!(
                "turn failure category must be between 1 and {MAX_TURN_FAILURE_CATEGORY_LENGTH} characters"
            ));
        }
        if !value.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_')
        }) {
            return Err("turn failure category must be a lowercase machine identifier".into());
        }
        Ok(Self(value))
    }
}

impl From<TurnFailureCategory> for String {
    fn from(value: TurnFailureCategory) -> Self {
        value.0
    }
}

impl fmt::Display for TurnFailureCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationIdentityTransition {
    Established,
    Cleared,
    Resumed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConversationIdentity {
    pub id: ProviderConversationId,
    pub transition: ConversationIdentityTransition,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RemainingWork {
    pub active_subagents: bool,
    pub background_tasks: bool,
    pub scheduled_tasks: bool,
}

impl RemainingWork {
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.active_subagents && !self.background_tasks && !self.scheduled_tasks
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NormalizedProviderEvent {
    PromptSubmitted,
    TurnStarted {
        conversation: Option<ConversationIdentity>,
    },
    PermissionRequested,
    /// A provider permission check, which may be decided automatically without user input.
    PermissionCheckStarted,
    UserInputRequested,
    /// Observational tool execution; does not imply a user-input request.
    ToolStarted,
    ToolCompleted,
    SubagentStarted {
        agent_id: String,
    },
    SubagentStopped {
        agent_id: String,
    },
    TurnStopped {
        remaining_work: RemainingWork,
    },
    /// A recoverable provider turn failure; the owning process and Session remain live.
    TurnFailed {
        category: TurnFailureCategory,
    },
    /// The user interrupted the current turn; the owning process may remain live.
    TurnInterrupted,
    SessionEnded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionStatusMachine {
    status: SessionStatus,
    failure_category: Option<TurnFailureCategory>,
    active_subagents: HashSet<String>,
    subagent_capacity_exhausted: bool,
    remaining_work: RemainingWork,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionStatusTransition {
    pub status: SessionStatus,
    pub failure_category: Option<TurnFailureCategory>,
}

impl SessionStatusMachine {
    #[must_use]
    pub fn new(lifecycle: SessionLifecycle) -> Self {
        Self {
            status: SessionStatus {
                lifecycle,
                ..SessionStatus::default()
            },
            failure_category: None,
            active_subagents: HashSet::new(),
            subagent_capacity_exhausted: false,
            remaining_work: RemainingWork::default(),
        }
    }

    #[must_use]
    pub fn from_persisted(
        status: SessionStatus,
        failure_category: Option<TurnFailureCategory>,
    ) -> Self {
        Self {
            status,
            failure_category,
            active_subagents: HashSet::new(),
            subagent_capacity_exhausted: false,
            remaining_work: RemainingWork::default(),
        }
    }

    #[must_use]
    pub const fn status(&self) -> SessionStatus {
        self.status
    }

    /// Mirrors one daemon-persisted acknowledgement into the live reducer without changing
    /// lifecycle, Provider activity, or in-flight work tracking.
    #[must_use]
    pub fn acknowledge_attention(
        &mut self,
        event_id: AttentionEventId,
        reason: AttentionReason,
    ) -> bool {
        let Some(event) = self.status.attention.as_mut() else {
            return false;
        };
        if event.id != event_id
            || event.reason != reason
            || event.acknowledgement != AttentionAcknowledgement::Pending
        {
            return false;
        }
        event.acknowledgement = AttentionAcknowledgement::Acknowledged;
        true
    }

    /// Applies one trusted Provider outcome and derives independent Session status dimensions.
    ///
    /// # Errors
    /// Returns an error if the per-Session Attention Event identity is exhausted.
    pub fn apply(
        &mut self,
        event: &NormalizedProviderEvent,
        occurred_at: i64,
    ) -> Result<SessionStatusTransition, &'static str> {
        if matches!(
            self.status.lifecycle,
            SessionLifecycle::Exited
                | SessionLifecycle::Failed
                | SessionLifecycle::Terminated
                | SessionLifecycle::Disconnected
        ) {
            return Ok(self.transition());
        }
        self.status.lifecycle = SessionLifecycle::Running;
        match event {
            NormalizedProviderEvent::ToolStarted | NormalizedProviderEvent::ToolCompleted => {}
            NormalizedProviderEvent::PermissionCheckStarted => {
                self.set_live_condition(ProviderActivity::Working, None, occurred_at)?;
            }
            NormalizedProviderEvent::PromptSubmitted
            | NormalizedProviderEvent::TurnStarted { .. } => {
                self.active_subagents.clear();
                self.subagent_capacity_exhausted = false;
                self.remaining_work = RemainingWork::default();
                self.set_live_condition(ProviderActivity::Working, None, occurred_at)?;
            }
            NormalizedProviderEvent::PermissionRequested
            | NormalizedProviderEvent::UserInputRequested => {
                self.set_live_condition(
                    ProviderActivity::AwaitingInput,
                    Some(AttentionReason::InputRequested),
                    occurred_at,
                )?;
            }
            NormalizedProviderEvent::TurnFailed { category } => {
                let materially_new = self
                    .failure_category
                    .as_ref()
                    .is_some_and(|current| current != category);
                self.failure_category = Some(category.clone());
                self.status.activity = ProviderActivity::RecoverableFailure;
                self.set_attention_condition(
                    AttentionReason::RecoverableFailure,
                    occurred_at,
                    materially_new,
                )?;
                self.status.validate()?;
            }
            NormalizedProviderEvent::SubagentStarted { agent_id } => {
                if !self.active_subagents.contains(agent_id) {
                    if self.active_subagents.len() < MAX_TRACKED_ACTIVE_SUBAGENTS {
                        self.active_subagents.insert(agent_id.clone());
                    } else {
                        self.subagent_capacity_exhausted = true;
                    }
                }
                if self.status.activity != ProviderActivity::AwaitingInput {
                    self.set_live_condition(ProviderActivity::Working, None, occurred_at)?;
                }
            }
            NormalizedProviderEvent::SubagentStopped { agent_id } => {
                self.active_subagents.remove(agent_id);
            }
            NormalizedProviderEvent::TurnStopped { remaining_work } => {
                self.remaining_work = *remaining_work;
                if self.active_subagents.is_empty()
                    && !self.subagent_capacity_exhausted
                    && remaining_work.is_empty()
                {
                    self.set_live_condition(
                        ProviderActivity::Completed,
                        Some(AttentionReason::SuccessfulCompletion),
                        occurred_at,
                    )?;
                } else {
                    self.set_live_condition(ProviderActivity::Working, None, occurred_at)?;
                }
            }
            NormalizedProviderEvent::TurnInterrupted => {
                self.set_live_condition(ProviderActivity::Interrupted, None, occurred_at)?;
            }
            NormalizedProviderEvent::SessionEnded => {
                if !matches!(
                    self.status.activity,
                    ProviderActivity::Completed
                        | ProviderActivity::Interrupted
                        | ProviderActivity::AwaitingInput
                        | ProviderActivity::RecoverableFailure
                ) && self.active_subagents.is_empty()
                    && !self.subagent_capacity_exhausted
                    && self.remaining_work.is_empty()
                {
                    self.set_live_condition(
                        ProviderActivity::Completed,
                        Some(AttentionReason::SuccessfulCompletion),
                        occurred_at,
                    )?;
                }
            }
        }
        Ok(self.transition())
    }

    /// Applies a process outcome after Provider-event processing has quiesced for the Session.
    ///
    /// # Errors
    /// Returns an error if the per-Session Attention Event identity is exhausted.
    pub fn apply_process_exit(
        &mut self,
        plain_shell: bool,
        exit_code: Option<i32>,
        stop_requested: bool,
        occurred_at: i64,
    ) -> Result<SessionStatusTransition, &'static str> {
        if stop_requested {
            self.status.lifecycle = SessionLifecycle::Terminated;
            self.status.activity = ProviderActivity::None;
            self.failure_category = None;
            self.resolve_attention();
            self.status.validate()?;
            return Ok(self.transition());
        }

        let has_completion = self.status.attention.is_some_and(|event| {
            event.reason == AttentionReason::SuccessfulCompletion
                && event.acknowledgement != AttentionAcknowledgement::Resolved
        });
        if (plain_shell && exit_code == Some(0)) || has_completion {
            self.status.lifecycle = SessionLifecycle::Exited;
            self.status.activity = ProviderActivity::None;
            self.failure_category = None;
            self.set_attention(AttentionReason::SuccessfulCompletion, occurred_at)?;
        } else if self.status.activity == ProviderActivity::Interrupted && exit_code == Some(0) {
            self.status.lifecycle = SessionLifecycle::Exited;
            self.status.activity = ProviderActivity::None;
            self.failure_category = None;
            self.resolve_attention();
        } else {
            self.status.lifecycle = SessionLifecycle::Failed;
            self.status.activity = ProviderActivity::None;
            let category = if exit_code.is_some_and(|code| code != 0) {
                "process_exit_nonzero"
            } else {
                "process_exit_without_completion"
            };
            self.failure_category = Some(
                TurnFailureCategory::new(category)
                    .map_err(|_| "invalid daemon failure category")?,
            );
            self.set_attention(AttentionReason::TerminalFailure, occurred_at)?;
        }
        self.status.validate()?;
        Ok(self.transition())
    }

    /// A distinct, correlated request replaces even a reviewed input event.
    ///
    /// # Errors
    /// Returns an error for an inactive Session, exhausted event identity, or invalid status.
    pub fn renew_input_attention(&mut self, occurred_at: i64) -> Result<(), &'static str> {
        if self.status.lifecycle != SessionLifecycle::Running {
            return Err("input requires a running Session");
        }
        self.status.activity = ProviderActivity::AwaitingInput;
        self.set_attention_condition(AttentionReason::InputRequested, occurred_at, true)?;
        self.status.validate()
    }

    /// Confirmed completion resolves only the exact currently observed input event.
    ///
    /// # Errors
    /// Returns an error if the resulting status is invalid.
    pub fn resolve_input_attention(
        &mut self,
        event_id: AttentionEventId,
        occurred_at: i64,
    ) -> Result<(), &'static str> {
        if self.status.activity == ProviderActivity::AwaitingInput
            && self
                .status
                .attention
                .is_some_and(|event| event.id == event_id)
        {
            self.set_live_condition(ProviderActivity::Working, None, occurred_at)?;
        }
        Ok(())
    }

    fn set_live_condition(
        &mut self,
        activity: ProviderActivity,
        reason: Option<AttentionReason>,
        occurred_at: i64,
    ) -> Result<(), &'static str> {
        self.status.activity = activity;
        if activity != ProviderActivity::RecoverableFailure {
            self.failure_category = None;
        }
        if let Some(reason) = reason {
            self.set_attention(reason, occurred_at)?;
        } else {
            self.resolve_attention();
        }
        self.status.validate()
    }

    fn set_attention(
        &mut self,
        reason: AttentionReason,
        occurred_at: i64,
    ) -> Result<(), &'static str> {
        self.set_attention_condition(reason, occurred_at, false)
    }

    fn set_attention_condition(
        &mut self,
        reason: AttentionReason,
        occurred_at: i64,
        materially_new: bool,
    ) -> Result<(), &'static str> {
        if !materially_new
            && self.status.attention.is_some_and(|event| {
                event.reason == reason
                    && event.acknowledgement != AttentionAcknowledgement::Resolved
            })
        {
            return Ok(());
        }
        let next = self
            .status
            .attention
            .map_or(Some(1), |event| u64::from(event.id).checked_add(1))
            .ok_or("attention event identity is out of range")?;
        self.status.attention = Some(AttentionEvent {
            id: AttentionEventId::try_from(next)?,
            reason,
            acknowledgement: AttentionAcknowledgement::Pending,
            occurred_at,
        });
        Ok(())
    }

    fn resolve_attention(&mut self) {
        if let Some(event) = &mut self.status.attention {
            event.acknowledgement = AttentionAcknowledgement::Resolved;
        }
    }

    /// Captures status and failure metadata without applying another provider event.
    #[must_use]
    pub fn transition(&self) -> SessionStatusTransition {
        SessionStatusTransition {
            status: self.status,
            failure_category: self.failure_category.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(machine: &mut SessionStatusMachine, event: &NormalizedProviderEvent) -> SessionStatus {
        machine.apply(event, 0).unwrap().status
    }

    #[test]
    fn active_subagent_keeps_a_stopped_turn_running() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Fresh);
        apply(
            &mut machine,
            &NormalizedProviderEvent::SubagentStarted {
                agent_id: "a".into(),
            },
        );
        assert_eq!(
            apply(
                &mut machine,
                &NormalizedProviderEvent::TurnStopped {
                    remaining_work: RemainingWork::default(),
                }
            ),
            SessionStatus {
                lifecycle: SessionLifecycle::Running,
                activity: ProviderActivity::Working,
                attention: None,
            }
        );
        apply(
            &mut machine,
            &NormalizedProviderEvent::SubagentStopped {
                agent_id: "a".into(),
            },
        );
        let completed = apply(
            &mut machine,
            &NormalizedProviderEvent::TurnStopped {
                remaining_work: RemainingWork::default(),
            },
        );
        assert_eq!(completed.lifecycle, SessionLifecycle::Running);
        assert_eq!(completed.activity, ProviderActivity::Completed);
        assert_eq!(
            completed.attention.unwrap().reason,
            AttentionReason::SuccessfulCompletion
        );
    }

    #[test]
    fn permission_checks_are_neutral_and_clear_legacy_input_attention() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
        let old = apply(&mut machine, &NormalizedProviderEvent::PermissionRequested);
        for _ in 0..2 {
            let checked = apply(
                &mut machine,
                &NormalizedProviderEvent::PermissionCheckStarted,
            );
            assert_eq!(checked.lifecycle, SessionLifecycle::Running);
            assert_eq!(checked.activity, ProviderActivity::Working);
            assert_eq!(checked.attention.unwrap().id, old.attention.unwrap().id);
            assert_eq!(
                checked.attention.unwrap().acknowledgement,
                AttentionAcknowledgement::Resolved
            );
        }
        let event = NormalizedProviderEvent::PermissionCheckStarted;
        let encoded = serde_json::to_string(&event).unwrap();
        assert_eq!(
            serde_json::from_str::<NormalizedProviderEvent>(&encoded).unwrap(),
            event
        );
    }

    #[test]
    fn permission_requests_need_feedback() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
        let status = apply(&mut machine, &NormalizedProviderEvent::PermissionRequested);
        assert_eq!(status.lifecycle, SessionLifecycle::Running);
        assert_eq!(status.activity, ProviderActivity::AwaitingInput);
    }

    #[test]
    fn reviewing_input_preserves_waiting_and_does_not_restore_unread_on_refresh() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
        let requested = apply(&mut machine, &NormalizedProviderEvent::PermissionRequested);
        let event = requested.pending_review().expect("input can be reviewed");
        assert!(machine.acknowledge_attention(event.id, event.reason));
        let reviewed = machine.status();
        assert_eq!(reviewed.activity, ProviderActivity::AwaitingInput);
        assert_eq!(
            reviewed.attention.unwrap().acknowledgement,
            AttentionAcknowledgement::Acknowledged
        );
        reviewed.validate().unwrap();
        let encoded = serde_json::to_vec(&reviewed).unwrap();
        let decoded: SessionStatus = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, reviewed);
        assert!(decoded.pending_review().is_none());
        assert_eq!(
            apply(&mut machine, &NormalizedProviderEvent::PermissionRequested),
            reviewed
        );
        let mut client = requested;
        assert!(client.apply_attention_acknowledgement(reviewed));
        assert_eq!(client, reviewed);
    }

    #[test]
    fn duplicate_subagent_events_are_idempotent() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
        let started = NormalizedProviderEvent::SubagentStarted {
            agent_id: "agent-1".into(),
        };
        apply(&mut machine, &started);
        apply(&mut machine, &started);
        apply(
            &mut machine,
            &NormalizedProviderEvent::SubagentStopped {
                agent_id: "agent-1".into(),
            },
        );
        let status = apply(
            &mut machine,
            &NormalizedProviderEvent::TurnStopped {
                remaining_work: RemainingWork::default(),
            },
        );
        assert_eq!(status.activity, ProviderActivity::Completed);
    }

    #[test]
    fn other_work_does_not_clear_an_outstanding_input_request() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
        let requested = apply(&mut machine, &NormalizedProviderEvent::PermissionRequested);
        for event in [
            NormalizedProviderEvent::ToolStarted,
            NormalizedProviderEvent::ToolCompleted,
            NormalizedProviderEvent::SubagentStarted {
                agent_id: "parallel".into(),
            },
        ] {
            assert_eq!(apply(&mut machine, &event), requested);
        }
    }

    #[test]
    fn subagent_tracking_is_bounded_and_fails_closed_after_capacity() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
        for index in 0..=MAX_TRACKED_ACTIVE_SUBAGENTS {
            apply(
                &mut machine,
                &NormalizedProviderEvent::SubagentStarted {
                    agent_id: format!("agent-{index}"),
                },
            );
        }
        assert_eq!(machine.active_subagents.len(), MAX_TRACKED_ACTIVE_SUBAGENTS);
        assert!(machine.subagent_capacity_exhausted);
        for index in 0..MAX_TRACKED_ACTIVE_SUBAGENTS {
            apply(
                &mut machine,
                &NormalizedProviderEvent::SubagentStopped {
                    agent_id: format!("agent-{index}"),
                },
            );
        }
        let status = apply(
            &mut machine,
            &NormalizedProviderEvent::TurnStopped {
                remaining_work: RemainingWork::default(),
            },
        );
        assert_eq!(status.activity, ProviderActivity::Working);
    }

    #[test]
    fn remaining_background_work_keeps_a_stopped_turn_running() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
        let status = apply(
            &mut machine,
            &NormalizedProviderEvent::TurnStopped {
                remaining_work: RemainingWork {
                    background_tasks: true,
                    ..RemainingWork::default()
                },
            },
        );
        assert_eq!(status.activity, ProviderActivity::Working);
    }

    #[test]
    fn recoverable_turn_failure_needs_feedback_without_ending_the_session() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
        let status = apply(
            &mut machine,
            &NormalizedProviderEvent::TurnFailed {
                category: TurnFailureCategory::new("provider_unavailable").unwrap(),
            },
        );
        assert_eq!(status.lifecycle, SessionLifecycle::Running);
        assert_eq!(status.activity, ProviderActivity::RecoverableFailure);
    }

    #[test]
    fn interruption_is_not_completion_even_when_the_process_exits_cleanly() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
        let input = machine
            .apply(&NormalizedProviderEvent::UserInputRequested, 10)
            .unwrap();
        let input_id = input.status.attention.unwrap().id;
        let interrupted = machine
            .apply(&NormalizedProviderEvent::TurnInterrupted, 11)
            .unwrap();
        assert_eq!(interrupted.status.activity, ProviderActivity::Interrupted);
        assert_eq!(interrupted.status.lifecycle, SessionLifecycle::Running);
        assert_eq!(
            interrupted.status.attention.unwrap().acknowledgement,
            AttentionAcknowledgement::Resolved
        );

        let ended = machine
            .apply(&NormalizedProviderEvent::SessionEnded, 12)
            .unwrap();
        assert_eq!(ended.status.activity, ProviderActivity::Interrupted);
        let exited = machine
            .apply_process_exit(false, Some(0), false, 13)
            .unwrap();
        assert_eq!(exited.status.lifecycle, SessionLifecycle::Exited);
        assert_eq!(exited.status.activity, ProviderActivity::None);
        assert_eq!(exited.status.attention.unwrap().id, input_id);
        assert_ne!(
            exited.status.attention.unwrap().reason,
            AttentionReason::SuccessfulCompletion
        );
    }

    #[test]
    fn completion_identity_is_reused_by_session_end_and_process_exit() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
        let completed = machine
            .apply(
                &NormalizedProviderEvent::TurnStopped {
                    remaining_work: RemainingWork::default(),
                },
                10,
            )
            .unwrap();
        let completion = completed.status.attention.unwrap();
        let ended = machine
            .apply(&NormalizedProviderEvent::SessionEnded, 11)
            .unwrap();
        assert_eq!(ended.status.attention, Some(completion));
        let exited = machine
            .apply_process_exit(false, Some(0), false, 12)
            .unwrap();
        assert_eq!(exited.status.attention, Some(completion));
    }

    #[test]
    fn terminal_failure_replaces_recoverable_failure_with_a_new_identity() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
        let recoverable = machine
            .apply(
                &NormalizedProviderEvent::TurnFailed {
                    category: TurnFailureCategory::new("rate_limit").unwrap(),
                },
                10,
            )
            .unwrap();
        let recoverable_id = recoverable.status.attention.unwrap().id;
        assert_eq!(recoverable.failure_category.unwrap().as_str(), "rate_limit");

        let failed = machine
            .apply_process_exit(false, Some(17), false, 11)
            .unwrap();
        let terminal = failed.status.attention.unwrap();
        assert_eq!(terminal.reason, AttentionReason::TerminalFailure);
        assert!(u64::from(terminal.id) > u64::from(recoverable_id));
        assert_eq!(
            failed.failure_category.unwrap().as_str(),
            "process_exit_nonzero"
        );
    }

    #[test]
    fn explicit_stop_resolves_every_live_condition_and_ignores_late_completion() {
        for event in [
            NormalizedProviderEvent::PromptSubmitted,
            NormalizedProviderEvent::PermissionRequested,
            NormalizedProviderEvent::TurnStopped {
                remaining_work: RemainingWork::default(),
            },
            NormalizedProviderEvent::TurnFailed {
                category: TurnFailureCategory::new("network").unwrap(),
            },
        ] {
            for exit_code in [Some(0), Some(1), None] {
                let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
                machine.apply(&event, 10).unwrap();
                let stopped = machine
                    .apply_process_exit(false, exit_code, true, 11)
                    .unwrap();
                assert_eq!(stopped.status.lifecycle, SessionLifecycle::Terminated);
                assert_eq!(stopped.status.activity, ProviderActivity::None);
                assert!(stopped.failure_category.is_none());
                assert!(stopped.status.attention.is_none_or(
                    |event| event.acknowledgement == AttentionAcknowledgement::Resolved
                ));
                let late = machine
                    .apply(&NormalizedProviderEvent::SessionEnded, 12)
                    .unwrap();
                assert_eq!(late, stopped);
            }
        }
    }

    #[test]
    fn recoverable_failure_categories_deduplicate_only_unchanged_conditions() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
        let rate_limit = NormalizedProviderEvent::TurnFailed {
            category: TurnFailureCategory::new("rate_limit").unwrap(),
        };
        let first = machine.apply(&rate_limit, 10).unwrap();
        let first_id = first.status.attention.unwrap().id;

        let unchanged = machine.apply(&rate_limit, 11).unwrap();
        assert_eq!(unchanged.status.attention.unwrap().id, first_id);

        let network = machine
            .apply(
                &NormalizedProviderEvent::TurnFailed {
                    category: TurnFailureCategory::new("network").unwrap(),
                },
                12,
            )
            .unwrap();
        let network_event = network.status.attention.unwrap();
        assert!(u64::from(network_event.id) > u64::from(first_id));
        assert_eq!(network_event.occurred_at, 12);
        assert_eq!(network.failure_category.unwrap().as_str(), "network");

        let resolved = machine
            .apply(&NormalizedProviderEvent::PromptSubmitted, 13)
            .unwrap();
        assert_eq!(
            resolved.status.attention.unwrap().acknowledgement,
            AttentionAcknowledgement::Resolved
        );
        let reentered = machine
            .apply(
                &NormalizedProviderEvent::TurnFailed {
                    category: TurnFailureCategory::new("network").unwrap(),
                },
                14,
            )
            .unwrap();
        assert!(u64::from(reentered.status.attention.unwrap().id) > u64::from(network_event.id));
    }

    #[test]
    fn session_end_does_not_complete_known_remaining_work() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
        machine
            .apply(
                &NormalizedProviderEvent::TurnStopped {
                    remaining_work: RemainingWork {
                        background_tasks: true,
                        ..RemainingWork::default()
                    },
                },
                10,
            )
            .unwrap();
        let ended = machine
            .apply(&NormalizedProviderEvent::SessionEnded, 11)
            .unwrap();
        assert_eq!(ended.status.activity, ProviderActivity::Working);
        assert!(ended.status.attention.is_none());
    }

    #[test]
    fn provider_derived_identity_and_failure_categories_are_bounded() {
        assert!(ProviderConversationId::new("conversation-123").is_ok());
        assert!(ProviderConversationId::new("../../conversation").is_err());
        assert!(ProviderConversationId::new("a".repeat(201)).is_err());

        assert!(TurnFailureCategory::new("rate_limited").is_ok());
        assert!(TurnFailureCategory::new("raw provider error: prompt text").is_err());
        assert!(TurnFailureCategory::new("a".repeat(65)).is_err());
    }

    #[test]
    fn late_hook_cannot_resurrect_a_terminal_session() {
        let mut machine = SessionStatusMachine::new(SessionLifecycle::Terminated);
        let status = apply(
            &mut machine,
            &NormalizedProviderEvent::TurnStarted { conversation: None },
        );
        assert_eq!(status.lifecycle, SessionLifecycle::Terminated);
    }
}
