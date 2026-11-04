use super::*;

impl Store {
    // ---- event intake (UiEvent from the net thread) ----

    pub fn on_net_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        match event {
            UiEvent::Log { ws_key, line } => {
                let name = self
                    .workspaces
                    .iter()
                    .find(|w| w.folder_path() == ws_key)
                    .map(|w| w.name.clone())
                    .unwrap_or_else(|| ws_key.clone());
                tracing::info!(target: "opencode", "[{name}] {line}");
            }
            UiEvent::ServerStarted { ws_key, result } => match result {
                Ok(()) => {
                    if ws_key == DSH_SERVER_KEY {
                        self.on_dsh_started(cx);
                        return;
                    }
                    // status starts Connected to match the pump's first attach
                    self.runtimes.insert(
                        ws_key.clone(),
                        WorkspaceRuntime {
                            stream_status: oc::StreamStatus::Connected,
                            healthy: None,
                        },
                    );
                    if let Some(ws) = self.workspace_by_path_mut(&ws_key) {
                        ws.is_running = true;
                    }
                    let is_selected = self.selected_workspace.as_deref() == Some(ws_key.as_str());
                    if is_selected {
                        self.is_server_running = true;
                    }
                    self.net.send(NetCommand::Request {
                        ws_key: ws_key.clone(),
                        request: ApiRequest::ListSessions,
                    });
                    // model/agent pickers feed off every running server
                    self.net.send(NetCommand::Request {
                        ws_key: ws_key.clone(),
                        request: ApiRequest::ListModels,
                    });
                    self.net.send(NetCommand::Request {
                        ws_key: ws_key.clone(),
                        request: ApiRequest::ListAgents,
                    });
                    // the / suggestion menu serves this workspace's commands
                    self.net.send(NetCommand::Request {
                        ws_key: ws_key.clone(),
                        request: ApiRequest::ListCommands,
                    });
                    // the MCP toggle needs the workspace's configured server names
                    self.net.send(NetCommand::Request {
                        ws_key: ws_key.clone(),
                        request: ApiRequest::ListMcpNames,
                    });
                    // a plain connect never reopens saved chats — only the
                    // banner's Restore button and explicit chat clicks do
                    // restores and opens that were waiting on this server start
                    let pending_restores: Vec<(String, Vec<String>)> = self
                        .pending_restores
                        .extract_if(.., |(path, _)| path == &ws_key)
                        .collect();
                    for (_, wanted) in pending_restores {
                        self.restore_open_tiles(&ws_key, wanted, cx);
                    }
                    let pending_opens: Vec<(String, String)> = self
                        .pending_opens
                        .extract_if(.., |(path, _)| path == &ws_key)
                        .collect();
                    for (_, session_id) in pending_opens {
                        self.net.send(NetCommand::Request {
                            ws_key: ws_key.clone(),
                            request: ApiRequest::GetMessages { session_id },
                        });
                    }
                    // door runs parked while this server spun up: their tile
                    // still holds the prompt
                    let parked_runs: Vec<String> = self
                        .ipc_runs
                        .values()
                        .filter(|run| run.workspace == ws_key && run.session_id.is_none())
                        .map(|run| run.tile_key.clone())
                        .collect();
                    for tile_key in parked_runs {
                        self.send_from_tile(&tile_key, cx);
                    }
                    // sends parked while this server's port was closed
                    self.drain_pending_session_sends(&ws_key, cx);
                    self.ipc_push_status();
                    self.repaint(cx);
                }
                Err(err) => {
                    let name = self
                        .workspaces
                        .iter()
                        .find(|w| w.folder_path() == ws_key)
                        .map(|w| w.name.clone())
                        .unwrap_or_else(|| ws_key.clone());
                    tracing::error!("server start failed for {name}: {err:#}");
                    // a models request parked on this start must not sit until
                    // the door's own timeout
                    let harness = if ws_key == DSH_SERVER_KEY {
                        HarnessKind::Dsh
                    } else {
                        HarnessKind::OpenCode
                    };
                    let waiting = self
                        .ipc_models_waiter
                        .as_ref()
                        .is_some_and(|(requested, _)| requested.is_none_or(|h| h == harness));
                    if waiting {
                        let (_, reply) = self
                            .ipc_models_waiter
                            .take()
                            .expect("a matching models waiter was checked just above");
                        let _ = reply.send(Err(format!("server failed to start: {err:#}")));
                    }
                    self.announce(&format!("Failed to start: {err:#}"), true, cx);
                    self.ipc_fail_parked_runs(
                        &ws_key,
                        &format!("server failed to start: {err:#}"),
                        cx,
                    );
                    self.ipc_push_status();
                }
            },
            UiEvent::ServerStopped => {}
            UiEvent::ServerCrashed { ws_key } => {
                // a user stop tore the workspace down meanwhile: stale report
                if !self.runtimes.contains_key(&ws_key) {
                    return;
                }
                let crashes = {
                    let now = Instant::now();
                    let entry = self
                        .crash_restarts
                        .entry(ws_key.clone())
                        .or_insert((0, now));
                    if now.duration_since(entry.1) >= CRASH_RESTART_RESET_AFTER {
                        entry.0 = 0; // the previous run was long ago: fresh ladder
                    }
                    entry.0 += 1;
                    entry.1 = now;
                    entry.0
                };
                let name = self
                    .workspaces
                    .iter()
                    .find(|w| w.folder_path() == ws_key)
                    .map(|w| w.name.clone())
                    .unwrap_or_else(|| ws_key.clone());
                // StopServer first: it tears down the net-side entry (dead
                // process handle, still-retrying pump) so StartServer can spawn
                self.net.send(NetCommand::StopServer {
                    ws_key: ws_key.clone(),
                });
                self.teardown_runtime(&ws_key);
                if crashes > MAX_CRASH_RESTARTS {
                    tracing::error!(
                        "server for {name} keeps crashing — giving up after {MAX_CRASH_RESTARTS} restarts"
                    );
                    self.announce(
                        &format!("{name}: server keeps crashing — stopped restarting."),
                        true,
                        cx,
                    );
                } else {
                    tracing::warn!(
                        "server for {name} crashed — restarting ({crashes}/{MAX_CRASH_RESTARTS})"
                    );
                    self.announce(
                        &format!(
                            "{name}: server crashed — restarting ({crashes}/{MAX_CRASH_RESTARTS})…"
                        ),
                        true,
                        cx,
                    );
                    self.start_server_for_key(&ws_key, cx);
                }
                self.ipc_push_status();
                self.repaint(cx);
            }
            UiEvent::Event { ws_key, event } => {
                self.handle_stream_event(&ws_key, &event, cx);
            }
            UiEvent::Ipc { command } => self.handle_ipc(command, cx),
            UiEvent::StreamStatus { ws_key, status } => {
                // transitions only — except Reconnecting, where every drop
                // rewrites the single combined note with its climbing count
                if let Some(runtime) = self.runtimes.get_mut(&ws_key) {
                    let update = match (&runtime.stream_status, &status) {
                        (
                            oc::StreamStatus::Reconnecting { .. },
                            oc::StreamStatus::Reconnecting { .. },
                        ) => true,
                        (current, incoming) => current != incoming,
                    };
                    if update {
                        runtime.stream_status = status;
                        match status {
                            oc::StreamStatus::Connected => {
                                // the server is back: its sessions may have
                                // changed (restart) while the stream was down
                                if ws_key == DSH_SERVER_KEY {
                                    self.refresh_dsh_lists();
                                } else {
                                    self.net.send(NetCommand::Request {
                                        ws_key: ws_key.clone(),
                                        request: ApiRequest::ListSessions,
                                    });
                                }
                                self.settle_reconnect_note(&ws_key, cx);
                            }
                            oc::StreamStatus::Reconnecting { attempt } => {
                                self.reconnect_note(
                                    &ws_key,
                                    &format!(
                                        "Connection lost — reconnecting… ({attempt}/{}).",
                                        oc::RECONNECT_MAX_ATTEMPTS
                                    ),
                                    true,
                                    cx,
                                );
                            }
                            oc::StreamStatus::Failed => {
                                self.reconnect_note(
                                    &ws_key,
                                    &format!(
                                        "Connection lost — gave up after {} attempts. Restart the server to retry.",
                                        oc::RECONNECT_MAX_ATTEMPTS
                                    ),
                                    true,
                                    cx,
                                );
                                self.reconnect_notes.remove(&ws_key);
                            }
                        }
                        self.repaint(cx);
                    }
                }
            }
            UiEvent::HealthChanged { ws_key, healthy } => {
                if let Some(runtime) = self.runtimes.get_mut(&ws_key)
                    && runtime.healthy != healthy
                {
                    runtime.healthy = healthy;
                    self.repaint(cx);
                }
            }
            UiEvent::SessionsLoaded {
                ws_key,
                dsh,
                result,
            } => match result {
                // the event names its own source: a dsh list (ListDshSessions)
                // carries the workspace folder and must never prune the same
                // workspace's opencode chats — and vice versa
                Ok(sessions) => self.apply_session_list(&ws_key, sessions, dsh, cx),
                Err(err) => tracing::debug!("session list refresh failed: {err:#}"),
            },
            UiEvent::MessagesLoaded {
                ws_key,
                session_id,
                result,
            } => match result {
                Ok(messages) => {
                    if self.tile_for_session(&session_id).is_some() {
                        return; // already opened meanwhile
                    }
                    if !self.runtimes.contains_key(&ws_key) {
                        return;
                    }
                    let title = self.title_for(&session_id);
                    // mint a key directly — create_tile() would push a phantom
                    // draft tile into open_tiles
                    let key = format!("t{}", self.next_tile_key);
                    self.next_tile_key += 1;
                    let mut tile = ChatTile::new(
                        key.clone(),
                        self.seed_model(),
                        self.seed_agent(),
                        self.seed_variant(),
                    );
                    tile.harness = self.session_harness(&session_id);
                    // the chat reopens with the model/variant its last turn ran
                    // on; the seeds only stand until the transcript says otherwise
                    if let Some((model, variant)) = session_chat_settings(&messages) {
                        tile.selected_model = Some(model);
                        tile.selected_variant =
                            Some(variant.unwrap_or_else(|| DEFAULT_VARIANT_ENTRY.into()));
                    }
                    tile.apply_variant_list(
                        &self.variants_by_model,
                        self.settings.default_variant_highest,
                    );
                    tile.session_id = Some(session_id.clone());
                    tile.title = title;
                    // restore the unsent composer draft, if one was saved
                    if let Some(draft) = self.ui.drafts.get(&session_id) {
                        tile.input = draft.clone();
                    }
                    {
                        let mut next_key = self.next_key;
                        tile.restore(&messages, self, &mut next_key);
                        self.next_key = next_key;
                    }
                    self.tile_by_session.insert(session_id.clone(), key.clone());
                    tile.rev = Some(cx.new(|_| ()));
                    self.open_tiles.push(tile);
                    // a tile just opened: the restore banner has served its purpose
                    self.show_restore_prompt = false;
                    self.flush_pending_permissions(&session_id, cx);
                    self.flush_pending_questions(&session_id, cx);
                    self.pull_pending_questions(&ws_key, &session_id);
                    self.show_queued_for(&key);
                    // opening a chat sets it active on its workspace and focuses it
                    if let Some(path) = self.workspace_by_session.get(&session_id).cloned()
                        && let Some(ws) = self.workspace_by_path_mut(&path)
                    {
                        ws.state.active_session = Some(session_id.clone());
                    }
                    self.focus_tile(&key, cx);
                    self.repaint(cx);
                }
                Err(err) => {
                    tracing::warn!("could not load chat {session_id}: {err:#}");
                    self.announce(&format!("Could not load chat: {err:#}"), true, cx);
                }
            },
            UiEvent::SessionCreated { ws_key, result } => match result {
                Ok(info) => {
                    // the draft tile that requested creation, tracked by pending_sends.
                    // FIFO: creates can be in flight from several drafts at once and
                    // replies carry no correlation id
                    let Some((draft_key, text, attachments)) = self.pending_sends.first().cloned()
                    else {
                        return;
                    };
                    self.pending_sends.remove(0);
                    // the create's harness rides its own FIFO; a dsh session is
                    // remembered so every later route picks the shared server
                    if self
                        .pending_create_harness
                        .front()
                        .is_some_and(|(folder, _)| folder == &ws_key)
                    {
                        let (_, kind) = self.pending_create_harness.pop_front().unwrap();
                        self.ui.harness_by_session.insert(info.id.clone(), kind);
                    }
                    self.workspace_by_session
                        .insert(info.id.clone(), ws_key.clone());
                    self.title_by_session
                        .insert(info.id.clone(), info.title.clone());
                    self.tile_by_session
                        .insert(info.id.clone(), draft_key.clone());
                    let title = info.title.clone();
                    if let Some(tile) = self.tile_by_key_mut(&draft_key) {
                        tile.session_id = Some(info.id.clone());
                        tile.title = title;
                        tile.auto_title_pending = true;
                    }
                    // a background run's chat stays out of the sidebar and the
                    // persisted session list entirely — the door client owns
                    // its result, and the run retires the session itself
                    let background = self
                        .tile_by_key(&draft_key)
                        .is_some_and(|tile| tile.background);
                    if background {
                        self.hidden_sessions.insert(info.id.clone());
                    }
                    if !background && let Some(ws) = self.workspace_by_path_mut(&ws_key) {
                        ws.state.sessions.insert(0, info.id.clone());
                    }
                    // only the selected workspace's sidebar mirror lives in
                    // self.sessions; other workspaces refill from their own
                    // session list (session.idle triggers that refresh)
                    if !background && self.selected_workspace.as_deref() == Some(ws_key.as_str()) {
                        self.sessions.push(SessionItem {
                            id: info.id.clone(),
                            title: info.title.clone(),
                            updated_ms: info.updated_ms,
                            created_ms: info.created_ms,
                            is_pinned: false,
                            is_selected: false,
                            is_running: false,
                            is_editing: false,
                            draft_title: String::new(),
                        });
                        self.sessions.sort_by_key(session_recency);
                    }
                    self.sync_workspace_chats(cx);
                    self.persist_tiles();
                    self.save_state();
                    // continue the parked send now that the session exists
                    self.send_core(&info.id, text, attachments, cx);
                    // door runs bound to this draft now have their session
                    let bound: Vec<u64> = self
                        .ipc_runs
                        .iter()
                        .filter(|(_, run)| run.tile_key == draft_key && run.session_id.is_none())
                        .map(|(run, _)| *run)
                        .collect();
                    for run in bound {
                        if let Some(entry) = self.ipc_runs.get_mut(&run) {
                            entry.session_id = Some(info.id.clone());
                        }
                        self.ipc_run_log(run, "session ready");
                    }
                }
                Err(err) => {
                    // drop the matching queued send or a later create would
                    // adopt it and fire the stale text into the new session
                    let dropped_key = self.pending_sends.first().map(|(key, _, _)| key.clone());
                    if !self.pending_sends.is_empty() {
                        self.pending_sends.remove(0);
                    }
                    if self
                        .pending_create_harness
                        .front()
                        .is_some_and(|(folder, _)| folder == &ws_key)
                    {
                        self.pending_create_harness.pop_front();
                    }
                    // a door run waiting on that draft fails with it (FIFO:
                    // the errored create belongs to the oldest pending send)
                    if let Some(key) = dropped_key {
                        let doomed: Vec<u64> = self
                            .ipc_runs
                            .iter()
                            .filter(|(_, run)| run.tile_key == key && run.session_id.is_none())
                            .map(|(run, _)| *run)
                            .collect();
                        for run in doomed {
                            if let Some(entry) = self.ipc_runs.remove(&run) {
                                let _ = entry
                                    .reply
                                    .send(Err(format!("could not create a session: {err:#}")));
                                // the draft this run parked on never became a
                                // chat: a background tile leaves with the run
                                if entry.background {
                                    self.detach_tile(&entry.tile_key, cx);
                                }
                            }
                        }
                    }
                    self.announce(&format!("Could not create a session: {err:#}"), true, cx);
                    self.repaint(cx);
                }
            },
            UiEvent::ForkSessionCreated {
                ws_key,
                context,
                result,
            } => match result {
                Ok(info) => {
                    // wire the new session in like SessionCreated does, minus the
                    // pending_send continuation — a fork never auto-sends, and the
                    // correlated event means no FIFO adoption of draft sends
                    self.workspace_by_session
                        .insert(info.id.clone(), ws_key.clone());
                    self.title_by_session
                        .insert(info.id.clone(), info.title.clone());
                    // mint a key directly — create_tile() would push a phantom
                    // draft tile into open_tiles (same as MessagesLoaded)
                    // the fork rode the create FIFO; a dsh fork is remembered
                    let fork_harness = if self
                        .pending_create_harness
                        .front()
                        .is_some_and(|(folder, _)| folder == &ws_key)
                    {
                        let (_, kind) = self.pending_create_harness.pop_front().unwrap();
                        self.ui.harness_by_session.insert(info.id.clone(), kind);
                        kind
                    } else {
                        self.session_harness(&info.id)
                    };
                    let key = format!("t{}", self.next_tile_key);
                    self.next_tile_key += 1;
                    let mut tile = ChatTile::new(
                        key.clone(),
                        self.seed_model(),
                        self.seed_agent(),
                        self.seed_variant(),
                    );
                    tile.harness = fork_harness;
                    tile.apply_variant_list(
                        &self.variants_by_model,
                        self.settings.default_variant_highest,
                    );
                    tile.session_id = Some(info.id.clone());
                    tile.title = info.title.clone();
                    tile.auto_title_pending = false; // the fork-point title stands
                    tile.input = context;
                    if let Some(ws) = self.workspace_by_path_mut(&ws_key) {
                        ws.set_expanded(true); // the forked chat belongs in a visible sidebar section
                        ws.state.sessions.insert(0, info.id.clone());
                    }
                    self.tile_by_session.insert(info.id.clone(), key.clone());
                    tile.rev = Some(cx.new(|_| ()));
                    self.open_tiles.push(tile);
                    if self.selected_workspace.as_deref() == Some(ws_key.as_str()) {
                        self.sessions.push(SessionItem {
                            id: info.id.clone(),
                            title: info.title.clone(),
                            updated_ms: info.updated_ms,
                            created_ms: info.created_ms,
                            is_pinned: false,
                            is_selected: false,
                            is_running: false,
                            is_editing: false,
                            draft_title: String::new(),
                        });
                        self.sessions.sort_by_key(session_recency);
                    }
                    self.show_restore_prompt = false; // a tile is about to be on screen
                    self.sync_workspace_chats(cx);
                    self.persist_tiles();
                    self.save_state();
                    self.focus_tile(&key, cx);
                }
                Err(err) => {
                    self.announce(&format!("Could not fork: {err:#}"), true, cx);
                    self.repaint(cx);
                }
            },
            UiEvent::SessionRenamed {
                session_id, result, ..
            } => match result {
                Ok(info) => {
                    let final_title = if info.title.is_empty() {
                        String::new()
                    } else {
                        info.title.clone()
                    };
                    self.title_by_session
                        .insert(session_id.clone(), final_title.clone());
                    if let Some(tile) = self.tile_for_session_mut(&session_id) {
                        tile.title = final_title.clone();
                    }
                    if let Some(item) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                        item.title = final_title;
                        item.updated_ms = info.updated_ms;
                        item.created_ms = info.created_ms;
                    }
                    self.sort_sessions();
                    self.sync_workspace_chats(cx);
                    self.repaint(cx);
                }
                Err(err) => {
                    tracing::warn!("rename failed: {err:#}");
                    self.announce(&format!("Rename failed: {err:#}"), true, cx);
                }
            },
            UiEvent::SessionDeleted {
                session_id, result, ..
            } => {
                if let Err(err) = result {
                    tracing::warn!("delete failed: {err:#}");
                    // the row is already gone locally (Delete unlinks at once)
                    self.announce(
                        &format!("Chat removed; the server could not delete it: {err:#}"),
                        true,
                        cx,
                    );
                } else {
                    self.forget_session(&session_id, cx);
                    self.save_state();
                }
                self.repaint(cx);
            }
            UiEvent::Aborted { session_id, result } => {
                if let Err(err) = result {
                    tracing::warn!("abort failed for {session_id}: {err:#}");
                    let note_key = self.next_key;
                    self.next_key += 1;
                    if let Some(tile) = self.tile_for_session_mut(&session_id) {
                        tile.items.push(Arc::new(ChatItem::SystemNote {
                            key: note_key,
                            text: format!("Abort failed: {err:#}"),
                            is_error: true,
                        }));
                    }
                } else {
                    self.user_stopped_by_session.insert(session_id.clone());
                    self.retry_due_by_session.remove(&session_id);
                    self.set_running(Some(&session_id), false);
                    self.set_subagent_running(&session_id, false);
                }
                self.repaint(cx);
            }
            UiEvent::PermissionReplied {
                permission_id,
                response,
                is_auto,
                result,
            } => {
                let label = match result {
                    Err(err) => Some(format!("reply failed: {err:#}")),
                    Ok(()) => match response.as_str() {
                        "once" => Some("allowed (once)".into()),
                        "always" => Some(if is_auto {
                            "auto-allowed (always)".into()
                        } else {
                            "always allowed".into()
                        }),
                        _ => Some("denied".into()),
                    },
                };
                if let Some(label) = label {
                    self.resolve_permission(&permission_id, label, is_auto);
                }
                self.repaint(cx);
            }
            UiEvent::QuestionsPulled {
                for_session,
                result,
                ..
            } => {
                if let Ok(pending) = result {
                    if for_session.is_empty() {
                        return;
                    }
                    if let Some(tile_key) = self.tile_by_session.get(&for_session).cloned() {
                        let owners: Vec<(oc::QuestionRequestInfo, String)> = pending
                            .into_iter()
                            .map(|request| {
                                let owner = self.owner_session_id(&request.session_id);
                                (request, owner)
                            })
                            .collect();
                        let mut next_key = self.next_key;
                        if let Some(tile) = self.tile_by_key_mut(&tile_key) {
                            for (request, owner) in owners {
                                if owner != for_session {
                                    continue;
                                }
                                if tile.items.iter().any(|i| matches!(i.as_ref(), ChatItem::Question(q) if q.id == request.id)) {
                                    continue;
                                }
                                tile.items
                                    .push(Arc::new(ChatItem::Question(build_question_item(
                                        &request,
                                        &mut next_key,
                                    ))));
                            }
                        }
                        self.next_key = next_key;
                    }
                }
                self.repaint(cx);
            }
            UiEvent::QuestionReplied {
                request_id,
                summary,
                result,
            } => {
                let resolution = match result {
                    Err(err) => format!("reply failed: {err:#}"),
                    Ok(()) => summary,
                };
                self.resolve_question(&request_id, &resolution);
                self.repaint(cx);
            }
            UiEvent::ModelsLoaded { harness, result } => {
                match result {
                    Ok(models) => {
                        self.models_live.insert(harness);
                        // merge: every running server contributes; existing entries stay for its own tiles
                        let restore: Vec<(String, Option<String>, Option<String>)> = self
                            .open_tiles
                            .iter()
                            .map(|t| {
                                (
                                    t.key.clone(),
                                    t.selected_model.clone(),
                                    t.selected_variant.clone(),
                                )
                            })
                            .collect();
                        // merge-dedup through a set: the model list runs to
                        // hundreds of entries and a Vec scan per model is O(n²)
                        let mut known: HashSet<String> = self.models.iter().cloned().collect();
                        self.model_infos.retain(|model| model.harness != harness);
                        for model in models {
                            self.variants_by_model
                                .insert(model.key.clone(), model.variants.clone());
                            self.context_limit_by_model
                                .insert(model.key.clone(), model.context_limit);
                            if known.insert(model.key.clone()) {
                                self.models.push(model.key.clone());
                            }
                            self.model_infos.push(model);
                        }
                        crate::settings::save_models_cache(&self.model_infos);
                        // tiles restored before this list arrived computed their
                        // context fraction against an empty map — re-resolve the
                        // limit and the % readout for every open tile now
                        let limits = self.context_limit_by_model.clone();
                        for tile in &mut self.open_tiles {
                            tile.context_limit = tile
                                .last_used_model
                                .as_ref()
                                .and_then(|key| lookup_context_limit(&limits, key))
                                .unwrap_or(0);
                            tile.update_context_text();
                        }
                        let known_models = self.models.clone();
                        let variants_by_model = self.variants_by_model.clone();
                        let prefer_highest = self.settings.default_variant_highest;
                        for (key, model, variant) in restore {
                            if let Some(tile) = self.tile_by_key_mut(&key) {
                                tile.selected_model = match model {
                                    Some(saved) if known_models.contains(&saved) => Some(saved),
                                    _ => Some(DEFAULT_MODEL_ENTRY.into()),
                                };
                                // the model list just landed: rebuild this tile's
                                // variant options around its model, keep a
                                // still-valid pick, else fall back to "(default)"
                                // (upgraded to highest when the setting asks)
                                tile.apply_variant_list(&variants_by_model, prefer_highest);
                                if let Some(saved_variant) = variant {
                                    // a saved "(default)" counts as unset: the
                                    // highest-default above already handled it
                                    if saved_variant != DEFAULT_VARIANT_ENTRY
                                        && tile.variants.contains(&saved_variant)
                                    {
                                        tile.selected_variant = Some(saved_variant);
                                    }
                                }
                            }
                        }
                        // the door's models command parked while a server spun
                        // up: the requested harness's catalog answers it (an
                        // un-harnessed request takes whichever list lands first)
                        let waiting = self
                            .ipc_models_waiter
                            .as_ref()
                            .is_some_and(|(requested, _)| requested.is_none_or(|h| h == harness));
                        if waiting {
                            let (requested, reply) = self
                                .ipc_models_waiter
                                .take()
                                .expect("a matching models waiter was checked just above");
                            let _ = reply.send(Ok(self.models_payload(requested)));
                        }
                    }
                    Err(err) => {
                        let waiting = self
                            .ipc_models_waiter
                            .as_ref()
                            .is_some_and(|(requested, _)| requested.is_none_or(|h| h == harness));
                        if waiting {
                            let (_, reply) = self
                                .ipc_models_waiter
                                .take()
                                .expect("a matching models waiter was checked just above");
                            let _ = reply.send(Err(format!(
                                "could not load {} models: {err:#}",
                                harness.label()
                            )));
                        }
                    }
                }
                self.repaint(cx);
            }
            UiEvent::AgentsLoaded { result, .. } => {
                if let Ok(agents) = result {
                    let restore: Vec<(String, Option<String>)> = self
                        .open_tiles
                        .iter()
                        .map(|t| (t.key.clone(), t.selected_agent.clone()))
                        .collect();
                    for agent in agents {
                        if agent.hidden || !matches!(agent.mode.as_str(), "primary" | "all") {
                            continue;
                        }
                        if !self.agents.contains(&agent.name) {
                            self.agents.push(agent.name);
                        }
                    }
                    let known_agents = self.agents.clone();
                    for (key, agent) in restore {
                        if let Some(tile) = self.tile_by_key_mut(&key) {
                            tile.selected_agent = match agent {
                                Some(saved) if known_agents.contains(&saved) => Some(saved),
                                _ => Some(DEFAULT_AGENT_ENTRY.into()),
                            };
                        }
                    }
                }
                self.repaint(cx);
            }
            UiEvent::CommandsLoaded { ws_key, result } => {
                if let Ok(commands) = result {
                    self.commands.insert(ws_key, commands);
                    self.repaint(cx);
                }
            }
            UiEvent::McpNamesLoaded { ws_key, result } => match result {
                Ok(names) => {
                    self.mcp_names.insert(ws_key.clone(), names);
                    // a fresh server starts every configured MCP server; keep
                    // the user's toggle-off (or the settings master switch) in
                    // force (best-effort, ignores 404)
                    if self.settings.mcp_disabled || self.mcp_disabled.contains(&ws_key) {
                        for name in self.mcp_names.get(&ws_key).cloned().unwrap_or_default() {
                            self.net.send(NetCommand::Request {
                                ws_key: ws_key.clone(),
                                request: ApiRequest::McpDisconnect { name },
                            });
                        }
                    }
                    self.repaint(cx);
                }
                Err(err) => tracing::debug!("mcp list refresh failed for {ws_key}: {err:#}"),
            },
            UiEvent::FilesFound {
                ws_key,
                query,
                result,
            } => {
                if let Ok(paths) = result {
                    self.file_results = Some(FileSuggest {
                        ws_key,
                        query,
                        paths,
                    });
                    self.repaint(cx);
                }
            }
            UiEvent::MessageSendFailed {
                session_id, error, ..
            } => {
                self.post_note_for_session(&session_id, &format!("Message failed: {error}"), true);
                self.repaint(cx);
            }
        }
    }
}
