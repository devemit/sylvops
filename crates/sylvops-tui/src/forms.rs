use sylvops_core::{
    domain::ProviderKind,
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    provider::ProviderHealth,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FormKind {
    CreateWorkspace,
    RegisterProject(WorkspaceId),
    CreateWorktree(ProjectId),
    CreateSession(WorktreeId),
    RenameProject(ProjectId),
    RenameWorktree(WorktreeId),
    RenameSession(SessionId),
}

#[derive(Clone, Debug)]
pub(crate) struct Field {
    pub label: &'static str,
    pub value: String,
    pub error: Option<String>,
    pub visible: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Form {
    pub title: String,
    pub kind: FormKind,
    pub fields: Vec<Field>,
    pub active: usize,
    pub providers: Vec<ProviderHealth>,
    pub provider_index: usize,
    pub submission_error: Option<String>,
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
        Self::new(
            "Create managed worktree",
            FormKind::CreateWorktree(project_id),
            vec![
                field("Branch", ""),
                field("Display name (optional)", ""),
                field("Base ref (optional, defaults to HEAD)", "HEAD"),
            ],
        )
    }

    pub fn session(worktree_id: WorktreeId, providers: Vec<ProviderHealth>) -> Self {
        let provider_index = providers
            .iter()
            .position(|provider| provider.kind == ProviderKind::Shell)
            .unwrap_or(0);
        let mut form = Self::new(
            "Create terminal session",
            FormKind::CreateSession(worktree_id),
            vec![
                field("Display name (optional)", ""),
                field("Model (optional)", ""),
                field("Effort (optional)", ""),
                field("Initial prompt (optional)", ""),
            ],
        );
        form.providers = providers;
        form.provider_index = provider_index;
        form.update_conditional_fields();
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
        }
    }

    pub fn is_session(&self) -> bool {
        matches!(self.kind, FormKind::CreateSession(_))
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
        self.update_conditional_fields();
    }

    pub fn update_conditional_fields(&mut self) {
        if !self.is_session() {
            return;
        }
        let codex = self
            .provider()
            .is_some_and(|provider| provider.kind == ProviderKind::Codex);
        for index in 1..self.fields.len() {
            self.fields[index].visible = codex;
        }
        self.active = self
            .active
            .min(self.visible_field_indices().len().saturating_sub(1));
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
            if !provider.available {
                self.submission_error = Some(
                    provider
                        .diagnostic
                        .clone()
                        .unwrap_or_else(|| "Selected provider is unavailable.".into()),
                );
                return false;
            }
            if provider.kind == ProviderKind::Codex && !provider.authenticated {
                self.submission_error =
                    Some("Codex is not authenticated. Run `codex login` explicitly.".into());
                return false;
            }
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
    use sylvops_core::provider::ProviderCapabilities;

    fn health(kind: ProviderKind, available: bool) -> ProviderHealth {
        ProviderHealth {
            kind,
            available,
            authenticated: available,
            executable_path: None,
            version: None,
            diagnostic: (!available).then(|| "not found".into()),
            capabilities: ProviderCapabilities::default(),
            checked_at: 0,
        }
    }

    #[test]
    fn session_defaults_to_shell_and_hides_codex_fields() {
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
    fn unavailable_provider_retains_form_with_inline_error() {
        let mut form = Form::session(WorktreeId::new(), vec![health(ProviderKind::Shell, false)]);
        assert!(!form.validate());
        assert_eq!(form.submission_error.as_deref(), Some("not found"));
    }
}
