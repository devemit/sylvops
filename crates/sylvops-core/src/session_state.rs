//! Independent process lifecycle, Provider activity, and durable attention contract.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLifecycle {
    #[default]
    Fresh,
    Starting,
    Running,
    Exited,
    Failed,
    Terminated,
    Disconnected,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderActivity {
    #[default]
    None,
    Working,
    AwaitingInput,
    Completed,
    Interrupted,
    RecoverableFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionReason {
    InputRequested,
    SuccessfulCompletion,
    RecoverableFailure,
    TerminalFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionAcknowledgement {
    Pending,
    Acknowledged,
    Resolved,
}

/// Monotonic identity within one Session, bounded to SQLite's positive integer range.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct AttentionEventId(u64);

impl TryFrom<u64> for AttentionEventId {
    type Error = &'static str;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if value == 0 || i64::try_from(value).is_err() {
            return Err("attention event identity is out of range");
        }
        Ok(Self(value))
    }
}

impl From<AttentionEventId> for u64 {
    fn from(value: AttentionEventId) -> Self {
        value.0
    }
}

/// Only the latest event is retained; resolved events retain the sequence for deduplication.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttentionEvent {
    pub id: AttentionEventId,
    pub reason: AttentionReason,
    pub acknowledgement: AttentionAcknowledgement,
    pub occurred_at: i64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "SessionStatusWire")]
pub struct SessionStatus {
    pub lifecycle: SessionLifecycle,
    pub activity: ProviderActivity,
    pub attention: Option<AttentionEvent>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionStatusWire {
    lifecycle: SessionLifecycle,
    activity: ProviderActivity,
    #[serde(deserialize_with = "Option::deserialize")]
    attention: Option<AttentionEvent>,
}

impl TryFrom<SessionStatusWire> for SessionStatus {
    type Error = &'static str;

    fn try_from(value: SessionStatusWire) -> Result<Self, Self::Error> {
        let status = Self {
            lifecycle: value.lifecycle,
            activity: value.activity,
            attention: value.attention,
        };
        status.validate()?;
        Ok(status)
    }
}

impl SessionStatus {
    /// Rejects impossible combinations at protocol and persistence boundaries.
    ///
    /// # Errors
    /// Returns an error for activity without a live process or inconsistent pending attention.
    pub fn validate(self) -> Result<(), &'static str> {
        if self.lifecycle != SessionLifecycle::Running && self.activity != ProviderActivity::None {
            return Err("Provider activity requires a running Session");
        }
        if matches!(
            self.lifecycle,
            SessionLifecycle::Fresh | SessionLifecycle::Starting
        ) && self.attention.is_some()
        {
            return Err("unstarted Session cannot carry attention");
        }
        let effective_reason = self
            .attention
            .filter(|event| event.acknowledgement != AttentionAcknowledgement::Resolved)
            .map(|event| event.reason);
        let expected_reason = match self.activity {
            ProviderActivity::AwaitingInput => Some(AttentionReason::InputRequested),
            ProviderActivity::Completed => Some(AttentionReason::SuccessfulCompletion),
            ProviderActivity::RecoverableFailure => Some(AttentionReason::RecoverableFailure),
            _ => None,
        };
        if self.lifecycle == SessionLifecycle::Running && effective_reason != expected_reason {
            return Err("Provider condition and attention disagree");
        }
        if let Some(event) = self.attention {
            if event.occurred_at < 0 {
                return Err("invalid attention timestamp");
            }
            if event.acknowledgement != AttentionAcknowledgement::Resolved {
                if event.reason == AttentionReason::InputRequested
                    && event.acknowledgement == AttentionAcknowledgement::Acknowledged
                {
                    return Err("input attention remains pending until resolved");
                }
                let valid = match event.reason {
                    AttentionReason::InputRequested => {
                        self.activity == ProviderActivity::AwaitingInput
                            || self.lifecycle == SessionLifecycle::Disconnected
                    }
                    AttentionReason::RecoverableFailure => {
                        self.activity == ProviderActivity::RecoverableFailure
                            || self.lifecycle == SessionLifecycle::Disconnected
                    }
                    AttentionReason::TerminalFailure => matches!(
                        self.lifecycle,
                        SessionLifecycle::Failed | SessionLifecycle::Disconnected
                    ),
                    AttentionReason::SuccessfulCompletion => {
                        self.activity == ProviderActivity::Completed
                            || matches!(
                                self.lifecycle,
                                SessionLifecycle::Exited | SessionLifecycle::Disconnected
                            )
                    }
                };
                if !valid {
                    return Err("attention does not match Session lifecycle or Provider activity");
                }
            }
        } else if matches!(
            self.activity,
            ProviderActivity::AwaitingInput
                | ProviderActivity::Completed
                | ProviderActivity::RecoverableFailure
        ) {
            return Err("Provider condition requires an attention event");
        }
        Ok(())
    }
}
