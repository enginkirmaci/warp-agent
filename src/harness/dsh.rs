//! DeepSeek Harness (`dsh web`) through the harness interface.
//!
//! A behaviour-preserving extraction of the app's dsh client: one managed
//! `dsh web` singleton, the launch-token/cookie exchange, the WebSocket pump
//! and the fold that turns dsh's durable session events into the same
//! message/part shapes the transcript already renders. The typed stream is the
//! app-level vocabulary; the legacy `"dsh"` net key never leaves this module.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_channel::{Receiver, Sender, unbounded};
use serde_json::{Value, json};

use super::interface::Harness;
use super::opencode::translate_event;
use super::shared::{PendingApprovals, filediffs, form_answers, partition_commands};
use super::types::*;
use crate::dsh::{
    DshHandle, agent_presets, commands as dsh_commands, create_session, execute_command,
    list_sessions as dsh_list_sessions, model_catalog, send_message as dsh_send_message,
};
use crate::oc::{
    DSH_SERVER_KEY, ServerProcess, StreamStatus, UiEvent, child_exited, find_free_port,
};
use crate::settings::HarnessKind;

const HEALTH_POLL: Duration = Duration::from_secs(10);

/// The managed dsh adapter.
pub struct DshHarness {
    id: HarnessId,
    config: HarnessConfig,
    runtime: tokio::runtime::Runtime,
    process: Option<ServerProcess>,
    handle: Option<DshHandle>,
    health: Arc<Mutex<HarnessHealth>>,
    tx: Sender<HarnessEvent>,
    rx: Receiver<HarnessEvent>,
    stop: Option<Sender<()>>,
    pump: Option<tokio::task::JoinHandle<()>>,
    forward: Option<tokio::task::JoinHandle<()>>,
    health_task: Option<tokio::task::JoinHandle<()>>,
    pending: Arc<PendingApprovals>,
}

impl DshHarness {
    pub fn new(config: HarnessConfig) -> Self {
        let (tx, rx) = unbounded();
        Self {
            id: HarnessId::new("dsh"),
            config,
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime"),
            process: None,
            handle: None,
            health: Arc::new(Mutex::new(HarnessHealth {
                state: HealthState::Starting,
                detail: None,
            })),
            tx,
            rx,
            stop: None,
            pump: None,
            forward: None,
            health_task: None,
            pending: PendingApprovals::new(),
        }
    }

    fn handle(&self) -> Result<DshHandle, HarnessError> {
        self.handle
            .clone()
            .ok_or_else(|| HarnessError::Unavailable("dsh is not running".into()))
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

    fn summary(&self, info: crate::oc::SessionInfo, directory: Option<PathBuf>) -> SessionSummary {
        SessionSummary {
            id: SessionId::new(info.id),
            title: info.title,
            parent: info.parent_id.map(SessionId::new),
            directory,
            updated_ms: info.updated_ms,
        }
    }

    fn spawn_forward(&mut self, ui_rx: Receiver<UiEvent>) {
        let tx = self.tx.clone();
        let health = Arc::clone(&self.health);
        let pending = Arc::clone(&self.pending);
        let task = self.runtime.spawn(async move {
            while let Ok(event) = ui_rx.recv().await {
                match event {
                    UiEvent::Event { event, .. } => {
                        if let Some(typed) = translate_event(&event) {
                            pending.observe(&typed);
                            let _ = tx.try_send(typed);
                        }
                    }
                    UiEvent::StreamStatus { status, .. } => match status {
                        StreamStatus::Connected => {
                            *health.lock().expect("health") = HarnessHealth::ready();
                            let _ = tx.try_send(HarnessEvent::Connected);
                        }
                        StreamStatus::Reconnecting { attempt } => {
                            *health.lock().expect("health") =
                                HarnessHealth::unavailable("dsh is reconnecting");
                            let _ = tx.try_send(HarnessEvent::Disconnected {
                                reason: Some(format!("reconnecting (attempt {attempt})")),
                            });
                        }
                        StreamStatus::Failed => {
                            *health.lock().expect("health") =
                                HarnessHealth::unavailable("dsh event stream is down");
                            let _ = tx.try_send(HarnessEvent::Disconnected {
                                reason: Some("event stream is down".into()),
                            });
                        }
                    },
                    UiEvent::HealthChanged { healthy, .. } => {
                        let state = match healthy {
                            Some(true) => HarnessHealth::ready(),
                            _ => HarnessHealth::unavailable("dsh health check failed"),
                        };
                        *health.lock().expect("health") = state.clone();
                        let _ = tx.try_send(HarnessEvent::Health(state));
                    }
                    UiEvent::ServerCrashed { .. } => {
                        *health.lock().expect("health") =
                            HarnessHealth::unavailable("dsh web exited");
                        let _ = tx.try_send(HarnessEvent::Disconnected {
                            reason: Some("dsh web exited".into()),
                        });
                    }
                    _ => {}
                }
            }
        });
        self.forward = Some(task);
    }

    fn spawn_health(
        &mut self,
        handle: DshHandle,
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
                    *health.lock().expect("health") = HarnessHealth::unavailable("dsh web exited");
                    let _ = tx.try_send(HarnessEvent::Disconnected {
                        reason: Some("dsh web exited".into()),
                    });
                    return;
                }
                let healthy = tokio::select! {
                    _ = stop_rx.recv() => return,
                    healthy = handle.alive() => healthy,
                };
                let state = if healthy {
                    HarnessHealth::ready()
                } else {
                    HarnessHealth::unavailable("dsh health check failed")
                };
                *health.lock().expect("health") = state.clone();
                let _ = tx.try_send(HarnessEvent::Health(state));
            }
        });
        self.health_task = Some(task);
    }

    fn sessions_raw(&self, handle: &DshHandle) -> Result<Vec<SessionSummary>, HarnessError> {
        let sessions = self.block_on(dsh_list_sessions(handle, None))?;
        Ok(sessions
            .into_iter()
            .map(|(info, directory)| SessionSummary {
                id: SessionId::new(info.id),
                title: info.title,
                parent: info.parent_id.map(SessionId::new),
                directory: directory.map(PathBuf::from),
                updated_ms: info.updated_ms,
            })
            .collect())
    }
}

impl Harness for DshHarness {
    fn id(&self) -> HarnessId {
        self.id.clone()
    }

    fn health(&self) -> HarnessHealth {
        self.health.lock().expect("health lock").clone()
    }

    fn start(&mut self) -> Result<(), HarnessError> {
        if self.handle.is_some() {
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
            HarnessKind::Dsh,
            &self.config.executable,
            &folder,
            port,
            |line| tracing::debug!("dsh: {line}"),
        ));
        let process = match result {
            Ok(process) => process,
            Err(err) => {
                let detail = format!("{err:#}");
                self.set_health(HarnessHealth::unavailable(detail.clone()));
                return Err(HarnessError::Failed(detail));
            }
        };
        let token = process
            .token
            .clone()
            .ok_or_else(|| HarnessError::Failed("dsh web started without a launch token".into()))?;
        let exit_probe = process.exit_probe();
        let (frame_tx, frame_rx) = async_channel::bounded(64);
        let handle = DshHandle::new(&process.base_url, &token, frame_tx);
        let (ui_tx, ui_rx) = unbounded();
        let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);
        self.process = Some(process);
        self.handle = Some(handle.clone());
        self.stop = Some(stop_tx);
        self.set_health(HarnessHealth::ready());
        self.pump = Some(self.runtime.spawn(crate::dsh::run_pump(
            handle.clone(),
            DSH_SERVER_KEY.to_string(),
            ui_tx,
            stop_rx.clone(),
            frame_rx,
        )));
        self.spawn_forward(ui_rx);
        self.spawn_health(handle, stop_rx, exit_probe);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), HarnessError> {
        if self.stop.is_none() && self.process.is_none() && self.handle.is_none() {
            return Ok(());
        }
        if let Some(stop) = self.stop.take() {
            stop.close();
        }
        if let Some(task) = self.pump.take() {
            task.abort();
        }
        if let Some(task) = self.forward.take() {
            task.abort();
        }
        if let Some(task) = self.health_task.take() {
            task.abort();
        }
        if let Some(mut process) = self.process.take() {
            process.stop();
        }
        self.handle = None;
        self.set_health(HarnessHealth::unavailable("stopped"));
        let _ = self.tx.try_send(HarnessEvent::Disconnected {
            reason: Some("stopped".into()),
        });
        Ok(())
    }

    fn providers(&mut self) -> Result<Vec<Provider>, HarnessError> {
        let handle = self.handle()?;
        let value = self.block_on(handle.rpc("session/modelCatalog", json!({})))?;
        let mut providers = BTreeMap::new();
        for group in value
            .get("groups")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = group.get("id").and_then(Value::as_str) else {
                continue;
            };
            let name = group.get("name").and_then(Value::as_str).unwrap_or(id);
            providers.insert(id.to_string(), name.to_string());
        }
        Ok(providers
            .into_iter()
            .map(|(id, name)| Provider { id, name })
            .collect())
    }

    fn models(&mut self) -> Result<Vec<Model>, HarnessError> {
        let handle = self.handle()?;
        let models = self.block_on(model_catalog(&handle))?;
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
        let handle = self.handle()?;
        let agents = self.block_on(agent_presets(&handle))?;
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
        let handle = self.handle()?;
        let session = handle.any_session();
        let commands = self.block_on(dsh_commands(&handle, session.as_deref()))?;
        Ok(partition_commands(commands).0)
    }

    fn skills(&mut self) -> Result<Vec<Skill>, HarnessError> {
        let handle = self.handle()?;
        let session = handle.any_session();
        let commands = self.block_on(dsh_commands(&handle, session.as_deref()))?;
        Ok(partition_commands(commands).1)
    }

    fn sessions(&mut self) -> Result<Vec<SessionSummary>, HarnessError> {
        let handle = self.handle()?;
        self.sessions_raw(&handle)
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
                "create_session with a parent on dsh",
            ));
        }
        let handle = self.handle()?;
        let directory = spec.directory.or_else(|| self.config.directory.clone());
        let folder = directory
            .as_ref()
            .map(|directory| directory.to_string_lossy().into_owned());
        let title = spec.title.unwrap_or_else(|| "New chat".into());
        let info = self.block_on(create_session(&handle, folder.as_deref(), &title))?;
        Ok(self.summary(info, directory))
    }

    fn fork_session(
        &mut self,
        _session: &SessionId,
        _before: Option<&str>,
    ) -> Result<SessionSummary, HarnessError> {
        // the app's dsh fork is a UI flow (create a session for a transcript
        // seed); the dsh API has no server-side fork
        Err(HarnessError::Unsupported("fork_session"))
    }

    fn rename_session(&mut self, session: &SessionId, title: &str) -> Result<(), HarnessError> {
        let handle = self.handle()?;
        self.block_on(handle.rpc(
            "session/rename",
            json!({ "request": { "sessionId": session.as_str(), "title": title } }),
        ))
        .map(|_| ())
    }

    fn delete_session(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        let handle = self.handle()?;
        let reply = self.block_on(handle.rpc(
            "workspace/archiveSession",
            json!({ "request": { "sessionId": session.as_str() } }),
        ))?;
        let ids: Vec<String> = reply
            .get("archivedSessionIds")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(|id| id.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_else(|| vec![session.as_str().to_string()]);
        handle.note_archived(&ids);
        Ok(())
    }

    fn send_prompt(&mut self, session: &SessionId, prompt: Prompt) -> Result<(), HarnessError> {
        if prompt.background {
            return Err(HarnessError::Unsupported("background_prompt"));
        }
        let handle = self.handle()?;
        let session_id = session.as_str().to_string();
        let error_session = session.clone();
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            if let Err(err) = dsh_send_message(
                &handle,
                &session_id,
                &prompt.text,
                prompt.choice.model.as_deref(),
                prompt.choice.variant.as_deref(),
                prompt.choice.agent.as_deref(),
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
        let handle = self.handle()?;
        self.block_on(handle.rpc(
            "session/cancel",
            json!({ "request": { "sessionId": session.as_str() } }),
        ))
        .map(|_| ())
    }

    fn compact(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        let handle = self.handle()?;
        let text = self.block_on(execute_command(&handle, session.as_str(), "/compact"))?;
        let _ = self.tx.try_send(HarnessEvent::CommandOutput {
            session: session.clone(),
            result: Ok(text),
        });
        Ok(())
    }

    fn run_command(
        &mut self,
        session: &SessionId,
        command: CommandRun,
    ) -> Result<(), HarnessError> {
        let handle = self.handle()?;
        let line = if command.text.is_empty() {
            format!("/{}", command.name)
        } else {
            format!("/{} {}", command.name, command.text)
        };
        let session_id = session.as_str().to_string();
        let error_session = session.clone();
        let tx = self.tx.clone();
        self.runtime.spawn(async move {
            let result = match execute_command(&handle, &session_id, &line).await {
                Ok(text) => Ok(text),
                Err(err) => Err(format!("{err:#}")),
            };
            let _ = tx.try_send(HarnessEvent::CommandOutput {
                session: error_session,
                result,
            });
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
        _session: &SessionId,
        request: &str,
        decision: PermissionDecision,
    ) -> Result<(), HarnessError> {
        let response = match decision {
            PermissionDecision::Once => "once",
            PermissionDecision::Always => "always",
            PermissionDecision::Reject => "reject",
        };
        let handle = self.handle()?;
        self.block_on(handle.answer_approval(request, response))?;
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
        let handle = self.handle()?;
        let result = if answers.iter().all(Vec::is_empty) {
            self.block_on(handle.reject_question(&pending.id))
        } else {
            self.block_on(handle.answer_question(&pending.id, &answers))
        };
        result?;
        self.pending.remove_form(form);
        Ok(())
    }

    fn diff(&mut self, session: &SessionId) -> Result<Vec<FileDiff>, HarnessError> {
        let handle = self.handle()?;
        let messages = self.block_on(handle.messages(session.as_str()))?;
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
        Err(HarnessError::Unsupported("mcp_servers"))
    }

    fn add_mcp(&mut self, _config: McpConfig) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("add_mcp"))
    }

    fn connect_mcp(&mut self, _name: &str) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("connect_mcp"))
    }

    fn disconnect_mcp(&mut self, _name: &str) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("disconnect_mcp"))
    }

    fn remove_mcp(&mut self, _name: &str) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("remove_mcp"))
    }

    fn events(&self) -> Receiver<HarnessEvent> {
        self.rx.clone()
    }
}

impl Drop for DshHarness {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::{DshHarness, filediffs, translate_event};
    use crate::harness::{
        FormFieldKind, Harness, HarnessConfig, HarnessError, HarnessEvent, HealthState,
        MessageRole, PartPayload, PermissionDecision, SessionId,
    };
    use crate::oc::StoredMessage;
    use serde_json::json;

    #[test]
    fn a_missing_executable_reports_a_typed_start_failure() {
        let mut harness = DshHarness::new(HarnessConfig::new("warp-agent-missing-dsh"));
        assert!(matches!(
            harness.start().unwrap_err(),
            HarnessError::Failed(_)
        ));
        assert_eq!(harness.health().state, HealthState::Unavailable);
        harness.stop().expect("a failed start still stops");
    }

    #[test]
    fn dsh_approval_and_question_waterfalls_translate() {
        let permission = translate_event(&json!({
            "type": "permission.v2.asked",
            "properties": {
                "id": "evt-1",
                "sessionID": "ses-1",
                "action": "bash",
                "resources": ["run ls"],
            },
        }))
        .expect("an approval request translates");
        assert!(matches!(
            permission,
            HarnessEvent::PermissionAsked { request }
                if request.id == "evt-1"
                    && request.session == SessionId::new("ses-1")
                    && request.resources == ["run ls"]
        ));

        let question = translate_event(&json!({
            "type": "question.asked",
            "properties": {
                "id": "evt-2",
                "sessionID": "ses-1",
                "questions": [{
                    "question": "Pick",
                    "header": "Pick",
                    "multiSelect": true,
                    "options": [{ "label": "a", "description": "" }],
                }],
            },
        }))
        .expect("a question translates");
        assert!(matches!(
            question,
            HarnessEvent::QuestionAsked { request }
                if request.id == "evt-2" && request.fields.len() == 1
                    && request.fields[0].kind == FormFieldKind::Multiselect
        ));

        let replied = translate_event(&json!({
            "type": "permission.v2.replied",
            "properties": { "id": "evt-1" },
        }))
        .expect("a permission reply translates");
        assert!(matches!(
            replied,
            HarnessEvent::PermissionReplied {
                decision: PermissionDecision::Once,
                ..
            }
        ));
    }

    #[test]
    fn dsh_error_text_survives_the_translation() {
        let error = translate_event(&json!({
            "type": "session.error",
            "properties": {
                "sessionID": "ses-1",
                "error": { "name": "provider", "message": "the model refused the turn" },
            },
        }))
        .expect("an error translates");
        assert!(matches!(
            error,
            HarnessEvent::Error { message, .. } if message == "the model refused the turn"
        ));
    }

    #[test]
    fn dsh_hunks_become_a_filediff() {
        let messages = vec![StoredMessage {
            id: "msg-1".into(),
            role: "assistant".into(),
            created_ms: 1,
            info: json!({}),
            parts: vec![json!({
                "type": "tool",
                "state": {
                    "metadata": {
                        "diffs": [{
                            "path": "src/main.rs",
                            "oldText": "old line",
                            "newText": "new line",
                        }],
                    },
                },
            })],
        }];
        let diffs = filediffs(messages);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].path, "src/main.rs");
        assert!(diffs[0].patch.contains("-old line"));
        assert!(diffs[0].patch.contains("+new line"));
    }

    #[test]
    fn dsh_message_and_part_frames_translate() {
        let message = translate_event(&json!({
            "type": "message.updated",
            "properties": {
                "sessionID": "ses-1",
                "info": {
                    "id": "msg-1",
                    "sessionID": "ses-1",
                    "role": "assistant",
                    "time": { "created": 1 },
                },
            },
        }))
        .expect("a message translates");
        assert!(matches!(
            message,
            HarnessEvent::MessageSnapshot { message } if message.role == MessageRole::Assistant
        ));

        let part = translate_event(&json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": "ses-1",
                "part": {
                    "id": "part-1",
                    "sessionID": "ses-1",
                    "messageID": "msg-1",
                    "type": "text",
                    "text": "hello from dsh",
                },
            },
        }))
        .expect("a part translates");
        assert!(matches!(
            part,
            HarnessEvent::PartSnapshot { part }
                if part.payload == PartPayload::Text("hello from dsh".into())
        ));
    }
}
