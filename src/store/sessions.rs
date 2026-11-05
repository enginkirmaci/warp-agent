use super::*;

impl Store {
    // ---- session list maintenance ----

    pub(super) fn apply_session_list(
        &mut self,
        ws_key: &str,
        sessions: Vec<SessionInfo>,
        is_dsh_list: bool,
        cx: &mut Context<Self>,
    ) {
        // a deleted chat can still ride a server list (dsh archives per
        // process); never let one back into a workspace after a restart
        let sessions: Vec<SessionInfo> = sessions
            .into_iter()
            .filter(|s| !self.deleted_sessions.contains(&s.id))
            .collect();
        // keep the subagent→parent map warm from the listing too
        for s in &sessions {
            if let Some(parent_id) = &s.parent_id {
                self.track_session_info(Some(&s.id), Some(parent_id), None);
            }
        }
        // a dsh list's entries are dsh chats by definition: mark them and
        // track the ones this workspace doesn't know yet (revealed here by
        // cwd, e.g. created from another window)
        if is_dsh_list {
            let unknown: Vec<SessionInfo> = {
                let known = self
                    .workspaces
                    .iter()
                    .find(|w| w.folder_path() == ws_key)
                    .map(|w| w.state.sessions.clone())
                    .unwrap_or_default();
                sessions
                    .iter()
                    // a background run's chat is revealed by cwd here — it
                    // must stay hidden until its run retires it
                    .filter(|s| !known.contains(&s.id) && !self.hidden_sessions.contains(&s.id))
                    .cloned()
                    .collect()
            };
            for s in &sessions {
                self.ui
                    .harness_by_session
                    .insert(s.id.clone(), HarnessKind::Dsh);
            }
            if !unknown.is_empty() {
                if let Some(ws) = self.workspace_by_path_mut(ws_key) {
                    for s in &unknown {
                        ws.state.sessions.insert(0, s.id.clone());
                    }
                }
                for s in &unknown {
                    self.workspace_by_session
                        .insert(s.id.clone(), ws_key.to_string());
                    self.title_by_session.insert(s.id.clone(), s.title.clone());
                }
            }
        }
        if self.selected_workspace.as_deref() != Some(ws_key) {
            // a background chat load for another workspace: fill its sidebar section
            let known: HashSet<String> = self
                .workspaces
                .iter()
                .find(|w| w.folder_path() == ws_key)
                .map(|w| w.state.sessions.iter().cloned().collect())
                .unwrap_or_default();
            // the selected branch fills the title map below; this branch must
            // too, or a chat clicked out of this section opens as "agent ses_xx"
            for s in sessions.iter().filter(|s| known.contains(&s.id)) {
                self.title_by_session.insert(s.id.clone(), s.title.clone());
                if let Some(tile) = self.tile_for_session_mut(&s.id) {
                    tile.title = s.title.clone();
                }
            }
            let fresh_labels: Vec<(String, String)> = self
                .running_agents
                .iter()
                .filter(|r| known.contains(&r.session_id))
                .map(|r| (r.session_id.clone(), self.title_for(&r.session_id)))
                .collect();
            for running in &mut self.running_agents {
                if let Some((_, title)) = fresh_labels
                    .iter()
                    .find(|(sid, _)| sid == &running.session_id)
                {
                    running.title = title.clone();
                }
            }
            let pins = &self.ui.pins;
            let mut chats: Vec<SessionItem> = sessions
                .iter()
                .filter(|s| known.contains(&s.id))
                .map(|s| SessionItem {
                    id: s.id.clone(),
                    title: s.title.clone(),
                    updated_ms: s.updated_ms,
                    created_ms: s.created_ms,
                    is_pinned: pins.contains_key(&s.id),
                    is_selected: false,
                    is_running: self.running_by_session.contains(&s.id),
                    is_editing: false,
                    draft_title: String::new(),
                })
                .collect();
            chats.sort_by_key(session_recency);
            // this server's list knows nothing about the other harness's chats
            // the section already shows — keep those instead of flip-flopping
            let keep: Vec<SessionItem> = {
                let existing = self
                    .workspaces
                    .iter()
                    .find(|w| w.folder_path() == ws_key)
                    .map(|w| w.chats.clone())
                    .unwrap_or_default();
                existing
                    .into_iter()
                    .filter(|chat| {
                        (self.session_harness(&chat.id) == HarnessKind::Dsh) != is_dsh_list
                    })
                    .collect()
            };
            for chat in keep {
                if !chats.iter().any(|c| c.id == chat.id) {
                    chats.push(chat);
                }
            }
            chats.sort_by_key(session_recency);
            if let Some(ws) = self.workspace_by_path_mut(ws_key) {
                ws.chats = chats;
            }
            self.repaint(cx);
            return;
        }
        let Some(state) = self
            .workspaces
            .iter()
            .find(|w| w.folder_path() == ws_key)
            .map(|w| w.state.clone())
        else {
            return;
        };
        let pins = self.ui.pins.clone();
        let mut mine: Vec<SessionInfo> = sessions
            .into_iter()
            .filter(|s| state.sessions.contains(&s.id))
            .collect();
        mine.sort_by_key(|s| {
            std::cmp::Reverse((pins.contains_key(&s.id), s.updated_ms, s.created_ms))
        });
        let alive: HashSet<String> = mine.iter().map(|s| s.id.clone()).collect();
        // drop chats that no longer exist on the server — only the ones this
        // list's server owns (a dsh list knows nothing about opencode sessions)
        let gone: Vec<String> = state
            .sessions
            .iter()
            .filter(|id| {
                let dsh = self.session_harness(id) == HarnessKind::Dsh;
                dsh == is_dsh_list && !alive.contains(*id)
            })
            .cloned()
            .collect();
        if !gone.is_empty() {
            if let Some(ws) = self.workspace_by_path_mut(ws_key) {
                ws.state.sessions.retain(|id| alive.contains(id));
            }
            for id in &gone {
                self.workspace_by_session.remove(id);
                self.title_by_session.remove(id);
                let tile_key = self.tile_by_session.get(id).cloned();
                if let Some(tile_key) = tile_key {
                    self.detach_tile(&tile_key, cx);
                }
            }
            self.drop_queue(&gone);
            self.drop_pending(&gone);
        }
        // this server's list knows nothing about the other harness's chats the
        // sidebar already shows — keep those instead of flip-flopping the
        // section between servers on every list refresh. The section (ws.chats)
        // is the keep source, not the selected mirror: the mirror only fills
        // from list replies, so a bare opencode list on a dsh-heavy workspace
        // would otherwise wipe the section
        let mut items: Vec<SessionItem> = mine
            .iter()
            .map(|s| SessionItem {
                id: s.id.clone(),
                title: s.title.clone(),
                updated_ms: s.updated_ms,
                created_ms: s.created_ms,
                is_pinned: self.ui.pins.contains_key(&s.id),
                is_selected: false,
                is_running: self.running_by_session.contains(&s.id),
                is_editing: false,
                draft_title: String::new(),
            })
            .collect();
        let keep: Vec<SessionItem> = {
            let existing = self
                .workspaces
                .iter()
                .find(|w| w.folder_path() == ws_key)
                .map(|w| w.chats.clone())
                .unwrap_or_default();
            existing
                .into_iter()
                .filter(|item| (self.session_harness(&item.id) == HarnessKind::Dsh) != is_dsh_list)
                .collect()
        };
        for item in keep {
            if !items.iter().any(|c| c.id == item.id) {
                items.push(item);
            }
        }
        items.sort_by_key(session_recency);
        // an idle fires at the end of every turn; when nothing visible changed,
        // skip the full rebuild + disk save (the common case while chatting)
        let unchanged = self.sessions.len() == items.len()
            && self.sessions.iter().zip(items.iter()).all(|(cur, s)| {
                cur.id == s.id
                    && cur.title == s.title
                    && cur.updated_ms == s.updated_ms
                    && cur.is_pinned == s.is_pinned
                    && cur.is_running == s.is_running
            });
        if unchanged {
            return;
        }
        for s in &mine {
            self.title_by_session.insert(s.id.clone(), s.title.clone());
            if let Some(tile) = self.tile_for_session_mut(&s.id) {
                tile.title = s.title.clone();
            }
        }
        self.sessions = items;
        let titles: Vec<(String, String)> = self
            .running_agents
            .iter()
            .filter(|r| alive.contains(&r.session_id))
            .map(|r| (r.session_id.clone(), self.title_for(&r.session_id)))
            .collect();
        for running in &mut self.running_agents {
            if let Some((_, title)) = titles.iter().find(|(sid, _)| sid == &running.session_id) {
                running.title = title.clone();
            }
        }
        self.update_sidebar_selection();
        self.sync_workspace_chats(cx);
        self.save_state();
        self.repaint(cx);
    }

    pub fn sort_sessions(&mut self) {
        self.sessions.sort_by_key(session_recency);
    }

    pub(super) fn sync_workspace_chats(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.selected_workspace.clone() else {
            return;
        };
        let chats = self.sessions.clone();
        if let Some(ws) = self.workspace_by_path_mut(&path) {
            ws.chats = chats;
        }
        self.update_sidebar_selection();
        self.repaint(cx);
    }

    pub fn update_sidebar_selection(&mut self) {
        let selected_path = self.selected_workspace.clone();
        let focused_session = self
            .focused_tile
            .as_deref()
            .and_then(|key| self.tile_by_key(key))
            .and_then(|t| t.session_id.clone());
        for ws in &mut self.workspaces {
            ws.is_selected = Some(ws.folder_path()) == selected_path.as_deref();
            for chat in &mut ws.chats {
                chat.is_selected = focused_session.as_deref() == Some(chat.id.as_str());
            }
        }
    }

    /// The star of the station repos list, over workspaces only: chats are
    /// kept by pins. Favorites persist in `ui.json` and sort to the top of the
    /// workspace list.
    pub fn toggle_workspace_favorite(&mut self, path: &str, cx: &mut Context<Self>) {
        let is_favorite = !self.ui.favorites.contains_key(path);
        if is_favorite {
            self.ui.favorites.insert(path.to_string(), true);
        } else {
            self.ui.favorites.remove(path);
        }
        for ws in &mut self.workspaces {
            if ws.folder_path() == path {
                ws.is_favorite = is_favorite;
            }
        }
        // both lists sort favorites to the top: the serialized mirror for the
        // next launch, the runtime items for this session's sidebar
        sort_workspaces(&mut self.ui.workspaces, &self.ui.favorites);
        self.workspaces
            .sort_by_key(|w| workspace_order_key(w.folder_path(), &self.ui.favorites));
        self.save_state();
        self.repaint(cx);
    }

    pub fn toggle_pin(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let is_pinned = !self.ui.pins.contains_key(session_id);
        if is_pinned {
            self.ui.pins.insert(session_id.to_string(), true);
        } else {
            self.ui.pins.remove(session_id);
        }
        if let Some(item) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            item.is_pinned = is_pinned;
        }
        for ws in &mut self.workspaces {
            if let Some(chat) = ws.chats.iter_mut().find(|c| c.id == session_id) {
                chat.is_pinned = is_pinned;
            }
        }
        self.save_state();
        self.sort_sessions();
        self.sync_workspace_chats(cx);
        self.repaint(cx);
    }

    pub fn begin_rename(&mut self, session_id: &str, cx: &mut Context<Self>) {
        for ws in &mut self.workspaces {
            if let Some(chat) = ws.chats.iter_mut().find(|c| c.id == session_id) {
                chat.draft_title = chat.title.clone();
                chat.is_editing = true;
            }
        }
        let _ = cx;
    }

    /// The rename field's current text, applied before commit.
    pub fn set_rename_draft(&mut self, session_id: &str, text: String) {
        for ws in &mut self.workspaces {
            if let Some(chat) = ws.chats.iter_mut().find(|c| c.id == session_id) {
                chat.draft_title = text.clone();
            }
        }
    }

    pub fn commit_rename(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let draft = self
            .workspaces
            .iter()
            .flat_map(|w| w.chats.iter())
            .find(|c| c.id == session_id)
            .map(|c| {
                (
                    c.draft_title.trim().to_string(),
                    c.title.clone(),
                    c.is_editing,
                )
            });
        let Some((title, current, editing)) = draft else {
            return;
        };
        // Enter commits and also blurs the field: the blur that follows must
        // not send a second rename for the already-closed edit
        if !editing {
            return;
        }
        for ws in &mut self.workspaces {
            if let Some(chat) = ws.chats.iter_mut().find(|c| c.id == session_id) {
                chat.is_editing = false;
            }
        }
        if title.is_empty() || title == current {
            return;
        }
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
        self.repaint(cx);
    }

    pub fn delete_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self.is_busy(session_id) {
            self.announce("Stop the running turn before deleting this chat.", true, cx);
            return;
        }
        // Delete absorbs the old Remove row: the chat leaves the sidebar at
        // once (the local unlink Remove did) and the server is asked to delete
        // the session behind it. Waiting for the round-trip kept a stale row on
        // a server that had already gone away, so the list update comes first.
        let ws_key = self.net_key_for_session(session_id);
        if let Some(ws_key) = ws_key {
            self.net.send(NetCommand::Request {
                ws_key,
                request: ApiRequest::DeleteSession {
                    session_id: session_id.to_string(),
                },
            });
        }
        self.forget_session(session_id, cx);
        self.save_state();
        self.repaint(cx);
    }

    pub(super) fn forget_session(&mut self, id: &str, cx: &mut Context<Self>) {
        // a dsh follow stream keeps folded state for the chat; the net thread
        // drops it (a no-op for opencode) so deleting chats frees memory
        self.net.send(NetCommand::ForgetDshSession {
            session_id: id.to_string(),
        });
        if let Some(tile_key) = self.tile_by_session.get(id).cloned() {
            self.detach_tile(&tile_key, cx);
        }
        self.turn_start_by_session.remove(id);
        self.user_stopped_by_session.remove(id);
        self.workspace_by_session.remove(id);
        self.hidden_sessions.remove(id);
        // dsh's `session/list` keeps naming archived chats after a restart;
        // remembering the id here is what keeps a deleted chat deleted
        self.deleted_sessions.insert(id.to_string());
        self.ui.harness_by_session.remove(id);
        for ws in &mut self.ui.workspaces {
            ws.sessions.retain(|s| s != id);
        }
        for ws in &mut self.workspaces {
            // the runtime list too, or the next save_state resurrects the id
            ws.state.sessions.retain(|s| s != id);
            ws.chats.retain(|c| c.id != id);
        }
        self.title_by_session.remove(id);
        self.sessions.retain(|s| s.id != id);
        self.ui.pins.remove(id);
        self.drop_queue(&[id.to_string()]);
        self.drop_pending(&[id.to_string()]);
        self.drop_retries(&[id.to_string()]);
        // no sync_workspace_chats here: it overwrites the selected workspace's
        // section with the (often stale) self.sessions mirror, wiping every
        // chat that never made it into the mirror — the retains above already
        // updated the sections
        self.update_sidebar_selection();
        self.repaint(cx);
    }

    // ---- restore ----

    /// Reopens every chat the last session left tiled, starting each involved workspace's server.
    pub fn restore_previous_session(&mut self, cx: &mut Context<Self>) {
        self.show_restore_prompt = false;
        let wanted_by_ws: Vec<(String, Vec<String>)> = self
            .workspaces
            .iter()
            .filter(|w| !w.state.open_sessions.is_empty())
            .map(|w| (w.folder_path().to_string(), w.state.open_sessions.clone()))
            .collect();
        for (path, wanted) in wanted_by_ws {
            if let Some(ws) = self.workspace_by_path_mut(&path) {
                ws.set_expanded(true); // restored chats belong in a visible sidebar section
            }
            if self.runtimes.contains_key(&path) {
                self.restore_open_tiles(&path, wanted, cx);
            } else {
                self.start_workspace(&path, cx);
                // the restore completes when ServerStarted arrives; stash the wanted list
                self.pending_restores.push((path.clone(), wanted));
            }
        }
        self.repaint(cx);
    }

    pub fn dismiss_restore_prompt(&mut self, cx: &mut Context<Self>) {
        self.show_restore_prompt = false;
        for ws in &mut self.workspaces {
            ws.state.open_sessions.clear();
        }
        self.save_state();
        let _ = cx;
    }

    /// Refills the grid from a workspace's saved open chats; sessions already tiled are skipped.
    pub(super) fn restore_open_tiles(
        &mut self,
        ws_key: &str,
        wanted: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        // tiles are (re)opening: the restore banner has served its purpose
        self.show_restore_prompt = false;
        let mut added = 0;
        for id in wanted {
            let known = self
                .workspaces
                .iter()
                .find(|w| w.folder_path() == ws_key)
                .map(|w| w.state.sessions.contains(&id))
                .unwrap_or(false);
            if !known || self.tile_by_session.contains_key(&id) {
                continue;
            }
            let route = self
                .net_key_for_session(&id)
                .unwrap_or_else(|| ws_key.to_string());
            if route == DSH_SERVER_KEY && !self.runtimes.contains_key(DSH_SERVER_KEY) {
                self.ensure_dsh(cx);
                self.pending_opens.push((DSH_SERVER_KEY.into(), id.clone()));
            } else {
                self.net.send(NetCommand::Request {
                    ws_key: route,
                    request: ApiRequest::GetMessages {
                        session_id: id.clone(),
                    },
                });
            }
            added += 1;
        }
        if added == 0 {
            self.prune_untouched_drafts(cx);
        }
        self.repaint(cx);
    }
}
