//! opencode v1 through the harness interface.
//!
//! This is a behaviour-preserving extraction of the app's existing client: one
//! managed `opencode serve` per workspace, attach-to-healthy, the 25 s
//! readiness wait, the SSE pump with the reconnect ladder, the REST catalogs
//! and the idle stop. The store still reads the legacy net path until the
//! contract ticket; the adapter is the seam the host and the probes use.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_channel::{Receiver, Sender, unbounded};
use serde_json::Value;

use super::interface::Harness;
use super::shared::{PendingApprovals, filediffs, form_answers, partition_commands};
use super::types::*;
use crate::oc::{
    ApiClient, RECONNECT_INITIAL, RECONNECT_MAX, RECONNECT_MAX_ATTEMPTS, RECONNECT_RESET_AFTER,
    ServerProcess, child_exited, find_free_port, read_question_request,
};
use crate::settings::HarnessKind;

const HEALTH_POLL: Duration = Duration::from_secs(10);

/// The managed opencode v1 adapter.
pub struct OpenCodeV1Harness {
    id: HarnessId,
    config: HarnessConfig,
    runtime: tokio::runtime::Runtime,
    process: Option<ServerProcess>,
    client: Option<ApiClient>,
    health: Arc<Mutex<HarnessHealth>>,
    tx: Sender<HarnessEvent>,
    rx: Receiver<HarnessEvent>,
    stop: Option<Sender<()>>,
    pump: Option<tokio::task::JoinHandle<()>>,
    health_task: Option<tokio::task::JoinHandle<()>>,
    pending: Arc<PendingApprovals>,
    last_model: Arc<Mutex<HashMap<SessionId, String>>>,
}

impl OpenCodeV1Harness {
    pub fn new(config: HarnessConfig) -> Self {
        let (tx, rx) = unbounded();
        Self {
            id: HarnessId::new("opencode"),
            config,
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime"),
            process: None,
            client: None,
            health: Arc::new(Mutex::new(HarnessHealth {
                state: HealthState::Starting,
                detail: None,
            })),
            tx,
            rx,
            stop: None,
            pump: None,
            health_task: None,
            pending: PendingApprovals::new(),
            last_model: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn client(&self) -> Result<ApiClient, HarnessError> {
        self.client
            .clone()
            .ok_or_else(|| HarnessError::Unavailable("opencode is not running".into()))
    }

    fn block_on<T>(
        &self,
        future: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> Result<T, HarnessError> {
        self.runtime
            .block_on(future)
            .map_err(|err| HarnessError::Failed(format!("{err:#}")))
    }

    fn set_health(&self, state: HarnessHealth) {
        *self.health.lock().expect("health lock") = state;
    }

    fn session_summary(&self, info: crate::oc::SessionInfo) -> SessionSummary {
        SessionSummary {
            id: SessionId::new(info.id),
            title: info.title,
            parent: info.parent_id.map(SessionId::new),
            directory: self.config.directory.clone(),
            updated_ms: info.updated_ms,
        }
    }

    fn spawn_pump(&mut self, client: ApiClient, stop_rx: Receiver<()>) {
        let tx = self.tx.clone();
        let health = Arc::clone(&self.health);
        let pending = Arc::clone(&self.pending);
        let task = self.runtime.spawn(async move {
            let mut attempts = 0u32;
            let mut delay = RECONNECT_INITIAL;
            let was_connected = Arc::new(AtomicBool::new(false));
            loop {
                let (cancel_tx, cancel_rx) = async_channel::bounded::<()>(1);
                let started = Instant::now();
                let on_event = {
                    let tx = tx.clone();
                    let pending = Arc::clone(&pending);
                    move |event: Value| {
                        if let Some(typed) = translate_event(&event) {
                            pending.observe(&typed);
                            let _ = tx.try_send(typed);
                        }
                    }
                };
                let on_attached = {
                    let tx = tx.clone();
                    let health = Arc::clone(&health);
                    let was_connected = Arc::clone(&was_connected);
                    move || {
                        was_connected.store(true, Ordering::Release);
                        *health.lock().expect("health") = HarnessHealth::ready();
                        let _ = tx.try_send(HarnessEvent::Connected);
                    }
                };
                let result = tokio::select! {
                    _ = stop_rx.recv() => return,
                    result = client.stream_events(on_event, on_attached, cancel_rx) => result,
                };
                drop(cancel_tx);
                if let Err(err) = result {
                    tracing::warn!("opencode event stream dropped: {err:#}");
                }
                if started.elapsed() >= RECONNECT_RESET_AFTER {
                    attempts = 0;
                    delay = RECONNECT_INITIAL;
                }
                attempts += 1;
                if attempts >= RECONNECT_MAX_ATTEMPTS {
                    *health.lock().expect("health") =
                        HarnessHealth::unavailable("opencode event stream is down");
                    let _ = tx.try_send(HarnessEvent::Disconnected {
                        reason: Some("event stream is down".into()),
                    });
                    return;
                }
                if was_connected.swap(false, Ordering::AcqRel) {
                    *health.lock().expect("health") =
                        HarnessHealth::unavailable("opencode event stream is reconnecting");
                    let _ = tx.try_send(HarnessEvent::Disconnected {
                        reason: Some(format!("reconnecting (attempt {attempts})")),
                    });
                }
                loop {
                    tokio::select! {
                        _ = stop_rx.recv() => return,
                        _ = tokio::time::sleep(delay) => {}
                    }
                    let healthy = tokio::select! {
                        _ = stop_rx.recv() => return,
                        healthy = client.healthy() => healthy,
                    };
                    if healthy {
                        break;
                    }
                    delay = (delay * 2).min(RECONNECT_MAX);
                }
                delay = (delay * 2).min(RECONNECT_MAX);
            }
        });
        self.pump = Some(task);
    }

    fn spawn_health(
        &mut self,
        client: ApiClient,
        stop_rx: Receiver<()>,
        exit_probe: Arc<Mutex<Option<tokio::process::Child>>>,
    ) {
        let tx = self.tx.clone();
        let health = Arc::clone(&self.health);
        let task = self.runtime.spawn(async move {
            loop {
                tokio::select! {
                    _ = stop_rx.recv() => return,
                    _ = tokio::time::sleep(HEALTH_POLL) => {}
                }
                if child_exited(&exit_probe) {
                    *health.lock().expect("health") = HarnessHealth::unavailable("opencode exited");
                    let _ = tx.try_send(HarnessEvent::Disconnected {
                        reason: Some("opencode exited".into()),
                    });
                    return;
                }
                let healthy = tokio::select! {
                    _ = stop_rx.recv() => return,
                    healthy = client.healthy() => healthy,
                };
                let state = if healthy {
                    HarnessHealth::ready()
                } else {
                    HarnessHealth::unavailable("opencode health check failed")
                };
                *health.lock().expect("health") = state.clone();
                let _ = tx.try_send(HarnessEvent::Health(state));
            }
        });
        self.health_task = Some(task);
    }

    fn command_catalog(&self) -> Result<(Vec<Command>, Vec<Skill>), HarnessError> {
        let client = self.client()?;
        let commands = self.block_on(client.get_commands())?;
        Ok(partition_commands(commands))
    }
}

impl Harness for OpenCodeV1Harness {
    fn id(&self) -> HarnessId {
        self.id.clone()
    }

    fn scope(&self) -> Option<std::path::PathBuf> {
        self.config.directory.clone()
    }

    fn health(&self) -> HarnessHealth {
        self.health.lock().expect("health lock").clone()
    }

    fn start(&mut self) -> Result<(), HarnessError> {
        if self.client.is_some() {
            return Ok(());
        }
        let folder = self
            .config
            .directory
            .as_ref()
            .map(|directory| directory.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".into());
        let port = find_free_port().map_err(|err| HarnessError::Failed(format!("{err:#}")))?;
        let result = self.runtime.block_on(ServerProcess::start(
            HarnessKind::OpenCode,
            &self.config.executable,
            &folder,
            port,
            |line| tracing::debug!("opencode: {line}"),
        ));
        let process = match result {
            Ok(process) => process,
            Err(err) => {
                let detail = format!("{err:#}");
                self.set_health(HarnessHealth::unavailable(detail.clone()));
                return Err(HarnessError::Failed(detail));
            }
        };
        let client = ApiClient::new(&process.base_url);
        let exit_probe = process.exit_probe();
        self.process = Some(process);
        self.client = Some(client.clone());
        self.set_health(HarnessHealth::ready());
        let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
        self.stop = Some(stop_tx);
        self.spawn_pump(client.clone(), stop_rx.clone());
        self.spawn_health(client, stop_rx, exit_probe);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), HarnessError> {
        if self.stop.is_none() && self.process.is_none() && self.client.is_none() {
            return Ok(());
        }
        if let Some(stop) = self.stop.take() {
            stop.close();
        }
        if let Some(task) = self.pump.take() {
            task.abort();
        }
        if let Some(task) = self.health_task.take() {
            task.abort();
        }
        if let Some(mut process) = self.process.take() {
            process.stop();
        }
        self.client = None;
        self.set_health(HarnessHealth::unavailable("stopped"));
        let _ = self.tx.try_send(HarnessEvent::Disconnected {
            reason: Some("stopped".into()),
        });
        Ok(())
    }

    fn providers(&mut self) -> Result<Vec<Provider>, HarnessError> {
        let client = self.client()?;
        let providers = self.block_on(client.get_providers())?;
        Ok(providers
            .into_iter()
            .map(|(id, name)| Provider { id, name })
            .collect())
    }

    fn models(&mut self) -> Result<Vec<Model>, HarnessError> {
        let client = self.client()?;
        let models = self.block_on(client.get_models())?;
        Ok(models
            .into_iter()
            .map(|model| {
                let provider = model
                    .key
                    .split_once('/')
                    .map(|(provider, _)| provider.to_string())
                    .unwrap_or_default();
                Model {
                    key: model.key,
                    provider,
                    variants: model.variants,
                    context_limit: model.context_limit,
                }
            })
            .collect())
    }

    fn agents(&mut self) -> Result<Vec<Agent>, HarnessError> {
        let client = self.client()?;
        let agents = self.block_on(client.get_agents())?;
        Ok(agents
            .into_iter()
            .map(|agent| Agent {
                name: agent.name,
                mode: agent.mode,
                hidden: agent.hidden,
            })
            .collect())
    }

    fn commands(&mut self) -> Result<Vec<Command>, HarnessError> {
        self.command_catalog().map(|(commands, _)| commands)
    }

    fn skills(&mut self) -> Result<Vec<Skill>, HarnessError> {
        self.command_catalog().map(|(_, skills)| skills)
    }

    fn sessions(&mut self) -> Result<Vec<SessionSummary>, HarnessError> {
        let client = self.client()?;
        let sessions = self.block_on(client.list_sessions())?;
        Ok(sessions
            .into_iter()
            .map(|info| self.session_summary(info))
            .collect())
    }

    fn session(&mut self, session: &SessionId) -> Result<SessionSummary, HarnessError> {
        self.sessions()?
            .into_iter()
            .find(|summary| &summary.id == session)
            .ok_or_else(|| HarnessError::Unavailable(format!("session {session} not found")))
    }

    fn create_session(&mut self, spec: NewSession) -> Result<SessionSummary, HarnessError> {
        if spec.parent.is_some() {
            return Err(HarnessError::Unsupported(
                "create_session with a parent on opencode v1",
            ));
        }
        let title = spec.title.unwrap_or_else(|| "New chat".into());
        let client = self.client()?;
        let info = self.block_on(client.create_session(&title))?;
        Ok(self.session_summary(info))
    }

    fn fork_session(
        &mut self,
        _session: &SessionId,
        _before: Option<&str>,
    ) -> Result<SessionSummary, HarnessError> {
        // the app's v1 fork is a UI flow (create a session for a transcript
        // seed); the opencode API has no server-side fork
        Err(HarnessError::Unsupported("fork_session"))
    }

    fn rename_session(&mut self, session: &SessionId, title: &str) -> Result<(), HarnessError> {
        let client = self.client()?;
        self.block_on(client.rename_session(session.as_str(), title))
            .map(|_| ())
    }

    fn delete_session(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        let client = self.client()?;
        self.block_on(client.delete_session(session.as_str()))
    }

    fn send_prompt(&mut self, session: &SessionId, prompt: Prompt) -> Result<(), HarnessError> {
        let Prompt {
            text,
            choice,
            background,
        } = prompt;
        if background {
            return Err(HarnessError::Unsupported("background_prompt"));
        }
        if let Some(model) = choice.model.as_deref() {
            self.last_model
                .lock()
                .expect("last model")
                .insert(session.clone(), model.to_string());
        }
        let client = self.client()?;
        let session_id = session.as_str().to_string();
        let error_session = session.clone();
        let tx = self.tx.clone();
        // v1's message POST resolves only when the turn ends: send it on the
        // runtime so an approval or an interrupt can still reach this instance
        self.runtime.spawn(async move {
            if let Err(err) = client
                .send_message(
                    &session_id,
                    &text,
                    choice.model.as_deref(),
                    choice.variant.as_deref(),
                    choice.agent.as_deref(),
                    &[],
                )
                .await
            {
                let _ = tx.try_send(HarnessEvent::Error {
                    session: Some(error_session),
                    message: format!("{err:#}"),
                    retryable: false,
                });
            }
        });
        Ok(())
    }

    fn interrupt(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        let client = self.client()?;
        self.block_on(client.abort(session.as_str()))
    }

    fn compact(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        let model = self
            .last_model
            .lock()
            .expect("last model")
            .get(session)
            .cloned()
            .ok_or_else(|| HarnessError::Unavailable("pick a model before compacting".into()))?;
        let client = self.client()?;
        self.block_on(client.compact_session(session.as_str(), &model))
    }

    fn run_command(
        &mut self,
        session: &SessionId,
        command: CommandRun,
    ) -> Result<(), HarnessError> {
        let client = self.client()?;
        let session_id = session.as_str().to_string();
        let error_session = session.clone();
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            if let Err(err) = client
                .send_command(
                    &session_id,
                    &command.name,
                    &command.text,
                    None,
                    None,
                    None,
                    &[],
                )
                .await
            {
                let _ = tx.try_send(HarnessEvent::Error {
                    session: Some(error_session),
                    message: format!("{err:#}"),
                    retryable: false,
                });
            }
        });
        Ok(())
    }

    fn activate_skill(&mut self, _session: &SessionId, _skill: &str) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("activate_skill"))
    }

    fn permissions(&mut self, session: &SessionId) -> Result<Vec<PermissionRequest>, HarnessError> {
        Ok(self.pending.permissions_for(session))
    }

    fn reply_permission(
        &mut self,
        session: &SessionId,
        request: &str,
        decision: PermissionDecision,
    ) -> Result<(), HarnessError> {
        let response = match decision {
            PermissionDecision::Once => "once",
            PermissionDecision::Always => "always",
            PermissionDecision::Reject => "reject",
        };
        let client = self.client()?;
        self.block_on(client.respond_permission(session.as_str(), request, response))?;
        self.pending.remove_permission(request);
        Ok(())
    }

    fn forms(&mut self, session: &SessionId) -> Result<Vec<FormRequest>, HarnessError> {
        Ok(self.pending.forms_for(session))
    }

    fn reply_form(
        &mut self,
        _session: &SessionId,
        form: &str,
        answer: FormAnswer,
    ) -> Result<(), HarnessError> {
        let pending = self
            .pending
            .form(form)
            .ok_or_else(|| HarnessError::Unavailable(format!("form {form} not found")))?;
        let answers = form_answers(&pending, answer);
        let client = self.client()?;
        self.block_on(client.reply_question(&pending.id, &answers))?;
        self.pending.remove_form(form);
        Ok(())
    }

    fn diff(&mut self, session: &SessionId) -> Result<Vec<FileDiff>, HarnessError> {
        let client = self.client()?;
        let messages = self.block_on(client.get_messages(session.as_str()))?;
        Ok(filediffs(messages))
    }

    fn inbox(&mut self, _session: &SessionId) -> Result<Vec<InboxItem>, HarnessError> {
        Err(HarnessError::Unsupported("inbox"))
    }

    fn update_inbox(
        &mut self,
        _session: &SessionId,
        _item: &str,
        _delivery: InboxDelivery,
    ) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("update_inbox"))
    }

    fn cancel_inbox(&mut self, _session: &SessionId, _item: &str) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("cancel_inbox"))
    }

    fn stage_revert(&mut self, _session: &SessionId, _before: &str) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("stage_revert"))
    }

    fn commit_revert(&mut self, _session: &SessionId) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("commit_revert"))
    }

    fn clear_revert(&mut self, _session: &SessionId) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("clear_revert"))
    }

    fn run_shell(&mut self, _session: &SessionId, _command: &str) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("run_shell"))
    }

    fn background_session(&mut self, _session: &SessionId) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("background_session"))
    }

    fn mcp_servers(&mut self) -> Result<Vec<McpServer>, HarnessError> {
        let client = self.client()?;
        let config = self.block_on(client.get_config())?;
        let mut servers = Vec::new();
        if let Some(entries) = config.get("mcp").and_then(Value::as_object) {
            for (name, entry) in entries {
                if !entry.is_object() {
                    continue;
                }
                let enabled = entry
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                servers.push(McpServer {
                    name: name.clone(),
                    status: if enabled {
                        McpStatus::Pending
                    } else {
                        McpStatus::Disabled
                    },
                });
            }
        }
        servers.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(servers)
    }

    fn add_mcp(&mut self, _config: McpConfig) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("add_mcp"))
    }

    fn connect_mcp(&mut self, name: &str) -> Result<(), HarnessError> {
        let _ = name;
        Err(HarnessError::Unsupported("connect_mcp"))
    }

    fn disconnect_mcp(&mut self, name: &str) -> Result<(), HarnessError> {
        let _ = name;
        Err(HarnessError::Unsupported("disconnect_mcp"))
    }

    fn remove_mcp(&mut self, _name: &str) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("remove_mcp"))
    }

    fn events(&self) -> Receiver<HarnessEvent> {
        self.rx.clone()
    }
}

impl Drop for OpenCodeV1Harness {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Translate one opencode v1 `/event` frame into the typed vocabulary.
///
/// Unknown events (todos, logs, plugin chatter) are ignored; the adapter's
/// pending approval and question maps are maintained from the returned events
/// by the pump.
pub(super) fn translate_event(event: &Value) -> Option<HarnessEvent> {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    let properties = event.get("properties").unwrap_or(event);
    match kind {
        "session.created" | "session.updated" => {
            let info = properties.get("info").unwrap_or(properties);
            let id = info.get("id").and_then(Value::as_str)?;
            let session = SessionId::new(id);
            let title = info
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string);
            let parent = info
                .get("parentID")
                .and_then(Value::as_str)
                .map(SessionId::new);
            let agent = info
                .get("agent")
                .and_then(Value::as_str)
                .map(str::to_string);
            if kind == "session.created" {
                Some(HarnessEvent::SessionCreated {
                    session,
                    title,
                    parent,
                    agent,
                })
            } else {
                Some(HarnessEvent::SessionChanged {
                    session,
                    title,
                    parent,
                    agent,
                })
            }
        }
        "session.deleted" => {
            let id = properties
                .pointer("/info/id")
                .and_then(Value::as_str)
                .or_else(|| properties.get("sessionID").and_then(Value::as_str))?;
            Some(HarnessEvent::SessionDeleted {
                session: SessionId::new(id),
            })
        }
        "session.idle" => read_session(properties).map(|session| HarnessEvent::SessionStatus {
            session,
            state: SessionState::Idle,
        }),
        "session.status" => {
            let session = read_session(properties)?;
            let state = match properties.pointer("/status/type").and_then(Value::as_str) {
                Some("idle") => SessionState::Idle,
                Some("retry") => SessionState::Retry,
                _ => SessionState::Busy,
            };
            Some(HarnessEvent::SessionStatus { session, state })
        }
        "session.error" => {
            let session = read_session(properties);
            let message = properties
                .pointer("/error/data/message")
                .and_then(Value::as_str)
                .or_else(|| properties.pointer("/error/message").and_then(Value::as_str))
                .or_else(|| properties.get("error").and_then(Value::as_str))
                .unwrap_or("the harness reported an error")
                .to_string();
            let retryable = properties
                .pointer("/error/data/isRetryable")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(HarnessEvent::Error {
                session,
                message,
                retryable,
            })
        }
        "message.updated" => message_event(properties),
        "message.part.updated" => part_event(properties),
        "message.part.removed" => {
            let session = read_session(properties)?;
            let part_id = properties
                .get("partID")
                .and_then(Value::as_str)
                .or_else(|| properties.pointer("/part/id").and_then(Value::as_str))?;
            Some(HarnessEvent::PartRemoved {
                session,
                part_id: part_id.to_string(),
            })
        }
        "permission.asked" | "permission.v2.asked" => permission_event(properties),
        "permission.replied" | "permission.v2.replied" => permission_reply(properties),
        "question.asked" => question_event(properties),
        "question.replied" | "question.rejected" => question_resolved(kind, properties),
        _ => None,
    }
}

fn read_session(properties: &Value) -> Option<SessionId> {
    properties
        .get("sessionID")
        .and_then(Value::as_str)
        .or_else(|| properties.pointer("/info/id").and_then(Value::as_str))
        .map(SessionId::new)
}

fn message_event(properties: &Value) -> Option<HarnessEvent> {
    let info = properties.get("info").unwrap_or(properties);
    let session = info
        .get("sessionID")
        .and_then(Value::as_str)
        .or_else(|| properties.get("sessionID").and_then(Value::as_str))
        .map(SessionId::new)?;
    let id = info.get("id").and_then(Value::as_str)?.to_string();
    let role = match info.get("role").and_then(Value::as_str) {
        Some("user") => MessageRole::User,
        _ => MessageRole::Assistant,
    };
    let created_ms = info
        .pointer("/time/created")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let completed_ms = info.pointer("/time/completed").and_then(Value::as_i64);
    let provider = info
        .get("providerID")
        .and_then(Value::as_str)
        .map(str::to_string);
    let model = info
        .get("modelID")
        .and_then(Value::as_str)
        .map(str::to_string);
    let error = info
        .pointer("/error/data/message")
        .and_then(Value::as_str)
        .or_else(|| info.pointer("/error/message").and_then(Value::as_str))
        .or_else(|| info.get("error").and_then(Value::as_str))
        .map(str::to_string);
    let tokens = info.get("tokens").and_then(Value::as_object).map(|tokens| {
        let cache = tokens.get("cache").and_then(Value::as_object);
        TokenUsage {
            input: tokens.get("input").and_then(Value::as_i64).unwrap_or(0),
            output: tokens.get("output").and_then(Value::as_i64).unwrap_or(0),
            reasoning: tokens.get("reasoning").and_then(Value::as_i64).unwrap_or(0),
            cache_read: cache
                .and_then(|cache| cache.get("read"))
                .and_then(Value::as_i64)
                .unwrap_or(0),
            cache_write: cache
                .and_then(|cache| cache.get("write"))
                .and_then(Value::as_i64)
                .unwrap_or(0),
        }
    });
    let cost_micros = info
        .get("cost")
        .and_then(Value::as_f64)
        .map(|cost| (cost * 1_000_000.0).round() as i64);
    Some(HarnessEvent::MessageSnapshot {
        message: MessageSnapshot {
            id,
            session,
            role,
            created_ms,
            completed_ms,
            provider,
            model,
            error,
            tokens,
            cost_micros,
        },
    })
}

fn part_event(properties: &Value) -> Option<HarnessEvent> {
    let part = properties.get("part")?;
    let session = part
        .get("sessionID")
        .and_then(Value::as_str)
        .or_else(|| properties.get("sessionID").and_then(Value::as_str))
        .map(SessionId::new)?;
    let id = part.get("id").and_then(Value::as_str)?.to_string();
    let message_id = part
        .get("messageID")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let payload = match part.get("type").and_then(Value::as_str) {
        Some("text") => PartPayload::Text(
            part.get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        Some("reasoning") => PartPayload::Reasoning(
            part.get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
        Some("tool") => {
            let state = part.get("state")?;
            let status = state.get("status").and_then(Value::as_str).unwrap_or("");
            let tool_state = match status {
                "pending" => ToolState::Pending,
                "running" => ToolState::Running,
                "completed" => ToolState::Completed {
                    output: state
                        .get("output")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                },
                "error" => ToolState::Failed {
                    message: state
                        .get("error")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                },
                _ => ToolState::Pending,
            };
            PartPayload::Tool(ToolSnapshot {
                name: part
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string(),
                state: tool_state,
                input: state.get("input").cloned(),
                title: state
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                metadata: state.get("metadata").cloned(),
            })
        }
        Some("file") => PartPayload::File(FileSnapshot {
            url: part
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            filename: part
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        }),
        Some("step-start") => PartPayload::StepStart,
        Some("step-finish") => PartPayload::StepFinish,
        _ => return None,
    };
    Some(HarnessEvent::PartSnapshot {
        part: PartSnapshot {
            id,
            session,
            message_id,
            payload,
        },
    })
}

fn permission_event(properties: &Value) -> Option<HarnessEvent> {
    let id = properties.get("id").and_then(Value::as_str)?.to_string();
    let session = read_session(properties)?;
    let action = properties
        .get("action")
        .and_then(Value::as_str)
        .or_else(|| properties.get("permission").and_then(Value::as_str))
        .unwrap_or("permission")
        .to_string();
    let resources = read_strings(
        properties
            .get("resources")
            .or_else(|| properties.get("patterns")),
    );
    let message = properties
        .pointer("/metadata/command")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            properties
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let diff = properties
        .pointer("/metadata/diff")
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(HarnessEvent::PermissionAsked {
        request: PermissionRequest {
            id,
            session,
            action,
            resources,
            message,
            diff,
        },
    })
}

fn permission_reply(properties: &Value) -> Option<HarnessEvent> {
    let request = properties
        .get("requestID")
        .and_then(Value::as_str)
        .or_else(|| properties.get("id").and_then(Value::as_str))?
        .to_string();
    let session = read_session(properties);
    let decision = match properties
        .get("reply")
        .and_then(Value::as_str)
        .or_else(|| properties.get("response").and_then(Value::as_str))
    {
        Some("always") => PermissionDecision::Always,
        Some("reject") => PermissionDecision::Reject,
        _ => PermissionDecision::Once,
    };
    Some(HarnessEvent::PermissionReplied {
        session,
        request,
        decision,
    })
}

fn question_event(properties: &Value) -> Option<HarnessEvent> {
    let request = read_question_request(properties)?;
    let fields: Vec<FormField> = request
        .questions
        .iter()
        .enumerate()
        .map(|(index, question)| FormField {
            id: index.to_string(),
            title: if question.question.is_empty() {
                question.header.clone()
            } else {
                question.question.clone()
            },
            kind: if question.multiple {
                FormFieldKind::Multiselect
            } else {
                FormFieldKind::Text
            },
            required: true,
            options: question
                .options
                .iter()
                .map(|option| option.label.clone())
                .collect(),
        })
        .collect();
    let title = request
        .questions
        .first()
        .map(|question| {
            if question.header.is_empty() {
                question.question.clone()
            } else {
                question.header.clone()
            }
        })
        .unwrap_or_default();
    Some(HarnessEvent::QuestionAsked {
        request: FormRequest {
            id: request.id,
            session: SessionId::new(request.session_id),
            title,
            fields,
        },
    })
}

fn question_resolved(kind: &str, properties: &Value) -> Option<HarnessEvent> {
    let request = properties
        .get("requestID")
        .and_then(Value::as_str)
        .or_else(|| properties.get("id").and_then(Value::as_str))?
        .to_string();
    let session = read_session(properties);
    let summary = if kind == "question.replied" {
        let answers: Vec<String> = properties
            .get("answers")
            .and_then(Value::as_array)
            .map(|answers| {
                answers
                    .iter()
                    .filter_map(Value::as_array)
                    .map(|labels| {
                        labels
                            .iter()
                            .filter_map(Value::as_str)
                            .filter(|label| !label.is_empty())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .filter(|answer| !answer.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if answers.is_empty() {
            "dismissed".to_string()
        } else {
            format!("answered: {}", answers.join(" · "))
        }
    } else {
        "dismissed".to_string()
    };
    Some(HarnessEvent::QuestionResolved {
        session,
        request,
        summary: Some(summary),
    })
}

fn read_strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{OpenCodeV1Harness, translate_event};
    use crate::harness::{
        Harness, HarnessConfig, HarnessError, HarnessEvent, HarnessId, HealthState, MessageRole,
        PartPayload, PermissionDecision, SessionState, ToolState,
    };
    use crate::store::ChatItem;
    use crate::store::Store;
    use crate::store::test_support::{HomeGuard, lock_home};
    use serde_json::json;

    #[test]
    fn v1_session_and_message_events_become_typed() {
        let created = translate_event(&json!({
            "type": "session.created",
            "properties": { "info": {
                "id": "ses_1",
                "title": "Hello",
                "parentID": "ses_0",
            } },
        }))
        .expect("a session.created frame translates");
        assert!(matches!(
            created,
            HarnessEvent::SessionCreated {
                session,
                title: Some(title),
                parent: Some(parent),
                ..
            } if session.as_str() == "ses_1" && title == "Hello" && parent.as_str() == "ses_0"
        ));

        let message = translate_event(&json!({
            "type": "message.updated",
            "properties": {
                "sessionID": "ses_1",
                "info": {
                    "id": "msg_1",
                    "sessionID": "ses_1",
                    "role": "assistant",
                    "time": { "created": 1, "completed": 9 },
                    "providerID": "openai",
                    "modelID": "gpt",
                    "tokens": { "input": 3, "output": 4 },
                    "cost": 0.0005,
                },
            },
        }))
        .expect("a message.updated frame translates");
        assert!(matches!(
            message,
            HarnessEvent::MessageSnapshot { message }
                if message.role == MessageRole::Assistant
                    && message.completed_ms == Some(9)
                    && message.tokens.as_ref().is_some_and(|tokens| tokens.input == 3 && tokens.output == 4)
                    && message.cost_micros == Some(500)
        ));
    }

    #[test]
    fn v1_parts_tools_permissions_questions_and_idle_become_typed() {
        let text = translate_event(&json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": "ses_1",
                "part": {
                    "id": "prt_1",
                    "sessionID": "ses_1",
                    "messageID": "msg_1",
                    "type": "text",
                    "text": "hi",
                },
            },
        }))
        .expect("a text part translates");
        assert!(matches!(
            text,
            HarnessEvent::PartSnapshot { part }
                if part.message_id == "msg_1" && part.payload == PartPayload::Text("hi".into())
        ));

        let tool = translate_event(&json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": "ses_1",
                "part": {
                    "id": "prt_2",
                    "sessionID": "ses_1",
                    "messageID": "msg_1",
                    "type": "tool",
                    "tool": "bash",
                    "state": {
                        "status": "completed",
                        "output": "ok",
                        "input": { "command": "ls" },
                        "title": "bash",
                        "metadata": { "filediff": { "patch": "diff", "filepath": "x" } },
                    },
                },
            },
        }))
        .expect("a tool part translates");
        assert!(matches!(
            tool,
            HarnessEvent::PartSnapshot { part }
                if matches!(&part.payload, PartPayload::Tool(tool)
                    if tool.name == "bash"
                        && matches!(&tool.state, ToolState::Completed { .. })
                        && tool.title.as_deref() == Some("bash")
                        && tool.input.is_some()
                        && tool.metadata.is_some())
        ));

        let permission = translate_event(&json!({
            "type": "permission.asked",
            "properties": {
                "id": "per_1",
                "sessionID": "ses_1",
                "permission": "bash",
                "patterns": ["ls"],
                "metadata": { "command": "ls", "diff": "diff" },
            },
        }))
        .expect("a permission frame translates");
        assert!(matches!(
            permission,
            HarnessEvent::PermissionAsked { request }
                if request.id == "per_1"
                    && request.action == "bash"
                    && request.resources == ["ls"]
                    && request.message.as_deref() == Some("ls")
                    && request.diff.as_deref() == Some("diff")
        ));

        let question = translate_event(&json!({
            "type": "question.asked",
            "properties": {
                "id": "que_1",
                "sessionID": "ses_1",
                "questions": [{
                    "question": "Pick",
                    "header": "Pick",
                    "multiple": true,
                    "options": [{ "label": "a", "description": "" }],
                }],
            },
        }))
        .expect("a question frame translates");
        assert!(matches!(
            question,
            HarnessEvent::QuestionAsked { request }
                if request.id == "que_1" && request.fields.len() == 1
                    && request.fields[0].options == ["a"]
        ));

        let idle = translate_event(&json!({
            "type": "session.idle",
            "properties": { "sessionID": "ses_1" },
        }))
        .expect("an idle frame translates");
        assert!(matches!(
            idle,
            HarnessEvent::SessionStatus {
                state: SessionState::Idle,
                ..
            }
        ));

        let replied = translate_event(&json!({
            "type": "permission.replied",
            "properties": { "sessionID": "ses_1", "requestID": "per_1", "response": "always" },
        }))
        .expect("a permission.replied frame translates");
        assert!(matches!(
            replied,
            HarnessEvent::PermissionReplied {
                decision: PermissionDecision::Always,
                ..
            }
        ));
    }

    #[test]
    fn an_unknown_event_is_ignored() {
        assert!(translate_event(&json!({ "type": "todo.updated" })).is_none());
    }

    #[test]
    fn a_missing_executable_reports_a_typed_start_failure() {
        let mut harness = OpenCodeV1Harness::new(HarnessConfig {
            executable: "warp-agent-missing-opencode".into(),
            directory: Some(std::env::temp_dir()),
        });
        assert!(matches!(
            harness.start().unwrap_err(),
            HarnessError::Failed(_)
        ));
        assert_eq!(harness.health().state, HealthState::Unavailable);
        harness.stop().expect("a failed start still stops");
    }

    #[gpui_kit::gpui::test]
    fn v1_frames_reach_the_store_through_the_typed_stream(cx: &mut gpui::TestAppContext) {
        let _lock = lock_home();
        let home = HomeGuard::new();
        let folder = home.home.join("workspace");
        std::fs::create_dir_all(&folder).expect("workspace");
        let folder = folder.to_string_lossy().to_string();
        let store = cx.update(Store::for_probe);
        let key = cx.update(|cx| {
            store.update(cx, |store, cx| {
                store.add_workspace(&folder, cx);
                store.set_harness_ingest(true);
                store.seed_probe_tile(&folder, "ses_1", "Probe", Vec::new(), true, cx)
            })
        });

        let frames = [
            json!({
                "type": "session.created",
                "properties": { "info": { "id": "ses_1", "title": "V1 chat" } },
            }),
            json!({
                "type": "session.updated",
                "properties": { "info": { "id": "ses_1", "title": "V1 chat" } },
            }),
            json!({
                "type": "message.updated",
                "properties": {
                    "sessionID": "ses_1",
                    "info": {
                        "id": "msg_1",
                        "sessionID": "ses_1",
                        "role": "assistant",
                        "time": { "created": 1 },
                    },
                },
            }),
            json!({
                "type": "message.part.updated",
                "properties": {
                    "sessionID": "ses_1",
                    "part": {
                        "id": "prt_1",
                        "sessionID": "ses_1",
                        "messageID": "msg_1",
                        "type": "text",
                        "text": "hello from opencode v1",
                    },
                },
            }),
            json!({
                "type": "session.idle",
                "properties": { "sessionID": "ses_1" },
            }),
        ];
        store.update(cx, |store, cx| {
            for frame in frames {
                if let Some(event) = translate_event(&frame) {
                    store
                        .ingest_harness_event(&HarnessId::new("opencode"), event, cx)
                        .expect("the typed event ingests");
                }
            }
        });
        store.read_with(cx, |store, _| {
            let tile = store.tile_by_key(&key).expect("the tile is open");
            assert_eq!(tile.title, "V1 chat");
            assert!(tile.items.iter().any(|item| matches!(
                item.as_ref(),
                ChatItem::AssistantText { text, .. } if text == "hello from opencode v1"
            )));
            assert!(!store.is_busy("ses_1"));
        });
    }
}
