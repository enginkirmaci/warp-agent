use super::*;

impl Store {
    pub fn tile_for_session_mut(&mut self, session_id: &str) -> Option<&mut ChatTile> {
        let key = self.tile_by_session.get(session_id)?.clone();
        self.tile_by_key_mut(&key)
    }

    pub(super) fn create_tile(&mut self, cx: &mut Context<Self>) -> &mut ChatTile {
        let key = format!("t{}", self.next_tile_key);
        self.next_tile_key += 1;
        let harness = self.settings.default_harness;
        let model = self.seed_model_for(harness);
        // a variant only makes sense next to a model; the model's own list
        // decides the rest when the tile is built
        let variant = model.as_ref().and_then(|_| self.seed_variant());
        let mut tile = ChatTile::new(key.clone(), model, self.seed_agent(), variant);
        tile.harness = harness;
        tile.apply_variant_list(
            &self.variants_by_model,
            self.settings.default_variant_highest,
        );
        tile.rev = Some(cx.new(|_| ()));
        // switching a draft to dsh warms the shared server so the picker can
        // list its catalog by the time it opens
        if harness == HarnessKind::Dsh {
            self.ensure_dsh(cx);
        }
        self.open_tiles.push(tile);
        self.tile_by_key_mut(&key).unwrap()
    }

    /// The seed model for a new chat: the last model used, then the settings
    /// default, but only when the chosen harness lists it. The UI remembers
    /// one last-used key across both backends, and an opencode key means
    /// nothing to dsh.
    fn seed_model_for(&self, harness: HarnessKind) -> Option<String> {
        seed_model_for_harness(
            self.ui.model.as_deref(),
            self.settings.default_model.as_deref(),
            &self.model_infos,
            harness,
        )
    }

    /// Moves untouched drafts from the old default harness to the new one.
    /// A draft that already carries a session, text or a manual harness pick
    /// is left alone, and a switch clears the model/variant that belonged to
    /// the old harness.
    pub fn retarget_untouched_drafts(
        &mut self,
        from: HarnessKind,
        to: HarnessKind,
        cx: &mut Context<Self>,
    ) {
        if from == to {
            return;
        }
        let mut changed = false;
        for tile in &mut self.open_tiles {
            if tile.background || !tile.is_untouched() || tile.harness != from {
                continue;
            }
            tile.harness = to;
            tile.selected_model = None;
            tile.selected_variant = None;
            changed = true;
        }
        if changed {
            if to == HarnessKind::Dsh {
                self.ensure_dsh(cx);
            }
            self.repaint(cx);
        }
    }

    /// New chat: focus the trailing untouched draft or open a new tile, then connect.
    pub fn new_chat(&mut self, cx: &mut Context<Self>) {
        let Some(ws_path) = self.selected_workspace.clone() else {
            return;
        };
        if let Some(ws) = self.workspace_by_path_mut(&ws_path) {
            ws.set_expanded(true); // the new chat belongs in a visible sidebar section
        }
        let untouched_last = self
            .open_tiles
            .last()
            .map(|t| t.is_untouched())
            .unwrap_or(false);
        self.show_restore_prompt = false; // a tile is about to be on screen
        if untouched_last {
            let key = self.open_tiles.last().unwrap().key.clone();
            self.focus_tile(&key, cx);
        } else {
            let key = {
                let tile = self.create_tile(cx);
                tile.key.clone()
            };
            self.focus_tile(&key, cx);
        }
        // a new chat is an explicit intent to talk: connect the server if it isn't up
        if !self.runtimes.contains_key(&ws_path) {
            self.start_workspace(&ws_path, cx);
        }
        self.repaint(cx);
    }

    /// Switches a draft's harness. Only drafts: a chat with a session is
    /// bound to the server it was created on.
    pub fn set_tile_harness(
        &mut self,
        tile_key: &str,
        harness: HarnessKind,
        cx: &mut Context<Self>,
    ) {
        if let Some(tile) = self.tile_by_key_mut(tile_key)
            && tile.session_id.is_none()
            && tile.harness != harness
        {
            tile.harness = harness;
            // the two catalogs name different models (dsh's deepseek-flash is
            // not opencode's deepseek-v4.1-flash) — a draft must not carry a
            // model the new harness never listed; back to server default
            tile.selected_model = None;
            tile.selected_variant = None;
        }
        // switching a draft to dsh warms the shared server so the model
        // picker can list its catalog by the time the pill opens
        if harness == HarnessKind::Dsh {
            self.ensure_dsh(cx);
        }
        let _ = cx;
    }

    pub fn new_chat_for_workspace(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.selected_workspace.as_deref() != Some(path) {
            self.select_workspace(path, cx);
        }
        self.new_chat(cx);
    }

    pub fn focus_tile(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.focused_tile.as_deref() == Some(key) {
            return;
        }
        for tile in &mut self.open_tiles {
            tile.is_focused = tile.key == key;
        }
        self.focused_tile = Some(key.to_string());
        // record the focused chat only on the workspace it belongs to
        let session = self.tile_by_key(key).and_then(|t| t.session_id.clone());
        let owner = session
            .as_ref()
            .and_then(|sid| self.workspace_by_session.get(sid).cloned());
        if let Some(ws) = self.selected_workspace_mut() {
            match owner {
                Some(path) if path == ws.folder_path() => ws.state.active_session = session,
                None => ws.state.active_session = None,
                _ => {}
            }
        }
        self.update_sidebar_selection();
        self.persist_tiles();
        self.save_state();
        self.repaint(cx);
    }

    /// Closing a tile only un-tiles it; the chat stays in the sidebar and keeps running in the background.
    pub fn close_tile(&mut self, key: &str, cx: &mut Context<Self>) {
        let was_focused = self.focused_tile.as_deref() == Some(key);
        self.detach_tile(key, cx);
        if !was_focused {
            self.persist_tiles();
            self.save_state();
        }
        self.repaint(cx);
    }

    pub(super) fn detach_tile(&mut self, key: &str, cx: &mut Context<Self>) {
        let session = self
            .open_tiles
            .iter()
            .find(|t| t.key == key)
            .and_then(|t| t.session_id.clone());
        if let Some(sid) = session {
            self.tile_by_session.remove(&sid);
            self.forget_saved_tile(&sid);
        }
        self.open_tiles.retain(|t| t.key != key);
        if self.focused_tile.as_deref() == Some(key) {
            self.focused_tile = self.open_tiles.last().map(|t| t.key.clone());
            for tile in &mut self.open_tiles {
                tile.is_focused = self.focused_tile.as_deref() == Some(tile.key.as_str());
            }
            if let Some(focus) = self.focused_tile.clone() {
                self.focus_tile(&focus, cx);
            }
        }
    }

    fn forget_saved_tile(&mut self, session_id: &str) {
        let Some(path) = self.workspace_by_session.get(session_id).cloned() else {
            return;
        };
        if let Some(ws) = self.workspace_by_path_mut(&path) {
            ws.state.open_sessions.retain(|id| id != session_id);
            if ws.state.active_session.as_deref() == Some(session_id) {
                ws.state.active_session = None;
            }
        }
    }

    /// Drafts without content are dropped once real chats are restored.
    pub(super) fn prune_untouched_drafts(&mut self, cx: &mut Context<Self>) {
        if self.tile_by_session.is_empty() {
            return;
        }
        let untouched: Vec<String> = self
            .open_tiles
            .iter()
            .filter(|t| t.is_untouched())
            .map(|t| t.key.clone())
            .collect();
        for key in untouched {
            if self.open_tiles.len() == 1 {
                break;
            }
            self.open_tiles.retain(|t| t.key != key);
            if self.focused_tile.as_deref() == Some(&key) {
                self.focused_tile = None;
            }
        }
        let _ = cx;
    }

    pub(super) fn show_queued_for(&mut self, tile_key: &str) {
        let Some(session_id) = self
            .tile_by_key(tile_key)
            .and_then(|t| t.session_id.clone())
        else {
            return;
        };
        let keys: Vec<usize> = self
            .queue
            .iter()
            .filter(|e| e.session_id == session_id)
            .map(|e| e.key)
            .collect();
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            for key in keys {
                if !tile.queue_rows.contains(&key) {
                    tile.queue_rows.push(key);
                }
            }
        }
    }

    // ---- sending ----

    pub fn send_from_tile(&mut self, tile_key: &str, cx: &mut Context<Self>) {
        let (text, attachment_count) = {
            let Some(tile) = self.tile_by_key(tile_key) else {
                return;
            };
            (tile.input.trim().to_string(), tile.attachments.len())
        };
        if text.is_empty() && attachment_count == 0 {
            return;
        }
        if let Some(key) = self.tile_net_key_or(tile_key) {
            self.touch_server_activity(&key);
        }
        // a leading `/name` stays as typed; send_core routes it to the
        // server's /command endpoint, which does the template expansion
        // an existing chat talks to its own workspace's server; a draft to the selected one
        let Some(tile) = self.tile_by_key(tile_key) else {
            return;
        };
        let existing_session = tile.session_id.clone();
        let is_dsh_draft = existing_session.is_none() && tile.harness == HarnessKind::Dsh;
        // a draft talks to the selected workspace's server — except a
        // background run's draft, which belongs to the workspace its door run
        // named; the run's registration answers that lookup
        let draft_ws = self.draft_workspace(tile_key);
        if let Some(session_id) = &existing_session {
            if !self.has_runtime(session_id) {
                // the server idled out: restart it and flow the send when
                // ServerStarted lands, instead of dead-ending the composer
                let Some(net_key) = self.net_key_for_session(session_id) else {
                    return;
                };
                let attachments = self
                    .tile_by_key_mut(tile_key)
                    .map(|tile| std::mem::take(&mut tile.attachments))
                    .unwrap_or_default();
                if let Some(tile) = self.tile_by_key_mut(tile_key) {
                    tile.input.clear();
                    tile.history_index = 0;
                }
                self.record_prompt(&text);
                self.pending_session_sends.push((
                    net_key.clone(),
                    session_id.clone(),
                    text,
                    attachments,
                ));
                self.start_server_for_key(&net_key, cx);
                self.repaint(cx);
                return;
            }
        } else if is_dsh_draft {
            // a dsh draft needs the shared dsh server, not the workspace's
            if !self.runtimes.contains_key(DSH_SERVER_KEY) {
                let folder = draft_ws.clone().unwrap_or_default();
                let attachments =
                    std::mem::take(&mut self.tile_by_key_mut(tile_key).unwrap().attachments);
                self.pending_dsh_drafts.push((
                    tile_key.to_string(),
                    text.clone(),
                    attachments,
                    folder,
                ));
                let Some(tile) = self.tile_by_key_mut(tile_key) else {
                    return;
                };
                tile.input.clear();
                tile.history_index = 0;
                self.record_prompt(&text);
                self.ensure_dsh(cx);
                self.repaint(cx);
                return;
            }
        } else if draft_ws
            .as_ref()
            .is_none_or(|ws| !self.runtimes.contains_key(ws))
        {
            // no server for the draft's workspace yet: the prompt stays in
            // the tile until ServerStarted's parked-run pickup sends it
            return;
        }

        let attachments: Vec<AttachmentItem> =
            std::mem::take(&mut self.tile_by_key_mut(tile_key).unwrap().attachments);
        let tile = self.tile_by_key_mut(tile_key).unwrap();
        let text_to_record = text.clone();
        tile.input.clear();
        tile.history_index = 0;
        self.record_prompt(&text_to_record);

        let Some(tile) = self.tile_by_key(tile_key) else {
            return;
        };
        let is_busy = tile.is_busy(self);
        let session_id = tile.session_id.clone();
        if let Some(sid) = &session_id {
            // the composer text is going out; a saved draft would resurrect it
            self.ui.drafts.remove(sid);
        }

        if is_busy {
            // park the message on this chat; it is sent when that chat goes idle
            let entry = QueueEntry {
                session_id: session_id.clone().unwrap(),
                text: text.clone(),
                attachments,
                key: self.next_key,
            };
            self.next_key += 1;
            let key = entry.key;
            self.queue.push(entry);
            if let Some(tile) = self.tile_by_key_mut(tile_key) {
                tile.queue_rows.push(key);
            }
            self.repaint(cx);
            return;
        }

        if session_id.is_none() {
            // create the session first; the send continues when the reply lands
            let ws_key = draft_ws.expect("the draft's server was checked above");
            let harness = self
                .tile_by_key(tile_key)
                .map(|tile| tile.harness)
                .unwrap_or_default();
            self.pending_sends
                .push((tile_key.to_string(), text, attachments));
            match harness {
                HarnessKind::Dsh => {
                    self.pending_create_harness
                        .push_back((ws_key.clone(), HarnessKind::Dsh));
                    self.net.send(NetCommand::Request {
                        ws_key: DSH_SERVER_KEY.into(),
                        request: ApiRequest::CreateDshSession {
                            folder: ws_key,
                            title: DRAFT_TITLE.into(),
                        },
                    });
                }
                HarnessKind::OpenCode => {
                    self.pending_create_harness
                        .push_back((ws_key.clone(), HarnessKind::OpenCode));
                    self.net.send(NetCommand::Request {
                        ws_key,
                        request: ApiRequest::CreateSession {
                            title: DRAFT_TITLE.into(),
                        },
                    });
                }
            }
            return;
        }
        self.send_core(&session_id.unwrap(), text, attachments, cx);
    }

    pub(super) fn send_core(
        &mut self,
        session_id: &str,
        text: String,
        attachments: Vec<AttachmentItem>,
        cx: &mut Context<Self>,
    ) {
        let tile_key = self.tile_by_session.get(session_id).cloned();
        let tile = tile_key.as_deref().and_then(|key| self.tile_by_key(key));
        let model = tile
            .and_then(|t| t.selected_model.clone())
            .filter(|m| m != DEFAULT_MODEL_ENTRY);
        let variant = tile
            .and_then(|t| t.selected_variant.clone())
            .filter(|v| v != DEFAULT_VARIANT_ENTRY);
        let agent = tile
            .and_then(|t| t.selected_agent.clone())
            .filter(|a| a != DEFAULT_AGENT_ENTRY);
        // a leading `/name args` matching a server command/skill goes to the
        // /command route: the server expands the template and honors the
        // command's own agent/model config (subtask skills included). dsh
        // commands are keyed under the shared server, and its send path
        // ignores the distinction (dsh expands slash text itself).
        let command = self
            .net_key_for_session(session_id)
            .and_then(|ws_key| self.commands.get(&ws_key))
            .and_then(|commands| parse_slash_command(&text, commands));
        self.dispatch_send(
            session_id,
            PromptPayload {
                text,
                attachments,
                model,
                variant,
                agent,
                command,
                attempts: 0,
            },
            cx,
        );
    }

    /// Puts a prompt on the wire: optimistic transcript items, running state,
    /// auto title, then the net command. The auto-retry path replays a stored
    /// [`PromptPayload`] through here unchanged.
    fn dispatch_send(&mut self, session_id: &str, payload: PromptPayload, cx: &mut Context<Self>) {
        let Some(ws_key) = self.net_key_for_session(session_id) else {
            return;
        };
        let tile_key = self.tile_by_session.get(session_id).cloned();
        // verbatim copy for the auto-retry path (attachment bytes are Arc'd)
        self.last_prompt_by_session
            .insert(session_id.to_string(), payload.clone());
        let sent_at = Instant::now();
        if let Some(tile_key) = &tile_key {
            let mut items: Vec<Arc<ChatItem>> = Vec::new();
            if !payload.text.is_empty() {
                items.push(Arc::new(ChatItem::UserMessage {
                    key: self.next_key,
                    text: payload.text.clone(),
                    created_ms: 0, // rendered with no time label, like "now"
                }));
                self.next_key += 1;
            }
            for attachment in &payload.attachments {
                if attachment.is_image() {
                    items.push(Arc::new(ChatItem::Image {
                        key: format!("local{}", self.next_key),
                        file_name: attachment.file_name.clone(),
                        png: Arc::clone(&attachment.data),
                        created_ms: 0,
                    }));
                } else {
                    items.push(Arc::new(ChatItem::SystemNote {
                        key: self.next_key,
                        text: format!("Attachment: {}", attachment.file_name),
                        is_error: false,
                    }));
                }
                self.next_key += 1;
            }
            if let Some(tile) = self.tile_by_key_mut(tile_key) {
                tile.items.extend(items);
            }
        }
        self.set_running(Some(session_id), true);
        self.turn_start_by_session
            .insert(session_id.to_string(), sent_at);
        let title_seed = if !payload.text.is_empty() {
            payload.text.clone()
        } else {
            payload
                .attachments
                .first()
                .map(|a| a.file_name.clone())
                .unwrap_or_else(|| DRAFT_TITLE.into())
        };
        self.auto_title(session_id, &title_seed);
        self.net.send(NetCommand::SendMessage {
            ws_key,
            session_id: session_id.to_string(),
            text: payload.text,
            model: payload.model,
            variant: payload.variant,
            agent: payload.agent,
            attachments: payload
                .attachments
                .into_iter()
                .map(|a| (a.file_name, a.mime, a.data))
                .collect(),
            command: payload.command,
        });
        self.repaint(cx);
    }

    /// Steer: jump the queue and interrupt the running turn so this prompt runs next.
    pub fn steer_queued(&mut self, queue_key: usize, cx: &mut Context<Self>) {
        let Some(index) = self.queue.iter().position(|e| e.key == queue_key) else {
            return;
        };
        let entry = self.queue[index].clone();
        let Some(tile_key) = self.tile_by_session.get(&entry.session_id).cloned() else {
            return;
        };
        if !self.is_busy(&entry.session_id) {
            // nothing running anymore: dispatch it right away
            self.queue.remove(index);
            if let Some(tile) = self.tile_by_key_mut(&tile_key) {
                tile.queue_rows.retain(|k| *k != queue_key);
            }
            self.send_core(&entry.session_id, entry.text, entry.attachments, cx);
            return;
        }
        // put it in front; the running turn's idle then sends this one first
        if index > 0 {
            self.queue.remove(index);
            self.queue.insert(0, entry.clone());
        }
        self.abort_tile(&tile_key, cx);
    }

    /// Edit: pull a queued prompt back into the composer, dropping it from the queue.
    pub fn edit_queued(&mut self, queue_key: usize, cx: &mut Context<Self>) {
        let Some(index) = self.queue.iter().position(|e| e.key == queue_key) else {
            return;
        };
        let entry = self.queue.remove(index);
        if let Some(tile_key) = self.tile_by_session.get(&entry.session_id).cloned()
            && let Some(tile) = self.tile_by_key_mut(&tile_key)
        {
            tile.queue_rows.retain(|k| *k != queue_key);
            tile.set_input_direct(entry.text.clone());
            for attachment in entry.attachments {
                tile.attachments.push(attachment);
            }
        }
        self.repaint(cx);
    }

    pub fn remove_queued(&mut self, queue_key: usize, cx: &mut Context<Self>) {
        self.queue.retain(|e| e.key != queue_key);
        for tile in &mut self.open_tiles {
            tile.queue_rows.retain(|k| *k != queue_key);
        }
        self.repaint(cx);
    }

    pub(super) fn drop_queue(&mut self, session_ids: &[String]) {
        let keys: Vec<usize> = self
            .queue
            .iter()
            .filter(|e| session_ids.contains(&e.session_id))
            .map(|e| e.key)
            .collect();
        for key in keys {
            self.remove_queued_inner(key);
        }
    }

    fn remove_queued_inner(&mut self, key: usize) {
        self.queue.retain(|e| e.key != key);
        for tile in &mut self.open_tiles {
            tile.queue_rows.retain(|k| *k != key);
        }
    }

    /// Sends the oldest queued message after that chat finishes its turn; called on session.idle.
    pub(super) fn try_drain_queue(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if !self.has_runtime(session_id) || self.is_busy(session_id) {
            return; // busy: a steered prompt already took over; the rest stay queued
        }
        let Some(index) = self.queue.iter().position(|e| e.session_id == session_id) else {
            return;
        };
        let entry = self.queue.remove(index);
        if let Some(tile_key) = self.tile_by_session.get(session_id).cloned()
            && let Some(tile) = self.tile_by_key_mut(&tile_key)
        {
            tile.queue_rows.retain(|k| *k != entry.key);
        }
        self.send_core(session_id, entry.text, entry.attachments, cx);
    }

    /// Marks a session for an automatic resend when its next idle lands.
    /// Only prompts this app sent qualify, only up to [`MAX_AUTO_RETRY`]
    /// attempts, and an explicitly stopped turn never retries.
    pub(super) fn arm_auto_retry(&mut self, session_id: &str) -> Option<u32> {
        if self.user_stopped_by_session.contains(session_id) {
            return None;
        }
        let payload = self.last_prompt_by_session.get_mut(session_id)?;
        if payload.attempts >= MAX_AUTO_RETRY {
            return None;
        }
        payload.attempts += 1;
        self.retry_due_by_session.insert(session_id.to_string());
        Some(payload.attempts)
    }

    /// The retry timer fired: put the stored prompt back on the wire.
    /// A prompt the user sent meanwhile or a stop press wins over the retry.
    pub(super) fn fire_auto_retry(&mut self, session_id: &str, cx: &mut Context<Self>) {
        self.retry_due_by_session.remove(session_id);
        if self.is_busy(session_id) || self.user_stopped_by_session.contains(session_id) {
            return;
        }
        if !self.has_runtime(session_id) {
            self.post_note_for_session(
                session_id,
                "Auto-retry cancelled — this chat's workspace server is stopped.",
                true,
            );
            return;
        }
        let Some(payload) = self.last_prompt_by_session.get(session_id).cloned() else {
            return;
        };
        self.dispatch_send(session_id, payload, cx);
    }

    pub fn abort_tile(&mut self, tile_key: &str, cx: &mut Context<Self>) {
        let Some(tile) = self.tile_by_key(tile_key) else {
            return;
        };
        let Some(session_id) = tile.session_id.clone() else {
            return;
        };
        if !self.is_busy(&session_id) || !self.has_runtime(&session_id) {
            return;
        }
        let ws_key = self.net_key_for_session(&session_id).unwrap();
        self.net.send(NetCommand::Request {
            ws_key,
            request: ApiRequest::Abort {
                session_id: session_id.clone(),
            },
        });
        let _ = cx;
    }

    pub fn stop_running(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(ws_key) = self.net_key_for_session(session_id) else {
            return;
        };
        self.retry_due_by_session.remove(session_id);
        self.net.send(NetCommand::Request {
            ws_key,
            request: ApiRequest::Abort {
                session_id: session_id.to_string(),
            },
        });
        self.repaint(cx);
    }

    fn auto_title(&mut self, session_id: &str, text: &str) {
        let Some(tile) = self.tile_for_session(session_id) else {
            return;
        };
        if !tile.auto_title_pending {
            return;
        }
        if let Some(tile) = self.tile_for_session_mut(session_id) {
            tile.auto_title_pending = false;
        }
        let title = derive_title(text);
        let Some(ws_key) = self.net_key_for_session(session_id) else {
            return;
        };
        self.net.send(NetCommand::Request {
            ws_key,
            request: ApiRequest::RenameSession {
                session_id: session_id.to_string(),
                title,
            },
        });
    }

    // ---- prompt history ----

    fn record_prompt(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.prompt_history.retain(|p| p != text);
        self.prompt_history.push(text.to_string());
        if self.prompt_history.len() > 100 {
            let excess = self.prompt_history.len() - 100;
            self.prompt_history.drain(..excess);
        }
    }

    pub fn prompt_history_count(&self) -> usize {
        self.prompt_history.len()
    }

    pub fn peek_prompt_history(&self, steps_from_end: usize) -> String {
        self.prompt_history[self.prompt_history.len() - steps_from_end - 1].clone()
    }

    /// Cycle depth for a tile: its own sent messages when it has any, else the
    /// global history so a fresh draft still recalls something.
    fn history_count_for(&self, tile_key: &str) -> usize {
        let own = self
            .tile_by_key(tile_key)
            .map(|t| t.sent_prompt_count())
            .unwrap_or(0);
        if own > 0 {
            own
        } else {
            self.prompt_history_count()
        }
    }

    /// `steps_from_end`-th prompt back for a tile (0 = newest), per the same rule.
    fn peek_history_for(&self, tile_key: &str, steps_from_end: usize) -> Option<String> {
        let tile = self.tile_by_key(tile_key)?;
        if tile.sent_prompt_count() > 0 {
            return Some(tile.peek_sent_prompt(steps_from_end));
        }
        if steps_from_end < self.prompt_history.len() {
            return Some(self.peek_prompt_history(steps_from_end));
        }
        None
    }

    /// Terminal-style Up/Down cycling through previously sent prompts; returns false to let the caret move.
    /// The source is the tile's OWN sent user messages, most-recent first; only
    /// a tile with none of its own yet falls back to the global prompt history.
    pub fn cycle_prompt_history(&mut self, tile_key: &str, up: bool) -> bool {
        let count = self.history_count_for(tile_key);
        let (current_index, current_input, current_draft) = {
            let Some(tile) = self.tile_by_key(tile_key) else {
                return false;
            };
            (
                tile.history_index,
                tile.input.clone(),
                tile.history_draft.clone(),
            )
        };
        let mut new_index = current_index;
        let text;
        if up {
            if current_index == 0 {
                if count == 0 {
                    return false;
                }
                new_index = 1;
                text = self.peek_history_for(tile_key, 0);
            } else if current_index < count {
                new_index = current_index + 1;
                text = self.peek_history_for(tile_key, current_index);
            } else {
                text = self.peek_history_for(tile_key, current_index - 1);
            }
        } else {
            if current_index == 0 {
                return false;
            }
            new_index = current_index - 1;
            text = if new_index == 0 {
                Some(current_draft.clone())
            } else {
                self.peek_history_for(tile_key, new_index - 1)
            };
        }
        let Some(text) = text else { return false };
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            if current_index == 0 && up && tile.history_draft.is_empty() {
                tile.history_draft = current_input;
            }
            tile.history_index = new_index;
            tile.set_input_direct(text);
        }
        true
    }

    /// Re-prompt: sends a previous user message's text again in this chat by
    /// staging it into the composer first, so the queue rules, prompt-history
    /// record and "done in Xs" note all ride the regular send path.
    pub fn reprompt(&mut self, tile_key: &str, text: &str, cx: &mut Context<Self>) {
        if text.trim().is_empty() {
            return;
        }
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            tile.input = text.to_string();
            tile.reset_history();
        }
        self.send_from_tile(tile_key, cx);
    }

    /// Fork from here: create a NEW session in the same workspace and open it
    /// as a tile whose composer holds the transcript up to and including the
    /// chosen user message (`stop_key`) as a quoted context block. The serve
    /// API has no parts-append endpoint — its only message POST starts a model
    /// turn — so the seed waits in the composer for the user to send manually.
    pub fn fork_from_message(&mut self, tile_key: &str, stop_key: usize, cx: &mut Context<Self>) {
        let Some(tile) = self.tile_by_key(tile_key) else {
            return;
        };
        let Some(session_id) = tile.session_id.clone() else {
            return;
        };
        let Some(ws_key) = self.net_key_for_session(&session_id) else {
            return;
        };
        if !self.runtimes.contains_key(&ws_key) {
            self.tile_note(
                tile_key,
                "This chat's workspace server is stopped — start it to fork.",
                true,
                cx,
            );
            return;
        }
        let stop_text = tile.peek_sent_prompt_key(stop_key);
        if stop_text.trim().is_empty() {
            return;
        }
        let title = derive_title(&stop_text);
        let context = fork_transcript(tile, stop_key, &self.title_for(&session_id));
        let folder = self
            .effective_workspace_path(&session_id)
            .unwrap_or_default();
        let harness = self.session_harness(&session_id);
        self.pending_create_harness
            .push_back((folder.clone(), harness));
        let request = match harness {
            HarnessKind::Dsh => ApiRequest::CreateDshForkSession {
                folder,
                title,
                context,
            },
            HarnessKind::OpenCode => ApiRequest::CreateForkSession { title, context },
        };
        self.net.send(NetCommand::Request { ws_key, request });
        let _ = cx;
    }

    /// Shift+Tab in the composer: next entry in the agent list, wrapping;
    /// the "(default build)" sentinel participates in the cycle like any other entry.
    pub fn cycle_agent(&mut self, tile_key: &str) {
        if !self.has_agents() {
            return;
        }
        let current = self
            .tile_by_key(tile_key)
            .and_then(|t| t.selected_agent.clone());
        let index = current
            .as_ref()
            .and_then(|name| self.agents.iter().position(|a| a == name))
            .map(|i| i as isize)
            .unwrap_or(-1);
        let next = self.agents[((index + 1) as usize) % self.agents.len()].clone();
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            tile.selected_agent = Some(next);
        }
        self.persist_composer();
    }
}

/// The seed model for a new chat: the last model used, then the settings
/// default, but only when the chosen harness lists it. The UI remembers one
/// last-used key across both backends, and an opencode key means nothing to
/// dsh (and vice versa). An empty catalog cannot validate, so it keeps the
/// candidate rather than dropping it.
fn seed_model_for_harness(
    last_used: Option<&str>,
    default: Option<&str>,
    infos: &[crate::oc::ModelInfo],
    harness: HarnessKind,
) -> Option<String> {
    let fits = |key: &str| {
        infos.is_empty()
            || infos
                .iter()
                .any(|model| model.harness == harness && model.key == key)
    };
    last_used
        .filter(|model| fits(model))
        .or_else(|| default.filter(|model| fits(model)))
        .map(str::to_string)
}

#[cfg(test)]
mod seed_tests {
    use super::seed_model_for_harness;
    use crate::oc::ModelInfo;
    use crate::settings::HarnessKind;

    fn info(key: &str, harness: HarnessKind) -> ModelInfo {
        ModelInfo {
            key: key.into(),
            harness,
            variants: Vec::new(),
            context_limit: 0,
        }
    }

    #[test]
    fn the_last_used_model_only_wins_when_the_new_harness_lists_it() {
        let infos = [
            info("opencode/a", HarnessKind::OpenCode),
            info("deepseek/b", HarnessKind::Dsh),
        ];

        // same harness: the last-used key stays
        assert_eq!(
            seed_model_for_harness(
                Some("opencode/a"),
                Some("opencode/c"),
                &infos,
                HarnessKind::OpenCode
            )
            .as_deref(),
            Some("opencode/a")
        );
        // switched to dsh: the opencode key is dropped, the dsh default wins
        assert_eq!(
            seed_model_for_harness(
                Some("opencode/a"),
                Some("deepseek/b"),
                &infos,
                HarnessKind::Dsh
            )
            .as_deref(),
            Some("deepseek/b")
        );
        // neither candidate is a dsh model: the server default is safer
        assert_eq!(
            seed_model_for_harness(Some("opencode/a"), None, &infos, HarnessKind::Dsh),
            None
        );
        // an empty catalog cannot validate; keep the candidate
        assert_eq!(
            seed_model_for_harness(Some("opencode/a"), None, &[], HarnessKind::Dsh).as_deref(),
            Some("opencode/a")
        );
    }
}
