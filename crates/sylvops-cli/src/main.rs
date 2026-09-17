use std::{
    fs::OpenOptions,
    io::{Write, stdout},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    time::Duration,
};

use clap::{Parser, Subcommand};
use crossterm::{
    cursor::MoveTo,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use sylvops_core::{
    domain::{ProviderKind, SessionState},
    ids::{ProjectId, SessionId, WorkspaceId, WorktreeId},
    protocol::{ClientRequest, DaemonEvent, DaemonResponse},
};
use sylvops_daemon::{DaemonError, client::DaemonClient, runtime::RuntimePaths};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "sylvops",
    version,
    about = "Local-first mission control for coding-agent sessions"
)]
struct Arguments {
    /// Override all `SylvOps` state paths (primarily for tests and development).
    #[arg(long, global = true, value_name = "DIRECTORY")]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage the authoritative background daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Print the daemon's current persisted entity snapshot as JSON.
    Snapshot,
    /// Manage workspaces.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// Register local Git projects.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Create, inspect, and safely remove managed Git worktrees.
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
    /// Manage daemon-owned terminal sessions.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Inspect built-in provider availability and authentication.
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    /// Open the hierarchical mission-control interface.
    Tui,
    /// Register or select a repository and open mission control.
    Open {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Create or reuse this workspace instead of the active workspace.
        #[arg(long)]
        workspace: Option<String>,
        /// Explicitly launch a session in the root checkout.
        #[arg(long)]
        provider: Option<ProviderKind>,
        #[arg(long, requires = "provider")]
        model: Option<String>,
        #[arg(long, requires = "provider")]
        effort: Option<String>,
        #[arg(long, requires = "provider")]
        prompt: Option<String>,
    },
    /// Run redacted installation, daemon, Git, provider, and PTY checks.
    Doctor,
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    /// Create a workspace.
    Add { name: String },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// Register a repository and its root checkout.
    Add {
        #[arg(long)]
        workspace: WorkspaceId,
        path: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum WorktreeCommand {
    /// Create a new branch in an isolated managed worktree.
    Create {
        #[arg(long)]
        project: ProjectId,
        #[arg(long)]
        branch: String,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Refresh and print tracked, untracked, and ignored state.
    Status { worktree_id: WorktreeId },
    /// Remove an exactly verified clean worktree while preserving its branch.
    Remove {
        worktree_id: WorktreeId,
        #[arg(long)]
        confirm: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// Create an interactive provider session in an active checkout.
    Create {
        #[arg(long)]
        worktree: WorktreeId,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, default_value = "shell")]
        provider: ProviderKind,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        #[arg(long, default_value_t = 80)]
        columns: u16,
        #[arg(long, default_value_t = 24)]
        rows: u16,
    },
    /// Attach interactively. Press Ctrl+] to detach without stopping the shell.
    Attach { session_id: SessionId },
    /// Resume an inactive provider session using its verified external ID.
    Resume {
        session_id: SessionId,
        #[arg(long, default_value_t = 80)]
        columns: u16,
        #[arg(long, default_value_t = 24)]
        rows: u16,
    },
    /// Stop the session and its complete process tree.
    Stop { session_id: SessionId },
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    List,
    Probe { provider: ProviderKind },
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    Emit,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Start the daemon in the background and wait until it is healthy.
    Start,
    /// Run the daemon in the foreground.
    Run,
    /// Print daemon health and protocol information.
    Status,
    /// Ask the daemon to shut down cleanly.
    Stop,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let Arguments { state_dir, command } = Arguments::parse();
    let paths = RuntimePaths::discover(state_dir.as_deref())?;
    let command = command.unwrap_or(Command::Tui);

    match command {
        Command::Daemon { command } => match command {
            DaemonCommand::Start => start_daemon(&paths, state_dir.as_deref()).await?,
            DaemonCommand::Run => sylvops_daemon::daemon::run(paths).await?,
            DaemonCommand::Status => show_status(&paths).await?,
            DaemonCommand::Stop => stop_daemon(&paths).await?,
        },
        Command::Snapshot => show_snapshot(&paths).await?,
        Command::Workspace { command } => match command {
            WorkspaceCommand::Add { name } => add_workspace(&paths, name).await?,
        },
        Command::Project { command } => match command {
            ProjectCommand::Add { workspace, path } => add_project(&paths, workspace, path).await?,
        },
        Command::Worktree { command } => match command {
            WorktreeCommand::Create {
                project,
                branch,
                base,
                name,
            } => create_worktree(&paths, project, branch, base, name).await?,
            WorktreeCommand::Status { worktree_id } => {
                show_worktree_status(&paths, worktree_id).await?;
            }
            WorktreeCommand::Remove {
                worktree_id,
                confirm,
            } => remove_worktree(&paths, worktree_id, confirm).await?,
        },
        Command::Session { command } => match command {
            SessionCommand::Create {
                worktree,
                name,
                provider,
                model,
                effort,
                prompt,
                columns,
                rows,
            } => {
                create_session(
                    &paths, worktree, name, provider, model, effort, prompt, columns, rows,
                )
                .await?;
            }
            SessionCommand::Attach { session_id } => attach_session(&paths, session_id).await?,
            SessionCommand::Resume {
                session_id,
                columns,
                rows,
            } => resume_session(&paths, session_id, columns, rows).await?,
            SessionCommand::Stop { session_id } => stop_session(&paths, session_id).await?,
        },
        Command::Provider { command } => match command {
            ProviderCommand::List => list_providers(&paths).await?,
            ProviderCommand::Probe { provider } => probe_provider(&paths, provider).await?,
        },
        Command::Tui => run_tui(&paths, state_dir.as_deref()).await?,
        Command::Open {
            path,
            workspace,
            provider,
            model,
            effort,
            prompt,
        } => {
            open_repository(
                &paths,
                state_dir.as_deref(),
                path,
                workspace,
                provider,
                model,
                effort,
                prompt,
            )
            .await?;
        }
        Command::Doctor => doctor(&paths).await?,
        Command::Hook {
            command: HookCommand::Emit,
        } => sylvops_daemon::hook::emit_from_environment()?,
    }

    Ok(())
}

async fn start_daemon(
    paths: &RuntimePaths,
    state_dir: Option<&Path>,
) -> sylvops_daemon::Result<()> {
    if let Ok(client) = DaemonClient::connect(paths, "sylvops-cli").await
        && let Ok(DaemonResponse::Health(health)) = client.request(&ClientRequest::Health).await
    {
        println!(
            "SylvOps daemon is already running (pid {})",
            health.process_id
        );
        return Ok(());
    }

    paths.prepare()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.daemon_log)
        .map_err(|error| DaemonError::Lifecycle(format!("failed to open daemon log: {error}")))?;
    let stderr = log.try_clone().map_err(|error| {
        DaemonError::Lifecycle(format!("failed to duplicate daemon log handle: {error}"))
    })?;

    let executable = std::env::current_exe().map_err(|error| {
        DaemonError::Lifecycle(format!("failed to resolve current executable: {error}"))
    })?;
    let mut command = ProcessCommand::new(executable);
    if let Some(root) = state_dir {
        command.arg("--state-dir").arg(root);
    }
    command
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    configure_detached_process(&mut command);

    let mut child = command.spawn().map_err(|error| {
        DaemonError::Lifecycle(format!("failed to start daemon process: {error}"))
    })?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(client) = DaemonClient::connect(paths, "sylvops-cli").await
            && let Ok(DaemonResponse::Health(health)) = client.request(&ClientRequest::Health).await
        {
            if health.process_id == child.id() {
                println!("SylvOps daemon started (pid {})", child.id());
            } else {
                println!(
                    "SylvOps daemon is already running (pid {})",
                    health.process_id
                );
            }
            return Ok(());
        }

        if let Some(status) = child.try_wait().map_err(|error| {
            DaemonError::Lifecycle(format!("failed to inspect daemon process: {error}"))
        })? {
            return Err(DaemonError::Lifecycle(format!(
                "daemon exited before becoming ready ({status}); see {}",
                paths.daemon_log.display()
            )));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(DaemonError::Lifecycle(format!(
                "daemon did not become ready within 10 seconds; see {}",
                paths.daemon_log.display()
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn show_status(paths: &RuntimePaths) -> sylvops_daemon::Result<()> {
    let client = DaemonClient::connect(paths, "sylvops-cli").await?;
    match client.request(&ClientRequest::Health).await? {
        DaemonResponse::Health(health) => {
            println!("SylvOps daemon is healthy");
            println!("  version: {}", health.daemon_version);
            println!("  process id: {}", health.process_id);
            println!(
                "  protocol: {}.{}",
                health.protocol_major, health.protocol_minor
            );
            println!("  uptime: {} seconds", health.uptime_seconds);
            println!("  connected clients: {}", health.connected_clients);
            println!("  database ready: {}", health.database_ready);
            println!("  database: {}", paths.database.display());
            Ok(())
        }
        response => Err(DaemonError::Lifecycle(format!(
            "unexpected health response: {response:?}"
        ))),
    }
}

async fn stop_daemon(paths: &RuntimePaths) -> sylvops_daemon::Result<()> {
    let client = DaemonClient::connect(paths, "sylvops-cli").await?;
    match client.request(&ClientRequest::ShutdownDaemon).await? {
        DaemonResponse::Acknowledged => {
            println!("SylvOps daemon is shutting down");
            Ok(())
        }
        response => Err(DaemonError::Lifecycle(format!(
            "unexpected shutdown response: {response:?}"
        ))),
    }
}

async fn show_snapshot(paths: &RuntimePaths) -> sylvops_daemon::Result<()> {
    let client = DaemonClient::connect(paths, "sylvops-cli").await?;
    match client.request(&ClientRequest::GetSnapshot).await? {
        DaemonResponse::Snapshot(snapshot) => {
            let rendered = serde_json::to_string_pretty(&snapshot).map_err(|error| {
                DaemonError::Lifecycle(format!("failed to encode snapshot: {error}"))
            })?;
            println!("{rendered}");
            Ok(())
        }
        response => Err(DaemonError::Lifecycle(format!(
            "unexpected snapshot response: {response:?}"
        ))),
    }
}

async fn add_workspace(paths: &RuntimePaths, name: String) -> sylvops_daemon::Result<()> {
    let client = DaemonClient::connect(paths, "sylvops-cli").await?;
    match client
        .request(&ClientRequest::AddWorkspace { name })
        .await?
    {
        DaemonResponse::WorkspaceAdded { workspace, .. } => {
            println!("Created workspace '{}' ({})", workspace.name, workspace.id);
            Ok(())
        }
        response => unexpected("workspace creation", response),
    }
}

async fn add_project(
    paths: &RuntimePaths,
    workspace_id: WorkspaceId,
    path: PathBuf,
) -> sylvops_daemon::Result<()> {
    let client = DaemonClient::connect(paths, "sylvops-cli").await?;
    let repository_path = path
        .to_str()
        .ok_or_else(|| DaemonError::Git("repository path is not valid Unicode".into()))?
        .to_owned();
    match client
        .request(&ClientRequest::AddProject {
            workspace_id,
            repository_path,
        })
        .await?
    {
        DaemonResponse::ProjectAdded {
            project,
            root_worktree,
            ..
        } => {
            println!("Registered project '{}' ({})", project.name, project.id);
            println!("Root worktree: {}", root_worktree.id);
            Ok(())
        }
        response => unexpected("project registration", response),
    }
}

async fn create_worktree(
    paths: &RuntimePaths,
    project_id: ProjectId,
    branch: String,
    base_ref: Option<String>,
    name: Option<String>,
) -> sylvops_daemon::Result<()> {
    let client = DaemonClient::connect(paths, "sylvops-cli").await?;
    match client
        .request(&ClientRequest::CreateWorktree {
            project_id,
            name,
            branch,
            base_ref,
        })
        .await?
    {
        DaemonResponse::WorktreeCreated { worktree, .. } => {
            println!("Created worktree '{}' ({})", worktree.name, worktree.id);
            println!("Path: {}", worktree.canonical_path);
            println!(
                "Branch: {}",
                worktree.branch.as_deref().unwrap_or("detached")
            );
            Ok(())
        }
        response => unexpected("worktree creation", response),
    }
}

async fn show_worktree_status(
    paths: &RuntimePaths,
    worktree_id: WorktreeId,
) -> sylvops_daemon::Result<()> {
    let client = DaemonClient::connect(paths, "sylvops-cli").await?;
    match client
        .request(&ClientRequest::GetWorktreeStatus { worktree_id })
        .await?
    {
        DaemonResponse::WorktreeStatus(status) => {
            print_worktree_status(&status);
            Ok(())
        }
        response => unexpected("worktree status", response),
    }
}

async fn remove_worktree(
    paths: &RuntimePaths,
    worktree_id: WorktreeId,
    confirm: bool,
) -> sylvops_daemon::Result<()> {
    if !confirm {
        return Err(DaemonError::Lifecycle(
            "worktree removal requires the explicit --confirm flag".into(),
        ));
    }
    let client = DaemonClient::connect(paths, "sylvops-cli").await?;
    let status = match client
        .request(&ClientRequest::GetWorktreeStatus { worktree_id })
        .await?
    {
        DaemonResponse::WorktreeStatus(status) => status,
        response => return unexpected("worktree status", response),
    };
    if !status.clean {
        print_worktree_status(&status);
        return Err(DaemonError::Lifecycle(
            "refusing to remove a worktree containing tracked, untracked, or ignored content"
                .into(),
        ));
    }
    let confirmation_token = status.removal_confirmation_token.ok_or_else(|| {
        DaemonError::Lifecycle("daemon did not provide a clean-state confirmation token".into())
    })?;
    match client
        .request(&ClientRequest::RemoveWorktree {
            worktree_id,
            confirmation_token,
        })
        .await?
    {
        DaemonResponse::WorktreeRemoved { worktree, .. } => {
            println!("Removed worktree '{}' ({})", worktree.name, worktree.id);
            println!(
                "Branch preserved: {}",
                worktree.branch.as_deref().unwrap_or("none")
            );
            Ok(())
        }
        response => unexpected("worktree removal", response),
    }
}

fn print_worktree_status(status: &sylvops_core::domain::GitWorktreeState) {
    println!("Worktree: {}", status.worktree_id);
    println!("HEAD: {}", status.head_commit);
    println!("Branch: {}", status.branch.as_deref().unwrap_or("detached"));
    println!("Clean: {}", status.clean);
    println!("Tracked changes: {}", status.tracked_changes);
    println!("Untracked files: {}", status.untracked_files);
    println!("Ignored files: {}", status.ignored_files);
}

#[allow(clippy::too_many_arguments)]
async fn create_session(
    paths: &RuntimePaths,
    worktree_id: WorktreeId,
    display_name: Option<String>,
    provider: ProviderKind,
    model: Option<String>,
    effort: Option<String>,
    initial_prompt: Option<String>,
    columns: u16,
    rows: u16,
) -> sylvops_daemon::Result<()> {
    let client = DaemonClient::connect(paths, "sylvops-cli").await?;
    match client
        .request(&ClientRequest::CreateSession {
            worktree_id,
            provider,
            display_name,
            model,
            effort,
            initial_prompt,
            columns,
            rows,
        })
        .await?
    {
        DaemonResponse::SessionCreated { session, .. } => {
            println!(
                "Created session '{}' ({})",
                session.display_name, session.id
            );
            Ok(())
        }
        response => unexpected("session creation", response),
    }
}

async fn resume_session(
    paths: &RuntimePaths,
    session_id: SessionId,
    columns: u16,
    rows: u16,
) -> sylvops_daemon::Result<()> {
    let client = DaemonClient::connect(paths, "sylvops-cli").await?;
    match client
        .request(&ClientRequest::ResumeSession {
            session_id,
            columns,
            rows,
        })
        .await?
    {
        DaemonResponse::SessionResumed { session, .. } => {
            println!("Resumed '{}' as {}", session.display_name, session.id);
            Ok(())
        }
        response => unexpected("session resume", response),
    }
}

async fn list_providers(paths: &RuntimePaths) -> sylvops_daemon::Result<()> {
    let client = DaemonClient::connect(paths, "sylvops-cli").await?;
    match client.request(&ClientRequest::ListProviders).await? {
        DaemonResponse::Providers(providers) => {
            for provider in providers {
                println!(
                    "{}: {}{}",
                    provider.kind,
                    if provider.available {
                        "available"
                    } else {
                        "unavailable"
                    },
                    if provider.available && !provider.authenticated {
                        ", not authenticated"
                    } else {
                        ""
                    }
                );
                if let Some(path) = provider.executable_path {
                    println!("  executable: {path}");
                }
                if let Some(version) = provider.version {
                    println!("  version: {version}");
                }
                if let Some(diagnostic) = provider.diagnostic {
                    println!("  diagnostic: {diagnostic}");
                }
            }
            Ok(())
        }
        response => unexpected("provider listing", response),
    }
}

async fn probe_provider(
    paths: &RuntimePaths,
    provider: ProviderKind,
) -> sylvops_daemon::Result<()> {
    let client = DaemonClient::connect(paths, "sylvops-cli").await?;
    match client
        .request(&ClientRequest::ProbeProvider { kind: provider })
        .await?
    {
        DaemonResponse::Provider(health) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&health).map_err(|error| {
                    DaemonError::Lifecycle(format!("failed to render provider health: {error}"))
                })?
            );
            Ok(())
        }
        response => unexpected("provider probe", response),
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn open_repository(
    paths: &RuntimePaths,
    state_dir: Option<&Path>,
    path: PathBuf,
    workspace_name: Option<String>,
    provider: Option<ProviderKind>,
    model: Option<String>,
    effort: Option<String>,
    prompt: Option<String>,
) -> sylvops_daemon::Result<()> {
    start_daemon(paths, state_dir).await?;
    let client = DaemonClient::connect(paths, "sylvops-open").await?;
    let snapshot = match client.request(&ClientRequest::GetSnapshot).await? {
        DaemonResponse::Snapshot(snapshot) => snapshot,
        response => return unexpected("workspace discovery", response),
    };
    let workspace = if let Some(name) = workspace_name {
        match snapshot
            .workspaces
            .iter()
            .find(|workspace| workspace.name == name)
            .cloned()
        {
            Some(workspace) => workspace,
            None => match client
                .request(&ClientRequest::AddWorkspace { name })
                .await?
            {
                DaemonResponse::WorkspaceAdded { workspace, .. } => workspace,
                response => return unexpected("workspace creation", response),
            },
        }
    } else if let Some(workspace) = snapshot
        .workspaces
        .iter()
        .find(|workspace| workspace.is_open)
        .or_else(|| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.name == "Local")
        })
        .cloned()
    {
        workspace
    } else {
        match client
            .request(&ClientRequest::AddWorkspace {
                name: "Local".into(),
            })
            .await?
        {
            DaemonResponse::WorkspaceAdded { workspace, .. } => workspace,
            response => return unexpected("default workspace creation", response),
        }
    };
    if !workspace.is_open {
        match client
            .request(&ClientRequest::OpenWorkspace {
                workspace_id: workspace.id,
            })
            .await?
        {
            DaemonResponse::WorkspaceOpened { .. } => {}
            response => return unexpected("workspace selection", response),
        }
    }
    let repository_path = path
        .to_str()
        .ok_or_else(|| DaemonError::Git("repository path is not valid Unicode".into()))?
        .to_owned();
    let (project, root_worktree, created) = match client
        .request(&ClientRequest::EnsureProject {
            workspace_id: workspace.id,
            repository_path,
        })
        .await?
    {
        DaemonResponse::ProjectReady {
            project,
            root_worktree,
            created,
            ..
        } => (project, root_worktree, created),
        response => return unexpected("repository selection", response),
    };
    println!(
        "{} project '{}' in workspace '{}'",
        if created { "Registered" } else { "Selected" },
        project.name,
        workspace.name
    );
    if let Some(provider) = provider {
        match client
            .request(&ClientRequest::CreateSession {
                worktree_id: root_worktree.id,
                provider,
                display_name: None,
                model,
                effort,
                initial_prompt: prompt,
                columns: 80,
                rows: 24,
            })
            .await?
        {
            DaemonResponse::SessionCreated { session, .. } => {
                println!("Created session '{}'", session.display_name);
            }
            response => return unexpected("session creation", response),
        }
    }
    drop(client);
    run_tui(paths, state_dir).await
}

async fn doctor(paths: &RuntimePaths) -> sylvops_daemon::Result<()> {
    println!("SylvOps doctor {}", env!("CARGO_PKG_VERSION"));
    paths.prepare()?;
    println!("[ok] local state directory is available");

    let git = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("git")
            .arg("--version")
            .stdin(Stdio::null())
            .output(),
    )
    .await
    .map_err(|_| DaemonError::Lifecycle("Git version check timed out".into()))?
    .map_err(|error| DaemonError::Lifecycle(format!("Git is unavailable: {error}")))?;
    if !git.status.success() || git.stdout.len() > 64 * 1024 || git.stderr.len() > 64 * 1024 {
        return Err(DaemonError::Lifecycle(
            "Git version check failed or produced excessive output".into(),
        ));
    }
    println!("[ok] {}", String::from_utf8_lossy(&git.stdout).trim());

    let client = DaemonClient::connect(paths, "sylvops-doctor")
        .await
        .map_err(|error| {
            DaemonError::Lifecycle(format!(
                "daemon is not reachable; run `sylvops` to start it: {error}"
            ))
        })?;
    match client.request(&ClientRequest::Health).await? {
        DaemonResponse::Health(health) if health.database_ready => println!(
            "[ok] daemon {} uses protocol {}.{}",
            health.daemon_version, health.protocol_major, health.protocol_minor
        ),
        response => return unexpected("daemon health", response),
    }
    match client.request(&ClientRequest::ListProviders).await? {
        DaemonResponse::Providers(providers) => {
            for provider in providers {
                println!(
                    "[{}] provider {}{}",
                    if provider.available { "ok" } else { "--" },
                    provider.kind,
                    if provider.available && !provider.authenticated {
                        " (login required)"
                    } else {
                        ""
                    }
                );
            }
        }
        response => return unexpected("provider diagnostics", response),
    }
    sylvops_daemon::session::run_pty_probe().await?;
    println!("[ok] PTY spawn, output, exit, and cleanup probe passed");
    println!("[ok] secrets and environment values were not printed");
    Ok(())
}

async fn run_tui(paths: &RuntimePaths, state_dir: Option<&Path>) -> sylvops_daemon::Result<()> {
    if DaemonClient::connect(paths, "sylvops-tui-probe")
        .await
        .is_err()
    {
        start_daemon(paths, state_dir).await?;
    }
    let sylvops_tui::TuiExit::Quit = sylvops_tui::run(paths).await?;
    Ok(())
}

async fn stop_session(paths: &RuntimePaths, session_id: SessionId) -> sylvops_daemon::Result<()> {
    let client = DaemonClient::connect(paths, "sylvops-cli").await?;
    match client
        .request(&ClientRequest::StopSession { session_id })
        .await?
    {
        DaemonResponse::Acknowledged => {
            println!("Stopped session {session_id}");
            Ok(())
        }
        response => unexpected("session stop", response),
    }
}

#[allow(clippy::too_many_lines)]
async fn attach_session(paths: &RuntimePaths, session_id: SessionId) -> sylvops_daemon::Result<()> {
    let client = DaemonClient::connect(paths, "sylvops-cli-attach").await?;
    let mut events = client.subscribe();
    let mut connection_closed = client.connection_closed();
    let (columns, rows) = bounded_terminal_size();
    let response = client
        .request(&ClientRequest::AttachSession {
            session_id,
            from_sequence: 0,
            columns,
            rows,
        })
        .await?;
    let DaemonResponse::Attached {
        session,
        role,
        replay_through_sequence,
        terminal_snapshot,
        ..
    } = response
    else {
        return unexpected("session attachment", response);
    };

    let _terminal = TerminalGuard::enter()?;
    let mut parser = vt100::Parser::new(rows, columns, 0);
    let restored_snapshot = terminal_snapshot.is_some();
    if let Some(snapshot) = terminal_snapshot {
        parser.process(&snapshot);
    }
    render_terminal(&parser)?;
    let mut last_sequence = if restored_snapshot {
        replay_through_sequence
    } else {
        0
    };
    let initially_finished = matches!(
        session.state,
        SessionState::FinishedUnseen
            | SessionState::FinishedSeen
            | SessionState::Failed
            | SessionState::Terminated
            | SessionState::Disconnected
    );
    if initially_finished && last_sequence >= replay_through_sequence {
        return Ok(());
    }
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(64);
    std::thread::Builder::new()
        .name("sylvops-terminal-input".into())
        .spawn(move || {
            while !input_tx.is_closed() {
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => return,
                }
                let Ok(input) = event::read() else {
                    return;
                };
                if input_tx.blocking_send(input).is_err() {
                    return;
                }
            }
        })
        .map_err(|error| {
            DaemonError::Lifecycle(format!("failed to start terminal input: {error}"))
        })?;

    if role == sylvops_core::domain::AttachmentRole::Observer {
        let mut output = stdout();
        output.write_all(b"\r\n[read-only observer; detach with Ctrl+]]\r\n")?;
        output.flush()?;
    }
    loop {
        tokio::select! {
            input = input_rx.recv() => {
                let Some(input) = input else { break; };
                match input {
                    Event::Key(key) if is_detach_key(key) => {
                        let _ = client.request(&ClientRequest::DetachSession { session_id }).await;
                        break;
                    }
                    Event::Key(key) if role == sylvops_core::domain::AttachmentRole::Controller => {
                        if let Some(bytes) = encode_key(key) {
                            ensure_ack(client.request(&ClientRequest::SessionInput { session_id, bytes }).await?,
                                "terminal input")?;
                        }
                    }
                    Event::Paste(text) if role == sylvops_core::domain::AttachmentRole::Controller => {
                        for chunk in text.as_bytes().chunks(sylvops_core::protocol::MAX_PTY_CHUNK_SIZE) {
                            ensure_ack(client.request(&ClientRequest::SessionInput {
                                session_id, bytes: chunk.to_vec(),
                            }).await?, "terminal paste")?;
                        }
                    }
                    Event::Resize(columns, rows) if role == sylvops_core::domain::AttachmentRole::Controller => {
                        let (columns, rows) = bounded_dimensions(columns, rows);
                        parser.screen_mut().set_size(rows, columns);
                        ensure_ack(client.request(&ClientRequest::ResizeSession {
                            session_id, columns, rows,
                        }).await?, "terminal resize")?;
                    }
                    _ => {}
                }
            }
            event = events.recv() => match event {
                Ok(DaemonEvent::SessionOutput { session_id: id, sequence, bytes, .. })
                    if id == session_id && sequence > last_sequence => {
                    parser.process(&bytes);
                    last_sequence = sequence;
                    render_terminal(&parser)?;
                    if initially_finished && last_sequence >= replay_through_sequence { break; }
                }
                Ok(DaemonEvent::ResynchronizationRequired {
                    session_id: id, snapshot_sequence, columns, rows, terminal_snapshot,
                }) if id == session_id => {
                    parser = vt100::Parser::new(rows, columns, 0);
                    parser.process(&terminal_snapshot);
                    last_sequence = snapshot_sequence;
                    render_terminal(&parser)?;
                    if initially_finished && last_sequence >= replay_through_sequence { break; }
                }
                Ok(DaemonEvent::SessionExited { session_id: id, .. }) if id == session_id => break,
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    let (columns, rows) = bounded_terminal_size();
                    if let DaemonResponse::Attached {
                        replay_through_sequence,
                        terminal_snapshot: Some(snapshot),
                        ..
                    } = client.request(&ClientRequest::AttachSession {
                        session_id,
                        // A future boundary asks the daemon for its parser snapshot after this
                        // client's local bounded event queue reports loss.
                        from_sequence: u64::MAX,
                        columns,
                        rows,
                    }).await? {
                        parser = vt100::Parser::new(rows, columns, 0);
                        parser.process(&snapshot);
                        last_sequence = replay_through_sequence;
                        render_terminal(&parser)?;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Err(DaemonError::Lifecycle("daemon event stream closed".into()));
                }
            },
            changed = connection_closed.changed() => {
                if changed.is_err() || *connection_closed.borrow() {
                    return Err(DaemonError::Lifecycle("daemon connection closed".into()));
                }
            }
        }
    }
    Ok(())
}

fn render_terminal(parser: &vt100::Parser) -> sylvops_daemon::Result<()> {
    let mut output = stdout();
    execute!(output, MoveTo(0, 0), Clear(ClearType::All))?;
    // This byte stream is generated by the maintained parser, never copied directly from the PTY.
    output.write_all(&parser.screen().contents_formatted())?;
    output.flush()?;
    Ok(())
}

fn bounded_terminal_size() -> (u16, u16) {
    let (columns, rows) = terminal::size().unwrap_or((80, 24));
    bounded_dimensions(columns, rows)
}

fn bounded_dimensions(columns: u16, rows: u16) -> (u16, u16) {
    (
        columns.clamp(
            sylvops_core::protocol::MIN_TERMINAL_COLUMNS,
            sylvops_core::protocol::MAX_TERMINAL_COLUMNS,
        ),
        rows.clamp(
            sylvops_core::protocol::MIN_TERMINAL_ROWS,
            sylvops_core::protocol::MAX_TERMINAL_ROWS,
        ),
    )
}

fn is_detach_key(key: KeyEvent) -> bool {
    key.code == KeyCode::Char(']') && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn encode_key(key: KeyEvent) -> Option<Vec<u8>> {
    let bytes = match key.code {
        KeyCode::Char(character)
            if key.modifiers.contains(KeyModifiers::CONTROL) && character.is_ascii() =>
        {
            let byte = u8::try_from(u32::from(character.to_ascii_uppercase())).ok()?;
            vec![byte & 0x1f]
        }
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        _ => return None,
    };
    Some(bytes)
}

fn ensure_ack(response: DaemonResponse, operation: &str) -> sylvops_daemon::Result<()> {
    match response {
        DaemonResponse::Acknowledged => Ok(()),
        other => unexpected(operation, other),
    }
}

fn unexpected<T>(operation: &str, response: DaemonResponse) -> sylvops_daemon::Result<T> {
    match response {
        DaemonResponse::Error(failure) => Err(DaemonError::Lifecycle(format!(
            "{operation} failed: {}",
            failure.message
        ))),
        other => Err(DaemonError::Lifecycle(format!(
            "unexpected {operation} response: {other:?}"
        ))),
    }
}

#[derive(Debug)]
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> sylvops_daemon::Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(error) = execute!(stdout(), EnterAlternateScreen) {
            let _ = terminal::disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(unix)]
fn configure_detached_process(command: &mut ProcessCommand) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(windows)]
fn configure_detached_process(command: &mut ProcessCommand) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
    };

    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | DETACHED_PROCESS);
}
