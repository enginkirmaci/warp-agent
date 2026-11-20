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
        let http = self.http.clone();
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

    fn commands(&mut self) -> Result<Vec<Command>, HarnessError> {
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

    fn skills(&mut self) -> Result<Vec<Skill>, HarnessError> {
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
        if let Some(agent) = prompt.choice.agent {
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
        _session: &SessionId,
        _command: CommandRun,
    ) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("run_command"))
    }

    fn activate_skill(&mut self, _session: &SessionId, _skill: &str) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("activate_skill"))
    }

    fn permissions(
        &mut self,
        _session: &SessionId,
    ) -> Result<Vec<PermissionRequest>, HarnessError> {
        Err(HarnessError::Unsupported("permissions"))
    }

    fn reply_permission(
        &mut self,
        _session: &SessionId,
        _request: &str,
        _decision: PermissionDecision,
    ) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("reply_permission"))
    }

    fn forms(&mut self, _session: &SessionId) -> Result<Vec<FormRequest>, HarnessError> {
        Err(HarnessError::Unsupported("forms"))
    }

    fn reply_form(
        &mut self,
        _session: &SessionId,
        _form: &str,
        _answer: FormAnswer,
    ) -> Result<(), HarnessError> {
        Err(HarnessError::Unsupported("reply_form"))
    }

    fn diff(&mut self, _session: &SessionId) -> Result<Vec<FileDiff>, HarnessError> {
        Err(HarnessError::Unsupported("diff"))
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
    }

    impl FakeV2 {
        fn start(password: &str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("fake v2 listener");
            listener.set_nonblocking(true).expect("nonblocking");
            let url = format!("http://{}", listener.local_addr().expect("addr"));
            let stop = Arc::new(AtomicBool::new(false));
            let password = Arc::new(Mutex::new(password.to_string()));
            let rotation = Arc::new(Mutex::new(None));
            let thread_password = password.clone();
            let thread_rotation = rotation.clone();
            let thread_stop = stop.clone();
            std::thread::spawn(move || {
                while !thread_stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            handle(stream, &thread_password, &thread_rotation);
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
            }
        }

        fn rotate_on_unauthorized(&self, registration: PathBuf, password: &str) {
            *self.rotation.lock().expect("rotation") = Some(Rotation {
                registration,
                url: self.url.clone(),
                password: password.to_string(),
            });
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
    ) {
        let Some((method, path, authorization)) = read_request(&mut stream) else {
            return;
        };
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
            _ => ("404 Not Found", r#"{"error":"not found"}"#),
        };
        respond(&mut stream, status, body);
    }

    fn read_request(stream: &mut TcpStream) -> Option<(String, String, String)> {
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok()?;
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let read = stream.read(&mut chunk).ok()?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
            if buffer.len() > 64 * 1024 {
                break;
            }
        }
        let text = String::from_utf8_lossy(&buffer);
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
        Some((method, path, authorization))
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

        let commands = harness.commands().expect("commands");
        assert_eq!(commands[0].name, "review");
        let skills = harness.skills().expect("skills");
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
        assert!(idle, "session.idle settled the turn");
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
