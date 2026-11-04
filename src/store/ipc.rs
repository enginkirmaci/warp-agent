//! Store side of the DevTools door: every [`IpcCommand`] the net thread
//! relays lands here. Runs ride the normal chat flow — a real tile the user
//! can watch — with a watcher that answers the door when the turn idles.
//! A background run executes on the same pipeline but holds a hidden tile:
//! the UI never renders it, no workspace is selected, and when the turn
//! settles the tile and its session are retired so nothing surfaces.

use gpui::Context;
use serde_json::{Value, json};

use super::{Store, normalize_dir};
use crate::ipc::{DoorModel, IpcCommand, Reply};
use crate::oc::{ApiRequest, DSH_SERVER_KEY, NetCommand};
use crate::paths::AppPaths;
use crate::settings::HarnessKind;

/// One door-run: a prompt DevTools wants executed and answered. Bound to a
/// draft tile key until the session exists, then to the session id.
pub struct IpcRun {
    pub workspace: String,
    pub tile_key: String,
    pub session_id: Option<String>,
    pub background: bool,
    pub reply: Reply,
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
            IpcCommand::Models { harness, reply } => self.ipc_models(harness, reply, cx),
            IpcCommand::Status { reply } => {
                let _ = reply.send(Ok(json!({"workspaces": self.ipc_status_workspaces()})));
            }
        }
    }

    /// Opens a workspace from the door or a launch arg: adds it when
    /// missing ("workspace yoksa bağlanır"), connects its server, selects it.
    pub fn open_workspace_at(
        &mut self,
        folder: &str,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let path = self.ipc_resolve_workspace(folder)?;
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

    /// "general" names the built-in workspace; anything else is a folder
    /// path that must exist on disk.
    fn ipc_resolve_workspace(&self, workspace: &str) -> Result<String, String> {
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

    fn ipc_new_chat(
        &mut self,
        workspace: &str,
        prefill: String,
        auto_send: bool,
        model: DoorModel,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let path = self.ipc_resolve_workspace(workspace)?;
        self.open_workspace_at(&path, cx)?;
        self.new_chat(cx);
        let Some(tile_key) = self.focused_tile.clone() else {
            return Err("no chat tile".into());
        };
        // the picker's harness is applied before the model so switching
        // backends cannot leave a model the new harness never listed
        if let Some(harness) = model.harness {
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
        let path = match self.ipc_resolve_workspace(workspace) {
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
        if let Some(harness) = model.harness {
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

    fn ipc_models(&mut self, harness: Option<HarnessKind>, reply: Reply, cx: &mut Context<Self>) {
        let known = match harness {
            Some(harness) => self.models_live.contains(&harness),
            None => !self.model_infos.is_empty(),
        };
        if known {
            let _ = reply.send(Ok(self.models_payload(harness)));
            return;
        }
        if harness == Some(HarnessKind::Dsh) {
            let running = self.runtimes.contains_key(DSH_SERVER_KEY);
            self.ensure_dsh(cx);
            if running {
                self.net.send(NetCommand::Request {
                    ws_key: DSH_SERVER_KEY.into(),
                    request: ApiRequest::ListModels,
                });
            }
        } else {
            let Ok(path) = self.ipc_resolve_workspace("general") else {
                let _ = reply.send(Err("general workspace unavailable".into()));
                return;
            };
            if !self.workspaces.iter().any(|w| w.folder_path() == path) {
                self.add_workspace(&path, cx);
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
        self.ipc_models_waiter = Some((harness, reply));
    }

    /// One `models` answer: the legacy flat key list plus the harness and
    /// variant metadata the settings pickers need. A requested harness only
    /// sees its own catalog; without one every known model is included.
    pub(super) fn models_payload(&self, harness: Option<HarnessKind>) -> Value {
        models_payload(&self.model_infos, harness)
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
fn models_payload(infos: &[crate::oc::ModelInfo], harness: Option<HarnessKind>) -> Value {
    let entries: Vec<&crate::oc::ModelInfo> = infos
        .iter()
        .filter(|model| harness.is_none_or(|harness| model.harness == harness))
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
    let harnesses: Vec<Value> = HarnessKind::ALL
        .iter()
        .map(|harness| {
            json!({
                "id": harness.label(),
                "label": harness.option_label(),
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
                harness: HarnessKind::OpenCode,
                variants: vec!["low".into(), "high".into()],
                context_limit: 100,
            },
            ModelInfo {
                key: "b/y".into(),
                harness: HarnessKind::Dsh,
                variants: vec!["max".into()],
                context_limit: 0,
            },
        ];
        let dsh = models_payload(&infos, Some(HarnessKind::Dsh));
        assert_eq!(dsh["models"], json!(["b/y"]));
        assert_eq!(dsh["catalog"][0]["key"], "b/y");
        assert_eq!(dsh["catalog"][0]["harness"], "dsh");
        assert_eq!(dsh["catalog"][0]["variants"], json!(["max"]));
        assert_eq!(dsh["harnesses"][0]["id"], "opencode");
        assert_eq!(dsh["harnesses"][1]["label"], "DeepSeek (dsh)");

        let all = models_payload(&infos, None);
        assert_eq!(all["models"], json!(["a/x", "b/y"]));
        assert_eq!(all["catalog"].as_array().unwrap().len(), 2);
    }
}
