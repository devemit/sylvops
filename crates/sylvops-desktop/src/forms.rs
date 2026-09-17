use sylvops_core::{domain::GitWorktreeState, ui_forms::Form};

#[derive(Clone, Debug)]
pub(crate) struct FormModal {
    pub form: Form,
    pub pending: bool,
}

impl FormModal {
    pub(crate) fn new(form: Form) -> Self {
        Self {
            form,
            pending: false,
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
    Form(FormModal),
    Confirmation(Confirmation),
}
