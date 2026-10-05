use sylvops_core::{
    domain::{GitWorktreeState, ProviderKind},
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
    },
    RemoveWorktree {
        state: GitWorktreeState,
        name: String,
        canonical_path: String,
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
}

#[cfg(test)]
mod tests {
    use sylvops_core::{
        ids::{WorkspaceId, WorktreeId},
        ui_forms::FormKind,
    };
    use sylvops_daemon::data_removal::DATA_REMOVAL_CONFIRMATION;

    use super::{DataRemovalConfirmation, FirstRunStepState, first_run_steps};

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
}
