//! Deterministic Codex CLI stand-in for provider, hook, and resume tests.

use std::{
    env,
    io::{self, BufRead, Read, Write},
    net::{SocketAddr, TcpStream},
    process::ExitCode,
    time::Duration,
};

use serde_json::{Value, json};

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
    if arguments == ["login", "status"] {
        println!("Logged in (fake fixture)");
        return Ok(());
    }

    let resume_index = arguments.iter().position(|argument| argument == "resume");
    let resumed = resume_index.is_some();
    let external_id = if let Some(resume_index) = resume_index {
        arguments
            .get(resume_index + 1)
            .cloned()
            .unwrap_or_else(|| "fake-missing".into())
    } else {
        format!(
            "fake-{}",
            env::var("SYLVOPS_SESSION_ID").unwrap_or_else(|_| "session".into())
        )
    };
    emit_hook(
        "SessionStart",
        &external_id,
        Some(json!({"source": if resumed { "resume" } else { "startup" }})),
    )?;
    if resumed {
        println!("FAKE_CODEX_RESUMED={external_id}");
    } else {
        println!("FAKE_CODEX_READY={external_id}");
        if initial_prompt(&arguments).is_some() {
            emit_hook(
                "UserPromptSubmit",
                &external_id,
                Some(json!({"turn_id": "turn-initial"})),
            )?;
        }
    }
    io::stdout().flush()?;

    for line in io::stdin().lock().lines() {
        match line?.trim() {
            "permission" => {
                emit_hook(
                    "PermissionRequest",
                    &external_id,
                    Some(json!({"turn_id": "turn-1", "tool_name": "Bash"})),
                )?;
                println!("FAKE_CODEX_PERMISSION");
            }
            "finish" => {
                emit_hook("Stop", &external_id, Some(json!({"turn_id": "turn-1"})))?;
                emit_hook("SessionEnd", &external_id, None)?;
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

fn initial_prompt(arguments: &[String]) -> Option<&str> {
    if arguments.iter().any(|argument| argument == "resume") {
        return None;
    }
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        if matches!(
            argument.as_str(),
            "--profile" | "--cd" | "--model" | "--config"
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

fn emit_hook(event: &str, external_id: &str, extra: Option<Value>) -> io::Result<()> {
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
