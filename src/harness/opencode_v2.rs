//! opencode v2 through the harness interface.
//!
//! v2 is an attached harness: the user's shared background service keeps
//! running, and this adapter discovers its URL and registration password,
//! starts it when it is stopped, attaches over the authenticated `/api`
//! surface and exposes the catalogs and sessions. Chat streaming, approvals,
//! forms and the v2-only surfaces land in the following tickets.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_channel::{Receiver, Sender, unbounded};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use super::interface::Harness;
use super::types::*;

const INFO_TIMEOUT: Duration = Duration::from_secs(10);
const START_POLLS: usize = 50;
const START_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// The attached opencode v2 adapter.
pub struct OpenCodeV2Harness {
    id: HarnessId,
    config: HarnessConfig,
    runtime: tokio::runtime::Runtime,
    http: reqwest::Client,
    base_url: Mutex<Option<String>>,
    password: Mutex<Option<String>>,
    health: Mutex<HarnessHealth>,
    tx: Sender<HarnessEvent>,
    rx: Receiver<HarnessEvent>,
    pump: Option<tokio::task::JoinHandle<()>>,
    pump_stop: Option<Arc<AtomicBool>>,
}

impl OpenCodeV2Harness {
    pub fn new(config: HarnessConfig) -> Self {
        let (tx, rx) = unbounded();
        Self {
            id: HarnessId::new("opencode-v2"),
            config,
            runtime: tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime"),
            http: reqwest::Client::builder()
                .timeout(INFO_TIMEOUT)
                .build()
                .unwrap_or_default(),
            base_url: Mutex::new(None),
            password: Mutex::new(None),
            health: Mutex::new(HarnessHealth {
                state: HealthState::Starting,
                detail: None,
            }),
            tx,
            rx,
            pump: None,
            pump_stop: None,
        }
    }

    fn credentials(&self) -> Option<(String, String)> {
        let base = self.base_url.lock().expect("v2 base").clone()?;
        let password = self.password.lock().expect("v2 password").clone()?;
        Some((base, password))
    }

    fn set_credentials(&self, base_url: String, password: String) {
        *self.base_url.lock().expect("v2 base") = Some(base_url);
        *self.password.lock().expect("v2 password") = Some(password);
    }

    fn detach(&self) {
        *self.base_url.lock().expect("v2 base") = None;
        *self.password.lock().expect("v2 password") = None;
    }

    fn set_health(&self, state: HarnessHealth) {
        *self.health.lock().expect("v2 health") = state;
    }

    fn fail(&self, detail: impl Into<String>) -> HarnessError {
        let detail = detail.into();
        self.set_health(HarnessHealth::unavailable(detail.clone()));
        HarnessError::Unavailable(detail)
    }

    fn start_event_pump(&mut self) {
        if self.pump.is_some() {
            return;
        }
        let Some((base, password)) = self.credentials() else {
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        // the SSE stream is long-lived: the shared client's 10 s total
        // timeout killed it every reconnect and lost the turn's events
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let tx = self.tx.clone();
        let handle = self.runtime.spawn(async move {
            event_pump(http, base, password, tx, thread_stop).await;
        });
        self.pump = Some(handle);
        self.pump_stop = Some(stop);
    }

    fn block_on<T>(
        &self,
        future: impl std::future::Future<Output = Result<T, HarnessError>>,
    ) -> Result<T, HarnessError> {
        self.runtime.block_on(future)
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        location: bool,
        body: Option<Value>,
    ) -> Result<Value, HarnessError> {
        let mut retried = false;
        loop {
            let Some((base, password)) = self.credentials() else {
                return Err(HarnessError::Unavailable(
                    "opencode v2 is not attached".into(),
                ));
            };
            let mut url = reqwest::Url::parse(&format!("{}{}", base.trim_end_matches('/'), path))
                .map_err(|err| HarnessError::Failed(format!("invalid v2 URL: {err}")))?;
            if location && let Some(directory) = self.config.directory.as_ref() {
                url.query_pairs_mut()
                    .append_pair("location[directory]", &directory.to_string_lossy());
            }
            let mut request = self
                .http
                .request(method.clone(), url)
                .basic_auth("opencode", Some(password));
            if let Some(body) = body.clone() {
                request = request.json(&body);
            }
            let response = request.send().await.map_err(|err| {
                HarnessError::Failed(format!("opencode v2 request failed: {err}"))
            })?;
            let status = response.status();
            if status == reqwest::StatusCode::UNAUTHORIZED {
                if !retried && let Ok((base, password)) = read_registration() {
                    self.set_credentials(base, password);
                    retried = true;
                    continue;
                }
                return Err(HarnessError::Unavailable(
                    "opencode v2 rejected the registration password".into(),
                ));
            }
            if !status.is_success() {
                let detail = response.text().await.unwrap_or_default();
                return Err(HarnessError::Failed(format!(
                    "opencode v2 {status}: {}",
                    detail.trim()
                )));
            }
            if status == reqwest::StatusCode::NO_CONTENT || response.content_length() == Some(0) {
                return Ok(json!({}));
            }
            return response.json().await.map_err(|err| {
                HarnessError::Failed(format!("opencode v2 returned bad JSON: {err}"))
            });
        }
    }

    fn get(&self, path: &str, location: bool) -> Result<Value, HarnessError> {
        self.block_on(self.request(reqwest::Method::GET, path, location, None))
    }

    fn providers_catalog(&self) -> Result<Vec<Provider>, HarnessError> {
        let value = self.get("/api/provider", true)?;
        let mut providers = Vec::new();
        for item in data_array(&value) {
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            providers.push(Provider {
                id: id.to_string(),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
            });
        }
        Ok(providers)
    }

    fn session_list_path(&self) -> String {
        match self.config.directory.as_ref() {
            Some(directory) => {
                let directory = directory.to_string_lossy();
                path_with_query(
                    "/api/session",
                    &[("limit", "100"), ("directory", directory.as_ref())],
                )
            }
            None => "/api/session?limit=100".into(),
        }
    }

    fn session_from_value(value: &Value) -> Result<SessionSummary, HarnessError> {
        let data = value.get("data").unwrap_or(value);
        session_summary(data)
            .ok_or_else(|| HarnessError::Failed("opencode v2 returned no session".into()))
    }
}

impl Harness for OpenCodeV2Harness {
    fn id(&self) -> HarnessId {
        self.id.clone()
    }

    fn health(&self) -> HarnessHealth {
        self.health.lock().expect("v2 health").clone()
    }

    fn start(&mut self) -> Result<(), HarnessError> {
        if self.credentials().is_some() {
            return Ok(());
        }
        let (base_url, password) = match discover(&self.config.executable) {
            Ok(pair) => pair,
            Err(err) => {
                self.set_health(HarnessHealth::unavailable(format!("{err:?}")));
                return Err(err);
            }
        };
        self.set_credentials(base_url, password);
        match self.block_on(self.request(reqwest::Method::GET, "/api/info", false, None)) {
            Ok(info) if info.get("version").is_some() => {
                self.set_health(HarnessHealth::ready());
                self.start_event_pump();
                Ok(())
            }
            Ok(_) => {
                self.detach();
                Err(self.fail("opencode v2 did not return /api/info"))
            }
            Err(err) => {
                self.detach();
                Err(self.fail(format!("{err:?}")))
            }
        }
    }

    fn stop(&mut self) -> Result<(), HarnessError> {
        if let Some(stop) = self.pump_stop.take() {
            stop.store(true, Ordering::SeqCst);
        }
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
        self.detach();
        self.set_health(HarnessHealth::unavailable("disconnected"));
        let _ = self.tx.try_send(HarnessEvent::Disconnected {
            reason: Some("disconnected".into()),
        });
        Ok(())
    }

    fn providers(&mut self) -> Result<Vec<Provider>, HarnessError> {
        self.providers_catalog()
    }

    fn models(&mut self) -> Result<Vec<Model>, HarnessError> {
        let value = self.get("/api/model", true)?;
        let mut models = Vec::new();
        for item in data_array(&value) {
            if item.get("enabled").and_then(Value::as_bool) == Some(false) {
                continue;
            }
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            let provider = item
                .get("providerID")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            models.push(Model {
                key: format!("{provider}/{id}"),
                provider,
                variants: variant_names(item.get("variants")),
                context_limit: item
                    .pointer("/limit/context")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
            });
        }
        Ok(models)
    }

    fn agents(&mut self) -> Result<Vec<Agent>, HarnessError> {
        let value = self.get("/api/agent", true)?;
        let mut agents = Vec::new();
        for item in data_array(&value) {
            let Some(id) = item
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| item.get("name").and_then(Value::as_str))
            else {
                continue;
            };
            agents.push(Agent {
                name: id.to_string(),
                mode: item
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("all")
                    .to_string(),
                hidden: item.get("hidden").and_then(Value::as_bool).unwrap_or(false),
            });
        }
        Ok(agents)
    }

    fn commands(&mut self, _session: Option<&SessionId>) -> Result<Vec<Command>, HarnessError> {
        let value = self.get("/api/command", true)?;
        let mut commands = Vec::new();
        for item in data_array(&value) {
            let Some(name) = item.get("name").and_then(Value::as_str) else {
                continue;
            };
            commands.push(Command {
                name: name.to_string(),
                description: item
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
        Ok(commands)
    }

    fn skills(&mut self, _session: Option<&SessionId>) -> Result<Vec<Skill>, HarnessError> {
        let value = self.get("/api/skill", true)?;
        let mut skills = Vec::new();
        for item in data_array(&value) {
            let Some(id) = item
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| item.get("name").and_then(Value::as_str))
            else {
                continue;
            };
            skills.push(Skill {
                id: id.to_string(),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(id)
                    .to_string(),
                description: item
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            });
        }
        Ok(skills)
    }

    fn sessions(&mut self) -> Result<Vec<SessionSummary>, HarnessError> {
        let value = self.get(&self.session_list_path(), false)?;
        Ok(data_array(&value).filter_map(session_summary).collect())
    }

    fn session(&mut self, session: &SessionId) -> Result<SessionSummary, HarnessError> {
        let value = self.get(&format!("/api/session/{}", session.as_str()), false)?;
        Self::session_from_value(&value)
    }

    fn transcript(&mut self, _session: &SessionId) -> Result<Vec<HistoryMessage>, HarnessError> {
        // the v2 service has no message-list endpoint; attached chats load
        // their history through the store's own session restore only
        Err(HarnessError::Unsupported("transcript"))
    }

    fn create_session(&mut self, spec: NewSession) -> Result<SessionSummary, HarnessError> {
        let mut body = json!({});
        if let Some(title) = spec.title {
            body["title"] = json!(title);
        }
        if let Some(parent) = spec.parent {
            body["parentID"] = json!(parent.as_str());
        }
        if let Some(agent) = spec.choice.agent {
            body["agent"] = json!(agent);
        }
        if let Some(model) = spec.choice.model.as_deref() {
            let (provider, id) = match model.split_once('/') {
                Some((provider, id)) => (provider, id),
                None => (model, ""),
            };
            let mut reference = json!({ "providerID": provider, "id": id });
            if let Some(variant) = spec.choice.variant {
                reference["variant"] = json!(variant);
            }
            body["model"] = reference;
        }
        let directory = spec.directory.or_else(|| self.config.directory.clone());
        if let Some(directory) = directory {
            body["location"] = json!({ "directory": directory.to_string_lossy() });
        }
        let value =
            self.block_on(self.request(reqwest::Method::POST, "/api/session", false, Some(body)))?;
        Self::session_from_value(&value)
    }

    fn fork_session(
        &mut self,
        session: &SessionId,
        before: Option<&str>,
    ) -> Result<SessionSummary, HarnessError> {
        let mut body = json!({});
        if let Some(before) = before {
            body["before"] = json!(before);
        }
        let value = self.block_on(self.request(
            reqwest::Method::POST,
            &format!("/api/session/{}/fork", session.as_str()),
            false,
            Some(body),
        ))?;
        Self::session_from_value(&value)
    }

    fn rename_session(&mut self, session: &SessionId, title: &str) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::PATCH,
            &format!("/api/session/{}", session.as_str()),
            false,
            Some(json!({ "title": title })),
        ))
        .map(|_| ())
    }

    fn delete_session(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::DELETE,
            &format!("/api/session/{}", session.as_str()),
            false,
            None,
        ))
        .map(|_| ())
    }

    fn send_prompt(&mut self, session: &SessionId, prompt: Prompt) -> Result<(), HarnessError> {
        let mut body = json!({ "text": prompt.text });
        if let Some(agent) = prompt.choice.agent
            && self
                .agents()
                .map(|agents| agents.iter().any(|known| known.name == agent))
                .unwrap_or(false)
        {
            body["agents"] = json!([agent]);
        }
        self.block_on(self.request(
            reqwest::Method::POST,
            &format!("/api/session/{}/prompt", session.as_str()),
            false,
            Some(body),
        ))
        .map(|_| ())
    }

    fn interrupt(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::POST,
            &format!("/api/session/{}/interrupt", session.as_str()),
            false,
            None,
        ))
        .map(|_| ())
    }

    fn compact(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::POST,
            &format!("/api/session/{}/compact", session.as_str()),
            false,
            Some(json!({})),
        ))
        .map(|_| ())
    }

    fn run_command(
        &mut self,
        session: &SessionId,
        command: CommandRun,
    ) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::POST,
            &format!("/api/session/{}/command", session.as_str()),
            false,
            Some(json!({ "name": command.name, "text": command.text })),
        ))
        .map(|_| ())
    }

    fn activate_skill(&mut self, session: &SessionId, skill: &str) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::POST,
            &format!("/api/experimental/session/{}/skill", session.as_str()),
            false,
            Some(json!({ "id": skill })),
        ))
        .map(|_| ())
    }

    fn permissions(&mut self, session: &SessionId) -> Result<Vec<PermissionRequest>, HarnessError> {
        let value = self.get(
            &format!("/api/session/{}/permission", session.as_str()),
            false,
        )?;
        Ok(list_values(&value)
            .filter_map(permission_from_value)
            .collect())
    }

    fn reply_permission(
        &mut self,
        session: &SessionId,
        request: &str,
        decision: PermissionDecision,
    ) -> Result<(), HarnessError> {
        let reply = match decision {
            PermissionDecision::Once => "once",
            PermissionDecision::Always => "always",
            PermissionDecision::Reject => "reject",
        };
        self.block_on(self.request(
            reqwest::Method::POST,
            &format!(
                "/api/session/{}/permission/{}/reply",
                session.as_str(),
                request
            ),
            false,
            Some(json!({ "reply": reply })),
        ))
        .map(|_| ())
    }

    fn forms(&mut self, session: &SessionId) -> Result<Vec<FormRequest>, HarnessError> {
        let value = self.get(&format!("/api/session/{}/form", session.as_str()), false)?;
        Ok(list_values(&value).filter_map(form_from_value).collect())
    }

    fn reply_form(
        &mut self,
        session: &SessionId,
        form: &str,
        answer: FormAnswer,
    ) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::POST,
            &format!("/api/session/{}/form/{}/reply", session.as_str(), form),
            false,
            Some(json!({ "answer": answer.values })),
        ))
        .map(|_| ())
    }

    fn diff(&mut self, session: &SessionId) -> Result<Vec<FileDiff>, HarnessError> {
        let value = self.get(&format!("/api/session/{}/diff", session.as_str()), false)?;
        Ok(list_values(&value).filter_map(file_diff).collect())
    }

    fn find_files(&mut self, _query: &str) -> Result<Vec<String>, HarnessError> {
        Err(HarnessError::Unsupported("find_files"))
    }

    fn forget_session(&mut self, _session: &SessionId) -> Result<(), HarnessError> {
        // the attached adapter keeps no folded per-session state
        Ok(())
    }

    fn inbox(&mut self, session: &SessionId) -> Result<Vec<InboxItem>, HarnessError> {
        let value = self.get(&format!("/api/session/{}/inbox", session.as_str()), false)?;
        Ok(list_values(&value).filter_map(inbox_item).collect())
    }

    fn update_inbox(
        &mut self,
        session: &SessionId,
        item: &str,
        delivery: InboxDelivery,
    ) -> Result<(), HarnessError> {
        let delivery = match delivery {
            InboxDelivery::Queue => "queue",
            InboxDelivery::Steer => "steer",
        };
        self.block_on(self.request(
            reqwest::Method::PATCH,
            &format!("/api/session/{}/inbox/{}", session.as_str(), item),
            false,
            Some(json!({ "delivery": delivery })),
        ))
        .map(|_| ())
    }

    fn cancel_inbox(&mut self, session: &SessionId, item: &str) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::DELETE,
            &format!("/api/session/{}/inbox/{}", session.as_str(), item),
            false,
            None,
        ))
        .map(|_| ())
    }

    fn stage_revert(&mut self, session: &SessionId, before: &str) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::POST,
            &format!("/api/session/{}/revert/stage", session.as_str()),
            false,
            Some(json!({ "messageID": before })),
        ))
        .map(|_| ())
    }

    fn commit_revert(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::POST,
            &format!("/api/session/{}/revert/commit", session.as_str()),
            false,
            None,
        ))
        .map(|_| ())
    }

    fn clear_revert(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::DELETE,
            &format!("/api/session/{}/revert", session.as_str()),
            false,
            None,
        ))
        .map(|_| ())
    }

    fn run_shell(&mut self, session: &SessionId, command: &str) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::POST,
            &format!("/api/session/{}/shell", session.as_str()),
            false,
            Some(json!({ "command": command })),
        ))
        .map(|_| ())
    }

    fn background_session(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::POST,
            &format!("/api/session/{}/background", session.as_str()),
            false,
            None,
        ))
        .map(|_| ())
    }

    fn mcp_servers(&mut self) -> Result<Vec<McpServer>, HarnessError> {
        let value = self.get("/api/mcp", true)?;
        Ok(data_array(&value).filter_map(mcp_server).collect())
    }

    fn add_mcp(&mut self, config: McpConfig) -> Result<(), HarnessError> {
        let mut body = if let Some(command) = config.command.as_deref() {
            json!({
                "type": "local",
                "command": command.split_whitespace().collect::<Vec<_>>(),
            })
        } else if let Some(url) = config.url.as_deref() {
            json!({ "type": "remote", "url": url })
        } else {
            return Err(HarnessError::Failed(
                "an MCP server needs a command or a url".into(),
            ));
        };
        if !config.env.is_empty() {
            body["environment"] = json!(config.env);
        }
        self.block_on(self.request(
            reqwest::Method::PUT,
            &format!("/api/experimental/mcp/{}", config.name),
            false,
            Some(json!({ "config": body })),
        ))
        .map(|_| ())
    }

    fn connect_mcp(&mut self, name: &str) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::POST,
            &format!("/api/experimental/mcp/{name}/connect"),
            false,
            None,
        ))
        .map(|_| ())
    }

    fn disconnect_mcp(&mut self, name: &str) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::POST,
            &format!("/api/experimental/mcp/{name}/disconnect"),
            false,
            None,
        ))
        .map(|_| ())
    }

    fn remove_mcp(&mut self, name: &str) -> Result<(), HarnessError> {
        self.block_on(self.request(
            reqwest::Method::DELETE,
            &format!("/api/experimental/mcp/{name}"),
            false,
            None,
        ))
        .map(|_| ())
    }

    fn events(&self) -> Receiver<HarnessEvent> {
        self.rx.clone()
    }
}

impl Drop for OpenCodeV2Harness {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// ---- discovery -----------------------------------------------------------

enum ServiceStatus {
    Running(String),
    Stopped,
}

fn discover(executable: &str) -> Result<(String, String), HarnessError> {
    match service_status(executable)? {
        ServiceStatus::Running(url) => {
            let (_, password) = read_registration()?;
            Ok((url, password))
        }
        ServiceStatus::Stopped => {
            service_start(executable)?;
            let mut running = None;
            for _ in 0..START_POLLS {
                std::thread::sleep(START_POLL_INTERVAL);
                if let ServiceStatus::Running(url) = service_status(executable)? {
                    running = Some(url);
                    break;
                }
            }
            let Some(url) = running else {
                return Err(HarnessError::Unavailable(
                    "opencode v2 service did not start".into(),
                ));
            };
            let (_, password) = read_registration()?;
            Ok((url, password))
        }
    }
}

fn service_status(executable: &str) -> Result<ServiceStatus, HarnessError> {
    let output = ProcessCommand::new(executable)
        .args(["service", "status"])
        .output()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                HarnessError::Unavailable("opencode CLI not found".into())
            } else {
                HarnessError::Unavailable(format!("opencode service status failed: {err}"))
            }
        })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(HarnessError::Unavailable(format!(
            "opencode service status failed: {}",
            detail.trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let status = stdout.trim();
    if status == "stopped" {
        Ok(ServiceStatus::Stopped)
    } else if status.starts_with("http://") || status.starts_with("https://") {
        Ok(ServiceStatus::Running(status.to_string()))
    } else {
        Err(HarnessError::Unavailable(format!(
            "unexpected opencode service status: {status}"
        )))
    }
}

fn service_start(executable: &str) -> Result<(), HarnessError> {
    let output = ProcessCommand::new(executable)
        .args(["service", "start"])
        .output()
        .map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                HarnessError::Unavailable("opencode CLI not found".into())
            } else {
                HarnessError::Unavailable(format!("opencode service start failed: {err}"))
            }
        })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(HarnessError::Unavailable(format!(
            "opencode service start failed: {}",
            detail.trim()
        )));
    }
    Ok(())
}

fn registration_path() -> PathBuf {
    let root = if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(xdg)
    } else if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        PathBuf::from(local).join("opencode").join("state")
    } else {
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".local")
            .join("state")
    };
    root.join("opencode").join("service.json")
}

#[derive(Deserialize)]
struct Registration {
    url: String,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pid: Option<u64>,
}

fn read_registration() -> Result<(String, String), HarnessError> {
    let path = registration_path();
    let text = std::fs::read_to_string(&path).map_err(|_| {
        HarnessError::Unavailable(format!(
            "opencode v2 registration file is missing: {}",
            path.display()
        ))
    })?;
    let registration: Registration = serde_json::from_str(&text).map_err(|err| {
        HarnessError::Unavailable(format!("opencode v2 registration file is invalid: {err}"))
    })?;
    let password = registration.password.ok_or_else(|| {
        HarnessError::Unavailable("opencode v2 registration file has no password".into())
    })?;
    Ok((registration.url.trim_end_matches('/').to_string(), password))
}

// ---- wire helpers --------------------------------------------------------

fn data_array(value: &Value) -> impl Iterator<Item = &Value> {
    value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn list_values(value: &Value) -> impl Iterator<Item = &Value> {
    value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .into_iter()
        .flatten()
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                item.as_str().map(str::to_string).or_else(|| {
                    item.get("value")
                        .or_else(|| item.get("label"))
                        .or_else(|| item.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
            })
            .collect(),
        Some(Value::String(text)) => vec![text.clone()],
        _ => Vec::new(),
    }
}

fn permission_from_value(value: &Value) -> Option<PermissionRequest> {
    let id = value
        .get("id")
        .or_else(|| value.get("requestID"))
        .and_then(Value::as_str)?;
    let session = value
        .get("sessionID")
        .or_else(|| value.get("sessionId"))
        .and_then(Value::as_str)
        .map(SessionId::new)?;
    Some(PermissionRequest {
        id: id.to_string(),
        session,
        action: value
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("permission")
            .to_string(),
        resources: string_list(value.get("resources")),
        message: value
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string),
        diff: value
            .pointer("/metadata/diff")
            .and_then(Value::as_str)
            .or_else(|| value.get("diff").and_then(Value::as_str))
            .map(str::to_string),
    })
}

fn form_from_value(value: &Value) -> Option<FormRequest> {
    let id = value
        .get("id")
        .or_else(|| value.get("formID"))
        .and_then(Value::as_str)?;
    let session = value
        .get("sessionID")
        .or_else(|| value.get("sessionId"))
        .and_then(Value::as_str)
        .map(SessionId::new)?;
    let fields = value
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| fields.iter().filter_map(form_field).collect())
        .unwrap_or_default();
    Some(FormRequest {
        id: id.to_string(),
        session,
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Question")
            .to_string(),
        fields,
    })
}

fn form_field(value: &Value) -> Option<FormField> {
    let id = value
        .get("id")
        .or_else(|| value.get("fieldID"))
        .and_then(Value::as_str)?;
    let raw = value
        .get("kind")
        .or_else(|| value.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("string")
        .to_ascii_lowercase();
    let kind = match raw.as_str() {
        "number" => FormFieldKind::Number,
        "integer" => FormFieldKind::Integer,
        "boolean" | "bool" => FormFieldKind::Boolean,
        "multiselect" | "multi-select" | "multi_select" => FormFieldKind::Multiselect,
        "external" => FormFieldKind::External,
        _ => FormFieldKind::Text,
    };
    Some(FormField {
        id: id.to_string(),
        title: value
            .get("title")
            .or_else(|| value.get("label"))
            .or_else(|| value.get("name"))
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_string(),
        kind,
        required: value
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        options: string_list(value.get("options")),
    })
}

fn permission_decision(value: &Value) -> Option<PermissionDecision> {
    match value
        .get("reply")
        .or_else(|| value.get("decision"))
        .and_then(Value::as_str)
    {
        Some("once") => Some(PermissionDecision::Once),
        Some("always") => Some(PermissionDecision::Always),
        Some("reject") => Some(PermissionDecision::Reject),
        _ => None,
    }
}

fn file_diff(value: &Value) -> Option<FileDiff> {
    let path = value
        .get("path")
        .or_else(|| value.get("file"))
        .or_else(|| value.get("filename"))
        .and_then(Value::as_str)?;
    let patch = value
        .get("patch")
        .or_else(|| value.get("diff"))
        .or_else(|| value.get("text"))
        .and_then(Value::as_str)?;
    Some(FileDiff {
        path: path.to_string(),
        patch: patch.to_string(),
    })
}

fn inbox_item(value: &Value) -> Option<InboxItem> {
    let id = value
        .get("id")
        .or_else(|| value.get("inboxID"))
        .and_then(Value::as_str)?;
    let session = value
        .get("sessionID")
        .and_then(Value::as_str)
        .map(SessionId::new)?;
    let kind = match value.get("type").and_then(Value::as_str).unwrap_or("user") {
        "synthetic" => InboxKind::Synthetic,
        "compaction" => InboxKind::Compaction,
        "move" => InboxKind::Move,
        _ => InboxKind::User,
    };
    let delivery = match value
        .get("delivery")
        .and_then(Value::as_str)
        .unwrap_or("queue")
    {
        "steer" => InboxDelivery::Steer,
        _ => InboxDelivery::Queue,
    };
    Some(InboxItem {
        id: id.to_string(),
        session,
        kind,
        delivery,
    })
}

fn mcp_server(value: &Value) -> Option<McpServer> {
    let name = value.get("name").and_then(Value::as_str)?;
    let status = value
        .get("status")
        .and_then(|status| {
            status
                .as_str()
                .or_else(|| status.get("status").and_then(Value::as_str))
        })
        .unwrap_or("pending");
    let status = match status {
        "connected" => McpStatus::Connected,
        "disabled" => McpStatus::Disabled,
        "failed" => McpStatus::Failed,
        "needs_auth" => McpStatus::NeedsAuth,
        _ => McpStatus::Pending,
    };
    Some(McpServer {
        name: name.to_string(),
        status,
    })
}

fn variant_names(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .or_else(|| item.get("id").and_then(Value::as_str).map(str::to_string))
                    .or_else(|| item.get("name").and_then(Value::as_str).map(str::to_string))
            })
            .collect(),
        Some(Value::Object(map)) => map.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

fn session_summary(value: &Value) -> Option<SessionSummary> {
    let id = value.get("id").and_then(Value::as_str)?;
    let updated_ms = value
        .pointer("/time/updated")
        .and_then(Value::as_i64)
        .or_else(|| value.get("updatedAt").and_then(Value::as_i64))
        .unwrap_or(0);
    Some(SessionSummary {
        id: SessionId::new(id),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("New chat")
            .to_string(),
        created_ms: value
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .or_else(|| value.get("createdAt").and_then(Value::as_i64))
            .unwrap_or(0),
        parent: value
            .get("parentID")
            .and_then(Value::as_str)
            .or_else(|| value.get("parentId").and_then(Value::as_str))
            .map(SessionId::new),
        directory: value
            .pointer("/location/directory")
            .and_then(Value::as_str)
            .or_else(|| value.get("directory").and_then(Value::as_str))
            .map(PathBuf::from),
        updated_ms,
    })
}

fn path_with_query(path: &str, pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse(&format!("http://v2.local{path}")).expect("v2 path");
    for (key, value) in pairs {
        url.query_pairs_mut().append_pair(key, value);
    }
    let query = url
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    format!("{}{}", url.path(), query)
}

// ---- event stream --------------------------------------------------------

/// The v2 stream has no part ids: text and reasoning deltas are keyed by
/// `(assistantMessageID, ordinal)`, tools by their assistant-scoped tool id.
/// This state turns those deltas back into the full snapshots the app stores.
#[derive(Default)]
struct V2StreamState {
    text: HashMap<(SessionId, String), String>,
    reasoning: HashMap<(SessionId, String), String>,
    tools: HashMap<(SessionId, String, String), ToolSnapshot>,
}

async fn event_pump(
    http: reqwest::Client,
    mut base: String,
    mut password: String,
    tx: Sender<HarnessEvent>,
    stop: Arc<AtomicBool>,
) {
    let mut state = V2StreamState::default();
    while !stop.load(Ordering::SeqCst) {
        let request = http
            .get(format!("{}/api/event", base.trim_end_matches('/')))
            .basic_auth("opencode", Some(password.clone()))
            .header("accept", "text/event-stream");
        match request.send().await {
            Ok(response) if response.status().is_success() => {
                if tx.try_send(HarnessEvent::Connected).is_err() {
                    break;
                }
                let mut stream = response.bytes_stream();
                let mut buffer = String::new();
                while let Some(chunk) = stream.next().await {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(bytes) = chunk else {
                        break;
                    };
                    buffer.push_str(&String::from_utf8_lossy(&bytes).replace("\r\n", "\n"));
                    while let Some(position) = buffer.find("\n\n") {
                        let frame = buffer[..position].to_string();
                        buffer.drain(..position + 2);
                        let Some(value) = sse_data(&frame) else {
                            continue;
                        };
                        if let Some(event) = translate_v2_event(&value, &mut state)
                            && tx.try_send(event).is_err()
                        {
                            return;
                        }
                    }
                }
                let _ = tx.try_send(HarnessEvent::Disconnected {
                    reason: Some("event stream ended".into()),
                });
            }
            Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                if let Ok((new_base, new_password)) = read_registration() {
                    base = new_base;
                    password = new_password;
                }
            }
            Ok(_) | Err(_) => {}
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn sse_data(frame: &str) -> Option<Value> {
    let data = frame
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        None
    } else {
        serde_json::from_str(&data).ok()
    }
}

fn translate_v2_event(value: &Value, state: &mut V2StreamState) -> Option<HarnessEvent> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| value.get("event").and_then(Value::as_str))?;
    let data = value.get("data").unwrap_or(value);
    match kind {
        "server.connected" => Some(HarnessEvent::Connected),
        "session.status" => {
            let session = event_session(data)?;
            let status = data
                .get("status")
                .and_then(|status| status.get("type"))
                .and_then(Value::as_str)?;
            let state = match status {
                "idle" => SessionState::Idle,
                "busy" => SessionState::Busy,
                _ => SessionState::Retry,
            };
            Some(HarnessEvent::SessionStatus { session, state })
        }
        "session.idle" => Some(HarnessEvent::SessionStatus {
            session: event_session(data)?,
            state: SessionState::Idle,
        }),
        // the live v2 service ends a turn with execution.* rather than
        // session.idle; without this the tile stayed "Running" after the
        // answer had landed
        "session.execution.succeeded" | "session.execution.cancelled" => {
            Some(HarnessEvent::SessionStatus {
                session: event_session(data)?,
                state: SessionState::Idle,
            })
        }
        "session.execution.started" => Some(HarnessEvent::SessionStatus {
            session: event_session(data)?,
            state: SessionState::Busy,
        }),
        "session.text.delta" => text_part(state, data, false),
        "session.reasoning.delta" => text_part(state, data, true),
        "session.step.started" => step_part(data, true),
        "session.step.ended" => step_part(data, false),
        "session.step.failed" | "session.execution.failed" | "session.compaction.failed" => {
            Some(HarnessEvent::Error {
                session: event_session(data),
                message: error_message(data),
                retryable: data
                    .get("error")
                    .and_then(|error| error.get("retryAfterMs"))
                    .is_some()
                    || kind == "session.retry.scheduled",
            })
        }
        "session.retry.scheduled" => Some(HarnessEvent::SessionStatus {
            session: event_session(data)?,
            state: SessionState::Retry,
        }),
        "session.tool.called"
        | "session.tool.progress"
        | "session.tool.success"
        | "session.tool.failed" => tool_part(state, data, kind),
        "session.created" => {
            let info = data.get("info").unwrap_or(data);
            let summary = session_summary(info)?;
            Some(HarnessEvent::SessionCreated {
                session: summary.id,
                title: Some(summary.title),
                parent: summary.parent,
                agent: info
                    .get("agent")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        }
        "session.deleted" => Some(HarnessEvent::SessionDeleted {
            session: event_session(data)?,
        }),
        "session.renamed" => Some(HarnessEvent::SessionChanged {
            session: event_session(data)?,
            title: data
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            parent: None,
            agent: None,
        }),
        "session.agent.selected" => Some(HarnessEvent::SessionChanged {
            session: event_session(data)?,
            title: None,
            parent: None,
            agent: data
                .get("agent")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        "permission.asked" => {
            permission_from_value(data).map(|request| HarnessEvent::PermissionAsked { request })
        }
        "permission.replied" => {
            let request = data
                .get("requestID")
                .or_else(|| data.get("id"))
                .and_then(Value::as_str)?;
            Some(HarnessEvent::PermissionReplied {
                session: event_session(data),
                request: request.to_string(),
                decision: permission_decision(data)?,
            })
        }
        "form.created" => {
            let form = data.get("form").unwrap_or(data);
            form_from_value(form).map(|request| HarnessEvent::QuestionAsked { request })
        }
        "form.replied" | "form.cancelled" => {
            let request = data
                .get("id")
                .or_else(|| data.get("formID"))
                .and_then(Value::as_str)?;
            let summary = if kind == "form.replied" {
                data.get("answer")
                    .and_then(|answer| serde_json::to_string(answer).ok())
            } else {
                None
            };
            Some(HarnessEvent::QuestionResolved {
                session: event_session(data),
                request: request.to_string(),
                summary,
            })
        }
        "session.shell.ended" => {
            let session = event_session(data)?;
            let status = data
                .pointer("/shell/status")
                .and_then(Value::as_str)
                .unwrap_or("exited");
            let output = data
                .pointer("/output/output")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let result = if status == "exited" {
                Ok(output)
            } else {
                Err(format!("shell {status}"))
            };
            Some(HarnessEvent::CommandOutput { session, result })
        }
        "session.skill.activated" => {
            let session = event_session(data)?;
            let name = data
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| data.get("id").and_then(Value::as_str))
                .unwrap_or("skill");
            Some(HarnessEvent::CommandOutput {
                session,
                result: Ok(format!("Skill activated: {name}")),
            })
        }
        _ => None,
    }
}

fn event_session(data: &Value) -> Option<SessionId> {
    data.get("sessionID")
        .and_then(Value::as_str)
        .map(SessionId::new)
}

fn text_part(state: &mut V2StreamState, data: &Value, reasoning: bool) -> Option<HarnessEvent> {
    let session = event_session(data)?;
    let assistant = data.get("assistantMessageID")?.as_str()?.to_string();
    let delta = data.get("delta")?.as_str()?;
    let key = (session.clone(), assistant.clone());
    let text = if reasoning {
        state.reasoning.entry(key).or_default()
    } else {
        state.text.entry(key).or_default()
    };
    text.push_str(delta);
    let payload = if reasoning {
        PartPayload::Reasoning(text.clone())
    } else {
        PartPayload::Text(text.clone())
    };
    Some(HarnessEvent::PartSnapshot {
        part: PartSnapshot {
            id: format!(
                "{assistant}:{}",
                if reasoning { "reasoning" } else { "text" }
            ),
            session,
            message_id: assistant,
            payload,
        },
    })
}

fn step_part(data: &Value, started: bool) -> Option<HarnessEvent> {
    let session = event_session(data)?;
    let assistant = data.get("assistantMessageID")?.as_str()?.to_string();
    Some(HarnessEvent::PartSnapshot {
        part: PartSnapshot {
            id: format!(
                "{assistant}:{}",
                if started { "step-start" } else { "step-finish" }
            ),
            session,
            message_id: assistant,
            payload: if started {
                PartPayload::StepStart
            } else {
                PartPayload::StepFinish
            },
        },
    })
}

fn tool_part(state: &mut V2StreamState, data: &Value, kind: &str) -> Option<HarnessEvent> {
    let session = event_session(data)?;
    let assistant = data.get("assistantMessageID")?.as_str()?.to_string();
    let tool_id = data.get("id")?.as_str()?.to_string();
    let key = (session.clone(), assistant.clone(), tool_id.clone());
    let mut tool = state.tools.get(&key).cloned().unwrap_or(ToolSnapshot {
        name: data
            .get("name")
            .or_else(|| data.get("tool"))
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string(),
        state: ToolState::Pending,
        input: None,
        title: None,
        metadata: None,
    });
    if let Some(name) = data
        .get("name")
        .or_else(|| data.get("tool"))
        .and_then(Value::as_str)
    {
        tool.name = name.to_string();
    }
    if let Some(input) = data.get("input") {
        tool.input = Some(input.clone());
    }
    if let Some(metadata) = data.get("metadata") {
        tool.metadata = Some(metadata.clone());
    }
    tool.state = match kind {
        "session.tool.success" => ToolState::Completed {
            output: tool_output(data.get("content")),
        },
        "session.tool.failed" => ToolState::Failed {
            message: error_message(data),
        },
        _ => ToolState::Running,
    };
    state.tools.insert(key, tool.clone());
    Some(HarnessEvent::PartSnapshot {
        part: PartSnapshot {
            id: format!("{assistant}:tool:{tool_id}"),
            session,
            message_id: assistant,
            payload: PartPayload::Tool(tool),
        },
    })
}

fn error_message(data: &Value) -> String {
    data.get("error")
        .and_then(|error| {
            error
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
        })
        .or_else(|| data.get("message").and_then(Value::as_str))
        .unwrap_or("opencode v2 reported an error")
        .to_string()
}

fn tool_output(value: Option<&Value>) -> String {
    match value {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.get("text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| item.to_string())
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::store::test_support::lock_home;

    use std::ffi::OsString;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    struct XdgGuard {
        old: Option<OsString>,
        dir: PathBuf,
    }

    impl XdgGuard {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "warp-agent-v2-{name}-{}-{nanos}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("opencode")).expect("v2 state");
            let old = std::env::var_os("XDG_STATE_HOME");
            unsafe {
                std::env::set_var("XDG_STATE_HOME", &dir);
            }
            Self { old, dir }
        }

        fn dir(&self) -> &std::path::Path {
            &self.dir
        }

        fn registration_path(&self) -> PathBuf {
            self.dir.join("opencode").join("service.json")
        }

        fn write_registration(&self, url: &str, password: &str) {
            std::fs::write(
                self.registration_path(),
                json!({ "url": url, "pid": 1, "password": password }).to_string(),
            )
            .expect("registration");
        }
    }

    impl Drop for XdgGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.old {
                    Some(value) => std::env::set_var("XDG_STATE_HOME", value),
                    None => std::env::remove_var("XDG_STATE_HOME"),
                }
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn fake_cli(dir: &std::path::Path, url: &str, start_required: bool) -> String {
        use std::os::unix::fs::PermissionsExt;

        let marker = dir.join("started");
        if !start_required {
            std::fs::write(&marker, b"started").expect("marker");
        }
        let script = dir.join("fake-opencode");
        let body = format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"service\" ] && [ \"$2\" = \"status\" ]; then\n\
               if [ -f \"{}\" ]; then echo \"{}\"; else echo stopped; fi\n\
             elif [ \"$1\" = \"service\" ] && [ \"$2\" = \"start\" ]; then\n\
               touch \"{}\"\n\
             else\n\
               echo stopped\n\
             fi\n",
            marker.display(),
            url,
            marker.display()
        );
        std::fs::write(&script, body).expect("fake CLI");
        let mut permissions = std::fs::metadata(&script)
            .expect("CLI metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("CLI permissions");
        script.to_string_lossy().into_owned()
    }

    struct Rotation {
        registration: PathBuf,
        url: String,
        password: String,
    }

    struct FakeV2 {
        url: String,
        stop: Arc<AtomicBool>,
        rotation: Arc<Mutex<Option<Rotation>>>,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl FakeV2 {
        fn start(password: &str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("fake v2 listener");
            listener.set_nonblocking(true).expect("nonblocking");
            let url = format!("http://{}", listener.local_addr().expect("addr"));
            let stop = Arc::new(AtomicBool::new(false));
            let password = Arc::new(Mutex::new(password.to_string()));
            let rotation = Arc::new(Mutex::new(None));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let thread_password = password.clone();
            let thread_rotation = rotation.clone();
            let thread_requests = requests.clone();
            let thread_stop = stop.clone();
            std::thread::spawn(move || {
                while !thread_stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            handle(stream, &thread_password, &thread_rotation, &thread_requests);
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                url,
                stop,
                rotation,
                requests,
            }
        }

        fn rotate_on_unauthorized(&self, registration: PathBuf, password: &str) {
            *self.rotation.lock().expect("rotation") = Some(Rotation {
                registration,
                url: self.url.clone(),
                password: password.to_string(),
            });
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().expect("requests").clone()
        }
    }

    impl Drop for FakeV2 {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
        }
    }

    fn handle(
        mut stream: TcpStream,
        password: &Arc<Mutex<String>>,
        rotation: &Arc<Mutex<Option<Rotation>>>,
        requests: &Arc<Mutex<Vec<String>>>,
    ) {
        let Some((method, path, authorization, body)) = read_request(&mut stream) else {
            return;
        };
        requests
            .lock()
            .expect("requests")
            .push(format!("{method} {path} {body}"));
        let expected = format!(
            "Basic {}",
            crate::oc::base64_encode(
                format!("opencode:{}", password.lock().expect("password")).as_bytes()
            )
        );
        if authorization != expected {
            if let Some(rotation) = rotation.lock().expect("rotation").take() {
                let _ = std::fs::write(
                    &rotation.registration,
                    json!({
                        "url": rotation.url,
                        "pid": 1,
                        "password": rotation.password,
                    })
                    .to_string(),
                );
                *password.lock().expect("password") = rotation.password;
            }
            respond(
                &mut stream,
                "401 Unauthorized",
                r#"{"error":"unauthorized"}"#,
            );
            return;
        }
        let path = path.split('?').next().unwrap_or("/");
        if path == "/api/event" {
            respond_sse(&mut stream);
            return;
        }
        let (status, body) = match (method.as_str(), path) {
            ("GET", "/api/info") => (
                "200 OK",
                r#"{"version":"2.3.0","pid":1,"hostname":"127.0.0.1","urls":[]}"#,
            ),
            ("GET", "/api/model") => (
                "200 OK",
                r#"{"data":[{"id":"m1","providerID":"p1","variants":[{"id":"low"},{"id":"high"}],"limit":{"context":1000},"enabled":true},{"id":"off","providerID":"p1","enabled":false}]}"#,
            ),
            ("GET", "/api/provider") => {
                ("200 OK", r#"{"data":[{"id":"p1","name":"Provider One"}]}"#)
            }
            ("GET", "/api/agent") => (
                "200 OK",
                r#"{"data":[{"id":"build","name":"build","mode":"primary","hidden":false}]}"#,
            ),
            ("GET", "/api/command") => (
                "200 OK",
                r#"{"data":[{"name":"review","description":"Review the change"}]}"#,
            ),
            ("GET", "/api/skill") => (
                "200 OK",
                r#"{"data":[{"id":"sk1","name":"skill-one","description":"A skill"}]}"#,
            ),
            ("GET", "/api/session") => (
                "200 OK",
                r#"{"data":[{"id":"s1","title":"Session One","time":{"created":1,"updated":2},"location":{"directory":"/tmp/work"}},{"id":"s2","title":"Child","parentID":"s1","time":{"updated":3},"location":{"directory":"/tmp/work"}}]}"#,
            ),
            ("POST", "/api/session") => (
                "200 OK",
                r#"{"data":{"id":"s3","title":"Created","time":{"updated":4},"location":{"directory":"/tmp/work"}}}"#,
            ),
            ("GET", "/api/session/s1") => (
                "200 OK",
                r#"{"data":{"id":"s1","title":"Session One","time":{"updated":2},"location":{"directory":"/tmp/work"}}}"#,
            ),
            ("PATCH", "/api/session/s1") | ("DELETE", "/api/session/s1") => ("200 OK", "{}"),
            ("POST", "/api/session/s1/fork") => (
                "200 OK",
                r#"{"data":{"id":"s4","title":"Fork","parentID":"s1","time":{"updated":5},"location":{"directory":"/tmp/work"}}}"#,
            ),
            ("POST", "/api/session/s1/prompt")
            | ("POST", "/api/session/s1/interrupt")
            | ("POST", "/api/session/s1/compact") => ("200 OK", r#"{"data":{}}"#),
            ("GET", "/api/session/s1/permission") => (
                "200 OK",
                r#"{"data":[{"id":"p1","sessionID":"s1","action":"edit","resources":["src/main.rs"],"metadata":{"diff":"@@ -1 +1 @@"},"message":"Allow edit?"}]}"#,
            ),
            ("POST", "/api/session/s1/permission/p1/reply") => ("200 OK", "{}"),
            ("GET", "/api/session/s1/form") => (
                "200 OK",
                r#"{"data":[{"id":"f1","sessionID":"s1","title":"Deploy where?","fields":[{"id":"target","title":"Target","type":"string","required":true},{"id":"envs","title":"Environments","type":"multiselect","options":["dev","prod"]}]}]}"#,
            ),
            ("POST", "/api/session/s1/form/f1/reply") => ("200 OK", "{}"),
            ("POST", "/api/session/s1/form/bad/reply") => {
                ("400 Bad Request", r#"{"error":"invalid answer"}"#)
            }
            ("POST", "/api/session/s1/command") => ("200 OK", r#"{"data":{}}"#),
            ("GET", "/api/session/s1/diff") => (
                "200 OK",
                r#"{"data":[{"path":"src/main.rs","patch":"@@ -1 +1 @@\n-old\n+new"}]}"#,
            ),
            ("GET", "/api/session/s1/inbox") => (
                "200 OK",
                r#"{"data":[{"id":"i1","sessionID":"s1","type":"user","delivery":"queue"},{"id":"i2","sessionID":"s1","type":"synthetic","delivery":"steer"}]}"#,
            ),
            ("PATCH", "/api/session/s1/inbox/i1") | ("DELETE", "/api/session/s1/inbox/i1") => {
                ("204 No Content", "")
            }
            ("POST", "/api/session/s1/revert/stage") => (
                "200 OK",
                r#"{"data":{"messageID":"m1","files":[{"path":"src/main.rs","patch":"@@ -1 +1 @@"}]}}"#,
            ),
            ("POST", "/api/session/s1/revert/commit")
            | ("DELETE", "/api/session/s1/revert")
            | ("POST", "/api/session/s1/shell")
            | ("POST", "/api/session/s1/background") => ("204 No Content", ""),
            ("POST", "/api/experimental/session/s1/skill") => ("204 No Content", ""),
            ("GET", "/api/mcp") => (
                "200 OK",
                r#"{"data":[{"name":"files","status":{"status":"connected"}},{"name":"web","status":{"status":"pending"}}]}"#,
            ),
            ("PUT", "/api/experimental/mcp/files")
            | ("POST", "/api/experimental/mcp/files/connect")
            | ("POST", "/api/experimental/mcp/files/disconnect")
            | ("DELETE", "/api/experimental/mcp/files") => ("204 No Content", ""),
            ("PUT", "/api/experimental/mcp/bad") => {
                ("500 Internal Server Error", r#"{"error":"boom"}"#)
            }
            _ => ("404 Not Found", r#"{"error":"not found"}"#),
        };
        respond(&mut stream, status, body);
    }

    fn read_request(stream: &mut TcpStream) -> Option<(String, String, String, String)> {
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok()?;
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        let mut header_end = None;
        loop {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = Some(position + 4);
                break;
            }
            if buffer.len() > 64 * 1024 {
                break;
            }
        }
        let header_end = header_end?;
        let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while buffer.len() < header_end + content_length {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        let text = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let mut lines = text.split("\r\n");
        let request_line = lines.next()?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next()?.to_string();
        let path = parts.next()?.to_string();
        let mut authorization = String::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("authorization")
            {
                authorization = value.trim().to_string();
            }
        }
        let body = if buffer.len() > header_end {
            String::from_utf8_lossy(&buffer[header_end..])
                .trim()
                .to_string()
        } else {
            String::new()
        };
        Some((method, path, authorization, body))
    }

    fn respond(stream: &mut TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    fn respond_sse(stream: &mut TcpStream) {
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        );
        let frames = [
            json!({"type":"session.status","data":{"sessionID":"s1","status":{"type":"busy"}}}),
            json!({"type":"session.step.started","data":{"sessionID":"s1","assistantMessageID":"m1"}}),
            json!({"type":"session.text.delta","data":{"sessionID":"s1","assistantMessageID":"m1","ordinal":1,"delta":"Hello"}}),
            json!({"type":"session.text.delta","data":{"sessionID":"s1","assistantMessageID":"m1","ordinal":2,"delta":" world"}}),
            json!({"type":"session.reasoning.delta","data":{"sessionID":"s1","assistantMessageID":"m1","ordinal":1,"delta":"think"}}),
            json!({"type":"session.tool.called","data":{"sessionID":"s1","assistantMessageID":"m1","id":"t1","name":"bash","input":{"command":"ls"},"executed":true}}),
            json!({"type":"session.tool.success","data":{"sessionID":"s1","assistantMessageID":"m1","id":"t1","content":[{"type":"text","text":"ok"}],"metadata":{},"executed":true}}),
            json!({"type":"session.step.ended","data":{"sessionID":"s1","assistantMessageID":"m1"}}),
            json!({"type":"permission.asked","data":{"id":"p1","sessionID":"s1","action":"edit","resources":["src/main.rs"],"message":"Allow edit?"}}),
            json!({"type":"form.created","data":{"form":{"id":"f1","sessionID":"s1","title":"Deploy where?","fields":[{"id":"target","title":"Target","type":"string","required":true}]}}}),
            json!({"type":"permission.replied","data":{"sessionID":"s1","requestID":"p1","reply":"once"}}),
            json!({"type":"form.replied","data":{"sessionID":"s1","id":"f1","answer":{"target":"prod"}}}),
            json!({"type":"form.cancelled","data":{"sessionID":"s1","id":"f2"}}),
            json!({"type":"session.shell.ended","data":{"sessionID":"s1","shell":{"status":"exited"},"output":{"output":"done"}}}),
            json!({"type":"session.idle","data":{"sessionID":"s1"}}),
        ];
        for frame in frames {
            let _ = write!(stream, "data: {frame}\n\n");
            let _ = stream.flush();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn v2_harness(cli: &str) -> OpenCodeV2Harness {
        OpenCodeV2Harness::new(HarnessConfig::new(cli))
    }

    #[test]
    fn a_fake_v2_service_discovers_and_lists_catalogs() {
        let _lock = lock_home();
        let xdg = XdgGuard::new("catalog");
        let server = FakeV2::start("secret");
        xdg.write_registration(&server.url, "secret");
        let mut harness = v2_harness(&fake_cli(xdg.dir(), &server.url, false));

        harness.start().expect("attaches to the fake service");
        assert_eq!(harness.health().state, HealthState::Ready);
        assert_eq!(harness.id().as_str(), "opencode-v2");

        let providers = harness.providers().expect("providers");
        assert_eq!(providers[0].id, "p1");
        assert_eq!(providers[0].name, "Provider One");

        let models = harness.models().expect("models");
        assert_eq!(models.len(), 1, "disabled models are left out");
        assert_eq!(models[0].key, "p1/m1");
        assert_eq!(models[0].variants, ["low", "high"]);
        assert_eq!(models[0].context_limit, 1000);

        let agents = harness.agents().expect("agents");
        assert_eq!(agents[0].name, "build");
        assert_eq!(agents[0].mode, "primary");

        let commands = harness.commands(None).expect("commands");
        assert_eq!(commands[0].name, "review");
        let skills = harness.skills(None).expect("skills");
        assert_eq!(skills[0].id, "sk1");

        let sessions = harness.sessions().expect("sessions");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id.as_str(), "s1");
        assert_eq!(sessions[0].title, "Session One");
        assert_eq!(
            sessions[0].directory.as_deref(),
            Some(std::path::Path::new("/tmp/work"))
        );
        assert_eq!(
            sessions[1].parent.as_ref().map(SessionId::as_str),
            Some("s1")
        );

        let session = harness.session(&SessionId::new("s1")).expect("session");
        assert_eq!(session.title, "Session One");

        let created = harness
            .create_session(NewSession {
                title: Some("Created".into()),
                ..Default::default()
            })
            .expect("create");
        assert_eq!(created.id.as_str(), "s3");
        let fork = harness
            .fork_session(&SessionId::new("s1"), None)
            .expect("fork");
        assert_eq!(fork.id.as_str(), "s4");
        harness
            .rename_session(&SessionId::new("s1"), "Renamed")
            .expect("rename");
        harness
            .delete_session(&SessionId::new("s1"))
            .expect("delete");

        harness.stop().expect("disconnect");
        assert_eq!(harness.health().state, HealthState::Unavailable);
        assert!(matches!(
            harness.models().unwrap_err(),
            HarnessError::Unavailable(_)
        ));
    }

    #[test]
    fn a_prompt_streams_text_reasoning_tools_and_idle() {
        let _lock = lock_home();
        let xdg = XdgGuard::new("stream");
        let server = FakeV2::start("secret");
        xdg.write_registration(&server.url, "secret");
        let mut harness = v2_harness(&fake_cli(xdg.dir(), &server.url, false));
        harness.start().expect("attaches");
        let events = harness.events();
        let session = SessionId::new("s1");
        harness
            .send_prompt(
                &session,
                Prompt {
                    text: "hi".into(),
                    ..Default::default()
                },
            )
            .expect("prompt is accepted");
        harness.interrupt(&session).expect("interrupt");
        harness.compact(&session).expect("compact");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut text = None;
        let mut reasoning = None;
        let mut tool = None;
        let mut idle = false;
        let mut steps = Vec::new();
        let mut permission_asked = 0;
        let mut form_asked = 0;
        let mut resolved = 0;
        let mut shell_output = None;
        while std::time::Instant::now() < deadline {
            while let Ok(event) = events.try_recv() {
                match event {
                    HarnessEvent::PartSnapshot { part } => match part.payload {
                        PartPayload::Text(value) if value.contains("Hello world") => {
                            text = Some(value);
                        }
                        PartPayload::Reasoning(value) => reasoning = Some(value),
                        PartPayload::Tool(snapshot) => {
                            if let ToolState::Completed { output } = &snapshot.state {
                                tool = Some(output.clone());
                            }
                        }
                        PartPayload::StepStart => steps.push("start"),
                        PartPayload::StepFinish => steps.push("finish"),
                        _ => {}
                    },
                    HarnessEvent::SessionStatus {
                        state: SessionState::Idle,
                        ..
                    } => idle = true,
                    HarnessEvent::PermissionAsked { .. } => permission_asked += 1,
                    HarnessEvent::QuestionAsked { .. } => form_asked += 1,
                    HarnessEvent::QuestionResolved { .. } => resolved += 1,
                    HarnessEvent::CommandOutput {
                        result: Ok(output), ..
                    } => shell_output = Some(output),
                    _ => {}
                }
            }
            if text.is_some() && reasoning.is_some() && tool.is_some() && idle {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(text.as_deref(), Some("Hello world"));
        assert_eq!(reasoning.as_deref(), Some("think"));
        assert_eq!(tool.as_deref(), Some("ok"));
        assert_eq!(steps, ["start", "finish"]);
        assert_eq!(permission_asked, 1);
        assert_eq!(form_asked, 1);
        assert_eq!(resolved, 2);
        assert_eq!(shell_output.as_deref(), Some("done"));
        assert!(idle, "session.idle settled the turn");
    }

    #[test]
    fn a_v2_permission_and_form_round_trip() {
        let _lock = lock_home();
        let xdg = XdgGuard::new("approvals");
        let server = FakeV2::start("secret");
        xdg.write_registration(&server.url, "secret");
        let mut harness = v2_harness(&fake_cli(xdg.dir(), &server.url, false));
        harness.start().expect("attaches");
        let session = SessionId::new("s1");

        let permissions = harness.permissions(&session).expect("permissions");
        assert_eq!(permissions.len(), 1);
        assert_eq!(permissions[0].id, "p1");
        assert_eq!(permissions[0].action, "edit");
        assert_eq!(permissions[0].resources, ["src/main.rs"]);
        assert_eq!(permissions[0].diff.as_deref(), Some("@@ -1 +1 @@"));
        assert_eq!(permissions[0].message.as_deref(), Some("Allow edit?"));

        let forms = harness.forms(&session).expect("forms");
        assert_eq!(forms.len(), 1);
        assert_eq!(forms[0].title, "Deploy where?");
        assert_eq!(forms[0].fields[0].kind, FormFieldKind::Text);
        assert!(forms[0].fields[0].required);
        assert_eq!(forms[0].fields[1].kind, FormFieldKind::Multiselect);
        assert_eq!(forms[0].fields[1].options, ["dev", "prod"]);

        harness
            .reply_permission(&session, "p1", PermissionDecision::Once)
            .expect("reply once");
        let mut answer = FormAnswer::default();
        answer.values.insert("target".into(), json!("prod"));
        answer.values.insert("envs".into(), json!(["dev"]));
        harness
            .reply_form(&session, "f1", answer.clone())
            .expect("multi-field answer");

        let requests = server.requests();
        assert!(requests.iter().any(|request| {
            request.contains("/api/session/s1/permission/p1/reply")
                && request.contains("\"reply\":\"once\"")
        }));
        assert!(requests.iter().any(|request| {
            request.contains("/api/session/s1/form/f1/reply")
                && request.contains("\"answer\"")
                && request.contains("\"target\":\"prod\"")
        }));

        let error = harness
            .reply_form(&session, "bad", answer)
            .expect_err("the fake rejects the bad form id");
        assert!(matches!(
            error,
            HarnessError::Failed(ref detail) if detail.contains("invalid answer")
        ));
    }

    #[test]
    fn a_v2_command_and_diff_round_trip() {
        let _lock = lock_home();
        let xdg = XdgGuard::new("command-diff");
        let server = FakeV2::start("secret");
        xdg.write_registration(&server.url, "secret");
        let mut harness = v2_harness(&fake_cli(xdg.dir(), &server.url, false));
        harness.start().expect("attaches");
        let session = SessionId::new("s1");

        harness
            .run_command(
                &session,
                CommandRun {
                    name: "review".into(),
                    text: "check the diff".into(),
                },
            )
            .expect("command");
        assert!(server.requests().iter().any(|request| {
            request.contains("/api/session/s1/command")
                && request.contains("\"name\":\"review\"")
                && request.contains("\"text\":\"check the diff\"")
        }));

        let diffs = harness.diff(&session).expect("diff");
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].path, "src/main.rs");
        assert!(diffs[0].patch.contains("-old"));
        assert!(diffs[0].patch.contains("+new"));
    }

    #[test]
    fn a_v2_inbox_revert_shell_and_background_round_trip() {
        let _lock = lock_home();
        let xdg = XdgGuard::new("v2-surfaces");
        let server = FakeV2::start("secret");
        xdg.write_registration(&server.url, "secret");
        let mut harness = v2_harness(&fake_cli(xdg.dir(), &server.url, false));
        harness.start().expect("attaches");
        let session = SessionId::new("s1");

        let items = harness.inbox(&session).expect("inbox");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].kind, InboxKind::User);
        assert_eq!(items[0].delivery, InboxDelivery::Queue);
        assert_eq!(items[1].kind, InboxKind::Synthetic);
        assert_eq!(items[1].delivery, InboxDelivery::Steer);

        harness
            .update_inbox(&session, "i1", InboxDelivery::Steer)
            .expect("steer");
        harness.cancel_inbox(&session, "i1").expect("cancel");
        harness.stage_revert(&session, "m1").expect("stage revert");
        harness.commit_revert(&session).expect("commit revert");
        harness.clear_revert(&session).expect("clear revert");
        harness.run_shell(&session, "ls -la").expect("shell");
        harness.background_session(&session).expect("background");

        let requests = server.requests();
        assert!(requests.iter().any(|request| {
            request.contains("PATCH /api/session/s1/inbox/i1")
                && request.contains("\"delivery\":\"steer\"")
        }));
        assert!(
            requests
                .iter()
                .any(|request| request.contains("DELETE /api/session/s1/inbox/i1"))
        );
        assert!(requests.iter().any(|request| {
            request.contains("POST /api/session/s1/revert/stage")
                && request.contains("\"messageID\":\"m1\"")
        }));
        assert!(
            requests
                .iter()
                .any(|request| request.contains("POST /api/session/s1/revert/commit"))
        );
        assert!(
            requests
                .iter()
                .any(|request| request.contains("DELETE /api/session/s1/revert"))
        );
        assert!(requests.iter().any(|request| {
            request.contains("POST /api/session/s1/shell") && request.contains("ls -la")
        }));
        assert!(
            requests
                .iter()
                .any(|request| request.contains("POST /api/session/s1/background"))
        );
    }

    #[test]
    fn a_v2_skill_activation_and_mcp_management_round_trip() {
        let _lock = lock_home();
        let xdg = XdgGuard::new("v2-skills-mcp");
        let server = FakeV2::start("secret");
        xdg.write_registration(&server.url, "secret");
        let mut harness = v2_harness(&fake_cli(xdg.dir(), &server.url, false));
        harness.start().expect("attaches");
        let session = SessionId::new("s1");

        harness
            .activate_skill(&session, "sk1")
            .expect("activate skill");
        let servers = harness.mcp_servers().expect("mcp servers");
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "files");
        assert_eq!(servers[0].status, McpStatus::Connected);
        assert_eq!(servers[1].status, McpStatus::Pending);

        let mut env = std::collections::BTreeMap::new();
        env.insert("TOKEN".to_string(), "t".to_string());
        harness
            .add_mcp(McpConfig {
                name: "files".into(),
                command: Some("npx server".into()),
                url: None,
                env,
            })
            .expect("add mcp");
        harness.connect_mcp("files").expect("connect");
        harness.disconnect_mcp("files").expect("disconnect");
        harness.remove_mcp("files").expect("remove");

        let error = harness
            .add_mcp(McpConfig {
                name: "bad".into(),
                command: Some("npx bad".into()),
                url: None,
                env: Default::default(),
            })
            .expect_err("the fake rejects the bad MCP server");
        assert!(matches!(
            error,
            HarnessError::Failed(ref detail) if detail.contains("boom")
        ));

        let requests = server.requests();
        assert!(requests.iter().any(|request| {
            request.contains("POST /api/experimental/session/s1/skill")
                && request.contains("\"id\":\"sk1\"")
        }));
        assert!(requests.iter().any(|request| {
            request.contains("PUT /api/experimental/mcp/files")
                && request.contains("\"type\":\"local\"")
                && request.contains("\"npx\"")
                && request.contains("\"server\"")
                && request.contains("\"TOKEN\":\"t\"")
        }));
        assert!(
            requests
                .iter()
                .any(|request| { request.contains("POST /api/experimental/mcp/files/connect") })
        );
        assert!(
            requests
                .iter()
                .any(|request| { request.contains("POST /api/experimental/mcp/files/disconnect") })
        );
        assert!(
            requests
                .iter()
                .any(|request| { request.contains("DELETE /api/experimental/mcp/files") })
        );
    }

    #[test]
    fn v2_structured_errors_and_session_events_translate() {
        let mut state = V2StreamState::default();
        let error = json!({
            "type": "session.execution.failed",
            "data": {
                "sessionID": "s1",
                "error": {
                    "type": "provider",
                    "message": "rate limited",
                    "retryAfterMs": 1000
                }
            }
        });
        assert_eq!(
            translate_v2_event(&error, &mut state),
            Some(HarnessEvent::Error {
                session: Some(SessionId::new("s1")),
                message: "rate limited".into(),
                retryable: true,
            })
        );

        let created = json!({
            "type": "session.created",
            "data": {
                "info": {
                    "id": "s2",
                    "title": "Child",
                    "parentID": "s1",
                    "agent": "build",
                    "time": { "updated": 7 },
                    "location": { "directory": "/tmp/work" }
                }
            }
        });
        assert_eq!(
            translate_v2_event(&created, &mut state),
            Some(HarnessEvent::SessionCreated {
                session: SessionId::new("s2"),
                title: Some("Child".into()),
                parent: Some(SessionId::new("s1")),
                agent: Some("build".into()),
            })
        );

        let skill = json!({
            "type": "session.skill.activated",
            "data": {
                "sessionID": "s1",
                "id": "sk1",
                "name": "deploy",
                "text": "activated"
            }
        });
        assert_eq!(
            translate_v2_event(&skill, &mut state),
            Some(HarnessEvent::CommandOutput {
                session: SessionId::new("s1"),
                result: Ok("Skill activated: deploy".into()),
            })
        );

        let ended = json!({
            "type": "session.execution.succeeded",
            "data": { "sessionID": "s1" }
        });
        assert_eq!(
            translate_v2_event(&ended, &mut state),
            Some(HarnessEvent::SessionStatus {
                session: SessionId::new("s1"),
                state: SessionState::Idle,
            })
        );
    }

    #[test]
    fn a_stopped_service_is_started_and_attached() {
        let _lock = lock_home();
        let xdg = XdgGuard::new("start");
        let server = FakeV2::start("secret");
        xdg.write_registration(&server.url, "secret");
        let cli = fake_cli(xdg.dir(), &server.url, true);
        let mut harness = v2_harness(&cli);
        harness.start().expect("starts and attaches");
        assert_eq!(harness.health().state, HealthState::Ready);
        assert!(xdg.dir().join("started").exists(), "service start ran");
    }

    #[test]
    fn missing_cli_and_missing_registration_are_distinct_states() {
        let _lock = lock_home();
        let xdg = XdgGuard::new("missing");
        let missing = xdg.dir().join("not-an-opencode");
        let mut harness = v2_harness(&missing.to_string_lossy());
        let error = harness.start().unwrap_err();
        assert!(matches!(
            error,
            HarnessError::Unavailable(ref detail) if detail.contains("CLI not found")
        ));

        let server = FakeV2::start("secret");
        let mut harness = v2_harness(&fake_cli(xdg.dir(), &server.url, false));
        let error = harness.start().unwrap_err();
        assert!(matches!(
            error,
            HarnessError::Unavailable(ref detail) if detail.contains("registration file is missing")
        ));
    }

    #[test]
    fn bad_credentials_are_unavailable() {
        let _lock = lock_home();
        let xdg = XdgGuard::new("bad");
        let server = FakeV2::start("secret");
        xdg.write_registration(&server.url, "wrong");
        let mut harness = v2_harness(&fake_cli(xdg.dir(), &server.url, false));
        let error = harness.start().unwrap_err();
        assert!(matches!(
            error,
            HarnessError::Unavailable(ref detail) if detail.contains("registration password")
        ));
        assert_eq!(harness.health().state, HealthState::Unavailable);
    }

    #[test]
    fn a_401_rereads_the_rotated_registration_file() {
        let _lock = lock_home();
        let xdg = XdgGuard::new("rotate");
        let server = FakeV2::start("new-secret");
        xdg.write_registration(&server.url, "old-secret");
        server.rotate_on_unauthorized(xdg.registration_path(), "new-secret");
        let mut harness = v2_harness(&fake_cli(xdg.dir(), &server.url, false));
        harness.start().expect("the rotated password is re-read");
        assert_eq!(harness.health().state, HealthState::Ready);
    }
}
