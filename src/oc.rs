//! opencode server + REST/SSE client + the network thread.
//!
//! All network work happens on a dedicated thread running its own tokio
//! runtime; the GPUI UI talks to it through two async channels (NetCommand in,
//! UiEvent out). The chat is driven purely by the /event stream: the message
//! POST is fire-and-forget, assistant output arrives as message.part.updated
//! events whose "part" carries the full text-so-far each time.
//!
//! A second harness, DeepSeek Harness (`dsh web`), rides the same thread: its
//! client and event translation live in [`crate::dsh`], keyed by the reserved
//! workspace key [`DSH_SERVER_KEY`].

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::settings::HarnessKind;

pub const ASK_CONFIG: &str = r#"{"$schema":"https://opencode.ai/config.json","permission":{"edit":"ask","bash":"ask","webfetch":"ask"}}"#;

/// Reserved net-thread key of the shared `dsh web` singleton. dsh sessions
/// carry their own cwd, so one process serves every workspace's dsh chats.
pub const DSH_SERVER_KEY: &str = "dsh";

// ---- reconnect policy ------------------------------------------------------

/// Backoff ladder start for the event pump's reconnect loop.
pub(crate) const RECONNECT_INITIAL: Duration = Duration::from_secs(1);
/// Backoff ladder cap.
pub(crate) const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// A connection that survived this long counts as stable: the next drop
/// restarts the ladder at [`RECONNECT_INITIAL`] instead of continuing it, and
/// the store closes the combined "connection lost" note.
pub const RECONNECT_RESET_AFTER: Duration = Duration::from_secs(60);

/// Give up reconnecting after this many consecutive failed tries; a
/// connection that stayed up for [`RECONNECT_RESET_AFTER`] restarts the count.
pub(crate) const RECONNECT_MAX_ATTEMPTS: u32 = 10;
/// /global/health poll period feeding the health gauge.
const HEALTH_POLL_INTERVAL: Duration = Duration::from_secs(10);

// ---- data types ----------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub updated_ms: i64,
    pub created_ms: i64,
    pub parent_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StoredMessage {
    pub id: String,
    pub role: String,
    pub created_ms: i64,
    pub info: Value,
    pub parts: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelInfo {
    pub key: String,
    /// Which backend serves the model; the two catalogs overlap on some
    /// names, so a key alone is not enough to resolve it.
    pub harness: HarnessKind,
    #[serde(default)]
    pub variants: Vec<String>,
    #[serde(default)]
    pub context_limit: i64,
}

#[derive(Clone, Debug)]
pub struct AgentInfo {
    pub name: String,
    pub mode: String,
    pub hidden: bool,
}

/// A server-side slash command or skill: GET /command. The template never
/// reaches the client — `/name args` runs via POST /session/{id}/command and
/// the server does the expansion (skills included).
#[derive(Clone, Debug)]
pub struct CommandInfo {
    pub name: String,
    /// `name.to_lowercase()`, computed once here: the menu filter runs it
    /// against every command on every keystroke and stream repaint
    pub name_lower: String,
    pub description: String,
    /// "command" (built-in / user markdown) or "skill"
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct QuestionOptionInfo {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct QuestionPromptInfo {
    pub question: String,
    pub header: String,
    pub multiple: bool,
    pub options: Vec<QuestionOptionInfo>,
}

/// A pending `question` tool ask: id (que_…), owning session and the questions in order.
#[derive(Clone, Debug)]
pub struct QuestionRequestInfo {
    pub id: String,
    pub session_id: String,
    pub questions: Vec<QuestionPromptInfo>,
}

// ---- server spawn --------------------------------------------------------

pub struct ServerProcess {
    pub base_url: String,
    /// dsh only: the launch token printed on stdout. opencode servers have no
    /// equivalent (None).
    pub token: Option<String>,
    /// behind Arc+Mutex so the health poll can watch for an early exit while
    /// the workspace map on the net thread keeps the owning handle; None for
    /// an attached (not spawned) server — there is nothing to watch
    child: Arc<Mutex<Option<tokio::process::Child>>>,
}

/// True when the spawned serve process exited on its own. A taken (stopped)
/// or never-spawned (attached) child reads false.
fn child_exited(probe: &Mutex<Option<tokio::process::Child>>) -> bool {
    let mut guard = probe.lock().expect("child lock");
    match guard.as_mut() {
        Some(child) => matches!(child.try_wait(), Ok(Some(_))),
        None => false,
    }
}

impl ServerProcess {
    pub async fn start(
        kind: HarnessKind,
        executable: &str,
        folder: &str,
        port: u16,
        on_log: impl Fn(String) + Clone + Send + Sync + 'static,
    ) -> Result<Self> {
        let base_url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()?;
        // dsh's launch token is per-process: an existing server cannot be
        // attached to (its token is unknowable), so always spawn our own
        if kind == HarnessKind::OpenCode && is_healthy(&client, &base_url).await {
            tracing::info!("attached to a healthy opencode server at {base_url}");
            on_log(format!(
                "attaching to a healthy opencode server at {base_url}"
            ));
            return Ok(ServerProcess {
                base_url,
                token: None,
                child: Arc::new(Mutex::new(None)),
            });
        }

        let mut command = tokio::process::Command::new(executable);
        match kind {
            HarnessKind::OpenCode => {
                command
                    .args([
                        "serve",
                        "--hostname",
                        "127.0.0.1",
                        "--port",
                        &port.to_string(),
                    ])
                    .env("OPENCODE_CONFIG_CONTENT", ASK_CONFIG);
            }
            HarnessKind::Dsh => {
                command.args([
                    "web",
                    "--no-open",
                    "--host",
                    "127.0.0.1",
                    "--port",
                    &port.to_string(),
                ]);
            }
        }
        command
            .current_dir(std::path::Path::new(
                if std::path::Path::new(folder).exists() {
                    folder
                } else {
                    "."
                },
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command.env_remove("ELECTRON_RUN_AS_NODE");
        // GUI app: without this flag Windows opens a visible console window
        // for the console-subsystem child process
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

        let mut child = command
            .spawn()
            .with_context(|| format!("'{executable}' failed to start"))?;
        tracing::info!("spawned {executable} (pid {:?}) in {folder}", child.id());

        // dsh prints its auth token on stdout; the wait loop below polls it
        let token_slot = Arc::new(Mutex::new(None::<String>));
        // stream stdout/stderr into the log
        let log = std::sync::Arc::new(on_log);
        if let Some(stdout) = child.stdout.take() {
            let log = log.clone();
            let token_slot = Arc::clone(&token_slot);
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(token) = crate::dsh::token_from_line(&line)
                        && token_slot.lock().expect("dsh token").is_none()
                    {
                        *token_slot.lock().expect("dsh token") = Some(token.to_string());
                    }
                    log(line);
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let log = log.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log(line);
                }
            });
        }

        let mut process = ServerProcess {
            base_url: base_url.clone(),
            token: None,
            child: Arc::new(Mutex::new(Some(child))),
        };

        // dsh is a node app — give its boot far more headroom than opencode
        let timeout = match kind {
            HarnessKind::OpenCode => Duration::from_secs(25),
            HarnessKind::Dsh => Duration::from_secs(40),
        };
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if std::time::Instant::now() >= deadline {
                let what = match kind {
                    HarnessKind::OpenCode => "opencode serve did not become healthy within 25 s.",
                    HarnessKind::Dsh => "dsh web did not print its URL within 40 s.",
                };
                tracing::warn!("{what}");
                return Err(anyhow!("{what}"));
            }
            {
                let mut guard = process.child.lock().expect("child lock");
                if let Some(child) = guard.as_mut()
                    && let Ok(Some(status)) = child.try_wait()
                {
                    tracing::warn!("server exited early with code {status}");
                    return Err(anyhow!("the server exited with code {status}."));
                }
            }
            match kind {
                HarnessKind::OpenCode => {
                    if is_healthy(&client, &base_url).await {
                        return Ok(process);
                    }
                }
                HarnessKind::Dsh => {
                    let token = token_slot.lock().expect("dsh token").clone();
                    if let Some(token) = token {
                        process.token = Some(token);
                        return Ok(process);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    pub fn stop(&mut self) {
        // take (not just kill): the health poll must not report a crash for a
        // process the user stopped, and kill_on_drop finishes the job
        if let Some(mut child) = self.child.lock().expect("child lock").take() {
            let _ = child.start_kill();
        }
    }
}

async fn is_healthy(client: &reqwest::Client, base_url: &str) -> bool {
    matches!(
        client
            .get(format!("{base_url}/global/health"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await,
        Ok(resp) if resp.status().is_success()
    )
}

pub fn find_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

// ---- REST + SSE client ---------------------------------------------------

// Clone shares the connection pool (reqwest::Client is an Arc inside) — the
// command loop clones the workspace's client per request instead of building
// a fresh pool + TLS config per request.
#[derive(Clone)]
pub struct ApiClient {
    /// The `/event` SSE connection: no total timeout (it lives as long as the
    /// session), only a bounded connect phase.
    stream_http: reqwest::Client,
    /// Short REST calls: bounded end to end so a wedged server cannot leave a
    /// tile loading forever (audit B11). The two long POSTs override this with
    /// their own 600 s timeout.
    rest: reqwest::Client,
    pub base_url: String,
}

impl ApiClient {
    pub fn new(base_url: &str) -> Self {
        let stream_http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let rest = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            stream_http,
            rest,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    pub async fn create_session(&self, title: &str) -> Result<SessionInfo> {
        let resp = self
            .rest
            .post(format!("{}/session", self.base_url))
            .json(&json!({ "title": title }))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        Ok(read_session(&value))
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let resp = self
            .rest
            .get(format!("{}/session", self.base_url))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        Ok(value
            .as_array()
            .map(|arr| arr.iter().map(read_session).collect())
            .unwrap_or_default())
    }

    pub async fn get_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>> {
        let resp = self
            .rest
            .get(format!("{}/session/{}/message", self.base_url, session_id))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        let mut messages = Vec::new();
        if let Some(arr) = value.as_array() {
            for m in arr {
                let info = m.get("info").cloned().unwrap_or(Value::Null);
                let (Some(id), Some(role)) = (
                    info.get("id").and_then(Value::as_str),
                    info.get("role").and_then(Value::as_str),
                ) else {
                    continue;
                };
                let created_ms = info
                    .pointer("/time/created")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let parts = m
                    .get("parts")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                messages.push(StoredMessage {
                    id: id.to_string(),
                    role: role.to_string(),
                    created_ms,
                    info,
                    parts,
                });
            }
        }
        Ok(messages)
    }

    pub async fn rename_session(&self, session_id: &str, title: &str) -> Result<SessionInfo> {
        let resp = self
            .rest
            .patch(format!("{}/session/{}", self.base_url, session_id))
            .json(&json!({ "title": title }))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        Ok(read_session(&value))
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let resp = self
            .rest
            .delete(format!("{}/session/{}", self.base_url, session_id))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn abort(&self, session_id: &str) -> Result<()> {
        let resp = self
            .rest
            .post(format!("{}/session/{}/abort", self.base_url, session_id))
            .json(&json!({}))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    /// Fire-and-forget chat send; the answer arrives on the event stream.
    pub async fn send_message(
        &self,
        session_id: &str,
        text: &str,
        model: Option<&str>,
        variant: Option<&str>,
        agent: Option<&str>,
        attachments: &[(String, String, std::sync::Arc<Vec<u8>>)], // (filename, mime, bytes)
    ) -> Result<()> {
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(json!({ "type": "text", "text": text }));
        }
        for (file_name, mime, data) in attachments {
            parts.push(json!({
                "type": "file",
                "mime": mime,
                "filename": file_name,
                "url": format!("data:{mime};base64,{}", base64_encode(data)),
            }));
        }
        let mut body = json!({ "parts": parts });
        if let Some(model) = model {
            let (provider_id, model_id) = match model.split_once('/') {
                Some((p, m)) => (p, m),
                None => (model, ""),
            };
            body["model"] = json!({ "providerID": provider_id, "modelID": model_id });
        }
        if let Some(variant) = variant {
            body["variant"] = json!(variant);
        }
        if let Some(agent) = agent {
            body["agent"] = json!(agent);
        }
        let resp = self
            .rest
            .post(format!("{}/session/{}/message", self.base_url, session_id))
            .json(&body)
            .timeout(Duration::from_secs(600))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    /// Server-side slash command / skill execution (POST /session/{id}/command):
    /// the server expands the template, honors the command's own agent/model
    /// config and starts the turn — the client never sees the expanded text.
    /// `model` is a "provider/model" string on this route, unlike the object
    /// shape the plain message route takes.
    #[expect(
        clippy::too_many_arguments,
        reason = "the parameter list mirrors the server's POST body one-to-one"
    )]
    pub async fn send_command(
        &self,
        session_id: &str,
        name: &str,
        args: &str,
        model: Option<&str>,
        variant: Option<&str>,
        agent: Option<&str>,
        attachments: &[(String, String, std::sync::Arc<Vec<u8>>)], // (filename, mime, bytes)
    ) -> Result<()> {
        let mut body = json!({ "command": name, "arguments": args });
        if let Some(model) = model {
            body["model"] = json!(model);
        }
        if let Some(variant) = variant {
            body["variant"] = json!(variant);
        }
        if let Some(agent) = agent {
            body["agent"] = json!(agent);
        }
        if !attachments.is_empty() {
            let parts: Vec<_> = attachments
                .iter()
                .map(|(file_name, mime, data)| {
                    json!({
                        "type": "file",
                        "mime": mime,
                        "filename": file_name,
                        "url": format!("data:{mime};base64,{}", base64_encode(data)),
                    })
                })
                .collect();
            body["parts"] = json!(parts);
        }
        let resp = self
            .rest
            .post(format!("{}/session/{}/command", self.base_url, session_id))
            .json(&body)
            .timeout(Duration::from_secs(600))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn respond_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        response: &str,
    ) -> Result<()> {
        let resp = self
            .rest
            .post(format!(
                "{}/session/{}/permissions/{}",
                self.base_url, session_id, permission_id
            ))
            .json(&json!({ "response": response }))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn reply_v2(&self, request_id: &str, reply: &str) -> Result<()> {
        let resp = self
            .rest
            .post(format!("{}/permission/{}/reply", self.base_url, request_id))
            .json(&json!({ "reply": reply }))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn get_questions(&self) -> Result<Vec<QuestionRequestInfo>> {
        let resp = self
            .rest
            .get(format!("{}/question", self.base_url))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        Ok(value
            .as_array()
            .map(|arr| arr.iter().filter_map(read_question_request).collect())
            .unwrap_or_default())
    }

    pub async fn reply_question(&self, request_id: &str, answers: &[Vec<String>]) -> Result<()> {
        let resp = self
            .rest
            .post(format!("{}/question/{}/reply", self.base_url, request_id))
            .json(&json!({ "answers": answers }))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn reject_question(&self, request_id: &str) -> Result<()> {
        let resp = self
            .rest
            .post(format!("{}/question/{}/reject", self.base_url, request_id))
            .json(&json!({}))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn get_agents(&self) -> Result<Vec<AgentInfo>> {
        let resp = self
            .rest
            .get(format!("{}/agent", self.base_url))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        let mut agents = Vec::new();
        if let Some(arr) = value.as_array() {
            for a in arr {
                let Some(name) = a.get("name").and_then(Value::as_str) else {
                    continue;
                };
                agents.push(AgentInfo {
                    name: name.to_string(),
                    mode: a.get("mode").and_then(Value::as_str).unwrap_or("").into(),
                    hidden: a.get("hidden").and_then(Value::as_bool).unwrap_or(false),
                });
            }
        }
        Ok(agents)
    }

    /// The server's slash commands and skills, in server order.
    pub async fn get_commands(&self) -> Result<Vec<CommandInfo>> {
        let resp = self
            .rest
            .get(format!("{}/command", self.base_url))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        let mut commands = Vec::new();
        if let Some(arr) = value.as_array() {
            for c in arr {
                let Some(name) = c.get("name").and_then(Value::as_str) else {
                    continue;
                };
                commands.push(CommandInfo {
                    name_lower: name.to_lowercase(),
                    name: name.to_string(),
                    description: c
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    source: c
                        .get("source")
                        .and_then(Value::as_str)
                        .unwrap_or("command")
                        .into(),
                });
            }
        }
        Ok(commands)
    }

    /// Filename search over the server's working directory (the `@` mention
    /// source); paths come back as the server reports them, usually relative.
    pub async fn find_files(&self, query: &str) -> Result<Vec<String>> {
        let resp = self
            .rest
            .get(format!("{}/find/file", self.base_url))
            .query(&[("query", query)])
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        Ok(value
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// The server's merged config (global + project + env content). The `mcp`
    /// section lists the workspace's configured MCP servers.
    pub async fn get_config(&self) -> Result<Value> {
        let resp = self
            .rest
            .get(format!("{}/config", self.base_url))
            .send()
            .await?;
        error_for_status(resp)
            .await?
            .json()
            .await
            .map_err(anyhow::Error::from)
    }

    /// Runtime MCP toggles: they change which tools the running server exposes
    /// immediately, touch no config file, and a 404 just means the entry
    /// vanished from config between listing and acting — still a success.
    pub async fn mcp_connect(&self, name: &str) -> Result<()> {
        let resp = self
            .rest
            .post(format!("{}/mcp/{}/connect", self.base_url, name))
            .json(&json!({}))
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn mcp_disconnect(&self, name: &str) -> Result<()> {
        let resp = self
            .rest
            .post(format!("{}/mcp/{}/disconnect", self.base_url, name))
            .json(&json!({}))
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn get_models(&self) -> Result<Vec<ModelInfo>> {
        let resp = self
            .rest
            .get(format!("{}/config/providers", self.base_url))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        let mut models = Vec::new();
        if let Some(providers) = value.get("providers").and_then(Value::as_array) {
            for provider in providers {
                let Some(pid) = provider.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(models_map) = provider.get("models").and_then(Value::as_object) else {
                    continue;
                };
                for (name, m) in models_map {
                    let mut variants: Vec<String> = m
                        .get("variants")
                        .and_then(Value::as_object)
                        .map(|v| v.keys().cloned().collect())
                        .unwrap_or_default();
                    // picker order = effort tiers low→max, not alphabetical
                    variants.sort_by_key(|v| (crate::store::variant_rank(v), v.to_lowercase()));
                    let context_limit = m
                        .pointer("/limit/context")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    models.push(ModelInfo {
                        key: format!("{pid}/{name}"),
                        harness: HarnessKind::OpenCode,
                        variants,
                        context_limit,
                    });
                }
            }
        }
        Ok(models)
    }

    /// Yields events from the /event SSE stream until it ends. `on_attached`
    /// fires once the server accepted the stream (response status is in) —
    /// the reconnect pump reports Connected from there, not from a mere
    /// attempt.
    pub async fn stream_events(
        &self,
        on_event: impl Fn(Value),
        on_attached: impl Fn(),
        stop: async_channel::Receiver<()>,
    ) -> Result<()> {
        use futures_util::StreamExt as _;
        let resp = self
            .stream_http
            .get(format!("{}/event", self.base_url))
            .send()
            .await?;
        let resp = error_for_status(resp).await?;
        on_attached();
        let mut stream = resp.bytes_stream();
        let mut buffer = Vec::new();
        loop {
            tokio::select! {
                _ = stop.recv() => return Ok(()),
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => buffer.extend_from_slice(&bytes),
                        Some(Err(err)) => return Err(err.into()),
                        None => return Ok(()),
                    }
                }
            }
            // extract complete lines
            while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buffer.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line[..line.len() - 1]);
                let line = line.trim_end_matches('\r');
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() {
                    continue;
                }
                if let Ok(evt) = serde_json::from_str::<Value>(payload) {
                    on_event(evt);
                }
            }
        }
    }
}

async fn error_for_status(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        let detail = truncate(&detail, 1000);
        tracing::warn!("request failed: {status} — {detail}");
        return Err(anyhow!(
            "{status}{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(" — {detail}")
            }
        ));
    }
    Ok(resp)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // byte budgets must land on a char boundary or multibyte text panics
        let mut cut = max;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

fn read_session(s: &Value) -> SessionInfo {
    SessionInfo {
        id: s.get("id").and_then(Value::as_str).unwrap_or("").into(),
        title: s.get("title").and_then(Value::as_str).unwrap_or("").into(),
        updated_ms: s
            .pointer("/time/updated")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        created_ms: s
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        parent_id: s.get("parentID").and_then(Value::as_str).map(String::from),
    }
}

/// Shared by the /question listing and the question.asked event — same Request shape.
pub fn read_question_request(r: &Value) -> Option<QuestionRequestInfo> {
    let id = r.get("id").and_then(Value::as_str)?;
    let session_id = r.get("sessionID").and_then(Value::as_str)?;
    let questions_value = r.get("questions")?.as_array()?;
    let mut questions = Vec::new();
    for q in questions_value {
        let options = q
            .get("options")
            .and_then(Value::as_array)
            .map(|os| {
                os.iter()
                    .filter_map(|o| {
                        let label = o.get("label").and_then(Value::as_str)?;
                        Some(QuestionOptionInfo {
                            label: label.into(),
                            description: o
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .into(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        questions.push(QuestionPromptInfo {
            question: q
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            header: q.get("header").and_then(Value::as_str).unwrap_or("").into(),
            multiple: q.get("multiple").and_then(Value::as_bool).unwrap_or(false),
            options,
        });
    }
    Some(QuestionRequestInfo {
        id: id.into(),
        session_id: session_id.into(),
        questions,
    })
}

pub(crate) fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// ---- net thread ----------------------------------------------------------

pub enum NetCommand {
    /// Spawn (or attach to) the server for a workspace and start its event pump.
    /// `ws_key` is [`DSH_SERVER_KEY`] for the shared dsh singleton.
    StartServer {
        ws_key: String,
        kind: HarnessKind,
        executable: String,
        folder: String,
    },
    /// Stop the workspace's server and event pump.
    StopServer { ws_key: String },
    /// Run a request against the workspace's API; results come back as UiEvents.
    Request { ws_key: String, request: ApiRequest },
    /// Fire-and-forget message send. `command` is `(name, args)` when the text
    /// parsed as a server command/skill — it goes to the /command route
    /// instead of a plain prompt, and the server expands the template.
    SendMessage {
        ws_key: String,
        session_id: String,
        text: String,
        model: Option<String>,
        variant: Option<String>,
        agent: Option<String>,
        attachments: Vec<(String, String, std::sync::Arc<Vec<u8>>)>,
        command: Option<(String, String)>,
    },
    /// Fan a pre-serialized event frame out to every connected door client.
    IpcBroadcast { line: String },
}

pub enum ApiRequest {
    ListSessions,
    /// dsh: one workspace folder's sessions, filtered by cwd from the shared
    /// server's list. The reply carries the folder as ws_key so the store
    /// applies it like that workspace's own list.
    ListDshSessions {
        folder: String,
    },
    /// dsh: create a session owned by `folder` (dsh sessions carry their own
    /// cwd). The reply carries the folder as ws_key.
    CreateDshSession {
        folder: String,
        title: String,
    },
    /// dsh: the fork flow — an empty session whose composer is seeded with
    /// `context` (the dsh server has no parts-append endpoint either).
    CreateDshForkSession {
        folder: String,
        title: String,
        context: String,
    },
    ListModels,
    ListAgents,
    GetMessages {
        session_id: String,
    },
    CreateSession {
        title: String,
    },
    /// Fork: create an empty session for a transcript seed; the context rides
    /// back on the event because it goes into the new tile's composer, never
    /// to the model (the serve API cannot append parts without a turn).
    CreateForkSession {
        title: String,
        context: String,
    },
    RenameSession {
        session_id: String,
        title: String,
    },
    DeleteSession {
        session_id: String,
    },
    Abort {
        session_id: String,
    },
    RespondPermission {
        session_id: String,
        permission_id: String,
        response: String,
    },
    ReplyV2 {
        request_id: String,
        response: String,
    },
    GetQuestions {
        for_session: String,
    },
    ReplyQuestion {
        request_id: String,
        answers: Vec<Vec<String>>,
    },
    RejectQuestion {
        request_id: String,
    },
    /// The workspace's slash commands and skills (GET /command).
    ListCommands,
    /// Filename search for the `@` mention menu (GET /find/file).
    FindFiles {
        query: String,
    },
    /// Names of the workspace's configured MCP servers (GET /config "mcp" section).
    ListMcpNames,
    /// Runtime MCP toggle for one configured server; instant, no config write.
    McpConnect {
        name: String,
    },
    McpDisconnect {
        name: String,
    },
}

/// Live state of a workspace's event pump. Connected is reported on every
/// successful SSE attach, Reconnecting once per drop of a connected stream
/// (carrying the failing attempt number of [`RECONNECT_MAX_ATTEMPTS`]),
/// Failed when those tries ran out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamStatus {
    Connected,
    Reconnecting { attempt: u32 },
    Failed,
}

/// Everything the UI learns from the network thread arrives as one of these.
pub enum UiEvent {
    ServerStarted {
        ws_key: String,
        result: Result<()>,
    },
    ServerStopped,
    /// the spawned serve process exited on its own (not a user stop)
    ServerCrashed {
        ws_key: String,
    },
    Event {
        ws_key: String,
        event: Value,
    },
    StreamStatus {
        ws_key: String,
        status: StreamStatus,
    },
    /// Latest /global/health answer; None until the first poll lands.
    HealthChanged {
        ws_key: String,
        healthy: Option<bool>,
    },
    SessionsLoaded {
        ws_key: String,
        /// True when the reply came from the shared dsh server (a dsh list
        /// must never prune the same workspace's opencode chats, and vice
        /// versa); carried on the event so replies can never be misattributed
        dsh: bool,
        result: Result<Vec<SessionInfo>>,
    },
    MessagesLoaded {
        ws_key: String,
        session_id: String,
        result: Result<Vec<StoredMessage>>,
    },
    SessionCreated {
        ws_key: String,
        result: Result<SessionInfo>,
    },
    ForkSessionCreated {
        ws_key: String,
        context: String,
        result: Result<SessionInfo>,
    },
    SessionRenamed {
        session_id: String,
        result: Result<SessionInfo>,
    },
    SessionDeleted {
        session_id: String,
        result: Result<()>,
    },
    Aborted {
        session_id: String,
        result: Result<()>,
    },
    PermissionReplied {
        permission_id: String,
        response: String,
        is_auto: bool,
        result: Result<()>,
    },
    QuestionsPulled {
        for_session: String,
        result: Result<Vec<QuestionRequestInfo>>,
    },
    QuestionReplied {
        request_id: String,
        summary: String,
        result: Result<()>,
    },
    ModelsLoaded {
        harness: HarnessKind,
        result: Result<Vec<ModelInfo>>,
    },
    AgentsLoaded {
        result: Result<Vec<AgentInfo>>,
    },
    CommandsLoaded {
        ws_key: String,
        result: Result<Vec<CommandInfo>>,
    },
    /// Answer to a FindFiles request; the query rides along so stale answers
    /// can be told apart from the menu's current token.
    FilesFound {
        ws_key: String,
        query: String,
        result: Result<Vec<String>>,
    },
    /// The workspace's configured MCP server names, in stable name order.
    McpNamesLoaded {
        ws_key: String,
        result: Result<Vec<String>>,
    },
    MessageSendFailed {
        session_id: String,
        error: String,
    },
    Log {
        ws_key: String,
        line: String,
    },
    /// A door command relayed from the ipc listener; answered by the store.
    Ipc {
        command: crate::ipc::IpcCommand,
    },
}

struct WorkspaceNet {
    kind: HarnessKind,
    process: Option<ServerProcess>,
    api: ApiClient,
    /// set when kind is Dsh: the shared web client + its event pump feeds
    /// UiEvents that are already opencode-shaped
    dsh: Option<crate::dsh::DshHandle>,
    event_stop: Option<async_channel::Sender<()>>,
}

/// Cloneable handle the UI uses to talk to the net thread.
#[derive(Clone)]
pub struct NetSender(pub async_channel::Sender<NetCommand>);

impl NetSender {
    pub fn send(&self, command: NetCommand) {
        if let Err(err) = self.0.send_blocking(command) {
            tracing::error!("net thread gone: {err}");
        }
    }
}

pub fn spawn_net_thread() -> (
    std::thread::JoinHandle<()>,
    NetSender,
    async_channel::Receiver<UiEvent>,
) {
    let (command_tx, command_rx) = async_channel::unbounded::<NetCommand>();
    let (event_tx, event_rx) = async_channel::unbounded::<UiEvent>();
    let handle = std::thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(net_main(command_rx, event_tx));
        })
        .expect("net thread");
    (handle, NetSender(command_tx), event_rx)
}

async fn net_main(
    command_rx: async_channel::Receiver<NetCommand>,
    event_tx: async_channel::Sender<UiEvent>,
) {
    let mut workspaces: HashMap<String, WorkspaceNet> = HashMap::new();
    // the DevTools door rides this runtime; a bind failure logs and the app
    // keeps working — the door is an integration, not a dependency
    let (door_tx, _) = tokio::sync::broadcast::channel::<String>(64);
    let door = match crate::ipc::start(event_tx.clone(), door_tx.clone()).await {
        Ok(door) => Some(door),
        Err(err) => {
            tracing::warn!("ipc door unavailable: {err:#}");
            None
        }
    };
    while let Ok(command) = command_rx.recv().await {
        match command {
            NetCommand::StartServer {
                ws_key,
                kind,
                executable,
                folder,
            } => {
                let event_tx = event_tx.clone();
                // sequential spawn keeps one workspace's failed start from racing others
                match start_workspace(
                    &mut workspaces,
                    &ws_key,
                    kind,
                    &executable,
                    &folder,
                    &event_tx,
                )
                .await
                {
                    Ok(()) => {
                        // async_channel::send is a future — dropping it without
                        // awaiting silently discards the event
                        let _ = event_tx
                            .send(UiEvent::ServerStarted {
                                ws_key: ws_key.clone(),
                                result: Ok(()),
                            })
                            .await;
                    }
                    Err(err) => {
                        workspaces.remove(&ws_key);
                        let _ = event_tx
                            .send(UiEvent::ServerStarted {
                                ws_key,
                                result: Err(anyhow!("{err}")),
                            })
                            .await;
                    }
                }
            }
            NetCommand::StopServer { ws_key } => {
                if let Some(ws) = workspaces.get_mut(&ws_key) {
                    // close (not a single token): every task selecting on this
                    // channel — event pump and health poll — wakes and exits
                    if let Some(stop) = ws.event_stop.take() {
                        stop.close();
                    }
                    if let Some(process) = ws.process.as_mut() {
                        process.stop();
                    }
                }
                workspaces.remove(&ws_key);
                let _ = event_tx.send(UiEvent::ServerStopped).await;
            }
            NetCommand::Request { ws_key, request } => {
                let Some(ws) = workspaces.get(&ws_key) else {
                    continue;
                };
                let event_tx = event_tx.clone();
                match ws.kind {
                    HarnessKind::OpenCode => {
                        let client = ws.api.clone();
                        tokio::spawn(async move {
                            let event = run_request(&client, ws_key, request).await;
                            let _ = event_tx.send(event).await;
                        });
                    }
                    HarnessKind::Dsh => {
                        let Some(handle) = ws.dsh.clone() else {
                            continue;
                        };
                        tokio::spawn(async move {
                            let event = crate::dsh::run_request(&handle, ws_key, request).await;
                            let _ = event_tx.send(event).await;
                        });
                    }
                }
            }
            NetCommand::SendMessage {
                ws_key,
                session_id,
                text,
                model,
                variant,
                agent,
                attachments,
                command,
            } => {
                let Some(ws) = workspaces.get(&ws_key) else {
                    // don't drop silently — the tile would sit "running" forever
                    let _ = event_tx
                        .send(UiEvent::MessageSendFailed {
                            session_id: session_id.clone(),
                            error: "this chat's workspace server is not running".into(),
                        })
                        .await;
                    continue;
                };
                let event_tx = event_tx.clone();
                match ws.kind {
                    HarnessKind::OpenCode => {
                        let client = ws.api.clone();
                        tokio::spawn(async move {
                            send_with_blips(
                                &client,
                                session_id,
                                text,
                                model,
                                variant,
                                agent,
                                attachments,
                                command,
                                &event_tx,
                            )
                            .await;
                        });
                    }
                    HarnessKind::Dsh => {
                        let Some(handle) = ws.dsh.clone() else {
                            let _ = event_tx
                                .send(UiEvent::MessageSendFailed {
                                    session_id: session_id.clone(),
                                    error: "this chat's workspace server is not running".into(),
                                })
                                .await;
                            continue;
                        };
                        tokio::spawn(async move {
                            // slash text goes out verbatim: dsh expands it itself
                            let result = crate::dsh::send_message(
                                &handle,
                                &session_id,
                                &text,
                                model.as_deref(),
                                variant.as_deref(),
                                agent.as_deref(),
                                &attachments,
                            )
                            .await;
                            if let Err(err) = result {
                                let _ = event_tx
                                    .send(UiEvent::MessageSendFailed {
                                        session_id,
                                        error: format!("{err:#}"),
                                    })
                                    .await;
                            }
                        });
                    }
                }
            }
            NetCommand::IpcBroadcast { line } => {
                let _ = door_tx.send(line);
            }
        }
    }
    // shutdown: kill every spawned server
    for (_, mut ws) in workspaces {
        if let Some(process) = ws.process.as_mut() {
            process.stop();
        }
    }
    if let Some(door) = &door {
        crate::ipc::unpublish(door);
    }
}

async fn start_workspace(
    workspaces: &mut HashMap<String, WorkspaceNet>,
    ws_key: &str,
    kind: HarnessKind,
    executable: &str,
    folder: &str,
    event_tx: &async_channel::Sender<UiEvent>,
) -> Result<()> {
    if workspaces
        .get(ws_key)
        .map(|ws| ws.process.is_some())
        .unwrap_or(false)
    {
        // already running: reuse
        return Ok(());
    }
    let port = find_free_port()?;
    let log_tx = event_tx.clone();
    let key = ws_key.to_string();
    let process = ServerProcess::start(
        kind,
        executable,
        folder,
        port,
        Box::new(move |line| {
            // sync closure: send_blocking; unbounded channel so it never blocks
            let _ = log_tx.send_blocking(UiEvent::Log {
                ws_key: key.clone(),
                line,
            });
        }),
    )
    .await?;
    let base_url = process.base_url.clone();
    let api = ApiClient::new(&base_url);

    if kind == HarnessKind::Dsh {
        let token = process
            .token
            .clone()
            .ok_or_else(|| anyhow!("dsh web started without a launch token"))?;
        let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
        let (frame_tx, frame_rx) = async_channel::bounded::<String>(64);
        let handle = crate::dsh::DshHandle::new(&base_url, &token, frame_tx);
        let pump_tx = event_tx.clone();
        let pump_key = ws_key.to_string();
        let pump_handle = handle.clone();
        let pump_stop = stop_rx.clone();
        tokio::spawn(async move {
            crate::dsh::run_pump(pump_handle, pump_key, pump_tx, pump_stop, frame_rx).await;
        });
        // liveness: the child process plus the HTTP server answering at all
        // (any status — the trust fence's 401 included); the WS connection
        // state itself drives StreamStatus
        let poll_tx = event_tx.clone();
        let poll_key = ws_key.to_string();
        let poll_handle = handle.clone();
        let poll_probe = Arc::clone(&process.child);
        tokio::spawn(async move {
            loop {
                if child_exited(&poll_probe) {
                    tracing::warn!("[{poll_key}] dsh web exited — reporting crash");
                    let _ = poll_tx
                        .send(UiEvent::ServerCrashed {
                            ws_key: poll_key.clone(),
                        })
                        .await;
                    return;
                }
                let healthy = tokio::select! {
                    _ = stop_rx.recv() => return,
                    healthy = poll_handle.alive() => healthy,
                };
                let _ = poll_tx
                    .send(UiEvent::HealthChanged {
                        ws_key: poll_key.clone(),
                        healthy: Some(healthy),
                    })
                    .await;
                tokio::select! {
                    _ = stop_rx.recv() => return,
                    _ = tokio::time::sleep(HEALTH_POLL_INTERVAL) => {}
                }
            }
        });
        workspaces.insert(
            ws_key.to_string(),
            WorkspaceNet {
                kind,
                process: Some(process),
                api,
                dsh: Some(handle),
                event_stop: Some(stop_tx),
            },
        );
        return Ok(());
    }

    // event pump + health poll; both select on the same stop channel, closed
    // by StopServer to cancel them together
    let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
    let pump_tx = event_tx.clone();
    let pump_key = ws_key.to_string();
    let pump_client = ApiClient::new(&base_url);
    let was_connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pump_stop = stop_rx.clone(); // the health poll below keeps the original
    tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        let stop_rx = pump_stop;
        let mut attempts = 0u32;
        let mut delay = RECONNECT_INITIAL;
        loop {
            // per-attempt cancel channel: shutdown goes through stop_rx (the
            // select below drops the whole future, closing the SSE response);
            // this one only satisfies stream_events' own select for the pass
            let (cancel_tx, cancel_rx) = async_channel::bounded::<()>(1);
            let started = std::time::Instant::now();
            let on_event = {
                let pump_tx = pump_tx.clone();
                let pump_key = pump_key.clone();
                move |event: Value| {
                    // sync callback: send_blocking; unbounded channel so it never blocks
                    let _ = pump_tx.send_blocking(UiEvent::Event {
                        ws_key: pump_key.clone(),
                        event,
                    });
                }
            };
            let on_attached = {
                let pump_tx = pump_tx.clone();
                let pump_key = pump_key.clone();
                let was_connected = was_connected.clone();
                move || {
                    was_connected.store(true, Ordering::Release);
                    let _ = pump_tx.send_blocking(UiEvent::StreamStatus {
                        ws_key: pump_key.clone(),
                        status: StreamStatus::Connected,
                    });
                }
            };
            tokio::select! {
                _ = stop_rx.recv() => return,
                result = pump_client.stream_events(on_event, on_attached, cancel_rx) => {
                    drop(cancel_tx);
                    if let Err(err) = result {
                        tracing::warn!("[{pump_key}] event stream dropped: {err:#}");
                    } else {
                        tracing::info!("[{pump_key}] event stream ended");
                    }
                }
            }
            // a connection that lasted a while restarts the ladder and the
            // attempt count; a fast drop continues both, so flapping servers
            // pace themselves out and eventually give up
            if started.elapsed() >= RECONNECT_RESET_AFTER {
                attempts = 0;
                delay = RECONNECT_INITIAL;
            }
            attempts += 1;
            if attempts >= RECONNECT_MAX_ATTEMPTS {
                tracing::error!(
                    "[{pump_key}] event stream still down after {RECONNECT_MAX_ATTEMPTS} attempts — giving up"
                );
                let _ = pump_tx.send_blocking(UiEvent::StreamStatus {
                    ws_key: pump_key.clone(),
                    status: StreamStatus::Failed,
                });
                return;
            }
            // one Reconnecting per drop — retries inside the gate below stay quiet
            if was_connected.swap(false, Ordering::AcqRel) {
                let _ = pump_tx.send_blocking(UiEvent::StreamStatus {
                    ws_key: pump_key.clone(),
                    status: StreamStatus::Reconnecting { attempt: attempts },
                });
            }
            // paced wait + health gate: the sleep is the pacing mechanism, the
            // health probe only keeps a still-down server from churning SSE
            // handshakes at every tick. Every failure doubles the wait —
            // a failed probe while the server is down, and a failed attach
            // once it answers again — unlike doubling on probes alone, which
            // kept a health-but-not-SSE server retrying once a second forever.
            loop {
                tokio::select! {
                    _ = stop_rx.recv() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
                let healthy = tokio::select! {
                    _ = stop_rx.recv() => return,
                    healthy = is_healthy(&pump_client.stream_http, &pump_client.base_url) => healthy,
                };
                if healthy {
                    break;
                }
                tracing::debug!("[{pump_key}] server still down, next retry in {delay:?}");
                delay = (delay * 2).min(RECONNECT_MAX);
            }
            delay = (delay * 2).min(RECONNECT_MAX);
        }
    });

    // server health poll: /global/health every 10 s feeds the gauge's
    // "connected but degraded" color; the store dedupes unchanged answers.
    // Each tick first checks the spawned child: an exited serve can never
    // come back on its own, so the store gets a ServerCrashed to restart it
    let poll_tx = event_tx.clone();
    let poll_key = ws_key.to_string();
    let poll_client = ApiClient::new(&base_url);
    let poll_probe = Arc::clone(&process.child);
    tokio::spawn(async move {
        loop {
            if child_exited(&poll_probe) {
                tracing::warn!("[{poll_key}] serve process exited — reporting crash");
                let _ = poll_tx
                    .send(UiEvent::ServerCrashed {
                        ws_key: poll_key.clone(),
                    })
                    .await;
                return;
            }
            let healthy = tokio::select! {
                _ = stop_rx.recv() => return,
                healthy = is_healthy(&poll_client.stream_http, &poll_client.base_url) => healthy,
            };
            let _ = poll_tx
                .send(UiEvent::HealthChanged {
                    ws_key: poll_key.clone(),
                    healthy: Some(healthy),
                })
                .await;
            tokio::select! {
                _ = stop_rx.recv() => return,
                _ = tokio::time::sleep(HEALTH_POLL_INTERVAL) => {}
            }
        }
    });

    workspaces.insert(
        ws_key.to_string(),
        WorkspaceNet {
            kind,
            process: Some(process),
            api,
            dsh: None,
            event_stop: Some(stop_tx),
        },
    );
    Ok(())
}

/// opencode send with the quiet connect-blip retry (a failure in the connect
/// phase never reached the server, so a re-POST cannot double-send).
#[expect(
    clippy::too_many_arguments,
    reason = "single private caller already holds these pieces; a param struct would just re-pack them"
)]
async fn send_with_blips(
    client: &ApiClient,
    session_id: String,
    text: String,
    model: Option<String>,
    variant: Option<String>,
    agent: Option<String>,
    attachments: Vec<(String, String, std::sync::Arc<Vec<u8>>)>,
    command: Option<(String, String)>,
    event_tx: &async_channel::Sender<UiEvent>,
) {
    let mut blips = 0u32;
    let result = loop {
        let attempt = match &command {
            Some((name, args)) => {
                client
                    .send_command(
                        &session_id,
                        name,
                        args,
                        model.as_deref(),
                        variant.as_deref(),
                        agent.as_deref(),
                        &attachments,
                    )
                    .await
            }
            None => {
                client
                    .send_message(
                        &session_id,
                        &text,
                        model.as_deref(),
                        variant.as_deref(),
                        agent.as_deref(),
                        &attachments,
                    )
                    .await
            }
        };
        match attempt {
            Ok(()) => break Ok(()),
            Err(err) if blips < 2 && is_connect_blip(&err) => {
                blips += 1;
                tracing::warn!("send to {session_id} could not connect, retry {blips}/2: {err:#}");
                tokio::time::sleep(Duration::from_millis(1500 * blips as u64)).await;
            }
            Err(err) => break Err(err),
        }
    };
    if let Err(err) = result {
        let _ = event_tx
            .send(UiEvent::MessageSendFailed {
                session_id,
                error: format!("{err:#}"),
            })
            .await;
    }
}

/// True when a request failed in the connect phase (nothing was sent, the
/// server was unreachable) — a re-POST cannot double-send.
fn is_connect_blip(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .any(|e| e.is_connect())
}

async fn run_request(client: &ApiClient, ws_key: String, request: ApiRequest) -> UiEvent {
    match request {
        ApiRequest::ListSessions => UiEvent::SessionsLoaded {
            ws_key,
            dsh: false,
            result: client.list_sessions().await,
        },
        // dsh-only requests never reach the opencode runner (net_main routes
        // by workspace kind); answering with a list would prune the workspace's
        // real dsh chats, so log the misrouting instead
        ApiRequest::ListDshSessions { folder } => UiEvent::Log {
            ws_key,
            line: format!("dsh session list for {folder} misrouted to an opencode runner"),
        },
        ApiRequest::CreateDshSession { folder, .. } => UiEvent::SessionCreated {
            ws_key: folder,
            result: Err(anyhow!("dsh request routed to an opencode server")),
        },
        ApiRequest::CreateDshForkSession { folder, .. } => UiEvent::ForkSessionCreated {
            ws_key: folder,
            context: String::new(),
            result: Err(anyhow!("dsh request routed to an opencode server")),
        },
        ApiRequest::ListModels => UiEvent::ModelsLoaded {
            harness: HarnessKind::OpenCode,
            result: client.get_models().await,
        },
        ApiRequest::ListAgents => UiEvent::AgentsLoaded {
            result: client.get_agents().await,
        },
        ApiRequest::GetMessages { session_id } => UiEvent::MessagesLoaded {
            ws_key,
            session_id: session_id.clone(),
            result: client.get_messages(&session_id).await,
        },
        ApiRequest::CreateSession { title } => UiEvent::SessionCreated {
            ws_key,
            result: client.create_session(&title).await,
        },
        ApiRequest::CreateForkSession { title, context } => UiEvent::ForkSessionCreated {
            ws_key,
            context,
            result: client.create_session(&title).await,
        },
        ApiRequest::RenameSession { session_id, title } => {
            let result = client.rename_session(&session_id, &title).await;
            UiEvent::SessionRenamed { session_id, result }
        }
        ApiRequest::DeleteSession { session_id } => {
            let result = client.delete_session(&session_id).await;
            UiEvent::SessionDeleted { session_id, result }
        }
        ApiRequest::Abort { session_id } => {
            let result = client.abort(&session_id).await;
            UiEvent::Aborted { session_id, result }
        }
        ApiRequest::RespondPermission {
            session_id,
            permission_id,
            response,
        } => {
            let result = client
                .respond_permission(&session_id, &permission_id, &response)
                .await;
            UiEvent::PermissionReplied {
                permission_id,
                response,
                is_auto: false,
                result,
            }
        }
        ApiRequest::ReplyV2 {
            request_id,
            response,
        } => {
            let result = client.reply_v2(&request_id, &response).await;
            UiEvent::PermissionReplied {
                permission_id: request_id,
                response,
                is_auto: false,
                result,
            }
        }
        ApiRequest::GetQuestions { for_session } => UiEvent::QuestionsPulled {
            for_session,
            result: client.get_questions().await,
        },
        ApiRequest::ReplyQuestion {
            request_id,
            answers,
        } => {
            let summary = answers
                .iter()
                .map(|a| {
                    if a.is_empty() {
                        "(empty)".to_string()
                    } else {
                        a.join(", ")
                    }
                })
                .collect::<Vec<_>>()
                .join(" · ");
            let result = client.reply_question(&request_id, &answers).await;
            UiEvent::QuestionReplied {
                request_id,
                summary,
                result,
            }
        }
        ApiRequest::RejectQuestion { request_id } => {
            let result = client.reject_question(&request_id).await;
            UiEvent::QuestionReplied {
                request_id,
                summary: "dismissed".into(),
                result,
            }
        }
        ApiRequest::ListCommands => UiEvent::CommandsLoaded {
            ws_key,
            result: client.get_commands().await,
        },
        ApiRequest::FindFiles { query } => UiEvent::FilesFound {
            ws_key,
            query: query.clone(),
            result: client.find_files(&query).await,
        },
        ApiRequest::ListMcpNames => UiEvent::McpNamesLoaded {
            ws_key,
            result: client.get_config().await.map(|v| mcp_names(&v)),
        },
        ApiRequest::McpConnect { name } => {
            let line = match client.mcp_connect(&name).await {
                Ok(()) => format!("mcp: connect {name}"),
                Err(err) => format!("mcp: connect {name} failed: {err:#}"),
            };
            UiEvent::Log { ws_key, line }
        }
        ApiRequest::McpDisconnect { name } => {
            let line = match client.mcp_disconnect(&name).await {
                Ok(()) => format!("mcp: disconnect {name}"),
                Err(err) => format!("mcp: disconnect {name} failed: {err:#}"),
            };
            UiEvent::Log { ws_key, line }
        }
    }
}

/// The `mcp` section's server names from a merged /config answer, in stable
/// name order. Non-object entries are not servers and are skipped.
fn mcp_names(config: &Value) -> Vec<String> {
    let mut names: Vec<String> = config
        .get("mcp")
        .and_then(Value::as_object)
        .map(|mcp| {
            mcp.iter()
                .filter(|(_, entry)| entry.is_object())
                .map(|(name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::decode_base64;
    use serde_json::json;

    #[test]
    fn base64_roundtrips_rfc4648_vectors() {
        for (raw, want) in [
            (&b""[..], ""),
            (b"f".as_slice(), "Zg=="),
            (b"fo".as_slice(), "Zm8="),
            (b"foo".as_slice(), "Zm9v"),
            (b"foob".as_slice(), "Zm9vYg=="),
            (b"fooba".as_slice(), "Zm9vYmE="),
            (b"foobar".as_slice(), "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(raw), want, "encoding {raw:?}");
            assert_eq!(decode_base64(want).as_deref(), Some(raw), "decoding {want}");
        }
        // the attachment decoder tolerates transport newlines
        assert_eq!(
            decode_base64("Zm9v\nYmFy").as_deref(),
            Some(b"foobar".as_slice())
        );
        assert_eq!(decode_base64("not*base64"), None);
    }

    #[test]
    fn mcp_names_parses_section_and_skips_non_objects() {
        let config = json!({
            "mcp": {
                "wiki": { "type": "remote", "url": "http://x" },
                "broken": "not a server",
                "tools": { "type": "local", "command": ["npx"] }
            },
            "agent": {}
        });
        assert_eq!(
            mcp_names(&config),
            vec!["tools".to_string(), "wiki".to_string()]
        );
    }

    #[test]
    fn mcp_names_handles_missing_and_empty_sections() {
        assert!(mcp_names(&json!({ "agent": {} })).is_empty());
        assert!(mcp_names(&json!({ "mcp": {} })).is_empty());
        assert!(mcp_names(&json!({ "mcp": "garbage" })).is_empty());
    }
}
