use sylvops_core::{
    domain::{GitWorktreeState, ProviderKind},
    ui_forms::Form,
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
    use sylvops_daemon::data_removal::DATA_REMOVAL_CONFIRMATION;

    use super::DataRemovalConfirmation;

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
}
