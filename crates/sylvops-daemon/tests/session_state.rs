use std::path::Path;

use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use sylvops_core::{
    domain::{ProviderKind, SessionState},
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    session_state::{
        AttentionAcknowledgement, AttentionReason, ProviderActivity, SessionLifecycle,
    },
};
use sylvops_daemon::database::{DatabaseHandle, NewSession, RepositoryRegistration};

const LEGACY_MIGRATIONS: &[(&str, &str)] = &[
    ("initial", include_str!("../migrations/0001_initial.sql")),
    (
        "stop_persisting_codex_prompts",
        include_str!("../migrations/0002_stop_persisting_codex_prompts.sql"),
    ),
    (
        "session_resumptions",
        include_str!("../migrations/0003_session_resumptions.sql"),
    ),
    (
        "desktop_signature_themes",
        include_str!("../migrations/0004_desktop_signature_themes.sql"),
    ),
    (
        "project_removal_generation",
        include_str!("../migrations/0005_project_removal_generation.sql"),
    ),
    (
        "workspace_deletion_generation",
        include_str!("../migrations/0006_workspace_deletion_generation.sql"),
    ),
    (
        "project_github_associations",
        include_str!("../migrations/0007_project_github_associations.sql"),
    ),
    (
        "work_sources",
        include_str!("../migrations/0008_work_sources.sql"),
    ),
];

fn legacy_database(path: &Path, timestamp: i64) -> Vec<(SessionId, bool, bool)> {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL,
         applied_at INTEGER NOT NULL, checksum TEXT NOT NULL) STRICT;",
        )
        .unwrap();
    for (index, (name, source)) in LEGACY_MIGRATIONS.iter().enumerate() {
        connection.execute_batch(source).unwrap();
        let checksum = format!("{:x}", Sha256::digest(source.as_bytes()));
        connection
            .execute(
                "INSERT INTO schema_migrations VALUES (?1, ?2, 0, ?3)",
                params![i64::try_from(index + 1).unwrap(), name, checksum],
            )
            .unwrap();
    }
    let workspace = WorkspaceId::new().to_string();
    let project = ProjectId::new().to_string();
    let worktree = WorktreeId::new().to_string();
    connection
        .execute(
            "INSERT INTO workspaces(id, name, created_at, updated_at, is_open)
        VALUES (?1, 'Legacy', 0, 0, 1)",
            [&workspace],
        )
        .unwrap();
    connection.execute(
        "INSERT INTO projects(id, workspace_id, name, repository_path, canonical_repository_path,
         created_at, last_activity_at) VALUES (?1, ?2, 'Legacy', 'repo', 'repo', 0, 0)",
        params![project, workspace],
    ).unwrap();
    connection.execute(
        "INSERT INTO worktrees(id, project_id, name, path, canonical_path, base_ref, base_commit,
         is_root_checkout, status, created_at, last_activity_at)
         VALUES (?1, ?2, 'Legacy', 'repo', 'repo', 'main', 'deadbeef', 1, 'active', 0, 0)",
        params![worktree, project],
    ).unwrap();
    let mut records = Vec::new();
    for unseen in [false, true] {
        for (pid, identity) in [
            (None, None),
            (Some(42), None),
            (None, Some("old identity")),
            (Some(42), Some("old identity")),
        ] {
            let id = SessionId::new();
            connection
                .execute(
                    "INSERT INTO sessions(id, worktree_id, provider_kind, display_name, state,
                 process_id, process_identity, command, cwd, created_at, last_activity_at)
                 VALUES (?1, ?2, 'codex', 'Legacy', ?3, ?4, ?5, 'codex', 'repo', 0, ?6)",
                    params![
                        id.to_string(),
                        worktree,
                        if unseen {
                            "finished_unseen"
                        } else {
                            "finished_seen"
                        },
                        pid,
                        identity,
                        timestamp
                    ],
                )
                .unwrap();
            records.push((id, unseen, pid.is_some() || identity.is_some()));
        }
    }
    records
}

#[tokio::test]
async fn legacy_completion_migrates_without_adopting_any_process_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let records = legacy_database(&path, 100);
    let database = DatabaseHandle::open(&path).unwrap();
    database.reconcile_after_restart().await.unwrap();
    let snapshot = database.snapshot().await.unwrap();
    for (id, unseen, had_identity) in records {
        let session = snapshot
            .sessions
            .iter()
            .find(|session| session.id == id)
            .unwrap();
        assert_eq!(session.process_id, None);
        assert_eq!(
            session.status.lifecycle,
            if had_identity {
                SessionLifecycle::Disconnected
            } else {
                SessionLifecycle::Exited
            }
        );
        assert_eq!(session.status.activity, ProviderActivity::None);
        let event = session.status.attention.unwrap();
        assert_eq!(event.reason, AttentionReason::SuccessfulCompletion);
        assert_eq!(u64::from(event.id), 1);
        assert_eq!(event.occurred_at, 100);
        assert_eq!(
            event.acknowledgement,
            if unseen {
                AttentionAcknowledgement::Pending
            } else {
                AttentionAcknowledgement::Acknowledged
            }
        );
    }
    database.shutdown().await.unwrap();
    let database = DatabaseHandle::open(&path).unwrap();
    assert_eq!(
        database.snapshot().await.unwrap().sessions,
        snapshot.sessions
    );
    database.shutdown().await.unwrap();
}

#[test]
fn invalid_legacy_state_rolls_back_the_entire_expansion() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    legacy_database(&path, -1);
    assert!(DatabaseHandle::open(&path).is_err());
    let connection = Connection::open(&path).unwrap();
    let version: i64 = connection
        .query_row("SELECT max(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(version, 8);
    assert!(
        connection
            .prepare("SELECT status_json FROM sessions")
            .is_err()
    );
}

#[tokio::test]
async fn legacy_feedback_preserves_input_and_recoverable_failure_as_distinct_reasons() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let records = legacy_database(&path, 100);
    let connection = Connection::open(&path).unwrap();
    for (index, failure) in [None, Some("bounded legacy failure")]
        .into_iter()
        .enumerate()
    {
        connection
            .execute(
                "UPDATE sessions SET state = 'needs_feedback', failure_reason = ?2 WHERE id = ?1",
                params![records[index].0.to_string(), failure],
            )
            .unwrap();
    }
    drop(connection);
    let database = DatabaseHandle::open(&path).unwrap();
    for (index, reason) in [
        AttentionReason::InputRequested,
        AttentionReason::RecoverableFailure,
    ]
    .into_iter()
    .enumerate()
    {
        let session = database.session(records[index].0).await.unwrap();
        assert_eq!(session.status.lifecycle, SessionLifecycle::Disconnected);
        assert_eq!(session.status.attention.unwrap().reason, reason);
        assert_eq!(
            session.status.attention.unwrap().acknowledgement,
            AttentionAcknowledgement::Pending
        );
    }
    database.shutdown().await.unwrap();
}

#[tokio::test]
async fn pending_live_provider_completion_survives_restart_without_adopting_the_process() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    let database = DatabaseHandle::open(&path).unwrap();
    let (_, workspace) = database.add_workspace("Test".into()).await.unwrap();
    let (_, _, worktree) = database
        .add_project(RepositoryRegistration {
            workspace_id: workspace.id,
            name: "Test".into(),
            repository_path: "repo".into(),
            canonical_repository_path: "repo".into(),
            default_branch: None,
            remote_url: None,
            branch: None,
            base_commit: "deadbeef".into(),
        })
        .await
        .unwrap();
    let id = SessionId::new();
    let mut record = NewSession {
        id,
        worktree_id: worktree.id,
        display_name: "Codex".into(),
        provider_profile_id: None,
        provider_kind: ProviderKind::Codex,
        command: "codex".into(),
        arguments_json: "[]".into(),
        cwd: "repo".into(),
        external_session_id: None,
        resumed_from_session_id: None,
    };
    database.create_session(record.clone()).await.unwrap();
    database.mark_session_running(id, 42).await.unwrap();
    let (_, completed) = database
        .update_session_status(id, SessionState::FinishedUnseen, None)
        .await
        .unwrap();
    assert_eq!(completed.status.lifecycle, SessionLifecycle::Running);
    assert_eq!(completed.status.activity, ProviderActivity::Completed);
    let pending = completed.status.attention.unwrap();
    assert_eq!(pending.acknowledgement, AttentionAcknowledgement::Pending);
    record.id = SessionId::new();
    let acknowledged_id = record.id;
    database.create_session(record).await.unwrap();
    database
        .mark_session_running(acknowledged_id, 43)
        .await
        .unwrap();
    database
        .update_session_status(acknowledged_id, SessionState::FinishedUnseen, None)
        .await
        .unwrap();
    let (_, acknowledged) = database
        .mark_session_seen(acknowledged_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(acknowledged.status.lifecycle, SessionLifecycle::Running);
    assert_eq!(acknowledged.status.activity, ProviderActivity::Completed);
    assert_eq!(
        acknowledged.status.attention.unwrap().acknowledgement,
        AttentionAcknowledgement::Acknowledged
    );
    database.shutdown().await.unwrap();

    let database = DatabaseHandle::open(&path).unwrap();
    database.reconcile_after_restart().await.unwrap();
    let recovered = database.session(id).await.unwrap();
    assert_eq!(recovered.state, SessionState::Disconnected);
    assert_eq!(recovered.status.lifecycle, SessionLifecycle::Disconnected);
    assert_eq!(recovered.status.activity, ProviderActivity::None);
    assert_eq!(recovered.process_id, None);
    assert_eq!(recovered.status.attention, Some(pending));
    let recovered_acknowledged = database.session(acknowledged_id).await.unwrap();
    assert_eq!(
        recovered_acknowledged.status.attention,
        acknowledged.status.attention
    );
    assert_eq!(
        recovered_acknowledged.status.lifecycle,
        SessionLifecycle::Disconnected
    );
    database.shutdown().await.unwrap();
}

#[tokio::test]
async fn invalid_persisted_dimensions_fail_closed_before_startup() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("state.db");
    legacy_database(&path, 100);
    let database = DatabaseHandle::open(&path).unwrap();
    database.shutdown().await.unwrap();
    for status in [
        r#"{"lifecycle":"future","activity":"none","attention":null}"#,
        r#"{"lifecycle":"exited","activity":"working","attention":null}"#,
        r#"{"lifecycle":"running","activity":"completed","attention":null}"#,
        r#"{"lifecycle":"running","activity":"working","attention":null}"#,
    ] {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute("UPDATE sessions SET status_json = ?1", [status])
            .unwrap();
        drop(connection);
        assert!(DatabaseHandle::open(&path).is_err());
    }
}
