//! Store side of the DevTools door: every [`IpcCommand`] the net thread
//! relays lands here. Runs ride the normal chat flow — a real tile the user
//! can watch — with a watcher that answers the door when the turn idles.
//! A background run executes on the same pipeline but holds a hidden tile:
//! the UI never renders it, no workspace is selected, and when the turn
//! settles the tile and its session are retired so nothing surfaces.

use gpui::Context;
use serde_json::{Value, json};

use super::{Store, normalize_dir};
use crate::harness::{
    FormFieldKind, HarnessCapabilities, HarnessEvent, HarnessId, HarnessRegistry, MessageRole,
    PartPayload, SessionId, SessionState, ToolState,
};
use crate::ipc::{DoorModel, IpcCommand, Reply};
use crate::oc::{ApiRequest, NetCommand};
use crate::paths::AppPaths;

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
            } => {
                let _ = reply.send(self.ipc_harness_request(&method, &params));
            }
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

    fn ipc_harness_request(&mut self, method: &str, params: &Value) -> Result<Value, String> {
        let harness = params.get("harness").and_then(Value::as_str);
        let require_enabled = |id: &str| -> Result<(), String> {
            if !HarnessRegistry::builtin().resolve(id).is_ok() {
                return Err(format!("unknown: harness {id}"));
            }
            if !self.settings.harness_enabled(id) {
                return Err(format!("disabled: harness {id} is disabled"));
            }
            Ok(())
        };
        match method {
            "metadata" | "list-harnesses" => Ok(json!({
                "harnesses": self.ipc_harness_metadata(),
            })),
            "status" => Ok(json!({
                "harnesses": self.ipc_harness_metadata(),
            })),
            "models" => {
                let id = harness.ok_or_else(|| "harness is required".to_string())?;
                require_enabled(id)?;
                let Some(kind) = HarnessId::from_wire(id) else {
                    return Err(format!(
                        "unavailable: harness {id} has no model catalog yet"
                    ));
                };
                Ok(self.models_payload(Some(kind)))
            }
            "sessions" => {
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
                Ok(json!({ "sessions": sessions }))
            }
            "session" => {
                let session = params
                    .get("session")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "session is required".to_string())?;
                let tile = self.tile_for_session(session);
                match tile {
                    Some(tile) => Ok(json!({
                        "id": session,
                        "title": self.title_for(session),
                        "harness": tile.harness.as_str(),
                        "running": self.running_by_session.contains(session),
                    })),
                    None => Err(format!("not-found: session {session}")),
                }
            }
            other => Err(format!("unsupported: harness method {other}")),
        }
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
        HarnessEvent::Disconnected { reason } => {
            ("disconnected", None, json!({ "reason": reason }))
        }
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
        let metadata = store.update(cx, |store, _| {
            store
                .ipc_harness_request("metadata", &json!({}))
                .expect("metadata")
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
        let metadata = store.update(cx, |store, _| {
            store
                .ipc_harness_request("metadata", &json!({}))
                .expect("metadata")
        });
        assert_eq!(metadata["harnesses"].as_array().unwrap().len(), 1);
        let error = store.update(cx, |store, _| {
            store.ipc_harness_request("models", &json!({"harness": "dsh"}))
        });
        assert!(matches!(error, Err(ref detail) if detail.contains("disabled")));
        let error = store.update(cx, |store, _| {
            store.ipc_harness_request("no-such-method", &json!({}))
        });
        assert!(matches!(error, Err(ref detail) if detail.contains("unsupported")));
    }
}
