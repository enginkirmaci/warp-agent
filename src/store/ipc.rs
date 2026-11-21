//! Store side of the DevTools door: every [`IpcCommand`] the net thread
//! relays lands here. Runs ride the normal chat flow — a real tile the user
//! can watch — with a watcher that answers the door when the turn idles.
//! A background run executes on the same pipeline but holds a hidden tile:
//! the UI never renders it, no workspace is selected, and when the turn
//! settles the tile and its session are retired so nothing surfaces.

use gpui::Context;
use serde_json::{Value, json};
use std::time::Duration;

use super::{Store, normalize_dir};
use crate::harness::{
    FormFieldKind, HarnessCapabilities, HarnessEvent, HarnessHealth, HarnessId, HarnessRegistry,
    MessageRole, PartPayload, SessionId, SessionState, ToolState,
};
use crate::ipc::{DoorModel, IpcCommand, Reply};
use crate::oc::{ApiRequest, NetCommand};
use crate::paths::AppPaths;

/// How long the store keeps a door harness reply parked on the net thread.
/// The door's own client timeout is 60 s; this only sweeps a reply a client
/// can no longer receive (a wedged or vanished net thread).
const HARNESS_CALL_TIMEOUT: Duration = Duration::from_secs(65);

/// One door `harness` request's store-side answer: ready now, or parked under
/// a call id until the net thread (or a server-start fact) answers it.
enum HarnessAnswer {
    Ready(Result<Value, String>),
    Pending(u64),
}

/// One door-run: a prompt DevTools wants executed and answered. Bound to a
/// draft tile key until the session exists, then to the session id.
pub struct IpcRun {
    pub workspace: String,
    pub tile_key: String,
    pub session_id: Option<String>,
    pub background: bool,
    pub reply: Reply,
}

/// The workspace a door command names. `general` — the empty string too — is
/// the built-in Tasks workspace, which is where a client sends a run that
/// carries its own context (the wands do); anything else is a folder path that
/// must exist on disk.
fn resolve_workspace(workspace: &str) -> Result<String, String> {
    if workspace.is_empty() || workspace == "general" {
        return Ok(AppPaths::general_workspace_path()
            .to_string_lossy()
            .into_owned());
    }
    let normalized = normalize_dir(workspace);
    if !std::path::Path::new(&normalized).exists() {
        return Err(format!("folder does not exist: {normalized}"));
    }
    Ok(normalized)
}

impl Store {
    /// Entry from the door, relayed through the UiEvent pump.
    pub fn handle_ipc(&mut self, command: IpcCommand, cx: &mut Context<Self>) {
        match command {
            IpcCommand::OpenWorkspace { path, reply } => {
                let _ = reply.send(self.open_workspace_at(&path, cx));
            }
            IpcCommand::NewChat {
                workspace,
                prefill,
                auto_send,
                model,
                reply,
            } => {
                let _ = reply.send(self.ipc_new_chat(&workspace, prefill, auto_send, model, cx));
            }
            IpcCommand::RunPrompt {
                run,
                workspace,
                prompt,
                model,
                background,
                reply,
            } => {
                self.ipc_run_prompt(run, &workspace, prompt, model, background, reply, cx);
            }
            IpcCommand::CancelRun { run, reply } => {
                let _ = reply.send(self.ipc_cancel_run(run, cx));
            }
            IpcCommand::Models {
                harness,
                refresh,
                reply,
            } => self.ipc_models(harness, refresh, reply, cx),
            IpcCommand::Harness {
                method,
                params,
                reply,
            } => match self.ipc_harness_request(&method, &params, cx) {
                HarnessAnswer::Ready(result) => {
                    let _ = reply.send(result);
                }
                HarnessAnswer::Pending(call_id) => self.ipc_harness_park(call_id, reply, cx),
            },
            IpcCommand::Status { reply } => {
                let _ = reply.send(Ok(json!({
                    "workspaces": self.ipc_status_workspaces(),
                    "harnesses": self.ipc_harness_metadata(),
                })));
            }
        }
    }

    fn ipc_harness_metadata(&self) -> Vec<Value> {
        let mut harnesses = Vec::new();
        for descriptor in HarnessRegistry::builtin().iter() {
            let id = descriptor.id.as_str();
            if !self.settings.harness_enabled(id) {
                continue;
            }
            let (state, available) = if crate::harness::attached(&descriptor.id) {
                if self.attached.contains_key(&descriptor.id) {
                    ("ready", true)
                } else if self.attached_status.contains_key(&descriptor.id) {
                    ("unavailable", false)
                } else {
                    ("stopped", false)
                }
            } else if crate::harness::app_scope(&descriptor.id) {
                (
                    if self.runtimes.contains_key(id) {
                        "ready"
                    } else {
                        "stopped"
                    },
                    true,
                )
            } else {
                (
                    if self.runtimes.keys().any(|key| !self.is_shared_key(key)) {
                        "ready"
                    } else {
                        "stopped"
                    },
                    true,
                )
            };
            let connection = match descriptor.connection {
                crate::harness::ConnectionKind::Managed => "managed",
                crate::harness::ConnectionKind::Attached => "attached",
            };
            harnesses.push(json!({
                "id": id,
                "label": descriptor.label,
                "connection": connection,
                "enabled": true,
                "available": available,
                "state": state,
                "executable": self.settings.harness_executable(id),
                "capabilities": harness_capabilities(&descriptor.capabilities),
            }));
        }
        harnesses
    }

    /// One door `harness` request. The cached answers (metadata, models,
    /// sessions, session) stay fast and local; everything else is parked on
    /// the net thread, and `health`/`start`/`stop` go through the app's own
    /// lifecycle so the door cannot split the store's state from the runtime.
    fn ipc_harness_request(
        &mut self,
        method: &str,
        params: &Value,
        cx: &mut Context<Self>,
    ) -> HarnessAnswer {
        let raw = method.trim();
        let method = raw.replace('-', "_");
        // The namespace's whole method set: an unknown one is refused before
        // a harness id is even read, because a typo must not start a backend.
        let known = matches!(
            method.as_str(),
            "metadata"
                | "list_harnesses"
                | "status"
                | "models"
                | "sessions"
                | "session"
                | "health"
                | "start"
                | "stop"
        ) || crate::harness::call::known(&method);
        if !known {
            return HarnessAnswer::Ready(Err(format!("unsupported: harness method {raw}")));
        }
        match method.as_str() {
            "metadata" | "list_harnesses" => HarnessAnswer::Ready(Ok(json!({
                "harnesses": self.ipc_harness_metadata(),
            }))),
            "status" => HarnessAnswer::Ready(Ok(json!({
                "harnesses": self.ipc_harness_metadata(),
            }))),
            "models" => {
                let harness = match self.ipc_harness_target(params) {
                    Ok(harness) => harness,
                    Err(error) => return HarnessAnswer::Ready(Err(error)),
                };
                HarnessAnswer::Ready(Ok(self.models_payload(Some(harness))))
            }
            "sessions" => {
                let harness = params.get("harness").and_then(Value::as_str);
                let mut sessions = Vec::new();
                for tile in &self.open_tiles {
                    let Some(session) = tile.session_id.as_ref() else {
                        continue;
                    };
                    if let Some(id) = harness
                        && id != tile.harness.as_str()
                    {
                        continue;
                    }
                    sessions.push(json!({
                        "id": session,
                        "title": self.title_for(session),
                        "harness": tile.harness.as_str(),
                        "running": self.running_by_session.contains(session),
                    }));
                }
                HarnessAnswer::Ready(Ok(json!({ "sessions": sessions })))
            }
            "session" => {
                let Some(session) = params.get("session").and_then(Value::as_str) else {
                    return HarnessAnswer::Ready(Err("session is required".into()));
                };
                match self.tile_for_session(session) {
                    Some(tile) => HarnessAnswer::Ready(Ok(json!({
                        "id": session,
                        "title": self.title_for(session),
                        "harness": tile.harness.as_str(),
                        "running": self.running_by_session.contains(session),
                    }))),
                    None => HarnessAnswer::Ready(Err(format!("not-found: session {session}"))),
                }
            }
            "health" => {
                let harness = match self.ipc_harness_target(params) {
                    Ok(harness) => harness,
                    Err(error) => return HarnessAnswer::Ready(Err(error)),
                };
                HarnessAnswer::Ready(Ok(self.ipc_harness_health(&harness, params)))
            }
            "start" => {
                let harness = match self.ipc_harness_target(params) {
                    Ok(harness) => harness,
                    Err(error) => return HarnessAnswer::Ready(Err(error)),
                };
                self.ipc_harness_start(harness, params, cx)
            }
            "stop" => {
                let harness = match self.ipc_harness_target(params) {
                    Ok(harness) => harness,
                    Err(error) => return HarnessAnswer::Ready(Err(error)),
                };
                HarnessAnswer::Ready(self.ipc_harness_stop(&harness, params))
            }
            _ => self.ipc_harness_dispatch(&method, params),
        }
    }

    /// Resolve the harness a door method names, refusing an unknown id or a
    /// disabled harness before any adapter is touched.
    fn ipc_harness_target(&self, params: &Value) -> Result<HarnessId, String> {
        let id = params
            .get("harness")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "harness is required".to_string())?;
        let harness = HarnessId::from_wire(id).ok_or_else(|| crate::harness::call::unknown(id))?;
        if !self.harness_enabled(&harness) {
            return Err(crate::harness::call::disabled(id));
        }
        Ok(harness)
    }

    /// The instance scope a door method names: an explicit `directory`, the
    /// session's own workspace when it has one, else the selected workspace
    /// for a per-workspace harness.
    fn ipc_harness_directory(&self, harness: &HarnessId, params: &Value) -> Option<String> {
        if let Some(directory) = params
            .get("directory")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|directory| !directory.is_empty())
        {
            return Some(normalize_dir(directory));
        }
        if let Some(session) = params.get("session").and_then(Value::as_str)
            && let Some(folder) = self.effective_workspace_path(session)
        {
            return Some(folder);
        }
        if crate::harness::workspace_scoped(harness) {
            return self.selected_workspace.clone();
        }
        None
    }

    /// The store's runtime for a managed harness, when it has one.
    fn ipc_harness_runtime(
        &self,
        harness: &HarnessId,
        params: &Value,
    ) -> Option<&super::WorkspaceRuntime> {
        if crate::harness::shared_server(harness) {
            return self.runtimes.get(harness.as_str());
        }
        let directory = self.ipc_harness_directory(harness, params)?;
        self.runtimes.get(&directory)
    }

    /// The typed health for a harness: the attached adapter's own state, or
    /// the managed server's latest health poll.
    fn ipc_harness_health(&self, harness: &HarnessId, params: &Value) -> Value {
        let health = if crate::harness::attached(harness) {
            match self.attached.get(harness) {
                Some(adapter) => adapter.health(),
                None => match self.attached_status.get(harness) {
                    Some(detail) => HarnessHealth::unavailable(detail.clone()),
                    None => HarnessHealth::unavailable("not attached"),
                },
            }
        } else if let Some(runtime) = self.ipc_harness_runtime(harness, params) {
            match runtime.healthy {
                Some(true) => HarnessHealth::ready(),
                Some(false) => HarnessHealth::unavailable("health check failed"),
                None => HarnessHealth::starting("waiting for the first health check"),
            }
        } else {
            HarnessHealth::unavailable("not running")
        };
        crate::harness::call::health_json(&health)
    }

    /// Start a harness through the app's own lifecycle: the attached adapter
    /// attaches, a managed server starts on its net key, and the reply is
    /// parked until `ServerStarted` lands.
    fn ipc_harness_start(
        &mut self,
        harness: HarnessId,
        params: &Value,
        cx: &mut Context<Self>,
    ) -> HarnessAnswer {
        if crate::harness::attached(&harness) {
            self.ensure_attached(&harness, cx);
            return match self.attached.get(&harness) {
                Some(_) => HarnessAnswer::Ready(Ok(json!({ "ok": true }))),
                None => {
                    let detail = self
                        .attached_status
                        .get(&harness)
                        .cloned()
                        .unwrap_or_else(|| format!("{} did not attach", harness.label()));
                    HarnessAnswer::Ready(Err(format!("unavailable: {detail}")))
                }
            };
        }
        let key = if crate::harness::shared_server(&harness) {
            self.shared_key()
        } else {
            let Some(directory) = self.ipc_harness_directory(&harness, params) else {
                return HarnessAnswer::Ready(Err(format!(
                    "unavailable: harness {harness} needs a workspace directory"
                )));
            };
            if !self
                .workspaces
                .iter()
                .any(|workspace| workspace.folder_path() == directory)
            {
                return HarnessAnswer::Ready(Err(format!(
                    "unavailable: workspace {directory} is not open"
                )));
            }
            directory
        };
        if self.runtimes.contains_key(&key) {
            return HarnessAnswer::Ready(Ok(json!({ "ok": true })));
        }
        let call_id = self.ipc_harness_call_id();
        self.ipc_harness_start_calls
            .push((harness.clone(), call_id));
        if crate::harness::shared_server(&harness) {
            self.ensure_shared_server(cx);
        } else {
            self.start_workspace(&key, cx);
        }
        HarnessAnswer::Pending(call_id)
    }

    /// Stop a harness through the app's own lifecycle: detach an attached
    /// adapter, stop a managed server and drop its runtime bookkeeping.
    fn ipc_harness_stop(&mut self, harness: &HarnessId, params: &Value) -> Result<Value, String> {
        self.fail_harness_start_calls(harness, "the harness was stopped");
        if crate::harness::attached(harness) {
            if let Some(mut adapter) = self.attached.remove(harness) {
                let _ = adapter.stop();
            }
            self.set_attached_status(harness, "disconnected");
            return Ok(json!({ "ok": true }));
        }
        let key = if crate::harness::shared_server(harness) {
            self.shared_key()
        } else {
            self.ipc_harness_directory(harness, params).ok_or_else(|| {
                format!("unavailable: harness {harness} needs a workspace directory")
            })?
        };
        if self.runtimes.contains_key(&key) {
            self.net.send(NetCommand::StopServer {
                ws_key: key.clone(),
            });
            self.teardown_runtime(&key);
        }
        Ok(json!({ "ok": true }))
    }

    /// Dispatch one app-level trait call through the net thread's host.
    fn ipc_harness_dispatch(&mut self, method: &str, params: &Value) -> HarnessAnswer {
        let harness = match self.ipc_harness_target(params) {
            Ok(harness) => harness,
            Err(error) => return HarnessAnswer::Ready(Err(error)),
        };
        let directory = self.ipc_harness_directory(&harness, params);
        let executable = self.settings.harness_executable(harness.as_str());
        let call_id = self.ipc_harness_call_id();
        self.net.send(NetCommand::HarnessCall {
            call_id,
            harness,
            executable,
            directory: directory.map(std::path::PathBuf::from),
            method: method.to_string(),
            params: params.clone(),
        });
        HarnessAnswer::Pending(call_id)
    }

    /// Park a door reply until its net-thread answer lands, and sweep it if
    /// the answer never does.
    fn ipc_harness_park(&mut self, call_id: u64, reply: Reply, cx: &mut Context<Self>) {
        self.ipc_harness_calls.insert(call_id, reply);
        cx.spawn(async move |store, cx| {
            cx.background_executor().timer(HARNESS_CALL_TIMEOUT).await;
            let _ = store.update(cx, |store, _cx| store.ipc_harness_timeout(call_id));
        })
        .detach();
    }

    /// One net-thread answer for a parked door call.
    pub(super) fn ipc_harness_result(&mut self, call_id: u64, result: Result<Value, String>) {
        if let Some(reply) = self.ipc_harness_calls.remove(&call_id) {
            let _ = reply.send(result);
        }
    }

    fn ipc_harness_timeout(&mut self, call_id: u64) {
        if let Some(reply) = self.ipc_harness_calls.remove(&call_id) {
            let _ = reply.send(Err("timed out".into()));
        }
    }

    /// Answers the door `harness start` calls parked on one server start.
    pub(super) fn ipc_harness_started(&mut self, ws_key: &str, error: Option<String>) {
        let harness = if self.is_shared_key(ws_key) {
            crate::harness::shared_server_id()
        } else {
            crate::harness::harness_for_key(ws_key)
        };
        let Some(harness) = harness else {
            return;
        };
        let (answered, waiting): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.ipc_harness_start_calls)
                .into_iter()
                .partition(|(id, _)| id == &harness);
        self.ipc_harness_start_calls = waiting;
        for (_, call_id) in answered {
            let result = match &error {
                Some(error) => Err(format!("unavailable: server failed to start: {error}")),
                None => Ok(json!({ "ok": true })),
            };
            self.ipc_harness_result(call_id, result);
        }
    }

    /// Fails a harness's parked start calls when the harness stops first.
    fn fail_harness_start_calls(&mut self, harness: &HarnessId, detail: &str) {
        let (failed, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.ipc_harness_start_calls)
            .into_iter()
            .partition(|(id, _)| id == harness);
        self.ipc_harness_start_calls = waiting;
        for (_, call_id) in failed {
            self.ipc_harness_result(call_id, Err(format!("unavailable: {detail}")));
        }
    }

    fn ipc_harness_call_id(&mut self) -> u64 {
        let call_id = self.next_ipc_harness_call;
        self.next_ipc_harness_call = self.next_ipc_harness_call.wrapping_add(1).max(1);
        call_id
    }

    /// Broadcasts one typed event to door subscribers. The connection filters
    /// on the top-level harness/session/run keys before it forwards a frame.
    pub(super) fn ipc_broadcast_harness_event(&self, harness: &HarnessId, event: &HarnessEvent) {
        self.net.send(NetCommand::IpcBroadcast {
            line: harness_event_wire(harness, event).to_string(),
        });
    }

    /// Opens a workspace from the door or a launch arg: adds it when
    /// missing ("workspace yoksa bağlanır"), connects its server, selects it.
    pub fn open_workspace_at(
        &mut self,
        folder: &str,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let path = resolve_workspace(folder)?;
        if !self.workspaces.iter().any(|w| w.folder_path() == path) {
            self.add_workspace(&path, cx);
            if !self.workspaces.iter().any(|w| w.folder_path() == path) {
                return Err(format!("could not add workspace: {path}"));
            }
        }
        if !self.runtimes.contains_key(&path) {
            self.start_workspace(&path, cx);
        }
        self.select_workspace(&path, cx);
        self.ipc_push_status();
        Ok(json!({"workspace": path}))
    }

    fn ipc_new_chat(
        &mut self,
        workspace: &str,
        prefill: String,
        auto_send: bool,
        model: DoorModel,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        if let Some(harness) = &model.harness
            && !self.harness_enabled(harness)
        {
            return Err(format!("harness {} is disabled", harness.label()));
        }
        let path = resolve_workspace(workspace)?;
        self.open_workspace_at(&path, cx)?;
        self.new_chat(cx);
        let Some(tile_key) = self.focused_tile.clone() else {
            return Err("no chat tile".into());
        };
        // the picker's harness is applied before the model so switching
        // backends cannot leave a model the new harness never listed
        if let Some(harness) = model.harness.clone() {
            self.set_tile_harness(&tile_key, harness, cx);
        }
        self.ipc_seed_tile(&tile_key, prefill, model);
        if auto_send && self.runtimes.contains_key(&path) {
            // a draft send silently drops while the server is still starting;
            // the ServerStarted pickup covers that case for registered runs
            self.send_from_tile(&tile_key, cx);
        }
        Ok(json!({"tile": tile_key}))
    }

    fn ipc_seed_tile(&mut self, tile_key: &str, prefill: String, model: DoorModel) {
        let Some(selected_model) = model.model.filter(|model| !model.trim().is_empty()) else {
            if let Some(tile) = self.tile_by_key_mut(tile_key)
                && !prefill.is_empty()
            {
                tile.input = prefill;
            }
            return;
        };
        let variants = self.variants_by_model.clone();
        let prefer_highest = self.settings.default_variant_highest;
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            if !prefill.is_empty() {
                tile.input = prefill;
            }
            tile.selected_model = Some(selected_model);
            tile.apply_variant_list(&variants, prefer_highest);
            // an explicit pick wins over the "highest by default" setting
            if let Some(variant) = model.variant.filter(|variant| !variant.trim().is_empty()) {
                tile.selected_variant = Some(variant);
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one door request's fields, unpacked from the wire frame; a struct would only rename them"
    )]
    fn ipc_run_prompt(
        &mut self,
        run: u64,
        workspace: &str,
        prompt: String,
        model: DoorModel,
        background: bool,
        reply: Reply,
        cx: &mut Context<Self>,
    ) {
        let path = match resolve_workspace(workspace) {
            Ok(path) => path,
            Err(err) => {
                let _ = reply.send(Err(err));
                return;
            }
        };
        if background {
            self.ipc_run_prompt_background(run, &path, prompt, model, reply, cx);
        } else {
            self.ipc_run_prompt_visible(run, &path, prompt, model, reply, cx);
        }
    }

    /// The visible flow: reuse the new-chat flow, a real tile the user can
    /// watch; when the server is not up yet the ServerStarted pickup sends it.
    fn ipc_run_prompt_visible(
        &mut self,
        run: u64,
        path: &str,
        prompt: String,
        model: DoorModel,
        reply: Reply,
        cx: &mut Context<Self>,
    ) {
        if let Err(err) = self.ipc_new_chat(path, prompt, true, model, cx) {
            let _ = reply.send(Err(err));
            return;
        }
        let tile_key = self.focused_tile.clone().unwrap_or_default();
        self.ipc_runs.insert(
            run,
            IpcRun {
                workspace: path.to_string(),
                tile_key,
                session_id: None,
                background: false,
                reply,
            },
        );
        self.ipc_run_log(run, "prompt queued");
    }

    /// The silent flow: the same draft → send → session pipeline, but the
    /// draft tile is flagged hidden so it never renders, the workspace is
    /// joined without being selected, and nothing focuses. The session is
    /// retired when the turn settles (or the run is cancelled).
    fn ipc_run_prompt_background(
        &mut self,
        run: u64,
        path: &str,
        prompt: String,
        model: DoorModel,
        reply: Reply,
        cx: &mut Context<Self>,
    ) {
        if let Some(harness) = &model.harness
            && !self.harness_enabled(harness)
        {
            let _ = reply.send(Err(format!("harness {} is disabled", harness.label())));
            return;
        }
        if !self.workspaces.iter().any(|w| w.folder_path() == path)
            && !self.install_workspace(path, cx)
        {
            let _ = reply.send(Err(format!("could not add workspace: {path}")));
            return;
        }
        // the server spins up if it must; until ServerStarted lands the
        // parked-runs pickup sends the tile on its own
        if !self.runtimes.contains_key(path) {
            self.start_workspace(path, cx);
        }
        let tile_key = {
            // create_tile never focuses or touches the sidebar — new_chat's
            // visible fanfare is exactly what a background run leaves out
            let tile = self.create_tile(cx);
            tile.key.clone()
        };
        // the picker's harness is applied before the model so switching
        // backends cannot leave a model the new harness never listed
        if let Some(harness) = model.harness.clone() {
            self.set_tile_harness(&tile_key, harness, cx);
        }
        self.ipc_seed_tile(&tile_key, prompt, model);
        self.ipc_runs.insert(
            run,
            IpcRun {
                workspace: path.to_string(),
                tile_key: tile_key.clone(),
                session_id: None,
                background: true,
                reply,
            },
        );
        self.ipc_run_log(run, "prompt queued");
        self.send_from_tile(&tile_key, cx);
    }

    /// Tears a background run's chat down: the hidden tile leaves the store,
    /// every session map forgets the id, and the server-side session is
    /// deleted so no later list refresh can surface the run in the sidebar.
    /// Idempotent — several runs may watch one session.
    pub fn retire_background_run(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let ws_key = self.net_key_for_session(session_id);
        self.forget_session(session_id, cx);
        // the deleted id must survive the restart too, or dsh's list would
        // bring the retired background chat back
        self.save_state();
        if let Some(ws_key) = ws_key {
            self.net.send(NetCommand::Request {
                ws_key,
                request: ApiRequest::DeleteSession {
                    session_id: session_id.to_string(),
                },
            });
        }
    }

    fn ipc_cancel_run(&mut self, run: u64, cx: &mut Context<Self>) -> Result<Value, String> {
        let Some(entry) = self.ipc_runs.remove(&run) else {
            return Ok(json!({"cancelled": false}));
        };
        if let Some(session_id) = entry.session_id.clone() {
            if self.running_by_session.contains(&session_id) {
                let ws_key = self
                    .net_key_for_session(&session_id)
                    .unwrap_or_else(|| entry.workspace.clone());
                self.net.send(NetCommand::Request {
                    ws_key,
                    request: ApiRequest::Abort {
                        session_id: session_id.clone(),
                    },
                });
            }
            // the run entry is gone, so the idle watcher will not clean up
            // behind this chat: retire it here
            if entry.background {
                self.retire_background_run(&session_id, cx);
            }
        } else if entry.background {
            // still a parked draft: the tile holds the prompt and nothing else
            self.detach_tile(&entry.tile_key, cx);
        }
        // dropping the run's reply sender reads as "cancelled" on the wire
        Ok(json!({"cancelled": true}))
    }

    fn ipc_models(
        &mut self,
        harness: Option<HarnessId>,
        refresh: bool,
        reply: Reply,
        cx: &mut Context<Self>,
    ) {
        if let Some(harness) = &harness
            && !self.harness_enabled(harness)
        {
            let _ = reply.send(Err(format!("harness {} is disabled", harness.label())));
            return;
        }
        if refresh {
            // a refresh is a request to re-read the backend, not a cache
            // lookup: drop the "live" marker so the answer lands only after
            // the fresh catalog does, and a later plain request re-lists too
            match &harness {
                Some(harness) => {
                    self.models_live.remove(harness);
                }
                None => {
                    let _ = reply.send(Err("refresh requires a harness".into()));
                    return;
                }
            }
        } else {
            let known = match &harness {
                Some(harness) => self.models_live.contains(harness),
                None => !self.model_infos.is_empty(),
            };
            if known {
                let _ = reply.send(Ok(self.models_payload(harness.clone())));
                return;
            }
        }
        if let Some(harness) = harness.clone().filter(crate::harness::attached) {
            self.ensure_attached(&harness, cx);
            if self.models_live.contains(&harness) {
                let _ = reply.send(Ok(self.models_payload(Some(harness))));
            } else {
                let detail = self
                    .attached_status
                    .get(&harness)
                    .cloned()
                    .unwrap_or_else(|| {
                        format!("{} did not list models", crate::harness::label(&harness))
                    });
                let _ = reply.send(Err(format!("unavailable: {detail}")));
            }
            return;
        }
        if harness.as_ref().is_some_and(crate::harness::shared_server) {
            let shared = self.shared_key();
            let running = self.runtimes.contains_key(&shared);
            self.ensure_shared_server(cx);
            if running {
                self.net.send(NetCommand::Request {
                    ws_key: shared,
                    request: ApiRequest::ListModels,
                });
            }
        } else {
            let Ok(path) = resolve_workspace("general") else {
                let _ = reply.send(Err("general workspace unavailable".into()));
                return;
            };
            if !self.workspaces.iter().any(|w| w.folder_path() == path) {
                // install without selecting: a settings refresh must not move
                // the user's focused workspace
                self.install_workspace(&path, cx);
            }
            if self.runtimes.contains_key(&path) {
                self.net.send(NetCommand::Request {
                    ws_key: path,
                    request: ApiRequest::ListModels,
                });
            } else {
                // ServerStarted asks for the model list on its own
                self.start_workspace(&path, cx);
            }
        }
        self.ipc_models_waiters.push((harness, reply));
    }

    /// One `models` answer: the legacy flat key list plus the harness and
    /// variant metadata the settings pickers need. A requested harness only
    /// sees its own catalog; without one every known model is included.
    pub(super) fn models_payload(&self, harness: Option<HarnessId>) -> Value {
        let enabled = self.settings.enabled_harness_ids();
        models_payload(&self.model_infos, harness, &enabled)
    }

    /// The last assistant message of a run, as the run's answer.
    pub fn ipc_last_assistant_text(&self, tile_key: &str) -> String {
        self.tile_by_key(tile_key)
            .and_then(|tile| {
                tile.items
                    .iter()
                    .rev()
                    .find_map(|item| match item.as_ref() {
                        super::ChatItem::AssistantText { text, .. } => Some(text.clone()),
                        _ => None,
                    })
            })
            .unwrap_or_default()
    }

    fn ipc_status_workspaces(&self) -> Value {
        let workspaces: Vec<Value> = self
            .workspaces
            .iter()
            .map(|ws| {
                let sessions: Vec<Value> = ws
                    .state
                    .sessions
                    .iter()
                    .map(|sid| {
                        json!({
                            "id": sid,
                            "title": self.title_for(sid),
                            "running": self.running_by_session.contains(sid),
                        })
                    })
                    .collect();
                json!({
                    "path": ws.folder_path(),
                    "name": ws.name,
                    "general": ws.is_general(),
                    "connected": self.runtimes.contains_key(ws.folder_path()),
                    "running": ws.state.sessions.iter().filter(|sid| self.running_by_session.contains(*sid)).count(),
                    "sessions": sessions,
                })
            })
            .collect();
        Value::Array(workspaces)
    }

    /// Fails every run still parked on a workspace whose server just died
    /// before their session ever existed. A background run's parked tile
    /// leaves with it — nothing visible should linger behind a dead run.
    pub fn ipc_fail_parked_runs(&mut self, workspace: &str, error: &str, cx: &mut Context<Self>) {
        let doomed: Vec<u64> = self
            .ipc_runs
            .iter()
            .filter(|(_, run)| run.workspace == workspace && run.session_id.is_none())
            .map(|(run, _)| *run)
            .collect();
        for run in doomed {
            if let Some(entry) = self.ipc_runs.remove(&run) {
                let _ = entry.reply.send(Err(error.to_string()));
                if entry.background {
                    self.detach_tile(&entry.tile_key, cx);
                }
            }
        }
    }

    /// Fans the current status snapshot out to every connected door client.
    pub fn ipc_push_status(&mut self) {
        let frame =
            json!({"event": "status", "data": {"workspaces": self.ipc_status_workspaces()}});
        self.net.send(NetCommand::IpcBroadcast {
            line: frame.to_string(),
        });
    }

    pub fn ipc_run_log(&self, run: u64, line: &str) {
        let frame = json!({"event": "run-log", "run": run, "line": line});
        self.net.send(NetCommand::IpcBroadcast {
            line: frame.to_string(),
        });
    }
}

/// The wire shape of one `models` answer: the released flat `models` key list
/// plus the harness-qualified `catalog` (variants included) and the harness
/// options. Kept separate from [`Store`] so the contract is unit-testable.
fn harness_event_wire(harness: &HarnessId, event: &HarnessEvent) -> Value {
    let mut frame = json!({"event": "harness-event", "harness": harness.as_str()});
    let (kind, session, data) = match event {
        HarnessEvent::Connected => ("connected", None, json!({})),
        HarnessEvent::Reconnecting { attempt } => {
            ("reconnecting", None, json!({ "attempt": attempt }))
        }
        HarnessEvent::Disconnected { reason } => {
            ("disconnected", None, json!({ "reason": reason }))
        }
        HarnessEvent::Exited => ("exited", None, json!({})),
        HarnessEvent::Health(health) => (
            "health",
            None,
            json!({
                "state": match health.state {
                    crate::harness::HealthState::Starting => "starting",
                    crate::harness::HealthState::Ready => "ready",
                    crate::harness::HealthState::Unavailable => "unavailable",
                },
                "detail": health.detail,
            }),
        ),
        HarnessEvent::SessionCreated {
            session,
            title,
            parent,
            agent,
        } => (
            "session-created",
            Some(session.as_str()),
            json!({
                "title": title,
                "parent": parent.as_ref().map(SessionId::as_str),
                "agent": agent,
            }),
        ),
        HarnessEvent::SessionChanged {
            session,
            title,
            parent,
            agent,
        } => (
            "session-changed",
            Some(session.as_str()),
            json!({
                "title": title,
                "parent": parent.as_ref().map(SessionId::as_str),
                "agent": agent,
            }),
        ),
        HarnessEvent::SessionDeleted { session } => {
            ("session-deleted", Some(session.as_str()), json!({}))
        }
        HarnessEvent::SessionStatus { session, state } => (
            "session-status",
            Some(session.as_str()),
            json!({
                "state": match state {
                    SessionState::Idle => "idle",
                    SessionState::Busy => "busy",
                    SessionState::Retry => "retry",
                },
            }),
        ),
        HarnessEvent::MessageSnapshot { message } => (
            "message",
            Some(message.session.as_str()),
            json!({
                "id": message.id,
                "role": match message.role {
                    MessageRole::User => "user",
                    MessageRole::Assistant => "assistant",
                },
            }),
        ),
        HarnessEvent::PartSnapshot { part } => (
            "part",
            Some(part.session.as_str()),
            json!({
                "id": part.id,
                "message_id": part.message_id,
                "payload": part_payload_wire(&part.payload),
            }),
        ),
        HarnessEvent::PartRemoved { session, part_id } => (
            "part-removed",
            Some(session.as_str()),
            json!({"id": part_id}),
        ),
        HarnessEvent::Todos { session, todos } => (
            "todos",
            Some(session.as_str()),
            json!({
                "todos": todos
                    .iter()
                    .map(|todo| json!({
                        "content": todo.content,
                        "status": todo.status,
                        "priority": todo.priority,
                    }))
                    .collect::<Vec<_>>(),
            }),
        ),
        HarnessEvent::PermissionAsked { request } => (
            "permission-asked",
            Some(request.session.as_str()),
            json!({
                "id": request.id,
                "action": request.action,
                "resources": request.resources,
                "message": request.message,
                "diff": request.diff,
            }),
        ),
        HarnessEvent::PermissionReplied {
            session,
            request,
            decision,
        } => (
            "permission-replied",
            session.as_ref().map(SessionId::as_str),
            json!({
                "id": request,
                "decision": match decision {
                    crate::harness::PermissionDecision::Once => "once",
                    crate::harness::PermissionDecision::Always => "always",
                    crate::harness::PermissionDecision::Reject => "reject",
                },
            }),
        ),
        HarnessEvent::QuestionAsked { request } => (
            "question-asked",
            Some(request.session.as_str()),
            json!({
                "id": request.id,
                "title": request.title,
                "fields": request.fields.iter().map(|field| json!({
                    "id": field.id,
                    "title": field.title,
                    "kind": form_field_kind_wire(field.kind),
                    "required": field.required,
                    "options": field.options,
                })).collect::<Vec<_>>(),
            }),
        ),
        HarnessEvent::QuestionResolved {
            session,
            request,
            summary,
        } => (
            "question-resolved",
            session.as_ref().map(SessionId::as_str),
            json!({"id": request, "summary": summary}),
        ),
        HarnessEvent::CommandOutput { session, result } => (
            "command-output",
            Some(session.as_str()),
            match result {
                Ok(output) => json!({"ok": true, "text": output}),
                Err(error) => json!({"ok": false, "error": error}),
            },
        ),
        HarnessEvent::ModelsLoaded { models } => {
            ("models-loaded", None, json!({"count": models.len()}))
        }
        HarnessEvent::AgentsLoaded { agents } => {
            ("agents-loaded", None, json!({"count": agents.len()}))
        }
        HarnessEvent::CommandsLoaded {
            directory,
            commands,
        } => (
            "commands-loaded",
            None,
            json!({
                "directory": directory.as_ref().map(|path| path.to_string_lossy()),
                "count": commands.len(),
            }),
        ),
        HarnessEvent::SkillsLoaded { directory, skills } => (
            "skills-loaded",
            None,
            json!({
                "directory": directory.as_ref().map(|path| path.to_string_lossy()),
                "count": skills.len(),
            }),
        ),
        HarnessEvent::Error {
            session,
            message,
            retryable,
        } => (
            "error",
            session.as_ref().map(SessionId::as_str),
            json!({"message": message, "retryable": retryable}),
        ),
    };
    frame["type"] = json!(kind);
    if let Some(session) = session {
        frame["session"] = json!(session);
    }
    frame["data"] = data;
    frame
}

fn part_payload_wire(payload: &PartPayload) -> Value {
    match payload {
        PartPayload::Text(text) => json!({"kind": "text", "text": text}),
        PartPayload::Reasoning(text) => json!({"kind": "reasoning", "text": text}),
        PartPayload::Tool(tool) => json!({
            "kind": "tool",
            "name": tool.name,
            "state": match &tool.state {
                ToolState::Pending => json!({"kind": "pending"}),
                ToolState::Running => json!({"kind": "running"}),
                ToolState::Completed { output } => json!({"kind": "completed", "output": output}),
                ToolState::Failed { message } => json!({"kind": "failed", "message": message}),
            },
            "input": tool.input,
            "title": tool.title,
            "metadata": tool.metadata,
        }),
        PartPayload::StepStart => json!({"kind": "step-start"}),
        PartPayload::StepFinish => json!({"kind": "step-finish"}),
        PartPayload::File(file) => {
            json!({"kind": "file", "url": file.url, "filename": file.filename})
        }
    }
}

fn form_field_kind_wire(kind: FormFieldKind) -> &'static str {
    match kind {
        FormFieldKind::Text => "text",
        FormFieldKind::Number => "number",
        FormFieldKind::Integer => "integer",
        FormFieldKind::Boolean => "boolean",
        FormFieldKind::Multiselect => "multiselect",
        FormFieldKind::External => "external",
    }
}

fn harness_capabilities(capabilities: &HarnessCapabilities) -> Value {
    json!({
        "commands": capabilities.commands,
        "skills": capabilities.skills,
        "skill_activation": capabilities.skill_activation,
        "forms": capabilities.forms,
        "questions": capabilities.questions,
        "permissions": capabilities.permissions,
        "diffs": capabilities.diffs,
        "subagents": capabilities.subagents,
        "inbox": capabilities.inbox,
        "revert": capabilities.revert,
        "shell": capabilities.shell,
        "background_sessions": capabilities.background_sessions,
        "mcp_read": capabilities.mcp_read,
        "mcp_management": capabilities.mcp_management,
        "compaction": capabilities.compaction,
        "fork": capabilities.fork,
        "multi_location": capabilities.multi_location,
    })
}

fn models_payload(
    infos: &[crate::oc::ModelInfo],
    harness: Option<HarnessId>,
    enabled_ids: &[String],
) -> Value {
    let enabled: Vec<HarnessId> = enabled_ids
        .iter()
        .filter_map(|id| HarnessId::from_wire(id))
        .collect();
    let entries: Vec<&crate::oc::ModelInfo> = infos
        .iter()
        .filter(|model| enabled.contains(&model.harness))
        .filter(|model| {
            harness
                .as_ref()
                .is_none_or(|harness| &model.harness == harness)
        })
        .collect();
    let models: Vec<&str> = entries.iter().map(|model| model.key.as_str()).collect();
    let catalog: Vec<Value> = entries
        .iter()
        .map(|model| {
            json!({
                "key": model.key,
                "harness": model.harness,
                "variants": model.variants,
                "context_limit": model.context_limit,
            })
        })
        .collect();
    let harnesses: Vec<Value> = HarnessId::all()
        .iter()
        .filter(|harness| enabled.contains(harness))
        .map(|harness| {
            json!({
                "id": harness.as_str(),
                "label": harness.label(),
            })
        })
        .collect();
    json!({
        "models": models,
        "catalog": catalog,
        "harnesses": harnesses,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oc::ModelInfo;

    /// The ready half of a door answer; a parked answer panics.
    fn ready(answer: HarnessAnswer) -> Result<Value, String> {
        match answer {
            HarnessAnswer::Ready(result) => result,
            HarnessAnswer::Pending(call_id) => panic!("unexpected parked call {call_id}"),
        }
    }

    #[test]
    fn models_payload_scopes_the_catalog_and_keeps_harnesses() {
        let infos = vec![
            ModelInfo {
                key: "a/x".into(),
                harness: HarnessId::from("opencode"),
                variants: vec!["low".into(), "high".into()],
                context_limit: 100,
            },
            ModelInfo {
                key: "b/y".into(),
                harness: HarnessId::from("dsh"),
                variants: vec!["max".into()],
                context_limit: 0,
            },
        ];
        let enabled = vec!["opencode".to_string(), "dsh".to_string()];
        let dsh = models_payload(&infos, Some(HarnessId::from("dsh")), &enabled);
        assert_eq!(dsh["models"], json!(["b/y"]));
        assert_eq!(dsh["catalog"][0]["key"], "b/y");
        assert_eq!(dsh["catalog"][0]["harness"], "dsh");
        assert_eq!(dsh["catalog"][0]["variants"], json!(["max"]));
        assert_eq!(dsh["harnesses"][0]["id"], "opencode");
        assert_eq!(dsh["harnesses"][1]["label"], "DeepSeek (dsh)");

        let all = models_payload(&infos, None, &enabled);
        assert_eq!(all["models"], json!(["a/x", "b/y"]));
        assert_eq!(all["catalog"].as_array().unwrap().len(), 2);

        let only_opencode = vec!["opencode".to_string()];
        let disabled = models_payload(&infos, Some(HarnessId::from("dsh")), &only_opencode);
        assert_eq!(disabled["models"], json!([]));
        assert_eq!(disabled["harnesses"].as_array().unwrap().len(), 1);
    }

    /// The wands send `general`: it must name the built-in Tasks workspace,
    /// not a folder on disk (which would be worked as a repo path).
    #[test]
    fn the_general_name_is_the_tasks_workspace() {
        let _lock = crate::store::test_support::lock_home();
        let tasks = AppPaths::general_workspace_path()
            .to_string_lossy()
            .into_owned();
        assert_eq!(resolve_workspace("general").unwrap(), tasks);
        assert_eq!(resolve_workspace("").unwrap(), tasks);
        assert!(resolve_workspace("/no/such/folder/for-warp-agent").is_err());
    }

    #[gpui_kit::gpui::test]
    fn harness_metadata_is_enabled_only_and_carries_capabilities(cx: &mut gpui::TestAppContext) {
        let _lock = crate::store::test_support::lock_home();
        let _home = crate::store::test_support::HomeGuard::new();
        let store = cx.update(Store::for_probe);
        let metadata = store.update(cx, |store, cx| {
            ready(store.ipc_harness_request("metadata", &json!({}), cx)).expect("metadata")
        });
        let harnesses = metadata["harnesses"].as_array().expect("harnesses");
        let ids: Vec<&str> = harnesses
            .iter()
            .filter_map(|harness| harness["id"].as_str())
            .collect();
        assert_eq!(ids, ["opencode", "dsh"]);
        assert_eq!(harnesses[0]["available"], true);
        assert_eq!(harnesses[0]["capabilities"]["commands"], true);
        assert_eq!(harnesses[0]["capabilities"]["forms"], false);

        store.update(cx, |store, cx| {
            store
                .set_harness_enabled("dsh", false, cx)
                .expect("opencode stays enabled");
        });
        let metadata = store.update(cx, |store, cx| {
            ready(store.ipc_harness_request("metadata", &json!({}), cx)).expect("metadata")
        });
        assert_eq!(metadata["harnesses"].as_array().unwrap().len(), 1);
        let error = store.update(cx, |store, cx| {
            ready(store.ipc_harness_request("models", &json!({"harness": "dsh"}), cx))
        });
        assert!(matches!(error, Err(ref detail) if detail.contains("disabled")));
        let error = store.update(cx, |store, cx| {
            ready(store.ipc_harness_request("no-such-method", &json!({}), cx))
        });
        assert!(matches!(error, Err(ref detail) if detail.contains("unsupported")));
    }

    /// A door `harness` call for a method this ticket dispatches to the net
    /// thread leaves the reply parked under a call id; the result event lands
    /// it on the wire.
    #[gpui_kit::gpui::test]
    fn a_dispatched_method_parks_the_reply_until_the_net_thread_answers(
        cx: &mut gpui::TestAppContext,
    ) {
        let _lock = crate::store::test_support::lock_home();
        let _home = crate::store::test_support::HomeGuard::new();
        let store = cx.update(Store::for_probe);
        let (reply, mut received) = tokio::sync::oneshot::channel();
        store.update(cx, |store, cx| {
            store.handle_ipc(
                IpcCommand::Harness {
                    method: "permissions".into(),
                    params: json!({"harness": "opencode", "session": "s1"}),
                    reply,
                },
                cx,
            );
        });
        let call_id = store.update(cx, |store, _| {
            let call_id = *store
                .ipc_harness_calls
                .keys()
                .next()
                .expect("a parked call");
            store.ipc_harness_result(call_id, Ok(json!({"permissions": []})));
            call_id
        });
        assert!(call_id > 0);
        assert_eq!(
            received.try_recv().expect("the reply fires").unwrap()["permissions"],
            json!([])
        );
    }

    /// A net thread that never answers must not leave a door reply parked.
    #[gpui_kit::gpui::test]
    fn a_parked_harness_reply_times_out_instead_of_hanging(cx: &mut gpui::TestAppContext) {
        let _lock = crate::store::test_support::lock_home();
        let _home = crate::store::test_support::HomeGuard::new();
        let store = cx.update(Store::for_probe);
        let (reply, mut received) = tokio::sync::oneshot::channel();
        store.update(cx, |store, cx| {
            store.handle_ipc(
                IpcCommand::Harness {
                    method: "diff".into(),
                    params: json!({"harness": "opencode", "session": "s1"}),
                    reply,
                },
                cx,
            );
        });
        store.update(cx, |store, _| {
            let call_id = *store
                .ipc_harness_calls
                .keys()
                .next()
                .expect("a parked call");
            store.ipc_harness_timeout(call_id);
            assert!(store.ipc_harness_calls.is_empty());
        });
        assert_eq!(
            received.try_recv().expect("the reply fires").unwrap_err(),
            "timed out"
        );
    }

    /// Unknown ids and disabled harnesses are refused store-side, before any
    /// adapter or net thread is touched.
    #[gpui_kit::gpui::test]
    fn unknown_and_disabled_harnesses_are_refused_before_dispatch(cx: &mut gpui::TestAppContext) {
        let _lock = crate::store::test_support::lock_home();
        let _home = crate::store::test_support::HomeGuard::new();
        let store = cx.update(Store::for_probe);
        let error = store.update(cx, |store, cx| {
            ready(store.ipc_harness_request(
                "permissions",
                &json!({"harness": "no-such-harness", "session": "s1"}),
                cx,
            ))
            .unwrap_err()
        });
        assert_eq!(error, "unknown: harness no-such-harness");
        store.update(cx, |store, cx| {
            store
                .set_harness_enabled("dsh", false, cx)
                .expect("dsh off");
        });
        let error = store.update(cx, |store, cx| {
            ready(store.ipc_harness_request(
                "permissions",
                &json!({"harness": "dsh", "session": "s1"}),
                cx,
            ))
            .unwrap_err()
        });
        assert_eq!(error, "disabled: harness dsh is disabled");
    }

    /// The dispatched call carries the harness, its instance folder, the
    /// method and the untouched params to the net thread.
    #[gpui_kit::gpui::test]
    fn a_dispatch_names_the_harness_folder_method_and_params(cx: &mut gpui::TestAppContext) {
        let _lock = crate::store::test_support::lock_home();
        let _home = crate::store::test_support::HomeGuard::new();
        let (store, net) = cx.update(Store::for_probe_with_net);
        let answer = store.update(cx, |store, cx| {
            store.ipc_harness_request(
                "send_prompt",
                &json!({
                    "harness": "opencode",
                    "directory": "/tmp/door-ws",
                    "session": "s1",
                    "text": "hello",
                }),
                cx,
            )
        });
        assert!(matches!(answer, HarnessAnswer::Pending(_)));
        let command = std::iter::from_fn(|| net.try_recv().ok())
            .find_map(|command| match command {
                command @ NetCommand::HarnessCall { .. } => Some(command),
                _ => None,
            })
            .expect("a harness call on the wire");
        let NetCommand::HarnessCall {
            call_id,
            harness,
            method,
            directory,
            params,
            ..
        } = command
        else {
            panic!("expected a harness call");
        };
        assert!(call_id > 0);
        assert_eq!(harness.as_str(), "opencode");
        assert_eq!(method, "send_prompt");
        assert_eq!(
            directory.as_deref(),
            Some(std::path::Path::new("/tmp/door-ws"))
        );
        assert_eq!(params["session"], "s1");
        assert_eq!(params["text"], "hello");
    }

    /// `harness start` parks on the app's own server-start bookkeeping and
    /// answers when the runner reports the boot.
    #[gpui_kit::gpui::test]
    fn a_start_answers_when_the_server_started_fact_lands(cx: &mut gpui::TestAppContext) {
        let _lock = crate::store::test_support::lock_home();
        let home = crate::store::test_support::HomeGuard::new();
        let folder = home.home.join("door-workspace");
        std::fs::create_dir_all(&folder).expect("workspace");
        let folder = folder.to_string_lossy().to_string();
        let store = cx.update(Store::for_probe);
        store.update(cx, |store, cx| {
            store.add_workspace(&folder, cx);
        });
        let (reply, mut received) = tokio::sync::oneshot::channel();
        store.update(cx, |store, cx| {
            store.handle_ipc(
                IpcCommand::Harness {
                    method: "start".into(),
                    params: json!({"harness": "opencode", "directory": folder}),
                    reply,
                },
                cx,
            );
            assert!(store.ipc_harness_start_calls.len() == 1);
        });
        store.update(cx, |store, cx| {
            store.on_net_event(
                crate::oc::UiEvent::ServerStarted {
                    ws_key: folder.clone(),
                    result: Ok(()),
                },
                cx,
            );
        });
        assert_eq!(
            received.try_recv().expect("the reply fires").unwrap(),
            json!({"ok": true})
        );
    }

    /// Health reads the attached adapter's own state and the managed server's
    /// latest health poll, without starting anything.
    #[gpui_kit::gpui::test]
    fn health_reports_the_adapter_or_the_runtime_state(cx: &mut gpui::TestAppContext) {
        let _lock = crate::store::test_support::lock_home();
        let _home = crate::store::test_support::HomeGuard::new();
        let store = cx.update(Store::for_probe);
        let health = store.update(cx, |store, cx| {
            ready(store.ipc_harness_request("health", &json!({"harness": "dsh"}), cx))
                .expect("health")
        });
        assert_eq!(health["state"], "unavailable");
        assert_eq!(health["detail"], "not running");

        store.update(cx, |store, _| {
            store.runtimes.insert(
                "dsh".into(),
                crate::store::WorkspaceRuntime {
                    stream_status: crate::oc::StreamStatus::Connected,
                    healthy: Some(true),
                },
            );
        });
        let health = store.update(cx, |store, cx| {
            ready(store.ipc_harness_request("health", &json!({"harness": "dsh"}), cx))
                .expect("health")
        });
        assert_eq!(health["state"], "ready");

        // an attached harness answers from its adapter, not from a runtime
        store.update(cx, |store, cx| {
            store
                .set_harness_enabled("opencode-v2", true, cx)
                .expect("opencode-v2 on");
        });
        store.update(cx, |store, _| {
            store.attached.insert(
                HarnessId::new("opencode-v2"),
                Box::new(crate::harness::test_support::FakeHarness::new(
                    "opencode-v2",
                )),
            );
        });
        let health = store.update(cx, |store, cx| {
            ready(store.ipc_harness_request("health", &json!({"harness": "opencode-v2"}), cx))
                .expect("health")
        });
        assert_eq!(health["state"], "ready");
    }

    /// Stopping detaches an attached adapter and tears a managed runtime down
    /// through the same bookkeeping the app uses.
    #[gpui_kit::gpui::test]
    fn stop_detaches_attached_and_tears_managed_down(cx: &mut gpui::TestAppContext) {
        let _lock = crate::store::test_support::lock_home();
        let _home = crate::store::test_support::HomeGuard::new();
        let store = cx.update(Store::for_probe);
        store.update(cx, |store, cx| {
            store
                .set_harness_enabled("opencode-v2", true, cx)
                .expect("opencode-v2 on");
        });
        store.update(cx, |store, _| {
            store.attached.insert(
                HarnessId::new("opencode-v2"),
                Box::new(crate::harness::test_support::FakeHarness::new(
                    "opencode-v2",
                )),
            );
            store.runtimes.insert(
                "dsh".into(),
                crate::store::WorkspaceRuntime {
                    stream_status: crate::oc::StreamStatus::Connected,
                    healthy: Some(true),
                },
            );
        });
        for harness in ["opencode-v2", "dsh"] {
            let result = store.update(cx, |store, cx| {
                ready(store.ipc_harness_request("stop", &json!({"harness": harness}), cx))
                    .expect("stop")
            });
            assert_eq!(result, json!({"ok": true}));
        }
        store.read_with(cx, |store, _| {
            assert!(!store.attached.contains_key(&HarnessId::new("opencode-v2")));
            assert!(!store.runtimes.contains_key("dsh"));
        });
    }
}
