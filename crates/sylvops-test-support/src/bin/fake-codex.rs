//! Deterministic Codex CLI stand-in for provider, hook, and resume tests.

use std::{
    env,
    io::{self, BufRead, Read, Write},
    net::{SocketAddr, TcpStream},
    process::{ExitCode, Stdio},
    time::Duration,
};

use serde_json::{Value, json};

#[path = "../fake_codex_bridge.rs"]
mod bridge;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fake-codex error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments == ["--version"] {
        println!("codex-cli 0.0.0-fake");
        return Ok(());
    }
    if arguments == ["--help"] {
        // Real help places this option beyond the diagnostic display truncation.
        print!("{}", "Other supported options\n".repeat(40));
        println!("--no-daemon Run without the shared background server");
        println!("--remote --remote-auth-token-env");
        return Ok(());
    }
    if arguments == ["app-server", "--help"] {
        println!("--listen stdio://");
        return Ok(());
    }
    if arguments
        .first()
        .is_some_and(|argument| argument == "app-server")
    {
        return bridge::server();
    }
    if arguments == ["login", "status"] {
        invalidate_codex_home_if_requested()?;
        println!("Logged in (fake fixture)");
        return Ok(());
    }

    let resume_index = arguments.iter().position(|argument| argument == "resume");
    let resumed = resume_index.is_some();
    let external_id = fake_external_id(&arguments, resume_index);
    if resumed && fail_next_resume()? {
        return Err(io::Error::other("injected resumed-process launch failure"));
    }
    emit_hook(
        "SessionStart",
        &external_id,
        Some(json!({"source": if resumed { "resume" } else { "startup" }})),
    )?;
    if resumed {
        println!("FAKE_CODEX_RESUMED={external_id}");
    } else {
        println!("FAKE_CODEX_READY={external_id}");
        let prompt = initial_prompt(&arguments);
        if prompt.is_some() {
            emit_hook(
                "UserPromptSubmit",
                &external_id,
                Some(json!({"turn_id": "turn-1"})),
            )?;
        }
        if prompt == Some("SYLVOPS_TEST_AUTO_FINISH") {
            emit_hook("Stop", &external_id, Some(json!({"turn_id": "turn-1"})))?;
            emit_hook("SessionEnd", &external_id, None)?;
            println!("FAKE_CODEX_FINISHED");
            io::stdout().flush()?;
            return Ok(());
        }
    }
    io::stdout().flush()?;

    run_interactive(&external_id)
}

fn run_interactive(external_id: &str) -> io::Result<()> {
    let mut bridge = bridge::Terminal::connect()?;
    for line in io::stdin().lock().lines() {
        let line = line?;
        if let Some(bridge) = &mut bridge
            && bridge.input(line.trim(), external_id)?
        {
            io::stdout().flush()?;
            continue;
        }
        if emit_permission_fixture(line.trim(), external_id)? {
            continue;
        }
        match line.trim() {
            "permission" => {
                emit_hook(
                    "PermissionRequest",
                    external_id,
                    Some(json!({"turn_id": "turn-1", "tool_name": "Bash"})),
                )?;
                println!("FAKE_CODEX_PERMISSION");
            }
            "interrupt" => {
                emit_hook("Interrupt", external_id, Some(json!({"turn_id": "turn-1"})))?;
                println!("FAKE_CODEX_INTERRUPTED");
            }
            "complete-live" => {
                emit_hook(
                    "UserPromptSubmit",
                    external_id,
                    Some(json!({"turn_id": "turn-2"})),
                )?;
                emit_hook("Stop", external_id, Some(json!({"turn_id": "turn-2"})))?;
                println!("FAKE_CODEX_COMPLETED_LIVE");
            }
            "end-session" => {
                emit_hook("SessionEnd", external_id, None)?;
                println!("FAKE_CODEX_SESSION_ENDED");
                io::stdout().flush()?;
                return Ok(());
            }
            "clean-exit" => return Ok(()),
            "finish" => {
                emit_hook("Stop", external_id, Some(json!({"turn_id": "turn-1"})))?;
                emit_hook("SessionEnd", external_id, None)?;
                println!("FAKE_CODEX_FINISHED");
                io::stdout().flush()?;
                return Ok(());
            }
            value => println!("FAKE_CODEX_ECHO={value}"),
        }
        io::stdout().flush()?;
    }
    Ok(())
}

fn invalidate_codex_home_if_requested() -> io::Result<()> {
    let Some(home) = env::var_os("CODEX_HOME").map(std::path::PathBuf::from) else {
        return Ok(());
    };
    let marker = home.join(".fake-codex-invalidate-home");
    if !marker.is_file() {
        return Ok(());
    }
    let invalidated = home.with_extension("invalidated");
    if invalidated.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "fake Codex invalidated home already exists",
        ));
    }
    std::fs::rename(home, invalidated)
}

fn fail_next_resume() -> io::Result<bool> {
    let Some(home) = env::var_os("CODEX_HOME").map(std::path::PathBuf::from) else {
        return Ok(false);
    };
    let marker = home.join(".fake-codex-fail-next-resume");
    match std::fs::remove_file(marker) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn fake_external_id(arguments: &[String], resume_index: Option<usize>) -> String {
    resume_index.map_or_else(
        || {
            format!(
                "fake-{}",
                env::var("SYLVOPS_SESSION_ID").unwrap_or_else(|_| "session".into())
            )
        },
        |resume_index| {
            arguments
                .get(resume_index + 1)
                .cloned()
                .unwrap_or_else(|| "fake-missing".into())
        },
    )
}

fn initial_prompt(arguments: &[String]) -> Option<&str> {
    if arguments.iter().any(|argument| argument == "resume") {
        return None;
    }
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        if matches!(
            argument.as_str(),
            "--profile" | "--cd" | "--model" | "--config" | "--remote" | "--remote-auth-token-env"
        ) {
            index += 2;
        } else if argument.starts_with('-') {
            index += 1;
        } else {
            return Some(argument);
        }
    }
    None
}

fn emit_permission_fixture(command: &str, external_id: &str) -> io::Result<bool> {
    match command {
        "permission-auto" => {
            emit_hook(
                "UserPromptSubmit",
                external_id,
                Some(json!({"turn_id": "turn-auto"})),
            )?;
            for event in ["PreToolUse", "PermissionRequest", "PostToolUse"] {
                emit_tool_hook(event, external_id, "turn-auto", "auto", "automatic")?;
            }
        }
        "permission-wait" => {
            emit_hook(
                "UserPromptSubmit",
                external_id,
                Some(json!({"turn_id": "turn-wait"})),
            )?;
            emit_tool_hook("PreToolUse", external_id, "turn-wait", "first", "first")?;
            emit_tool_hook(
                "PermissionRequest",
                external_id,
                "turn-wait",
                "first",
                "first",
            )?;
        }
        "permission-second" => {
            emit_tool_hook("PreToolUse", external_id, "turn-wait", "second", "second")?;
            emit_tool_hook(
                "PermissionRequest",
                external_id,
                "turn-wait",
                "second",
                "second",
            )?;
        }
        "permission-first-done" => {
            emit_tool_hook("PostToolUse", external_id, "turn-wait", "first", "first")?;
        }
        "permission-second-done" => {
            emit_tool_hook("PostToolUse", external_id, "turn-wait", "second", "second")?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn emit_tool_hook(
    event: &str,
    external_id: &str,
    turn: &str,
    call: &str,
    command: &str,
) -> io::Result<()> {
    let mut extra =
        json!({"turn_id": turn, "tool_name": "Bash", "tool_input": {"command": command}});
    // The documented PermissionRequest payload does not carry tool_use_id.
    if event != "PermissionRequest" {
        extra["tool_use_id"] = json!(call);
    }
    emit_hook(event, external_id, Some(extra))
}

fn emit_hook(event: &str, external_id: &str, extra: Option<Value>) -> io::Result<()> {
    // A shared provider server has its own environment and cannot inherit this
    // managed Session's hook credentials or process-tree ownership.
    if let Some(home) = env::var_os("CODEX_HOME").map(std::path::PathBuf::from)
        && home.join(".fake-codex-shared-daemon").is_file()
        && !env::args().any(|argument| argument == "--no-daemon")
        && !env::args().any(|argument| argument == "--remote")
    {
        return Ok(());
    }
    let Ok(endpoint) = env::var("SYLVOPS_HOOK_ENDPOINT") else {
        return Ok(());
    };
    let token = required_environment("SYLVOPS_HOOK_TOKEN")?;
    let session_id = required_environment("SYLVOPS_SESSION_ID")?;
    let worktree_id = required_environment("SYLVOPS_WORKTREE_ID")?;
    let mut payload = json!({
        "hook_event_name": event,
        "session_id": external_id,
        "cwd": env::current_dir()?.to_string_lossy()
    });
    if let (Some(target), Some(extra)) = (
        payload.as_object_mut(),
        extra.and_then(|v| v.as_object().cloned()),
    ) {
        target.extend(extra);
    }
    let body = serde_json::to_vec(&payload).map_err(io::Error::other)?;
    if let Some(home) = env::var_os("CODEX_HOME").map(std::path::PathBuf::from)
        && home.join(".fake-codex-execute-hooks").is_file()
    {
        return execute_hook(&home, event, &body);
    }
    let (address, path) = parse_endpoint(&endpoint)?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nX-SylvOps-Session-Id: {session_id}\r\nX-SylvOps-Worktree-Id: {worktree_id}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;
    let mut response = [0_u8; 64];
    let read = stream.read(&mut response)?;
    let status = String::from_utf8_lossy(&response[..read]);
    if !status.starts_with("HTTP/1.1 204") {
        return Err(io::Error::other("hook receiver refused fake event"));
    }
    Ok(())
}

fn execute_hook(home: &std::path::Path, event: &str, body: &[u8]) -> io::Result<()> {
    let arguments: Vec<_> = env::args().collect();
    let profile = arguments
        .windows(2)
        .find(|pair| pair[0] == "--profile")
        .map(|pair| &pair[1])
        .ok_or_else(|| io::Error::other("missing managed hook profile"))?;
    let source = std::fs::read_to_string(home.join(format!("{profile}.config.toml")))?;
    let config: toml::Value = toml::from_str(&source).map_err(io::Error::other)?;
    let key = if cfg!(windows) {
        "command_windows"
    } else {
        "command"
    };
    let command = config["hooks"][event][0]["hooks"][0][key]
        .as_str()
        .ok_or_else(|| io::Error::other("missing generated hook command"))?;

    // Mirror Codex's command processor contract; never parse the executable out
    // of the generated command, as doing so would hide serialization failures.
    #[cfg(windows)]
    let mut child = {
        let mut child =
            if env::args().any(|argument| argument == "--no-daemon" || argument == "--remote") {
                let shell = std::path::PathBuf::from(
                    env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()),
                )
                .join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
                let mut child = tokio::process::Command::new(shell);
                child.args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    command,
                ]);
                child
            } else {
                let shell = env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
                let mut child = tokio::process::Command::new(shell);
                child.arg("/C").raw_arg(format!(r#""{command}""#));
                child
            };
        child.creation_flags(0x0800_0000);
        child
    };
    #[cfg(unix)]
    let mut child = {
        let mut child = tokio::process::Command::new("/bin/sh");
        child.args(["-lc", command]);
        child
    };
    let body = body.to_vec();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            use tokio::io::AsyncWriteExt;
            let mut child = child
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            tokio::time::timeout(Duration::from_secs(2), async {
                let mut stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| io::Error::other("hook stdin"))?;
                stdin.write_all(&body).await?;
                drop(stdin);
                let status = child.wait().await?;
                if !status.success() {
                    return Err(io::Error::other("generated hook command failed"));
                }
                Ok(())
            })
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "generated hook command timed out")
            })?
        })
}

fn required_environment(name: &str) -> io::Result<String> {
    env::var(name).map_err(|_| io::Error::new(io::ErrorKind::NotFound, format!("missing {name}")))
}

fn parse_endpoint(endpoint: &str) -> io::Result<(SocketAddr, String)> {
    let remainder = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "endpoint is not HTTP"))?;
    let (authority, path) = remainder
        .split_once('/')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "endpoint path is missing"))?;
    let address: SocketAddr = authority
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "endpoint address is invalid"))?;
    if !address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "endpoint is not loopback",
        ));
    }
    Ok((address, format!("/{path}")))
}
