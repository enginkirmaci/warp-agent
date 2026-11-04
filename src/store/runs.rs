use super::*;

impl Store {
    // ---- pending permission/question parking ----

    pub(super) fn drop_pending(&mut self, session_ids: &[String]) {
        for sid in session_ids {
            if self.pending_by_session.remove(sid).is_some() {
                self.mark_attention(sid, false);
            }
            self.pending_questions_by_session.remove(sid);
        }
    }

    /// Drops the auto-retry bookkeeping for sessions whose workspace went away.
    pub(super) fn drop_retries(&mut self, session_ids: &[String]) {
        for sid in session_ids {
            self.last_prompt_by_session.remove(sid);
            self.retry_due_by_session.remove(sid);
        }
    }

    pub(super) fn flush_pending_permissions(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending_by_session.remove(session_id) {
            // snapshot remaining countdowns first (items move under a &mut self borrow)
            let lefts: Vec<Option<i64>> = pending
                .iter()
                .map(|i| self.countdowns.get(&i.id).copied())
                .collect();
            if let Some(tile) = self.tile_for_session_mut(session_id) {
                for (mut item, left) in pending.into_iter().zip(lefts) {
                    // the countdown ran while the card was parked — show what's left
                    if let Some(left) = left {
                        item.auto_countdown = Some(left);
                    }
                    tile.items.push(Arc::new(ChatItem::Permission(item)));
                }
            }
            self.mark_attention(session_id, false);
        }
        let _ = cx;
    }

    pub(super) fn flush_pending_questions(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending_questions_by_session.remove(session_id) {
            if let Some(tile) = self.tile_for_session_mut(session_id) {
                for item in pending {
                    if item.is_pending()
                        && !tile
                            .items
                            .iter()
                            .any(|i| matches!(i.as_ref(), ChatItem::Question(q) if q.id == item.id))
                    {
                        tile.items.push(Arc::new(ChatItem::Question(item)));
                    }
                }
            }
            self.mark_attention(session_id, false);
        }
        let _ = cx;
    }

    pub(super) fn pull_pending_questions(&mut self, ws_key: &str, session_id: &str) {
        self.net.send(NetCommand::Request {
            ws_key: ws_key.to_string(),
            request: ApiRequest::GetQuestions {
                for_session: session_id.to_string(),
            },
        });
    }

    pub fn mark_attention(&mut self, session_id: &str, attention: bool) {
        // light the row itself, or the run that spawned it for subagent asks
        let index = self
            .running_agents
            .iter()
            .position(|r| r.session_id == session_id)
            .or_else(|| {
                self.running_agents
                    .iter()
                    .position(|r| r.parent_session_id.as_deref() == Some(session_id))
            });
        if let Some(index) = index {
            self.running_agents[index].needs_attention = attention;
        }
    }

    // ---- running/subagent tracking ----

    pub(super) fn set_running(&mut self, session_id: Option<&str>, running: bool) {
        let Some(session_id) = session_id else { return };
        if !self.is_app_session(session_id) {
            return;
        }
        let changed = if running {
            self.running_by_session.insert(session_id.to_string())
        } else {
            self.running_by_session.remove(session_id)
        };
        if !changed {
            return;
        }
        let existing_index = self
            .running_agents
            .iter()
            .position(|r| r.session_id == session_id);
        if running {
            // a background door run stays off the visible agents rail — its
            // client watches it over the door, and a row would be churn
            let background = self
                .tile_for_session(session_id)
                .is_some_and(|tile| tile.background);
            if !background && existing_index.is_none() {
                self.running_agents.push(RunningAgentItem {
                    session_id: session_id.to_string(),
                    parent_session_id: None,
                    workspace: self.workspace_name_for(session_id),
                    title: self.title_for(session_id),
                    is_subagent: false,
                    needs_attention: self.pending_by_session.contains_key(session_id),
                });
            }
        } else if let Some(index) = existing_index {
            self.running_agents.remove(index);
        }
        self.update_sidebar_running();
        self.refresh_tile_agents();
        self.ipc_push_status();
    }

    fn update_sidebar_running(&mut self) {
        let running: HashSet<String> = self.running_by_session.clone();
        // both mirrors must move together: sync_workspace_chats copies
        // self.sessions over ws.chats (e.g. on the mid-turn auto-rename), so a
        // stale flag there would kill the spinner while the chat still runs
        for chat in &mut self.sessions {
            chat.is_running = running.contains(&chat.id);
        }
        for ws in &mut self.workspaces {
            for chat in &mut ws.chats {
                chat.is_running = running.contains(&chat.id);
            }
        }
    }

    /// Returns whether running-agent-visible state changed: `session.updated`
    /// fires at token rate for every (subagent) session, and an unchanged
    /// snapshot must not rebuild the tiles' agent lists.
    pub(super) fn track_session_info(
        &mut self,
        session_id: Option<&str>,
        parent_id: Option<&str>,
        agent: Option<&str>,
    ) -> bool {
        let Some(session_id) = session_id else {
            return false;
        };
        match parent_id {
            None if parent_id.is_none() || parent_id == Some(session_id) => {
                if self.subagent_by_session.remove(session_id).is_some() {
                    self.set_subagent_running(session_id, false);
                    return true;
                }
                false
            }
            Some(parent_id) if parent_id == session_id => {
                if self.subagent_by_session.remove(session_id).is_some() {
                    self.set_subagent_running(session_id, false);
                    return true;
                }
                false
            }
            Some(parent_id) => {
                let agent_label = agent
                    .filter(|a| !a.trim().is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| "subagent".into());
                let info = SubagentInfo {
                    parent_session_id: parent_id.to_string(),
                    agent: agent_label.clone(),
                };
                let prior = self
                    .subagent_by_session
                    .insert(session_id.to_string(), info);
                let info_changed = prior
                    .is_none_or(|p| p.parent_session_id != parent_id || p.agent != agent_label);
                // a running row's label follows the parent's title; compute it
                // only for sessions that actually have a row
                let mut label_changed = false;
                if self.running_subagents.contains(session_id) {
                    let fresh_label = format!("{} → {}", agent_label, self.title_for(parent_id));
                    if let Some(row) = self
                        .running_agents
                        .iter_mut()
                        .find(|r| r.session_id == session_id)
                        && row.title != fresh_label
                    {
                        row.title = fresh_label;
                        label_changed = true;
                    }
                }
                info_changed || label_changed
            }
            None => false,
        }
    }

    pub(super) fn set_subagent_running(&mut self, child_id: &str, running: bool) {
        if !self.subagent_by_session.contains_key(child_id) {
            return;
        }
        let changed = if running {
            self.running_subagents.insert(child_id.to_string())
        } else {
            self.running_subagents.remove(child_id)
        };
        if !changed {
            return;
        }
        let existing_index = self
            .running_agents
            .iter()
            .position(|r| r.session_id == child_id);
        if running {
            if existing_index.is_none() {
                let info = self.subagent_by_session.get(child_id).unwrap();
                let title = format!(
                    "{} → {}",
                    info.agent,
                    self.title_for(&info.parent_session_id)
                );
                self.running_agents.push(RunningAgentItem {
                    session_id: child_id.to_string(),
                    parent_session_id: Some(info.parent_session_id.clone()),
                    workspace: self.workspace_name_for(child_id),
                    title,
                    is_subagent: true,
                    needs_attention: false,
                });
            }
        } else if let Some(index) = existing_index {
            self.running_agents.remove(index);
        }
        self.refresh_tile_agents();
    }

    /// Mirrors the owner's running set into each chat's flyout: only rows whose
    /// parent is that session (the subagent runs it spawned).
    pub(super) fn refresh_tile_agents(&mut self) {
        for tile in &mut self.open_tiles {
            tile.tile_agents = self
                .running_agents
                .iter()
                .filter(|r| r.parent_session_id.as_deref() == tile.session_id.as_deref())
                .cloned()
                .collect();
        }
    }

    pub(super) fn drop_subagent_rows(&mut self, folder_path: &str) {
        let children: Vec<String> = self
            .running_subagents
            .iter()
            .filter(|id| {
                self.effective_workspace_path(id)
                    .map(|p| p == folder_path)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        for child_id in children {
            self.set_subagent_running(&child_id, false);
            // the mapping only describes live runs; keep it from growing forever
            self.subagent_by_session.remove(&child_id);
        }
    }

    /// Opens a chat from the running-agents panel straight into the grid; a
    /// subagent row opens the chat that spawned it.
    pub fn open_running(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let target = self
            .subagent_by_session
            .get(session_id)
            .map(|s| s.parent_session_id.clone())
            .unwrap_or_else(|| session_id.to_string());
        let Some(path) = self.workspace_by_session.get(&target).cloned() else {
            return;
        };
        self.open_session_into_grid(&path, &target, cx);
    }

    /// Opens a subagent child chat into the grid: focuses its tile when one is
    /// already open, otherwise loads it through its parent's workspace server.
    pub fn open_subagent_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if let Some(tile_key) = self.tile_by_session.get(session_id).cloned() {
            self.focus_tile(&tile_key, cx);
            return;
        }
        let Some(path) = self.effective_workspace_path(session_id) else {
            return;
        };
        self.open_session_into_grid(&path, session_id, cx);
    }

    /// Opens the clicked sidebar chat: focuses its tile when one exists, otherwise
    /// loads it from its own workspace's server — without switching the selected
    /// workspace, so chats of different workspaces share one grid.
    pub fn select_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        tracing::info!("select_session {}", session_id);
        if self
            .focused_tile
            .as_deref()
            .and_then(|k| self.tile_by_key(k))
            .and_then(|t| t.session_id.clone())
            .as_deref()
            == Some(session_id)
        {
            return;
        }
        if let Some(tile_key) = self.tile_by_session.get(session_id).cloned() {
            self.focus_tile(&tile_key, cx);
            return;
        }
        let Some(path) = self.workspace_by_session.get(session_id).cloned() else {
            return;
        };
        self.open_session_into_grid(&path, session_id, cx);
    }

    /// Loads a chat into a grid tile through its own workspace's server, starting
    /// that server if needed; the selected workspace is left alone.
    pub fn open_session_into_grid(
        &mut self,
        ws_path: &str,
        session_id: &str,
        cx: &mut Context<Self>,
    ) {
        if let Some(ws) = self.workspace_by_path_mut(ws_path) {
            ws.set_expanded(true); // the opened chat's row belongs in a visible sidebar section
        }
        self.touch_server_activity(ws_path);
        if self.session_harness(session_id) == HarnessKind::Dsh {
            if !self.runtimes.contains_key(DSH_SERVER_KEY) {
                self.ensure_dsh(cx);
                self.pending_opens
                    .push((DSH_SERVER_KEY.into(), session_id.to_string()));
                self.repaint(cx);
                return;
            }
            self.net.send(NetCommand::Request {
                ws_key: DSH_SERVER_KEY.into(),
                request: ApiRequest::GetMessages {
                    session_id: session_id.to_string(),
                },
            });
            self.repaint(cx);
            return;
        }
        if !self.runtimes.contains_key(ws_path) {
            self.start_workspace(ws_path, cx);
            self.pending_opens
                .push((ws_path.to_string(), session_id.to_string()));
            self.repaint(cx);
            return;
        }
        self.net.send(NetCommand::Request {
            ws_key: ws_path.to_string(),
            request: ApiRequest::GetMessages {
                session_id: session_id.to_string(),
            },
        });
        self.repaint(cx);
    }
}
