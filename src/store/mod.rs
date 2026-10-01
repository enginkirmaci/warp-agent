//! The full application state: workspaces, servers, the shared tile grid,
//! transcripts, queues, permissions, questions and subagent tracking. A 1:1
//! port of MainViewModel + ChatTileViewModel + the ChatItems model.

use crate::oc::{self, ApiRequest, NetCommand, SessionInfo, StoredMessage, UiEvent};
use crate::settings::{AppSettings, PromptTemplate, UiState, WorkspaceState, DEFAULT_EXECUTABLE};
use crate::theme::{self, AccentColors, FontTokens};
use chrono::{Local, TimeZone, Timelike};
use gpui::prelude::*;
use gpui::{App, Context, Entity};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

pub const DEFAULT_MODEL_ENTRY: &str = "(server default)";
pub const DEFAULT_VARIANT_ENTRY: &str = "(default)";
pub const DEFAULT_AGENT_ENTRY: &str = "(default build)";
pub const DRAFT_TITLE: &str = "New chat";

/// ["(default)"] plus the model's variants — a tile's picker list. Stays at
/// the bare sentinel until the server's model list lands (ModelsLoaded) or
/// the model is a real key present in `variants_by_model`.
fn variant_list_for(variants_by_model: &HashMap<String, Vec<String>>, model: &Option<String>) -> Vec<String> {
    let mut variants = vec![DEFAULT_VARIANT_ENTRY.to_string()];
    if let Some(m) = model {
        if let Some(vs) = variants_by_model.get(m) {
            variants.extend(vs.iter().cloned());
        }
    }
    variants
}

pub fn format_tokens(tokens: i64) -> String {
    if tokens < 1000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.1$}k", tokens as f64 / 1000.0, 1).trim_end_matches(".0").to_string()
    } else {
        let v = tokens as f64 / 1_000_000.0;
        if (v * 100.0).fract().abs() < f64::EPSILON {
            format!("{:.2}M", v).trim_end_matches("00").trim_end_matches('.').to_string()
        } else {
            format!("{:.2}M", v).trim_end_matches('0').to_string()
        }
    }
}

pub fn format_percent(value: f64) -> String {
    if value >= 10.0 {
        format!("{:.0}", value)
    } else {
        let s = format!("{:.1}", value);
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

pub fn format_duration(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs < 1.0 {
        format!("{} ms", elapsed.as_millis())
    } else if secs < 60.0 {
        format!("{:.0}s", secs)
    } else {
        format!("{}m {:02}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    }
}

pub fn from_ms(ms: i64) -> Option<chrono::DateTime<Local>> {
    if ms > 0 {
        Local.timestamp_millis_opt(ms).single()
    } else {
        None
    }
}

pub fn message_time_label(created_ms: i64) -> String {
    from_ms(created_ms)
        .map(|at| {
            if at.date_naive() == Local::now().date_naive() {
                at.format("%H:%M").to_string()
            } else {
                at.format("%d %b %H:%M").to_string()
            }
        })
        .unwrap_or_default()
}

pub fn relative_time_label(updated_ms: i64) -> String {
    let Some(at) = from_ms(updated_ms) else { return String::new() };
    let now = Local::now();
    let span = now.signed_duration_since(at);
    if span.num_minutes() < 1 {
        "now".into()
    } else if span.num_hours() < 1 {
        format!("{}m", span.num_minutes())
    } else if span.num_days() < 1 {
        format!("{}h", span.num_hours())
    } else if span.num_days() < 7 {
        format!("{}d", span.num_days())
    } else {
        at.format("%Y-%m-%d").to_string()
    }
}

// ---- chat items ----------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct AttachmentItem {
    pub file_name: String,
    pub mime: String,
    /// file bytes behind an Arc: the composer preview and the queue clone
    /// attachments on every render, and the bytes must not move with them
    pub data: Arc<Vec<u8>>,
    pub key: usize,
}

impl AttachmentItem {
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }
}

/// Unified diff tint values for a tool's file edit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Numstat {
    pub additions: i64,
    pub deletions: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolItem {
    pub key: usize,
    pub tool: String,
    pub status: String,
    pub title: String,
    pub file_path: String,
    pub diff: Option<String>,
    pub numstat: Option<Numstat>,
    pub is_error: bool,
    pub error_preview: String,
    pub details: String,
}

impl ToolItem {
    pub fn is_running(&self) -> bool {
        self.status == "running"
    }
    pub fn is_completed(&self) -> bool {
        self.status == "completed"
    }
    pub fn has_diff(&self) -> bool {
        self.diff.as_deref().is_some_and(|d| !d.is_empty())
    }
    pub fn has_numstat(&self) -> bool {
        self.numstat.as_ref().is_some_and(|n| n.additions > 0 || n.deletions > 0)
    }
    pub fn has_details(&self) -> bool {
        !self.details.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct TaskItem {
    pub content: String,
    pub status: String,
}

impl TaskItem {
    pub fn completed(&self) -> bool {
        self.status == "completed"
    }
    pub fn in_progress(&self) -> bool {
        self.status == "in_progress"
    }
    pub fn pending(&self) -> bool {
        !matches!(self.status.as_str(), "in_progress" | "completed" | "cancelled")
    }
    pub fn cancelled(&self) -> bool {
        self.status == "cancelled"
    }
    pub fn done(&self) -> bool {
        self.completed() || self.cancelled()
    }
}

#[derive(Clone, Debug)]
pub struct PermissionItem {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub detail: String,
    pub is_v2: bool,
    pub diff: Option<String>,
    pub answer: Option<String>,
    pub was_auto_answered: bool,
    /// Seconds left until auto-allow fires; None while no countdown is running.
    pub auto_countdown: Option<i64>,
}

impl PermissionItem {
    pub fn is_pending(&self) -> bool {
        self.answer.is_none()
    }
    pub fn has_diff(&self) -> bool {
        self.diff.as_deref().is_some_and(|d| !d.is_empty())
    }
    pub fn title(&self) -> String {
        format!("Permission required — {}", self.kind)
    }
    pub fn has_auto_countdown(&self) -> bool {
        self.is_pending() && self.auto_countdown.is_some_and(|c| c > 0)
    }
    pub fn auto_countdown_text(&self) -> String {
        let left = self.auto_countdown.unwrap_or(0);
        if left >= 60 {
            format!("auto-allow in {}m {}s", left / 60, left % 60)
        } else {
            format!("auto-allow in {left}s")
        }
    }
}

#[derive(Clone, Debug)]
pub struct QuestionOptionItem {
    pub label: String,
    pub description: String,
    pub is_selected: bool,
}

#[derive(Clone, Debug)]
pub struct QuestionEntry {
    pub question: String,
    pub header: String,
    pub multiple: bool,
    pub options: Vec<QuestionOptionItem>,
    pub custom_text: String,
    pub locked: bool,
}

impl QuestionEntry {
    pub fn custom_filled(&self) -> bool {
        !self.custom_text.trim().is_empty()
    }
    pub fn has_answer(&self) -> bool {
        self.custom_filled() || self.options.iter().any(|o| o.is_selected)
    }
    /// A typed custom answer wins over the picked options; multi-select keeps every picked label.
    pub fn answer(&self) -> Vec<String> {
        if self.custom_filled() {
            vec![self.custom_text.trim().to_string()]
        } else {
            self.options
                .iter()
                .filter(|o| o.is_selected)
                .map(|o| o.label.clone())
                .collect()
        }
    }
}

#[derive(Clone, Debug)]
pub struct QuestionItem {
    pub id: String,
    pub session_id: String,
    pub questions: Vec<QuestionEntry>,
    pub resolution: Option<String>,
}

impl QuestionItem {
    pub fn is_pending(&self) -> bool {
        self.resolution.is_none()
    }
    pub fn can_submit(&self) -> bool {
        self.questions.iter().all(|q| q.has_answer())
    }
}

#[derive(Clone, Debug)]
pub enum ChatItem {
    UserMessage {
        key: usize,
        text: String,
        created_ms: i64,
    },
    AssistantText {
        key: String, // part id
        text: String,
        meta_label: String,
        created_ms: i64,
    },
    Reasoning {
        key: String,
        text: String,
        created_ms: i64,
    },
    Tool(ToolItem),
    Image {
        key: String,
        file_name: String,
        /// decoded on first render and cached in the view; Arc keeps the
        /// per-frame menu/copy clones at pointer cost
        png: Arc<Vec<u8>>,
        created_ms: i64,
    },
    Permission(PermissionItem),
    Question(QuestionItem),
    SystemNote {
        key: usize,
        text: String,
        is_error: bool,
    },
}

impl ChatItem {
    pub fn id(&self) -> String {
        match self {
            ChatItem::UserMessage { key, .. } => format!("u{key}"),
            ChatItem::AssistantText { key, .. } => format!("a{key}"),
            ChatItem::Reasoning { key, .. } => format!("r{key}"),
            ChatItem::Tool(t) => format!("t{}", t.key),
            ChatItem::Image { key, .. } => format!("i{key}"),
            ChatItem::Permission(p) => format!("p{}", p.id),
            ChatItem::Question(q) => format!("q{}", q.id),
            ChatItem::SystemNote { key, .. } => format!("n{key}"),
        }
    }
}

/// A message parked while a turn is still running; sent when the session goes idle.
#[derive(Clone, Debug)]
pub struct QueueEntry {
    pub session_id: String,
    pub text: String,
    pub attachments: Vec<AttachmentItem>,
    pub key: usize,
}

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

    role_by_message: HashMap<String, String>,
    created_by_message: HashMap<String, i64>,
    meta_by_message: HashMap<String, String>,
    tokens_by_message: HashMap<String, i64>,
    token_order: Vec<String>,
    key_by_part: HashMap<String, String>, // part id → item id
    last_used_model: Option<String>,
    context_limit: i64,

    // prompt-history browsing: 0 = not browsing, N = N steps back from the newest entry
    history_index: usize,
    history_draft: String,
    suppress_history_reset: bool,
}

impl ChatTile {
    /// Rebuild the variant picker list around the tile's model and keep the
    /// current pick when it still exists, else fall back to "(default)".
    /// Runs wherever the option list could have drifted: tile creation, the
    /// ModelsLoaded restore, and (list-only) set_tile_model.
    fn apply_variant_list(&mut self, variants_by_model: &HashMap<String, Vec<String>>) {
        self.variants = variant_list_for(variants_by_model, &self.selected_model);
        let selected_ok = self.variants.iter().any(|v| Some(v.as_str()) == self.selected_variant.as_deref());
        if !selected_ok {
            self.selected_variant = Some(DEFAULT_VARIANT_ENTRY.into());
        }
    }

    pub fn new(key: String, seed_model: Option<String>, seed_agent: Option<String>, seed_variant: Option<String>) -> Self {
        let variants = vec![DEFAULT_VARIANT_ENTRY.to_string()];
        let selected_variant = if seed_variant.is_some() { seed_variant.clone() } else { Some(DEFAULT_VARIANT_ENTRY.into()) };
        ChatTile {
            key,
            session_id: None,
            title: DRAFT_TITLE.into(),
            input: String::new(),
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
        self.session_id.as_ref().is_some_and(|id| store.running_by_session.contains(id))
    }

    pub fn is_untouched(&self) -> bool {
        self.session_id.is_none() && self.input.is_empty() && self.attachments.is_empty() && self.items.is_empty()
    }

    pub fn reset_history(&mut self) {
        self.history_index = 0;
        self.history_draft.clear();
    }

    /// How many user messages this tile itself has sent — the ↑/↓ recall source.
    pub fn sent_prompt_count(&self) -> usize {
        self.items.iter().filter(|i| matches!(i.as_ref(), ChatItem::UserMessage { .. })).count()
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
    fn peek_sent_prompt_key(&self, key: usize) -> String {
        for item in &self.items {
            if let ChatItem::UserMessage { key: k, text, .. } = item.as_ref() {
                if *k == key {
                    return text.clone();
                }
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
        format!("{}/{}", self.tasks.iter().filter(|t| t.done()).count(), self.tasks.len())
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
    pub fn has_tasks_and_agents(&self) -> bool {
        self.has_tasks() && self.has_tile_agents()
    }
    pub fn has_context(&self) -> bool {
        self.context_text.is_some()
    }
    pub fn has_attachments(&self) -> bool {
        !self.attachments.is_empty()
    }

    /// Display-only name for the composer pill: the segment after the last '/'.
    pub fn selected_model_short(&self) -> String {
        match &self.selected_model {
            Some(m) => m.rsplit_once('/').map(|(_, s)| s).unwrap_or(m).to_string(),
            None => String::new(),
        }
    }

    fn set_input_direct(&mut self, text: String) {
        self.suppress_history_reset = true;
        self.input = text;
        self.suppress_history_reset = false;
    }

    fn refresh_variants(&mut self, store: &Store, seed_from_settings: bool) {
        self.variants.clear();
        self.variants.push(DEFAULT_VARIANT_ENTRY.to_string());
        if let Some(model) = &self.selected_model {
            if let Some(variants) = store.variants_by_model.get(model) {
                self.variants.extend(variants.iter().cloned());
            }
        }
        self.selected_variant = if seed_from_settings {
            store.seed_variant()
        } else {
            Some(DEFAULT_VARIANT_ENTRY.into())
        };
    }

    fn update_context_text(&mut self) {
        let total = self.token_order.last().map(|id| self.tokens_by_message[id]).unwrap_or(0);
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
            .and_then(|key| store.context_limit_by_model.get(key))
            .copied()
            .unwrap_or(0);
    }

    fn meta_for(&self, message_id: Option<&String>) -> String {
        message_id.and_then(|id| self.meta_by_message.get(id)).cloned().unwrap_or_default()
    }

    fn created_at_for(&self, message_id: Option<&String>) -> i64 {
        message_id
            .and_then(|id| self.created_by_message.get(id))
            .copied()
            .unwrap_or(0)
    }

    fn find_item_index(&self, item_id: &str) -> Option<usize> {
        self.items.iter().position(|i| i.id() == item_id)
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
        self.role_by_message.insert(id.clone(), role.to_string());
        if let Some(ms) = info.pointer("/time/created").and_then(serde_json::Value::as_i64) {
            self.created_by_message.insert(id.clone(), ms);
        }
        if role != "assistant" {
            return;
        }
        let provider = info.get("providerID").and_then(serde_json::Value::as_str).unwrap_or("");
        let model = info.get("modelID").and_then(serde_json::Value::as_str).unwrap_or("");
        if !provider.is_empty() && !model.is_empty() {
            self.last_used_model = Some(format!("{provider}/{model}"));
            self.update_context_limit(store);
        }
        if let Some(meta) = build_meta(info) {
            if let Some(items) = self.meta_items(&id) {
                for key in items {
                    if let Some(index) = self.find_item_index(&key) {
                        if let ChatItem::AssistantText { meta_label, .. } = Arc::make_mut(&mut self.items[index]) {
                            *meta_label = meta.clone();
                        }
                    }
                }
            }
            self.meta_by_message.insert(id.clone(), meta);
        }
        let total = read_token_total(info);
        if total > 0 {
            if self.tokens_by_message.insert(id.clone(), total).is_none() {
                self.token_order.push(id);
            }
            self.update_context_text();
        }
    }

    /// Item keys of assistant text parts belonging to a message (for meta refresh).
    fn meta_items(&self, message_id: &str) -> Option<Vec<String>> {
        // rebuilt from key_by_part: parts of this message were recorded when their text item was added
        Some(
            self.key_by_part
                .iter()
                .filter(|(part_id, _)| part_id.starts_with(&format!("{message_id}:")))
                .map(|(_, item_id)| item_id.clone())
                .collect(),
        )
    }

    /// Applies a `message.part.updated` event; returns whether anything the
    /// transcript shows actually changed — the caller skips the repaint when
    /// the stream re-sends an unchanged part.
    pub fn on_part_updated(&mut self, data: &serde_json::Value, next_key: &mut usize) -> bool {
        let Some(part) = data.get("part") else { return false };
        let Some(id) = part.get("id").and_then(serde_json::Value::as_str) else { return false };
        let part_type = part.get("type").and_then(serde_json::Value::as_str).unwrap_or("");
        let message_id = part.get("messageID").and_then(serde_json::Value::as_str).map(String::from);

        // user parts are rendered locally at send time; stream echoes are skipped
        if let Some(mid) = &message_id {
            if self.role_by_message.get(mid).map(String::as_str) == Some("user") {
                return false;
            }
        }

        match part_type {
            "text" => {
                let item_id = format!("a{id}");
                if self.find_item_index(&item_id).is_none() {
                    let item = ChatItem::AssistantText {
                        key: id.to_string(),
                        text: String::new(),
                        meta_label: self.meta_for(message_id.as_ref()),
                        created_ms: self.created_at_for(message_id.as_ref()),
                    };
                    self.items.push(Arc::new(item));
                    if let Some(mid) = &message_id {
                        self.key_by_part.insert(format!("{mid}:{id}"), item_id.clone());
                    }
                }
                let text = part.get("text").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                let Some(index) = self.find_item_index(&item_id) else { return false };
                let changed = match Arc::make_mut(&mut self.items[index]) {
                    ChatItem::AssistantText { text: t, .. } => {
                        let changed = *t != text;
                        if changed {
                            *t = text;
                        }
                        changed
                    }
                    _ => false,
                };
                changed
            }
            "reasoning" => {
                let item_id = format!("r{id}");
                if self.find_item_index(&item_id).is_none() {
                    self.items.push(Arc::new(ChatItem::Reasoning {
                        key: id.to_string(),
                        text: String::new(),
                        created_ms: self.created_at_for(message_id.as_ref()),
                    }));
                }
                let text = part.get("text").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                let Some(index) = self.find_item_index(&item_id) else { return false };
                match Arc::make_mut(&mut self.items[index]) {
                    ChatItem::Reasoning { text: t, .. } => {
                        let changed = *t != text;
                        if changed {
                            *t = text;
                        }
                        changed
                    }
                    _ => false,
                }
            }
            "tool" => {
                let item_id = format!("t{id}");
                if self.find_item_index(&item_id).is_none() {
                    let tool_name = part.get("tool").and_then(serde_json::Value::as_str).unwrap_or("tool");
                    self.items.push(Arc::new(ChatItem::Tool(ToolItem {
                        key: *next_key,
                        tool: tool_name.into(),
                        status: "pending".into(),
                        title: tool_name.into(),
                        file_path: String::new(),
                        diff: None,
                        numstat: None,
                        is_error: false,
                        error_preview: String::new(),
                        details: String::new(),
                    })));
                    *next_key += 1;
                }
                let Some(state) = part.get("state") else { return true };
                let Some(index) = self.find_item_index(&item_id) else { return false };
                let changed = {
                    let tool = match Arc::make_mut(&mut self.items[index]) {
                        ChatItem::Tool(tool) => tool,
                        _ => return false,
                    };
                    let before = tool.clone();
                    apply_tool_state(state, tool);
                    apply_tool_metadata(state, tool);
                    *tool != before
                };
                changed
            }
            "file" => {
                let item_id = format!("i{id}");
                if self.find_item_index(&item_id).is_some() {
                    return false;
                }
                if let (Some(url), Some(file_name)) = (
                    part.get("url").and_then(serde_json::Value::as_str),
                    part.get("filename").and_then(serde_json::Value::as_str),
                ) {
                    if let Some(png) = decode_data_url(url) {
                        self.items.push(Arc::new(ChatItem::Image {
                            key: id.to_string(),
                            file_name: file_name.into(),
                            png: Arc::new(png),
                            created_ms: self.created_at_for(message_id.as_ref()),
                        }));
                        return true;
                    }
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
        for prefix in ["a", "r", "t", "i"] {
            let item_id = format!("{prefix}{id}");
            if let Some(index) = self.find_item_index(&item_id) {
                self.items.remove(index);
                return;
            }
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
            let status = todo.get("status").and_then(serde_json::Value::as_str).unwrap_or("pending");
            self.tasks.push(TaskItem { content: content.into(), status: status.into() });
        }
        !self.tasks.is_empty()
    }

    /// Rebuild the transcript from stored messages (chat reopen / restore).
    pub fn restore(&mut self, messages: &[StoredMessage], store: &Store, next_key: &mut usize) {
        let mut todos: Option<Vec<TaskItem>> = None;
        for msg in messages {
            self.role_by_message.insert(msg.id.clone(), msg.role.clone());
            self.created_by_message.insert(msg.id.clone(), msg.created_ms);
            if let Some(meta) = build_meta(&msg.info) {
                self.meta_by_message.insert(msg.id.clone(), meta);
            }
            if msg.role == "assistant" {
                let provider = msg.info.get("providerID").and_then(serde_json::Value::as_str).unwrap_or("");
                let model = msg.info.get("modelID").and_then(serde_json::Value::as_str).unwrap_or("");
                if !provider.is_empty() && !model.is_empty() {
                    self.last_used_model = Some(format!("{provider}/{model}"));
                    self.update_context_limit(store);
                }
                let total = read_token_total(&msg.info);
                if total > 0 {
                    if self.tokens_by_message.insert(msg.id.clone(), total).is_none() {
                        self.token_order.push(msg.id.clone());
                    }
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
                        let text = part.get("text").and_then(serde_json::Value::as_str).unwrap_or("");
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
                            self.key_by_part.insert(format!("{}:{}", msg.id, id), item_id);
                        }
                    }
                    "reasoning" => {
                        let text = part.get("text").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                        self.items.push(Arc::new(ChatItem::Reasoning { key: id.into(), text, created_ms: msg.created_ms }));
                    }
                    "tool" => {
                        let tool_name = part.get("tool").and_then(serde_json::Value::as_str).unwrap_or("tool");
                        let mut tool = ToolItem {
                            key: *next_key,
                            tool: tool_name.into(),
                            status: "pending".into(),
                            title: tool_name.into(),
                            file_path: String::new(),
                            diff: None,
                            numstat: None,
                            is_error: false,
                            error_preview: String::new(),
                            details: String::new(),
                        };
                        *next_key += 1;
                        if let Some(state) = part.get("state") {
                            apply_tool_state(state, &mut tool);
                            apply_tool_metadata(state, &mut tool);
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

// ---- JSON shape parsing (ChatParsing port) -------------------------------

pub fn read_token_total(info: &serde_json::Value) -> i64 {
    let Some(tokens) = info.get("tokens").and_then(serde_json::Value::as_object) else {
        return 0;
    };
    let sum = |name: &str| tokens.get(name).and_then(serde_json::Value::as_i64).unwrap_or(0);
    let (mut cache_read, mut cache_write) = (0, 0);
    if let Some(cache) = tokens.get("cache").and_then(serde_json::Value::as_object) {
        cache_read = cache.get("read").and_then(serde_json::Value::as_i64).unwrap_or(0);
        cache_write = cache.get("write").and_then(serde_json::Value::as_i64).unwrap_or(0);
    }
    sum("input") + sum("output") + sum("reasoning") + cache_read + cache_write
}

/// Compact per-message detail line: token flow, cost.
pub fn build_meta(info: &serde_json::Value) -> Option<String> {
    if !info.is_object() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(tokens) = info.get("tokens").and_then(serde_json::Value::as_object) {
        let input = tokens.get("input").and_then(serde_json::Value::as_i64).unwrap_or(0);
        let output = tokens.get("output").and_then(serde_json::Value::as_i64).unwrap_or(0);
        if input > 0 || output > 0 {
            parts.push(format!("↑{} ↓{}", format_tokens(input), format_tokens(output)));
        }
    }
    let cost = info.get("cost").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
    if cost > 0.0 {
        parts.push(format!("${:.4}", cost).trim_end_matches('0').trim_end_matches('.').to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn describe_part(state: &serde_json::Value, name: &str) -> String {
    let Some(value) = state.get(name) else { return String::new() };
    if value.is_null() {
        return String::new();
    }
    let text = if let Some(s) = value.as_str() {
        s.to_string()
    } else {
        serde_json::to_string_pretty(value).unwrap_or_default()
    };
    if text.is_empty() {
        String::new()
    } else if text.len() <= 4000 {
        text
    } else {
        // cut on a char boundary — byte 4000 can split a multibyte char
        let mut cut = 4000;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n… (truncated)", &text[..cut])
    }
}

fn build_tool_details(input: &str, output: &str, error: &str) -> String {
    let mut out = String::new();
    if !input.is_empty() {
        out.push_str("input:\n");
        out.push_str(input);
        out.push('\n');
    }
    if !output.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("output:\n");
        out.push_str(output);
        out.push('\n');
    }
    if !error.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("error:\n");
        out.push_str(error);
        out.push('\n');
    }
    out
}

fn apply_tool_state(state: &serde_json::Value, tool: &mut ToolItem) {
    if let Some(status) = state.get("status").and_then(serde_json::Value::as_str) {
        tool.status = status.into();
        tool.is_error = status.eq_ignore_ascii_case("error");
    }
    if let Some(title) = state.get("title").and_then(serde_json::Value::as_str) {
        if !title.is_empty() {
            tool.title = title.into();
        }
    }
    let input = describe_part(state, "input");
    let output = describe_part(state, "output");
    let error = describe_part(state, "error");
    tool.error_preview = error.split('\n').next().unwrap_or("").to_string();
    tool.details = build_tool_details(&input, &output, &error);
}

fn apply_tool_metadata(state: &serde_json::Value, tool: &mut ToolItem) {
    let Some(md) = state.get("metadata").and_then(serde_json::Value::as_object) else {
        return;
    };
    if let Some(file_path) = md.get("filepath").and_then(serde_json::Value::as_str) {
        if !file_path.is_empty() {
            tool.file_path = file_path.into();
        }
    }
    if let Some(fd) = md.get("filediff").and_then(serde_json::Value::as_object) {
        tool.diff = fd.get("patch").and_then(serde_json::Value::as_str).map(String::from);
        tool.numstat = Some(Numstat {
            additions: fd.get("additions").and_then(serde_json::Value::as_i64).unwrap_or(0),
            deletions: fd.get("deletions").and_then(serde_json::Value::as_i64).unwrap_or(0),
        });
    }
}

/// Last todowrite tool part in the restored transcript carries the session's todos.
fn todos_from_part(part: &serde_json::Value) -> Option<Vec<TaskItem>> {
    let state = part.get("state")?;
    let list = state
        .pointer("/input/todos")
        .filter(|v| v.is_array())
        .or_else(|| state.pointer("/metadata/todos").filter(|v| v.is_array()))?;
    let mut result = Vec::new();
    for todo in list.as_array()? {
        let Some(content) = todo.get("content").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if content.is_empty() {
            continue;
        }
        let status = todo.get("status").and_then(serde_json::Value::as_str).unwrap_or("pending");
        result.push(TaskItem { content: content.into(), status: status.into() });
    }
    Some(result)
}

fn decode_data_url(url: &str) -> Option<Vec<u8>> {
    let rest = url.strip_prefix("data:image")?;
    let comma = rest.find(',')?;
    let base64_part = &rest[comma + 1..];
    decode_base64(base64_part)
}

pub fn decode_base64(input: &str) -> Option<Vec<u8>> {
    const INVALID: u8 = 255;
    fn table() -> [u8; 256] {
        let mut t = [INVALID; 256];
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        for (i, &c) in alphabet.iter().enumerate() {
            t[c as usize] = i as u8;
        }
        t
    }
    let table = table();
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &c in input.as_bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = table[c as usize];
        if v == INVALID {
            return None;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

pub fn encode_base64(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn derive_title(text: &str) -> String {
    let mut first_line = text.split('\n').next().unwrap_or("").trim().to_string();
    for extra in ["\r", "\t"] {
        first_line = first_line.replace(extra, " ");
    }
    while first_line.contains("  ") {
        first_line = first_line.replace("  ", " ");
    }
    if first_line.chars().count() <= 48 {
        first_line
    } else {
        let cut: String = first_line.chars().take(48).collect();
        format!("{}…", cut.trim_end())
    }
}

/// The fork seed: the transcript up to and including the chosen user message
/// (`stop_key`) flattened to a plain-text block for the new session's
/// composer. User lines keep their text, assistant replies get a role tag;
/// reasoning/tools stay out — the block is context for a manual send, not a
/// replay.
fn fork_transcript(tile: &ChatTile, stop_key: usize, source_title: &str) -> String {
    let mut out = format!("— forked from {source_title} —\n");
    for item in &tile.items {
        match item.as_ref() {
            ChatItem::UserMessage { key, text, .. } => {
                out.push_str("\nYou: ");
                out.push_str(text);
                if *key == stop_key {
                    return out;
                }
            }
            ChatItem::AssistantText { text, .. } => {
                out.push_str("\nAssistant: ");
                out.push_str(text);
            }
            _ => {}
        }
    }
    out
}

// ---- sidebar models ------------------------------------------------------

#[derive(Clone)]
pub struct WorkspaceItem {
    pub state: WorkspaceState,
    pub name: String,
    pub is_running: bool,
    pub is_selected: bool,
    pub chats: Vec<crate::store::SessionItem>,
}

impl WorkspaceItem {
    pub fn new(state: WorkspaceState) -> Self {
        let general = state.path == crate::paths::AppPaths::general_workspace_path().to_string_lossy();
        let name = if general {
            "General".to_string()
        } else {
            std::path::Path::new(&state.path)
                .file_name()
                .and_then(|n| n.to_str())
                .filter(|n| !n.is_empty())
                .unwrap_or(&state.path)
                .to_string()
        };
        Self { state, name, is_running: false, is_selected: false, chats: Vec::new() }
    }

    pub fn folder_path(&self) -> &str {
        &self.state.path
    }

    pub fn is_general(&self) -> bool {
        self.state.path == crate::paths::AppPaths::general_workspace_path().to_string_lossy()
    }

    pub fn tooltip_text(&self) -> String {
        if self.is_general() {
            "General chat — not tied to a folder".into()
        } else {
            self.state.path.clone()
        }
    }

    pub fn is_expanded(&self) -> bool {
        !self.state.collapsed
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        self.state.collapsed = !expanded;
    }

    pub fn has_chats(&self) -> bool {
        !self.chats.is_empty()
    }

    pub fn show_empty_hint(&self) -> bool {
        self.is_running && self.chats.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct SessionItem {
    pub id: String,
    pub title: String,
    pub updated_ms: i64,
    pub is_pinned: bool,
    pub is_selected: bool,
    pub is_running: bool,
    pub is_editing: bool,
    pub draft_title: String,
}

impl SessionItem {
    pub fn updated_label(&self) -> String {
        relative_time_label(self.updated_ms)
    }
    pub fn pin_visible(&self) -> bool {
        self.is_pinned && !self.is_running
    }
}

// ---- the store -----------------------------------------------------------

struct WorkspaceRuntime {
    events_running: bool,
    /// live state of the workspace's event pump: drives the tile
    /// "reconnecting" pill and the amber gauge leg
    stream_status: oc::StreamStatus,
    /// latest /global/health answer; None until the first poll lands
    /// (salmon gauge leg while Some(false))
    healthy: Option<bool>,
}

#[derive(Clone)]
pub struct SubagentInfo {
    pub parent_session_id: String,
    pub agent: String,
}

pub struct Store {
    pub settings: AppSettings,
    pub ui: UiState,
    pub accent: AccentColors,
    pub fonts: FontTokens,
    pub system_font_factor: f32,
    pub tool_tips_visible: bool,

    pub workspaces: Vec<WorkspaceItem>,
    pub selected_workspace: Option<String>, // folder path
    pub sessions: Vec<SessionItem>,         // the selected workspace's sidebar mirror
    pub models: Vec<String>,
    pub agents: Vec<String>,
    pub running_agents: Vec<RunningAgentItem>,

    pub open_tiles: Vec<ChatTile>,
    pub focused_tile: Option<String>, // tile key

    pub is_server_running: bool,
    pub auto_allow: bool,
    pub is_settings_open: bool,
    pub show_restore_prompt: bool,
    pub restore_prompt_text: String,

    pub sidebar_shown_width: f32,
    pub sidebar_visible: bool,

    pub next_key: usize,
    pub next_tile_key: usize,

    // event handling maps (MainViewModel fields)
    net: crate::oc::NetSender,
    runtimes: HashMap<String, WorkspaceRuntime>,
    workspace_by_session: HashMap<String, String>,
    title_by_session: HashMap<String, String>,
    pending_by_session: HashMap<String, Vec<PermissionItem>>,
    pending_questions_by_session: HashMap<String, Vec<QuestionItem>>,
    tile_by_session: HashMap<String, String>, // session id → tile key
    turn_start_by_session: HashMap<String, Instant>,
    user_stopped_by_session: HashSet<String>,
    pub queue: Vec<QueueEntry>,
    workspace_generation: u64,
    running_by_session: HashSet<String>,
    variants_by_model: HashMap<String, Vec<String>>,
    context_limit_by_model: HashMap<String, i64>,
    subagent_by_session: HashMap<String, SubagentInfo>,
    running_subagents: HashSet<String>,
    /// auto-allow countdowns keyed by permission id
    countdowns: HashMap<String, i64>,
    prompt_history: Vec<String>,
    /// draft tiles whose session creation is in flight, with the send payload
    /// (text + attachments) stashed until the reply lands
    pending_sends: Vec<(String, String, Vec<AttachmentItem>)>,
    /// workspaces started by Restore whose open-tile lists reload on ServerStarted
    pending_restores: Vec<(String, Vec<String>)>,
    /// chats requested while their workspace server was still starting
    pending_opens: Vec<(String, String)>,
    /// tile whose flyout should spring open after a live todo batch
    pub todos_spring: Option<String>,
}

impl Store {
    pub fn new(cx: &mut App, net: crate::oc::NetSender) -> Entity<Self> {
        let settings = AppSettings::load();
        let mut ui = UiState::load();
        // one-time migration: the pre-settings auto-allow choice moves into settings.json
        if settings.auto_allow_always.is_none() && ui.auto_allow_always.is_some() {
            // keep reading from ui state like the C# fallback until settings has it
        }
        let system_font_factor = 1.0;
        let accent_preset = theme::normalize_accent(settings.accent.as_deref());
        let fonts = theme::font_tokens(settings.font_scale.as_deref(), system_font_factor);
        let auto_allow = ui.auto_allow_always.unwrap_or(true);

        if ui.workspaces.is_empty() {
            if let Some(folder) = ui.folder.clone().filter(|f| !f.trim().is_empty()) {
                ui.workspaces.insert(0, WorkspaceState { path: normalize_dir(&folder), ..Default::default() });
            }
        }

        let mut store = Self {
            settings: settings.clone(),
            ui: ui.clone(),
            accent: AccentColors::for_preset(accent_preset),
            fonts,
            system_font_factor,
            tool_tips_visible: settings.show_tool_tips,
            workspaces: Vec::new(),
            selected_workspace: None,
            sessions: Vec::new(),
            models: vec![DEFAULT_MODEL_ENTRY.into()],
            agents: vec![DEFAULT_AGENT_ENTRY.into()],
            running_agents: Vec::new(),
            open_tiles: Vec::new(),
            focused_tile: None,
            is_server_running: false,
            auto_allow,
            is_settings_open: false,
            show_restore_prompt: false,
            restore_prompt_text: String::new(),
            sidebar_shown_width: ui
                .sidebar_width
                .unwrap_or(theme::SIDEBAR_DEFAULT_WIDTH as f64)
                .clamp(theme::SIDEBAR_MIN_WIDTH as f64, theme::SIDEBAR_MAX_WIDTH as f64)
                as f32,
            sidebar_visible: ui.sidebar_visible.unwrap_or(true),
            next_key: 1,
            next_tile_key: 1,
            net,
            runtimes: HashMap::new(),
            workspace_by_session: HashMap::new(),
            title_by_session: HashMap::new(),
            pending_by_session: HashMap::new(),
            pending_questions_by_session: HashMap::new(),
            tile_by_session: HashMap::new(),
            turn_start_by_session: HashMap::new(),
            user_stopped_by_session: HashSet::new(),
            queue: Vec::new(),
            workspace_generation: 0,
            running_by_session: HashSet::new(),
            variants_by_model: HashMap::new(),
            context_limit_by_model: HashMap::new(),
            subagent_by_session: HashMap::new(),
            running_subagents: HashSet::new(),
            countdowns: HashMap::new(),
            prompt_history: Vec::new(),
            pending_sends: Vec::new(),
            pending_restores: Vec::new(),
            pending_opens: Vec::new(),
            todos_spring: None,
        };

        // ensure the General workspace exists and leads the list
        let general_path = crate::paths::AppPaths::general_workspace_path().to_string_lossy().to_string();
        if !store.ui.workspaces.iter().any(|w| w.path == general_path) {
            store.ui.workspaces.insert(0, WorkspaceState { path: general_path.clone(), ..Default::default() });
        } else if store.ui.workspaces[0].path != general_path {
            let index = store.ui.workspaces.iter().position(|w| w.path == general_path).unwrap();
            let general = store.ui.workspaces.remove(index);
            store.ui.workspaces.insert(0, general);
        }
        let _ = std::fs::create_dir_all(&general_path);

        // every launch starts with the workspace sections collapsed, whatever was saved
        for ws in &mut store.ui.workspaces {
            ws.collapsed = true;
            // saved open-tile ids of chats that no longer exist could never restore
            ws.open_sessions.retain(|id| ws.sessions.contains(id));
        }
        let pending_tiles: usize = store.ui.workspaces.iter().map(|w| w.open_sessions.len()).sum();
        store.restore_prompt_text = if pending_tiles == 1 {
            "1 chat was open last time".into()
        } else {
            format!("{pending_tiles} chats were open last time")
        };
        store.show_restore_prompt = pending_tiles > 0;

        store.workspaces = store.ui.workspaces.clone().into_iter().map(WorkspaceItem::new).collect();
        for ws in &store.workspaces {
            for sid in &ws.state.sessions {
                store.workspace_by_session.insert(sid.clone(), ws.folder_path().to_string());
            }
        }

        // restore the last workspace selection but never auto-start a server
        let selected = store
            .workspaces
            .iter()
            .find(|w| Some(w.folder_path()) == store.ui.active_workspace.as_deref())
            .map(|w| w.folder_path().to_string())
            .or_else(|| store.workspaces.first().map(|w| w.folder_path().to_string()));
        let entity = cx.new(|_| store);
        entity.update(cx, |store, cx| {
            if let Some(path) = selected {
                store.select_workspace(&path, cx);
            }
        });
        entity
    }

    // ---- seeds / persistence ----

    pub fn seed_model(&self) -> Option<String> {
        self.ui.model.clone().or_else(|| self.settings.default_model.clone())
    }
    pub fn seed_variant(&self) -> Option<String> {
        self.settings.default_variant.clone()
    }
    pub fn seed_agent(&self) -> Option<String> {
        self.ui.agent.clone().or_else(|| self.settings.default_agent.clone())
    }

    pub fn save_state(&mut self) {
        self.ui.active_workspace = self.selected_workspace.clone();
        self.ui.auto_allow_always = Some(self.auto_allow);
        self.persist_tiles();
        self.persist_drafts();
        // persist_tiles (and most mutations) write the runtime WorkspaceItems;
        // ui.workspaces is the mirror that actually gets serialized, so copy
        // the persisted fields back before saving
        for ws in &mut self.ui.workspaces {
            if let Some(item) = self.workspaces.iter().find(|i| i.folder_path() == ws.path) {
                ws.sessions = item.state.sessions.clone();
                ws.active_session = item.state.active_session.clone();
                ws.open_sessions = item.state.open_sessions.clone();
                ws.collapsed = item.state.collapsed;
            }
        }
        self.ui.save();
    }

    /// Merge open tiles' composer text over the saved draft map. Closing a
    /// tile keeps its draft (the chat lives on in the sidebar); drafts for
    /// sessions that no longer exist anywhere are dropped.
    fn persist_drafts(&mut self) {
        for tile in &self.open_tiles {
            let Some(sid) = &tile.session_id else { continue };
            if tile.input.trim().is_empty() {
                self.ui.drafts.remove(sid);
            } else {
                self.ui.drafts.insert(sid.clone(), tile.input.clone());
            }
        }
        self.ui.drafts.retain(|sid, _| {
            self.tile_by_session.contains_key(sid)
                || self.workspaces.iter().any(|w| w.state.sessions.contains(sid))
        });
    }

    fn persist_tiles(&mut self) {
        for ws in &mut self.workspaces {
            if !self.runtimes.contains_key(ws.folder_path()) {
                continue;
            }
            let ids: Vec<String> = self
                .open_tiles
                .iter()
                .filter_map(|t| t.session_id.clone())
                .filter(|sid| self.workspace_by_session.get(sid).map(|p| p == ws.folder_path()).unwrap_or(false))
                .collect();
            ws.state.open_sessions = ids;
        }
        if let Some(focus_key) = self.focused_tile.clone() {
            let focus_session = self
                .tile_by_key(&focus_key)
                .and_then(|t| t.session_id.clone());
            if let Some(sid) = focus_session {
                let owner = self.workspace_by_session.get(&sid).cloned();
                if let Some(ws) = self.selected_workspace_mut() {
                    if owner.as_deref() == Some(ws.folder_path()) {
                        ws.state.active_session = Some(sid);
                    }
                }
            }
        }
    }

    pub fn persist_composer(&mut self) {
        let (model, variant, agent) = {
            let tile = self.focused_tile();
            (
                tile.and_then(|t| t.selected_model.clone()).filter(|m| m != DEFAULT_MODEL_ENTRY),
                tile.and_then(|t| t.selected_variant.clone()).filter(|v| v != DEFAULT_VARIANT_ENTRY),
                tile.and_then(|t| t.selected_agent.clone()).filter(|a| a != DEFAULT_AGENT_ENTRY),
            )
        };
        self.ui.model = model;
        self.ui.variant = variant;
        self.ui.agent = agent;
        self.ui.save();
    }

    // ---- prompt library (ui.json "Prompts") ----

    /// Insert or update a prompt template; the library is client-side state,
    /// so every mutation rewrites ui.json immediately. `replace` names the
    /// entry being edited (rename-safe); None appends. The name is normalized
    /// (whitespace and `/` stripped) and must be unique — a rename onto an
    /// existing name replaces that entry instead of duplicating it.
    pub fn save_prompt(&mut self, replace: Option<&str>, name: &str, body: &str, cx: &mut Context<Self>) {
        let name = PromptTemplate::normalize_name(name);
        if name.is_empty() || body.trim().is_empty() {
            return;
        }
        if replace != Some(name.as_str()) {
            self.ui.prompts.retain(|t| t.name != name);
        }
        match replace.and_then(|original| self.ui.prompts.iter_mut().find(|t| t.name == original)) {
            Some(existing) => {
                existing.name = name;
                existing.body = body.trim_end().to_string();
            }
            None => self.ui.prompts.push(PromptTemplate { name, body: body.trim_end().to_string() }),
        }
        self.ui.save();
        cx.notify();
    }

    pub fn delete_prompt(&mut self, name: &str, cx: &mut Context<Self>) {
        self.ui.prompts.retain(|t| t.name != name);
        self.ui.save();
        cx.notify();
    }

    // ---- lookups ----

    pub fn selected_workspace(&self) -> Option<&WorkspaceItem> {
        self.selected_workspace
            .as_deref()
            .and_then(|path| self.workspaces.iter().find(|w| w.folder_path() == path))
    }

    pub fn selected_workspace_mut(&mut self) -> Option<&mut WorkspaceItem> {
        let path = self.selected_workspace.clone()?;
        self.workspaces.iter_mut().find(|w| w.folder_path() == path)
    }

    pub fn workspace_by_path_mut(&mut self, path: &str) -> Option<&mut WorkspaceItem> {
        self.workspaces.iter_mut().find(|w| w.folder_path() == path)
    }

    pub fn tile_by_key(&self, key: &str) -> Option<&ChatTile> {
        self.open_tiles.iter().find(|t| t.key == key)
    }

    pub fn tile_by_key_mut(&mut self, key: &str) -> Option<&mut ChatTile> {
        self.open_tiles.iter_mut().find(|t| t.key == key)
    }

    pub fn focused_tile(&self) -> Option<&ChatTile> {
        self.focused_tile.as_deref().and_then(|key| self.tile_by_key(key))
    }

    pub fn tile_for_session(&self, session_id: &str) -> Option<&ChatTile> {
        self.tile_by_session.get(session_id).and_then(|key| self.tile_by_key(key))
    }

    pub fn has_runtime(&self, session_id: &str) -> bool {
        self.effective_workspace_path(session_id)
            .map(|p| self.runtimes.contains_key(&p))
            .unwrap_or(false)
    }

    /// Whether this session's workspace event stream is currently reconnecting.
    /// A session without a workspace (fresh draft) falls back to the selected
    /// workspace; a stopped workspace has no runtime and never reads true.
    pub fn stream_reconnecting(&self, session_id: Option<&str>) -> bool {
        let path = session_id
            .and_then(|id| self.effective_workspace_path(id))
            .or_else(|| self.selected_workspace.clone());
        path.and_then(|p| self.runtimes.get(&p))
            .map(|rt| rt.stream_status == oc::StreamStatus::Reconnecting)
            .unwrap_or(false)
    }

    /// Gauge inputs over every running workspace; any reconnecting server
    /// outranks a merely unhealthy one (checked first at the call site).
    pub fn any_stream_reconnecting(&self) -> bool {
        self.runtimes.values().any(|rt| rt.stream_status == oc::StreamStatus::Reconnecting)
    }

    pub fn any_stream_unhealthy(&self) -> bool {
        self.runtimes.values().any(|rt| rt.healthy == Some(false))
    }

    pub fn is_busy(&self, session_id: &str) -> bool {
        self.running_by_session.contains(session_id)
    }

    pub fn any_busy(&self) -> bool {
        !self.running_by_session.is_empty()
    }

    pub fn working_label(&self) -> String {
        if self.any_busy() {
            format!("{} working", self.running_by_session.len())
        } else {
            "working".into()
        }
    }

    pub fn tile_columns(&self) -> usize {
        ((self.open_tiles.len() as f64).sqrt().ceil() as usize).max(1)
    }

    pub fn has_tiles(&self) -> bool {
        !self.open_tiles.is_empty()
    }

    pub fn has_no_workspace(&self) -> bool {
        self.workspaces.is_empty()
    }

    pub fn has_idle_workspace(&self) -> bool {
        !self.workspaces.is_empty() && !self.is_server_running
    }

    pub fn has_running_workspace(&self) -> bool {
        !self.workspaces.is_empty() && self.is_server_running
    }

    pub fn multiple_workspaces(&self) -> bool {
        self.workspaces.len() > 1
    }

    pub fn has_agents(&self) -> bool {
        self.agents.len() > 1
    }

    pub fn has_attention_needed(&self) -> bool {
        self.running_agents.iter().any(|r| r.needs_attention)
    }

    fn effective_workspace_path(&self, session_id: &str) -> Option<String> {
        if let Some(path) = self.workspace_by_session.get(session_id) {
            return Some(path.clone());
        }
        if let Some(sub) = self.subagent_by_session.get(session_id) {
            if let Some(path) = self.workspace_by_session.get(&sub.parent_session_id) {
                return Some(path.clone());
            }
        }
        None
    }

    fn workspace_name_for(&self, session_id: &str) -> String {
        self.effective_workspace_path(session_id)
            .and_then(|path| self.workspaces.iter().find(|w| w.folder_path() == path).map(|w| w.name.clone()))
            .unwrap_or_default()
    }

    fn title_for(&self, session_id: &str) -> String {
        self.title_by_session
            .get(session_id)
            .cloned()
            .unwrap_or_else(|| {
                let mut cut = session_id.len().min(8);
                while cut > 0 && !session_id.is_char_boundary(cut) {
                    cut -= 1;
                }
                format!("agent {}", &session_id[..cut])
            })
    }

    fn is_app_session(&self, session_id: &str) -> bool {
        self.workspace_by_session.contains_key(session_id)
    }

    /// The chat a session's asks belong to: the session itself, or the root chat
    /// that spawned it (a nested subagent climbs to its top-level parent).
    fn owner_session_id(&self, session_id: &str) -> String {
        let mut current = session_id.to_string();
        while let Some(sub) = self.subagent_by_session.get(&current) {
            current = sub.parent_session_id.clone();
        }
        current
    }

    // ---- workspaces ----

    pub fn add_workspace(&mut self, path: &str, cx: &mut Context<Self>) {
        if path.trim().is_empty() {
            return;
        }
        let normalized = normalize_dir(path);
        if !std::path::Path::new(&normalized).exists() {
            self.announce(&format!("Folder does not exist: {normalized}"), true, cx);
            return;
        }
        if self.workspaces.iter().any(|w| w.folder_path() == normalized) {
            self.announce("That folder is already a workspace.", false, cx);
            return;
        }
        let state = WorkspaceState { path: normalized.clone(), ..Default::default() };
        self.ui.workspaces.push(state.clone());
        let item = WorkspaceItem::new(state);
        let path_key = item.folder_path().to_string();
        self.workspaces.push(item);
        self.save_state();
        self.select_workspace(&path_key, cx);
    }

    pub fn remove_workspace(&mut self, path: &str, cx: &mut Context<Self>) {
        let Some(index) = self.workspaces.iter().position(|w| w.folder_path() == path) else {
            return;
        };
        {
            let ws = &self.workspaces[index];
            if ws.is_general() {
                return; // built in, always there
            }
        }
        let was_selected = self.selected_workspace.as_deref() == Some(path);
        if let Some(runtime) = self.runtimes.remove(path) {
            let _ = runtime;
            self.net.send(NetCommand::StopServer { ws_key: path.into() });
        }
        self.drop_subagent_rows(path);
        let session_ids: Vec<String> = self.workspaces[index].state.sessions.clone();
        for sid in &session_ids {
            self.set_running(Some(sid), false);
        }
        self.drop_queue(&session_ids);
        self.drop_pending(&session_ids);
        for sid in &session_ids {
            self.workspace_by_session.remove(sid);
            self.title_by_session.remove(sid);
            self.turn_start_by_session.remove(sid);
            self.user_stopped_by_session.remove(sid);
        }
        // only this workspace's tiles leave the grid; other workspaces' stay
        let tiles_to_remove: Vec<String> = self
            .open_tiles
            .iter()
            .filter(|t| {
                t.session_id
                    .as_ref()
                    .map(|sid| session_ids.contains(sid))
                    .unwrap_or(false)
            })
            .map(|t| t.key.clone())
            .collect();
        for key in tiles_to_remove {
            self.detach_tile(&key, cx);
        }
        self.ui.workspaces.retain(|w| w.path != path);
        self.workspaces.remove(index);
        if was_selected {
            let next = self.workspaces.first().map(|w| w.folder_path().to_string());
            self.selected_workspace = next.clone();
            if next.is_none() {
                self.is_server_running = false;
            }
            if let Some(next) = next {
                self.select_workspace(&next, cx);
            }
        }
        self.save_state();
    }

    pub fn start_workspace(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.runtimes.contains_key(path) {
            return;
        }
        let Some(ws) = self.workspaces.iter().find(|w| w.folder_path() == path) else {
            return;
        };
        let folder = ws.folder_path().to_string();
        let executable = self.settings.open_code_executable.clone();
        tracing::info!("starting server for {}", ws.name);
        self.net.send(NetCommand::StartServer {
            ws_key: folder,
            executable: if executable.is_empty() { DEFAULT_EXECUTABLE.into() } else { executable },
            folder: path.into(),
        });
        let _ = cx;
    }

    pub fn stop_workspace(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.runtimes.contains_key(path) {
            return;
        }
        self.net.send(NetCommand::StopServer { ws_key: path.into() });
        self.runtimes.remove(path);
        self.drop_subagent_rows(path);
        if let Some(ws) = self.workspace_by_path_mut(path) {
            ws.is_running = false;
            ws.chats.clear();
        }
        let session_ids: Vec<String> = self
            .workspaces
            .iter()
            .find(|w| w.folder_path() == path)
            .map(|w| w.state.sessions.clone())
            .unwrap_or_default();
        for sid in &session_ids {
            self.set_running(Some(sid), false);
        }
        self.drop_queue(&session_ids);
        self.drop_pending(&session_ids);
        if self.selected_workspace.as_deref() == Some(path) {
            self.is_server_running = false;
            self.sessions.clear();
        }
        self.save_state();
        let _ = cx;
    }

    pub fn shutdown(&mut self) {
        for path in self.runtimes.keys().cloned().collect::<Vec<_>>() {
            self.net.send(NetCommand::StopServer { ws_key: path });
        }
        self.runtimes.clear();
        // full save_state (not ui.save) so open tiles mirror into ui.workspaces
        self.save_state();
        self.settings.save();
    }

    pub fn select_workspace(&mut self, path: &str, cx: &mut Context<Self>) {
        if self.selected_workspace.as_deref() == Some(path) {
            return;
        }
        self.selected_workspace = Some(path.to_string());
        self.sessions.clear();
        self.update_sidebar_selection();
        let running = self
            .selected_workspace()
            .map(|w| self.runtimes.contains_key(w.folder_path()))
            .unwrap_or(false);
        self.is_server_running = running;
        if !running {
            return;
        }
        self.workspace_generation += 1;
        if let Some(ws) = self.selected_workspace() {
            let path = ws.folder_path().to_string();
            self.net.send(NetCommand::Request { ws_key: path.clone(), request: ApiRequest::ListSessions });
            self.net.send(NetCommand::Request { ws_key: path.clone(), request: ApiRequest::ListModels });
            self.net.send(NetCommand::Request { ws_key: path, request: ApiRequest::ListAgents });
        }
        let _ = cx;
    }

    /// Expanding a workspace's chat list shows intent to work in it: connect its
    /// server so the chat list fills; an already-running workspace is left alone.
    pub fn connect_expanded_workspace(&mut self, path: &str, cx: &mut Context<Self>) {
        let running = self
            .workspaces
            .iter()
            .find(|w| w.folder_path() == path)
            .map(|w| self.runtimes.contains_key(w.folder_path()))
            .unwrap_or(false);
        if !running {
            self.start_workspace(path, cx);
        }
    }

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
                    // status starts Connected to match the pump's first attach
                    self.runtimes.insert(
                        ws_key.clone(),
                        WorkspaceRuntime { events_running: true, stream_status: oc::StreamStatus::Connected, healthy: None },
                    );
                    if let Some(ws) = self.workspace_by_path_mut(&ws_key) {
                        ws.is_running = true;
                    }
                    let is_selected = self.selected_workspace.as_deref() == Some(ws_key.as_str());
                    if is_selected {
                        self.is_server_running = true;
                    }
                    self.net.send(NetCommand::Request { ws_key: ws_key.clone(), request: ApiRequest::ListSessions });
                    // model/agent pickers feed off every running server
                    self.net.send(NetCommand::Request { ws_key: ws_key.clone(), request: ApiRequest::ListModels });
                    self.net.send(NetCommand::Request { ws_key: ws_key.clone(), request: ApiRequest::ListAgents });
                    if is_selected {
                        let wanted = self
                            .selected_workspace()
                            .map(|w| w.state.open_sessions.clone())
                            .unwrap_or_default();
                        self.restore_open_tiles(&ws_key, wanted, cx);
                    }
                    // restores and opens that were waiting on this server start
                    let pending_restores: Vec<(String, Vec<String>)> =
                        self.pending_restores.extract_if(.., |(path, _)| path == &ws_key).collect();
                    for (_, wanted) in pending_restores {
                        self.restore_open_tiles(&ws_key, wanted, cx);
                    }
                    let pending_opens: Vec<(String, String)> =
                        self.pending_opens.extract_if(.., |(path, _)| path == &ws_key).collect();
                    for (_, session_id) in pending_opens {
                        self.net.send(NetCommand::Request {
                            ws_key: ws_key.clone(),
                            request: ApiRequest::GetMessages { session_id },
                        });
                    }
                    cx.notify();
                }
                Err(err) => {
                    let name = self
                        .workspaces
                        .iter()
                        .find(|w| w.folder_path() == ws_key)
                        .map(|w| w.name.clone())
                        .unwrap_or_else(|| ws_key.clone());
                    tracing::error!("server start failed for {name}: {err:#}");
                    self.announce(&format!("Failed to start: {err:#}"), true, cx);
                }
            },
            UiEvent::ServerStopped { .. } => {}
            UiEvent::Event { ws_key, event } => {
                self.handle_stream_event(&ws_key, &event, cx);
            }
            UiEvent::StreamStatus { ws_key, status } => {
                // transitions only — the backoff ladder waits silently
                if let Some(runtime) = self.runtimes.get_mut(&ws_key) {
                    if runtime.stream_status != status {
                        runtime.stream_status = status;
                        match status {
                            oc::StreamStatus::Connected => {
                                // the server is back: its sessions may have
                                // changed (restart) while the stream was down
                                self.net.send(NetCommand::Request { ws_key: ws_key.clone(), request: ApiRequest::ListSessions });
                                self.announce("Event stream reconnected.", false, cx);
                            }
                            oc::StreamStatus::Reconnecting => {
                                self.announce("Connection lost — reconnecting…", true, cx);
                            }
                        }
                        cx.notify();
                    }
                }
            }
            UiEvent::HealthChanged { ws_key, healthy } => {
                if let Some(runtime) = self.runtimes.get_mut(&ws_key) {
                    if runtime.healthy != healthy {
                        runtime.healthy = healthy;
                        cx.notify();
                    }
                }
            }
            UiEvent::SessionsLoaded { ws_key, result } => match result {
                Ok(sessions) => self.apply_session_list(&ws_key, sessions, cx),
                Err(err) => tracing::debug!("session list refresh failed: {err:#}"),
            },
            UiEvent::MessagesLoaded { ws_key, session_id, result } => match result {
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
                    let mut tile = ChatTile::new(key.clone(), self.seed_model(), self.seed_agent(), self.seed_variant());
                    tile.apply_variant_list(&self.variants_by_model);
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
                    self.open_tiles.push(tile);
                    // a tile just opened: the restore banner has served its purpose
                    self.show_restore_prompt = false;
                    self.flush_pending_permissions(&session_id, cx);
                    self.flush_pending_questions(&session_id, cx);
                    self.pull_pending_questions(&ws_key, &session_id);
                    self.show_queued_for(&key);
                    // opening a chat sets it active on its workspace and focuses it
                    if let Some(path) = self.workspace_by_session.get(&session_id).cloned() {
                        if let Some(ws) = self.workspace_by_path_mut(&path) {
                            ws.state.active_session = Some(session_id.clone());
                        }
                    }
                    self.focus_tile(&key, cx);
                    cx.notify();
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
                    let Some((draft_key, text, attachments)) = self.pending_sends.first().cloned() else {
                        return;
                    };
                    self.pending_sends.remove(0);
                    self.workspace_by_session.insert(info.id.clone(), ws_key.clone());
                    self.title_by_session.insert(info.id.clone(), info.title.clone());
                    self.tile_by_session.insert(info.id.clone(), draft_key.clone());
                    let title = info.title.clone();
                    if let Some(tile) = self.tile_by_key_mut(&draft_key) {
                        tile.session_id = Some(info.id.clone());
                        tile.title = title;
                        tile.auto_title_pending = true;
                    }
                    if let Some(ws) = self.workspace_by_path_mut(&ws_key) {
                        ws.state.sessions.insert(0, info.id.clone());
                    }
                    self.sessions.insert(
                        0,
                        SessionItem {
                            id: info.id.clone(),
                            title: info.title.clone(),
                            updated_ms: info.updated_ms,
                            is_pinned: false,
                            is_selected: false,
                            is_running: false,
                            is_editing: false,
                            draft_title: String::new(),
                        },
                    );
                    self.sync_workspace_chats(cx);
                    self.persist_tiles();
                    self.save_state();
                    // continue the parked send now that the session exists
                    self.send_core(&info.id, text, attachments, cx);
                }
                Err(err) => {
                    // drop the matching queued send or a later create would
                    // adopt it and fire the stale text into the new session
                    if !self.pending_sends.is_empty() {
                        self.pending_sends.remove(0);
                    }
                    self.announce(&format!("Could not create a session: {err:#}"), true, cx);
                    cx.notify();
                }
            },
            UiEvent::ForkSessionCreated { ws_key, context, result } => match result {
                Ok(info) => {
                    // wire the new session in like SessionCreated does, minus the
                    // pending_send continuation — a fork never auto-sends, and the
                    // correlated event means no FIFO adoption of draft sends
                    self.workspace_by_session.insert(info.id.clone(), ws_key.clone());
                    self.title_by_session.insert(info.id.clone(), info.title.clone());
                    // mint a key directly — create_tile() would push a phantom
                    // draft tile into open_tiles (same as MessagesLoaded)
                    let key = format!("t{}", self.next_tile_key);
                    self.next_tile_key += 1;
                    let mut tile = ChatTile::new(key.clone(), self.seed_model(), self.seed_agent(), self.seed_variant());
                    tile.apply_variant_list(&self.variants_by_model);
                    tile.session_id = Some(info.id.clone());
                    tile.title = info.title.clone();
                    tile.auto_title_pending = false; // the fork-point title stands
                    tile.input = context;
                    if let Some(ws) = self.workspace_by_path_mut(&ws_key) {
                        ws.set_expanded(true); // the forked chat belongs in a visible sidebar section
                        ws.state.sessions.insert(0, info.id.clone());
                    }
                    self.tile_by_session.insert(info.id.clone(), key.clone());
                    self.open_tiles.push(tile);
                    self.sessions.insert(
                        0,
                        SessionItem {
                            id: info.id.clone(),
                            title: info.title.clone(),
                            updated_ms: info.updated_ms,
                            is_pinned: false,
                            is_selected: false,
                            is_running: false,
                            is_editing: false,
                            draft_title: String::new(),
                        },
                    );
                    self.show_restore_prompt = false; // a tile is about to be on screen
                    self.sync_workspace_chats(cx);
                    self.persist_tiles();
                    self.save_state();
                    self.focus_tile(&key, cx);
                }
                Err(err) => {
                    self.announce(&format!("Could not fork: {err:#}"), true, cx);
                    cx.notify();
                }
            },
            UiEvent::SessionRenamed { session_id, result, .. } => match result {
                Ok(info) => {
                    let final_title = if info.title.is_empty() { String::new() } else { info.title.clone() };
                    self.title_by_session.insert(session_id.clone(), final_title.clone());
                    if let Some(tile) = self.tile_for_session_mut(&session_id) {
                        tile.title = final_title.clone();
                    }
                    if let Some(item) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                        item.title = final_title;
                        item.updated_ms = info.updated_ms;
                    }
                    self.sort_sessions();
                    self.sync_workspace_chats(cx);
                    cx.notify();
                }
                Err(err) => {
                    tracing::warn!("rename failed: {err:#}");
                    self.announce(&format!("Rename failed: {err:#}"), true, cx);
                }
            },
            UiEvent::SessionDeleted { session_id, result, .. } => {
                if let Err(err) = result {
                    tracing::warn!("delete failed: {err:#}");
                    self.announce(&format!("Delete failed: {err:#}"), true, cx);
                } else {
                    self.forget_session(&session_id, cx);
                    self.save_state();
                }
                cx.notify();
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
                    self.set_running(Some(&session_id), false);
                    self.set_subagent_running(&session_id, false);
                }
                cx.notify();
            }
            UiEvent::PermissionReplied { permission_id, response, is_auto, result } => {
                let label = match result {
                    Err(err) => Some(format!("reply failed: {err:#}")),
                    Ok(()) => match response.as_str() {
                        "once" => Some("allowed (once)".into()),
                        "always" => Some(if is_auto { "auto-allowed (always)".into() } else { "always allowed".into() }),
                        _ => Some("denied".into()),
                    },
                };
                if let Some(label) = label {
                    self.resolve_permission(&permission_id, label, is_auto);
                }
                cx.notify();
            }
            UiEvent::QuestionsPulled { for_session, result, .. } => {
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
                                tile.items.push(Arc::new(ChatItem::Question(build_question_item(&request, &mut next_key))));
                            }
                        }
                        self.next_key = next_key;
                    }
                }
                cx.notify();
            }
            UiEvent::QuestionReplied { request_id, summary, result } => {
                let resolution = match result {
                    Err(err) => format!("reply failed: {err:#}"),
                    Ok(()) => summary,
                };
                self.resolve_question(&request_id, &resolution);
                cx.notify();
            }
            UiEvent::ModelsLoaded { result, .. } => {
                if let Ok(models) = result {
                    // merge: every running server contributes; existing entries stay for its own tiles
                    let restore: Vec<(String, Option<String>, Option<String>)> = self
                        .open_tiles
                        .iter()
                        .map(|t| (t.key.clone(), t.selected_model.clone(), t.selected_variant.clone()))
                        .collect();
                    for m in models {
                        self.variants_by_model.insert(m.key.clone(), m.variants);
                        self.context_limit_by_model.insert(m.key.clone(), m.context_limit);
                        if !self.models.contains(&m.key) {
                            self.models.push(m.key);
                        }
                    }
                    let known_models = self.models.clone();
                    let variants_by_model = self.variants_by_model.clone();
                    for (key, model, variant) in restore {
                        if let Some(tile) = self.tile_by_key_mut(&key) {
                            tile.selected_model = match model {
                                Some(saved) if known_models.contains(&saved) => Some(saved),
                                _ => Some(DEFAULT_MODEL_ENTRY.into()),
                            };
                            // the model list just landed: rebuild this tile's
                            // variant options around its model, keep a
                            // still-valid pick, else fall back to "(default)"
                            tile.apply_variant_list(&variants_by_model);
                            if let Some(saved_variant) = variant {
                                if tile.variants.contains(&saved_variant) {
                                    tile.selected_variant = Some(saved_variant);
                                }
                            }
                        }
                    }
                }
                cx.notify();
            }
            UiEvent::AgentsLoaded { result, .. } => {
                if let Ok(agents) = result {
                    let restore: Vec<(String, Option<String>)> =
                        self.open_tiles.iter().map(|t| (t.key.clone(), t.selected_agent.clone())).collect();
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
                cx.notify();
            }
            UiEvent::MessageSendFailed { session_id, error, .. } => {
                self.post_note_for_session(&session_id, &format!("Message failed: {error}"), true);
                cx.notify();
            }
        }
    }

    pub fn tile_for_session_mut(&mut self, session_id: &str) -> Option<&mut ChatTile> {
        let key = self.tile_by_session.get(session_id)?.clone();
        self.tile_by_key_mut(&key)
    }

    fn create_tile(&mut self) -> &mut ChatTile {
        let key = format!("t{}", self.next_tile_key);
        self.next_tile_key += 1;
        let mut tile = ChatTile::new(key.clone(), self.seed_model(), self.seed_agent(), self.seed_variant());
        tile.apply_variant_list(&self.variants_by_model);
        self.open_tiles.push(tile);
        self.tile_by_key_mut(&key).unwrap()
    }

    /// New chat: focus the trailing untouched draft or open a new tile, then connect.
    pub fn new_chat(&mut self, cx: &mut Context<Self>) {
        let Some(ws_path) = self.selected_workspace.clone() else { return };
        if let Some(ws) = self.workspace_by_path_mut(&ws_path) {
            ws.set_expanded(true); // the new chat belongs in a visible sidebar section
        }
        let untouched_last = self.open_tiles.last().map(|t| t.is_untouched()).unwrap_or(false);
        self.show_restore_prompt = false; // a tile is about to be on screen
        if untouched_last {
            let key = self.open_tiles.last().unwrap().key.clone();
            self.focus_tile(&key, cx);
        } else {
            let key = {
                let tile = self.create_tile();
                tile.key.clone()
            };
            self.focus_tile(&key, cx);
        }
        // a new chat is an explicit intent to talk: connect the server if it isn't up
        if !self.runtimes.contains_key(&ws_path) {
            self.start_workspace(&ws_path, cx);
        }
        cx.notify();
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
        cx.notify();
    }

    /// Closing a tile only un-tiles it; the chat stays in the sidebar and keeps running in the background.
    pub fn close_tile(&mut self, key: &str, cx: &mut Context<Self>) {
        let was_focused = self.focused_tile.as_deref() == Some(key);
        self.detach_tile(key, cx);
        if !was_focused {
            self.persist_tiles();
            self.save_state();
        }
        cx.notify();
    }

    fn detach_tile(&mut self, key: &str, cx: &mut Context<Self>) {
        let session = self.open_tiles.iter().find(|t| t.key == key).and_then(|t| t.session_id.clone());
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
        let Some(path) = self.workspace_by_session.get(session_id).cloned() else { return };
        if let Some(ws) = self.workspace_by_path_mut(&path) {
            ws.state.open_sessions.retain(|id| id != session_id);
            if ws.state.active_session.as_deref() == Some(session_id) {
                ws.state.active_session = None;
            }
        }
    }

    /// Drafts without content are dropped once real chats are restored.
    fn prune_untouched_drafts(&mut self, cx: &mut Context<Self>) {
        if self.tile_by_session.is_empty() {
            return;
        }
        let untouched: Vec<String> = self.open_tiles.iter().filter(|t| t.is_untouched()).map(|t| t.key.clone()).collect();
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

    fn show_queued_for(&mut self, tile_key: &str) {
        let Some(session_id) = self.tile_by_key(tile_key).and_then(|t| t.session_id.clone()) else { return };
        let keys: Vec<usize> = self.queue.iter().filter(|e| e.session_id == session_id).map(|e| e.key).collect();
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
            let Some(tile) = self.tile_by_key(tile_key) else { return };
            (tile.input.trim().to_string(), tile.attachments.len())
        };
        if text.is_empty() && attachment_count == 0 {
            return;
        }
        // an existing chat talks to its own workspace's server; a draft to the selected one
        let Some(tile) = self.tile_by_key(tile_key) else { return };
        if let Some(session_id) = &tile.session_id {
            if !self.has_runtime(session_id) {
                self.tile_note(tile_key, "This chat's workspace server is stopped — start it to continue.", true, cx);
                return;
            }
        } else if !self.is_server_running || self.selected_workspace.is_none() {
            return;
        }

        let attachments: Vec<AttachmentItem> = std::mem::take(&mut self.tile_by_key_mut(tile_key).unwrap().attachments);
        let tile = self.tile_by_key_mut(tile_key).unwrap();
        let text_to_record = text.clone();
        tile.input.clear();
        tile.history_index = 0;
        self.record_prompt(&text_to_record);

        let Some(tile) = self.tile_by_key(tile_key) else { return };
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
            cx.notify();
            return;
        }

        if session_id.is_none() {
            // create the session first; the send continues when the reply lands
            let ws_key = self.selected_workspace.clone().unwrap();
            self.pending_sends.push((tile_key.to_string(), text, attachments));
            self.net.send(NetCommand::Request { ws_key, request: ApiRequest::CreateSession { title: DRAFT_TITLE.into() } });
            return;
        }
        self.send_core(&session_id.unwrap(), text, attachments, cx);
    }

    fn send_core(&mut self, session_id: &str, text: String, attachments: Vec<AttachmentItem>, cx: &mut Context<Self>) {
        let Some(ws_key) = self.effective_workspace_path(session_id) else { return };
        let tile_key = self.tile_by_session.get(session_id).cloned();
        let sent_at = Instant::now();
        if let Some(tile_key) = &tile_key {
            let mut items: Vec<Arc<ChatItem>> = Vec::new();
            if !text.is_empty() {
                items.push(Arc::new(ChatItem::UserMessage {
                    key: self.next_key,
                    text: text.clone(),
                    created_ms: 0, // rendered with no time label, like "now"
                }));
                self.next_key += 1;
            }
            for attachment in &attachments {
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
        self.turn_start_by_session.insert(session_id.to_string(), sent_at);
        let title_seed = if !text.is_empty() {
            text.clone()
        } else {
            attachments.first().map(|a| a.file_name.clone()).unwrap_or_else(|| DRAFT_TITLE.into())
        };
        self.auto_title(session_id, &title_seed);
        let tile = tile_key.as_deref().and_then(|key| self.tile_by_key(key));
        let model = tile
            .and_then(|t| t.selected_model.clone())
            .filter(|m| m != DEFAULT_MODEL_ENTRY);
        let variant = tile
            .and_then(|t| t.selected_variant.clone())
            .filter(|v| v != DEFAULT_VARIANT_ENTRY);
        let agent = tile.and_then(|t| t.selected_agent.clone()).filter(|a| a != DEFAULT_AGENT_ENTRY);
        self.net.send(NetCommand::SendMessage {
            ws_key,
            session_id: session_id.to_string(),
            text,
            model,
            variant,
            agent,
            attachments: attachments
                .into_iter()
                .map(|a| (a.file_name, a.mime, a.data))
                .collect(),
        });
        cx.notify();
    }

    /// Steer: jump the queue and interrupt the running turn so this prompt runs next.
    pub fn steer_queued(&mut self, queue_key: usize, cx: &mut Context<Self>) {
        let Some(index) = self.queue.iter().position(|e| e.key == queue_key) else { return };
        let entry = self.queue[index].clone();
        let Some(tile_key) = self.tile_by_session.get(&entry.session_id).cloned() else { return };
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
        let Some(index) = self.queue.iter().position(|e| e.key == queue_key) else { return };
        let entry = self.queue.remove(index);
        if let Some(tile_key) = self.tile_by_session.get(&entry.session_id).cloned() {
            if let Some(tile) = self.tile_by_key_mut(&tile_key) {
                tile.queue_rows.retain(|k| *k != queue_key);
                tile.set_input_direct(entry.text.clone());
                for attachment in entry.attachments {
                    tile.attachments.push(attachment);
                }
            }
        }
        cx.notify();
    }

    pub fn remove_queued(&mut self, queue_key: usize, cx: &mut Context<Self>) {
        self.queue.retain(|e| e.key != queue_key);
        for tile in &mut self.open_tiles {
            tile.queue_rows.retain(|k| *k != queue_key);
        }
        cx.notify();
    }

    fn drop_queue(&mut self, session_ids: &[String]) {
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
    fn try_drain_queue(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if !self.has_runtime(session_id) || self.is_busy(session_id) {
            return; // busy: a steered prompt already took over; the rest stay queued
        }
        let Some(index) = self.queue.iter().position(|e| e.session_id == session_id) else { return };
        let entry = self.queue.remove(index);
        if let Some(tile_key) = self.tile_by_session.get(session_id).cloned() {
            if let Some(tile) = self.tile_by_key_mut(&tile_key) {
                tile.queue_rows.retain(|k| *k != entry.key);
            }
        }
        self.send_core(session_id, entry.text, entry.attachments, cx);
    }

    pub fn abort_tile(&mut self, tile_key: &str, cx: &mut Context<Self>) {
        let Some(tile) = self.tile_by_key(tile_key) else { return };
        let Some(session_id) = tile.session_id.clone() else { return };
        if !self.is_busy(&session_id) || !self.has_runtime(&session_id) {
            return;
        }
        let ws_key = self.effective_workspace_path(&session_id).unwrap();
        self.net.send(NetCommand::Request { ws_key, request: ApiRequest::Abort { session_id: session_id.clone() } });
        let _ = cx;
    }

    pub fn stop_running(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(ws_key) = self.effective_workspace_path(session_id) else { return };
        self.net.send(NetCommand::Request { ws_key, request: ApiRequest::Abort { session_id: session_id.to_string() } });
        cx.notify();
    }

    fn auto_title(&mut self, session_id: &str, text: &str) {
        let Some(tile) = self.tile_for_session(session_id) else { return };
        if !tile.auto_title_pending {
            return;
        }
        if let Some(tile) = self.tile_for_session_mut(session_id) {
            tile.auto_title_pending = false;
        }
        let title = derive_title(text);
        let Some(ws_key) = self.effective_workspace_path(session_id) else { return };
        self.net.send(NetCommand::Request {
            ws_key,
            request: ApiRequest::RenameSession { session_id: session_id.to_string(), title },
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
        let own = self.tile_by_key(tile_key).map(|t| t.sent_prompt_count()).unwrap_or(0);
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
            let Some(tile) = self.tile_by_key(tile_key) else { return false };
            (tile.history_index, tile.input.clone(), tile.history_draft.clone())
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

    /// Loads the tile's most recent sent user message into the composer for
    /// review (the "↻ last prompt" pill) — no auto-send.
    pub fn recall_last_prompt(&mut self, tile_key: &str, cx: &mut Context<Self>) {
        let text = self.tile_by_key(tile_key).map(|t| t.peek_sent_prompt(0)).unwrap_or_default();
        if text.trim().is_empty() {
            return;
        }
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            tile.set_input_direct(text);
        }
        cx.notify();
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
        let Some(tile) = self.tile_by_key(tile_key) else { return };
        let Some(session_id) = tile.session_id.clone() else { return };
        let Some(ws_key) = self.effective_workspace_path(&session_id) else { return };
        if !self.runtimes.contains_key(&ws_key) {
            self.tile_note(tile_key, "This chat's workspace server is stopped — start it to fork.", true, cx);
            return;
        }
        let stop_text = tile.peek_sent_prompt_key(stop_key);
        if stop_text.trim().is_empty() {
            return;
        }
        let title = derive_title(&stop_text);
        let context = fork_transcript(tile, stop_key, &self.title_for(&session_id));
        self.net.send(NetCommand::Request {
            ws_key,
            request: ApiRequest::CreateForkSession { title, context },
        });
        let _ = cx;
    }

    /// Shift+Tab in the composer: next entry in the agent list, wrapping;
    /// the "(default build)" sentinel participates in the cycle like any other entry.
    pub fn cycle_agent(&mut self, tile_key: &str) {
        if !self.has_agents() {
            return;
        }
        let current = self.tile_by_key(tile_key).and_then(|t| t.selected_agent.clone());
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

    // ---- session list maintenance ----

    fn apply_session_list(&mut self, ws_key: &str, sessions: Vec<SessionInfo>, cx: &mut Context<Self>) {
        // keep the subagent→parent map warm from the listing too
        for s in &sessions {
            if let Some(parent_id) = &s.parent_id {
                self.track_session_info(Some(&s.id), Some(parent_id), None);
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
            let pins = &self.ui.pins;
            let mut chats: Vec<SessionItem> = sessions
                .iter()
                .filter(|s| known.contains(&s.id))
                .map(|s| SessionItem {
                    id: s.id.clone(),
                    title: s.title.clone(),
                    updated_ms: s.updated_ms,
                    is_pinned: pins.contains_key(&s.id),
                    is_selected: false,
                    is_running: self.running_by_session.contains(&s.id),
                    is_editing: false,
                    draft_title: String::new(),
                })
                .collect();
            chats.sort_by_key(|s| std::cmp::Reverse((s.is_pinned, s.updated_ms)));
            if let Some(ws) = self.workspace_by_path_mut(ws_key) {
                ws.chats = chats;
            }
            cx.notify();
            return;
        }
        let Some(state) = self.workspaces.iter().find(|w| w.folder_path() == ws_key).map(|w| w.state.clone()) else { return };
        let pins = self.ui.pins.clone();
        let mut mine: Vec<SessionInfo> = sessions
            .into_iter()
            .filter(|s| state.sessions.contains(&s.id))
            .collect();
        mine.sort_by_key(|s| {
            std::cmp::Reverse((pins.contains_key(&s.id), s.updated_ms))
        });
        let alive: HashSet<String> = mine.iter().map(|s| s.id.clone()).collect();
        // drop chats that no longer exist on the server
        let gone: Vec<String> = state.sessions.iter().filter(|id| !alive.contains(*id)).cloned().collect();
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
        // an idle fires at the end of every turn; when nothing visible changed,
        // skip the full rebuild + disk save (the common case while chatting)
        let unchanged = self.sessions.len() == mine.len()
            && self.sessions.iter().zip(mine.iter()).all(|(cur, s)| {
                cur.id == s.id
                    && cur.title == s.title
                    && cur.updated_ms == s.updated_ms
                    && cur.is_pinned == pins.contains_key(&s.id)
                    && cur.is_running == self.running_by_session.contains(&s.id)
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
        self.sessions = mine
            .iter()
            .map(|s| SessionItem {
                id: s.id.clone(),
                title: s.title.clone(),
                updated_ms: s.updated_ms,
                is_pinned: self.ui.pins.contains_key(&s.id),
                is_selected: false,
                is_running: self.running_by_session.contains(&s.id),
                is_editing: false,
                draft_title: String::new(),
            })
            .collect();
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
        cx.notify();
    }

    pub fn sort_sessions(&mut self) {
        self.sessions.sort_by_key(|s| {
            std::cmp::Reverse((s.is_pinned, s.updated_ms))
        });
    }

    fn sync_workspace_chats(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.selected_workspace.clone() else { return };
        let chats = self.sessions.clone();
        if let Some(ws) = self.workspace_by_path_mut(&path) {
            ws.chats = chats;
        }
        self.update_sidebar_selection();
        cx.notify();
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
        cx.notify();
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

    pub fn cancel_rename(&mut self, session_id: &str, cx: &mut Context<Self>) {
        for ws in &mut self.workspaces {
            if let Some(chat) = ws.chats.iter_mut().find(|c| c.id == session_id) {
                chat.is_editing = false;
            }
        }
        let _ = cx;
    }

    pub fn commit_rename(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let draft = self
            .workspaces
            .iter()
            .flat_map(|w| w.chats.iter())
            .find(|c| c.id == session_id)
            .map(|c| (c.draft_title.trim().to_string(), c.title.clone()));
        let Some((title, current)) = draft else { return };
        for ws in &mut self.workspaces {
            if let Some(chat) = ws.chats.iter_mut().find(|c| c.id == session_id) {
                chat.is_editing = false;
            }
        }
        if title.is_empty() || title == current {
            return;
        }
        let Some(ws_key) = self.effective_workspace_path(session_id) else { return };
        self.net.send(NetCommand::Request {
            ws_key,
            request: ApiRequest::RenameSession { session_id: session_id.to_string(), title },
        });
        cx.notify();
    }

    pub fn delete_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self.is_busy(session_id) {
            self.announce("Stop the running turn before deleting this chat.", true, cx);
            return;
        }
        let Some(ws_key) = self.effective_workspace_path(session_id) else { return };
        self.net.send(NetCommand::Request {
            ws_key,
            request: ApiRequest::DeleteSession { session_id: session_id.to_string() },
        });
    }

    /// Unlinks the chat from its workspace; the session itself survives on the server.
    pub fn remove_from_workspace(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self.is_busy(session_id) {
            self.announce("Stop the running turn before removing this chat.", true, cx);
            return;
        }
        self.forget_session(session_id, cx);
        self.save_state();
        cx.notify();
    }

    fn forget_session(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(tile_key) = self.tile_by_session.get(id).cloned() {
            self.detach_tile(&tile_key, cx);
        }
        self.turn_start_by_session.remove(id);
        self.user_stopped_by_session.remove(id);
        self.workspace_by_session.remove(id);
        for ws in &mut self.ui.workspaces {
            ws.sessions.retain(|s| s != id);
        }
        self.title_by_session.remove(id);
        self.sessions.retain(|s| s.id != id);
        for ws in &mut self.workspaces {
            ws.chats.retain(|c| c.id != id);
        }
        self.ui.pins.remove(id);
        self.drop_queue(&[id.to_string()]);
        self.drop_pending(&[id.to_string()]);
        self.sync_workspace_chats(cx);
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
        cx.notify();
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
    fn restore_open_tiles(&mut self, ws_key: &str, wanted: Vec<String>, cx: &mut Context<Self>) {
        // tiles are (re)opening: the restore banner has served its purpose
        self.show_restore_prompt = false;
        let mut added = 0;
        for id in wanted {
            let known = self.workspaces.iter().find(|w| w.folder_path() == ws_key).map(|w| w.state.sessions.contains(&id)).unwrap_or(false);
            if !known || self.tile_by_session.contains_key(&id) {
                continue;
            }
            self.net.send(NetCommand::Request {
                ws_key: ws_key.to_string(),
                request: ApiRequest::GetMessages { session_id: id.clone() },
            });
            added += 1;
        }
        if added == 0 {
            self.prune_untouched_drafts(cx);
        }
        cx.notify();
    }

    // ---- pending permission/question parking ----

    fn drop_pending(&mut self, session_ids: &[String]) {
        for sid in session_ids {
            if self.pending_by_session.remove(sid).is_some() {
                self.mark_attention(sid, false);
            }
            self.pending_questions_by_session.remove(sid);
        }
    }

    fn flush_pending_permissions(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending_by_session.remove(session_id) {
            // snapshot remaining countdowns first (items move under a &mut self borrow)
            let lefts: Vec<Option<i64>> = pending.iter().map(|i| self.countdowns.get(&i.id).copied()).collect();
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

    fn flush_pending_questions(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending_questions_by_session.remove(session_id) {
            if let Some(tile) = self.tile_for_session_mut(session_id) {
                for item in pending {
                    if item.is_pending()
                        && !tile.items.iter().any(|i| matches!(i.as_ref(), ChatItem::Question(q) if q.id == item.id))
                    {
                        tile.items.push(Arc::new(ChatItem::Question(item)));
                    }
                }
            }
            self.mark_attention(session_id, false);
        }
        let _ = cx;
    }

    fn pull_pending_questions(&mut self, ws_key: &str, session_id: &str) {
        self.net.send(NetCommand::Request {
            ws_key: ws_key.to_string(),
            request: ApiRequest::GetQuestions { for_session: session_id.to_string() },
        });
    }

    pub fn mark_attention(&mut self, session_id: &str, attention: bool) {
        // light the row itself, or the run that spawned it for subagent asks
        let index = self
            .running_agents
            .iter()
            .position(|r| r.session_id == session_id)
            .or_else(|| self.running_agents.iter().position(|r| r.parent_session_id.as_deref() == Some(session_id)));
        if let Some(index) = index {
            self.running_agents[index].needs_attention = attention;
        }
    }

    // ---- running/subagent tracking ----

    fn set_running(&mut self, session_id: Option<&str>, running: bool) {
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
        let existing_index = self.running_agents.iter().position(|r| r.session_id == session_id);
        if running {
            if existing_index.is_none() {
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
    }

    fn update_sidebar_running(&mut self) {
        let running: HashSet<String> = self.running_by_session.clone();
        for ws in &mut self.workspaces {
            for chat in &mut ws.chats {
                chat.is_running = running.contains(&chat.id);
            }
        }
    }

    fn track_session_info(&mut self, session_id: Option<&str>, parent_id: Option<&str>, agent: Option<&str>) {
        let Some(session_id) = session_id else { return };
        match parent_id {
            None if parent_id.is_none() || parent_id == Some(session_id) => {
                if self.subagent_by_session.remove(session_id).is_some() {
                    self.set_subagent_running(session_id, false);
                }
            }
            Some(parent_id) if parent_id == session_id => {
                if self.subagent_by_session.remove(session_id).is_some() {
                    self.set_subagent_running(session_id, false);
                }
            }
            Some(parent_id) => {
                let info = SubagentInfo {
                    parent_session_id: parent_id.to_string(),
                    agent: agent.filter(|a| !a.trim().is_empty()).map(String::from).unwrap_or_else(|| "subagent".into()),
                };
                let fresh_label = format!("{} → {}", info.agent, self.title_for(parent_id));
                self.subagent_by_session.insert(session_id.to_string(), info);
                if self.running_subagents.contains(session_id) {
                    if let Some(row) = self.running_agents.iter_mut().find(|r| r.session_id == session_id) {
                        row.title = fresh_label;
                    }
                }
            }
            None => {}
        }
    }

    fn set_subagent_running(&mut self, child_id: &str, running: bool) {
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
        let existing_index = self.running_agents.iter().position(|r| r.session_id == child_id);
        if running {
            if existing_index.is_none() {
                let info = self.subagent_by_session.get(child_id).unwrap();
                let title = format!("{} → {}", info.agent, self.title_for(&info.parent_session_id));
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
    fn refresh_tile_agents(&mut self) {
        for tile in &mut self.open_tiles {
            tile.tile_agents = self
                .running_agents
                .iter()
                .filter(|r| r.parent_session_id.as_deref() == tile.session_id.as_deref())
                .cloned()
                .collect();
        }
    }

    fn drop_subagent_rows(&mut self, folder_path: &str) {
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
        let Some(path) = self.workspace_by_session.get(&target).cloned() else { return };
        self.open_session_into_grid(&path, &target, cx);
    }

    /// Opens a subagent child chat into the grid: focuses its tile when one is
    /// already open, otherwise loads it through its parent's workspace server.
    pub fn open_subagent_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if let Some(tile_key) = self.tile_by_session.get(session_id).cloned() {
            self.focus_tile(&tile_key, cx);
            return;
        }
        let Some(path) = self.effective_workspace_path(session_id) else { return };
        self.open_session_into_grid(&path, session_id, cx);
    }

    /// Opens the clicked sidebar chat: focuses its tile when one exists, otherwise
    /// loads it from its own workspace's server — without switching the selected
    /// workspace, so chats of different workspaces share one grid.
    pub fn select_session(&mut self, session_id: &str, cx: &mut Context<Self>) {
        tracing::info!("select_session {}", session_id);
        if self.focused_tile.as_deref().and_then(|k| self.tile_by_key(k)).and_then(|t| t.session_id.clone()).as_deref() == Some(session_id) {
            return;
        }
        if let Some(tile_key) = self.tile_by_session.get(session_id).cloned() {
            self.focus_tile(&tile_key, cx);
            return;
        }
        let Some(path) = self.workspace_by_session.get(session_id).cloned() else { return };
        self.open_session_into_grid(&path, session_id, cx);
    }

    /// Loads a chat into a grid tile through its own workspace's server, starting
    /// that server if needed; the selected workspace is left alone.
    pub fn open_session_into_grid(&mut self, ws_path: &str, session_id: &str, cx: &mut Context<Self>) {
        if let Some(ws) = self.workspace_by_path_mut(ws_path) {
            ws.set_expanded(true); // the opened chat's row belongs in a visible sidebar section
        }
        if !self.runtimes.contains_key(ws_path) {
            self.start_workspace(ws_path, cx);
            self.pending_opens.push((ws_path.to_string(), session_id.to_string()));
            cx.notify();
            return;
        }
        self.net.send(NetCommand::Request {
            ws_key: ws_path.to_string(),
            request: ApiRequest::GetMessages { session_id: session_id.to_string() },
        });
        cx.notify();
    }

    // ---- stream events (HandleEvent port) ----

    fn handle_stream_event(&mut self, ws_key: &str, evt: &serde_json::Value, cx: &mut Context<Self>) {
        let type_ = evt.get("type").and_then(serde_json::Value::as_str).unwrap_or("");
        let data = evt.get("properties").unwrap_or(evt);
        match type_ {
            "server.connected" => {
                self.announce("Connected to the opencode event stream.", false, cx);
            }
            "session.created" | "session.updated" => {
                if let Some(info) = data.get("info") {
                    let tracked_id = info.get("id").and_then(serde_json::Value::as_str);
                    let parent_id = info.get("parentID").and_then(serde_json::Value::as_str);
                    let agent = info.get("agent").and_then(serde_json::Value::as_str);
                    self.track_session_info(tracked_id, parent_id, agent);
                    self.refresh_tile_agents();
                }
            }
            "message.updated" => {
                if let Some(tile_key) = self.tile_from_data(data, ws_key) {
                    self.with_tile(&tile_key, |tile, store, _| tile.on_message_updated(data, store));
                }
            }
            "message.part.updated" => {
                // the stream re-sends unchanged part snapshots constantly while a
                // turn runs; only an actual transcript change may repaint
                let changed = self
                    .tile_from_data(data, ws_key)
                    .and_then(|tile_key| {
                        self.with_tile(&tile_key, |tile, _store, next_key| tile.on_part_updated(data, next_key))
                    })
                    .unwrap_or(false);
                if changed {
                    cx.notify();
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
                    // refresh the chat list of the workspace the chat belongs to
                    if let Some(idle_path) = self.workspace_by_session.get(sid).cloned() {
                        if self.selected_workspace.as_deref() == Some(idle_path.as_str()) {
                            self.net.send(NetCommand::Request { ws_key: idle_path, request: ApiRequest::ListSessions });
                        } else if self.runtimes.contains_key(&idle_path) {
                            self.net.send(NetCommand::Request { ws_key: idle_path, request: ApiRequest::ListSessions });
                        }
                    }
                    let sid = sid.to_string();
                    self.try_drain_queue(&sid, cx);
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
                let message = read_error(data);
                let error_session = read_session_id(data);
                if let Some(sid) = error_session {
                    self.post_note_for_session(sid, &message, true);
                } else {
                    self.announce(&message, true, cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn tile_from_data(&self, data: &serde_json::Value, ws_key: &str) -> Option<String> {
        if let Some(id) = read_session_id(data) {
            return self.tile_by_session.get(id).cloned();
        }
        // events without a session id belong to a tile of the same workspace
        let focused = self.focused_tile.as_deref().and_then(|key| self.tile_by_key(key))?;
        let focused_session = focused.session_id.as_ref()?;
        if self.workspace_by_session.get(focused_session).map(|p| p == ws_key).unwrap_or(false) {
            Some(focused.key.clone())
        } else {
            None
        }
    }

    /// Turn finished: drop a muted note telling how long the prompt took, send to final answer.
    /// A user-aborted turn says "stopped in" instead — it didn't finish.
    fn note_turn_duration(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let stopped = self.user_stopped_by_session.remove(session_id);
        let Some(start) = self.turn_start_by_session.remove(session_id) else { return };
        let Some(tile_key) = self.tile_by_session.get(session_id).cloned() else { return };
        let duration = format_duration(start.elapsed());
        let text = format!("{} in {}", if stopped { "stopped" } else { "done" }, duration);
        let note_key = self.next_key;
        self.next_key += 1;
        if let Some(tile) = self.tile_by_key_mut(&tile_key) {
            tile.items.push(Arc::new(ChatItem::SystemNote { key: note_key, text, is_error: false }));
        }
        let _ = cx;
    }

    fn on_permission_asked(&mut self, data: &serde_json::Value, v2: bool, cx: &mut Context<Self>) {
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
            kind = data.get("action").and_then(serde_json::Value::as_str).unwrap_or("permission").to_string();
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
            kind = data.get("permission").and_then(serde_json::Value::as_str).unwrap_or("permission").to_string();
            detail = String::new();
            if let Some(md) = data.get("metadata").and_then(serde_json::Value::as_object) {
                if let Some(command) = md.get("command").and_then(serde_json::Value::as_str) {
                    if !command.is_empty() {
                        detail = command.to_string();
                    }
                }
                if let Some(d) = md.get("diff").and_then(serde_json::Value::as_str) {
                    if !d.is_empty() {
                        diff = Some(d.to_string());
                    }
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
            cx.notify();
            return;
        }
        self.pending_by_session.entry(owner).or_default().push(item);
        if !auto_allow {
            self.mark_attention(&session_id, true);
        }
        cx.notify();
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
            (s.auto_allow_default_min_seconds, s.auto_allow_default_max_seconds)
        };
        let min = min.max(0);
        (min, max.max(min))
    }

    /// Called once per second by the countdown tick; answers "always" for due asks.
    pub fn tick_countdowns(&mut self, cx: &mut Context<Self>) {
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
            if self.find_permission(&id).map(|p| p.is_pending()).unwrap_or(false) && self.auto_allow {
                self.respond_permission(&id, "always", true, cx);
            }
        }
        // refresh the visible countdown labels
        for tile in &mut self.open_tiles {
            for item in &mut tile.items {
                if let ChatItem::Permission(p) = Arc::make_mut(item) {
                    if let Some(left) = self.countdowns.get(&p.id) {
                        p.auto_countdown = Some(*left);
                    }
                }
            }
        }
        cx.notify();
    }

    fn find_permission(&self, permission_id: &str) -> Option<&PermissionItem> {
        for tile in &self.open_tiles {
            for item in &tile.items {
                if let ChatItem::Permission(p) = item.as_ref() {
                    if p.id == permission_id {
                        return Some(p);
                    }
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

    pub fn respond_permission(&mut self, permission_id: &str, response: &str, is_auto: bool, cx: &mut Context<Self>) {
        // close the buttons immediately so a slow reply cannot be clicked twice
        let mut session_id = None;
        let mut is_v2 = false;
        for tile in &mut self.open_tiles {
            for item in &mut tile.items {
                if let ChatItem::Permission(p) = Arc::make_mut(item) {
                    if p.id == permission_id && p.is_pending() {
                        p.answer = Some(if is_auto { "auto-allowing…".into() } else { "sending…".into() });
                        p.was_auto_answered = is_auto;
                        p.auto_countdown = None;
                        session_id = Some(p.session_id.clone());
                        is_v2 = p.is_v2;
                    }
                }
            }
        }
        for pending in self.pending_by_session.values_mut() {
            for item in pending {
                if item.id == permission_id && item.is_pending() {
                    item.answer = Some(if is_auto { "auto-allowing…".into() } else { "sending…".into() });
                    item.was_auto_answered = is_auto;
                    item.auto_countdown = None;
                    session_id = Some(item.session_id.clone());
                    is_v2 = item.is_v2;
                }
            }
        }
        let Some(session_id) = session_id else { return };
        let Some(ws_key) = self.effective_workspace_path(&session_id) else {
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
            ApiRequest::ReplyV2 { request_id: permission_id.to_string(), response }
        } else {
            ApiRequest::RespondPermission {
                session_id: session_id.clone(),
                permission_id: permission_id.to_string(),
                response,
            }
        };
        self.net.send(NetCommand::Request { ws_key, request });
        // the card flipped to "sending…" — repaint now, not on the next event
        cx.notify();
    }

    fn resolve_permission(&mut self, permission_id: &str, label: String, is_auto: bool) {
        for tile in &mut self.open_tiles {
            for item in &mut tile.items {
                if let ChatItem::Permission(p) = Arc::make_mut(item) {
                    if p.id == permission_id && p.is_pending() {
                        p.answer = Some(label.clone());
                        p.was_auto_answered = is_auto;
                        p.auto_countdown = None;
                    }
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

    fn on_permission_replied(&mut self, data: &serde_json::Value) {
        let Some(request_id) = data.get("requestID").and_then(serde_json::Value::as_str) else { return };
        self.resolve_permission(request_id, "answered elsewhere".into(), false);
    }

    // ---- question tool asks ----
    // The agent's `question` tool parks the turn until the user answers; never auto-answered.

    fn on_question_asked(&mut self, data: &serde_json::Value, cx: &mut Context<Self>) {
        let Some(request) = oc::read_question_request(data) else { return };
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
            if let Some(tile) = self.tile_by_key_mut(&tile_key) {
                if !tile.items.iter().any(|i| matches!(i.as_ref(), ChatItem::Question(q) if q.id == built.id)) {
                    tile.items.push(Arc::new(ChatItem::Question(built)));
                }
            }
            cx.notify();
            return;
        }
        let pending = self.pending_questions_by_session.entry(owner).or_default();
        if !pending.iter().any(|i| i.id == built.id) {
            pending.push(built);
        }
        self.mark_attention(&request.session_id, true);
        cx.notify();
    }

    fn on_question_resolved(&mut self, data: &serde_json::Value) {
        let Some(request_id) = data.get("requestID").and_then(serde_json::Value::as_str) else { return };
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

    fn resolve_question(&mut self, request_id: &str, summary: &str) {
        for tile in &mut self.open_tiles {
            for item in &mut tile.items {
                if let ChatItem::Question(q) = Arc::make_mut(item) {
                    if q.id == request_id && q.is_pending() {
                        q.resolution = Some(summary.to_string());
                        for question in &mut q.questions {
                            question.locked = true;
                        }
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
                if let ChatItem::Question(q) = Arc::make_mut(item) {
                    if q.id == question_id && q.is_pending() && q.can_submit() {
                        answers = Some(q.questions.iter().map(|e| e.answer()).collect());
                        q.resolution = Some(format!("sending {}…", answers.as_ref().unwrap().iter().map(|a| if a.is_empty() { "(empty)".to_string() } else { a.join(", ") }).collect::<Vec<_>>().join(" · ")));
                        for question in &mut q.questions {
                            question.locked = true;
                        }
                        session_id = Some(q.session_id.clone());
                    }
                }
            }
        }
        let (Some(answers), Some(session_id)) = (answers, session_id) else { return };
        let Some(ws_key) = self.effective_workspace_path(&session_id) else {
            self.resolve_question(question_id, "server unavailable");
            return;
        };
        self.net.send(NetCommand::Request {
            ws_key,
            request: ApiRequest::ReplyQuestion { request_id: question_id.to_string(), answers },
        });
        cx.notify();
    }

    pub fn dismiss_question(&mut self, question_id: &str, cx: &mut Context<Self>) {
        let mut session_id = None;
        for tile in &mut self.open_tiles {
            for item in &mut tile.items {
                if let ChatItem::Question(q) = Arc::make_mut(item) {
                    if q.id == question_id && q.is_pending() {
                        q.resolution = Some("dismissing…".into());
                        for question in &mut q.questions {
                            question.locked = true;
                        }
                        session_id = Some(q.session_id.clone());
                    }
                }
            }
        }
        let Some(session_id) = session_id else { return };
        let Some(ws_key) = self.effective_workspace_path(&session_id) else {
            self.resolve_question(question_id, "server unavailable");
            return;
        };
        self.net.send(NetCommand::Request {
            ws_key,
            request: ApiRequest::RejectQuestion { request_id: question_id.to_string() },
        });
        cx.notify();
    }

    // ---- notes ----

    pub fn announce(&mut self, text: &str, error: bool, cx: &mut Context<Self>) {
        // land in the focused transcript, else the first tile
        let target = self
            .focused_tile
            .clone()
            .or_else(|| self.open_tiles.first().map(|t| t.key.clone()));
        if let Some(target) = target {
            self.tile_note(&target, text, error, cx);
        }
    }

    pub fn tile_note(&mut self, tile_key: &str, text: &str, error: bool, cx: &mut Context<Self>) {
        let note_key = self.next_key;
        self.next_key += 1;
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            tile.items.push(Arc::new(ChatItem::SystemNote { key: note_key, text: text.into(), is_error: error }));
        }
        cx.notify();
    }

    fn post_note_for_session(&mut self, session_id: &str, text: &str, error: bool) {
        let note_key = self.next_key;
        self.next_key += 1;
        if let Some(tile_key) = self.tile_by_session.get(session_id).cloned() {
            if let Some(tile) = self.tile_by_key_mut(&tile_key) {
                tile.items.push(Arc::new(ChatItem::SystemNote { key: note_key, text: text.into(), is_error: error }));
                return;
            }
        }
        let title = self.title_for(session_id);
        let target = self
            .focused_tile
            .clone()
            .or_else(|| self.open_tiles.first().map(|t| t.key.clone()));
        if let Some(target) = target {
            if let Some(tile) = self.tile_by_key_mut(&target) {
                tile.items.push(Arc::new(ChatItem::SystemNote {
                    key: note_key,
                    text: format!("{title}: {text}"),
                    is_error: error,
                }));
            }
        }
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
        let Some(tile) = self.tile_by_key_mut(tile_key) else { return };
        for item in &mut tile.items {
            if let ChatItem::Question(q) = Arc::make_mut(item) {
                if q.id != question_id {
                    continue;
                }
                let Some(entry) = q.questions.get_mut(qi) else { return };
                for (i, option) in entry.options.iter_mut().enumerate() {
                    if multiple && i == oi {
                        option.is_selected = !option.is_selected;
                    } else if !multiple {
                        option.is_selected = i == oi;
                    }
                }
            }
        }
        cx.notify();
    }

    pub fn set_question_custom(&mut self, tile_key: &str, question_id: &str, qi: usize, text: String, cx: &mut Context<Self>) {
        let Some(tile) = self.tile_by_key_mut(tile_key) else { return };
        for item in &mut tile.items {
            if let ChatItem::Question(q) = Arc::make_mut(item) {
                if q.id == question_id {
                    if let Some(entry) = q.questions.get_mut(qi) {
                        entry.custom_text = text.clone();
                    }
                }
            }
        }
        cx.notify();
    }

    // ---- attachments ----

    pub fn attach_image(&mut self, tile_key: &str, png: Vec<u8>) {
        let akey = self.next_key;
        self.next_key += 1;
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            let count = tile.attachments.len() + 1;
            tile.attachments.push(AttachmentItem {
                file_name: format!("paste-{count}.png"),
                mime: "image/png".into(),
                data: Arc::new(png),
                key: akey,
            });
        }
    }

    pub fn attach_file(&mut self, tile_key: &str, file_name: String, mime: String, data: Vec<u8>) {
        let akey = self.next_key;
        self.next_key += 1;
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            tile.attachments.push(AttachmentItem { file_name, mime, data: Arc::new(data), key: akey });
        }
    }

    /// Files dragged onto the composer from the file manager; mirrors the
    /// picker path (directories skipped, over-512 KB announced and skipped).
    pub fn attach_paths(&mut self, tile_key: &str, paths: &[std::path::PathBuf], cx: &mut Context<Self>) {
        for path in paths {
            if path.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string());
            let Ok(data) = std::fs::read(path) else {
                self.tile_note(tile_key, &format!("{name} could not be read — skipped."), true, cx);
                continue;
            };
            if data.len() > 512 * 1024 {
                self.tile_note(tile_key, &format!("{name} is over 512 KB — skipped."), true, cx);
                continue;
            }
            let mime = crate::ui::mime_for_path(&name);
            self.attach_file(tile_key, name, mime.into(), data);
        }
        cx.notify();
    }

    pub fn remove_attachment(&mut self, tile_key: &str, attachment_key: usize, cx: &mut Context<Self>) {
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            tile.attachments.retain(|a| a.key != attachment_key);
        }
        cx.notify();
    }

    // ---- composer pickers ----

    pub fn set_tile_model(&mut self, tile_key: &str, model: Option<String>) {
        let variants_by_model = self.variants_by_model.clone();
        let Some(tile) = self.tile_by_key_mut(tile_key) else { return };
        tile.selected_model = model;
        tile.variants = variant_list_for(&variants_by_model, &tile.selected_model);
        tile.selected_variant = Some(DEFAULT_VARIANT_ENTRY.into());
        self.persist_composer();
    }

    pub fn set_tile_variant(&mut self, tile_key: &str, variant: Option<String>) {
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            tile.selected_variant = variant;
        }
        self.persist_composer();
    }

    pub fn set_tile_agent(&mut self, tile_key: &str, agent: Option<String>) {
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            tile.selected_agent = agent;
        }
        self.persist_composer();
    }

    // ---- sidebar geometry ----

    pub fn set_sidebar_width(&mut self, width: f32) {
        let width = width.clamp(theme::SIDEBAR_MIN_WIDTH, theme::SIDEBAR_MAX_WIDTH);
        if (self.sidebar_shown_width - width).abs() < 1.0 {
            return;
        }
        self.sidebar_shown_width = width;
        self.ui.sidebar_width = Some(width as f64);
        self.ui.save();
    }

    pub fn set_sidebar_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.sidebar_visible == visible {
            return;
        }
        self.sidebar_visible = visible;
        self.ui.sidebar_visible = Some(visible);
        self.ui.save();
        cx.notify();
    }

    /// Persists the dragged sidebar width after a resize gesture ends.
    pub fn commit_sidebar_width(&mut self) {
        self.ui.sidebar_width = Some(self.sidebar_shown_width as f64);
        self.ui.save();
    }

    /// Settings page applied: pushes persisted values back into live state.
    pub fn apply_saved_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(allowed) = self.settings.auto_allow_always {
            self.auto_allow = allowed;
        }
        self.ui.auto_allow_always = Some(self.auto_allow);
        self.fonts = theme::font_tokens(self.settings.font_scale.as_deref(), self.system_font_factor);
        self.accent = AccentColors::for_preset(theme::normalize_accent(self.settings.accent.as_deref()));
        self.tool_tips_visible = self.settings.show_tool_tips;
        self.settings.save();
        self.save_state();
        cx.notify();
    }
}

// extra helpers: the placeholder pattern lets tile methods borrow &Store while
// the tile itself is checked out of the vec
impl Store {
    fn placeholder_tile() -> ChatTile {
        ChatTile::new("::placeholder".into(), None, None, None)
    }

    /// Checks a tile out of the grid, runs `f` with shared store access, then puts it back.
    pub fn with_tile<R>(&mut self, key: &str, f: impl FnOnce(&mut ChatTile, &Store, &mut usize) -> R) -> Option<R> {
        let index = self.open_tiles.iter().position(|t| t.key == key)?;
        let mut tile = std::mem::replace(&mut self.open_tiles[index], Self::placeholder_tile());
        let mut next_key = self.next_key;
        let result = f(&mut tile, self, &mut next_key);
        self.next_key = next_key;
        self.open_tiles[index] = tile;
        Some(result)
    }
}

fn read_session_id(data: &serde_json::Value) -> Option<&str> {
    data.get("sessionID").and_then(serde_json::Value::as_str)
}

fn read_status(data: &serde_json::Value) -> String {
    match data.get("status") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Object(_)) => data
            .pointer("/status/type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn read_error(data: &serde_json::Value) -> String {
    if let Some(error) = data.get("error").filter(|e| e.is_object()) {
        let name = error.get("name").and_then(serde_json::Value::as_str).unwrap_or("");
        if name.to_lowercase().contains("abort") {
            return "Turn stopped.".into();
        }
        // API errors carry the human message nested in data; everything else is transport noise
        let mut message = String::new();
        if let Some(inner) = error.pointer("/data/message").and_then(serde_json::Value::as_str) {
            if !inner.is_empty() {
                message = inner.to_string();
            }
        }
        if message.is_empty() {
            if let Some(outer) = error.get("message").and_then(serde_json::Value::as_str) {
                if !outer.is_empty() {
                    message = outer.to_string();
                }
            }
        }
        if message.is_empty() && !name.is_empty() {
            message = name.to_string();
        }
        if message.is_empty() {
            message = "Session error.".into();
        }
        let text = if message.starts_with("Error:") {
            message
        } else {
            format!("Error: {message}")
        };
        return if text.len() > 600 {
            // cut on a char boundary — byte 600 can split a multibyte char
            let mut cut = 600;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}…", &text[..cut])
        } else {
            text
        };
    }
    "Session error.".into()
}

fn build_question_item(request: &oc::QuestionRequestInfo, next_key: &mut usize) -> QuestionItem {
    let _ = next_key;
    QuestionItem {
        id: request.id.clone(),
        session_id: request.session_id.clone(),
        questions: request
            .questions
            .clone()
            .into_iter()
            .map(|q| QuestionEntry {
                question: q.question,
                header: q.header,
                multiple: q.multiple,
                options: q
                    .options
                    .into_iter()
                    .map(|o| QuestionOptionItem { label: o.label, description: o.description, is_selected: false })
                    .collect(),
                custom_text: String::new(),
                locked: false,
            })
            .collect(),
        resolution: None,
    }
}

fn normalize_dir(path: &str) -> String {
    let trimmed = path.trim();
    match std::fs::canonicalize(trimmed) {
        Ok(full) => full.to_string_lossy().trim_end_matches('/').to_string(),
        Err(_) => trimmed.trim_end_matches('/').to_string(),
    }
}

/// Cheap pseudo-random in [0, n) for the auto-allow wait.
fn rand_0_to(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as i64 ^ (d.as_secs() as i64))
        .unwrap_or(0);
    nanos.rem_euclid(n + 1)
}
