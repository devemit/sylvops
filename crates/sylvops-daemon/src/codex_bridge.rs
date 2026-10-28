//! Session-owned stdio app server with an authenticated native-terminal connection.
//! Only structured requests establish input attention; terminal text is never inspected.

use std::{
    collections::HashSet, ffi::OsString, path::Path, process::Stdio, sync::Arc, time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use sylvops_core::{
    provider::{LaunchArguments, LaunchSpec, ProviderLifecycleEvent},
    status::{
        ConversationIdentity, ConversationIdentityTransition, NormalizedProviderEvent,
        ProviderConversationId, RemainingWork, TurnFailureCategory,
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::{mpsc, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    accept_hdr_async_with_config,
    tungstenite::{
        Message,
        handshake::server::{Callback, ErrorResponse, Request, Response},
        protocol::WebSocketConfig,
    },
};

use crate::{
    DaemonError, Result,
    process_tree::{self, ProcessTree},
    runtime::AuthenticationToken,
};

const MESSAGE_LIMIT: usize = 1024 * 1024;
const REQUEST_LIMIT: usize = 128;
const REQUEST_ID_LIMIT: usize = 4096;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const TOKEN_ENV: &str = "SYLVOPS_CODEX_REMOTE_TOKEN";

struct Authenticate<'a> {
    expected: &'a str,
}

impl Callback for Authenticate<'_> {
    fn on_request(
        self,
        request: &Request,
        response: Response,
    ) -> std::result::Result<Response, ErrorResponse> {
        let valid = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value.len() == self.expected.len()
                    && bool::from(value.as_bytes().ct_eq(self.expected.as_bytes()))
            });
        if valid && !request.headers().contains_key("origin") {
            return Ok(response);
        }
        let mut denied = ErrorResponse::new(Some("Unauthorized".into()));
        *denied.status_mut() = tokio_tungstenite::tungstenite::http::StatusCode::UNAUTHORIZED;
        Err(denied)
    }
}

fn refused() -> DaemonError {
    DaemonError::Provider("Codex structured runtime failed or sent an unsupported message; refresh Provider health before starting another Session".into())
}

#[derive(Debug)]
pub(crate) struct Bridge {
    tree: Arc<ProcessTree>,
    task: JoinHandle<()>,
    failure: watch::Receiver<bool>,
    events: Option<mpsc::Receiver<ProviderLifecycleEvent>>,
}

impl Bridge {
    pub(crate) fn take_events(&mut self) -> mpsc::Receiver<ProviderLifecycleEvent> {
        self.events.take().expect("one runtime event consumer")
    }

    pub(crate) async fn failed(&mut self) {
        while !*self.failure.borrow() {
            if self.failure.changed().await.is_err() {
                return;
            }
        }
    }

    pub(crate) fn terminate(&self) -> Result<()> {
        self.tree.terminate()
    }
    pub(crate) fn is_empty(&self) -> Result<bool> {
        self.tree.is_empty()
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.tree.terminate();
        self.task.abort();
    }
}

/// Prepare before spawning the PTY, so both processes are owned from their first instruction.
pub(crate) async fn prepare(launch: &mut LaunchSpec, cwd: &Path) -> Result<Bridge> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let token = AuthenticationToken::generate();
    let terminal_token = OsString::from(token.expose());
    let mut command = crate::background_process::command(&launch.executable);
    command
        .args(["app-server", "--listen", "stdio://"])
        .current_dir(cwd)
        .env_clear()
        .envs(&launch.environment)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }
    #[cfg(windows)]
    let tree = process_tree::create()?;
    #[cfg(windows)]
    command.creation_flags(
        windows_sys::Win32::System::Threading::CREATE_SUSPENDED
            | windows_sys::Win32::System::Threading::CREATE_NO_WINDOW,
    );
    let mut child = command.spawn().map_err(|_| refused())?;
    let process_id = child.id().ok_or_else(refused)?;
    #[cfg(unix)]
    let tree = match process_tree::attach(process_id, i32::try_from(process_id).ok()) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = child.kill().await;
            return Err(error);
        }
    };
    #[cfg(windows)]
    if let Err(error) = tree
        .assign_async(&child)
        .and_then(|()| ProcessTree::resume(process_id))
    {
        let _ = child.kill().await;
        return Err(error);
    }
    let tree = Arc::new(tree);
    let stdin = child.stdin.take().ok_or_else(refused)?;
    let stdout = child.stdout.take().ok_or_else(refused)?;
    let (events, receiver) = mpsc::channel(128);
    let (failed, failure) = watch::channel(false);
    let runtime_tree = tree.clone();
    let task = tokio::spawn(async move {
        let _ = forward(listener, token, stdin, stdout, events).await;
        let _ = runtime_tree.terminate();
        let _ = timeout(IO_TIMEOUT, child.wait()).await;
        // A native terminal closes its transport just before exiting. Let the
        // PTY waiter publish that real exit rather than racing it with a kill.
        tokio::time::sleep(Duration::from_millis(250)).await;
        failed.send_replace(true);
    });
    let persisted = launch
        .arguments
        .persisted_values()
        .iter()
        .filter(|value| *value != "--no-daemon")
        .cloned()
        .collect::<Vec<_>>();
    let transient = launch.arguments.all()[launch.arguments.persisted_values().len()..].to_vec();
    launch.arguments = LaunchArguments::with_transient_tail(persisted, transient);
    launch.arguments.prepend_persisted([
        OsString::from("--remote"),
        OsString::from(format!("ws://{address}")),
        OsString::from("--remote-auth-token-env"),
        OsString::from(TOKEN_ENV),
    ]);
    launch.environment.insert(TOKEN_ENV.into(), terminal_token);
    Ok(Bridge {
        tree,
        task,
        failure,
        events: Some(receiver),
    })
}

async fn forward(
    listener: TcpListener,
    token: AuthenticationToken,
    mut stdin: tokio::process::ChildStdin,
    stdout: tokio::process::ChildStdout,
    events: mpsc::Sender<ProviderLifecycleEvent>,
) -> Result<()> {
    let connection = timeout(CONNECT_TIMEOUT, async {
        for _ in 0..16 {
            let (stream, peer) = listener.accept().await?;
            if !peer.ip().is_loopback() {
                continue;
            }
            let expected = format!("Bearer {}", token.expose());
            let mut config = WebSocketConfig::default();
            config.max_message_size = Some(MESSAGE_LIMIT);
            config.max_frame_size = Some(MESSAGE_LIMIT);
            config.max_write_buffer_size = MESSAGE_LIMIT * 2;
            let handshake = accept_hdr_async_with_config(
                stream,
                Authenticate {
                    expected: &expected,
                },
                Some(config),
            );
            if let Ok(Ok(connection)) = timeout(Duration::from_secs(2), handshake).await {
                return Ok(connection);
            }
        }
        Err(refused())
    })
    .await
    .map_err(|_| refused())??;
    drop(listener); // Exactly one terminal owns this connection for this Session.
    let (mut terminal_writer, mut terminal_reader) = connection.split();
    let mut backend = BufReader::new(stdout);
    let mut partial = Vec::new();
    let mut tracker = InputTracker::default();
    loop {
        tokio::select! {
            message = terminal_reader.next() => {
                let Some(Ok(message)) = message else { return Err(refused()); };
                match message {
                    Message::Text(text) => {
                        if text.len() > MESSAGE_LIMIT { return Err(refused()); }
                        let value: Value = serde_json::from_str(&text).map_err(|_| refused())?;
                        if tracker.client_message(&value).is_err() {
                            let id = value.get("id").ok_or_else(refused)?;
                            let response = json!({"id":id,"error":{"code":-32000,"message":"The Session is waiting for an answer. Respond to the open prompt before starting other work."}});
                            timeout(IO_TIMEOUT, terminal_writer.send(Message::Text(response.to_string().into()))).await.map_err(|_| refused())?.map_err(|_| refused())?;
                            continue;
                        }
                        timeout(IO_TIMEOUT, async {
                            stdin.write_all(text.as_bytes()).await?;
                            stdin.write_all(b"\n").await?;
                            stdin.flush().await
                        }).await.map_err(|_| refused())??;
                    }
                    Message::Ping(bytes) => {
                        timeout(IO_TIMEOUT, terminal_writer.send(Message::Pong(bytes))).await.map_err(|_| refused())?.map_err(|_| refused())?;
                    }
                    Message::Pong(_) => {}
                    _ => return Err(refused()),
                }
            }
            message = read_message(&mut backend, &mut partial) => {
                let mut value = message?;
                if tracker.consume_private_response(&value)? { continue; }
                for event in tracker.server_message(&value)? {
                    timeout(IO_TIMEOUT, events.send(event)).await.map_err(|_| refused())?.map_err(|_| refused())?;
                }
                if let Some(pause) = tracker.pause_request.take() {
                    let bytes = serde_json::to_vec(&pause).map_err(|_| refused())?;
                    timeout(IO_TIMEOUT, async {
                        stdin.write_all(&bytes).await?;
                        stdin.write_all(b"\n").await?;
                        stdin.flush().await
                    }).await.map_err(|_| refused())??;
                }
                if value["method"] == "item/tool/requestUserInput" {
                    value["params"]["autoResolutionMs"] = Value::Null;
                    value["params"]["isBlocking"] = Value::Bool(true);
                }
                let text = serde_json::to_string(&value).map_err(|_| refused())?;
                timeout(IO_TIMEOUT, terminal_writer.send(Message::Text(text.into()))).await.map_err(|_| refused())?.map_err(|_| refused())?;
            }
        }
    }
}

async fn read_message(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    bytes: &mut Vec<u8>,
) -> Result<Value> {
    // fill_buf/consume avoids an unbounded allocation from read_line/read_until.
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(refused());
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |i| i + 1);
        if bytes.len() + count > MESSAGE_LIMIT {
            return Err(refused());
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if bytes.last() == Some(&b'\n') {
            let result = serde_json::from_slice(bytes).map_err(|_| refused());
            bytes.clear();
            return result;
        }
    }
}

#[derive(Debug, Default)]
struct InputTracker {
    conversation: Option<ProviderConversationId>,
    turn: Option<String>,
    pending: HashSet<String>,
    seen_requests: HashSet<String>,
    thread_requests: HashSet<String>,
    question: Option<String>,
    pause_request: Option<Value>,
    pause_id: Option<String>,
    pause_sequence: u64,
    answer_request: Option<String>,
    question_turn_finished: bool,
}

impl InputTracker {
    fn client_message(&mut self, message: &Value) -> Result<()> {
        if message["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("sylvops-input-pause-"))
        {
            return Err(refused());
        }
        if !self.pending.is_empty()
            && matches!(
                message["method"].as_str(),
                Some("turn/start" | "turn/steer" | "thread/resume" | "thread/fork")
            )
        {
            return Err(refused());
        }
        if self.question.is_some() && message["method"] == "turn/start" {
            let input = message["params"]["input"].as_array().ok_or_else(refused)?;
            if input.is_empty() {
                return Err(refused());
            }
            self.answer_request = Some(request_key(&message["id"])?);
        }
        if message["params"]["ephemeral"] != true
            && matches!(
                message["method"].as_str(),
                Some("thread/start" | "thread/resume")
            )
        {
            if self.thread_requests.len() >= REQUEST_LIMIT {
                return Err(refused());
            }
            self.thread_requests.insert(request_key(&message["id"])?);
        }
        Ok(())
    }

    fn consume_private_response(&mut self, message: &Value) -> Result<bool> {
        if self
            .pause_id
            .as_deref()
            .is_some_and(|id| message["id"].as_str() == Some(id))
        {
            if message.get("error").is_some() && !self.question_turn_finished {
                return Err(refused());
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn event(&self, event: NormalizedProviderEvent) -> ProviderLifecycleEvent {
        ProviderLifecycleEvent {
            event: Some(event),
            conversation_id: self.conversation.clone(),
            turn_id: self.turn.clone(),
        }
    }

    fn bind(&mut self, identity: &Value) -> Result<Vec<ProviderLifecycleEvent>> {
        let id = identity.as_str().ok_or_else(refused)?;
        let id = ProviderConversationId::new(id).map_err(|_| refused())?;
        if let Some(current) = &self.conversation {
            return if current == &id {
                Ok(Vec::new())
            } else {
                Err(refused())
            };
        }
        self.conversation = Some(id.clone());
        Ok(vec![self.event(NormalizedProviderEvent::TurnStarted {
            conversation: Some(ConversationIdentity {
                id,
                transition: ConversationIdentityTransition::Established,
            }),
        })])
    }

    #[allow(clippy::too_many_lines)]
    fn server_message(&mut self, message: &Value) -> Result<Vec<ProviderLifecycleEvent>> {
        if message.get("id").is_some()
            && message.get("method").is_none()
            && self.thread_requests.remove(&request_key(&message["id"])?)
            && !message["result"]["thread"]["id"].is_null()
        {
            return self.bind(&message["result"]["thread"]["id"]);
        }
        let params = &message["params"];
        let method = message["method"].as_str().unwrap_or_default();
        if method == "thread/started" {
            // Bind only the response to the native client's non-ephemeral
            // start/resume. Bootstrap and child threads can notify first.
            return Ok(Vec::new());
        }
        let Some(conversation) = &self.conversation else {
            return if message.get("id").is_some() && !method.is_empty() {
                Err(refused())
            } else {
                Ok(Vec::new())
            };
        };
        if params["threadId"].as_str() != Some(conversation.as_str()) {
            return if message.get("id").is_some() && !method.is_empty() {
                Err(refused())
            } else {
                Ok(Vec::new())
            };
        }
        let event = match method {
            "turn/started" => {
                if !self.pending.is_empty() {
                    return Err(refused());
                }
                if self.question.is_some() {
                    if self.answer_request.take().is_none() {
                        return Err(refused());
                    }
                    self.question = None;
                    self.pause_id = None;
                    self.question_turn_finished = false;
                    self.turn = Some(bounded_id(&params["turn"]["id"])?);
                    return Ok(vec![
                        self.event(NormalizedProviderEvent::InputRequestsResolved),
                        self.event(NormalizedProviderEvent::PromptSubmitted),
                    ]);
                }
                self.turn = Some(bounded_id(&params["turn"]["id"])?);
                Some(NormalizedProviderEvent::PromptSubmitted)
            }
            "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "item/tool/requestUserInput"
            | "mcpServer/elicitation/request" => {
                if method != "mcpServer/elicitation/request" && self.turn.is_none() {
                    return Err(refused());
                }
                if params["turnId"]
                    .as_str()
                    .is_some_and(|id| Some(id) != self.turn.as_deref())
                {
                    return Err(refused());
                }
                let key = request_key(&message["id"])?;
                if self.pending.contains(&key) {
                    return Ok(Vec::new());
                }
                if self.seen_requests.len() >= REQUEST_ID_LIMIT
                    || !self.seen_requests.insert(key.clone())
                {
                    return Err(refused());
                }
                if self.pending.len() >= REQUEST_LIMIT {
                    return Err(refused());
                }
                self.pending.insert(key);
                Some(NormalizedProviderEvent::InputRequestOpened)
            }
            "serverRequest/resolved" => {
                let key = request_key(&params["requestId"])?;
                if self.pending.remove(&key) && self.pending.is_empty() && self.question.is_none() {
                    Some(NormalizedProviderEvent::InputRequestsResolved)
                } else {
                    None
                }
            }
            "turn/completed" => {
                if self.turn.as_deref() != params["turn"]["id"].as_str() {
                    return Ok(Vec::new());
                }
                self.pending.clear();
                if self.question.is_some()
                    && matches!(
                        params["turn"]["status"].as_str(),
                        Some("completed" | "interrupted")
                    )
                {
                    self.question_turn_finished = true;
                    // A question deliberately interrupts an asynchronous turn.
                    // Keep its attention instead of announcing completion/interruption.
                    return Ok(Vec::new());
                }
                self.question = None;
                Some(match params["turn"]["status"].as_str() {
                    Some("completed") => NormalizedProviderEvent::TurnStopped {
                        remaining_work: RemainingWork::default(),
                    },
                    Some("interrupted") => NormalizedProviderEvent::TurnInterrupted,
                    Some("failed") => NormalizedProviderEvent::TurnFailed {
                        category: TurnFailureCategory::new("provider_unavailable")
                            .map_err(|_| refused())?,
                    },
                    _ => return Err(refused()),
                })
            }
            "item/completed"
                if params["item"]["type"] == "agentMessage"
                    && params["item"]["delivery"] == "async"
                    && params["item"]["questions"]
                        .as_array()
                        .is_some_and(|questions| !questions.is_empty()) =>
            {
                let item = bounded_id(&params["item"]["id"])?;
                if self.question.as_ref() == Some(&item) {
                    return Ok(Vec::new());
                }
                if self.question.is_some() {
                    return Err(refused());
                }
                let turn = self.turn.as_ref().ok_or_else(refused)?;
                if params["turnId"].as_str() != Some(turn) {
                    return Err(refused());
                }
                self.pause_sequence = self.pause_sequence.checked_add(1).ok_or_else(refused)?;
                let id = format!("sylvops-input-pause-{}", self.pause_sequence);
                self.pause_request = Some(
                    json!({"id":id,"method":"turn/interrupt","params":{"threadId":conversation.as_str(),"turnId":turn}}),
                );
                self.pause_id = Some(id);
                self.question = Some(item);
                self.question_turn_finished = false;
                Some(NormalizedProviderEvent::InputRequestOpened)
            }
            _ => {
                if message.get("id").is_some() && !method.is_empty() {
                    return Err(refused());
                }
                None
            }
        };
        Ok(event
            .map(|event| vec![self.event(event)])
            .unwrap_or_default())
    }
}

fn bounded_id(value: &Value) -> Result<String> {
    let text = value.as_str().ok_or_else(refused)?;
    if text.is_empty() || text.len() > 256 || text.chars().any(char::is_control) {
        return Err(refused());
    }
    Ok(text.into())
}

fn request_key(value: &Value) -> Result<String> {
    match value {
        Value::String(_) => {
            bounded_id(value)?;
        }
        Value::Number(number) if number.is_i64() || number.is_u64() => {}
        _ => return Err(refused()),
    }
    serde_json::to_string(value).map_err(|_| refused())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sylvops_core::{
        session_state::{ProviderActivity, SessionLifecycle},
        status::SessionStatusMachine,
    };

    fn tracker() -> InputTracker {
        let mut tracker = InputTracker::default();
        tracker
            .client_message(&json!({"id":0,"method":"thread/start","params":{}}))
            .unwrap();
        tracker
            .server_message(&json!({"id":0,"result":{"thread":{"id":"conversation"}}}))
            .unwrap();
        tracker.server_message(&json!({"method":"turn/started","params":{"threadId":"conversation","turn":{"id":"turn"}}})).unwrap();
        tracker
    }

    fn request(method: &str, id: impl Into<Value>) -> Value {
        let id: Value = id.into();
        json!({"id":id,"method":method,"params":{"threadId":"conversation","turnId":"turn"}})
    }

    fn resolved(id: impl Into<Value>) -> Value {
        let id: Value = id.into();
        json!({"method":"serverRequest/resolved","params":{"threadId":"conversation","requestId":id}})
    }

    #[test]
    fn real_questions_and_approvals_wait_until_the_provider_confirms_an_answer() {
        for method in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
            "item/permissions/requestApproval",
            "item/tool/requestUserInput",
            "mcpServer/elicitation/request",
        ] {
            let mut tracker = tracker();
            let mut machine = SessionStatusMachine::new(SessionLifecycle::Running);
            let events = tracker.server_message(&request(method, json!(1))).unwrap();
            machine.apply(events[0].event.as_ref().unwrap(), 1).unwrap();
            assert_eq!(machine.status().activity, ProviderActivity::AwaitingInput);
            let attention = machine.status().pending_review().unwrap();
            assert!(machine.acknowledge_attention(attention.id, attention.reason));
            assert_eq!(machine.status().activity, ProviderActivity::AwaitingInput);
            assert!(
                tracker
                    .client_message(&json!({"method":"turn/start"}))
                    .is_err()
            );
            // Submitting a response is not itself confirmation that the backend accepted it.
            tracker
                .client_message(&json!({"id":1,"result":{}}))
                .unwrap();
            assert_eq!(tracker.pending.len(), 1);
            let events = tracker.server_message(&resolved(json!(1))).unwrap();
            machine.apply(events[0].event.as_ref().unwrap(), 2).unwrap();
            assert_eq!(machine.status().activity, ProviderActivity::Working);
        }
    }

    #[test]
    fn duplicates_overlap_stale_resolutions_and_output_cannot_clear_waiting() {
        let mut tracker = tracker();
        assert_eq!(
            tracker
                .server_message(&request("item/tool/requestUserInput", json!(1)))
                .unwrap()
                .len(),
            1
        );
        assert!(
            tracker
                .server_message(&request("item/tool/requestUserInput", json!(1)))
                .unwrap()
                .is_empty()
        );
        tracker
            .server_message(&request(
                "item/commandExecution/requestApproval",
                json!("1"),
            ))
            .unwrap();
        assert!(
            tracker
                .server_message(&resolved(json!(99)))
                .unwrap()
                .is_empty()
        );
        assert!(tracker.server_message(&json!({"method":"item/agentMessage/delta","params":{"threadId":"conversation","delta":"Working?"}})).unwrap().is_empty());
        assert!(
            tracker
                .server_message(&resolved(json!(1)))
                .unwrap()
                .is_empty()
        );
        assert_eq!(tracker.pending.len(), 1);
        assert_eq!(
            tracker.server_message(&resolved(json!("1"))).unwrap().len(),
            1
        );
        assert!(
            tracker
                .server_message(&resolved(json!("1")))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn ordinary_tool_activity_never_creates_input_attention() {
        let mut tracker = tracker();
        for method in ["item/started", "item/completed", "thread/status/changed"] {
            assert!(
                tracker
                    .server_message(&json!({"method":method,"params":{"threadId":"conversation"}}))
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(tracker.pending.is_empty());
    }

    #[tokio::test]
    async fn backend_messages_are_bounded_before_json_decoding() {
        let bytes = vec![b'x'; MESSAGE_LIMIT + 1];
        assert!(
            read_message(&mut bytes.as_slice(), &mut Vec::new())
                .await
                .is_err()
        );
        assert!(
            read_message(&mut b"not-json\n".as_slice(), &mut Vec::new())
                .await
                .is_err()
        );
    }

    #[test]
    fn terminal_authentication_rejects_missing_wrong_and_browser_credentials() {
        for (authorization, origin, accepted) in [
            (None, None, false),
            (Some("Bearer wrong"), None, false),
            (Some("Bearer secret"), Some("http://localhost"), false),
            (Some("Bearer secret"), None, true),
        ] {
            let mut request = Request::builder();
            if let Some(authorization) = authorization {
                request = request.header("authorization", authorization);
            }
            if let Some(origin) = origin {
                request = request.header("origin", origin);
            }
            let result = Authenticate {
                expected: "Bearer secret",
            }
            .on_request(&request.body(()).unwrap(), Response::new(()));
            assert_eq!(result.is_ok(), accepted);
        }
    }

    #[test]
    fn unknown_approval_requests_and_request_overflow_fail_closed() {
        let mut tracker = tracker();
        assert!(
            tracker
                .server_message(&request("item/unknown/requestApproval", json!(1)))
                .is_err()
        );
        for index in 0..REQUEST_LIMIT {
            tracker
                .server_message(&request("item/tool/requestUserInput", json!(index)))
                .unwrap();
        }
        assert!(
            tracker
                .server_message(&request("item/tool/requestUserInput", json!(REQUEST_LIMIT)))
                .is_err()
        );
        assert!(
            tracker
                .server_message(&request(
                    "item/tool/requestUserInput",
                    json!("x".repeat(257))
                ))
                .is_err()
        );
    }

    #[test]
    fn asynchronous_questions_interrupt_work_and_only_an_answer_restarts_it() {
        let mut tracker = tracker();
        let question = json!({"method":"item/completed","params":{"threadId":"conversation","turnId":"turn","item":{"id":"question","type":"agentMessage","delivery":"async","questions":[{"title":"Choose a value"}]}}});
        assert_eq!(
            tracker.server_message(&question).unwrap()[0].event,
            Some(NormalizedProviderEvent::InputRequestOpened)
        );
        assert_eq!(
            tracker.pause_request.as_ref().unwrap()["method"],
            "turn/interrupt"
        );
        assert!(tracker.server_message(&question).unwrap().is_empty());
        assert!(tracker.server_message(&json!({"method":"turn/completed","params":{"threadId":"conversation","turn":{"id":"turn","status":"interrupted"}}})).unwrap().is_empty());
        assert_eq!(tracker.question.as_deref(), Some("question"));
        assert!(
            tracker
                .client_message(&json!({"id":2,"method":"turn/start","params":{"input":[]}}))
                .is_err()
        );
        tracker.client_message(&json!({"id":2,"method":"turn/start","params":{"input":[{"type":"text","text":"my answer"}]}})).unwrap();
        let events = tracker.server_message(&json!({"method":"turn/started","params":{"threadId":"conversation","turn":{"id":"next-turn"}}})).unwrap();
        assert_eq!(
            events[0].event,
            Some(NormalizedProviderEvent::InputRequestsResolved)
        );
        assert_eq!(
            events[1].event,
            Some(NormalizedProviderEvent::PromptSubmitted)
        );
        assert!(tracker.question.is_none());
    }
}
