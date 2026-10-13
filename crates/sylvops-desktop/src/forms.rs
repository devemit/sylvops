use sylvops_core::{
    domain::{GitWorktreeState, ProviderKind},
    github::{
        MAX_START_WORK_BRANCH_LENGTH, MAX_START_WORK_INSTRUCTIONS_LENGTH, StartWorkBranch,
        StartWorkInstructions, StartWorkPreparation,
    },
    ids::ProjectId,
    protocol::{
        ClientRequest, ProjectRemovalInspection, SessionDeletionInspection,
        WorkspaceDeletionInspection,
    },
    ui_forms::{Form, FormKind},
    upgrade::ActiveUpgradeSession,
};
use sylvops_daemon::data_removal::DATA_REMOVAL_CONFIRMATION;

#[derive(Clone, Debug)]
pub(crate) struct FormModal {
    pub form: Form,
    pub pending: bool,
    pub provider_probe: Option<ProviderKind>,
    pub first_run: bool,
}

impl FormModal {
    pub(crate) fn new(form: Form) -> Self {
        Self {
            form,
            pending: false,
            provider_probe: None,
            first_run: false,
        }
    }

    pub(crate) fn first_run(form: Form) -> Self {
        Self {
            form,
            pending: false,
            provider_probe: None,
            first_run: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FirstRunStepState {
    Complete,
    Current,
    Upcoming,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FirstRunStep {
    pub title: &'static str,
    pub description: &'static str,
    pub state: FirstRunStepState,
}

pub(crate) fn first_run_steps(kind: FormKind) -> Option<[FirstRunStep; 3]> {
    let current: usize = match kind {
        FormKind::CreateWorkspace => 0,
        FormKind::RegisterProject(_) => 1,
        FormKind::CreateSession(_) => 2,
        FormKind::CreateWorktree(_)
        | FormKind::RenameProject(_)
        | FormKind::RenameWorktree(_)
        | FormKind::RenameSession(_) => return None,
    };
    let state = |index: usize| match index.cmp(&current) {
        std::cmp::Ordering::Less => FirstRunStepState::Complete,
        std::cmp::Ordering::Equal => FirstRunStepState::Current,
        std::cmp::Ordering::Greater => FirstRunStepState::Upcoming,
    };
    Some([
        FirstRunStep {
            title: "Create workspace",
            description: "Group the repositories you want to supervise together.",
            state: state(0),
        },
        FirstRunStep {
            title: "Add repository",
            description: "Choose an existing Git repository and register its root checkout.",
            state: state(1),
        },
        FirstRunStep {
            title: "Start session",
            description: "Start Shell, Codex, or Claude Code in the selected checkout.",
            state: state(2),
        },
    ])
}

#[derive(Clone, Debug)]
pub(crate) enum Confirmation {
    StopSession {
        session_name: String,
        cwd: String,
        delete_after_stop: bool,
    },
    RemoveWorktree {
        state: GitWorktreeState,
        name: String,
        canonical_path: String,
    },
    DeleteWorkspace {
        inspection: WorkspaceDeletionInspection,
        name: String,
    },
    RemoveProject {
        inspection: ProjectRemovalInspection,
        name: String,
        canonical_path: String,
    },
    DeleteSession {
        inspection: SessionDeletionInspection,
        name: String,
        cwd: String,
    },
    InstallUpdate {
        version: String,
        active_sessions: Vec<ActiveUpgradeSession>,
    },
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DataRemovalConfirmation {
    pub confirmation: String,
    pub pending: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct StartWorkModal {
    pub project_id: ProjectId,
    pub preparation: StartWorkPreparation,
    pub branch: String,
    pub provider: Option<ProviderKind>,
    pub instructions: String,
    pub start_another_worktree: bool,
    pub pending: bool,
    pub error: Option<String>,
}

impl StartWorkModal {
    pub(crate) fn new(project_id: ProjectId, preparation: StartWorkPreparation) -> Self {
        Self {
            branch: preparation.suggested_branch.as_str().to_owned(),
            provider: preparation.eligible_providers.first().copied(),
            start_another_worktree: false,
            project_id,
            preparation,
            instructions: String::new(),
            pending: false,
            error: None,
        }
    }

    pub(crate) fn update_branch(&mut self, branch: String) {
        if !self.pending && branch.len() <= MAX_START_WORK_BRANCH_LENGTH {
            self.branch = branch;
            self.error = None;
        }
    }

    pub(crate) fn update_instructions(&mut self, instructions: String) {
        if !self.pending && instructions.len() <= MAX_START_WORK_INSTRUCTIONS_LENGTH {
            self.instructions = instructions;
            self.error = None;
        }
    }

    pub(crate) fn select_provider(&mut self, provider: ProviderKind) {
        if !self.pending && self.preparation.eligible_providers.contains(&provider) {
            self.provider = Some(provider);
            self.error = None;
        }
    }

    pub(crate) fn request(&self) -> Result<ClientRequest, String> {
        if self.pending {
            return Err("Start Work is already in progress.".into());
        }
        let branch = StartWorkBranch::new(self.branch.clone())?;
        if self.preparation.suggested_branch_conflict && branch == self.preparation.suggested_branch
        {
            return Err("The suggested branch already exists. Choose another branch.".into());
        }
        let provider = self
            .provider
            .filter(|provider| self.preparation.eligible_providers.contains(provider))
            .ok_or_else(|| "Choose an eligible task-capable Provider.".to_owned())?;
        if !self.preparation.existing_work.is_empty() && !self.start_another_worktree {
            return Err(
                "This Issue already has local work. Open it or choose Start another Worktree."
                    .into(),
            );
        }
        let additional_instructions = if self.instructions.trim().is_empty() {
            None
        } else {
            Some(StartWorkInstructions::new(self.instructions.clone())?)
        };
        Ok(ClientRequest::StartWork {
            confirmation: self.preparation.confirmation(self.project_id),
            branch,
            provider,
            additional_instructions,
            start_another_worktree: self.start_another_worktree,
            columns: 120,
            rows: 40,
        })
    }
}

impl DataRemovalConfirmation {
    pub(crate) fn update(&mut self, confirmation: String) {
        if !self.pending && confirmation.len() <= DATA_REMOVAL_CONFIRMATION.len() {
            self.confirmation = confirmation;
        }
    }

    pub(crate) fn can_submit(&self) -> bool {
        !self.pending && self.confirmation == DATA_REMOVAL_CONFIRMATION
    }

    pub(crate) fn begin_submission(&mut self) {
        if self.can_submit() {
            self.pending = true;
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Modal {
    Settings,
    Shortcuts,
    Form(FormModal),
    Confirmation(Confirmation),
    DataRemoval(DataRemovalConfirmation),
    StartWork(Box<StartWorkModal>),
}

#[cfg(test)]
mod tests {
    use sylvops_core::{
        domain::ProviderKind,
        github::{
            GitHubBody, GitHubBrowserTarget, GitHubHost, GitHubItemIdentity, GitHubItemKind,
            GitHubItemNumber, GitHubLabelName, GitHubOwner, GitHubRepositoryIdentity,
            GitHubRepositoryName, GitHubTimestamp, GitHubTitle, StartWorkBase, StartWorkBranch,
            StartWorkIssueContext, StartWorkPreparation,
        },
        ids::{ProjectId, WorkspaceId, WorktreeId},
        protocol::ClientRequest,
        ui_forms::FormKind,
    };
    use sylvops_daemon::data_removal::DATA_REMOVAL_CONFIRMATION;

    use super::{DataRemovalConfirmation, FirstRunStepState, StartWorkModal, first_run_steps};

    #[test]
    fn user_data_removal_requires_the_exact_confirmation_phrase() {
        let mut confirmation = DataRemovalConfirmation::default();

        assert!(!confirmation.can_submit());
        confirmation.update("delete sylvops user data".into());
        assert!(!confirmation.can_submit());
        confirmation.update(DATA_REMOVAL_CONFIRMATION.into());
        assert!(confirmation.can_submit());

        confirmation.begin_submission();
        assert!(confirmation.pending);
        assert!(!confirmation.can_submit());
    }

    #[test]
    fn first_run_checklist_collapses_completed_steps_and_expands_the_current_step() {
        let workspace = first_run_steps(FormKind::CreateWorkspace).unwrap();
        assert_eq!(
            workspace.map(|step| step.state),
            [
                FirstRunStepState::Current,
                FirstRunStepState::Upcoming,
                FirstRunStepState::Upcoming,
            ]
        );

        let repository = first_run_steps(FormKind::RegisterProject(WorkspaceId::new())).unwrap();
        assert_eq!(
            repository.map(|step| step.state),
            [
                FirstRunStepState::Complete,
                FirstRunStepState::Current,
                FirstRunStepState::Upcoming,
            ]
        );

        let session = first_run_steps(FormKind::CreateSession(WorktreeId::new())).unwrap();
        assert_eq!(
            session.map(|step| step.state),
            [
                FirstRunStepState::Complete,
                FirstRunStepState::Complete,
                FirstRunStepState::Current,
            ]
        );
    }

    #[test]
    fn start_work_keeps_remote_context_immutable_and_user_instructions_separate() {
        let project_id = ProjectId::new();
        let repository = GitHubRepositoryIdentity::new(
            GitHubHost::new("github.example.com").unwrap(),
            GitHubOwner::new("acme").unwrap(),
            GitHubRepositoryName::new("widgets").unwrap(),
        );
        let issue = StartWorkIssueContext {
            identity: GitHubItemIdentity::new(
                repository,
                GitHubItemKind::Issue,
                GitHubItemNumber::new(127).unwrap(),
            ),
            title: GitHubTitle::new("Start Issue work from Desktop").unwrap(),
            body: GitHubBody::new("Immutable untrusted Issue body").unwrap(),
            labels: vec![GitHubLabelName::new("ready-for-agent").unwrap()],
            assignees: Vec::new(),
            updated_at: GitHubTimestamp::new("2026-10-13T12:00:00Z").unwrap(),
            canonical_url: GitHubBrowserTarget::try_from(
                "https://github.example.com/acme/widgets/issues/127".to_owned(),
            )
            .unwrap(),
        };
        let preparation = StartWorkPreparation {
            issue: issue.clone(),
            base: StartWorkBase::new(
                WorktreeId::new(),
                "main",
                "0123456789abcdef0123456789abcdef01234567",
            )
            .unwrap(),
            suggested_branch: StartWorkBranch::new("issue/127-start-issue-work").unwrap(),
            suggested_branch_conflict: false,
            eligible_providers: vec![ProviderKind::Codex],
            existing_work: Vec::new(),
        };
        let mut modal = StartWorkModal::new(project_id, preparation);
        modal.update_instructions("Use the authenticated IPC seam.".into());

        assert_eq!(modal.preparation.issue, issue);
        let ClientRequest::StartWork {
            additional_instructions,
            provider,
            ..
        } = modal.request().unwrap()
        else {
            panic!("expected Start Work request");
        };
        assert_eq!(provider, ProviderKind::Codex);
        assert_eq!(
            additional_instructions.unwrap().as_str(),
            "Use the authenticated IPC seam."
        );

        modal.update_branch("unsafe branch".into());
        assert!(modal.request().is_err());
    }
}
