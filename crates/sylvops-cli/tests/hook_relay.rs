use std::time::Duration;

use sylvops_core::{
    domain::ProviderKind,
    ids::{SessionId, WorktreeId},
};
use sylvops_daemon::hook::HookReceiver;
use tokio::{io::AsyncWriteExt, process::Command};

#[tokio::test]
async fn production_cli_relays_authenticated_hooks_without_initializing_state() {
    exercise_production_relay(false).await;
}

#[cfg(windows)]
#[tokio::test]
async fn production_cli_relay_delivers_through_powershell() {
    exercise_production_relay(true).await;
}

async fn exercise_production_relay(powershell: bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("unused-state");
        let executable = std::path::PathBuf::from(env!("CARGO_BIN_EXE_sylvops"));
        let mut receiver = HookReceiver::bind(executable.clone(), 64 * 1024, 600)
            .await
            .unwrap();
        let mut deliveries = receiver.take_deliveries();
        let session_id = SessionId::new();
        let worktree_id = WorktreeId::new();
        let endpoint = receiver
            .register(ProviderKind::Codex, session_id, worktree_id)
            .unwrap();
        let mut command = Command::new(&executable);
        command
            .arg("--state-dir")
            .arg(&state)
            .args(["hook", "emit"]);
        #[cfg(windows)]
        if powershell {
            let shell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
                .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
            command = Command::new(shell);
            let executable = executable
                .to_string_lossy()
                .replace('`', "``")
                .replace('$', "`$");
            let state = state
                .to_string_lossy()
                .replace('`', "``")
                .replace('$', "`$");
            command
                .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
                .arg(format!(
                    "& \"{executable}\" --state-dir \"{state}\" hook emit"
                ));
        }
        #[cfg(not(windows))]
        assert!(!powershell);
        command
            .env("SYLVOPS_HOOK_ENDPOINT", endpoint.url())
            .env("SYLVOPS_HOOK_TOKEN", endpoint.bearer_token())
            .env("SYLVOPS_SESSION_ID", session_id.to_string())
            .env("SYLVOPS_WORKTREE_ID", worktree_id.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        let mut child = command.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        input
            .write_all(br#"{"hook_event_name":"SessionStart","session_id":"relay-conversation"}"#)
            .await
            .unwrap();
        drop(input);
        assert!(child.wait().await.unwrap().success());
        let delivery = deliveries.recv().await.unwrap();
        assert_eq!(delivery.session_id, session_id);
        assert_eq!(delivery.worktree_id, worktree_id);
        assert_eq!(delivery.provider, ProviderKind::Codex);
        assert!(
            !state.exists(),
            "hook relay must not initialize application state"
        );
        receiver.shutdown().await;
    })
    .await
    .expect("production relay timeout");
}
