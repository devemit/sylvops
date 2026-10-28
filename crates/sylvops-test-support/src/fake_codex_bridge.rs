//! Offline native-terminal and stdio-backend fixtures. No real account is used.

use serde_json::{Value, json};
use std::{
    env,
    io::{self, BufRead, Write},
    net::TcpStream,
    time::Duration,
};
use tokio_tungstenite::tungstenite::{
    self, Message, client::IntoClientRequest, stream::MaybeTlsStream,
};

pub struct Terminal {
    socket: tungstenite::WebSocket<MaybeTlsStream<TcpStream>>,
    pending: Option<Value>,
    started: bool,
}

fn error() -> io::Error {
    io::Error::other("fake structured Codex transport failed")
}

impl Terminal {
    pub fn connect() -> io::Result<Option<Self>> {
        let arguments: Vec<_> = env::args().collect();
        let Some(address) = arguments
            .windows(2)
            .find(|pair| pair[0] == "--remote")
            .map(|pair| &pair[1])
        else {
            return Ok(None);
        };
        let token_name = arguments
            .windows(2)
            .find(|pair| pair[0] == "--remote-auth-token-env")
            .map(|pair| &pair[1])
            .ok_or_else(error)?;
        let token = env::var(token_name).map_err(|_| error())?;
        let mut request = address
            .as_str()
            .into_client_request()
            .map_err(|_| error())?;
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {token}").parse().map_err(|_| error())?,
        );
        let (mut socket, _) = tungstenite::connect(request).map_err(|_| error())?;
        if let MaybeTlsStream::Plain(stream) = socket.get_mut() {
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        }
        socket.send(Message::Text(json!({"id":0,"method":"initialize","params":{"clientInfo":{"name":"fake-native","version":"1"}}}).to_string().into())).map_err(|_| error())?;
        socket.read().map_err(|_| error())?;
        Ok(Some(Self {
            socket,
            pending: None,
            started: false,
        }))
    }

    pub fn input(&mut self, line: &str, external_id: &str) -> io::Result<bool> {
        if let Some(id) = self.pending.clone() {
            if matches!(line, "answer" | "deny" | "cancel") {
                if id.is_null() {
                    self.send(&json!({"id":3,"method":"turn/start","params":{"threadId":external_id,"input":[{"type":"text","text":"answer-after-async"}]}}))?;
                    loop {
                        if self.read()?["method"] == "turn/started" {
                            break;
                        }
                    }
                    self.pending = None;
                    println!("FAKE_CODEX_CONTINUED");
                    return Ok(true);
                }
                self.send(&json!({"id":id,"result":{"decision":line,"answers":{"question":{"answers":["answer"]}}}}))?;
                loop {
                    let message = self.read()?;
                    if message["method"] == "serverRequest/resolved" {
                        self.pending = None;
                        println!("FAKE_CODEX_CONTINUED");
                        break;
                    }
                }
            } else {
                println!("FAKE_CODEX_WAITING_FOR_ANSWER");
            }
            return Ok(true);
        }
        if !matches!(
            line,
            "question"
                | "async-question"
                | "approval-real"
                | "file-approval"
                | "permissions-real"
                | "elicitation"
        ) {
            return Ok(false);
        }
        if !self.started {
            self.send(&json!({"id":1,"method":"thread/resume","params":{"threadId":external_id}}))?;
            loop {
                if self.read()?["id"] == 1 {
                    break;
                }
            }
            self.started = true;
        }
        self.send(&json!({"id":2,"method":"turn/start","params":{"threadId":external_id,"input":[{"type":"text","text":line}]}}))?;
        loop {
            let message = self.read()?;
            if message["method"] == "item/completed"
                && message["params"]["item"]["delivery"] == "async"
            {
                self.pending = Some(Value::Null);
                println!("FAKE_CODEX_AWAITING_INPUT");
                break;
            }
            if message["method"].as_str().is_some_and(|method| {
                method.contains("requestApproval")
                    || method.contains("requestUserInput")
                    || method.contains("elicitation/request")
            }) {
                if message["method"] == "item/tool/requestUserInput" {
                    assert!(
                        message["params"]["autoResolutionMs"].is_null(),
                        "host must not auto-answer"
                    );
                    assert_eq!(
                        message["params"]["isBlocking"], true,
                        "native question must wait for an answer"
                    );
                }
                self.pending = Some(message["id"].clone());
                println!("FAKE_CODEX_AWAITING_INPUT");
                break;
            }
        }
        Ok(true)
    }

    fn send(&mut self, message: &Value) -> io::Result<()> {
        self.socket
            .send(Message::Text(message.to_string().into()))
            .map_err(|_| error())
    }
    fn read(&mut self) -> io::Result<Value> {
        let message = self.socket.read().map_err(|_| error())?;
        serde_json::from_str(message.to_text().map_err(|_| error())?).map_err(|_| error())
    }
}

pub fn server() -> io::Result<()> {
    let _child = backend_child()?;
    let mut thread = String::new();
    let mut turn = 0;
    let mut pending = None;
    let mut paused = false;
    for line in io::stdin().lock().lines() {
        let message: Value = serde_json::from_str(&line?).map_err(|_| error())?;
        let id = &message["id"];
        match message["method"].as_str() {
            Some("initialize") => emit(&json!({"id":id,"result":{"userAgent":"fake-codex"}}))?,
            Some("thread/resume") => {
                thread = message["params"]["threadId"]
                    .as_str()
                    .ok_or_else(error)?
                    .into();
                emit(&json!({"id":id,"result":{"thread":{"id":thread}}}))?;
            }
            Some("turn/start") => {
                if pending.is_some() {
                    return Err(error());
                }
                turn += 1;
                emit(&json!({"id":id,"result":{"turn":{"id":format!("turn-{turn}")}}}))?;
                emit(
                    &json!({"method":"turn/started","params":{"threadId":thread,"turn":{"id":format!("turn-{turn}")}}}),
                )?;
                if message["params"]["input"][0]["text"] == "answer-after-async" {
                    if !paused {
                        return Err(error());
                    }
                    paused = false;
                    continue;
                }
                if message["params"]["input"][0]["text"] == "async-question" {
                    emit(
                        &json!({"method":"item/completed","params":{"threadId":thread,"turnId":format!("turn-{turn}"),"item":{"id":format!("question-{turn}"),"type":"agentMessage","delivery":"async","questions":[{"title":"Choose a value"}]}}}),
                    )?;
                    continue;
                }
                let method = match message["params"]["input"][0]["text"].as_str() {
                    Some("question") => "item/tool/requestUserInput",
                    Some("file-approval") => "item/fileChange/requestApproval",
                    Some("permissions-real") => "item/permissions/requestApproval",
                    Some("elicitation") => "mcpServer/elicitation/request",
                    _ => "item/commandExecution/requestApproval",
                };
                pending = Some(100 + turn);
                emit(
                    &json!({"id":100+turn,"method":method,"params":{"threadId":thread,"turnId":format!("turn-{turn}"),"autoResolutionMs":1,"questions":[{"id":"question","header":"Question","question":"Choose a value","options":[]}]}}),
                )?;
            }
            Some("turn/interrupt") => {
                paused = true;
                emit(&json!({"id":id,"result":{}}))?;
                emit(
                    &json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":format!("turn-{turn}"),"status":"interrupted"}}}),
                )?;
            }
            None if message.get("result").is_some() => {
                if pending != id.as_u64() {
                    return Err(error());
                }
                pending = None;
                emit(
                    &json!({"method":"serverRequest/resolved","params":{"threadId":thread,"requestId":id}}),
                )?;
            }
            _ => {}
        }
    }
    Ok(())
}

struct BackendChild(std::process::Child);

impl Drop for BackendChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn backend_child() -> io::Result<Option<BackendChild>> {
    let Some(home) = env::var_os("CODEX_HOME").map(std::path::PathBuf::from) else {
        return Ok(None);
    };
    let path = home.join(".fake-backend-child");
    if !path.is_file() {
        return Ok(None);
    }
    let executable = std::fs::read_to_string(path)?;
    let mut command = std::process::Command::new(executable);
    command
        .args(["heartbeat"])
        .arg(home.join("backend-heartbeat"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    Ok(Some(BackendChild(command.spawn()?)))
}

fn emit(message: &Value) -> io::Result<()> {
    let mut output = io::stdout().lock();
    writeln!(output, "{message}")?;
    output.flush()
}
