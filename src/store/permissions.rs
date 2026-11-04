use super::*;

impl Store {
    pub(super) fn on_permission_asked(
        &mut self,
        data: &serde_json::Value,
        v2: bool,
        cx: &mut Context<Self>,
    ) {
        let (Some(id), Some(session_id)) = (
            data.get("id").and_then(serde_json::Value::as_str),
            data.get("sessionID").and_then(serde_json::Value::as_str),
        ) else {
            return;
        };
        let id = id.to_string();
        let session_id = session_id.to_string();

        let kind;
        let mut detail;
        let mut diff = None;
        if v2 {
            kind = data
                .get("action")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("permission")
                .to_string();
            detail = data
                .get("resources")
                .and_then(serde_json::Value::as_array)
                .map(|r| {
                    r.iter()
                        .filter_map(|x| x.as_str())
                        .filter(|x| !x.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default();
        } else {
            kind = data
                .get("permission")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("permission")
                .to_string();
            detail = String::new();
            if let Some(md) = data.get("metadata").and_then(serde_json::Value::as_object) {
                if let Some(command) = md.get("command").and_then(serde_json::Value::as_str)
                    && !command.is_empty()
                {
                    detail = command.to_string();
                }
                if let Some(d) = md.get("diff").and_then(serde_json::Value::as_str)
                    && !d.is_empty()
                {
                    diff = Some(d.to_string());
                }
            }
            if detail.is_empty() {
                detail = data
                    .get("patterns")
                    .and_then(serde_json::Value::as_array)
                    .map(|p| {
                        p.iter()
                            .filter_map(|x| x.as_str())
                            .filter(|x| !x.is_empty())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
            }
        }
        if detail.is_empty() {
            detail = "(no detail provided)".into();
        }

        let item = PermissionItem {
            id: id.clone(),
            session_id: session_id.clone(),
            kind,
            detail,
            is_v2: v2,
            diff,
            answer: None,
            was_auto_answered: false,
            auto_countdown: None,
        };
        // a subagent's ask belongs to the chat that spawned it; an open tile takes the
        // card now, otherwise it parks until that chat's tile opens
        let owner = self.owner_session_id(&session_id);
        if !self.is_app_session(&owner) {
            return;
        }
        if self.auto_allow {
            if self.settings.auto_allow_instant {
                // "allow immediately": answer on the next tick instead of
                // drawing a random wait
                self.countdowns.insert(id.clone(), 0);
            } else {
                let range = self.auto_allow_range();
                let seconds = range.0 + (rand_0_to(range.1 - range.0));
                self.countdowns.insert(id.clone(), seconds);
            }
        }
        let auto_allow = self.auto_allow;
        if let Some(tile_key) = self.tile_by_session.get(&owner).cloned() {
            let tile = self.tile_by_key_mut(&tile_key).unwrap();
            tile.items.push(Arc::new(ChatItem::Permission(item)));
            if !auto_allow {
                self.mark_attention(&session_id, true);
            }
            self.repaint(cx);
            return;
        }
        self.pending_by_session.entry(owner).or_default().push(item);
        if !auto_allow {
            self.mark_attention(&session_id, true);
        }
        self.repaint(cx);
    }

    /// Random auto-allow wait: quick during working hours, long over lunch and off hours.
    fn auto_allow_range(&self) -> (i64, i64) {
        let hour = Local::now().hour() as i64;
        let s = &self.settings;
        let away = (hour >= s.lunch_start_hour && hour < s.lunch_end_hour)
            || hour >= s.off_hours_start_hour
            || hour < s.workday_start_hour;
        let (min, max) = if away {
            (s.auto_allow_away_min_seconds, s.auto_allow_away_max_seconds)
        } else {
            (
                s.auto_allow_default_min_seconds,
                s.auto_allow_default_max_seconds,
            )
        };
        let min = min.max(0);
        (min, max.max(min))
    }

    /// Called once per second by the countdown tick; answers "always" for due asks.
    pub fn tick_countdowns(&mut self, cx: &mut Context<Self>) {
        // the idle-server sweep piggybacks on the 1 Hz tick at a slower cadence
        if self
            .last_idle_sweep
            .map(|last| last.elapsed() >= std::time::Duration::from_secs(30))
            .unwrap_or(true)
        {
            self.last_idle_sweep = Some(Instant::now());
            self.sweep_idle_servers(cx);
        }
        if self.countdowns.is_empty() {
            return;
        }
        let mut due: Vec<String> = Vec::new();
        for (id, left) in self.countdowns.iter_mut() {
            *left -= 1;
            if *left <= 0 {
                due.push(id.clone());
            }
        }
        for id in &due {
            self.countdowns.remove(id);
        }
        for id in due {
            // only still-pending items fire
            if self
                .find_permission(&id)
                .map(|p| p.is_pending())
                .unwrap_or(false)
                && self.auto_allow
            {
                self.respond_permission(&id, "always", true, cx);
            }
        }
        // refresh the visible countdown labels — make_mut only the pending
        // permission items themselves: the snapshot/render side keeps Arc
        // clones alive, so an unguarded make_mut would deep-copy every item
        // of every tile once per second. Per-tile revision bumps only: the
        // 1 Hz tick must not rebuild the sidebar or the other tiles.
        let mut touched: Vec<String> = Vec::new();
        for tile in &mut self.open_tiles {
            let mut changed = false;
            for item in &mut tile.items {
                let Some(left) = (match item.as_ref() {
                    ChatItem::Permission(p) => self.countdowns.get(&p.id).copied(),
                    _ => None,
                }) else {
                    continue;
                };
                if let ChatItem::Permission(p) = Arc::make_mut(item) {
                    p.auto_countdown = Some(left);
                    changed = true;
                }
            }
            if changed {
                touched.push(tile.key.clone());
            }
        }
        for key in touched {
            self.touch_tile(&key, cx);
        }
    }

    fn find_permission(&self, permission_id: &str) -> Option<&PermissionItem> {
        for tile in &self.open_tiles {
            for item in &tile.items {
                if let ChatItem::Permission(p) = item.as_ref()
                    && p.id == permission_id
                {
                    return Some(p);
                }
            }
        }
        for pending in self.pending_by_session.values() {
            for p in pending {
                if p.id == permission_id {
                    return Some(p);
                }
            }
        }
        None
    }

    pub fn respond_permission(
        &mut self,
        permission_id: &str,
        response: &str,
        is_auto: bool,
        cx: &mut Context<Self>,
    ) {
        // close the buttons immediately so a slow reply cannot be clicked twice
        let mut session_id = None;
        let mut is_v2 = false;
        for tile in &mut self.open_tiles {
            for item in &mut tile.items {
                if let ChatItem::Permission(p) = Arc::make_mut(item)
                    && p.id == permission_id
                    && p.is_pending()
                {
                    p.answer = Some(if is_auto {
                        "auto-allowing…".into()
                    } else {
                        "sending…".into()
                    });
                    p.was_auto_answered = is_auto;
                    p.auto_countdown = None;
                    session_id = Some(p.session_id.clone());
                    is_v2 = p.is_v2;
                }
            }
        }
        for pending in self.pending_by_session.values_mut() {
            for item in pending {
                if item.id == permission_id && item.is_pending() {
                    item.answer = Some(if is_auto {
                        "auto-allowing…".into()
                    } else {
                        "sending…".into()
                    });
                    item.was_auto_answered = is_auto;
                    item.auto_countdown = None;
                    session_id = Some(item.session_id.clone());
                    is_v2 = item.is_v2;
                }
            }
        }
        let Some(session_id) = session_id else { return };
        let Some(ws_key) = self.net_key_for_session(&session_id) else {
            self.resolve_permission(permission_id, "server unavailable".to_string(), is_auto);
            return;
        };
        // a stopped/failed workspace would swallow the request silently and the
        // card would sit at "sending…" forever
        if !self.runtimes.contains_key(&ws_key) {
            self.resolve_permission(permission_id, "server unavailable".to_string(), is_auto);
            return;
        }
        let response = response.to_string();
        let request = if is_v2 {
            ApiRequest::ReplyV2 {
                request_id: permission_id.to_string(),
                response,
            }
        } else {
            ApiRequest::RespondPermission {
                session_id: session_id.clone(),
                permission_id: permission_id.to_string(),
                response,
            }
        };
        self.net.send(NetCommand::Request { ws_key, request });
        // the card flipped to "sending…" — repaint now, not on the next event
        self.repaint(cx);
    }

    pub(super) fn resolve_permission(&mut self, permission_id: &str, label: String, is_auto: bool) {
        for tile in &mut self.open_tiles {
            for item in &mut tile.items {
                if let ChatItem::Permission(p) = Arc::make_mut(item)
                    && p.id == permission_id
                    && p.is_pending()
                {
                    p.answer = Some(label.clone());
                    p.was_auto_answered = is_auto;
                    p.auto_countdown = None;
                }
            }
        }
        for pending in self.pending_by_session.values_mut() {
            for item in pending {
                if item.id == permission_id && item.is_pending() {
                    item.answer = Some(label.clone());
                    item.was_auto_answered = is_auto;
                    item.auto_countdown = None;
                }
            }
        }
    }

    pub(super) fn on_permission_replied(&mut self, data: &serde_json::Value) {
        let Some(request_id) = data.get("requestID").and_then(serde_json::Value::as_str) else {
            return;
        };
        self.resolve_permission(request_id, "answered elsewhere".into(), false);
    }

    // ---- question tool asks ----
    // The agent's `question` tool parks the turn until the user answers; never auto-answered.

    pub(super) fn on_question_asked(&mut self, data: &serde_json::Value, cx: &mut Context<Self>) {
        let Some(request) = oc::read_question_request(data) else {
            return;
        };
        if request.questions.is_empty() {
            return;
        }
        // a subagent's ask belongs to the chat that spawned it; when that chat's tile is
        // open the card must land now — the parked list only drains when a tile opens
        let owner = self.owner_session_id(&request.session_id);
        if !self.is_app_session(&owner) {
            return;
        }
        let mut next_key = self.next_key;
        let built = build_question_item(&request, &mut next_key);
        self.next_key = next_key;
        if let Some(tile_key) = self.tile_by_session.get(&owner).cloned() {
            if let Some(tile) = self.tile_by_key_mut(&tile_key)
                && !tile
                    .items
                    .iter()
                    .any(|i| matches!(i.as_ref(), ChatItem::Question(q) if q.id == built.id))
            {
                tile.items.push(Arc::new(ChatItem::Question(built)));
            }
            self.repaint(cx);
            return;
        }
        let pending = self.pending_questions_by_session.entry(owner).or_default();
        if !pending.iter().any(|i| i.id == built.id) {
            pending.push(built);
        }
        self.mark_attention(&request.session_id, true);
        self.repaint(cx);
    }

    pub(super) fn on_question_resolved(&mut self, data: &serde_json::Value) {
        let Some(request_id) = data.get("requestID").and_then(serde_json::Value::as_str) else {
            return;
        };
        let mut summary = "dismissed".to_string();
        if let Some(answers) = data.get("answers").and_then(serde_json::Value::as_array) {
            let answers: Vec<String> = answers
                .iter()
                .map(|x| {
                    x.as_array()
                        .map(|l| {
                            l.iter()
                                .filter_map(|l| l.as_str())
                                .filter(|l| !l.is_empty())
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default()
                })
                .filter(|x| !x.is_empty())
                .collect();
            if !answers.is_empty() {
                summary = format!("answered: {}", answers.join(" · "));
            }
        }
        self.resolve_question(request_id, &summary);
    }

    pub(super) fn resolve_question(&mut self, request_id: &str, summary: &str) {
        for tile in &mut self.open_tiles {
            for item in &mut tile.items {
                if let ChatItem::Question(q) = Arc::make_mut(item)
                    && q.id == request_id
                    && q.is_pending()
                {
                    q.resolution = Some(summary.to_string());
                    for question in &mut q.questions {
                        question.locked = true;
                    }
                }
            }
        }
        for pending in self.pending_questions_by_session.values_mut() {
            for item in pending {
                if item.id == request_id && item.is_pending() {
                    item.resolution = Some(summary.to_string());
                    for question in &mut item.questions {
                        question.locked = true;
                    }
                }
            }
        }
    }

    pub fn submit_question(&mut self, question_id: &str, cx: &mut Context<Self>) {
        let mut answers: Option<Vec<Vec<String>>> = None;
        let mut session_id = None;
        for tile in &mut self.open_tiles {
            for item in &mut tile.items {
                if let ChatItem::Question(q) = Arc::make_mut(item)
                    && q.id == question_id
                    && q.is_pending()
                    && q.can_submit()
                {
                    answers = Some(q.questions.iter().map(|e| e.answer()).collect());
                    q.resolution = Some(format!(
                        "sending {}…",
                        answers
                            .as_ref()
                            .unwrap()
                            .iter()
                            .map(|a| if a.is_empty() {
                                "(empty)".to_string()
                            } else {
                                a.join(", ")
                            })
                            .collect::<Vec<_>>()
                            .join(" · ")
                    ));
                    for question in &mut q.questions {
                        question.locked = true;
                    }
                    session_id = Some(q.session_id.clone());
                }
            }
        }
        let (Some(answers), Some(session_id)) = (answers, session_id) else {
            return;
        };
        let Some(ws_key) = self.net_key_for_session(&session_id) else {
            self.resolve_question(question_id, "server unavailable");
            return;
        };
        self.net.send(NetCommand::Request {
            ws_key,
            request: ApiRequest::ReplyQuestion {
                request_id: question_id.to_string(),
                answers,
            },
        });
        self.repaint(cx);
    }

    pub fn dismiss_question(&mut self, question_id: &str, cx: &mut Context<Self>) {
        let mut session_id = None;
        for tile in &mut self.open_tiles {
            for item in &mut tile.items {
                if let ChatItem::Question(q) = Arc::make_mut(item)
                    && q.id == question_id
                    && q.is_pending()
                {
                    q.resolution = Some("dismissing…".into());
                    for question in &mut q.questions {
                        question.locked = true;
                    }
                    session_id = Some(q.session_id.clone());
                }
            }
        }
        let Some(session_id) = session_id else { return };
        let Some(ws_key) = self.net_key_for_session(&session_id) else {
            self.resolve_question(question_id, "server unavailable");
            return;
        };
        self.net.send(NetCommand::Request {
            ws_key,
            request: ApiRequest::RejectQuestion {
                request_id: question_id.to_string(),
            },
        });
        self.repaint(cx);
    }

    // ---- question interaction ----

    /// Toggles a question option: single-select clears siblings, multi keeps them.
    pub fn toggle_question_option(
        &mut self,
        tile_key: &str,
        question_id: &str,
        qi: usize,
        oi: usize,
        multiple: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(tile) = self.tile_by_key_mut(tile_key) else {
            return;
        };
        for item in &mut tile.items {
            if let ChatItem::Question(q) = Arc::make_mut(item) {
                if q.id != question_id {
                    continue;
                }
                let Some(entry) = q.questions.get_mut(qi) else {
                    return;
                };
                for (i, option) in entry.options.iter_mut().enumerate() {
                    if multiple && i == oi {
                        option.is_selected = !option.is_selected;
                    } else if !multiple {
                        option.is_selected = i == oi;
                    }
                }
            }
        }
        self.repaint(cx);
    }

    /// Write-through for a question custom-answer field. Deliberately
    /// notify-free: the field lives in its own leaf view (typing must not
    /// repaint the window) and the leaf nudges the tile only when the
    /// answer's filled state flips, since that is the only visible
    /// consequence outside the field (the Answer button's can_submit).
    pub fn set_question_custom(
        &mut self,
        tile_key: &str,
        question_id: &str,
        qi: usize,
        text: String,
    ) {
        let Some(tile) = self.tile_by_key_mut(tile_key) else {
            return;
        };
        // make_mut only the one question item: a blind per-item make_mut
        // deep-copies the whole transcript on every keystroke
        let Some(index) = tile
            .items
            .iter()
            .position(|i| matches!(i.as_ref(), ChatItem::Question(q) if q.id == question_id))
        else {
            return;
        };
        if let ChatItem::Question(q) = Arc::make_mut(&mut tile.items[index])
            && let Some(entry) = q.questions.get_mut(qi)
        {
            entry.custom_text = text;
        }
    }
}
