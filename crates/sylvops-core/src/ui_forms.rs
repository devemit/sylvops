//! Shared provider-aware form defaults and validation for replaceable clients.

use crate::{
    domain::ProviderKind,
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    protocol::ClientRequest,
    provider::ProviderHealth,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum WorktreeBaseMode {
    #[default]
    LatestDefaultBranch,
    CurrentCheckout,
    CustomRef,
}

impl WorktreeBaseMode {
    /// Builds an explicit selection for this mode.
    ///
    /// # Errors
    /// Returns an error for an empty, oversized, option-like, or control-containing custom ref.
    pub fn selection(
        self,
        reference: &str,
    ) -> Result<crate::domain::WorktreeBaseSelection, String> {
        use crate::domain::WorktreeBaseSelection;
        match self {
            Self::LatestDefaultBranch => Ok(WorktreeBaseSelection::default()),
            Self::CurrentCheckout => Ok("HEAD".into()),
            Self::CustomRef => {
                let reference = reference.trim();
                if reference.is_empty()
                    || reference.len() > 1024
                    || reference.starts_with('-')
                    || reference.chars().any(char::is_control)
                {
                    return Err("Enter a valid custom local ref.".into());
                }
                Ok(reference.into())
            }
        }
    }

    pub fn from_selection(selection: &crate::domain::WorktreeBaseSelection) -> Self {
        match selection {
            crate::domain::WorktreeBaseSelection::LatestDefaultBranch { .. } => {
                Self::LatestDefaultBranch
            }
            crate::domain::WorktreeBaseSelection::LocalRef { reference } if reference == "HEAD" => {
                Self::CurrentCheckout
            }
            crate::domain::WorktreeBaseSelection::LocalRef { .. } => Self::CustomRef,
        }
    }
    pub const ALL: [Self; 3] = [
        Self::LatestDefaultBranch,
        Self::CurrentCheckout,
        Self::CustomRef,
    ];
    pub const fn label(self) -> &'static str {
        match self {
            Self::LatestDefaultBranch => "Latest default branch",
            Self::CurrentCheckout => "Current checkout",
            Self::CustomRef => "Custom local ref",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormKind {
    CreateWorkspace,
    RegisterProject(WorkspaceId),
    CreateWorktree(ProjectId),
    CreateSession(WorktreeId),
    RenameProject(ProjectId),
    RenameWorktree(WorktreeId),
    RenameSession(SessionId),
}

#[derive(Clone, Debug)]
pub struct Field {
    pub label: &'static str,
    pub value: String,
    pub error: Option<String>,
    pub visible: bool,
}

#[derive(Clone, Debug)]
pub struct Form {
    pub title: String,
    pub kind: FormKind,
    pub fields: Vec<Field>,
    pub active: usize,
    pub providers: Vec<ProviderHealth>,
    pub provider_index: usize,
    pub submission_error: Option<String>,
    pub worktree_base_mode: WorktreeBaseMode,
    pub pending: bool,
}

impl Form {
    pub fn workspace() -> Self {
        Self::new(
            "Create workspace",
            FormKind::CreateWorkspace,
            vec![field("Workspace name", "Local")],
        )
    }

    pub fn repository(workspace_id: WorkspaceId) -> Self {
        Self::new(
            "Register Git repository",
            FormKind::RegisterProject(workspace_id),
            vec![field("Repository path", ".")],
        )
    }

    pub fn worktree(project_id: ProjectId) -> Self {
        let mut form = Self::new(
            "Create isolated checkout",
            FormKind::CreateWorktree(project_id),
            vec![
                field("Branch", ""),
                field("Display name (optional)", ""),
                field("Custom local ref", "HEAD"),
                field("Remote (optional: origin or the only remote)", ""),
            ],
        );
        form.select_worktree_base(WorktreeBaseMode::LatestDefaultBranch);
        form
    }

    pub fn session(worktree_id: WorktreeId, providers: Vec<ProviderHealth>) -> Self {
        let providers: Vec<_> = providers
            .into_iter()
            .filter(|provider| provider.capabilities.interactive)
            .collect();
        let provider_index = providers
            .iter()
            .position(|provider| provider.kind == ProviderKind::Shell)
            .unwrap_or(0);
        let mut form = Self::new(
            "Start a session",
            FormKind::CreateSession(worktree_id),
            vec![field("Display name (optional)", "")],
        );
        form.providers = providers;
        form.provider_index = provider_index;
        form
    }

    pub fn rename(title: &str, value: &str, kind: FormKind) -> Self {
        Self::new(title, kind, vec![field("Display name", value)])
    }

    fn new(title: &str, kind: FormKind, fields: Vec<Field>) -> Self {
        Self {
            title: title.into(),
            kind,
            fields,
            active: 0,
            providers: Vec::new(),
            provider_index: 0,
            submission_error: None,
            worktree_base_mode: WorktreeBaseMode::default(),
            pending: false,
        }
    }

    pub fn is_session(&self) -> bool {
        matches!(self.kind, FormKind::CreateSession(_))
    }

    pub fn is_worktree(&self) -> bool {
        matches!(self.kind, FormKind::CreateWorktree(_))
    }

    pub fn select_worktree_base(&mut self, mode: WorktreeBaseMode) {
        if !self.is_worktree() {
            return;
        }
        self.worktree_base_mode = mode;
        self.fields[2].visible = mode == WorktreeBaseMode::CustomRef;
        self.fields[3].visible = mode == WorktreeBaseMode::LatestDefaultBranch;
        self.active = self
            .active
            .min(self.visible_field_indices().len().saturating_sub(1));
        self.clear_errors();
    }

    pub fn next_worktree_base(&mut self, delta: isize) {
        let index = WorktreeBaseMode::ALL
            .iter()
            .position(|mode| *mode == self.worktree_base_mode)
            .unwrap_or(0);
        self.select_worktree_base(WorktreeBaseMode::ALL[index.saturating_add_signed(delta).min(2)]);
    }

    /// Returns the selected source for a worktree form.
    ///
    /// # Errors
    /// Returns an error if the form or selected remote/ref is invalid.
    pub fn worktree_selection(&self) -> Result<crate::domain::WorktreeBaseSelection, String> {
        use crate::domain::WorktreeBaseSelection;
        if !self.is_worktree() || self.fields.len() < 4 {
            return Err("Not a Worktree form.".into());
        }
        match self.worktree_base_mode {
            WorktreeBaseMode::LatestDefaultBranch => {
                let remote = self.fields[3].value.trim();
                Ok(WorktreeBaseSelection::LatestDefaultBranch {
                    remote: if remote.is_empty() {
                        None
                    } else {
                        Some(crate::github::GitRemoteName::new(remote)?)
                    },
                })
            }
            WorktreeBaseMode::CurrentCheckout => Ok(WorktreeBaseSelection::LocalRef {
                reference: "HEAD".into(),
            }),
            WorktreeBaseMode::CustomRef => {
                let reference = self.fields[2].value.trim();
                if reference.is_empty()
                    || reference.len() > 1024
                    || reference.starts_with('-')
                    || reference.chars().any(char::is_control)
                {
                    return Err("Enter a valid local base ref.".into());
                }
                Ok(WorktreeBaseSelection::LocalRef {
                    reference: reference.into(),
                })
            }
        }
    }

    /// Builds the daemon request for a worktree form.
    ///
    /// # Errors
    /// Returns an error if the form or selected remote/ref is invalid.
    pub fn worktree_request(&self) -> Result<ClientRequest, String> {
        let FormKind::CreateWorktree(project_id) = self.kind else {
            return Err("Not a Worktree form.".into());
        };
        let name = self.fields[1].value.trim();
        Ok(ClientRequest::CreateWorktree {
            project_id,
            branch: self.fields[0].value.trim().into(),
            name: (!name.is_empty()).then(|| name.to_owned()),
            base_ref: Some(self.worktree_selection()?),
        })
    }

    pub fn repository_path(&self) -> Option<&str> {
        if !matches!(self.kind, FormKind::RegisterProject(_)) {
            return None;
        }
        self.fields.first().map(|field| field.value.as_str())
    }

    pub fn set_repository_path(&mut self, path: String) -> bool {
        if !matches!(self.kind, FormKind::RegisterProject(_)) {
            return false;
        }
        let Some(field) = self.fields.first_mut() else {
            return false;
        };
        field.value = path;
        self.clear_errors();
        true
    }

    pub fn provider(&self) -> Option<&ProviderHealth> {
        self.providers.get(self.provider_index)
    }

    pub fn select_next_provider(&mut self, delta: isize) {
        if self.providers.is_empty() {
            return;
        }
        self.provider_index = if delta < 0 {
            self.provider_index.saturating_sub(delta.unsigned_abs())
        } else {
            (self.provider_index + delta.unsigned_abs()).min(self.providers.len() - 1)
        };
    }

    pub fn select_provider(&mut self, kind: ProviderKind) -> bool {
        let Some(index) = self
            .providers
            .iter()
            .position(|provider| provider.kind == kind)
        else {
            return false;
        };
        self.provider_index = index;
        true
    }

    #[must_use]
    pub fn session_request(&self, columns: u16, rows: u16) -> Option<ClientRequest> {
        let FormKind::CreateSession(worktree_id) = self.kind else {
            return None;
        };
        let provider = self.provider()?.kind;
        let display_name = self.fields.first()?.value.trim();
        Some(ClientRequest::CreateSession {
            worktree_id,
            provider,
            display_name: (!display_name.is_empty()).then(|| display_name.to_owned()),
            model: None,
            effort: None,
            initial_prompt: None,
            columns,
            rows,
        })
    }

    pub fn visible_field_indices(&self) -> Vec<usize> {
        self.fields
            .iter()
            .enumerate()
            .filter_map(|(index, field)| field.visible.then_some(index))
            .collect()
    }

    pub fn active_field_index(&self) -> Option<usize> {
        self.visible_field_indices().get(self.active).copied()
    }

    pub fn next_field(&mut self, delta: isize) {
        let count = self.visible_field_indices().len();
        if count == 0 {
            return;
        }
        self.active = if delta < 0 {
            self.active.saturating_sub(delta.unsigned_abs())
        } else {
            (self.active + delta.unsigned_abs()).min(count - 1)
        };
    }

    pub fn clear_errors(&mut self) {
        self.submission_error = None;
        for field in &mut self.fields {
            field.error = None;
        }
    }

    pub fn validate(&mut self) -> bool {
        self.clear_errors();
        let required = match self.kind {
            FormKind::CreateWorkspace
            | FormKind::RegisterProject(_)
            | FormKind::CreateWorktree(_)
            | FormKind::RenameProject(_)
            | FormKind::RenameWorktree(_)
            | FormKind::RenameSession(_) => Some(0),
            FormKind::CreateSession(_) => None,
        };
        if let Some(index) = required
            && self.fields[index].value.trim().is_empty()
        {
            self.fields[index].error = Some("This field is required.".into());
            self.active = 0;
            return false;
        }
        if self.is_session() {
            let Some(provider) = self.provider() else {
                self.submission_error = Some("No providers were returned by the daemon.".into());
                return false;
            };
            if let Some(error) = provider.session_start_error() {
                self.submission_error = Some(error);
                return false;
            }
        }
        if self.is_worktree()
            && let Err(error) = self.worktree_selection()
        {
            self.submission_error = Some(error);
            return false;
        }
        true
    }
}

fn field(label: &'static str, value: &str) -> Field {
    Field {
        label,
        value: value.into(),
        error: None,
        visible: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::ClientRequest,
        provider::{AuthenticationRequirement, ProviderCapabilities},
    };

    fn health(kind: ProviderKind, available: bool) -> ProviderHealth {
        ProviderHealth {
            kind,
            available,
            authenticated: available,
            executable_path: None,
            version: None,
            diagnostic: (!available).then(|| "not found".into()),
            capabilities: ProviderCapabilities {
                interactive: true,
                ..ProviderCapabilities::default()
            },
            checked_at: 0,
        }
    }

    #[test]
    fn shell_is_the_session_default_and_session_name_is_the_only_field() {
        let form = Form::session(
            WorktreeId::new(),
            vec![
                health(ProviderKind::Codex, true),
                health(ProviderKind::Shell, true),
            ],
        );
        assert_eq!(
            form.provider().map(|item| item.kind),
            Some(ProviderKind::Shell)
        );
        assert_eq!(form.visible_field_indices(), vec![0]);
    }

    #[test]
    fn failed_validation_keeps_provider_diagnostic() {
        let mut form = Form::session(WorktreeId::new(), vec![health(ProviderKind::Shell, false)]);
        assert!(!form.validate());
        assert_eq!(form.submission_error.as_deref(), Some("not found"));
    }

    #[test]
    fn provider_can_be_selected_without_revealing_advanced_fields() {
        let mut form = Form::session(
            WorktreeId::new(),
            vec![
                health(ProviderKind::Codex, true),
                health(ProviderKind::Shell, true),
            ],
        );

        assert!(form.select_provider(ProviderKind::Codex));
        assert_eq!(
            form.provider().map(|provider| provider.kind),
            Some(ProviderKind::Codex)
        );
        assert_eq!(form.visible_field_indices(), vec![0]);
        assert_eq!(form.fields.len(), 1);
    }

    #[test]
    fn authentication_validation_uses_provider_health_instead_of_provider_names() {
        let mut claude = health(ProviderKind::Claude, true);
        claude.authenticated = false;
        claude.capabilities.authentication = AuthenticationRequirement::ExistingLogin;
        claude.diagnostic =
            Some("Claude Code is not logged in. Run `claude auth login` yourself.".into());
        let mut form = Form::session(WorktreeId::new(), vec![claude]);

        assert!(!form.validate());
        assert_eq!(
            form.submission_error.as_deref(),
            Some("Claude Code is not logged in. Run `claude auth login` yourself.")
        );
    }

    #[test]
    fn session_form_only_offers_interactive_providers() {
        let mut observational = health(ProviderKind::Pi, true);
        observational.capabilities.interactive = false;
        let form = Form::session(
            WorktreeId::new(),
            vec![
                observational,
                health(ProviderKind::Claude, true),
                health(ProviderKind::Shell, true),
            ],
        );

        assert_eq!(
            form.providers
                .iter()
                .map(|provider| provider.kind)
                .collect::<Vec<_>>(),
            vec![ProviderKind::Claude, ProviderKind::Shell]
        );
        assert_eq!(
            form.provider().map(|provider| provider.kind),
            Some(ProviderKind::Shell)
        );
    }

    #[test]
    fn interactive_claude_submission_builds_a_bounded_ipc_request() {
        let worktree_id = WorktreeId::new();
        let mut form = Form::session(
            worktree_id,
            vec![
                health(ProviderKind::Shell, true),
                health(ProviderKind::Claude, true),
            ],
        );
        assert!(form.select_provider(ProviderKind::Claude));
        form.fields[0].value = "Claude review".into();

        assert_eq!(
            form.session_request(120, 40),
            Some(ClientRequest::CreateSession {
                worktree_id,
                provider: ProviderKind::Claude,
                display_name: Some("Claude review".into()),
                model: None,
                effort: None,
                initial_prompt: None,
                columns: 120,
                rows: 40,
            })
        );
    }
}
