//! Presentation-only alerts for newly blocked Sessions. Never include terminal contents.

use crate::{
    domain::Session,
    ids::SessionId,
    session_state::{AttentionAcknowledgement, AttentionReason},
};
use std::collections::HashSet;

#[derive(Debug)]
pub struct AttentionTracker {
    waiting: HashSet<SessionId>,
}

impl AttentionTracker {
    pub fn new(sessions: &[Session]) -> Self {
        Self {
            waiting: waiting_sessions(sessions),
        }
    }

    pub fn update(&mut self, sessions: &[Session]) -> Option<String> {
        let waiting = waiting_sessions(sessions);
        let newly_waiting = sessions
            .iter()
            .filter(|session| waiting.contains(&session.id) && !self.waiting.contains(&session.id))
            .collect::<Vec<_>>();
        let message = match newly_waiting.as_slice() {
            [] => None,
            [session] => Some(format!(
                "{} needs your input or approval.",
                safe_label(&session.display_name)
            )),
            [first, ..] => Some(format!(
                "{} sessions need your input or approval, including {}.",
                newly_waiting.len(),
                safe_label(&first.display_name)
            )),
        };
        self.waiting = waiting;
        message
    }
}

fn waiting_sessions(sessions: &[Session]) -> HashSet<SessionId> {
    sessions
        .iter()
        .filter(|session| {
            session.status.attention.is_some_and(|event| {
                event.acknowledgement == AttentionAcknowledgement::Pending
                    && matches!(
                        event.reason,
                        AttentionReason::InputRequested | AttentionReason::RecoverableFailure
                    )
            })
        })
        .map(|session| session.id)
        .collect()
}

fn safe_label(label: &str) -> String {
    let label = label
        .chars()
        .filter(|character| !character.is_control())
        .take(80)
        .collect::<String>();
    if label.trim().is_empty() {
        "A session".into()
    } else {
        label
    }
}
