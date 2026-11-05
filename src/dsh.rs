//! DeepSeek Harness (`dsh web`) client + the opencode-event translation layer.
//!
//! dsh is not opencode-compatible: unary calls go to `POST /api/<method>` with
//! a `{type:"client-request", rpcId, method, payload:{args}}` envelope, and
//! streams ride one WebSocket at `/api/remote.mux` carrying logical-stream
//! frames (open/item/end/cancel). Auth is a per-process launch token printed
//! on stdout, exchanged at `GET /connect?token=…&interactions=answer` for a
//! `dsh-auth-*` cookie that must ride every /api call and the WS handshake;
//! the cookie dies with its WebSocket, so every reconnect re-exchanges it.
//!
//! The app's chat pipeline is left untouched: this module folds dsh's durable
//! session events into opencode-shaped messages/parts (full-text-so-far, same
//! field names the store already parses defensively) and emits them as
//! `UiEvent::Event` values, exactly like the opencode SSE pump does.

use anyhow::{Context as _, Result, anyhow};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::oc::{
    AgentInfo, ApiRequest, CommandInfo, ModelInfo, QuestionOptionInfo, QuestionPromptInfo,
    SessionInfo, StoredMessage, StreamStatus, UiEvent,
};
use crate::settings::HarnessKind;

/// How long a single follow snapshot may take to arrive before GetMessages
/// gives up (the pump may still be reconnecting).
const SNAPSHOT_WAIT: Duration = Duration::from_secs(10);
/// HTTP timeout for the `/connect` token exchange (dsh has no health
/// endpoint; the WS pump's own connection state is the liveness signal).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

// ---- spawn ---------------------------------------------------------------

/// Waits for the `dsh web: http://…/?token=…` line and exchanges the token
/// for an API cookie. Returns the cookie (bare `name=value` pair).
pub async fn connect_with_token(base_url: &str, token: &str) -> Result<String> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(CONNECT_TIMEOUT)
        .build()?;
    // never follow the 302 — the cookie rides this very response
    let resp = http
        .get(format!("{base_url}/connect"))
        .query(&[("token", token), ("interactions", "answer")])
        .send()
        .await
        .context("dsh /connect failed")?;
    for (name, value) in resp.headers().iter() {
        if name.as_str() == "set-cookie"
            && let Ok(full) = value.to_str()
            && let Some(pair) = full.split(';').next()
            && pair.starts_with("dsh-auth-")
        {
            return Ok(pair.to_string());
        }
    }
    Err(anyhow!(
        "dsh /connect returned no auth cookie (status {})",
        resp.status()
    ))
}

/// Extracts the launch token from a `dsh web` stdout line, if it is the URL
/// line. Plain char scan — the token alphabet is urlsafe base64.
pub fn token_from_line(line: &str) -> Option<&str> {
    let idx = line.find("token=")?;
    let rest = &line[idx + "token=".len()..];
    let end = rest
        .char_indices()
        .find(|(_, c)| !c.is_ascii_alphanumeric() && *c != '_' && *c != '-')
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    let token = &rest[..end];
    if token.is_empty() { None } else { Some(token) }
}

// ---- shared state ---------------------------------------------------------

#[derive(Default)]
pub struct DshSessionState {
    pub snapshot_seen: bool,
    pub title: Option<String>,
    /// Ordered opencode-shaped messages; the fold appends and updates in place.
    pub messages: Vec<MessageAcc>,
    /// dsh event seqs each message was built from (surfaceOp replace pruning).
    pub seqs_by_message: HashMap<String, Vec<i64>>,
}

#[derive(Default)]
pub struct MessageAcc {
    pub id: String,
    pub role: String,
    pub created_ms: i64,
    pub info: Value,
    pub parts: Vec<Value>,
    /// part id → index into parts
    pub part_at: HashMap<String, usize>,
}

impl MessageAcc {
    fn opencode_message(&self) -> StoredMessage {
        StoredMessage {
            id: self.id.clone(),
            role: self.role.clone(),
            created_ms: self.created_ms,
            info: self.info.clone(),
            parts: self.parts.clone(),
        }
    }
}

/// One dsh event folded into [`DshSessionState`], plus the opencode events to
/// emit for it on the live path.
pub struct FoldOutcome {
    pub message_updated: Option<Value>,
    pub part_updated: Vec<Value>,
    pub extra: Vec<Value>,
}

impl FoldOutcome {
    fn new() -> Self {
        Self {
            message_updated: None,
            part_updated: Vec::new(),
            extra: Vec::new(),
        }
    }
}

// ---- the client handle ----------------------------------------------------

/// Cloneable handle shared by the request tasks and the WS pump. The pump owns
/// the socket; requests are plain HTTP and only read the cookie.
#[derive(Clone)]
pub struct DshHandle {
    inner: Arc<Inner>,
}

struct Inner {
    base_url: String,
    token: String,
    http: reqwest::Client,
    cookie: RwLock<String>,
    /// outbound WS frames (open/cancel) → the pump's writer half
    ws_tx: async_channel::Sender<String>,
    pub sessions: Mutex<HashMap<String, DshSessionState>>,
    /// `$events` ready clientId — answers to waterfalls are keyed by it
    client_id: Mutex<Option<String>>,
    /// open question waterfalls by eventId, for reply mapping
    pub pending_questions: Mutex<HashMap<String, Vec<QuestionPromptInfo>>>,
    /// last session.error per session: one backend failure arrives twice (a
    /// transient `$events` emit plus the durable `turn/end`) and the
    /// second copy must not render a second error card
    recent_errors: Mutex<HashMap<String, (String, std::time::Instant)>>,
    /// ids the registry reports as archived (the cumulative reply of every
    /// `workspace/archiveSession`). Archived sessions keep coming back in
    /// `session/list` — the projection only drops them on compaction — and
    /// the list item carries no archive marker, so they are filtered here
    archived: Mutex<HashSet<String>>,
    rpc_counter: std::sync::atomic::AtomicU64,
}

impl DshHandle {
    pub fn new(base_url: &str, token: &str, ws_tx: async_channel::Sender<String>) -> Self {
        Self {
            inner: Arc::new(Inner {
                base_url: base_url.trim_end_matches('/').to_string(),
                token: token.to_string(),
                http: reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    // a unary RPC must not hang a request task forever; the
                    // SMS/event stream is a WebSocket and is not affected
                    .connect_timeout(Duration::from_secs(8))
                    .timeout(Duration::from_secs(120))
                    .build()
                    .unwrap_or_default(),
                cookie: RwLock::new(String::new()),
                ws_tx,
                sessions: Mutex::new(HashMap::new()),
                client_id: Mutex::new(None),
                pending_questions: Mutex::new(HashMap::new()),
                recent_errors: Mutex::new(HashMap::new()),
                archived: Mutex::new(HashSet::new()),
                rpc_counter: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    /// True when this exact error message was already folded for the session
    /// seconds ago (the backend mirrors one failure through both the transient
    /// `$events` emit and the durable `turn/end`); records it otherwise.
    fn error_seen(&self, session_id: &str, message: &str) -> bool {
        let mut recent = self.inner.recent_errors.lock().expect("dsh errors");
        let fresh = !matches!(
            recent.get(session_id),
            Some((last, at)) if *last == message && at.elapsed() < std::time::Duration::from_secs(10)
        );
        if fresh {
            recent.insert(
                session_id.to_string(),
                (message.to_string(), std::time::Instant::now()),
            );
        }
        fresh
    }

    pub fn set_cookie(&self, cookie: String) {
        *self.inner.cookie.write().expect("dsh cookie") = cookie;
    }

    /// Remembers the registry's cumulative archived-session ids (the reply of
    /// `workspace/archiveSession`; the delete call site falls back to the
    /// deleted id itself when the reply carries no list).
    fn note_archived(&self, ids: &[String]) {
        let mut archived = self.inner.archived.lock().expect("dsh archived");
        archived.extend(ids.iter().cloned());
    }

    fn is_archived(&self, session_id: &str) -> bool {
        self.inner
            .archived
            .lock()
            .expect("dsh archived")
            .contains(session_id)
    }

    pub fn cookie(&self) -> String {
        self.inner.cookie.read().expect("dsh cookie").clone()
    }

    pub fn set_client_id(&self, id: &str) {
        *self.inner.client_id.lock().expect("dsh client id") = Some(id.to_string());
    }

    /// Plain liveness probe for the health poll: any HTTP answer (the fence's
    /// 401 included) proves the server process is serving.
    pub async fn alive(&self) -> bool {
        self.inner
            .http
            .get(format!("{}/", self.inner.base_url))
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .is_ok()
    }

    /// Unary RPC. A 401 means the cookie died with a dropped WS: re-exchange
    /// the token and retry once.
    pub async fn rpc(&self, method: &str, args: Value) -> Result<Value> {
        let id = format!(
            "warp-{}",
            self.inner
                .rpc_counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let mut refreshed = false;
        for _ in 0..2 {
            let body = json!({
                "type": "client-request",
                "rpcId": id,
                "method": method,
                "payload": { "args": args },
            });
            let resp = self
                .inner
                .http
                .post(format!("{}/api/{method}", self.inner.base_url))
                .header("content-type", "application/json")
                .header("cookie", self.cookie())
                .json(&body)
                .send()
                .await
                .with_context(|| format!("dsh rpc {method} failed"))?;
            if resp.status() == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                refreshed = true;
                let cookie = connect_with_token(&self.inner.base_url, &self.inner.token).await?;
                self.set_cookie(cookie);
                continue;
            }
            let value: Value = resp.json().await.context("dsh rpc response was not JSON")?;
            let result = value.pointer("/result").cloned().unwrap_or(Value::Null);
            if result.get("ok").and_then(Value::as_bool) == Some(true) {
                return Ok(result.get("value").cloned().unwrap_or(Value::Null));
            }
            let error = result.get("error").cloned().unwrap_or(Value::Null);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown dsh error");
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("gateway/error");
            return Err(anyhow!("dsh {method}: {message} ({code})"));
        }
        Err(anyhow!(
            "dsh rpc {method}: unauthorized after cookie refresh"
        ))
    }

    /// Opens (once) the follow stream for a session and waits for its snapshot
    /// so history can be read from the folded state.
    pub async fn messages(&self, session_id: &str) -> Result<Vec<StoredMessage>> {
        self.ensure_follow(session_id).await?;
        let deadline = std::time::Instant::now() + SNAPSHOT_WAIT;
        loop {
            {
                let sessions = self.inner.sessions.lock().expect("dsh sessions");
                if let Some(state) = sessions.get(session_id)
                    && state.snapshot_seen
                {
                    return Ok(state
                        .messages
                        .iter()
                        .map(MessageAcc::opencode_message)
                        .collect());
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err(anyhow!("dsh history for this chat did not arrive in time"));
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
    }

    /// Opens the follow stream for a session if it is not open yet. Frames
    /// arrive on the pump; the state entry existing is what "open" means here.
    pub async fn ensure_follow(&self, session_id: &str) -> Result<()> {
        {
            let mut sessions = self.inner.sessions.lock().expect("dsh sessions");
            let exists = sessions.contains_key(session_id);
            if !exists {
                sessions.insert(session_id.to_string(), DshSessionState::default());
            }
            if exists {
                return Ok(());
            }
        }
        let frame = json!({
            "type": "open",
            "streamId": follow_stream_id(session_id),
            "endpoint": "session/follow",
            "payload": { "args": { "request": {
                "address": { "kind": "session", "sessionId": session_id },
                "maxMessages": 120,
            } } },
        });
        self.send_ws_frame(&frame).await
    }

    async fn send_ws_frame(&self, frame: &Value) -> Result<()> {
        // bounded channel to the pump; `.send().await` (not send_blocking)
        // keeps a full channel from stalling a tokio worker (audit B5)
        self.inner
            .ws_tx
            .send(frame.to_string())
            .await
            .map_err(|_| anyhow!("dsh stream pump is not running"))
    }

    /// Any session id we already follow — skills/file search need an agent.
    pub fn any_session(&self) -> Option<String> {
        self.inner
            .sessions
            .lock()
            .expect("dsh sessions")
            .keys()
            .next()
            .cloned()
    }

    /// Answer an approval waterfall: response is the opencode vocabulary
    /// ("once"/"always"/"reject"); dsh only has one-shot grants.
    pub async fn answer_approval(&self, event_id: &str, response: &str) -> Result<()> {
        let value = if response == "reject" {
            "rejected"
        } else {
            "allowed-once"
        };
        self.event_result(event_id, json!({ "kind": "result", "value": value }))
            .await
    }

    /// Answer a question waterfall from the opencode `Vec<Vec<String>>` shape
    /// (one selection list per question, in order).
    pub async fn answer_question(&self, event_id: &str, answers: &[Vec<String>]) -> Result<()> {
        let questions = self
            .inner
            .pending_questions
            .lock()
            .expect("dsh questions")
            .get(event_id)
            .cloned()
            .unwrap_or_default();
        let mut items = Vec::new();
        for (index, question) in questions.iter().enumerate() {
            let given = answers.get(index).cloned().unwrap_or_default();
            let mut selected = Vec::new();
            let mut custom = String::new();
            for answer in &given {
                if question.options.iter().any(|o| o.label == *answer) {
                    selected.push(answer.clone());
                } else if !answer.trim().is_empty() {
                    custom = answer.clone();
                }
            }
            items.push(
                json!({ "id": question_key(question), "selected": selected, "custom": custom }),
            );
        }
        self.inner
            .pending_questions
            .lock()
            .expect("dsh questions")
            .remove(event_id);
        self.event_result(
            event_id,
            json!({ "kind": "result", "value": { "answers": items } }),
        )
        .await
    }

    pub async fn reject_question(&self, event_id: &str) -> Result<()> {
        self.inner
            .pending_questions
            .lock()
            .expect("dsh questions")
            .remove(event_id);
        self.event_result(
            event_id,
            json!({ "kind": "rejected", "error": { "name": "dismissed", "message": "dismissed" } }),
        )
        .await
    }

    async fn event_result(&self, event_id: &str, outcome: Value) -> Result<()> {
        let client_id = self
            .inner
            .client_id
            .lock()
            .expect("dsh client id")
            .clone()
            .ok_or_else(|| anyhow!("dsh event stream not ready"))?;
        self.rpc(
            "$events/result",
            json!({ "clientId": client_id, "eventId": event_id, "outcome": outcome }),
        )
        .await
        .map(|_| ())
    }

    /// Clears every folded snapshot for a new connection generation and returns
    /// the session ids whose follow streams the pump must reopen. The pump
    /// writes those frames on its own socket: routing them back through the
    /// bounded outbound channel would wait on the pump to drain a channel it
    /// is the only reader of, deadlocking once a reconnect needs more frames
    /// than the channel holds (audit B5).
    fn reconnect_sessions(&self) -> Vec<String> {
        let session_ids: Vec<String> = self
            .inner
            .sessions
            .lock()
            .expect("dsh sessions")
            .keys()
            .cloned()
            .collect();
        // a fresh generation forgets every snapshot; drop folded state so the
        // new snapshot re-seeds instead of duplicating
        {
            let mut sessions = self.inner.sessions.lock().expect("dsh sessions");
            for id in &session_ids {
                if let Some(state) = sessions.get_mut(id) {
                    *state = DshSessionState::default();
                }
            }
        }
        session_ids
    }
}

fn follow_stream_id(session_id: &str) -> String {
    format!("follow-{session_id}")
}

/// Stable key for a question (dsh questions carry their own id; fall back to
/// the question text so answers still correlate).
fn question_key(question: &QuestionPromptInfo) -> String {
    let id = question.question.trim();
    if id.is_empty() {
        "(empty)".to_string()
    } else {
        id.to_string()
    }
}

// ---- request mapping ------------------------------------------------------

/// Runs one ApiRequest against the dsh web API and shapes the reply the store
/// already knows. `ws_key` is DSH_KEY; `folder` is the real workspace folder
/// for the requests whose reply events key off the workspace.
pub async fn run_request(handle: &DshHandle, ws_key: String, request: ApiRequest) -> UiEvent {
    match request {
        ApiRequest::ListSessions | ApiRequest::ListMcpNames => {
            // dsh sessions are listed per workspace folder (ListDshSessions)
            // and MCP has no dsh equivalent; neither can be answered here —
            // an empty list would read as "everything gone" and prune real
            // chats, so log the misrouting instead
            UiEvent::Log {
                ws_key,
                line: "request without a dsh equivalent hit the dsh runner".into(),
            }
        }
        ApiRequest::ListDshSessions { folder } => {
            let result = list_sessions_filtered(handle, &folder).await;
            UiEvent::SessionsLoaded {
                ws_key: folder,
                dsh: true,
                result,
            }
        }
        ApiRequest::ListModels => UiEvent::ModelsLoaded {
            harness: HarnessKind::Dsh,
            result: model_catalog(handle).await,
        },
        ApiRequest::ListAgents => UiEvent::AgentsLoaded {
            result: agent_presets(handle).await,
        },
        ApiRequest::ListCommands => {
            let result = skills(handle).await;
            UiEvent::CommandsLoaded { ws_key, result }
        }
        ApiRequest::GetMessages { session_id } => {
            handle.ensure_follow(&session_id).await.ok();
            let result = handle.messages(&session_id).await;
            UiEvent::MessagesLoaded {
                ws_key,
                session_id,
                result,
            }
        }
        ApiRequest::CreateSession { title } | ApiRequest::CreateForkSession { title, .. } => {
            // no folder known: create in the cwd the server was spawned in
            let result = create_session(handle, None, &title).await;
            UiEvent::SessionCreated { ws_key, result }
        }
        ApiRequest::CreateDshSession { folder, title } => {
            let result = create_session(handle, Some(&folder), &title).await;
            UiEvent::SessionCreated {
                ws_key: folder,
                result,
            }
        }
        ApiRequest::CreateDshForkSession {
            folder,
            title,
            context,
        } => {
            let result = create_session(handle, Some(&folder), &title).await;
            UiEvent::ForkSessionCreated {
                ws_key: folder,
                context,
                result,
            }
        }
        ApiRequest::RenameSession { session_id, title } => {
            let result = handle
                .rpc(
                    "session/rename",
                    json!({ "request": { "sessionId": session_id, "title": title } }),
                )
                .await
                .map(|v| SessionInfo {
                    id: session_id.clone(),
                    title: v
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or(&title)
                        .to_string(),
                    updated_ms: now_ms(),
                    created_ms: 0,
                    parent_id: None,
                });
            UiEvent::SessionRenamed { session_id, result }
        }
        ApiRequest::DeleteSession { session_id } => {
            // dsh has no hard delete; archiving removes it from the list. The
            // reply carries the registry's cumulative archivedSessionIds —
            // remembered so the sticky session/list projection (archived
            // sessions stay listed until compaction, with no marker on the
            // item) cannot pull the deleted chat back into the sidebar
            let result = handle
                .rpc(
                    "workspace/archiveSession",
                    json!({ "request": { "sessionId": session_id } }),
                )
                .await
                .map(|reply| {
                    let ids: Vec<String> = reply
                        .get("archivedSessionIds")
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter()
                                .filter_map(|id| id.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_else(|| vec![session_id.clone()]);
                    handle.note_archived(&ids);
                });
            UiEvent::SessionDeleted { session_id, result }
        }
        ApiRequest::Abort { session_id } => {
            let result = handle
                .rpc(
                    "session/cancel",
                    json!({ "request": { "sessionId": session_id } }),
                )
                .await
                .map(|_| ());
            UiEvent::Aborted { session_id, result }
        }
        ApiRequest::RespondPermission {
            permission_id,
            response,
            ..
        }
        | ApiRequest::ReplyV2 {
            request_id: permission_id,
            response,
        } => {
            let result = handle.answer_approval(&permission_id, &response).await;
            UiEvent::PermissionReplied {
                permission_id,
                response,
                is_auto: false,
                result,
            }
        }
        ApiRequest::GetQuestions { for_session } => {
            // questions arrive as waterfalls only; nothing to pull
            UiEvent::QuestionsPulled {
                for_session,
                result: Ok(Vec::new()),
            }
        }
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
            let result = handle.answer_question(&request_id, &answers).await;
            UiEvent::QuestionReplied {
                request_id,
                summary,
                result,
            }
        }
        ApiRequest::RejectQuestion { request_id } => {
            let result = handle.reject_question(&request_id).await;
            UiEvent::QuestionReplied {
                request_id,
                summary: "dismissed".into(),
                result,
            }
        }
        ApiRequest::FindFiles { query } => {
            let result = find_files(handle, &query).await;
            UiEvent::FilesFound {
                ws_key,
                query,
                result,
            }
        }
        ApiRequest::McpConnect { name } | ApiRequest::McpDisconnect { name } => UiEvent::Log {
            ws_key,
            line: format!("mcp: {name}: not available on dsh"),
        },
    }
}

async fn create_session(
    handle: &DshHandle,
    folder: Option<&str>,
    _title: &str,
) -> Result<SessionInfo> {
    let mut args = json!({});
    if let Some(folder) = folder.filter(|f| !f.is_empty()) {
        args["request"]["cwd"] = json!(folder);
    } else {
        args["request"] = json!({});
    }
    let value = handle.rpc("session/create", args).await?;
    let id = value
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("dsh session/create returned no sessionId"))?
        .to_string();
    handle.ensure_follow(&id).await.ok();
    Ok(SessionInfo {
        id,
        title: "New chat".into(),
        updated_ms: now_ms(),
        created_ms: now_ms(),
        parent_id: None,
    })
}

async fn list_sessions_filtered(handle: &DshHandle, folder: &str) -> Result<Vec<SessionInfo>> {
    let value = handle
        .rpc("session/list", json!({ "_request": {} }))
        .await?;
    let mut sessions = Vec::new();
    for item in value
        .pointer("/items")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        // dsh serves every folder from one process; a workspace only sees its own
        if item.get("cwd").and_then(Value::as_str) != Some(folder) {
            continue;
        }
        // archived chats keep riding the list (see Inner.archived)
        let id = item.get("sessionId").and_then(Value::as_str).unwrap_or("");
        if handle.is_archived(id) {
            continue;
        }
        sessions.push(SessionInfo {
            id: id.to_string(),
            title: item
                .pointer("/projections/values/title/title")
                .and_then(Value::as_str)
                .filter(|t| !t.is_empty())
                .unwrap_or("New chat")
                .to_string(),
            updated_ms: item.get("updatedAt").and_then(Value::as_i64).unwrap_or(0),
            created_ms: 0,
            parent_id: item
                .get("parentSessionId")
                .and_then(Value::as_str)
                .map(String::from),
        });
    }
    Ok(sessions)
}

async fn model_catalog(handle: &DshHandle) -> Result<Vec<ModelInfo>> {
    let value = handle.rpc("session/modelCatalog", json!({})).await?;
    let mut models = Vec::new();
    for group in value
        .get("groups")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        let provider = group.get("id").and_then(Value::as_str).unwrap_or("");
        if provider.is_empty() {
            continue;
        }
        for model in group
            .get("models")
            .and_then(Value::as_array)
            .unwrap_or(&Vec::new())
        {
            let name = model.get("id").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let mut variants: Vec<String> = model
                .pointer("/reasoning/efforts")
                .and_then(Value::as_array)
                .map(|efforts| {
                    efforts
                        .iter()
                        .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            variants.sort_by_key(|v| (crate::store::variant_rank(v), v.to_lowercase()));
            models.push(ModelInfo {
                key: format!("{provider}/{name}"),
                harness: HarnessKind::Dsh,
                variants,
                context_limit: 0,
            });
        }
    }
    Ok(models)
}

async fn agent_presets(handle: &DshHandle) -> Result<Vec<AgentInfo>> {
    let value = handle.rpc("agentPresets/list", json!({})).await?;
    let mut agents = Vec::new();
    for preset in value
        .pointer("/presets")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        let Some(name) = preset.get("id").and_then(Value::as_str) else {
            continue;
        };
        let is_default = preset
            .get("isDefault")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if preset.get("broken").and_then(Value::as_str).is_some() {
            continue;
        }
        agents.push(AgentInfo {
            name: name.to_string(),
            mode: if is_default {
                "primary".into()
            } else {
                "all".into()
            },
            hidden: false,
        });
    }
    Ok(agents)
}

async fn skills(handle: &DshHandle) -> Result<Vec<CommandInfo>> {
    // skills are session-scoped (cwd + preset); use any followed session
    let Some(session) = handle.any_session() else {
        return Ok(Vec::new());
    };
    let value = handle
        .rpc(
            "skills/list",
            json!({ "request": { "sessionId": session } }),
        )
        .await?;
    let mut commands = Vec::new();
    for skill in value
        .get("skills")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        let Some(name) = skill.get("name").and_then(Value::as_str) else {
            continue;
        };
        commands.push(CommandInfo {
            name: name.to_string(),
            name_lower: name.to_lowercase(),
            description: skill
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            source: "skill".into(),
        });
    }
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(commands)
}

async fn find_files(handle: &DshHandle, query: &str) -> Result<Vec<String>> {
    let Some(agent) = handle.any_session() else {
        return Ok(Vec::new());
    };
    let value = handle
        .rpc(
            "fileReferences/list",
            json!({ "agentId": agent, "query": query }),
        )
        .await?;
    Ok(value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.get("path").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default())
}

/// Sends a prompt (plain text, slash text, images — dsh expands commands
/// itself) with the tile's model/effort selection applied first.
pub async fn send_message(
    handle: &DshHandle,
    session_id: &str,
    text: &str,
    model: Option<&str>,
    variant: Option<&str>,
    _agent: Option<&str>,
    attachments: &[(String, String, std::sync::Arc<Vec<u8>>)], // (filename, mime, bytes)
) -> Result<()> {
    if let Some(model) = model {
        let (provider, name) = match model.split_once('/') {
            Some((p, m)) => (p, m),
            None => (model, ""),
        };
        if !name.is_empty() {
            let mut selection =
                json!({ "sessionId": session_id, "provider": provider, "model": name });
            if let Some(effort) = variant {
                selection["reasoningEffort"] = json!(effort);
            }
            // selection is durable "next request"; failures fall back to the
            // session's current model rather than blocking the send
            if let Err(err) = handle
                .rpc("session/selectModel", json!({ "request": selection }))
                .await
            {
                tracing::warn!("[dsh] selectModel ignored: {err:#}");
            }
        }
    }
    handle.ensure_follow(session_id).await.ok();
    let mut content = Vec::new();
    // images first, matching the reference clients' ordering
    for (file_name, mime, data) in attachments {
        if mime.starts_with("image/") {
            content.push(json!({
                "type": "image",
                "mediaType": mime,
                "data": crate::oc::base64_encode(data),
                "name": file_name,
            }));
        } else {
            content
                .push(json!({ "type": "text", "text": format!("[attached file: {file_name}]") }));
        }
    }
    if !text.is_empty() {
        content.push(json!({ "type": "text", "text": text }));
    }
    if content.is_empty() {
        return Ok(());
    }
    // a counter, not a timestamp: two sends within one millisecond (queue
    // drain racing a user send) must not share a requestId
    let request_id = format!(
        "warp-send-{}",
        handle
            .inner
            .rpc_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    handle
        .rpc(
            "session/prompt",
            json!({ "request": {
                "requestId": request_id,
                "sessionId": session_id,
                "mode": "queue",
                "content": content,
            } }),
        )
        .await
        .map(|_| ())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- event translation (the fold) -----------------------------------------

/// Folds one dsh wire event (from a follow stream: `event` frames carry
/// `{type, seq, time, data}`, snapshots carry records of the same shape) into
/// the session state, returning the opencode events to emit live.
pub fn fold_event(state: &mut DshSessionState, session_id: &str, event: &Value) -> FoldOutcome {
    let mut outcome = FoldOutcome::new();
    let seq = event.get("seq").and_then(Value::as_i64).unwrap_or(0);
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or("");
    let time = event.get("time").and_then(Value::as_i64).unwrap_or(0);
    let data = event.get("data").cloned().unwrap_or(Value::Null);

    // compaction replaces an earlier surface range with this event
    if let Some(op) = event.get("surfaceOp").and_then(Value::as_str)
        && op == "replace"
        && let (Some(start), Some(end)) = (
            event.pointer("/surfaceOp/startSeq").and_then(Value::as_i64),
            event.pointer("/surfaceOp/endSeq").and_then(Value::as_i64),
        )
    {
        prune_range(state, start, end);
    }

    match event_type {
        "user/message" => fold_user_message(state, session_id, seq, time, &data, &mut outcome),
        "assistant/message" => {
            fold_assistant_message(state, session_id, seq, time, &data, &mut outcome)
        }
        "tool/call" => fold_tool_call(state, session_id, seq, time, &data, &mut outcome),
        "tool/result" => fold_tool_result(state, session_id, seq, time, &data, &mut outcome),
        "turn/start" => {
            outcome.extra.push(status_event(session_id, "busy"));
        }
        "turn/end" => {
            outcome.extra.push(json!({
                "type": "session.idle",
                "properties": { "sessionID": session_id },
            }));
            if data.pointer("/reason/kind").and_then(Value::as_str) == Some("error") {
                let error = data
                    .pointer("/reason/error")
                    .cloned()
                    .unwrap_or(Value::Null);
                outcome.extra.push(json!({
                    "type": "session.error",
                    "properties": { "sessionID": session_id, "error": {
                        "name": error.get("code").and_then(Value::as_str).unwrap_or("error"),
                        "message": error.get("message").and_then(Value::as_str).unwrap_or("the turn failed"),
                    } },
                }));
            }
        }
        "todo/write" => {
            outcome.extra.push(json!({
                "type": "todo.updated",
                "properties": {
                    "sessionID": session_id,
                    "todos": data.get("todos").cloned().unwrap_or(Value::Null),
                },
            }));
        }
        "session/title" => {
            state.title = data.get("title").and_then(Value::as_str).map(String::from);
            outcome.extra.push(json!({
                "type": "session.updated",
                "properties": { "info": { "id": session_id } },
            }));
        }
        _ => {}
    }
    if seq > 0 {
        {
            let message = &outcome.message_updated;
            if let Some(updated) = message
                && let Some(id) = updated.pointer("/info/id").and_then(Value::as_str)
            {
                state
                    .seqs_by_message
                    .entry(id.to_string())
                    .or_default()
                    .push(seq);
            }
        }
    }
    outcome
}

fn prune_range(state: &mut DshSessionState, start: i64, end: i64) {
    let dead: Vec<String> = state
        .seqs_by_message
        .iter()
        .filter(|(_, seqs)| seqs.iter().any(|s| *s >= start && *s <= end))
        .map(|(id, _)| id.clone())
        .collect();
    for id in dead {
        state.messages.retain(|m| m.id != id);
        state.seqs_by_message.remove(&id);
    }
}

/// Only genuine human prompts render; injected contexts (runtime snapshots,
/// skill catalogs, time notices, model-change notes) ride user/message events
/// with other source kinds and would bury the transcript.
fn is_human_prompt(data: &Value) -> bool {
    matches!(
        data.pointer("/source/kind").and_then(Value::as_str),
        Some("user")
    )
}

fn fold_user_message(
    state: &mut DshSessionState,
    session_id: &str,
    seq: i64,
    time: i64,
    data: &Value,
    outcome: &mut FoldOutcome,
) {
    if !is_human_prompt(data) {
        return;
    }
    let id = data
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if id.is_empty() {
        return;
    }
    let message = upsert_message(
        state,
        &id,
        "user",
        time,
        json!({ "id": id, "role": "user", "time": { "created": time } }),
    );
    let empty_blocks = Vec::new();
    let blocks = data
        .get("content")
        .and_then(Value::as_array)
        .unwrap_or(&empty_blocks);
    for (index, block) in blocks.iter().enumerate() {
        if block.get("type").and_then(Value::as_str) != Some("text") {
            continue; // image/file attachment refs need a fetch; render nothing
        }
        let part = json!({
            "id": format!("{id}-{index}"),
            "messageID": id,
            "type": "text",
            "text": block.get("text").and_then(Value::as_str).unwrap_or(""),
        });
        if let Some(part) = upsert_part(message, part) {
            outcome.part_updated.push(part);
        }
    }
    outcome.message_updated = Some(json!({
        "type": "message.updated",
        "properties": { "info": message.info.clone(), "sessionID": session_id },
    }));
    let _ = (seq, session_id);
}

fn fold_assistant_message(
    state: &mut DshSessionState,
    session_id: &str,
    seq: i64,
    time: i64,
    data: &Value,
    outcome: &mut FoldOutcome,
) {
    // the assistant message of one step; tool calls of the same step already
    // live under this id (see fold_tool_call)
    let turn = data.get("turn").and_then(Value::as_i64).unwrap_or(0);
    let step = data.get("step").and_then(Value::as_i64).unwrap_or(0);
    let id = format!("dsh-{turn}-{step}");
    let message_value = data.get("message").cloned().unwrap_or(Value::Null);
    let source = message_value
        .pointer("/source")
        .cloned()
        .unwrap_or(Value::Null);
    let usage = data.get("usage").cloned().unwrap_or(Value::Null);
    let tokens = if usage.is_null() {
        json!({})
    } else {
        json!({
            "input": usage.get("inputTokens").and_then(Value::as_i64).unwrap_or(0),
            "output": usage.get("outputTokens").and_then(Value::as_i64).unwrap_or(0),
            "reasoning": usage.get("reasoningTokens").and_then(Value::as_i64).unwrap_or(0),
            "cache": {
                "read": usage.get("cacheReadTokens").and_then(Value::as_i64).unwrap_or(0),
                "write": usage.get("cacheWriteTokens").and_then(Value::as_i64).unwrap_or(0),
            },
        })
    };
    let info = json!({
        "id": id,
        "role": "assistant",
        "time": { "created": time },
        "providerID": source.get("provider").and_then(Value::as_str).unwrap_or(""),
        "modelID": source.get("model").and_then(Value::as_str).unwrap_or(""),
        "tokens": tokens,
        "cost": 0,
    });
    let message = upsert_message(state, &id, "assistant", time, info);
    let empty_blocks = Vec::new();
    let blocks = message_value
        .get("content")
        .and_then(Value::as_array)
        .unwrap_or(&empty_blocks);
    let mut text_index = 0usize;
    for block in blocks {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                let part = json!({
                    "id": format!("{id}-t{text_index}"),
                    "messageID": id,
                    "type": "text",
                    "text": block.get("text").and_then(Value::as_str).unwrap_or(""),
                });
                if let Some(part) = upsert_part(message, part) {
                    outcome.part_updated.push(part);
                }
                text_index += 1;
            }
            "reasoning" => {
                let part = json!({
                    "id": format!("{id}-r{text_index}"),
                    "messageID": id,
                    "type": "reasoning",
                    "text": block.get("text").and_then(Value::as_str).unwrap_or(""),
                });
                if let Some(part) = upsert_part(message, part) {
                    outcome.part_updated.push(part);
                }
                text_index += 1;
            }
            // tool-call blocks duplicate the tool/call events already folded
            _ => {}
        }
    }
    outcome.message_updated = Some(json!({
        "type": "message.updated",
        "properties": { "info": message.info.clone(), "sessionID": session_id },
    }));
    let _ = seq;
}

fn step_message_id(data: &Value) -> String {
    let turn = data.get("turn").and_then(Value::as_i64).unwrap_or(0);
    let step = data.get("step").and_then(Value::as_i64).unwrap_or(0);
    format!("dsh-{turn}-{step}")
}

fn fold_tool_call(
    state: &mut DshSessionState,
    session_id: &str,
    seq: i64,
    time: i64,
    data: &Value,
    outcome: &mut FoldOutcome,
) {
    let message_id = step_message_id(data);
    let call_id = data.get("callId").and_then(Value::as_str).unwrap_or("");
    let name = data.get("name").and_then(Value::as_str).unwrap_or("tool");
    if call_id.is_empty() {
        return;
    }
    let message = upsert_message(
        state,
        &message_id,
        "assistant",
        time,
        json!({ "id": message_id, "role": "assistant", "time": { "created": time } }),
    );
    let arguments = data.get("arguments").and_then(Value::as_str).unwrap_or("");
    let input = serde_json::from_str::<Value>(arguments).unwrap_or(Value::String(arguments.into()));
    let part = json!({
        "id": format!("tool-{call_id}"),
        "messageID": message_id,
        "type": "tool",
        "tool": name,
        "state": { "status": "running", "title": name, "input": input },
    });
    if let Some(part) = upsert_part(message, part) {
        outcome.part_updated.push(part);
    }
    let _ = (seq, session_id);
}

fn fold_tool_result(
    state: &mut DshSessionState,
    session_id: &str,
    seq: i64,
    time: i64,
    data: &Value,
    outcome: &mut FoldOutcome,
) {
    let message_value = data.get("message").cloned().unwrap_or(Value::Null);
    let call_id = message_value
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or("");
    if call_id.is_empty() {
        return;
    }
    let part_id = format!("tool-{call_id}");
    let is_error = message_value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let empty_blocks = Vec::new();
    let blocks = message_value
        .get("content")
        .and_then(Value::as_array)
        .unwrap_or(&empty_blocks);
    let output: String = blocks
        .iter()
        .filter_map(|b| {
            (b.get("type").and_then(Value::as_str) == Some("text"))
                .then(|| b.get("text").and_then(Value::as_str))
                .flatten()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let status = if is_error { "error" } else { "completed" };
    // find the owning message through the part table
    let owner = state
        .messages
        .iter()
        .position(|m| m.part_at.contains_key(&part_id))
        .or_else(|| {
            // a result with no folded call (e.g. snapshot window cut it): land
            // on the step's own message
            let id = step_message_id(data);
            state.messages.iter().position(|m| m.id == id)
        });
    let Some(index) = owner else { return };
    let message = &mut state.messages[index];
    let existing = message.part_at.get(&part_id).copied();
    let state_json = json!({
        "status": status,
        "output": output,
        "error": data.pointer("/error/reason").and_then(Value::as_str).unwrap_or(""),
        "metadata": data.get("meta").cloned().unwrap_or(Value::Null),
    });
    let part = match existing {
        Some(part_index) => {
            let part = &mut message.parts[part_index];
            if let Some(object) = part.get_mut("state").and_then(Value::as_object_mut) {
                for (key, value) in state_json.as_object().unwrap() {
                    object.insert(key.clone(), value.clone());
                }
            }
            part.clone()
        }
        None => {
            let part = json!({
                "id": part_id,
                "messageID": message.id,
                "type": "tool",
                "tool": "tool",
                "state": state_json,
            });
            message.parts.push(part.clone());
            message
                .part_at
                .insert(part_id.clone(), message.parts.len() - 1);
            part
        }
    };
    outcome.part_updated.push(part);
    let _ = (seq, session_id, time);
}

fn upsert_message<'a>(
    state: &'a mut DshSessionState,
    id: &str,
    role: &str,
    created_ms: i64,
    info: Value,
) -> &'a mut MessageAcc {
    let index = match state.messages.iter().position(|m| m.id == id) {
        Some(index) => index,
        None => {
            state.messages.push(MessageAcc {
                id: id.to_string(),
                role: role.to_string(),
                created_ms,
                info: Value::Null,
                parts: Vec::new(),
                part_at: HashMap::new(),
            });
            state.messages.len() - 1
        }
    };
    let message = &mut state.messages[index];
    if !info.is_null() {
        message.info = info;
    }
    message.created_ms = created_ms;
    message
}

/// Inserts or replaces a part by id; returns the part to emit, or None when
/// the stored copy is byte-identical.
fn upsert_part(message: &mut MessageAcc, part: Value) -> Option<Value> {
    let id = part.get("id").and_then(Value::as_str)?.to_string();
    match message.part_at.get(&id).copied() {
        Some(index) => {
            if message.parts[index] == part {
                return None;
            }
            message.parts[index] = part.clone();
            Some(part)
        }
        None => {
            message.parts.push(part.clone());
            message.part_at.insert(id, message.parts.len() - 1);
            Some(part)
        }
    }
}

fn status_event(session_id: &str, status: &str) -> Value {
    json!({
        "type": "session.status",
        "properties": { "sessionID": session_id, "status": status },
    })
}

// ---- $events frames --------------------------------------------------------

/// Handles one `$events` downlink frame (ready/emit/waterfall/cancel) and
/// returns opencode-shaped events for the store.
pub fn fold_remote_event(frame: &Value) -> Vec<Value> {
    let kind = frame.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "ready" => Vec::new(), // clientId is consumed by the pump directly
        "emit" => fold_emit(frame),
        "waterfall" => fold_waterfall(frame),
        "cancel" => {
            let event_id = frame.get("eventId").and_then(Value::as_str).unwrap_or("");
            vec![
                json!({ "type": "permission.v2.replied", "properties": { "id": event_id } }),
                json!({ "type": "question.rejected", "properties": { "id": event_id } }),
            ]
        }
        _ => Vec::new(),
    }
}

fn fold_emit(frame: &Value) -> Vec<Value> {
    let event = frame.get("event").and_then(Value::as_str).unwrap_or("");
    let args = frame
        .get("args")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    match event {
        // args: [sessionId, running] — idle must arrive as session.idle so
        // queues drain and turn notes land
        "api-session/status" => {
            let Some(session) = args.first().and_then(Value::as_str) else {
                return Vec::new();
            };
            let running = args.get(1).and_then(Value::as_bool).unwrap_or(false);
            if running {
                vec![status_event(session, "busy")]
            } else {
                vec![json!({
                    "type": "session.idle",
                    "properties": { "sessionID": session },
                })]
            }
        }
        // args: [sessionId, message]
        "api-session/error" => {
            let Some(session) = args.first().and_then(Value::as_str) else {
                return Vec::new();
            };
            let message = args
                .get(1)
                .and_then(Value::as_str)
                .unwrap_or("the agent failed");
            vec![json!({
                "type": "session.error",
                "properties": { "sessionID": session, "error": { "name": "error", "message": message } },
            })]
        }
        _ => Vec::new(),
    }
}

fn fold_waterfall(frame: &Value) -> Vec<Value> {
    let event = frame.get("event").and_then(Value::as_str).unwrap_or("");
    let event_id = frame.get("eventId").and_then(Value::as_str).unwrap_or("");
    let agent = frame
        .get("agentId")
        .and_then(Value::as_str)
        .or_else(|| frame.pointer("/request/agent/id").and_then(Value::as_str))
        .unwrap_or("");
    if event_id.is_empty() || agent.is_empty() {
        return Vec::new();
    }
    match event {
        "approval/request" => {
            vec![json!({
                "type": "permission.v2.asked",
                "properties": {
                    "id": event_id,
                    "sessionID": agent,
                    "action": frame.pointer("/request/toolName").and_then(Value::as_str).unwrap_or("permission"),
                    "resources": [
                        frame.pointer("/request/displayReason/en").and_then(Value::as_str)
                            .or_else(|| frame.pointer("/request/reason").and_then(Value::as_str))
                            .unwrap_or(""),
                    ],
                },
            })]
        }
        "user-questions/request" => {
            let questions = frame
                .pointer("/request/questions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let prompts: Vec<QuestionPromptInfo> = questions
                .iter()
                .map(|q| QuestionPromptInfo {
                    question: q
                        .get("question")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    header: q.get("header").and_then(Value::as_str).unwrap_or("").into(),
                    multiple: q
                        .get("multiSelect")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    options: q
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|options| {
                            options
                                .iter()
                                .map(|o| QuestionOptionInfo {
                                    label: o
                                        .get("label")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .into(),
                                    description: o
                                        .get("description")
                                        .and_then(Value::as_str)
                                        .unwrap_or("")
                                        .into(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect();
            // remember the payload so the reply can map answers back to ids
            frame_insert_question(event_id, prompts);
            vec![json!({
                "type": "question.asked",
                "properties": {
                    "id": event_id,
                    "sessionID": agent,
                    "questions": frame
                        .pointer("/request/questions")
                        .cloned()
                        .unwrap_or(Value::Null),
                },
            })]
        }
        _ => Vec::new(),
    }
}

// The question payload stash lives on the handle; a process-global side table
// keeps fold_remote_event pure-ish (single dsh server per app).
static STASHED_QUESTIONS: Mutex<Option<HashMap<String, Vec<QuestionPromptInfo>>>> =
    Mutex::new(None);

fn frame_insert_question(event_id: &str, prompts: Vec<QuestionPromptInfo>) {
    let mut stash = STASHED_QUESTIONS.lock().expect("dsh question stash");
    stash
        .get_or_insert_with(HashMap::new)
        .insert(event_id.to_string(), prompts);
}

/// Pops stashed question payloads for reply mapping (called by the pump when
/// it mints the handle's pending_questions from a waterfall).
pub fn take_stashed_questions(event_id: &str) -> Option<Vec<QuestionPromptInfo>> {
    let mut stash = STASHED_QUESTIONS.lock().expect("dsh question stash");
    stash.as_mut().and_then(|m| m.remove(event_id))
}

// ---- the WS pump -----------------------------------------------------------

/// Runs the mux connection for the lifetime of the dsh server: connect,
/// multiplex $events + one follow per followed session, translate frames into
/// UiEvents. Mirrors the opencode SSE pump's reconnect ladder exactly.
/// `outbound` is the receiver half of the channel the handle writes stream
/// open/cancel frames to.
pub async fn run_pump(
    handle: DshHandle,
    ws_key: String,
    event_tx: async_channel::Sender<UiEvent>,
    stop: async_channel::Receiver<()>,
    outbound: async_channel::Receiver<String>,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let token = handle.inner.token.clone();
    let was_connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut attempts = 0u32;
    let mut delay = crate::oc::RECONNECT_INITIAL;

    loop {
        // 1) cookie (fresh per generation — the old one died with the WS)
        let cookie = match connect_with_token(handle.base_url(), &token).await {
            Ok(cookie) => cookie,
            Err(err) => {
                tracing::warn!("[{ws_key}] dsh connect failed: {err:#}");
                tokio::select! {
                    _ = stop.recv() => return,
                    _ = tokio::time::sleep(delay) => {},
                }
                delay = (delay * 2).min(crate::oc::RECONNECT_MAX);
                continue;
            }
        };
        handle.set_cookie(cookie.clone());

        // 2) websocket — the builder fills the handshake headers; the
        // pre-built http::Request path adds nothing (no sec-websocket-key)
        let uri: tokio_tungstenite::tungstenite::http::Uri =
            format!("{}/api/remote.mux", handle.base_url())
                .replace("http", "ws")
                .parse()
                .expect("ws uri");
        let request = tokio_tungstenite::tungstenite::client::ClientRequestBuilder::new(uri)
            .with_header("cookie", cookie.clone());
        let connected = tokio::select! {
            _ = stop.recv() => return,
            result = tokio_tungstenite::connect_async(request) => result.map(|(socket, _)| socket),
        };
        let socket = match connected {
            Ok(socket) => socket,
            Err(err) => {
                tracing::warn!("[{ws_key}] dsh websocket failed: {err}");
                if !reconnect_pace(
                    &stop,
                    &mut attempts,
                    &mut delay,
                    &was_connected,
                    &event_tx,
                    &ws_key,
                )
                .await
                {
                    return;
                }
                continue;
            }
        };
        tracing::info!("[{ws_key}] dsh event stream connected");
        attempts = 0;
        delay = crate::oc::RECONNECT_INITIAL;
        was_connected.store(true, std::sync::atomic::Ordering::Release);
        let _ = event_tx
            .send(UiEvent::StreamStatus {
                ws_key: ws_key.clone(),
                status: StreamStatus::Connected,
            })
            .await;
        // the store's connect note keys off this event; scoped announce keeps
        // it inside dsh tiles (opencode's own server.connected must not land here)
        let _ = event_tx
            .send(UiEvent::Event {
                ws_key: ws_key.clone(),
                event: serde_json::json!({ "type": "server.connected" }),
            })
            .await;

        // 3) Open the streams this generation needs straight on this socket:
        // the pump is the only reader of the bounded outbound channel, so
        // queueing its own reopen frames there would deadlock once the
        // generation needs more frames than the channel holds (audit B5).
        while outbound.try_recv().is_ok() {}
        let session_ids = handle.reconnect_sessions();
        let (mut sink, mut stream) = socket.split();
        let started = std::time::Instant::now();
        let mut opened = true;
        let frames = std::iter::once(json!({
            "type": "open", "streamId": "events", "endpoint": "$events", "payload": { "args": {} },
        }))
        .chain(session_ids.iter().map(|id| {
            json!({
                "type": "open",
                "streamId": follow_stream_id(id),
                "endpoint": "session/follow",
                "payload": { "args": { "request": {
                    "address": { "kind": "session", "sessionId": id },
                    "maxMessages": 120,
                } } },
            })
        }));
        for frame in frames {
            if sink
                .send(Message::Text(frame.to_string().into()))
                .await
                .is_err()
            {
                opened = false;
                break;
            }
        }
        if opened {
            loop {
                tokio::select! {
                _ = stop.recv() => return,
                frame = outbound.recv() => {
                    match frame {
                        Ok(frame) => {
                            if sink.send(Message::Text(frame.into())).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                incoming = stream.next() => {
                    let message = match incoming {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = sink.send(Message::Pong(payload)).await;
                            continue;
                        }
                        Some(Ok(_)) => continue,
                        Some(Err(err)) => {
                            tracing::warn!("[{ws_key}] dsh event stream dropped: {err}");
                            break;
                        }
                        None => {
                            tracing::info!("[{ws_key}] dsh event stream ended");
                            break;
                        }
                    };
                    let Ok(frame) = serde_json::from_str::<Value>(&message) else { continue };
                    dispatch_frame(&handle, &ws_key, &event_tx, &frame);
                }
                }
            }
        }
        let _ = sink.close().await;
        if started.elapsed() >= crate::oc::RECONNECT_RESET_AFTER {
            attempts = 0;
            delay = crate::oc::RECONNECT_INITIAL;
        }
        attempts += 1;
        if attempts >= crate::oc::RECONNECT_MAX_ATTEMPTS {
            tracing::error!(
                "[{ws_key}] dsh event stream still down after {} attempts — giving up",
                crate::oc::RECONNECT_MAX_ATTEMPTS
            );
            let _ = event_tx
                .send(UiEvent::StreamStatus {
                    ws_key: ws_key.clone(),
                    status: StreamStatus::Failed,
                })
                .await;
            return;
        }
        if was_connected.swap(false, std::sync::atomic::Ordering::AcqRel) {
            let _ = event_tx
                .send(UiEvent::StreamStatus {
                    ws_key: ws_key.clone(),
                    status: StreamStatus::Reconnecting { attempt: attempts },
                })
                .await;
        }
        tokio::select! {
            _ = stop.recv() => return,
            _ = tokio::time::sleep(delay) => {},
        }
        delay = (delay * 2).min(crate::oc::RECONNECT_MAX);
    }
}

async fn reconnect_pace(
    stop: &async_channel::Receiver<()>,
    attempts: &mut u32,
    delay: &mut Duration,
    was_connected: &Arc<std::sync::atomic::AtomicBool>,
    event_tx: &async_channel::Sender<UiEvent>,
    ws_key: &str,
) -> bool {
    // true = keep retrying (the caller loops), false = stop the pump
    *attempts += 1;
    if *attempts >= crate::oc::RECONNECT_MAX_ATTEMPTS {
        let _ = event_tx
            .send(UiEvent::StreamStatus {
                ws_key: ws_key.to_string(),
                status: StreamStatus::Failed,
            })
            .await;
        return false;
    }
    if was_connected.swap(false, std::sync::atomic::Ordering::AcqRel) {
        let _ = event_tx
            .send(UiEvent::StreamStatus {
                ws_key: ws_key.to_string(),
                status: StreamStatus::Reconnecting { attempt: *attempts },
            })
            .await;
    }
    tokio::select! {
        _ = stop.recv() => return false,
        _ = tokio::time::sleep(*delay) => {},
    }
    *delay = (*delay * 2).min(crate::oc::RECONNECT_MAX);
    true
}

/// Guards the store from a second error card for one backend failure: both
/// the `$events` emit and the durable `turn/end` reason carry it.
fn error_is_duplicate(handle: &DshHandle, event: &Value) -> bool {
    if event.get("type").and_then(Value::as_str) != Some("session.error") {
        return false;
    }
    let session = event
        .pointer("/properties/sessionID")
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = event
        .pointer("/properties/error/message")
        .and_then(Value::as_str)
        .unwrap_or("");
    !handle.error_seen(session, message)
}

/// Routes one mux frame: follow items fold into session state (and emit
/// opencode events), $events items handle remote notifications and waterfalls.
fn dispatch_frame(
    handle: &DshHandle,
    ws_key: &str,
    event_tx: &async_channel::Sender<UiEvent>,
    frame: &Value,
) {
    let stream_id = frame
        .get("streamId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let frame_type = frame.get("type").and_then(Value::as_str).unwrap_or("");
    if frame_type == "error" {
        let code = frame
            .pointer("/error/code")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let message = frame
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("");
        tracing::warn!("[{ws_key}] dsh stream {stream_id} error: {code} {message}");
        return;
    }
    if frame_type != "item" {
        return;
    }
    let Some(value) = frame.get("value") else {
        return;
    };

    if stream_id == "events" {
        if value.get("type").and_then(Value::as_str) == Some("ready") {
            if let Some(client_id) = value.get("clientId").and_then(Value::as_str) {
                handle.set_client_id(client_id);
            }
            tracing::debug!("[{ws_key}] dsh $events ready");
            return;
        }
        for event in fold_remote_event(value) {
            if error_is_duplicate(handle, &event) {
                continue;
            }
            let _ = event_tx.send_blocking(UiEvent::Event {
                ws_key: ws_key.to_string(),
                event,
            });
        }
        // question waterfalls: fold stashed the payload for reply mapping;
        // move it onto the handle where answer_question reads it
        if value.get("type").and_then(Value::as_str) == Some("waterfall")
            && value.get("event").and_then(Value::as_str) == Some("user-questions/request")
            && let Some(event_id) = value.get("eventId").and_then(Value::as_str)
            && let Some(prompts) = take_stashed_questions(event_id)
        {
            handle
                .inner
                .pending_questions
                .lock()
                .expect("dsh questions")
                .insert(event_id.to_string(), prompts);
        }
        return;
    }

    let Some(session_id) = stream_id.strip_prefix("follow-") else {
        return;
    };
    let follow_kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    match follow_kind {
        "snapshot" => {
            let mut sessions = handle.inner.sessions.lock().expect("dsh sessions");
            let state = sessions.entry(session_id.to_string()).or_default();
            *state = DshSessionState::default();
            state.snapshot_seen = true;
            for record in value
                .get("records")
                .and_then(Value::as_array)
                .unwrap_or(&Vec::new())
            {
                let event = record.get("event").cloned().unwrap_or(Value::Null);
                if !event.is_null() {
                    // history fold: builds state, emits nothing — GetMessages
                    // reads the result once snapshot_seen flips
                    let _ = fold_event(state, session_id, &event);
                }
            }
            if let Some(title) = value
                .pointer("/projections/values/title/title")
                .and_then(Value::as_str)
            {
                state.title = Some(title.to_string());
            }
            drop(sessions);
        }
        "event" => {
            let event = value.get("event").cloned().unwrap_or(Value::Null);
            if event.is_null() {
                return;
            }
            let outcome = {
                let mut sessions = handle.inner.sessions.lock().expect("dsh sessions");
                let state = sessions.entry(session_id.to_string()).or_default();
                fold_event(state, session_id, &event)
            };
            // the part events carry the session id: the store routes by it,
            // and its focused-tile fallback cannot — the shared dsh server's
            // stream key ("dsh") never equals the session's workspace folder
            // the message info goes first (like opencode's live order): a
            // text part arriving before its role is known would render a user
            // echo as assistant text
            if let Some(event) = outcome.message_updated {
                let _ = event_tx.send_blocking(UiEvent::Event {
                    ws_key: ws_key.to_string(),
                    event,
                });
            }
            for part in outcome.part_updated {
                let _ = event_tx.send_blocking(UiEvent::Event {
                    ws_key: ws_key.to_string(),
                    event: json!({
                        "type": "message.part.updated",
                        "properties": { "part": part, "sessionID": session_id },
                    }),
                });
            }
            for extra in outcome.extra {
                if error_is_duplicate(handle, &extra) {
                    continue;
                }
                let _ = event_tx.send_blocking(UiEvent::Event {
                    ws_key: ws_key.to_string(),
                    event: extra,
                });
            }
        }
        "assistant-stream" => {
            // dense deltas; the durable event on commit carries the full text,
            // so live streaming granularity is skipped deliberately
        }
        _ => {}
    }
}

// ---- tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn wire_event(event_type: &str, seq: i64, data: Value) -> Value {
        json!({ "type": event_type, "seq": seq, "time": 1793271000000i64 + seq, "data": data })
    }

    #[test]
    fn token_from_line_extracts_urlsafe_token() {
        let line =
            "dsh web: http://127.0.0.1:39871/?token=0gdgPb7dID15XjFBuBiGF_F6nR9eTeYnmEuJMxbawJs";
        assert_eq!(
            token_from_line(line),
            Some("0gdgPb7dID15XjFBuBiGF_F6nR9eTeYnmEuJMxbawJs")
        );
        assert_eq!(token_from_line("no token here"), None);
        assert_eq!(token_from_line("token="), None);
    }

    #[test]
    fn folds_a_full_turn_into_opencode_shapes() {
        let mut state = DshSessionState::default();
        let sid = "session-1";

        // human prompt (injected contexts must be ignored)
        let prompt = fold_event(
            &mut state,
            sid,
            &wire_event(
                "user/message",
                8,
                json!({
                    "id": "msg-8",
                    "role": "user",
                    "content": [ { "type": "text", "text": "Say ready" } ],
                    "source": { "kind": "user" },
                }),
            ),
        );
        assert!(prompt.message_updated.is_some());
        assert_eq!(prompt.part_updated.len(), 1);
        let noise = fold_event(
            &mut state,
            sid,
            &wire_event(
                "user/message",
                9,
                json!({
                    "id": "msg-9",
                    "role": "user",
                    "content": [ { "type": "text", "text": "runtime context" } ],
                    "source": { "kind": "runtime-context" },
                }),
            ),
        );
        assert!(noise.message_updated.is_none());

        // tool call → running tool part on the step's virtual message
        let call = fold_event(
            &mut state,
            sid,
            &wire_event(
                "tool/call",
                10,
                json!({
                    "turn": 1, "step": 1, "callId": "call-1", "name": "write",
                    "arguments": "{\"path\":\"notes.txt\",\"content\":\"probe\"}",
                }),
            ),
        );
        let part = call.part_updated[0].clone();
        assert_eq!(part.pointer("/state/status"), Some(&json!("running")));
        assert_eq!(part.pointer("/state/input/path"), Some(&json!("notes.txt")));

        // assistant message of the same step lands on the same message id
        let assistant = fold_event(
            &mut state,
            sid,
            &wire_event(
                "assistant/message",
                11,
                json!({
                    "turn": 1, "step": 1,
                    "message": {
                        "id": "msg-a", "role": "assistant",
                        "content": [
                            { "type": "reasoning", "text": "thinking" },
                            { "type": "text", "text": "ready" },
                            { "type": "tool-call", "id": "call-1", "name": "write", "arguments": "{}" },
                        ],
                        "source": { "kind": "model", "provider": "zai", "model": "glm-5.3-flash" },
                    },
                    "usage": { "inputTokens": 10, "outputTokens": 2, "cacheReadTokens": 5 },
                }),
            ),
        );
        let info = assistant
            .message_updated
            .as_ref()
            .unwrap()
            .pointer("/properties/info")
            .cloned()
            .unwrap();
        assert_eq!(info.get("providerID"), Some(&json!("zai")));
        assert_eq!(info.get("modelID"), Some(&json!("glm-5.3-flash")));
        assert_eq!(info.pointer("/tokens/cache/read"), Some(&json!(5)));
        // tool-call blocks must not duplicate the event-driven tool part
        assert_eq!(assistant.part_updated.len(), 2); // reasoning + text only

        // result completes the same part
        let result = fold_event(
            &mut state,
            sid,
            &wire_event(
                "tool/result",
                12,
                json!({
                    "turn": 1, "step": 1,
                    "message": {
                        "id": "msg-tr", "role": "tool", "toolCallId": "call-1", "isError": false,
                        "content": [ { "type": "text", "text": "wrote 1 file" } ],
                    },
                }),
            ),
        );
        let part = result.part_updated[0].clone();
        assert_eq!(part.pointer("/state/status"), Some(&json!("completed")));
        assert_eq!(part.pointer("/state/output"), Some(&json!("wrote 1 file")));

        // turn lifecycle
        let start = fold_event(
            &mut state,
            sid,
            &wire_event("turn/start", 13, json!({ "turn": 1 })),
        );
        assert_eq!(start.extra[0].get("type"), Some(&json!("session.status")));
        let end = fold_event(
            &mut state,
            sid,
            &wire_event(
                "turn/end",
                14,
                json!({ "turn": 1, "reason": { "kind": "completed" } }),
            ),
        );
        assert_eq!(end.extra[0].get("type"), Some(&json!("session.idle")));
        assert!(end.extra.len() == 1); // no error event on success

        // stored transcript: one user + one assistant message, three parts
        assert_eq!(state.messages.len(), 2);
        let assistant_message = state.messages.iter().find(|m| m.id == "dsh-1-1").unwrap();
        assert_eq!(assistant_message.parts.len(), 3); // reasoning, text, tool
    }

    #[test]
    fn turn_end_error_maps_to_session_error() {
        let mut state = DshSessionState::default();
        let outcome = fold_event(
            &mut state,
            "s",
            &wire_event(
                "turn/end",
                18,
                json!({
                    "turn": 1,
                    "reason": { "kind": "error", "error": { "message": "Insufficient Balance", "code": "ACCOUNT_QUOTA", "status": 402 } },
                }),
            ),
        );
        assert_eq!(outcome.extra[0].get("type"), Some(&json!("session.idle")));
        assert_eq!(outcome.extra[1].get("type"), Some(&json!("session.error")));
        assert_eq!(
            outcome.extra[1].pointer("/properties/error/message"),
            Some(&json!("Insufficient Balance"))
        );
    }

    #[test]
    fn remote_status_emit_maps_both_ways() {
        let busy = fold_remote_event(&json!({
            "type": "emit", "event": "api-session/status", "args": ["s1", true],
        }));
        assert_eq!(busy[0].pointer("/properties/status"), Some(&json!("busy")));
        let idle = fold_remote_event(&json!({
            "type": "emit", "event": "api-session/status", "args": ["s1", false],
        }));
        assert_eq!(idle[0].get("type"), Some(&json!("session.idle")));
    }

    #[test]
    fn waterfall_approval_maps_to_v2_permission() {
        let events = fold_remote_event(&json!({
            "type": "waterfall", "eventId": "evt-1", "event": "approval/request", "agentId": "s1",
            "request": { "toolName": "write", "reason": "writing notes.txt" },
        }));
        assert_eq!(events[0].get("type"), Some(&json!("permission.v2.asked")));
        assert_eq!(
            events[0].pointer("/properties/sessionID"),
            Some(&json!("s1"))
        );
        assert_eq!(
            events[0].pointer("/properties/action"),
            Some(&json!("write"))
        );
        assert_eq!(
            events[0].pointer("/properties/resources/0"),
            Some(&json!("writing notes.txt"))
        );
    }

    #[test]
    fn waterfall_question_stashes_payload_for_reply() {
        let events = fold_remote_event(&json!({
            "type": "waterfall", "eventId": "evt-2", "event": "user-questions/request", "agentId": "s1",
            "request": { "questions": [ { "question": "Which?", "multiSelect": false, "options": [
                { "label": "A", "description": "first" }, { "label": "B" },
            ] } ] },
        }));
        assert_eq!(events[0].get("type"), Some(&json!("question.asked")));
        assert!(take_stashed_questions("evt-2").is_some());
        assert!(take_stashed_questions("evt-2").is_none());
    }
}
