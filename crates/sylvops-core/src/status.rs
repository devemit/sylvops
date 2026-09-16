//! Deterministic provider-event normalization and session attention state.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::domain::SessionState;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NormalizedProviderEvent {
    PromptSubmitted,
    TurnStarted,
    PermissionRequested,
    UserInputRequested,
    SubagentStarted { agent_id: String },
    SubagentStopped { agent_id: String },
    TurnStopped,
    SessionEnded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionStatusMachine {
    state: SessionState,
    active_subagents: HashSet<String>,
}

impl SessionStatusMachine {
    #[must_use]
    pub fn new(state: SessionState) -> Self {
        Self {
            state,
            active_subagents: HashSet::new(),
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
            NormalizedProviderEvent::PromptSubmitted | NormalizedProviderEvent::TurnStarted => {
                self.state = SessionState::Running;
            }
            NormalizedProviderEvent::PermissionRequested
            | NormalizedProviderEvent::UserInputRequested => {
                self.state = SessionState::NeedsFeedback;
            }
            NormalizedProviderEvent::SubagentStarted { agent_id } => {
                self.active_subagents.insert(agent_id.clone());
                self.state = SessionState::Running;
            }
            NormalizedProviderEvent::SubagentStopped { agent_id } => {
                self.active_subagents.remove(agent_id);
            }
            NormalizedProviderEvent::TurnStopped => {
                self.state = if self.active_subagents.is_empty() {
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
            machine.apply(&NormalizedProviderEvent::TurnStopped),
            SessionState::Running
        );
        machine.apply(&NormalizedProviderEvent::SubagentStopped {
            agent_id: "a".into(),
        });
        assert_eq!(
            machine.apply(&NormalizedProviderEvent::TurnStopped),
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
            machine.apply(&NormalizedProviderEvent::TurnStopped),
            SessionState::FinishedUnseen
        );
    }

    #[test]
    fn late_hook_cannot_resurrect_a_terminal_session() {
        let mut machine = SessionStatusMachine::new(SessionState::Terminated);
        assert_eq!(
            machine.apply(&NormalizedProviderEvent::PromptSubmitted),
            SessionState::Terminated
        );
    }
}
