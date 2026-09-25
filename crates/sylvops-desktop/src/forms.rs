use sylvops_core::{
    domain::{GitWorktreeState, ProviderKind},
    ui_forms::Form,
};

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
}

#[derive(Clone, Debug)]
pub(crate) enum Modal {
    Settings,
    Shortcuts,
    Form(FormModal),
    Confirmation(Confirmation),
}
