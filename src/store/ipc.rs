//! Store side of the DevTools door: every [`IpcCommand`] the net thread
//! relays lands here. Runs ride the normal chat flow — a real tile the user
//! can watch — with a watcher that answers the door when the turn idles.

use gpui::Context;
use serde_json::{Value, json};

use super::{Store, normalize_dir};
use crate::ipc::{IpcCommand, Reply};
use crate::oc::{ApiRequest, NetCommand};
use crate::paths::AppPaths;

/// One door-run: a prompt DevTools wants executed and answered. Bound to a
/// draft tile key until the session exists, then to the session id.
pub struct IpcRun {
    pub workspace: String,
    pub tile_key: String,
    pub session_id: Option<String>,
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
                reply,
            } => {
                self.ipc_run_prompt(run, &workspace, prompt, model, reply, cx);
            }
            IpcCommand::CancelRun { run, reply } => {
                let _ = reply.send(self.ipc_cancel_run(run));
            }
            IpcCommand::Models { reply } => self.ipc_models(reply, cx),
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
        model: Option<String>,
        cx: &mut Context<Self>,
    ) -> Result<Value, String> {
        let path = self.ipc_resolve_workspace(workspace)?;
        self.open_workspace_at(&path, cx)?;
        self.new_chat(cx);
        let Some(tile_key) = self.focused_tile.clone() else {
            return Err("no chat tile".into());
        };
        self.ipc_seed_tile(&tile_key, prefill, model);
        if auto_send && self.runtimes.contains_key(&path) {
            // a draft send silently drops while the server is still starting;
            // the ServerStarted pickup covers that case for registered runs
            self.send_from_tile(&tile_key, cx);
        }
        Ok(json!({"tile": tile_key}))
    }

    fn ipc_seed_tile(&mut self, tile_key: &str, prefill: String, model: Option<String>) {
        if model.is_none() {
            if let Some(tile) = self.tile_by_key_mut(tile_key)
                && !prefill.is_empty()
            {
                tile.input = prefill;
            }
            return;
        }
        let variants = self.variants_by_model.clone();
        let prefer_highest = self.settings.default_variant_highest;
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            if !prefill.is_empty() {
                tile.input = prefill;
            }
            tile.selected_model = model;
            tile.apply_variant_list(&variants, prefer_highest);
        }
    }

    fn ipc_run_prompt(
        &mut self,
        run: u64,
        workspace: &str,
        prompt: String,
        model: Option<String>,
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
        // reuse the new-chat flow: a real, visible tile the run happens in;
        // when the server is not up yet the ServerStarted pickup sends it
        if let Err(err) = self.ipc_new_chat(&path, prompt, true, model, cx) {
            let _ = reply.send(Err(err));
            return;
        }
        let tile_key = self.focused_tile.clone().unwrap_or_default();
        self.ipc_runs.insert(
            run,
            IpcRun {
                workspace: path,
                tile_key,
                session_id: None,
                reply,
            },
        );
        self.ipc_run_log(run, "prompt queued");
    }

    fn ipc_cancel_run(&mut self, run: u64) -> Result<Value, String> {
        let Some(entry) = self.ipc_runs.remove(&run) else {
            return Ok(json!({"cancelled": false}));
        };
        if let Some(session_id) = entry.session_id.clone()
            && self.running_by_session.contains(&session_id)
        {
            let ws_key = self
                .net_key_for_session(&session_id)
                .unwrap_or_else(|| entry.workspace.clone());
            self.net.send(NetCommand::Request {
                ws_key,
                request: ApiRequest::Abort { session_id },
            });
        }
        // dropping the run's reply sender reads as "cancelled" on the wire
        Ok(json!({"cancelled": true}))
    }

    fn ipc_models(&mut self, reply: Reply, cx: &mut Context<Self>) {
        if !self.models.is_empty() {
            let _ = reply.send(Ok(json!({"models": self.models.clone()})));
            return;
        }
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
        self.ipc_models_waiter = Some(reply);
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
    /// before their session ever existed.
    pub fn ipc_fail_parked_runs(&mut self, workspace: &str, error: &str) {
        let doomed: Vec<u64> = self
            .ipc_runs
            .iter()
            .filter(|(_, run)| run.workspace == workspace && run.session_id.is_none())
            .map(|(run, _)| *run)
            .collect();
        for run in doomed {
            if let Some(entry) = self.ipc_runs.remove(&run) {
                let _ = entry.reply.send(Err(error.to_string()));
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
