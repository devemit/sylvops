//! SQLite ownership actor, migrations, snapshots, and startup reconciliation.

use std::{
    fmt::Write as _,
    io,
    path::Path,
    str::FromStr,
    sync::{Arc, Mutex, mpsc as std_mpsc},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params, types::Type};
use sha2::{Digest, Sha256};
use sylvops_core::domain::{
    DaemonSnapshot, Project, ProviderProfile, Session, SessionState, Workspace, Worktree,
    WorktreeStatus,
};
use sylvops_core::ids::{ProjectId, ProviderProfileId, SessionId, WorkspaceId, WorktreeId};
use sylvops_core::ui::{DesktopState, TuiState};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::{DaemonError, Result};

const ACTOR_CAPACITY: usize = 128;
const TUI_STATE_SCOPE: &str = "tui";
const TUI_STATE_KEY: &str = "navigation.v1";
const MAX_TUI_STATE_BYTES: usize = 4 * 1024;
const DESKTOP_STATE_SCOPE: &str = "desktop";
const DESKTOP_STATE_KEY: &str = "navigation.v1";
const MAX_DESKTOP_STATE_BYTES: usize = 16 * 1024;
const MIGRATIONS: &[(i64, &str, &str)] =
    &[(1, "initial", include_str!("../migrations/0001_initial.sql"))];

#[derive(Clone, Debug)]
pub struct DatabaseHandle {
    inner: Arc<DatabaseInner>,
}

#[derive(Debug)]
struct DatabaseInner {
    commands: mpsc::Sender<DatabaseCommand>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

#[derive(Debug)]
enum DatabaseCommand {
    Snapshot {
        reply: oneshot::Sender<Result<DaemonSnapshot>>,
    },
    GetTuiState {
        reply: oneshot::Sender<Result<Option<TuiState>>>,
    },
    SaveTuiState {
        state: TuiState,
        reply: oneshot::Sender<Result<()>>,
    },
    GetDesktopState {
        reply: oneshot::Sender<Result<Option<DesktopState>>>,
    },
    SaveDesktopState {
        state: DesktopState,
        reply: oneshot::Sender<Result<()>>,
    },
    AddWorkspace {
        name: String,
        reply: oneshot::Sender<Result<(u64, Workspace)>>,
    },
    OpenWorkspace {
        id: WorkspaceId,
        reply: oneshot::Sender<Result<(u64, Workspace)>>,
    },
    AddProject {
        registration: RepositoryRegistration,
        reply: oneshot::Sender<Result<(u64, Project, Worktree)>>,
    },
    RenameProject {
        id: ProjectId,
        name: String,
        reply: oneshot::Sender<Result<(u64, Project)>>,
    },
    Worktree {
        id: WorktreeId,
        reply: oneshot::Sender<Result<Worktree>>,
    },
    Project {
        id: ProjectId,
        reply: oneshot::Sender<Result<Project>>,
    },
    Session {
        id: SessionId,
        reply: oneshot::Sender<Result<Session>>,
    },
    UpsertProvider {
        profile: ProviderProfile,
        reply: oneshot::Sender<Result<(u64, ProviderProfile)>>,
    },
    AddWorktree {
        record: NewManagedWorktree,
        reply: oneshot::Sender<Result<(u64, Worktree)>>,
    },
    RenameWorktree {
        id: WorktreeId,
        name: String,
        reply: oneshot::Sender<Result<(u64, Worktree)>>,
    },
    MarkWorktreeRemoved {
        id: WorktreeId,
        reply: oneshot::Sender<Result<(u64, Worktree)>>,
    },
    MarkWorktreeMissing {
        id: WorktreeId,
        reply: oneshot::Sender<Result<(u64, Worktree)>>,
    },
    WorktreeHasLiveSessions {
        id: WorktreeId,
        reply: oneshot::Sender<Result<bool>>,
    },
    CreateSession {
        record: NewSession,
        reply: oneshot::Sender<Result<Session>>,
    },
    RenameSession {
        id: SessionId,
        name: String,
        reply: oneshot::Sender<Result<(u64, Session)>>,
    },
    MarkSessionRunning {
        id: SessionId,
        process_id: u32,
        reply: oneshot::Sender<Result<(u64, Session)>>,
    },
    FinishSession {
        id: SessionId,
        state: SessionState,
        exit_code: Option<i32>,
        failure_reason: Option<String>,
        reply: oneshot::Sender<Result<(u64, Session)>>,
    },
    MarkSessionSeen {
        id: SessionId,
        reply: oneshot::Sender<Result<Option<(u64, Session)>>>,
    },
    UpdateSessionStatus {
        id: SessionId,
        state: SessionState,
        external_session_id: Option<String>,
        reply: oneshot::Sender<Result<(u64, Session)>>,
    },
    Reconcile {
        reply: oneshot::Sender<Result<usize>>,
    },
    Audit {
        action: String,
        outcome: String,
        details_json: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

#[derive(Clone, Debug)]
pub struct RepositoryRegistration {
    pub workspace_id: WorkspaceId,
    pub name: String,
    pub repository_path: String,
    pub canonical_repository_path: String,
    pub default_branch: Option<String>,
    pub remote_url: Option<String>,
    pub branch: Option<String>,
    pub base_commit: String,
}

#[derive(Clone, Debug)]
pub struct NewSession {
    pub id: SessionId,
    pub worktree_id: WorktreeId,
    pub display_name: String,
    pub provider_profile_id: Option<ProviderProfileId>,
    pub provider_kind: sylvops_core::domain::ProviderKind,
    pub command: String,
    pub arguments_json: String,
    pub cwd: String,
    pub initial_prompt: Option<String>,
    pub external_session_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NewManagedWorktree {
    pub id: WorktreeId,
    pub project_id: ProjectId,
    pub name: String,
    pub path: String,
    pub canonical_path: String,
    pub branch: String,
    pub base_ref: String,
    pub base_commit: String,
}

impl DatabaseHandle {
    /// Opens the database, applies migrations, and starts its dedicated owner thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the database thread cannot start, SQLite cannot be configured, or a
    /// migration fails or has a mismatched checksum.
    pub fn open(path: &Path) -> Result<Self> {
        let path = path.to_path_buf();
        let (commands, receiver) = mpsc::channel(ACTOR_CAPACITY);
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let actor_thread = thread::Builder::new()
            .name("sylvops-database".into())
            .spawn(move || database_thread(&path, receiver, ready_tx))
            .map_err(|error| DaemonError::Database(error.to_string()))?;

        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Self {
                inner: Arc::new(DatabaseInner {
                    commands,
                    thread: Mutex::new(Some(actor_thread)),
                }),
            }),
            Ok(Err(error)) => {
                let _ = actor_thread.join();
                Err(error)
            }
            Err(error) => Err(DaemonError::Database(format!(
                "database startup timed out: {error}"
            ))),
        }
    }

    /// Loads a consistent application snapshot on the database thread.
    ///
    /// # Errors
    ///
    /// Returns an actor, cancellation, query, or persisted-value validation error.
    pub async fn snapshot(&self) -> Result<DaemonSnapshot> {
        let (reply, receive) = oneshot::channel();
        self.inner
            .commands
            .send(DatabaseCommand::Snapshot { reply })
            .await
            .map_err(|_| DaemonError::Database("database actor stopped".into()))?;
        receive
            .await
            .map_err(|_| DaemonError::Database("snapshot request was cancelled".into()))?
    }

    /// Loads the last safe TUI navigation selection, ignoring corrupt values.
    ///
    /// # Errors
    ///
    /// Returns an actor or SQLite query error.
    pub async fn tui_state(&self) -> Result<Option<TuiState>> {
        request(&self.inner.commands, |reply| DatabaseCommand::GetTuiState {
            reply,
        })
        .await
    }

    /// Atomically stores bounded, non-sensitive TUI navigation state.
    ///
    /// This does not advance the authoritative entity revision.
    ///
    /// # Errors
    ///
    /// Returns an actor, serialization, size, or SQLite write error.
    pub async fn save_tui_state(&self, state: TuiState) -> Result<()> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::SaveTuiState { state, reply }
        })
        .await
    }

    /// Loads the last bounded native desktop state, ignoring corrupt values.
    ///
    /// # Errors
    ///
    /// Returns an actor or SQLite query error.
    pub async fn desktop_state(&self) -> Result<Option<DesktopState>> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::GetDesktopState { reply }
        })
        .await
    }

    /// Stores native desktop state without advancing the entity revision.
    ///
    /// # Errors
    ///
    /// Returns an actor, serialization, size, or SQLite write error.
    pub async fn save_desktop_state(&self, state: DesktopState) -> Result<()> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::SaveDesktopState { state, reply }
        })
        .await
    }

    /// Inserts a validated workspace and its audit event transactionally.
    ///
    /// # Errors
    ///
    /// Returns a validation, actor, or SQLite error.
    pub async fn add_workspace(&self, name: String) -> Result<(u64, Workspace)> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::AddWorkspace { name, reply }
        })
        .await
    }

    /// Makes one workspace the active workspace and records the selection atomically.
    ///
    /// # Errors
    ///
    /// Returns an actor, validation, not-found, transaction, or SQLite error.
    pub async fn open_workspace(&self, id: WorkspaceId) -> Result<(u64, Workspace)> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::OpenWorkspace { id, reply }
        })
        .await
    }

    /// Inserts a project and root worktree in one transaction.
    ///
    /// # Errors
    ///
    /// Returns an actor, foreign-key, duplicate-path, or SQLite error.
    pub async fn add_project(
        &self,
        registration: RepositoryRegistration,
    ) -> Result<(u64, Project, Worktree)> {
        request(&self.inner.commands, |reply| DatabaseCommand::AddProject {
            registration,
            reply,
        })
        .await
    }

    /// Changes only a project's display name.
    ///
    /// # Errors
    ///
    /// Returns an actor, validation, not-found, transaction, or SQLite error.
    pub async fn rename_project(&self, id: ProjectId, name: String) -> Result<(u64, Project)> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::RenameProject { id, name, reply }
        })
        .await
    }

    /// Loads one persisted worktree.
    ///
    /// # Errors
    ///
    /// Returns an actor, query, validation, or not-found error.
    pub async fn worktree(&self, id: WorktreeId) -> Result<Worktree> {
        request(&self.inner.commands, |reply| DatabaseCommand::Worktree {
            id,
            reply,
        })
        .await
    }

    /// Loads one persisted project.
    ///
    /// # Errors
    ///
    /// Returns an actor, query, validation, or not-found error.
    pub async fn project(&self, id: ProjectId) -> Result<Project> {
        request(&self.inner.commands, |reply| DatabaseCommand::Project {
            id,
            reply,
        })
        .await
    }

    /// Loads one persisted session.
    ///
    /// # Errors
    ///
    /// Returns an actor, query, validation, or not-found error.
    pub async fn session(&self, id: SessionId) -> Result<Session> {
        request(&self.inner.commands, |reply| DatabaseCommand::Session {
            id,
            reply,
        })
        .await
    }

    /// Inserts or refreshes a provider profile and advances the authoritative revision.
    ///
    /// # Errors
    ///
    /// Returns an actor, serialization, constraint, or SQLite error.
    pub async fn upsert_provider(
        &self,
        profile: ProviderProfile,
    ) -> Result<(u64, ProviderProfile)> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::UpsertProvider { profile, reply }
        })
        .await
    }

    /// Inserts a verified managed worktree and audit event transactionally.
    ///
    /// # Errors
    ///
    /// Returns an actor, foreign-key, duplicate-path, or SQLite error.
    pub async fn add_worktree(&self, record: NewManagedWorktree) -> Result<(u64, Worktree)> {
        request(&self.inner.commands, |reply| DatabaseCommand::AddWorktree {
            record,
            reply,
        })
        .await
    }

    /// Changes only a worktree's display name; its path and branch are untouched.
    ///
    /// # Errors
    ///
    /// Returns an actor, validation, not-found, transaction, or SQLite error.
    pub async fn rename_worktree(&self, id: WorktreeId, name: String) -> Result<(u64, Worktree)> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::RenameWorktree { id, name, reply }
        })
        .await
    }

    /// Marks a non-root managed worktree removed and records an audit event.
    ///
    /// # Errors
    ///
    /// Returns an actor, state-transition, not-found, or SQLite error.
    pub async fn mark_worktree_removed(&self, id: WorktreeId) -> Result<(u64, Worktree)> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::MarkWorktreeRemoved { id, reply }
        })
        .await
    }

    /// Marks a registered worktree unavailable without deleting or rewriting its directory.
    ///
    /// # Errors
    ///
    /// Returns an actor, state-transition, not-found, or SQLite error.
    pub async fn mark_worktree_missing(&self, id: WorktreeId) -> Result<(u64, Worktree)> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::MarkWorktreeMissing { id, reply }
        })
        .await
    }

    /// Reports whether a worktree has a persisted live-looking session.
    ///
    /// # Errors
    ///
    /// Returns an actor or SQLite query error.
    pub async fn worktree_has_live_sessions(&self, id: WorktreeId) -> Result<bool> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::WorktreeHasLiveSessions { id, reply }
        })
        .await
    }

    /// Persists a new session in the starting state.
    ///
    /// # Errors
    ///
    /// Returns an actor, foreign-key, constraint, or SQLite error.
    pub async fn create_session(&self, record: NewSession) -> Result<Session> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::CreateSession { record, reply }
        })
        .await
    }

    /// Changes only a session's display name.
    ///
    /// # Errors
    ///
    /// Returns an actor, validation, not-found, transaction, or SQLite error.
    pub async fn rename_session(&self, id: SessionId, name: String) -> Result<(u64, Session)> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::RenameSession { id, name, reply }
        })
        .await
    }

    /// Transitions a starting session to running after a successful spawn.
    ///
    /// # Errors
    ///
    /// Returns an actor, state-transition, or SQLite error.
    pub async fn mark_session_running(
        &self,
        id: SessionId,
        process_id: u32,
    ) -> Result<(u64, Session)> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::MarkSessionRunning {
                id,
                process_id,
                reply,
            }
        })
        .await
    }

    /// Persists a terminal session state and updates parent activity timestamps.
    ///
    /// # Errors
    ///
    /// Returns an actor, state-transition, not-found, or SQLite error.
    pub async fn finish_session(
        &self,
        id: SessionId,
        state: SessionState,
        exit_code: Option<i32>,
        failure_reason: Option<String>,
    ) -> Result<(u64, Session)> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::FinishSession {
                id,
                state,
                exit_code,
                failure_reason,
                reply,
            }
        })
        .await
    }

    /// Marks a completed unseen session as seen.
    ///
    /// # Errors
    ///
    /// Returns an actor, not-found, or SQLite error.
    pub async fn mark_session_seen(&self, id: SessionId) -> Result<Option<(u64, Session)>> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::MarkSessionSeen { id, reply }
        })
        .await
    }

    /// Applies a normalized provider status and optional verified external session ID.
    ///
    /// # Errors
    ///
    /// Returns an actor, state-transition, not-found, or SQLite error.
    pub async fn update_session_status(
        &self,
        id: SessionId,
        state: SessionState,
        external_session_id: Option<String>,
    ) -> Result<(u64, Session)> {
        request(&self.inner.commands, |reply| {
            DatabaseCommand::UpdateSessionStatus {
                id,
                state,
                external_session_id,
                reply,
            }
        })
        .await
    }

    /// Marks persisted active sessions disconnected without trusting stored process IDs.
    ///
    /// # Errors
    ///
    /// Returns an actor, cancellation, or SQLite transaction error.
    pub async fn reconcile_after_restart(&self) -> Result<usize> {
        let (reply, receive) = oneshot::channel();
        self.inner
            .commands
            .send(DatabaseCommand::Reconcile { reply })
            .await
            .map_err(|_| DaemonError::Database("database actor stopped".into()))?;
        receive
            .await
            .map_err(|_| DaemonError::Database("reconciliation was cancelled".into()))?
    }

    /// Persists one daemon security/lifecycle audit event.
    ///
    /// # Errors
    ///
    /// Returns an actor, cancellation, or SQLite write error.
    pub async fn audit(&self, action: &str, outcome: &str, details_json: &str) -> Result<()> {
        let (reply, receive) = oneshot::channel();
        self.inner
            .commands
            .send(DatabaseCommand::Audit {
                action: action.to_owned(),
                outcome: outcome.to_owned(),
                details_json: details_json.to_owned(),
                reply,
            })
            .await
            .map_err(|_| DaemonError::Database("database actor stopped".into()))?;
        receive
            .await
            .map_err(|_| DaemonError::Database("audit request was cancelled".into()))?
    }

    /// Stops the actor and joins its owner thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the actor has stopped unexpectedly or its thread panics.
    pub async fn shutdown(&self) -> Result<()> {
        let (reply, receive) = oneshot::channel();
        self.inner
            .commands
            .send(DatabaseCommand::Shutdown { reply })
            .await
            .map_err(|_| DaemonError::Database("database actor stopped".into()))?;
        receive
            .await
            .map_err(|_| DaemonError::Database("database shutdown was cancelled".into()))?;

        let actor_thread = self
            .inner
            .thread
            .lock()
            .map_err(|_| DaemonError::Database("database thread lock was poisoned".into()))?
            .take();
        if let Some(actor_thread) = actor_thread {
            tokio::task::spawn_blocking(move || actor_thread.join())
                .await
                .map_err(|error| DaemonError::Database(error.to_string()))?
                .map_err(|_| DaemonError::Database("database thread panicked".into()))?;
        }
        Ok(())
    }
}

async fn request<T>(
    commands: &mpsc::Sender<DatabaseCommand>,
    make_command: impl FnOnce(oneshot::Sender<Result<T>>) -> DatabaseCommand,
) -> Result<T> {
    let (reply, receive) = oneshot::channel();
    commands
        .send(make_command(reply))
        .await
        .map_err(|_| DaemonError::Database("database actor stopped".into()))?;
    receive
        .await
        .map_err(|_| DaemonError::Database("database request was cancelled".into()))?
}

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_lines)]
fn database_thread(
    path: &Path,
    mut commands: mpsc::Receiver<DatabaseCommand>,
    ready: std_mpsc::SyncSender<Result<()>>,
) {
    let mut connection = match open_connection(path) {
        Ok(connection) => {
            let _ = ready.send(Ok(()));
            connection
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    let mut revision = 1_u64;
    while let Some(command) = commands.blocking_recv() {
        match command {
            DatabaseCommand::Snapshot { reply } => {
                let _ = reply.send(load_snapshot(&connection, revision));
            }
            DatabaseCommand::GetTuiState { reply } => {
                let _ = reply.send(load_tui_state(&connection));
            }
            DatabaseCommand::SaveTuiState { state, reply } => {
                let _ = reply.send(save_tui_state(&connection, &state));
            }
            DatabaseCommand::GetDesktopState { reply } => {
                let _ = reply.send(load_desktop_state(&connection));
            }
            DatabaseCommand::SaveDesktopState { state, reply } => {
                let _ = reply.send(save_desktop_state(&connection, &state));
            }
            DatabaseCommand::AddWorkspace { name, reply } => {
                let result = insert_workspace(&mut connection, &name).map(|workspace| {
                    revision = revision.saturating_add(1);
                    (revision, workspace)
                });
                let _ = reply.send(result);
            }
            DatabaseCommand::OpenWorkspace { id, reply } => {
                let result = open_workspace(&mut connection, id).map(|workspace| {
                    revision = revision.saturating_add(1);
                    (revision, workspace)
                });
                let _ = reply.send(result);
            }
            DatabaseCommand::AddProject {
                registration,
                reply,
            } => {
                let result =
                    insert_project(&mut connection, &registration).map(|(project, worktree)| {
                        revision = revision.saturating_add(1);
                        (revision, project, worktree)
                    });
                let _ = reply.send(result);
            }
            DatabaseCommand::RenameProject { id, name, reply } => {
                let result = rename_project(&mut connection, id, &name).map(|project| {
                    revision = revision.saturating_add(1);
                    (revision, project)
                });
                let _ = reply.send(result);
            }
            DatabaseCommand::Worktree { id, reply } => {
                let _ = reply.send(load_worktree(&connection, id));
            }
            DatabaseCommand::Project { id, reply } => {
                let _ = reply.send(load_project(&connection, id));
            }
            DatabaseCommand::Session { id, reply } => {
                let _ = reply.send(load_session(&connection, id));
            }
            DatabaseCommand::UpsertProvider { profile, reply } => {
                let result = upsert_provider(&mut connection, &profile).map(|profile| {
                    revision = revision.saturating_add(1);
                    (revision, profile)
                });
                let _ = reply.send(result);
            }
            DatabaseCommand::AddWorktree { record, reply } => {
                let result = insert_worktree(&mut connection, &record).map(|worktree| {
                    revision = revision.saturating_add(1);
                    (revision, worktree)
                });
                let _ = reply.send(result);
            }
            DatabaseCommand::RenameWorktree { id, name, reply } => {
                let result = rename_worktree(&mut connection, id, &name).map(|worktree| {
                    revision = revision.saturating_add(1);
                    (revision, worktree)
                });
                let _ = reply.send(result);
            }
            DatabaseCommand::MarkWorktreeRemoved { id, reply } => {
                let result = mark_worktree_removed(&mut connection, id).map(|worktree| {
                    revision = revision.saturating_add(1);
                    (revision, worktree)
                });
                let _ = reply.send(result);
            }
            DatabaseCommand::MarkWorktreeMissing { id, reply } => {
                let result = mark_worktree_missing(&mut connection, id).map(|worktree| {
                    revision = revision.saturating_add(1);
                    (revision, worktree)
                });
                let _ = reply.send(result);
            }
            DatabaseCommand::WorktreeHasLiveSessions { id, reply } => {
                let _ = reply.send(worktree_has_live_sessions(&connection, id));
            }
            DatabaseCommand::CreateSession { record, reply } => {
                let _ = reply.send(insert_session(&mut connection, &record));
            }
            DatabaseCommand::RenameSession { id, name, reply } => {
                let result = rename_session(&mut connection, id, &name).map(|session| {
                    revision = revision.saturating_add(1);
                    (revision, session)
                });
                let _ = reply.send(result);
            }
            DatabaseCommand::MarkSessionRunning {
                id,
                process_id,
                reply,
            } => {
                let result = mark_session_running(&mut connection, id, process_id).map(|session| {
                    revision = revision.saturating_add(1);
                    (revision, session)
                });
                let _ = reply.send(result);
            }
            DatabaseCommand::FinishSession {
                id,
                state,
                exit_code,
                failure_reason,
                reply,
            } => {
                let result = finish_session(
                    &mut connection,
                    id,
                    state,
                    exit_code,
                    failure_reason.as_deref(),
                )
                .map(|session| {
                    revision = revision.saturating_add(1);
                    (revision, session)
                });
                let _ = reply.send(result);
            }
            DatabaseCommand::MarkSessionSeen { id, reply } => {
                let result = mark_session_seen(&mut connection, id).map(|session| {
                    session.map(|session| {
                        revision = revision.saturating_add(1);
                        (revision, session)
                    })
                });
                let _ = reply.send(result);
            }
            DatabaseCommand::UpdateSessionStatus {
                id,
                state,
                external_session_id,
                reply,
            } => {
                let result = update_session_status(
                    &mut connection,
                    id,
                    state,
                    external_session_id.as_deref(),
                )
                .map(|session| {
                    revision = revision.saturating_add(1);
                    (revision, session)
                });
                let _ = reply.send(result);
            }
            DatabaseCommand::Reconcile { reply } => {
                let result = reconcile_sessions(&mut connection);
                if matches!(result, Ok(changed) if changed > 0) {
                    revision = revision.saturating_add(1);
                }
                let _ = reply.send(result);
            }
            DatabaseCommand::Audit {
                action,
                outcome,
                details_json,
                reply,
            } => {
                let _ = reply.send(insert_audit_event(
                    &connection,
                    &action,
                    &outcome,
                    &details_json,
                ));
            }
            DatabaseCommand::Shutdown { reply } => {
                let _ = reply.send(());
                return;
            }
        }
    }
}

fn open_connection(path: &Path) -> Result<Connection> {
    let mut connection = Connection::open(path).map_err(database_error)?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(database_error)?;
    connection
        .pragma_update(None, "foreign_keys", true)
        .map_err(database_error)?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(database_error)?;
    connection
        .pragma_update(None, "synchronous", "NORMAL")
        .map_err(database_error)?;
    apply_migrations(&mut connection)?;
    Ok(connection)
}

fn apply_migrations(connection: &mut Connection) -> Result<()> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (\
             version INTEGER PRIMARY KEY, \
             name TEXT NOT NULL, \
             applied_at INTEGER NOT NULL, \
             checksum TEXT NOT NULL\
             ) STRICT;",
        )
        .map_err(database_error)?;

    let latest_supported = MIGRATIONS.last().map_or(0, |migration| migration.0);
    let latest_present: Option<i64> = connection
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .map_err(database_error)?;
    if let Some(version) = latest_present.filter(|version| *version > latest_supported) {
        return Err(DaemonError::Database(format!(
            "database schema version {version} is newer than supported version \
             {latest_supported}"
        )));
    }

    for &(version, name, source) in MIGRATIONS {
        let checksum = checksum(source);
        let existing: Option<String> = connection
            .query_row(
                "SELECT checksum FROM schema_migrations WHERE version = ?1",
                [version],
                |row| row.get(0),
            )
            .optional()
            .map_err(database_error)?;
        match existing {
            Some(existing) if existing != checksum => {
                return Err(DaemonError::Database(format!(
                    "migration {version} ({name}) checksum does not match"
                )));
            }
            Some(_) => continue,
            None => {}
        }

        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        transaction.execute_batch(source).map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO schema_migrations(version, name, applied_at, checksum) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![version, name, now_millis(), checksum],
            )
            .map_err(database_error)?;
        transaction.commit().map_err(database_error)?;
    }
    Ok(())
}

fn insert_workspace(connection: &mut Connection, name: &str) -> Result<Workspace> {
    let name = validated_name(name, "workspace")?;
    let now = now_millis();
    let workspace = Workspace {
        id: WorkspaceId::new(),
        name: name.to_owned(),
        created_at: now,
        updated_at: now,
        last_opened_at: Some(now),
        is_open: true,
    };
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    transaction
        .execute("UPDATE workspaces SET is_open = 0 WHERE is_open = 1", [])
        .map_err(database_error)?;
    transaction
        .execute(
            "INSERT INTO workspaces(id, name, created_at, updated_at, last_opened_at, is_open) \
             VALUES (?1, ?2, ?3, ?4, ?5, 1)",
            params![workspace.id.to_string(), workspace.name, now, now, now],
        )
        .map_err(database_error)?;
    insert_entity_audit(
        &transaction,
        "workspace_created",
        "workspace",
        &workspace.id.to_string(),
    )?;
    transaction.commit().map_err(database_error)?;
    Ok(workspace)
}

fn open_workspace(connection: &mut Connection, id: WorkspaceId) -> Result<Workspace> {
    let now = now_millis();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    if !transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM workspaces WHERE id = ?1)",
            [id.to_string()],
            |row| row.get::<_, bool>(0),
        )
        .map_err(database_error)?
    {
        return Err(DaemonError::Database(format!(
            "workspace {id} does not exist"
        )));
    }
    transaction
        .execute("UPDATE workspaces SET is_open = 0 WHERE is_open = 1", [])
        .map_err(database_error)?;
    transaction
        .execute(
            "UPDATE workspaces SET is_open = 1, last_opened_at = ?2, updated_at = ?2 WHERE id = ?1",
            params![id.to_string(), now],
        )
        .map_err(database_error)?;
    insert_entity_audit(
        &transaction,
        "workspace_opened",
        "workspace",
        &id.to_string(),
    )?;
    let workspace = load_workspace(&transaction, id)?;
    transaction.commit().map_err(database_error)?;
    Ok(workspace)
}

fn insert_project(
    connection: &mut Connection,
    registration: &RepositoryRegistration,
) -> Result<(Project, Worktree)> {
    let now = now_millis();
    let project = Project {
        id: sylvops_core::ids::ProjectId::new(),
        workspace_id: registration.workspace_id,
        name: registration.name.clone(),
        repository_path: registration.repository_path.clone(),
        canonical_repository_path: registration.canonical_repository_path.clone(),
        default_branch: registration.default_branch.clone(),
        remote_url: registration.remote_url.clone(),
        created_at: now,
        last_activity_at: now,
    };
    let worktree = Worktree {
        id: WorktreeId::new(),
        project_id: project.id,
        name: "Root checkout".into(),
        path: registration.repository_path.clone(),
        canonical_path: registration.canonical_repository_path.clone(),
        branch: registration.branch.clone(),
        base_ref: registration
            .branch
            .clone()
            .unwrap_or_else(|| registration.base_commit.clone()),
        base_commit: registration.base_commit.clone(),
        is_root_checkout: true,
        status: WorktreeStatus::Active,
        created_at: now,
        last_activity_at: now,
        removed_at: None,
    };
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    transaction
        .execute(
            "INSERT INTO projects(id, workspace_id, name, repository_path, \
             canonical_repository_path, default_branch, remote_url, created_at, last_activity_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                project.id.to_string(),
                project.workspace_id.to_string(),
                project.name,
                project.repository_path,
                project.canonical_repository_path,
                project.default_branch,
                project.remote_url,
                now,
                now
            ],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "INSERT INTO worktrees(id, project_id, name, path, canonical_path, branch, base_ref, \
             base_commit, is_root_checkout, status, created_at, last_activity_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, 'active', ?9, ?10)",
            params![
                worktree.id.to_string(),
                worktree.project_id.to_string(),
                worktree.name,
                worktree.path,
                worktree.canonical_path,
                worktree.branch,
                worktree.base_ref,
                worktree.base_commit,
                now,
                now
            ],
        )
        .map_err(database_error)?;
    insert_entity_audit(
        &transaction,
        "project_registered",
        "project",
        &project.id.to_string(),
    )?;
    transaction.commit().map_err(database_error)?;
    Ok((project, worktree))
}

fn rename_project(connection: &mut Connection, id: ProjectId, name: &str) -> Result<Project> {
    let name = validated_name(name, "project")?;
    let now = now_millis();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    let changed = transaction
        .execute(
            "UPDATE projects SET name = ?2, last_activity_at = ?3 WHERE id = ?1",
            params![id.to_string(), name, now],
        )
        .map_err(database_error)?;
    if changed != 1 {
        return Err(DaemonError::Database(format!(
            "project {id} does not exist"
        )));
    }
    insert_entity_audit(&transaction, "project_renamed", "project", &id.to_string())?;
    let project = load_project(&transaction, id)?;
    transaction.commit().map_err(database_error)?;
    Ok(project)
}

fn load_workspace(connection: &Connection, id: WorkspaceId) -> Result<Workspace> {
    connection
        .query_row(
            "SELECT id, name, created_at, updated_at, last_opened_at, is_open \
             FROM workspaces WHERE id = ?1",
            [id.to_string()],
            workspace_from_row,
        )
        .optional()
        .map_err(database_error)?
        .ok_or_else(|| DaemonError::Database(format!("workspace {id} does not exist")))
}

fn load_worktree(connection: &Connection, id: WorktreeId) -> Result<Worktree> {
    connection
        .query_row(
            "SELECT id, project_id, name, path, canonical_path, branch, base_ref, base_commit, \
             is_root_checkout, status, created_at, last_activity_at, removed_at \
             FROM worktrees WHERE id = ?1",
            [id.to_string()],
            worktree_from_row,
        )
        .optional()
        .map_err(database_error)?
        .ok_or_else(|| DaemonError::Database(format!("worktree {id} does not exist")))
}

fn load_project(connection: &Connection, id: ProjectId) -> Result<Project> {
    connection
        .query_row(
            "SELECT id, workspace_id, name, repository_path, canonical_repository_path, \
                    default_branch, remote_url, created_at, last_activity_at \
             FROM projects WHERE id = ?1",
            [id.to_string()],
            project_from_row,
        )
        .optional()
        .map_err(database_error)?
        .ok_or_else(|| DaemonError::Database(format!("project {id} does not exist")))
}

fn insert_worktree(connection: &mut Connection, record: &NewManagedWorktree) -> Result<Worktree> {
    let now = now_millis();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    transaction
        .execute(
            "INSERT INTO worktrees(id, project_id, name, path, canonical_path, branch, base_ref, \
             base_commit, is_root_checkout, status, created_at, last_activity_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 'active', ?9, ?10)",
            params![
                record.id.to_string(),
                record.project_id.to_string(),
                record.name,
                record.path,
                record.canonical_path,
                record.branch,
                record.base_ref,
                record.base_commit,
                now,
                now
            ],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "UPDATE projects SET last_activity_at = ?2 WHERE id = ?1",
            params![record.project_id.to_string(), now],
        )
        .map_err(database_error)?;
    insert_entity_audit(
        &transaction,
        "worktree_created",
        "worktree",
        &record.id.to_string(),
    )?;
    let worktree = load_worktree(&transaction, record.id)?;
    transaction.commit().map_err(database_error)?;
    Ok(worktree)
}

fn rename_worktree(connection: &mut Connection, id: WorktreeId, name: &str) -> Result<Worktree> {
    let name = validated_name(name, "worktree")?;
    let now = now_millis();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    let changed = transaction
        .execute(
            "UPDATE worktrees SET name = ?2, last_activity_at = ?3 \
             WHERE id = ?1 AND status != 'removed'",
            params![id.to_string(), name, now],
        )
        .map_err(database_error)?;
    if changed != 1 {
        return Err(DaemonError::Database(format!(
            "worktree {id} does not exist or has been removed"
        )));
    }
    transaction
        .execute(
            "UPDATE projects SET last_activity_at = ?2 \
             WHERE id = (SELECT project_id FROM worktrees WHERE id = ?1)",
            params![id.to_string(), now],
        )
        .map_err(database_error)?;
    insert_entity_audit(
        &transaction,
        "worktree_renamed",
        "worktree",
        &id.to_string(),
    )?;
    let worktree = load_worktree(&transaction, id)?;
    transaction.commit().map_err(database_error)?;
    Ok(worktree)
}

fn mark_worktree_removed(connection: &mut Connection, id: WorktreeId) -> Result<Worktree> {
    let now = now_millis();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    let changed = transaction
        .execute(
            "UPDATE worktrees SET status = 'removed', removed_at = ?2, last_activity_at = ?2 \
             WHERE id = ?1 AND is_root_checkout = 0 AND status = 'active'",
            params![id.to_string(), now],
        )
        .map_err(database_error)?;
    if changed != 1 {
        return Err(DaemonError::Database(format!(
            "worktree {id} is not an active managed worktree"
        )));
    }
    let worktree = load_worktree(&transaction, id)?;
    transaction
        .execute(
            "UPDATE projects SET last_activity_at = ?2 WHERE id = ?1",
            params![worktree.project_id.to_string(), now],
        )
        .map_err(database_error)?;
    insert_entity_audit(
        &transaction,
        "worktree_removed",
        "worktree",
        &id.to_string(),
    )?;
    transaction.commit().map_err(database_error)?;
    Ok(worktree)
}

fn mark_worktree_missing(connection: &mut Connection, id: WorktreeId) -> Result<Worktree> {
    let now = now_millis();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    let changed = transaction
        .execute(
            "UPDATE worktrees SET status = 'missing', last_activity_at = ?2 \
             WHERE id = ?1 AND status = 'active'",
            params![id.to_string(), now],
        )
        .map_err(database_error)?;
    if changed != 1 {
        return Err(DaemonError::Database(format!(
            "worktree {id} is not active"
        )));
    }
    insert_entity_audit(
        &transaction,
        "worktree_missing",
        "worktree",
        &id.to_string(),
    )?;
    let worktree = load_worktree(&transaction, id)?;
    transaction.commit().map_err(database_error)?;
    Ok(worktree)
}

fn worktree_has_live_sessions(connection: &Connection, id: WorktreeId) -> Result<bool> {
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE worktree_id = ?1 \
             AND state IN ('starting', 'running', 'needs_feedback')",
            [id.to_string()],
            |row| row.get(0),
        )
        .map_err(database_error)?;
    Ok(count > 0)
}

fn insert_session(connection: &mut Connection, record: &NewSession) -> Result<Session> {
    let now = now_millis();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    transaction
        .execute(
            "INSERT INTO sessions(id, worktree_id, provider_profile_id, provider_kind, display_name, state, command, \
             arguments_json, cwd, external_session_id, created_at, last_activity_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'starting', ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                record.id.to_string(),
                record.worktree_id.to_string(),
                record.provider_profile_id.map(|id| id.to_string()),
                record.provider_kind.to_string(),
                record.display_name,
                record.command,
                record.arguments_json,
                record.cwd,
                record.external_session_id,
                now,
                now
            ],
        )
        .map_err(database_error)?;
    if let Some(prompt) = record.initial_prompt.as_deref() {
        transaction
            .execute(
                "INSERT INTO session_prompts(id, session_id, submitted_at, prompt_text, byte_length, retention_class) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'recent')",
                params![
                    Uuid::now_v7().to_string(),
                    record.id.to_string(),
                    now,
                    prompt,
                    i64::try_from(prompt.len()).unwrap_or(i64::MAX)
                ],
            )
            .map_err(database_error)?;
    }
    insert_entity_audit(
        &transaction,
        "session_created",
        "session",
        &record.id.to_string(),
    )?;
    let session = load_session(&transaction, record.id)?;
    transaction.commit().map_err(database_error)?;
    Ok(session)
}

fn rename_session(connection: &mut Connection, id: SessionId, name: &str) -> Result<Session> {
    let name = validated_name(name, "session")?;
    let now = now_millis();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    let changed = transaction
        .execute(
            "UPDATE sessions SET display_name = ?2, last_activity_at = ?3 WHERE id = ?1",
            params![id.to_string(), name, now],
        )
        .map_err(database_error)?;
    if changed != 1 {
        return Err(DaemonError::Database(format!(
            "session {id} does not exist"
        )));
    }
    transaction
        .execute(
            "UPDATE worktrees SET last_activity_at = ?2 \
             WHERE id = (SELECT worktree_id FROM sessions WHERE id = ?1)",
            params![id.to_string(), now],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "UPDATE projects SET last_activity_at = ?2 WHERE id = (\
                SELECT worktrees.project_id FROM worktrees \
                JOIN sessions ON sessions.worktree_id = worktrees.id WHERE sessions.id = ?1\
             )",
            params![id.to_string(), now],
        )
        .map_err(database_error)?;
    insert_entity_audit(&transaction, "session_renamed", "session", &id.to_string())?;
    let session = load_session(&transaction, id)?;
    transaction.commit().map_err(database_error)?;
    Ok(session)
}

fn validated_name<'a>(name: &'a str, entity: &str) -> Result<&'a str> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(DaemonError::Database(format!(
            "{entity} name must contain between 1 and 200 characters"
        )));
    }
    Ok(name)
}

fn upsert_provider(
    connection: &mut Connection,
    profile: &ProviderProfile,
) -> Result<ProviderProfile> {
    connection
        .execute(
            "INSERT INTO provider_profiles(id, kind, display_name, executable_path, default_model, \
             default_effort, enabled, capabilities_json, last_probe_status, last_probe_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(id) DO UPDATE SET executable_path = excluded.executable_path, \
             default_model = excluded.default_model, default_effort = excluded.default_effort, \
             enabled = excluded.enabled, capabilities_json = excluded.capabilities_json, \
             last_probe_status = excluded.last_probe_status, last_probe_at = excluded.last_probe_at",
            params![
                profile.id.to_string(),
                profile.kind.to_string(),
                profile.display_name,
                profile.executable_path,
                profile.default_model,
                profile.default_effort,
                i64::from(profile.enabled),
                profile.capabilities_json,
                profile.last_probe_status,
                profile.last_probe_at
            ],
        )
        .map_err(database_error)?;
    load_provider(connection, profile.id)
}

fn load_provider(connection: &Connection, id: ProviderProfileId) -> Result<ProviderProfile> {
    connection
        .query_row(
            "SELECT id, kind, display_name, executable_path, default_model, default_effort, \
             enabled, capabilities_json, last_probe_status, last_probe_at \
             FROM provider_profiles WHERE id = ?1",
            [id.to_string()],
            provider_from_row,
        )
        .optional()
        .map_err(database_error)?
        .ok_or_else(|| DaemonError::Database(format!("provider profile {id} does not exist")))
}

fn mark_session_running(
    connection: &mut Connection,
    id: SessionId,
    process_id: u32,
) -> Result<Session> {
    let now = now_millis();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    let changed = transaction
        .execute(
            "UPDATE sessions SET \
             state = CASE WHEN state = 'starting' THEN 'running' ELSE state END, \
             process_id = ?2, started_at = COALESCE(started_at, ?3), \
             last_activity_at = ?3, failure_reason = NULL \
             WHERE id = ?1 AND state IN \
             ('starting', 'running', 'needs_feedback', 'finished_unseen', 'finished_seen')",
            params![id.to_string(), i64::from(process_id), now],
        )
        .map_err(database_error)?;
    if changed != 1 {
        return Err(DaemonError::Database(format!(
            "session {id} cannot accept a spawned process in its current state"
        )));
    }
    insert_entity_audit(&transaction, "session_started", "session", &id.to_string())?;
    let session = load_session(&transaction, id)?;
    transaction.commit().map_err(database_error)?;
    Ok(session)
}

fn finish_session(
    connection: &mut Connection,
    id: SessionId,
    state: SessionState,
    exit_code: Option<i32>,
    failure_reason: Option<&str>,
) -> Result<Session> {
    if !matches!(
        state,
        SessionState::FinishedUnseen
            | SessionState::FinishedSeen
            | SessionState::Failed
            | SessionState::Terminated
    ) {
        return Err(DaemonError::Database(
            "invalid terminal session state".into(),
        ));
    }
    let now = now_millis();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    let changed = transaction
        .execute(
            "UPDATE sessions SET state = ?2, process_id = NULL, ended_at = ?3, \
             last_activity_at = ?3, exit_code = ?4, failure_reason = ?5 WHERE id = ?1",
            params![
                id.to_string(),
                state.to_string(),
                now,
                exit_code,
                failure_reason
            ],
        )
        .map_err(database_error)?;
    if changed != 1 {
        return Err(DaemonError::Database(format!(
            "session {id} does not exist"
        )));
    }
    let session = load_session(&transaction, id)?;
    transaction
        .execute(
            "UPDATE worktrees SET last_activity_at = ?2 WHERE id = ?1",
            params![session.worktree_id.to_string(), now],
        )
        .map_err(database_error)?;
    transaction
        .execute(
            "UPDATE projects SET last_activity_at = ?2 WHERE id = \
             (SELECT project_id FROM worktrees WHERE id = ?1)",
            params![session.worktree_id.to_string(), now],
        )
        .map_err(database_error)?;
    insert_entity_audit(&transaction, "session_finished", "session", &id.to_string())?;
    transaction.commit().map_err(database_error)?;
    Ok(session)
}

fn mark_session_seen(connection: &mut Connection, id: SessionId) -> Result<Option<Session>> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    let changed = transaction
        .execute(
            "UPDATE sessions SET state = 'finished_seen' WHERE id = ?1 AND state = 'finished_unseen'",
            [id.to_string()],
        )
        .map_err(database_error)?;
    if changed == 0 {
        transaction.commit().map_err(database_error)?;
        return Ok(None);
    }
    insert_entity_audit(&transaction, "session_seen", "session", &id.to_string())?;
    let session = load_session(&transaction, id)?;
    transaction.commit().map_err(database_error)?;
    Ok(Some(session))
}

fn update_session_status(
    connection: &mut Connection,
    id: SessionId,
    state: SessionState,
    external_session_id: Option<&str>,
) -> Result<Session> {
    if !matches!(
        state,
        SessionState::Running
            | SessionState::NeedsFeedback
            | SessionState::FinishedUnseen
            | SessionState::FinishedSeen
    ) {
        return Err(DaemonError::Database(
            "invalid provider-driven session state".into(),
        ));
    }
    let now = now_millis();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    let changed = transaction
        .execute(
            "UPDATE sessions SET state = ?2, external_session_id = COALESCE(?3, external_session_id), \
             last_activity_at = ?4 WHERE id = ?1 AND state NOT IN ('failed', 'terminated', 'disconnected')",
            params![id.to_string(), state.to_string(), external_session_id, now],
        )
        .map_err(database_error)?;
    if changed != 1 {
        return Err(DaemonError::Database(format!(
            "session {id} cannot accept provider events"
        )));
    }
    insert_entity_audit(
        &transaction,
        "provider_status_changed",
        "session",
        &id.to_string(),
    )?;
    let session = load_session(&transaction, id)?;
    transaction.commit().map_err(database_error)?;
    Ok(session)
}

fn load_session(connection: &Connection, id: SessionId) -> Result<Session> {
    connection
        .query_row(
            "SELECT id, worktree_id, provider_profile_id, provider_kind, display_name, state, \
             process_id, external_session_id, command, arguments_json, cwd, created_at, \
             started_at, ended_at, last_activity_at, last_seen_output_sequence, exit_code, \
             failure_reason FROM sessions WHERE id = ?1",
            [id.to_string()],
            session_from_row,
        )
        .optional()
        .map_err(database_error)?
        .ok_or_else(|| DaemonError::Database(format!("session {id} does not exist")))
}

fn insert_entity_audit(
    connection: &Connection,
    action: &str,
    entity_kind: &str,
    entity_id: &str,
) -> Result<()> {
    connection
        .execute(
            "INSERT INTO audit_events(id, occurred_at, actor_kind, action, entity_kind, \
             entity_id, outcome, details_json) VALUES (?1, ?2, 'client', ?3, ?4, ?5, \
             'succeeded', '{}')",
            params![
                Uuid::now_v7().to_string(),
                now_millis(),
                action,
                entity_kind,
                entity_id
            ],
        )
        .map_err(database_error)?;
    Ok(())
}

fn reconcile_sessions(connection: &mut Connection) -> Result<usize> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    let session_ids = {
        let mut statement = transaction
            .prepare(
                "SELECT id FROM sessions WHERE state IN \
                 ('starting', 'running', 'needs_feedback')",
            )
            .map_err(database_error)?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(database_error)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(database_error)?
    };

    for session_id in &session_ids {
        transaction
            .execute(
                "UPDATE sessions SET state = 'disconnected', process_id = NULL, \
                 failure_reason = 'daemon restarted; process identity was not trusted' \
                 WHERE id = ?1",
                [session_id],
            )
            .map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO audit_events(\
                    id, occurred_at, actor_kind, action, entity_kind, entity_id, outcome, details_json\
                 ) VALUES (?1, ?2, 'recovery', 'session_disconnected', 'session', ?3, \
                           'succeeded', '{\"reason\":\"daemon_restart\"}')",
                params![Uuid::now_v7().to_string(), now_millis(), session_id],
            )
            .map_err(database_error)?;
    }
    transaction.commit().map_err(database_error)?;
    Ok(session_ids.len())
}

fn insert_audit_event(
    connection: &Connection,
    action: &str,
    outcome: &str,
    details_json: &str,
) -> Result<()> {
    if !matches!(outcome, "attempted" | "succeeded" | "refused" | "failed") {
        return Err(DaemonError::Database("invalid audit outcome".into()));
    }
    serde_json::from_str::<serde_json::Value>(details_json)
        .map_err(|error| DaemonError::Database(format!("invalid audit details: {error}")))?;
    connection
        .execute(
            "INSERT INTO audit_events(\
                id, occurred_at, actor_kind, action, outcome, details_json\
             ) VALUES (?1, ?2, 'daemon', ?3, ?4, ?5)",
            params![
                Uuid::now_v7().to_string(),
                now_millis(),
                action,
                outcome,
                details_json
            ],
        )
        .map_err(database_error)?;
    Ok(())
}

fn load_tui_state(connection: &Connection) -> Result<Option<TuiState>> {
    let value = connection
        .query_row(
            "SELECT value_json FROM ui_state WHERE client_scope = ?1 AND key = ?2",
            params![TUI_STATE_SCOPE, TUI_STATE_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(database_error)?;
    Ok(value.and_then(|value| {
        if value.len() > MAX_TUI_STATE_BYTES {
            None
        } else {
            serde_json::from_str(&value).ok()
        }
    }))
}

fn save_tui_state(connection: &Connection, state: &TuiState) -> Result<()> {
    let value = serde_json::to_string(state)
        .map_err(|error| DaemonError::Database(format!("cannot serialize TUI state: {error}")))?;
    if value.len() > MAX_TUI_STATE_BYTES {
        return Err(DaemonError::Database("TUI state exceeds size limit".into()));
    }
    connection
        .execute(
            "INSERT INTO ui_state(client_scope, key, value_json, updated_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(client_scope, key) DO UPDATE SET value_json = excluded.value_json, \
             updated_at = excluded.updated_at",
            params![TUI_STATE_SCOPE, TUI_STATE_KEY, value, now_millis()],
        )
        .map_err(database_error)?;
    Ok(())
}

fn load_desktop_state(connection: &Connection) -> Result<Option<DesktopState>> {
    let value = connection
        .query_row(
            "SELECT value_json FROM ui_state WHERE client_scope = ?1 AND key = ?2",
            params![DESKTOP_STATE_SCOPE, DESKTOP_STATE_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(database_error)?;
    Ok(value.and_then(|value| {
        if value.len() > MAX_DESKTOP_STATE_BYTES {
            None
        } else {
            serde_json::from_str::<DesktopState>(&value)
                .ok()
                .map(DesktopState::normalized)
        }
    }))
}

fn save_desktop_state(connection: &Connection, state: &DesktopState) -> Result<()> {
    let value = serde_json::to_string(&state.clone().normalized()).map_err(|error| {
        DaemonError::Database(format!("cannot serialize desktop state: {error}"))
    })?;
    if value.len() > MAX_DESKTOP_STATE_BYTES {
        return Err(DaemonError::Database(
            "desktop state exceeds size limit".into(),
        ));
    }
    connection
        .execute(
            "INSERT INTO ui_state(client_scope, key, value_json, updated_at) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(client_scope, key) DO UPDATE SET value_json = excluded.value_json, \
             updated_at = excluded.updated_at",
            params![DESKTOP_STATE_SCOPE, DESKTOP_STATE_KEY, value, now_millis()],
        )
        .map_err(database_error)?;
    Ok(())
}

fn load_snapshot(connection: &Connection, revision: u64) -> Result<DaemonSnapshot> {
    Ok(DaemonSnapshot {
        revision,
        captured_at: now_millis(),
        workspaces: query_all(
            connection,
            "SELECT id, name, created_at, updated_at, last_opened_at, is_open \
             FROM workspaces ORDER BY created_at ASC, id ASC",
            workspace_from_row,
        )?,
        projects: query_all(
            connection,
            "SELECT id, workspace_id, name, repository_path, canonical_repository_path, \
                    default_branch, remote_url, created_at, last_activity_at \
             FROM projects ORDER BY last_activity_at DESC",
            project_from_row,
        )?,
        worktrees: query_all(
            connection,
            "SELECT id, project_id, name, path, canonical_path, branch, base_ref, base_commit, \
                    is_root_checkout, status, created_at, last_activity_at, removed_at \
             FROM worktrees ORDER BY project_id, is_root_checkout DESC, last_activity_at DESC",
            worktree_from_row,
        )?,
        sessions: query_all(
            connection,
            "SELECT id, worktree_id, provider_profile_id, provider_kind, display_name, state, \
                    process_id, external_session_id, command, arguments_json, cwd, created_at, \
                    started_at, ended_at, last_activity_at, last_seen_output_sequence, exit_code, \
                    failure_reason FROM sessions ORDER BY last_activity_at DESC",
            session_from_row,
        )?,
        provider_profiles: query_all(
            connection,
            "SELECT id, kind, display_name, executable_path, default_model, default_effort, \
                    enabled, capabilities_json, last_probe_status, last_probe_at \
             FROM provider_profiles ORDER BY display_name",
            provider_from_row,
        )?,
    })
}

fn query_all<T>(
    connection: &Connection,
    sql: &str,
    map: fn(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>> {
    let mut statement = connection.prepare(sql).map_err(database_error)?;
    statement
        .query_map([], map)
        .map_err(database_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(database_error)
}

fn workspace_from_row(row: &Row<'_>) -> rusqlite::Result<Workspace> {
    Ok(Workspace {
        id: parse_text(row, 0)?,
        name: row.get(1)?,
        created_at: row.get(2)?,
        updated_at: row.get(3)?,
        last_opened_at: row.get(4)?,
        is_open: row.get::<_, i64>(5)? != 0,
    })
}

fn project_from_row(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: parse_text(row, 0)?,
        workspace_id: parse_text(row, 1)?,
        name: row.get(2)?,
        repository_path: row.get(3)?,
        canonical_repository_path: row.get(4)?,
        default_branch: row.get(5)?,
        remote_url: row.get(6)?,
        created_at: row.get(7)?,
        last_activity_at: row.get(8)?,
    })
}

fn worktree_from_row(row: &Row<'_>) -> rusqlite::Result<Worktree> {
    Ok(Worktree {
        id: parse_text(row, 0)?,
        project_id: parse_text(row, 1)?,
        name: row.get(2)?,
        path: row.get(3)?,
        canonical_path: row.get(4)?,
        branch: row.get(5)?,
        base_ref: row.get(6)?,
        base_commit: row.get(7)?,
        is_root_checkout: row.get::<_, i64>(8)? != 0,
        status: parse_text(row, 9)?,
        created_at: row.get(10)?,
        last_activity_at: row.get(11)?,
        removed_at: row.get(12)?,
    })
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: parse_text(row, 0)?,
        worktree_id: parse_text(row, 1)?,
        provider_profile_id: parse_optional_text(row, 2)?,
        provider_kind: parse_text(row, 3)?,
        display_name: row.get(4)?,
        state: parse_text(row, 5)?,
        process_id: checked_optional(row, 6)?,
        external_session_id: row.get(7)?,
        command: row.get(8)?,
        arguments_json: row.get(9)?,
        cwd: row.get(10)?,
        created_at: row.get(11)?,
        started_at: row.get(12)?,
        ended_at: row.get(13)?,
        last_activity_at: row.get(14)?,
        last_seen_output_sequence: checked_integer(row, 15)?,
        exit_code: row.get(16)?,
        failure_reason: row.get(17)?,
    })
}

fn provider_from_row(row: &Row<'_>) -> rusqlite::Result<ProviderProfile> {
    Ok(ProviderProfile {
        id: parse_text(row, 0)?,
        kind: parse_text(row, 1)?,
        display_name: row.get(2)?,
        executable_path: row.get(3)?,
        default_model: row.get(4)?,
        default_effort: row.get(5)?,
        enabled: row.get::<_, i64>(6)? != 0,
        capabilities_json: row.get(7)?,
        last_probe_status: row.get(8)?,
        last_probe_at: row.get(9)?,
    })
}

fn parse_text<T>(row: &Row<'_>, column: usize) -> rusqlite::Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    let value: String = row.get(column)?;
    value
        .parse()
        .map_err(|error: T::Err| invalid_value(column, error.to_string()))
}

fn parse_optional_text<T>(row: &Row<'_>, column: usize) -> rusqlite::Result<Option<T>>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    row.get::<_, Option<String>>(column)?
        .map(|value| {
            value
                .parse()
                .map_err(|error: T::Err| invalid_value(column, error.to_string()))
        })
        .transpose()
}

fn checked_integer<T>(row: &Row<'_>, column: usize) -> rusqlite::Result<T>
where
    T: TryFrom<i64>,
{
    let value: i64 = row.get(column)?;
    T::try_from(value).map_err(|_| invalid_value(column, format!("out-of-range integer {value}")))
}

fn checked_optional<T>(row: &Row<'_>, column: usize) -> rusqlite::Result<Option<T>>
where
    T: TryFrom<i64>,
{
    row.get::<_, Option<i64>>(column)?
        .map(|value| {
            T::try_from(value)
                .map_err(|_| invalid_value(column, format!("out-of-range integer {value}")))
        })
        .transpose()
}

fn invalid_value(column: usize, message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        Type::Text,
        Box::new(io::Error::new(io::ErrorKind::InvalidData, message)),
    )
}

fn checksum(source: &str) -> String {
    let digest = Sha256::digest(source.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a string cannot fail");
    }
    encoded
}

fn now_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[allow(clippy::needless_pass_by_value)]
fn database_error(error: rusqlite::Error) -> DaemonError {
    DaemonError::Database(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sylvops_core::ui::{DesktopDensity, DesktopState, DesktopTheme, MainTab};

    #[tokio::test]
    async fn migrates_and_returns_empty_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let database = DatabaseHandle::open(&directory.path().join("state.db")).unwrap();
        let snapshot = database.snapshot().await.unwrap();
        assert_eq!(snapshot.revision, 1);
        assert!(snapshot.workspaces.is_empty());
        database.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn tui_state_round_trips_without_advancing_revision() {
        let directory = tempfile::tempdir().unwrap();
        let database = DatabaseHandle::open(&directory.path().join("state.db")).unwrap();
        let revision = database.snapshot().await.unwrap().revision;
        let state = TuiState {
            selected_project_id: Some(ProjectId::new()),
            selected_worktree_id: Some(WorktreeId::new()),
            selected_session_id: Some(SessionId::new()),
            selected_main_tab: MainTab::Changes,
        };
        database.save_tui_state(state.clone()).await.unwrap();
        assert_eq!(database.tui_state().await.unwrap(), Some(state));
        assert_eq!(database.snapshot().await.unwrap().revision, revision);
        database.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn corrupt_tui_state_is_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let database = DatabaseHandle::open(&path).unwrap();
        database.shutdown().await.unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO ui_state(client_scope, key, value_json, updated_at) \
                 VALUES ('tui', 'navigation.v1', 'not-json', 1)",
                [],
            )
            .unwrap();
        drop(connection);
        let database = DatabaseHandle::open(&path).unwrap();
        assert_eq!(database.tui_state().await.unwrap(), None);
        database.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn desktop_state_round_trips_without_advancing_revision() {
        let directory = tempfile::tempdir().unwrap();
        let database = DatabaseHandle::open(&directory.path().join("state.db")).unwrap();
        let revision = database.snapshot().await.unwrap().revision;
        let state = DesktopState {
            selected_project_id: Some(ProjectId::new()),
            selected_worktree_id: Some(WorktreeId::new()),
            selected_session_id: Some(SessionId::new()),
            selected_main_tab: MainTab::Details,
            theme: DesktopTheme::Nord,
            density: DesktopDensity::Compact,
            terminal_font_size: 17,
            ..DesktopState::default()
        };
        database.save_desktop_state(state.clone()).await.unwrap();
        assert_eq!(database.desktop_state().await.unwrap(), Some(state));
        assert_eq!(database.snapshot().await.unwrap().revision, revision);
        database.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn corrupt_desktop_state_is_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let database = DatabaseHandle::open(&path).unwrap();
        database.shutdown().await.unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO ui_state(client_scope, key, value_json, updated_at) \
                 VALUES ('desktop', 'navigation.v1', 'not-json', 1)",
                [],
            )
            .unwrap();
        drop(connection);
        let database = DatabaseHandle::open(&path).unwrap();
        assert_eq!(database.desktop_state().await.unwrap(), None);
        database.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn restart_reconciliation_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let database = DatabaseHandle::open(&path).unwrap();
        assert_eq!(database.reconcile_after_restart().await.unwrap(), 0);
        assert_eq!(database.reconcile_after_restart().await.unwrap(), 0);
        database.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn opening_a_workspace_is_exclusive_and_revisioned() {
        let directory = tempfile::tempdir().unwrap();
        let database = DatabaseHandle::open(&directory.path().join("state.db")).unwrap();
        let (_, first) = database.add_workspace("First".into()).await.unwrap();
        let (second_revision, second) = database.add_workspace("Second".into()).await.unwrap();
        let initial_order: Vec<_> = database
            .snapshot()
            .await
            .unwrap()
            .workspaces
            .into_iter()
            .map(|workspace| workspace.id)
            .collect();
        let (opened_revision, opened) = database.open_workspace(first.id).await.unwrap();
        assert!(opened_revision > second_revision);
        assert_eq!(opened.id, first.id);
        let snapshot = database.snapshot().await.unwrap();
        assert_eq!(
            snapshot
                .workspaces
                .iter()
                .map(|workspace| workspace.id)
                .collect::<Vec<_>>(),
            initial_order,
            "opening a workspace must not move its desktop tab"
        );
        assert_eq!(
            snapshot
                .workspaces
                .iter()
                .filter(|workspace| workspace.is_open)
                .count(),
            1
        );
        assert!(
            snapshot
                .workspaces
                .iter()
                .any(|workspace| workspace.id == first.id && workspace.is_open)
        );
        assert!(
            snapshot
                .workspaces
                .iter()
                .any(|workspace| workspace.id == second.id && !workspace.is_open)
        );
        database.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn audit_events_validate_json_and_persist() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let database = DatabaseHandle::open(&path).unwrap();
        database
            .audit("daemon_tested", "succeeded", r#"{"bounded":true}"#)
            .await
            .unwrap();
        assert!(
            database
                .audit("daemon_tested", "succeeded", "not-json")
                .await
                .is_err()
        );
        database.shutdown().await.unwrap();

        let connection = Connection::open(path).unwrap();
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM audit_events WHERE action = 'daemon_tested'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn entity_mutations_are_transactional_and_revisioned() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("state.db");
        let database = DatabaseHandle::open(&database_path).unwrap();
        let (workspace_revision, workspace) = database
            .add_workspace("Test workspace".into())
            .await
            .unwrap();
        let repository = directory.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        let repository = repository.to_string_lossy().into_owned();
        let registration = RepositoryRegistration {
            workspace_id: workspace.id,
            name: "repository".into(),
            repository_path: repository.clone(),
            canonical_repository_path: repository.clone(),
            default_branch: Some("main".into()),
            remote_url: None,
            branch: Some("main".into()),
            base_commit: "deadbeef".into(),
        };
        let (project_revision, project, _) =
            database.add_project(registration.clone()).await.unwrap();
        assert!(database.add_project(registration).await.is_err());
        let after_duplicate = database.snapshot().await.unwrap();
        assert_eq!(after_duplicate.projects.len(), 1);
        assert_eq!(after_duplicate.worktrees.len(), 1);
        let managed_path = directory.path().join("managed-worktree");
        std::fs::create_dir(&managed_path).unwrap();
        let managed_path = managed_path.to_string_lossy().into_owned();
        let managed_id = WorktreeId::new();
        let (worktree_revision, worktree) = database
            .add_worktree(NewManagedWorktree {
                id: managed_id,
                project_id: project.id,
                name: "managed".into(),
                path: managed_path.clone(),
                canonical_path: managed_path.clone(),
                branch: "feature/managed".into(),
                base_ref: "main".into(),
                base_commit: "deadbeef".into(),
            })
            .await
            .unwrap();
        assert!(
            !database
                .worktree_has_live_sessions(managed_id)
                .await
                .unwrap()
        );
        let session_id = SessionId::new();
        let session = database
            .create_session(NewSession {
                id: session_id,
                worktree_id: worktree.id,
                display_name: "shell".into(),
                provider_profile_id: None,
                provider_kind: sylvops_core::domain::ProviderKind::Shell,
                command: "shell".into(),
                arguments_json: "[]".into(),
                cwd: managed_path,
                initial_prompt: None,
                external_session_id: None,
            })
            .await
            .unwrap();
        assert_eq!(session.state, SessionState::Starting);
        assert_eq!(
            database.snapshot().await.unwrap().revision,
            worktree_revision
        );
        let (running_revision, session) =
            database.mark_session_running(session_id, 42).await.unwrap();
        assert_eq!(session.state, SessionState::Running);
        let (project_renamed_revision, renamed_project) = database
            .rename_project(project.id, "Renamed project".into())
            .await
            .unwrap();
        assert_eq!(renamed_project.name, "Renamed project");
        let (worktree_renamed_revision, renamed_worktree) = database
            .rename_worktree(worktree.id, "Renamed worktree".into())
            .await
            .unwrap();
        assert_eq!(renamed_worktree.name, "Renamed worktree");
        let (session_renamed_revision, renamed_session) = database
            .rename_session(session_id, "Renamed session".into())
            .await
            .unwrap();
        assert_eq!(renamed_session.display_name, "Renamed session");
        assert!(
            database
                .worktree_has_live_sessions(managed_id)
                .await
                .unwrap()
        );
        let (finished_revision, session) = database
            .finish_session(session_id, SessionState::FinishedUnseen, Some(0), None)
            .await
            .unwrap();
        assert_eq!(session.state, SessionState::FinishedUnseen);
        assert!(
            !database
                .worktree_has_live_sessions(managed_id)
                .await
                .unwrap()
        );
        let (seen_revision, session) = database
            .mark_session_seen(session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session.state, SessionState::FinishedSeen);
        assert!(
            database
                .mark_session_seen(session_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(workspace_revision < project_revision);
        assert!(project_revision < worktree_revision);
        assert!(worktree_revision < running_revision);
        assert!(running_revision < project_renamed_revision);
        assert!(project_renamed_revision < worktree_renamed_revision);
        assert!(worktree_renamed_revision < session_renamed_revision);
        assert!(session_renamed_revision < finished_revision);
        assert!(finished_revision < seen_revision);
        let (removed_revision, removed) = database.mark_worktree_removed(managed_id).await.unwrap();
        assert_eq!(removed.status, WorktreeStatus::Removed);
        assert!(seen_revision < removed_revision);
        assert_eq!(
            database.snapshot().await.unwrap().revision,
            removed_revision
        );
        database.shutdown().await.unwrap();

        let connection = Connection::open(database_path).unwrap();
        let worktree_audits: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM audit_events WHERE action IN \
                 ('worktree_created', 'worktree_removed')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(worktree_audits, 2);
    }

    #[test]
    fn rejects_a_newer_schema_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_migrations (\
                 version INTEGER PRIMARY KEY, \
                 name TEXT NOT NULL, \
                 applied_at INTEGER NOT NULL, \
                 checksum TEXT NOT NULL\
                 ) STRICT; \
                 INSERT INTO schema_migrations VALUES (99, 'future', 0, 'future');",
            )
            .unwrap();
        drop(connection);

        let error = DatabaseHandle::open(&path).unwrap_err();
        assert!(error.to_string().contains("newer than supported"));
    }
}
