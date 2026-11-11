use super::*;

impl Store {
    // ---- stream events (HandleEvent port) ----

    pub(super) fn handle_stream_event(
        &mut self,
        ws_key: &str,
        evt: &serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let type_ = evt
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let data = evt.get("properties").unwrap_or(evt);
        match type_ {
            "server.connected" => {
                let harness = if ws_key == DSH_SERVER_KEY {
                    "dsh"
                } else {
                    "opencode"
                };
                self.announce_for_key(
                    ws_key,
                    &format!("Connected to the {harness} event stream."),
                    cx,
                );
            }
            "session.created" | "session.updated" => {
                if let Some(info) = data.get("info") {
                    let tracked_id = info.get("id").and_then(serde_json::Value::as_str);
                    let parent_id = info.get("parentID").and_then(serde_json::Value::as_str);
                    let agent = info.get("agent").and_then(serde_json::Value::as_str);
                    if self.track_session_info(tracked_id, parent_id, agent) {
                        // subagent tracking feeds the sidebar spinners too; rare
                        self.refresh_tile_agents();
                        self.repaint(cx);
                    }
                }
                // metadata-only session pings changed nothing visible: the
                // dirty set stays empty and the flush is a no-op
                self.notify_stream(cx);
                return;
            }
            "message.updated" => {
                if let Some(tile_key) = self.tile_from_data(data, ws_key) {
                    self.with_tile(&tile_key, |tile, store, _| {
                        tile.on_message_updated(data, store)
                    });
                    self.mark_stream_dirty(tile_key, cx);
                }
                return;
            }
            "message.part.updated" => {
                // the stream re-sends unchanged part snapshots constantly while a
                // turn runs; only an actual transcript change may repaint
                if let Some(tile_key) = self.tile_from_data(data, ws_key) {
                    let changed = self
                        .with_tile(&tile_key, |tile, _store, _| tile.on_part_updated(data))
                        .unwrap_or(false);
                    if changed {
                        self.mark_stream_dirty(tile_key, cx);
                    }
                }
                return;
            }
            "permission.asked" => self.on_permission_asked(data, false, cx),
            "permission.v2.asked" => self.on_permission_asked(data, true, cx),
            "permission.replied" | "permission.v2.replied" => self.on_permission_replied(data),
            "question.asked" => self.on_question_asked(data, cx),
            "question.replied" | "question.rejected" => self.on_question_resolved(data),
            "session.status" => {
                let status = read_status(data);
                let status_session = read_session_id(data);
                let busy = status == "busy" || status == "retry";
                if let Some(sid) = status_session {
                    if self.subagent_by_session.contains_key(sid) {
                        self.set_subagent_running(sid, busy);
                    } else {
                        self.set_running(Some(sid), busy);
                    }
                }
            }
            "session.idle" => {
                let idle_session = read_session_id(data);
                if let Some(sid) = idle_session {
                    self.set_running(Some(sid), false);
                    self.set_subagent_running(sid, false);
                    self.note_turn_duration(sid, cx);
                    // a background run's turn settling must not pull its
                    // (about to be deleted) session into the sidebar lists
                    let background = self
                        .tile_for_session(sid)
                        .is_some_and(|tile| tile.background);
                    // refresh the chat list of the workspace the chat belongs to
                    if background {
                        // retired below, with its session deleted server-side
                    } else if self.session_harness(sid) == HarnessKind::Dsh {
                        self.touch_server_activity(DSH_SERVER_KEY);
                        self.refresh_dsh_lists();
                        // the first completed turn is when dsh's agent-scoped
                        // command/skill catalog is certainly ready
                        self.net.send(NetCommand::Request {
                            ws_key: DSH_SERVER_KEY.into(),
                            request: ApiRequest::ListDshCommands {
                                session_id: Some(sid.to_string()),
                            },
                        });
                    } else if let Some(idle_path) = self.workspace_by_session.get(sid).cloned() {
                        self.touch_server_activity(&idle_path);
                        if self.selected_workspace.as_deref() == Some(idle_path.as_str()) {
                            self.net.send(NetCommand::Request {
                                ws_key: idle_path.clone(),
                                request: ApiRequest::ListSessions,
                            });
                        } else if self.runtimes.contains_key(&idle_path) {
                            self.net.send(NetCommand::Request {
                                ws_key: idle_path,
                                request: ApiRequest::ListSessions,
                            });
                        }
                    }
                    let sid = sid.to_string();
                    // a door run just finished: answer it with the last
                    // assistant message
                    let done: Vec<u64> = self
                        .ipc_runs
                        .iter()
                        .filter(|(_, run)| run.session_id.as_deref() == Some(sid.as_str()))
                        .map(|(run, _)| *run)
                        .collect();
                    for run in done {
                        if let Some(entry) = self.ipc_runs.remove(&run) {
                            let text = self.ipc_last_assistant_text(&entry.tile_key);
                            let _ = entry.reply.send(Ok(serde_json::json!({"text": text})));
                            if entry.background {
                                self.retire_background_run(&sid, cx);
                            }
                        }
                    }
                    if self.retry_due_by_session.remove(&sid) {
                        // the turn died retryable: re-send the stored prompt
                        // after a pause; the queue drains on the retry's idle
                        let delay = retry_backoff(
                            self.last_prompt_by_session
                                .get(&sid)
                                .map(|p| p.attempts)
                                .unwrap_or(1),
                        );
                        cx.spawn(async move |store, cx| {
                            cx.background_executor().timer(delay).await;
                            let _ = store.update(cx, |store, cx| store.fire_auto_retry(&sid, cx));
                        })
                        .detach();
                    } else {
                        self.try_drain_queue(&sid, cx);
                    }
                }
            }
            "todo.updated" => {
                if let Some(tile_key) = self.tile_from_data(data, ws_key) {
                    let spring = self
                        .with_tile(&tile_key, |tile, _store, _k| tile.on_todos_updated(data))
                        .unwrap_or(false);
                    if spring {
                        self.todos_spring = Some(tile_key);
                    }
                }
            }
            "message.part.removed" => {
                if let Some(tile_key) = self.tile_from_data(data, ws_key) {
                    self.with_tile(&tile_key, |tile, _store, _k| tile.on_part_removed(data));
                }
            }
            "session.error" => {
                let (message, retryable) = read_error_info(data);
                let error_session = read_session_id(data);
                if let Some(sid) = error_session {
                    let attempt = if retryable {
                        self.arm_auto_retry(sid)
                    } else {
                        None
                    };
                    // door runs on this chat: a retry keeps them waiting, a
                    // dead turn answers with the error
                    let watched: Vec<u64> = self
                        .ipc_runs
                        .iter()
                        .filter(|(_, run)| run.session_id.as_deref() == Some(sid))
                        .map(|(run, _)| *run)
                        .collect();
                    // a background chat posts nothing visible: its notes
                    // would land on the user's own tiles once the hidden
                    // tile retires, and the run's log already tells the door
                    let background = self
                        .tile_for_session(sid)
                        .is_some_and(|tile| tile.background);
                    for run in watched {
                        match attempt {
                            Some(attempt) => {
                                self.ipc_run_log(run, &format!("connection lost — retrying (attempt {attempt}/{MAX_AUTO_RETRY})"));
                            }
                            None => {
                                if let Some(entry) = self.ipc_runs.remove(&run) {
                                    let _ = entry.reply.send(Err(message.clone()));
                                    if entry.background
                                        && let Some(sid) = &entry.session_id
                                    {
                                        self.retire_background_run(sid, cx);
                                    }
                                }
                            }
                        }
                    }
                    if !background {
                        match attempt {
                            Some(attempt) => self.post_note_for_session(
                                sid,
                                &format!("Connection lost — re-sending the last prompt (attempt {attempt}/{MAX_AUTO_RETRY})…"),
                                false,
                            ),
                            None => self.post_note_for_session(sid, &message, true),
                        }
                    }
                } else {
                    self.announce(&message, true, cx);
                }
            }
            // an event type nothing renders must not repaint the window — the
            // bus emits plenty of these while a turn runs
            _ => return,
        }
        self.repaint(cx);
    }

    /// Repaint coalescer for the high-frequency stream arms (part and
    /// message updates): flush now when the last one is old enough,
    /// otherwise schedule a single deferred flush — a streaming transcript
    /// repaints at most once per STREAM_REPAINT_INTERVAL no matter how fast
    /// tokens arrive, and only the tiles that actually changed.
    fn notify_stream(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let since_last = self.stream_repaint_last.map(|t| now.duration_since(t));
        if since_last.is_none_or(|d| d >= STREAM_REPAINT_INTERVAL) {
            self.stream_repaint_last = Some(now);
            self.stream_repaint_scheduled = false;
            self.flush_stream(cx);
            return;
        }
        if self.stream_repaint_scheduled {
            return;
        }
        self.stream_repaint_scheduled = true;
        let wait = STREAM_REPAINT_INTERVAL - since_last.unwrap();
        cx.spawn(async move |store, cx| {
            cx.background_executor().timer(wait).await;
            let _ = store.update(cx, |store, cx| {
                store.stream_repaint_scheduled = false;
                store.stream_repaint_last = Some(Instant::now());
                store.flush_stream(cx);
            });
        })
        .detach();
    }

    /// Queue a tile into the coalesced stream flush.
    fn mark_stream_dirty(&mut self, key: String, cx: &mut Context<Self>) {
        if !self.stream_dirty.contains(&key) {
            self.stream_dirty.push(key);
        }
        self.notify_stream(cx);
    }

    /// Drain the stream-dirty set into per-tile revision bumps. Fires no
    /// store-level notify: unchanged tiles keep their cached subtree and the
    /// sidebar doesn't rebuild for token noise.
    fn flush_stream(&mut self, cx: &mut Context<Self>) {
        for key in std::mem::take(&mut self.stream_dirty) {
            self.touch_tile(&key, cx);
        }
    }

    fn tile_from_data(&self, data: &serde_json::Value, ws_key: &str) -> Option<String> {
        if let Some(id) = read_session_id(data) {
            return self.tile_by_session.get(id).cloned();
        }
        // events without a session id belong to a tile of the same workspace
        let focused = self
            .focused_tile
            .as_deref()
            .and_then(|key| self.tile_by_key(key))?;
        let focused_session = focused.session_id.as_ref()?;
        if self
            .workspace_by_session
            .get(focused_session)
            .map(|p| p == ws_key)
            .unwrap_or(false)
        {
            Some(focused.key.clone())
        } else {
            None
        }
    }

    /// Turn finished: drop a muted note telling how long the prompt took, send to final answer.
    /// A user-aborted turn says "stopped in" instead — it didn't finish.
    fn note_turn_duration(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let stopped = self.user_stopped_by_session.remove(session_id);
        let Some(start) = self.turn_start_by_session.remove(session_id) else {
            return;
        };
        let Some(tile_key) = self.tile_by_session.get(session_id).cloned() else {
            return;
        };
        let duration = format_duration(start.elapsed());
        let text = format!(
            "{} in {}",
            if stopped { "stopped" } else { "done" },
            duration
        );
        let note_key = self.next_key;
        self.next_key += 1;
        if let Some(tile) = self.tile_by_key_mut(&tile_key) {
            tile.items.push(Arc::new(ChatItem::SystemNote {
                key: note_key,
                text,
                is_error: false,
            }));
        }
        let _ = cx;
    }
}
