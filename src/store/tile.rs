use super::parsing::{
    apply_tool_metadata, apply_tool_state, decode_data_url, lookup_context_limit, todos_from_part,
};
use super::*;

// ---- tiles ---------------------------------------------------------------

/// Subagent runs spawned by THIS chat — the flyout's Agents section is scoped here.
#[derive(Clone, Debug)]
pub struct RunningAgentItem {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub workspace: String,
    pub title: String,
    pub is_subagent: bool,
    pub needs_attention: bool,
}

pub struct ChatTile {
    /// Grid-stable key; differs from the session id so unsent drafts are addressable.
    pub key: String,
    pub session_id: Option<String>,
    pub title: String,
    pub input: String,
    /// Backend this chat runs on; a draft carries the choice until its first
    /// send creates the session on that server.
    pub harness: HarnessKind,
    pub selected_model: Option<String>,
    pub selected_variant: Option<String>,
    pub selected_agent: Option<String>,
    pub context_text: Option<String>,
    /// fraction of the context window consumed, 0..1 (0 when the limit is unknown)
    pub context_frac: f32,
    pub auto_title_pending: bool,
    /// Shared items: the tile view snapshots this list on every render, so the
    /// payload rides an Arc and a snapshot costs pointer copies, not deep clones.
    pub items: Vec<Arc<ChatItem>>,
    pub tile_agents: Vec<RunningAgentItem>,
    pub attachments: Vec<AttachmentItem>,
    pub variants: Vec<String>,
    pub tasks: Vec<TaskItem>,
    pub queue_rows: Vec<usize>, // QueueEntry keys pinned above the composer
    pub is_focused: bool,
    /// A door-run's chat that must not surface: the tile stays out of every
    /// render and sidebar list, and the run retires it when the turn settles.
    /// Always false for tiles the user opened.
    pub background: bool,
    /// Notified only when THIS tile's content changed: its cached view
    /// (Entity::cached) re-renders on this and nothing else, so one tile's
    /// stream doesn't rebuild every open transcript. None on the with_tile
    /// checkout placeholder.
    pub rev: Option<Entity<()>>,

    role_by_message: HashMap<String, String>,
    created_by_message: HashMap<String, i64>,
    meta_by_message: HashMap<String, String>,
    tokens_by_message: HashMap<String, i64>,
    token_order: Vec<String>,
    key_by_part: HashMap<String, String>, // part id → item id
    pub(super) last_used_model: Option<String>,
    pub(super) context_limit: i64,

    // prompt-history browsing: 0 = not browsing, N = N steps back from the newest entry
    pub(super) history_index: usize,
    pub(super) history_draft: String,
    suppress_history_reset: bool,
}

impl ChatTile {
    /// Whether an open card here needs a person: a permission or a question.
    pub fn is_waiting(&self) -> bool {
        self.items.iter().any(|item| match item.as_ref() {
            ChatItem::Permission(perm) => perm.is_pending(),
            ChatItem::Question(question) => question.is_pending(),
            _ => false,
        })
    }

    /// Rebuild the variant picker list around the tile's model and keep the
    /// current pick when it still exists, else fall back to "(default)".
    /// Runs wherever the option list could have drifted: tile creation, the
    /// ModelsLoaded restore, and (list-only) set_tile_model. With
    /// `prefer_highest` (the settings checkbox), an unset/default pick
    /// upgrades to the model's top variant once real options are known.
    pub(super) fn apply_variant_list(
        &mut self,
        variants_by_model: &HashMap<String, Vec<String>>,
        prefer_highest: bool,
    ) {
        self.variants = variant_list_for(variants_by_model, &self.selected_model);
        let selected_ok = self
            .variants
            .iter()
            .any(|v| Some(v.as_str()) == self.selected_variant.as_deref());
        // a real model missing from the map means its options aren't known yet
        // (MessagesLoaded can outrun ModelsLoaded on a fresh server) — keep the
        // pick pending instead of resetting it to the sentinel the later
        // restore could not recover
        let options_pending = self
            .selected_model
            .as_deref()
            .is_some_and(|m| m != DEFAULT_MODEL_ENTRY && !variants_by_model.contains_key(m));
        if !selected_ok && !options_pending {
            self.selected_variant = Some(DEFAULT_VARIANT_ENTRY.into());
        }
        if prefer_highest
            && self.selected_variant.as_deref() == Some(DEFAULT_VARIANT_ENTRY)
            && let Some(model) = &self.selected_model
            && let Some(highest) = variants_by_model
                .get(model)
                .and_then(|vs| highest_variant(vs))
        {
            self.selected_variant = Some(highest);
        }
    }

    pub fn new(
        key: String,
        seed_model: Option<String>,
        seed_agent: Option<String>,
        seed_variant: Option<String>,
    ) -> Self {
        let variants = vec![DEFAULT_VARIANT_ENTRY.to_string()];
        let selected_variant = if seed_variant.is_some() {
            seed_variant.clone()
        } else {
            Some(DEFAULT_VARIANT_ENTRY.into())
        };
        ChatTile {
            key,
            session_id: None,
            title: DRAFT_TITLE.into(),
            input: String::new(),
            harness: HarnessKind::default(),
            selected_model: seed_model,
            selected_variant,
            selected_agent: seed_agent,
            context_text: None,
            context_frac: 0.0,
            auto_title_pending: false,
            items: Vec::new(),
            tile_agents: Vec::new(),
            attachments: Vec::new(),
            variants,
            tasks: Vec::new(),
            queue_rows: Vec::new(),
            is_focused: false,
            background: false,
            rev: None,
            role_by_message: HashMap::new(),
            created_by_message: HashMap::new(),
            meta_by_message: HashMap::new(),
            tokens_by_message: HashMap::new(),
            token_order: Vec::new(),
            key_by_part: HashMap::new(),
            last_used_model: None,
            context_limit: 0,
            history_index: 0,
            history_draft: String::new(),
            suppress_history_reset: false,
        }
    }

    pub fn is_busy(&self, store: &Store) -> bool {
        self.session_id
            .as_ref()
            .is_some_and(|id| store.running_by_session.contains(id))
    }

    pub fn is_untouched(&self) -> bool {
        self.session_id.is_none()
            && self.input.is_empty()
            && self.attachments.is_empty()
            && self.items.is_empty()
    }

    pub fn reset_history(&mut self) {
        self.history_index = 0;
        self.history_draft.clear();
    }

    /// How many user messages this tile itself has sent — the ↑/↓ recall source.
    pub fn sent_prompt_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.as_ref(), ChatItem::UserMessage { .. }))
            .count()
    }

    /// The `steps_from_end`-th sent user message of this tile (0 = newest).
    /// Empty when out of range.
    pub fn peek_sent_prompt(&self, steps_from_end: usize) -> String {
        let mut seen = 0usize;
        for item in self.items.iter().rev() {
            if let ChatItem::UserMessage { text, .. } = item.as_ref() {
                if seen == steps_from_end {
                    return text.clone();
                }
                seen += 1;
            }
        }
        String::new()
    }

    /// A sent user message's text by its item key (menu → fork target). Empty
    /// when the row is gone — the caller treats that as a stale menu click.
    pub(super) fn peek_sent_prompt_key(&self, key: usize) -> String {
        for item in &self.items {
            if let ChatItem::UserMessage { key: k, text, .. } = item.as_ref()
                && *k == key
            {
                return text.clone();
            }
        }
        String::new()
    }

    pub fn has_messages(&self) -> bool {
        !self.items.is_empty()
    }
    pub fn has_queue_rows(&self) -> bool {
        !self.queue_rows.is_empty()
    }
    pub fn has_variants(&self) -> bool {
        self.variants.len() > 1
    }
    pub fn has_tasks(&self) -> bool {
        !self.tasks.is_empty()
    }
    pub fn tasks_progress(&self) -> String {
        format!(
            "{}/{}",
            self.tasks.iter().filter(|t| t.done()).count(),
            self.tasks.len()
        )
    }
    pub fn has_tile_agents(&self) -> bool {
        !self.tile_agents.is_empty()
    }
    pub fn tile_agents_summary(&self) -> String {
        format!("{} running", self.tile_agents.len())
    }
    pub fn has_tasks_or_agents(&self) -> bool {
        self.has_tasks() || self.has_tile_agents()
    }
    pub fn has_context(&self) -> bool {
        self.context_text.is_some()
    }
    /// Display-only name for the composer pill: the segment after the last '/'.
    pub fn selected_model_short(&self) -> String {
        match &self.selected_model {
            Some(m) => m.rsplit_once('/').map(|(_, s)| s).unwrap_or(m).to_string(),
            None => String::new(),
        }
    }

    pub(super) fn set_input_direct(&mut self, text: String) {
        self.suppress_history_reset = true;
        self.input = text;
        self.suppress_history_reset = false;
    }

    pub(super) fn update_context_text(&mut self) {
        let total = self
            .token_order
            .last()
            .map(|id| self.tokens_by_message[id])
            .unwrap_or(0);
        self.context_frac = if total > 0 && self.context_limit > 0 {
            (total as f32 / self.context_limit as f32).clamp(0., 1.)
        } else {
            0.
        };
        self.context_text = if total <= 0 {
            None
        } else if self.context_limit > 0 {
            Some(format!(
                "{}% · {} / {}",
                format_percent(total as f64 * 100.0 / self.context_limit as f64),
                format_tokens(total),
                format_tokens(self.context_limit)
            ))
        } else {
            Some(format!("{} tok", format_tokens(total)))
        };
    }

    fn update_context_limit(&mut self, store: &Store) {
        self.context_limit = self
            .last_used_model
            .as_ref()
            .and_then(|key| lookup_context_limit(&store.context_limit_by_model, key))
            .unwrap_or(0);
    }

    fn meta_for(&self, message_id: Option<&String>) -> String {
        message_id
            .and_then(|id| self.meta_by_message.get(id))
            .cloned()
            .unwrap_or_default()
    }

    fn created_at_for(&self, message_id: Option<&String>) -> i64 {
        message_id
            .and_then(|id| self.created_by_message.get(id))
            .copied()
            .unwrap_or(0)
    }

    fn find_item_index(&self, item_id: &str) -> Option<usize> {
        self.items.iter().position(|i| i.matches_id(item_id))
    }

    // ---- SSE transcript building ----

    pub fn on_message_updated(&mut self, data: &serde_json::Value, store: &Store) {
        let Some(info) = data.get("info") else { return };
        let (Some(id), Some(role)) = (
            info.get("id").and_then(serde_json::Value::as_str),
            info.get("role").and_then(serde_json::Value::as_str),
        ) else {
            return;
        };
        let id = id.to_string();
        // the stream re-sends the same info snapshot constantly: guard every
        // per-event insert so an unchanged snapshot costs no allocation
        if self.role_by_message.get(&id).map(String::as_str) != Some(role) {
            self.role_by_message.insert(id.clone(), role.to_string());
        }
        if let Some(ms) = info
            .pointer("/time/created")
            .and_then(serde_json::Value::as_i64)
            && self.created_by_message.get(&id).copied() != Some(ms)
        {
            self.created_by_message.insert(id.clone(), ms);
        }
        if role != "assistant" {
            return;
        }
        let provider = info
            .get("providerID")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let model = info
            .get("modelID")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if !provider.is_empty() && !model.is_empty() {
            let next = format!("{provider}/{model}");
            if self.last_used_model.as_deref() != Some(next.as_str()) {
                self.last_used_model = Some(next);
                self.update_context_limit(store);
            }
        }
        if let Some(meta) = build_meta(info) {
            // the stream re-sends the same info snapshot constantly: skip the
            // item walk (and the make_mut deep-copy it does per text part)
            // unless the label actually moved
            if self.meta_by_message.get(&id).map(String::as_str) != Some(meta.as_str())
                && let Some(items) = self.meta_items(&id)
            {
                for key in items {
                    if let Some(index) = self.find_item_index(&key)
                        && let ChatItem::AssistantText { meta_label, .. } =
                            Arc::make_mut(&mut self.items[index])
                    {
                        *meta_label = meta.clone();
                    }
                }
                self.meta_by_message.insert(id.clone(), meta);
            }
        }
        let total = read_token_total(info);
        if total > 0 && self.tokens_by_message.get(&id).copied() != Some(total) {
            if self.tokens_by_message.insert(id.clone(), total).is_none() {
                self.token_order.push(id);
            }
            self.update_context_text();
        }
    }

    /// Item keys of assistant text parts belonging to a message (for meta refresh).
    fn meta_items(&self, message_id: &str) -> Option<Vec<String>> {
        // rebuilt from key_by_part: parts of this message were recorded when their text item was added
        let prefix = format!("{message_id}:");
        Some(
            self.key_by_part
                .iter()
                .filter(|(part_id, _)| part_id.starts_with(&prefix))
                .map(|(_, item_id)| item_id.clone())
                .collect(),
        )
    }

    /// Applies a `message.part.updated` event; returns whether anything the
    /// transcript shows actually changed — the caller skips the repaint when
    /// the stream re-sends an unchanged part.
    pub fn on_part_updated(&mut self, data: &serde_json::Value) -> bool {
        let Some(part) = data.get("part") else {
            return false;
        };
        let Some(id) = part.get("id").and_then(serde_json::Value::as_str) else {
            return false;
        };
        let part_type = part
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let message_id = part
            .get("messageID")
            .and_then(serde_json::Value::as_str)
            .map(String::from);

        // user parts are rendered locally at send time; stream echoes are skipped
        if let Some(mid) = &message_id
            && self.role_by_message.get(mid).map(String::as_str) == Some("user")
        {
            return false;
        }

        match part_type {
            "text" => {
                let item_id = format!("a{id}");
                // one scan answers both "exists?" and "where?": the push path
                // reuses the index instead of scanning the list a second time
                let index = match self.find_item_index(&item_id) {
                    Some(index) => index,
                    None => {
                        let item = ChatItem::AssistantText {
                            key: id.to_string(),
                            text: String::new(),
                            meta_label: self.meta_for(message_id.as_ref()),
                            created_ms: self.created_at_for(message_id.as_ref()),
                        };
                        self.items.push(Arc::new(item));
                        if let Some(mid) = &message_id {
                            self.key_by_part.insert(format!("{mid}:{id}"), item_id);
                        }
                        self.items.len() - 1
                    }
                };
                let text = part
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                // compare through the Arc first: make_mut on an item the render
                // snapshot still holds deep-copies the whole text just to
                // conclude "unchanged"
                if let ChatItem::AssistantText { text: t, .. } = self.items[index].as_ref()
                    && t == text
                {
                    return false;
                }
                match Arc::make_mut(&mut self.items[index]) {
                    ChatItem::AssistantText { text: t, .. } => {
                        *t = text.to_string();
                        true
                    }
                    _ => false,
                }
            }
            "reasoning" => {
                let item_id = format!("r{id}");
                let index = match self.find_item_index(&item_id) {
                    Some(index) => index,
                    None => {
                        self.items.push(Arc::new(ChatItem::Reasoning {
                            key: id.to_string(),
                            text: String::new(),
                        }));
                        self.items.len() - 1
                    }
                };
                let text = part
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if let ChatItem::Reasoning { text: t, .. } = self.items[index].as_ref()
                    && t == text
                {
                    return false;
                }
                match Arc::make_mut(&mut self.items[index]) {
                    ChatItem::Reasoning { text: t, .. } => {
                        *t = text.to_string();
                        true
                    }
                    _ => false,
                }
            }
            "tool" => {
                let item_id = format!("t{id}");
                let index = match self.find_item_index(&item_id) {
                    Some(index) => index,
                    None => {
                        let tool_name = part
                            .get("tool")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("tool");
                        self.items.push(Arc::new(ChatItem::Tool(ToolItem {
                            part_id: id.to_string(),
                            tool: tool_name.into(),
                            status: "pending".into(),
                            title: tool_name.into(),
                            file_path: String::new(),
                            diff: None,
                            numstat: None,
                            is_error: false,
                            error_preview: String::new(),
                            details: ToolDetails::default(),
                            state_sig: None,
                        })));
                        self.items.len() - 1
                    }
                };
                let Some(state) = part.get("state") else {
                    return true;
                };
                let sig = {
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    state.hash(&mut hasher);
                    hasher.finish()
                };
                if let ChatItem::Tool(t) = self.items[index].as_ref()
                    && t.state_sig == Some(sig)
                {
                    return false;
                }

                {
                    let tool = match Arc::make_mut(&mut self.items[index]) {
                        ChatItem::Tool(tool) => tool,
                        _ => return false,
                    };
                    let before = tool.clone();
                    apply_tool_state(state, tool);
                    apply_tool_metadata(state, tool);
                    tool.state_sig = Some(sig);
                    *tool != before
                }
            }
            "file" => {
                let item_id = format!("i{id}");
                if self.find_item_index(&item_id).is_some() {
                    return false;
                }
                if let (Some(url), Some(file_name)) = (
                    part.get("url").and_then(serde_json::Value::as_str),
                    part.get("filename").and_then(serde_json::Value::as_str),
                ) && let Some(png) = decode_data_url(url)
                {
                    self.items.push(Arc::new(ChatItem::Image {
                        key: id.to_string(),
                        file_name: file_name.into(),
                        png: Arc::new(png),
                        created_ms: self.created_at_for(message_id.as_ref()),
                    }));
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    pub fn on_part_removed(&mut self, data: &serde_json::Value) {
        let id = data
            .get("partID")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                data.get("part")
                    .and_then(|p| p.get("id"))
                    .and_then(serde_json::Value::as_str)
            })
            .map(String::from);
        let Some(id) = id else { return };
        if let Some(index) = self
            .items
            .iter()
            .position(|i| i.matches_part_id(id.as_str()))
        {
            self.items.remove(index);
        }
    }

    pub fn on_todos_updated(&mut self, data: &serde_json::Value) -> bool {
        self.tasks.clear();
        let Some(todos) = data.get("todos").and_then(serde_json::Value::as_array) else {
            return false;
        };
        for todo in todos {
            let Some(content) = todo.get("content").and_then(serde_json::Value::as_str) else {
                continue;
            };
            if content.is_empty() {
                continue;
            }
            let status = todo
                .get("status")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("pending");
            self.tasks.push(TaskItem {
                content: content.into(),
                status: status.into(),
            });
        }
        !self.tasks.is_empty()
    }

    /// Rebuild the transcript from stored messages (chat reopen / restore).
    pub fn restore(&mut self, messages: &[StoredMessage], store: &Store, next_key: &mut usize) {
        let mut todos: Option<Vec<TaskItem>> = None;
        for msg in messages {
            self.role_by_message
                .insert(msg.id.clone(), msg.role.clone());
            self.created_by_message
                .insert(msg.id.clone(), msg.created_ms);
            if let Some(meta) = build_meta(&msg.info) {
                self.meta_by_message.insert(msg.id.clone(), meta);
            }
            if msg.role == "assistant" {
                let provider = msg
                    .info
                    .get("providerID")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let model = msg
                    .info
                    .get("modelID")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if !provider.is_empty() && !model.is_empty() {
                    self.last_used_model = Some(format!("{provider}/{model}"));
                    self.update_context_limit(store);
                }
                let total = read_token_total(&msg.info);
                if total > 0
                    && self
                        .tokens_by_message
                        .insert(msg.id.clone(), total)
                        .is_none()
                {
                    self.token_order.push(msg.id.clone());
                }
            }
            for part in &msg.parts {
                let (Some(id), Some(part_type)) = (
                    part.get("id").and_then(serde_json::Value::as_str),
                    part.get("type").and_then(serde_json::Value::as_str),
                ) else {
                    continue;
                };
                match part_type {
                    "text" => {
                        let text = part
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("");
                        if msg.role == "user" {
                            if !text.is_empty() {
                                self.items.push(Arc::new(ChatItem::UserMessage {
                                    key: *next_key,
                                    text: text.into(),
                                    created_ms: msg.created_ms,
                                }));
                                *next_key += 1;
                            }
                        } else {
                            let item_id = format!("a{id}");
                            self.items.push(Arc::new(ChatItem::AssistantText {
                                key: id.into(),
                                text: text.into(),
                                meta_label: self.meta_for(Some(&msg.id)),
                                created_ms: msg.created_ms,
                            }));
                            self.key_by_part
                                .insert(format!("{}:{}", msg.id, id), item_id);
                        }
                    }
                    "reasoning" => {
                        let text = part
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        self.items.push(Arc::new(ChatItem::Reasoning {
                            key: id.into(),
                            text,
                        }));
                    }
                    "tool" => {
                        let tool_name = part
                            .get("tool")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("tool");
                        let mut tool = ToolItem {
                            part_id: id.into(),
                            tool: tool_name.into(),
                            status: "pending".into(),
                            title: tool_name.into(),
                            file_path: String::new(),
                            diff: None,
                            numstat: None,
                            is_error: false,
                            error_preview: String::new(),
                            details: ToolDetails::default(),
                            state_sig: None,
                        };
                        if let Some(state) = part.get("state") {
                            apply_tool_state(state, &mut tool);
                            apply_tool_metadata(state, &mut tool);
                            use std::hash::{Hash, Hasher};
                            let mut hasher = std::collections::hash_map::DefaultHasher::new();
                            state.hash(&mut hasher);
                            tool.state_sig = Some(hasher.finish());
                        }
                        if tool_name == "todowrite" {
                            todos = todos_from_part(part);
                        }
                        self.items.push(Arc::new(ChatItem::Tool(tool)));
                    }
                    "file" => {
                        if let Some(url) = part.get("url").and_then(serde_json::Value::as_str) {
                            let file_name = part
                                .get("filename")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("image.png");
                            match decode_data_url(url) {
                                Some(png) => self.items.push(Arc::new(ChatItem::Image {
                                    key: id.into(),
                                    file_name: file_name.into(),
                                    png: Arc::new(png),
                                    created_ms: msg.created_ms,
                                })),
                                None => self.items.push(Arc::new(ChatItem::SystemNote {
                                    key: *next_key,
                                    text: format!("Attachment: {file_name}"),
                                    is_error: false,
                                })),
                            }
                            *next_key += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(todos) = todos {
            self.tasks.extend(todos);
        }
        self.update_context_text();
    }
}
