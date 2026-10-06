//! Deterministic provider-event normalization and session attention state.

use std::{collections::HashSet, fmt};

use serde::{Deserialize, Serialize};

use crate::domain::SessionState;

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
    UserInputRequested,
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
    SessionEnded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionStatusMachine {
    state: SessionState,
    active_subagents: HashSet<String>,
    subagent_capacity_exhausted: bool,
}

impl SessionStatusMachine {
    #[must_use]
    pub fn new(state: SessionState) -> Self {
        Self {
            state,
            active_subagents: HashSet::new(),
            subagent_capacity_exhausted: false,
        }
    }

    #[must_use]
    pub const fn state(&self) -> SessionState {
        self.state
    }

    pub fn apply(&mut self, event: &NormalizedProviderEvent) -> SessionState {
        if matches!(
            self.state,
            SessionState::Failed | SessionState::Terminated | SessionState::Disconnected
        ) {
            return self.state;
        }
        match event {
            NormalizedProviderEvent::PromptSubmitted
            | NormalizedProviderEvent::TurnStarted { .. } => {
                self.state = SessionState::Running;
            }
            NormalizedProviderEvent::PermissionRequested
            | NormalizedProviderEvent::UserInputRequested
            | NormalizedProviderEvent::TurnFailed { .. } => {
                self.state = SessionState::NeedsFeedback;
            }
            NormalizedProviderEvent::SubagentStarted { agent_id } => {
                if !self.active_subagents.contains(agent_id) {
                    if self.active_subagents.len() < MAX_TRACKED_ACTIVE_SUBAGENTS {
                        self.active_subagents.insert(agent_id.clone());
                    } else {
                        self.subagent_capacity_exhausted = true;
                    }
                }
                self.state = SessionState::Running;
            }
            NormalizedProviderEvent::SubagentStopped { agent_id } => {
                self.active_subagents.remove(agent_id);
            }
            NormalizedProviderEvent::TurnStopped { remaining_work } => {
                self.state = if self.active_subagents.is_empty()
                    && !self.subagent_capacity_exhausted
                    && remaining_work.is_empty()
                {
                    SessionState::FinishedUnseen
                } else {
                    SessionState::Running
                };
            }
            NormalizedProviderEvent::SessionEnded => {
                if !matches!(self.state, SessionState::Failed | SessionState::Terminated) {
                    self.state = SessionState::FinishedUnseen;
                }
            }
        }
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_subagent_keeps_a_stopped_turn_running() {
        let mut machine = SessionStatusMachine::new(SessionState::Fresh);
        machine.apply(&NormalizedProviderEvent::SubagentStarted {
            agent_id: "a".into(),
        });
        assert_eq!(
            machine.apply(&NormalizedProviderEvent::TurnStopped {
                remaining_work: RemainingWork::default(),
            }),
            SessionState::Running
        );
        machine.apply(&NormalizedProviderEvent::SubagentStopped {
            agent_id: "a".into(),
        });
        assert_eq!(
            machine.apply(&NormalizedProviderEvent::TurnStopped {
                remaining_work: RemainingWork::default(),
            }),
            SessionState::FinishedUnseen
        );
    }

    #[test]
    fn permission_requests_need_feedback() {
        let mut machine = SessionStatusMachine::new(SessionState::Running);
        assert_eq!(
            machine.apply(&NormalizedProviderEvent::PermissionRequested),
            SessionState::NeedsFeedback
        );
    }

    #[test]
    fn duplicate_subagent_events_are_idempotent() {
        let mut machine = SessionStatusMachine::new(SessionState::Running);
        let started = NormalizedProviderEvent::SubagentStarted {
            agent_id: "agent-1".into(),
        };
        machine.apply(&started);
        machine.apply(&started);
        machine.apply(&NormalizedProviderEvent::SubagentStopped {
            agent_id: "agent-1".into(),
        });
        assert_eq!(
            machine.apply(&NormalizedProviderEvent::TurnStopped {
                remaining_work: RemainingWork::default(),
            }),
            SessionState::FinishedUnseen
        );
    }

    #[test]
    fn subagent_tracking_is_bounded_and_fails_closed_after_capacity() {
        let mut machine = SessionStatusMachine::new(SessionState::Running);
        for index in 0..=MAX_TRACKED_ACTIVE_SUBAGENTS {
            machine.apply(&NormalizedProviderEvent::SubagentStarted {
                agent_id: format!("agent-{index}"),
            });
        }
        assert_eq!(machine.active_subagents.len(), MAX_TRACKED_ACTIVE_SUBAGENTS);
        assert!(machine.subagent_capacity_exhausted);
        for index in 0..MAX_TRACKED_ACTIVE_SUBAGENTS {
            machine.apply(&NormalizedProviderEvent::SubagentStopped {
                agent_id: format!("agent-{index}"),
            });
        }
        assert_eq!(
            machine.apply(&NormalizedProviderEvent::TurnStopped {
                remaining_work: RemainingWork::default(),
            }),
            SessionState::Running
        );
    }

    #[test]
    fn remaining_background_work_keeps_a_stopped_turn_running() {
        let mut machine = SessionStatusMachine::new(SessionState::Running);
        assert_eq!(
            machine.apply(&NormalizedProviderEvent::TurnStopped {
                remaining_work: RemainingWork {
                    background_tasks: true,
                    ..RemainingWork::default()
                },
            }),
            SessionState::Running
        );
    }

    #[test]
    fn recoverable_turn_failure_needs_feedback_without_ending_the_session() {
        let mut machine = SessionStatusMachine::new(SessionState::Running);
        assert_eq!(
            machine.apply(&NormalizedProviderEvent::TurnFailed {
                category: TurnFailureCategory::new("provider_unavailable").unwrap(),
            }),
            SessionState::NeedsFeedback
        );
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
        let mut machine = SessionStatusMachine::new(SessionState::Terminated);
        assert_eq!(
            machine.apply(&NormalizedProviderEvent::TurnStarted { conversation: None }),
            SessionState::Terminated
        );
    }
}
