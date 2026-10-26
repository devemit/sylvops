//! Memory-only correlation of observational Codex tool and permission hooks.

use std::collections::HashMap;

use serde_json::Value;
use sha2::{Digest, Sha256};
use sylvops_core::status::NormalizedProviderEvent;

const CAPACITY: usize = 2_048;

#[derive(Clone, Debug)]
struct Call {
    key: [u8; 32],
    requested: bool,
    completed: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct InputTracking {
    turn: Option<String>,
    calls: HashMap<String, Call>,
    uncertain: bool,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum InputChange {
    #[default]
    None,
    NewRequest,
    Resolved,
    Ignore,
}

impl InputTracking {
    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }
    pub(crate) fn observe(
        &mut self,
        payload: &[u8],
        event: &NormalizedProviderEvent,
    ) -> InputChange {
        let Ok(value) = serde_json::from_slice::<Value>(payload) else {
            return InputChange::None;
        };
        let turn = identifier(&value, "turn_id");
        if matches!(
            event,
            NormalizedProviderEvent::PromptSubmitted | NormalizedProviderEvent::TurnStarted { .. }
        ) {
            *self = Self {
                turn: turn.map(str::to_owned),
                ..Self::default()
            };
            return InputChange::None;
        }
        if !matches!(
            event,
            NormalizedProviderEvent::ToolStarted
                | NormalizedProviderEvent::ToolCompleted
                | NormalizedProviderEvent::PermissionRequested
        ) {
            return InputChange::None;
        }
        let Some(turn) = turn else {
            if matches!(event, NormalizedProviderEvent::PermissionRequested) {
                self.uncertain = true;
            }
            return InputChange::None;
        };
        if self.turn.as_deref().is_some_and(|current| current != turn) {
            return InputChange::Ignore;
        }
        self.turn.get_or_insert_with(|| turn.to_owned());
        let Some(key) = tool_key(&value) else {
            if matches!(event, NormalizedProviderEvent::PermissionRequested) {
                self.uncertain = true;
            }
            return InputChange::None;
        };
        self.observe_call(&value, event, key)
    }

    fn observe_call(
        &mut self,
        value: &Value,
        event: &NormalizedProviderEvent,
        key: [u8; 32],
    ) -> InputChange {
        match event {
            NormalizedProviderEvent::ToolStarted => {
                let Some(id) = identifier(value, "tool_use_id") else {
                    return InputChange::None;
                };
                if let Some(call) = self.calls.get(id) {
                    if call.key != key {
                        self.uncertain = true;
                    }
                    return InputChange::Ignore;
                }
                if self.calls.len() >= CAPACITY {
                    self.uncertain = true;
                } else {
                    self.calls.insert(
                        id.to_owned(),
                        Call {
                            key,
                            requested: false,
                            completed: false,
                        },
                    );
                }
            }
            NormalizedProviderEvent::PermissionRequested => {
                let mut matching = self
                    .calls
                    .values_mut()
                    .filter(|call| call.key == key && !call.completed);
                let first = matching.next();
                if matching.next().is_some() {
                    self.uncertain = true;
                } else if let Some(call) = first {
                    if call.requested {
                        return InputChange::Ignore;
                    }
                    call.requested = true;
                    self.generation = self.generation.saturating_add(1);
                    return InputChange::NewRequest;
                } else {
                    if self
                        .calls
                        .values()
                        .any(|call| call.key == key && call.completed)
                    {
                        return InputChange::Ignore;
                    }
                    self.uncertain = true;
                }
            }
            NormalizedProviderEvent::ToolCompleted => {
                let Some(id) = identifier(value, "tool_use_id") else {
                    return InputChange::None;
                };
                let Some(call) = self.calls.get_mut(id) else {
                    return InputChange::None;
                };
                if call.key != key {
                    return InputChange::None;
                }
                if call.completed {
                    return InputChange::Ignore;
                }
                call.completed = true;
                if call.requested
                    && !self.uncertain
                    && self
                        .calls
                        .values()
                        .all(|call| !call.requested || call.completed)
                {
                    return InputChange::Resolved;
                }
            }
            _ => {}
        }
        InputChange::None
    }
}

fn identifier<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field)?.as_str().filter(|id| {
        !id.is_empty()
            && id.len() <= 200
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    })
}

fn tool_key(value: &Value) -> Option<[u8; 32]> {
    let name = value.get("tool_name")?.as_str()?;
    if name.is_empty() || name.len() > 200 {
        return None;
    }
    let input = value.get("tool_input")?;
    // Approval hooks can add a description to shell/edit arguments.
    let input = if matches!(name, "Bash" | "apply_patch") {
        input.get("command")?
    } else {
        input
    };
    let mut hash = Sha256::new();
    hash.update(name.as_bytes());
    hash.update([0]);
    hash.update(serde_json::to_vec(input).ok()?);
    Some(hash.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observe(tracking: &mut InputTracking, kind: &str, id: &str, command: &str) -> InputChange {
        let event = match kind {
            "PreToolUse" => NormalizedProviderEvent::ToolStarted,
            "PermissionRequest" => NormalizedProviderEvent::PermissionRequested,
            "PostToolUse" => NormalizedProviderEvent::ToolCompleted,
            _ => unreachable!(),
        };
        tracking.observe(&serde_json::to_vec(&serde_json::json!({"hook_event_name": kind, "session_id": "conversation", "turn_id": "turn", "tool_use_id": id, "tool_name": "Bash", "tool_input": {"command": command}})).unwrap(), &event)
    }

    #[test]
    fn completion_resolves_only_the_matching_last_request() {
        let mut tracking = InputTracking::default();
        observe(&mut tracking, "PreToolUse", "a", "first");
        assert_eq!(
            observe(&mut tracking, "PermissionRequest", "", "first"),
            InputChange::NewRequest
        );
        observe(&mut tracking, "PreToolUse", "b", "second");
        assert_eq!(
            observe(&mut tracking, "PermissionRequest", "", "second"),
            InputChange::NewRequest
        );
        assert_eq!(
            observe(&mut tracking, "PostToolUse", "a", "first"),
            InputChange::None
        );
        assert_eq!(
            observe(&mut tracking, "PostToolUse", "b", "second"),
            InputChange::Resolved
        );
        assert_eq!(
            observe(&mut tracking, "PostToolUse", "b", "second"),
            InputChange::Ignore
        );
    }

    #[test]
    fn repeated_commands_can_request_again_but_ambiguous_requests_never_auto_clear() {
        let mut tracking = InputTracking::default();
        observe(&mut tracking, "PreToolUse", "a", "same");
        assert_eq!(
            observe(&mut tracking, "PermissionRequest", "", "same"),
            InputChange::NewRequest
        );
        assert_eq!(
            observe(&mut tracking, "PermissionRequest", "", "same"),
            InputChange::Ignore
        );
        assert_eq!(
            observe(&mut tracking, "PostToolUse", "a", "same"),
            InputChange::Resolved
        );
        observe(&mut tracking, "PreToolUse", "b", "same");
        assert_eq!(
            observe(&mut tracking, "PermissionRequest", "", "same"),
            InputChange::NewRequest
        );
        observe(&mut tracking, "PreToolUse", "c", "same");
        assert_eq!(
            observe(&mut tracking, "PermissionRequest", "", "same"),
            InputChange::None
        );
        observe(&mut tracking, "PostToolUse", "c", "same");
        assert_eq!(
            observe(&mut tracking, "PostToolUse", "b", "same"),
            InputChange::None
        );
    }

    #[test]
    fn missing_correlation_and_capacity_exhaustion_fail_closed() {
        let mut tracking = InputTracking::default();
        assert_eq!(
            observe(&mut tracking, "PermissionRequest", "", "unknown"),
            InputChange::None
        );
        observe(&mut tracking, "PreToolUse", "a", "known");
        observe(&mut tracking, "PermissionRequest", "", "known");
        assert_eq!(
            observe(&mut tracking, "PostToolUse", "a", "known"),
            InputChange::None
        );
        let mut tracking = InputTracking::default();
        for index in 0..=CAPACITY {
            observe(
                &mut tracking,
                "PreToolUse",
                &format!("call-{index}"),
                &format!("command-{index}"),
            );
        }
        assert_eq!(tracking.calls.len(), CAPACITY);
        observe(&mut tracking, "PermissionRequest", "", "command-0");
        assert_eq!(
            observe(&mut tracking, "PostToolUse", "call-0", "command-0"),
            InputChange::None
        );
    }

    #[test]
    fn callbacks_from_an_older_turn_cannot_clear_a_new_request() {
        let mut tracking = InputTracking::default();
        observe(&mut tracking, "PreToolUse", "a", "command");
        observe(&mut tracking, "PermissionRequest", "", "command");
        tracking.turn = Some("new-turn".into());
        assert_eq!(
            observe(&mut tracking, "PostToolUse", "a", "command"),
            InputChange::Ignore
        );
    }

    #[test]
    fn a_permission_callback_after_its_tool_result_cannot_resurrect_attention() {
        let mut tracking = InputTracking::default();
        observe(&mut tracking, "PreToolUse", "a", "command");
        observe(&mut tracking, "PostToolUse", "a", "command");
        assert_eq!(
            observe(&mut tracking, "PermissionRequest", "", "command"),
            InputChange::Ignore
        );
    }
}
