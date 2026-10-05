//! Deterministic Claude Code CLI stand-in for discovery, health, and Session tests.

use std::{
    env, fs,
    io::{self, BufRead, Read, Write},
    net::{SocketAddr, TcpStream},
    process::ExitCode,
    time::Duration,
};

use serde_json::json;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fake-claude error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run() -> io::Result<()> {
    let arguments: Vec<_> = env::args().skip(1).collect();
    if let Some(exit) = probe_exit(&arguments) {
        return if exit == ExitCode::SUCCESS {
            Ok(())
        } else {
            Err(io::Error::other("fake authentication probe failed"))
        };
    }

    let settings_path = argument_value(&arguments, "--settings")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing --settings"))?;
    let settings = fs::read_to_string(settings_path)?;
    if !settings.contains("SessionStart")
        || !settings.contains("PermissionRequest")
        || !settings.contains("AskUserQuestion|ExitPlanMode")
        || !settings.contains("$SYLVOPS_HOOK_TOKEN")
        || settings.contains("permissionMode")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "managed settings contract is invalid",
        ));
    }
    let resumed_id = argument_value(&arguments, "--resume");
    let mut external_id = resumed_id.map_or_else(
        || {
            format!(
                "fake-claude-{}",
                env::var("SYLVOPS_SESSION_ID").unwrap_or_else(|_| "session".into())
            )
        },
        str::to_owned,
    );
    let session_start_source = if resumed_id.is_some() {
        "resume"
    } else {
        "startup"
    };
    match fixture("session-start", "immediate").as_str() {
        "late" => {
            emit_hook("UserPromptSubmit", &external_id)?;
            let late_id = external_id.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(18));
                let _ = emit_session_start(&late_id, "startup");
            });
        }
        "none" => {}
        _ => emit_session_start(&external_id, session_start_source)?,
    }
    println!("FAKE_CLAUDE_READY={external_id}");
    io::stdout().flush()?;

    for line in io::stdin().lock().lines() {
        let line = line?;
        match line.trim() {
            "session-start" => {
                emit_session_start(&external_id, "startup")?;
                println!("FAKE_CLAUDE_SESSION_START");
            }
            "clear-conversation" => {
                let replacement = format!("{external_id}-cleared");
                emit_session_start(&replacement, "clear")?;
                external_id = replacement;
                println!("FAKE_CLAUDE_CLEAR");
            }
            "resume-conversation" => {
                let replacement = format!("{external_id}-resumed");
                emit_session_start(&replacement, "resume")?;
                external_id = replacement;
                println!("FAKE_CLAUDE_RESUME_TRANSITION");
            }
            "unexpected-id-change" => {
                let replacement = format!("{external_id}-unexpected");
                let disposition = if emit_session_start(&replacement, "compact").is_ok() {
                    "accepted"
                } else {
                    "refused"
                };
                println!("FAKE_CLAUDE_ID_CHANGE={disposition}");
            }
            "permission" => {
                emit_hook("PermissionRequest", &external_id)?;
                println!("FAKE_CLAUDE_PERMISSION");
            }
            "stale-permission" => {
                emit_hook_with_marker("PermissionRequest", &external_id, "stale-turn")?;
                println!("FAKE_CLAUDE_STALE_PERMISSION");
            }
            "ask-user-question" => {
                emit_tool_hook("AskUserQuestion", &external_id)?;
                println!("FAKE_CLAUDE_ASK_USER");
            }
            "exit-plan-mode" => {
                emit_tool_hook("ExitPlanMode", &external_id)?;
                println!("FAKE_CLAUDE_EXIT_PLAN");
            }
            value if value.starts_with("user-prompt-") => {
                emit_hook_with_marker("UserPromptSubmit", &external_id, value)?;
                println!("FAKE_CLAUDE_USER_PROMPT");
            }
            "subagent-start" => {
                emit_subagent_hook("SubagentStart", &external_id, "agent-1")?;
                println!("FAKE_CLAUDE_SUBAGENT_START");
            }
            "subagent-stop" => {
                emit_subagent_hook("SubagentStop", &external_id, "agent-1")?;
                println!("FAKE_CLAUDE_SUBAGENT_STOP");
            }
            value if value.starts_with("stop-normal") => {
                emit_stop_hook(&external_id, value, false, false)?;
                println!("FAKE_CLAUDE_STOP");
            }
            "stop-background" => {
                emit_stop_hook(&external_id, "background", true, false)?;
                println!("FAKE_CLAUDE_STOP_BACKGROUND");
            }
            "stop-scheduled" => {
                emit_stop_hook(&external_id, "scheduled", false, true)?;
                println!("FAKE_CLAUDE_STOP_SCHEDULED");
            }
            "stop-failure" => {
                emit_hook_payload(
                    json!({
                        "hook_event_name": "StopFailure",
                        "session_id": external_id,
                        "error": "rate_limit",
                        "error_details": "SYLVOPS_PRIVATE_CLAUDE_FAILURE_75",
                        "last_assistant_message": "private provider error text"
                    }),
                    None,
                )?;
                println!("FAKE_CLAUDE_STOP_FAILURE");
            }
            "output-flood" => {
                let chunk = "x".repeat(16 * 1024);
                for _ in 0..512 {
                    println!("{chunk}");
                }
                io::stdout().flush()?;
                emit_hook_with_marker("UserPromptSubmit", &external_id, "after-output-flood")?;
                println!("FAKE_CLAUDE_OUTPUT_FLOOD_DONE");
            }
            "malformed-hook" => {
                emit_hook_bytes(
                    br#"{"hook_event_name":"Stop","private":"SYLVOPS_PRIVATE_MALFORMED_HOOK_75""#,
                )?;
                println!("FAKE_CLAUDE_MALFORMED_HOOK");
            }
            "unknown-hook" => {
                emit_hook_payload(
                    json!({
                        "hook_event_name": "FutureClaudeEvent",
                        "session_id": external_id,
                        "private": "SYLVOPS_PRIVATE_UNKNOWN_HOOK_75"
                    }),
                    None,
                )?;
                println!("FAKE_CLAUDE_UNKNOWN_HOOK");
            }
            "duplicate-hook" => {
                let payload = json!({
                    "hook_event_name": "UserPromptSubmit",
                    "session_id": external_id,
                    "fixture_marker": "duplicate"
                });
                emit_hook_payload(payload.clone(), None)?;
                emit_hook_payload(payload, None)?;
                println!("FAKE_CLAUDE_DUPLICATE_HOOK");
            }
            "unexpected-exit" => std::process::exit(17),
            "session-end" => {
                emit_hook("SessionEnd", &external_id)?;
                println!("FAKE_CLAUDE_SESSION_END");
            }
            "post-end-hook" => {
                let disposition = if emit_hook("UserPromptSubmit", &external_id).is_ok() {
                    "accepted"
                } else {
                    "refused"
                };
                println!("FAKE_CLAUDE_POST_END={disposition}");
            }
            "still-usable" => {
                emit_hook_with_marker("UserPromptSubmit", &external_id, "post-warning")?;
                println!("FAKE_CLAUDE_ECHO=still-usable");
            }
            value => println!("FAKE_CLAUDE_ECHO={value}"),
        }
        io::stdout().flush()?;
    }
    Ok(())
}

fn probe_exit(arguments: &[String]) -> Option<ExitCode> {
    if arguments == ["--version"] {
        println!("{} (Claude Code)", fixture("version", "2.1.145"));
        return Some(ExitCode::SUCCESS);
    }
    if arguments != ["auth", "status"] {
        return None;
    }
    Some(match fixture("auth", "claude.ai").as_str() {
        "logged-out" => {
            println!(r#"{{"loggedIn":false,"authMethod":"none"}}"#);
            ExitCode::FAILURE
        }
        "console" => {
            println!(r#"{{"loggedIn":true,"authMethod":"api_key","apiProvider":"firstParty"}}"#);
            ExitCode::SUCCESS
        }
        "oauth" => {
            println!(
                r#"{{"loggedIn":true,"authMethod":"oauth_token","apiProvider":"firstParty"}}"#
            );
            ExitCode::SUCCESS
        }
        "failed-supported" => {
            println!(r#"{{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty"}}"#);
            ExitCode::FAILURE
        }
        "unsupported" => {
            println!(
                r#"{{"loggedIn":true,"authMethod":"third_party","apiProvider":"thirdParty"}}"#
            );
            ExitCode::SUCCESS
        }
        "malformed" => {
            println!("not-json PRIVATE_AUTH_DETAIL");
            ExitCode::SUCCESS
        }
        "oversized" => {
            println!("{}", "x".repeat(65 * 1024));
            ExitCode::SUCCESS
        }
        "hang" => {
            std::thread::sleep(Duration::from_secs(30));
            ExitCode::SUCCESS
        }
        _ => {
            println!(
                r#"{{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"private@example.invalid"}}"#
            );
            ExitCode::SUCCESS
        }
    })
}

fn argument_value<'a>(arguments: &'a [String], flag: &str) -> Option<&'a str> {
    arguments
        .iter()
        .position(|argument| argument == flag)
        .and_then(|index| arguments.get(index + 1))
        .map(String::as_str)
}

fn emit_hook(event: &str, external_id: &str) -> io::Result<()> {
    emit_hook_payload(
        json!({
            "hook_event_name": event,
            "session_id": external_id,
            "source": "startup",
            "cwd": env::current_dir()?.to_string_lossy()
        }),
        None,
    )
}

fn emit_session_start(external_id: &str, source: &str) -> io::Result<()> {
    emit_hook_payload(
        json!({
            "hook_event_name": "SessionStart",
            "session_id": external_id,
            "source": source,
            "cwd": env::current_dir()?.to_string_lossy()
        }),
        None,
    )
}

fn emit_hook_with_marker(event: &str, external_id: &str, marker: &str) -> io::Result<()> {
    emit_hook_payload(
        json!({
            "hook_event_name": event,
            "session_id": external_id,
            "source": "startup",
            "cwd": env::current_dir()?.to_string_lossy()
        }),
        Some(marker),
    )
}

fn emit_tool_hook(tool_name: &str, external_id: &str) -> io::Result<()> {
    emit_hook_payload(
        json!({
            "hook_event_name": "PreToolUse",
            "session_id": external_id,
            "tool_name": tool_name,
            "tool_input": {}
        }),
        None,
    )
}

fn emit_subagent_hook(event: &str, external_id: &str, agent_id: &str) -> io::Result<()> {
    emit_hook_payload(
        json!({
            "hook_event_name": event,
            "session_id": external_id,
            "agent_id": agent_id,
            "agent_type": "Explore"
        }),
        None,
    )
}

fn emit_stop_hook(
    external_id: &str,
    marker: &str,
    background: bool,
    scheduled: bool,
) -> io::Result<()> {
    let background_tasks = if background {
        vec![json!({
            "id": "task-1",
            "type": "shell",
            "status": "running",
            "description": "bounded fixture task"
        })]
    } else {
        Vec::new()
    };
    let session_crons = if scheduled {
        vec![json!({
            "id": "cron-1",
            "schedule": "0 9 * * 1-5",
            "recurring": true,
            "prompt": "bounded fixture wakeup"
        })]
    } else {
        Vec::new()
    };
    emit_hook_payload(
        json!({
            "hook_event_name": "Stop",
            "session_id": external_id,
            "stop_hook_active": false,
            "background_tasks": background_tasks,
            "session_crons": session_crons
        }),
        Some(marker),
    )
}

fn emit_hook_payload(mut payload: serde_json::Value, marker: Option<&str>) -> io::Result<()> {
    if let Some(marker) = marker {
        payload["fixture_marker"] = json!(marker);
    }
    let body = serde_json::to_vec(&payload).map_err(io::Error::other)?;
    emit_hook_bytes(&body)
}

fn emit_hook_bytes(body: &[u8]) -> io::Result<()> {
    let endpoint = required_environment("SYLVOPS_HOOK_ENDPOINT")?;
    let token = required_environment("SYLVOPS_HOOK_TOKEN")?;
    let session_id = required_environment("SYLVOPS_SESSION_ID")?;
    let worktree_id = required_environment("SYLVOPS_WORKTREE_ID")?;
    let (address, path) = parse_endpoint(&endpoint)?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nX-SylvOps-Session-Id: {session_id}\r\nX-SylvOps-Worktree-Id: {worktree_id}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut response = [0_u8; 64];
    let read = stream.read(&mut response)?;
    if !String::from_utf8_lossy(&response[..read]).starts_with("HTTP/1.1 204") {
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

fn fixture(name: &str, default: &str) -> String {
    let path = env::current_exe().ok().and_then(|path| {
        path.parent()
            .map(|parent| parent.join(format!("fake-claude-{name}")))
    });
    path.and_then(|path| fs::read_to_string(path).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}
