//! Provider-neutral launch, health, resume, and hook contracts.

use std::{collections::BTreeMap, ffi::OsString, fmt, net::IpAddr, path::PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    CoreError, Result,
    domain::ProviderKind,
    ids::{SessionId, WorktreeId},
    status::{NormalizedProviderEvent, ProviderConversationId},
};

pub const MAX_PROVIDER_LIFECYCLE_PAYLOAD_SIZE: usize = 64 * 1024;
const MAX_PROVIDER_LIFECYCLE_URL_LENGTH: usize = 2 * 1024;
const MIN_PROVIDER_LIFECYCLE_TOKEN_LENGTH: usize = 32;
const MAX_PROVIDER_LIFECYCLE_TOKEN_LENGTH: usize = 512;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthenticationRequirement {
    #[default]
    None,
    ExistingLogin,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ProviderRuntimeCapabilities {
    pub lifecycle_events: bool,
    pub recoverable_turn_failures: bool,
    pub remaining_background_work: bool,
    pub conversation_identity_transitions: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ProviderCapabilities {
    pub interactive: bool,
    pub resume: bool,
    pub status_hooks: bool,
    pub model_selection: bool,
    pub effort_selection: bool,
    pub authentication: AuthenticationRequirement,
    pub runtime: Option<ProviderRuntimeCapabilities>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub kind: ProviderKind,
    pub available: bool,
    pub authenticated: bool,
    pub executable_path: Option<String>,
    pub version: Option<String>,
    pub diagnostic: Option<String>,
    pub capabilities: ProviderCapabilities,
    pub checked_at: i64,
}

#[derive(Clone)]
pub struct ProviderLifecycleEndpoint {
    provider: ProviderKind,
    session_id: SessionId,
    worktree_id: WorktreeId,
    url: String,
    bearer_token: String,
}

impl ProviderLifecycleEndpoint {
    /// Creates a memory-only lifecycle endpoint bound to one Provider, Session, and Worktree.
    ///
    /// # Errors
    ///
    /// Returns an error for an unbounded credential or a non-loopback HTTP endpoint.
    pub fn new(
        provider: ProviderKind,
        session_id: SessionId,
        worktree_id: WorktreeId,
        url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self> {
        let url = url.into();
        if url.is_empty() || url.len() > MAX_PROVIDER_LIFECYCLE_URL_LENGTH {
            return Err(provider_error("provider lifecycle endpoint URL is invalid"));
        }
        let parsed = url::Url::parse(&url)
            .map_err(|_| provider_error("provider lifecycle endpoint URL is invalid"))?;
        let loopback = parsed
            .host_str()
            .and_then(|host| host.parse::<IpAddr>().ok())
            .is_some_and(|address| address.is_loopback());
        if parsed.scheme() != "http"
            || !loopback
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(provider_error(
                "provider lifecycle endpoint must be an uncredentialed loopback HTTP URL",
            ));
        }

        let bearer_token = bearer_token.into();
        if bearer_token.len() < MIN_PROVIDER_LIFECYCLE_TOKEN_LENGTH
            || bearer_token.len() > MAX_PROVIDER_LIFECYCLE_TOKEN_LENGTH
            || bearer_token
                .chars()
                .any(|character| !character.is_ascii_graphic())
        {
            return Err(provider_error(
                "provider lifecycle endpoint credential is invalid",
            ));
        }

        Ok(Self {
            provider,
            session_id,
            worktree_id,
            url,
            bearer_token,
        })
    }

    #[must_use]
    pub const fn provider(&self) -> ProviderKind {
        self.provider
    }

    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    #[must_use]
    pub const fn worktree_id(&self) -> WorktreeId {
        self.worktree_id
    }

    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    #[must_use]
    pub fn bearer_token(&self) -> &str {
        &self.bearer_token
    }

    #[must_use]
    pub fn is_bound_to(
        &self,
        provider: ProviderKind,
        session_id: SessionId,
        worktree_id: WorktreeId,
    ) -> bool {
        self.provider == provider
            && self.session_id == session_id
            && self.worktree_id == worktree_id
    }
}

impl fmt::Debug for ProviderLifecycleEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderLifecycleEndpoint")
            .field("provider", &self.provider)
            .field("session_id", &self.session_id)
            .field("worktree_id", &self.worktree_id)
            .field("url", &self.url)
            .field("bearer_token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ProviderLifecyclePayload(Vec<u8>);

impl ProviderLifecyclePayload {
    /// Copies a provider payload only after enforcing the shared runtime limit.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload exceeds [`MAX_PROVIDER_LIFECYCLE_PAYLOAD_SIZE`].
    pub fn new(bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() > MAX_PROVIDER_LIFECYCLE_PAYLOAD_SIZE {
            return Err(provider_error("provider lifecycle payload is too large"));
        }
        Ok(Self(bytes))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for ProviderLifecyclePayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderLifecyclePayload")
            .field("byte_count", &self.0.len())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct LaunchContext {
    pub session_id: SessionId,
    pub worktree_id: WorktreeId,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub initial_prompt: Option<String>,
    pub lifecycle_endpoint: Option<ProviderLifecycleEndpoint>,
}

#[derive(Clone, Debug)]
pub struct ResumeContext {
    pub session_id: SessionId,
    pub worktree_id: WorktreeId,
    pub external_session_id: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub lifecycle_endpoint: Option<ProviderLifecycleEndpoint>,
}

#[derive(Clone)]
pub struct LaunchSpec {
    pub executable: PathBuf,
    pub arguments: LaunchArguments,
    pub environment: BTreeMap<OsString, OsString>,
}

#[derive(Clone, Debug)]
pub struct ProviderRuntimeSpec {
    pub launch: LaunchSpec,
    /// Application-owned runtime-overlay paths that the daemon must clean up after revalidation.
    pub owned_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProviderLifecycleEvent {
    pub event: Option<NormalizedProviderEvent>,
    pub conversation_id: Option<ProviderConversationId>,
    pub turn_id: Option<String>,
}

#[derive(Clone, Default)]
pub struct LaunchArguments {
    values: Vec<OsString>,
    persisted_len: usize,
}

impl fmt::Debug for LaunchArguments {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchArguments")
            .field("argument_count", &self.values.len())
            .field("persisted_argument_count", &self.persisted_len)
            .finish()
    }
}

impl LaunchArguments {
    #[must_use]
    pub fn persisted(values: Vec<OsString>) -> Self {
        Self {
            persisted_len: values.len(),
            values,
        }
    }

    #[must_use]
    pub fn with_transient_tail(mut persisted: Vec<OsString>, transient: Vec<OsString>) -> Self {
        let persisted_len = persisted.len();
        persisted.extend(transient);
        Self {
            values: persisted,
            persisted_len,
        }
    }

    pub fn prepend_persisted(&mut self, values: impl IntoIterator<Item = OsString>) {
        let values: Vec<_> = values.into_iter().collect();
        self.persisted_len = self.persisted_len.saturating_add(values.len());
        self.values.splice(0..0, values);
    }

    #[must_use]
    pub fn all(&self) -> &[OsString] {
        &self.values
    }

    #[must_use]
    pub fn persisted_values(&self) -> &[OsString] {
        &self.values[..self.persisted_len]
    }

    #[must_use]
    pub fn into_all(self) -> Vec<OsString> {
        self.values
    }
}

impl fmt::Debug for LaunchSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchSpec")
            .field("executable", &self.executable)
            .field("argument_count", &self.arguments.all().len())
            .field(
                "persisted_argument_count",
                &self.arguments.persisted_values().len(),
            )
            .field("environment_keys", &self.environment.keys())
            .finish()
    }
}

/// Provider-owned runtime behavior used by the authoritative daemon.
///
/// Shared daemon code supplies trusted Session and Worktree context, while each implementation
/// owns its structured launch/resume configuration and parsing of bounded provider hook payloads.
#[async_trait]
pub trait ProviderRuntime: Send + Sync + std::fmt::Debug {
    fn kind(&self) -> ProviderKind;
    fn capabilities(&self) -> ProviderCapabilities;
    async fn probe(&self) -> ProviderHealth;
    async fn refresh(&self) -> ProviderHealth {
        self.probe().await
    }

    /// Builds the executable, structured argument vector, and reviewed environment for a new
    /// Session. Transient input must remain outside [`LaunchArguments::persisted_values`].
    ///
    /// # Errors
    ///
    /// Returns an error when the provider is unavailable or the requested options are invalid.
    fn configure_launch(&self, context: LaunchContext) -> Result<ProviderRuntimeSpec>;

    /// Builds runtime configuration for resuming a verified provider conversation.
    ///
    /// # Errors
    ///
    /// Returns an error when the conversation identity or requested options are invalid.
    fn configure_resume(&self, context: ResumeContext) -> Result<Option<ProviderRuntimeSpec>>;

    /// Removes application-owned runtime overlays after revalidating their identity.
    ///
    /// # Errors
    ///
    /// Returns an error when cleanup detects an unsafe or inconsistent owned path.
    fn cleanup_runtime_paths(&self, _paths: &[PathBuf]) -> Result<()> {
        Ok(())
    }

    /// Converts one bounded provider-owned hook payload into provider-neutral lifecycle data and
    /// bounded identity metadata. Unknown and malformed payloads fail closed with an error.
    ///
    /// # Errors
    ///
    /// Returns an error when a recognized payload is malformed or contains invalid bounded data.
    fn normalize_lifecycle_event(
        &self,
        payload: &ProviderLifecyclePayload,
    ) -> Result<ProviderLifecycleEvent>;
}

pub fn provider_error(message: impl Into<String>) -> CoreError {
    CoreError::Provider(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{NormalizedProviderEvent, RemainingWork, TurnFailureCategory};

    #[derive(Debug)]
    struct FakeRuntime;

    #[async_trait]
    impl ProviderRuntime for FakeRuntime {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Claude
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                interactive: true,
                resume: true,
                status_hooks: true,
                model_selection: true,
                effort_selection: true,
                authentication: AuthenticationRequirement::ExistingLogin,
                runtime: Some(ProviderRuntimeCapabilities {
                    lifecycle_events: true,
                    recoverable_turn_failures: true,
                    remaining_background_work: true,
                    conversation_identity_transitions: true,
                }),
            }
        }

        async fn probe(&self) -> ProviderHealth {
            ProviderHealth {
                kind: self.kind(),
                available: true,
                authenticated: true,
                executable_path: Some("fake-provider".into()),
                version: Some("1.0.0".into()),
                diagnostic: None,
                capabilities: self.capabilities(),
                checked_at: 1,
            }
        }

        fn configure_launch(&self, context: LaunchContext) -> Result<ProviderRuntimeSpec> {
            let endpoint = context
                .lifecycle_endpoint
                .as_ref()
                .expect("hook-capable runtime receives an endpoint");
            assert!(endpoint.is_bound_to(self.kind(), context.session_id, context.worktree_id));
            let persisted = context
                .model
                .map(|model| vec![OsString::from("--model"), OsString::from(model)])
                .unwrap_or_default();
            let transient = context
                .initial_prompt
                .map(|prompt| vec![OsString::from(prompt)])
                .unwrap_or_default();
            Ok(ProviderRuntimeSpec {
                launch: LaunchSpec {
                    executable: PathBuf::from("fake-provider"),
                    arguments: LaunchArguments::with_transient_tail(persisted, transient),
                    environment: BTreeMap::new(),
                },
                owned_paths: vec![PathBuf::from("settings/runtime.json")],
            })
        }

        fn configure_resume(&self, context: ResumeContext) -> Result<Option<ProviderRuntimeSpec>> {
            Ok(Some(ProviderRuntimeSpec {
                launch: LaunchSpec {
                    executable: PathBuf::from("fake-provider"),
                    arguments: LaunchArguments::persisted(vec![
                        OsString::from("--resume"),
                        OsString::from(context.external_session_id),
                    ]),
                    environment: BTreeMap::new(),
                },
                owned_paths: vec![PathBuf::from("settings/runtime.json")],
            }))
        }

        fn normalize_lifecycle_event(
            &self,
            payload: &ProviderLifecyclePayload,
        ) -> Result<ProviderLifecycleEvent> {
            let event = match payload.as_bytes() {
                b"background" => Some(NormalizedProviderEvent::TurnStopped {
                    remaining_work: RemainingWork {
                        background_tasks: true,
                        ..RemainingWork::default()
                    },
                }),
                b"failure" => Some(NormalizedProviderEvent::TurnFailed {
                    category: TurnFailureCategory::new("provider_unavailable")
                        .expect("valid category"),
                }),
                _ => return Err(provider_error("unknown provider lifecycle event")),
            };
            Ok(ProviderLifecycleEvent {
                event,
                ..ProviderLifecycleEvent::default()
            })
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn provider_runtime_owns_configuration_and_lifecycle_normalization() {
        let runtime: Box<dyn ProviderRuntime> = Box::new(FakeRuntime);
        let health = runtime.probe().await;
        assert_eq!(health.kind, ProviderKind::Claude);
        assert_eq!(
            health.capabilities.authentication,
            AuthenticationRequirement::ExistingLogin
        );

        let prompt = "transient user prompt";
        let session_id = SessionId::new();
        let worktree_id = WorktreeId::new();
        let token = "0123456789abcdef0123456789abcdef";
        let endpoint = ProviderLifecycleEndpoint::new(
            ProviderKind::Claude,
            session_id,
            worktree_id,
            "http://127.0.0.1:4312/provider-events",
            token,
        )
        .unwrap();
        assert!(!format!("{endpoint:?}").contains(token));
        assert!(!endpoint.is_bound_to(ProviderKind::Codex, session_id, worktree_id));
        assert!(
            ProviderLifecycleEndpoint::new(
                ProviderKind::Claude,
                session_id,
                worktree_id,
                "https://example.com/provider-events",
                token,
            )
            .is_err()
        );
        let launch = runtime
            .configure_launch(LaunchContext {
                session_id,
                worktree_id,
                cwd: PathBuf::from("worktree"),
                model: Some("model-a".into()),
                effort: None,
                initial_prompt: Some(prompt.into()),
                lifecycle_endpoint: Some(endpoint.clone()),
            })
            .unwrap();
        assert!(
            launch
                .launch
                .arguments
                .all()
                .iter()
                .any(|argument| argument == prompt)
        );
        assert!(
            launch
                .launch
                .arguments
                .persisted_values()
                .iter()
                .all(|argument| argument != prompt)
        );
        assert_eq!(launch.owned_paths, [PathBuf::from("settings/runtime.json")]);

        let resume = runtime
            .configure_resume(ResumeContext {
                session_id,
                worktree_id,
                external_session_id: "conversation-123".into(),
                cwd: PathBuf::from("worktree"),
                model: None,
                effort: None,
                lifecycle_endpoint: Some(endpoint),
            })
            .unwrap()
            .expect("runtime supports resume");
        assert_eq!(
            resume.launch.arguments.persisted_values(),
            [
                OsString::from("--resume"),
                OsString::from("conversation-123")
            ]
        );

        let background_payload = ProviderLifecyclePayload::new(b"background".to_vec()).unwrap();
        assert!(!format!("{background_payload:?}").contains("background"));
        assert!(matches!(
            runtime
                .normalize_lifecycle_event(&background_payload)
                .unwrap()
                .event,
            Some(NormalizedProviderEvent::TurnStopped {
                remaining_work: RemainingWork {
                    background_tasks: true,
                    ..
                }
            })
        ));
        let failure_payload = ProviderLifecyclePayload::new(b"failure".to_vec()).unwrap();
        assert!(matches!(
            runtime
                .normalize_lifecycle_event(&failure_payload)
                .unwrap()
                .event,
            Some(NormalizedProviderEvent::TurnFailed { .. })
        ));
        assert!(
            ProviderLifecyclePayload::new(vec![0; MAX_PROVIDER_LIFECYCLE_PAYLOAD_SIZE + 1])
                .is_err()
        );
    }
}
