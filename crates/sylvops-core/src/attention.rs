//! Presentation-only alerts for newly blocked Sessions. Never include terminal contents.

use crate::{
    domain::Session,
    ids::SessionId,
    session_state::{AttentionAcknowledgement, AttentionEvent, AttentionEventId, AttentionReason},
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const MAX_ATTENTION_NOTIFICATION_LABEL_CHARS: usize = 80;

/// Observational callback health; never changes process state or creates attention.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleHealth {
    NotApplicable,
    Waiting,
    Ready,
    Missing,
}

impl LifecycleHealth {
    pub const fn guidance(self) -> &'static str {
        match self {
            Self::NotApplicable => "Lifecycle monitoring is not applicable.",
            Self::Waiting => {
                "Waiting for provider lifecycle callbacks. Review Codex hooks with /hooks if prompted."
            }
            Self::Ready => "Provider lifecycle callbacks received.",
            Self::Missing => {
                "Provider lifecycle callbacks are missing; attention and notifications may be incomplete. In Codex, review and trust SylvOps hooks with /hooks; then submit a prompt. Check provider hook configuration if this persists."
            }
        }
    }
}

/// Shared daemon-backed channel preferences for Attention Notifications.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AttentionNotificationPreferences {
    pub native_notifications_enabled: bool,
    pub sound_enabled: bool,
}

impl Default for AttentionNotificationPreferences {
    fn default() -> Self {
        Self {
            native_notifications_enabled: true,
            sound_enabled: true,
        }
    }
}

/// One live, daemon-coordinated right to present an Attention Event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttentionNotificationClaim {
    pub session_id: SessionId,
    pub attention_id: AttentionEventId,
    pub reason: AttentionReason,
    pub session_display_name: String,
    pub preferences: AttentionNotificationPreferences,
}

impl AttentionNotificationClaim {
    /// Fixed-content presentation probe. Does not create or acknowledge an Attention Event.
    ///
    /// # Panics
    /// Panics only if the fixed valid Attention Event identity is rejected by its validator.
    pub fn test(preferences: AttentionNotificationPreferences) -> Self {
        Self {
            session_id: SessionId::new(),
            attention_id: AttentionEventId::try_from(1).expect("valid fixed event identity"),
            reason: AttentionReason::InputRequested,
            session_display_name: "SylvOps notification test".into(),
            preferences,
        }
    }

    #[must_use]
    pub fn new(
        session_id: SessionId,
        display_name: &str,
        event: AttentionEvent,
        preferences: AttentionNotificationPreferences,
    ) -> Self {
        Self {
            session_id,
            attention_id: event.id,
            reason: event.reason,
            session_display_name: safe_label(display_name),
            preferences,
        }
    }
}

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
        .take(MAX_ATTENTION_NOTIFICATION_LABEL_CHARS)
        .collect::<String>();
    if label.trim().is_empty() {
        "A session".into()
    } else {
        label
    }
}
