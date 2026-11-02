//! The full application state: workspaces, servers, the shared tile grid,
//! transcripts, queues, permissions, questions and subagent tracking. A 1:1
//! port of MainViewModel + ChatTileViewModel + the ChatItems model.

use crate::oc::{
    self, ApiRequest, CommandInfo, DSH_SERVER_KEY, ModelInfo, NetCommand, SessionInfo,
    StoredMessage, UiEvent,
};
use crate::settings::{AppSettings, DEFAULT_EXECUTABLE, HarnessKind, UiState, WorkspaceState};
pub mod ipc;
use crate::theme::{self, AccentColors, FontTokens};
use chrono::{Local, TimeZone, Timelike};
use gpui::prelude::*;
use gpui::{App, Context, Entity};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const DEFAULT_MODEL_ENTRY: &str = "(server default)";
pub const DEFAULT_VARIANT_ENTRY: &str = "(default)";
pub const DEFAULT_AGENT_ENTRY: &str = "(default build)";
pub const DRAFT_TITLE: &str = "New chat";

/// Minimum spacing between stream-driven repaints; 10 fps is plenty for
/// streaming text and keeps a busy turn off the compositor's frame rate.
const STREAM_REPAINT_INTERVAL: Duration = Duration::from_millis(100);

/// ["(default)"] plus the model's variants — a tile's picker list. Stays at
/// the bare sentinel until the server's model list lands (ModelsLoaded) or
/// the model is a real key present in `variants_by_model`.
fn variant_list_for(
    variants_by_model: &HashMap<String, Vec<String>>,
    model: &Option<String>,
) -> Vec<String> {
    let mut variants = vec![DEFAULT_VARIANT_ENTRY.to_string()];
    if let Some(m) = model
        && let Some(vs) = variants_by_model.get(m)
    {
        variants.extend(vs.iter().cloned());
    }
    variants
}

/// A session's own model + variant, read from its last assistant message —
/// an old chat reopens with these instead of the global seeds. None when the
/// chat has no assistant reply yet (or the message predates model metadata).
fn session_chat_settings(messages: &[StoredMessage]) -> Option<(String, Option<String>)> {
    let info = &messages.iter().rev().find(|m| m.role == "assistant")?.info;
    let provider = info.get("providerID").and_then(serde_json::Value::as_str)?;
    let model = info.get("modelID").and_then(serde_json::Value::as_str)?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    let variant = info
        .get("variant")
        .and_then(serde_json::Value::as_str)
        .map(String::from);
    Some((format!("{provider}/{model}"), variant))
}

/// Effort rank for the variant names opencode serves (models.dev tiers):
/// minimal/none < low < medium < high < xhigh < max; unknown names rank below
/// everything. Ties break toward the later list entry (the list is sorted).
pub(crate) fn variant_rank(v: &str) -> i32 {
    match v.to_lowercase().as_str() {
        "off" | "minimal" | "none" => 0,
        "low" => 1,
        "medium" | "mid" => 2,
        "high" => 3,
        "xhigh" => 4,
        "max" => 5,
        _ => -1,
    }
}

/// The model's top-tier variant, for the "always highest variant" default.
fn highest_variant(variants: &[String]) -> Option<String> {
    variants.iter().max_by_key(|v| variant_rank(v)).cloned()
}

/// The tile's `@`-mention token: the last `@` that starts a token (text start
/// or right after whitespace) with no whitespace between it and the end.
/// Returns the byte index of the `@` and the query typed after it.
pub fn at_token_start(text: &str) -> Option<(usize, String)> {
    let byte_at = text.rfind('@')?;
    if text.is_char_boundary(byte_at) {
        let prev = text[..byte_at].chars().next_back();
        if prev.is_none_or(|c| c.is_whitespace())
            && !text[byte_at + 1..].chars().any(char::is_whitespace)
        {
            return Some((byte_at, text[byte_at + 1..].to_string()));
        }
    }
    None
}

/// `/name args` → `(name, args)` when `name` matches a server command or
/// skill. The template expansion is the server's job (POST
/// /session/{id}/command), so the composer, history and transcript keep the
/// short `/name args` form — no whole skill files pasted into prompts.
pub fn parse_slash_command(text: &str, commands: &[CommandInfo]) -> Option<(String, String)> {
    let rest = text.strip_prefix('/')?;
    let (name, args) = match rest.find(char::is_whitespace) {
        Some(pos) => (&rest[..pos], rest[pos..].trim()),
        None => (rest, ""),
    };
    if commands.iter().any(|c| c.name == name) {
        Some((name.to_string(), args.to_string()))
    } else {
        None
    }
}

pub fn format_tokens(tokens: i64) -> String {
    if tokens < 1000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.1$}k", tokens as f64 / 1000.0, 1)
            .trim_end_matches(".0")
            .to_string()
    } else {
        let v = tokens as f64 / 1_000_000.0;
        if (v * 100.0).fract().abs() < f64::EPSILON {
            format!("{:.2}M", v)
                .trim_end_matches("00")
                .trim_end_matches('.')
                .to_string()
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
    let Some(at) = from_ms(updated_ms) else {
        return String::new();
    };
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

#[derive(Clone, Debug)]
pub struct ToolItem {
    /// opencode part id — item identity must match the "t{id}" lookup in
    /// on_part_updated or every stream snapshot re-pushes the row
    pub part_id: String,
    pub tool: String,
    pub status: String,
    pub title: String,
    pub file_path: String,
    pub diff: Option<String>,
    pub numstat: Option<Numstat>,
    pub is_error: bool,
    pub error_preview: String,
    pub details: ToolDetails,
    /// fingerprint of the last applied `state` JSON — a re-sent identical
    /// snapshot skips the clone + JSON pretty-print entirely
    state_sig: Option<u64>,
}

impl PartialEq for ToolItem {
    fn eq(&self, other: &Self) -> bool {
        // state_sig is bookkeeping, not a visible field
        self.part_id == other.part_id
            && self.tool == other.tool
            && self.status == other.status
            && self.title == other.title
            && self.file_path == other.file_path
            && self.diff == other.diff
            && self.numstat == other.numstat
            && self.is_error == other.is_error
            && self.error_preview == other.error_preview
            && self.details == other.details
    }
}

/// Tool call detail sections; the details fold draws each as its own
/// labeled block.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolDetails {
    pub input: String,
    pub output: String,
    pub error: String,
}

impl ToolDetails {
    pub fn is_empty(&self) -> bool {
        self.input.is_empty() && self.output.is_empty() && self.error.is_empty()
    }
}

impl ToolItem {
    pub fn is_running(&self) -> bool {
        self.status == "running"
    }
    pub fn has_diff(&self) -> bool {
        self.diff.as_deref().is_some_and(|d| !d.is_empty())
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
            ChatItem::Tool(t) => format!("t{}", t.part_id),
            ChatItem::Image { key, .. } => format!("i{key}"),
            ChatItem::Permission(p) => format!("p{}", p.id),
            ChatItem::Question(q) => format!("q{}", q.id),
            ChatItem::SystemNote { key, .. } => format!("n{key}"),
        }
    }

    /// Allocation-free `id()` comparison: item lookups run per SSE part
    /// event, so building a String per candidate item would churn the
    /// allocator at token rate on long transcripts.
    pub fn matches_id(&self, item_id: &str) -> bool {
        match self {
            ChatItem::UserMessage { key, .. } => match_usize(item_id, 'u', *key),
            ChatItem::AssistantText { key, .. } => match_str(item_id, 'a', key),
            ChatItem::Reasoning { key, .. } => match_str(item_id, 'r', key),
            ChatItem::Tool(t) => match_str(item_id, 't', &t.part_id),
            ChatItem::Image { key, .. } => match_str(item_id, 'i', key),
            ChatItem::Permission(p) => match_str(item_id, 'p', &p.id),
            ChatItem::Question(q) => match_str(item_id, 'q', &q.id),
            ChatItem::SystemNote { key, .. } => match_usize(item_id, 'n', *key),
        }
    }

    /// Whether this item is keyed by the given SSE part id (any flavour the
    /// stream creates items for); lets `on_part_removed` scan once instead
    /// of once per id prefix.
    pub fn matches_part_id(&self, part_id: &str) -> bool {
        match self {
            ChatItem::AssistantText { key, .. }
            | ChatItem::Reasoning { key, .. }
            | ChatItem::Image { key, .. } => key == part_id,
            ChatItem::Tool(t) => t.part_id == part_id,
            _ => false,
        }
    }
}

fn match_str(item_id: &str, prefix: char, key: &str) -> bool {
    item_id.strip_prefix(prefix) == Some(key)
}

fn match_usize(item_id: &str, prefix: char, key: usize) -> bool {
    // ids only ever come from format!("{prefix}{key}"), so a numeric
    // round-trip cannot hit a false positive on server-generated keys
    item_id
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.parse::<usize>() == Ok(key))
}

/// A message parked while a turn is still running; sent when the session goes idle.
#[derive(Clone, Debug)]
pub struct QueueEntry {
    pub session_id: String,
    pub text: String,
    pub attachments: Vec<AttachmentItem>,
    pub key: usize,
}

/// The exact prompt a chat last put on the wire, kept so a retryable
/// connection error can replay it verbatim — re-reading the tile would pick
/// up model/variant switches made while the failed turn ran.
#[derive(Clone)]
struct PromptPayload {
    text: String,
    attachments: Vec<AttachmentItem>,
    model: Option<String>,
    variant: Option<String>,
    agent: Option<String>,
    command: Option<(String, String)>,
    /// automatic resends already spent on this prompt
    attempts: u32,
}

/// Automatic resends allowed per prompt before the error note stands as-is.
const MAX_AUTO_RETRY: u32 = 3;

/// Automatic restarts allowed when a workspace's serve process crashes,
/// before the app gives up and leaves the workspace stopped.
const MAX_CRASH_RESTARTS: u32 = 3;
/// A server that stayed up this long earns a fresh crash ladder — otherwise
/// a spawn-that-then-crashes cycle would reset its count on every start.
const CRASH_RESTART_RESET_AFTER: Duration = Duration::from_secs(600);

/// Pause before auto-retry attempt n; the server already spent its own
/// backoff ladder (tens of seconds) before the error reached us.
fn retry_backoff(attempts: u32) -> Duration {
    Duration::from_secs(5 * attempts.max(1) as u64)
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
    last_used_model: Option<String>,
    context_limit: i64,

    // prompt-history browsing: 0 = not browsing, N = N steps back from the newest entry
    history_index: usize,
    history_draft: String,
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
    fn apply_variant_list(
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
    fn peek_sent_prompt_key(&self, key: usize) -> String {
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

    fn set_input_direct(&mut self, text: String) {
        self.suppress_history_reset = true;
        self.input = text;
        self.suppress_history_reset = false;
    }

    fn update_context_text(&mut self) {
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

// ---- JSON shape parsing (ChatParsing port) -------------------------------

/// Context limit for a model key: exact `provider/model` match first, then a
/// model-name-only match — message info (`providerID`) and the provider list
/// can disagree on the provider segment (custom providers, forks). 0 when the
/// model is unknown to every running server.
fn lookup_context_limit(limits: &HashMap<String, i64>, key: &str) -> Option<i64> {
    if let Some(limit) = limits.get(key) {
        return if *limit > 0 { Some(*limit) } else { None };
    }
    let name = key.rsplit('/').next()?;
    limits
        .iter()
        .filter(|(k, _)| k.rsplit('/').next() == Some(name))
        .map(|(_, limit)| *limit)
        .find(|limit| *limit > 0)
}

pub fn read_token_total(info: &serde_json::Value) -> i64 {
    let Some(tokens) = info.get("tokens").and_then(serde_json::Value::as_object) else {
        return 0;
    };
    let sum = |name: &str| {
        tokens
            .get(name)
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    };
    let (mut cache_read, mut cache_write) = (0, 0);
    if let Some(cache) = tokens.get("cache").and_then(serde_json::Value::as_object) {
        cache_read = cache
            .get("read")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        cache_write = cache
            .get("write")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
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
        let input = tokens
            .get("input")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let output = tokens
            .get("output")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        if input > 0 || output > 0 {
            parts.push(format!(
                "↑{} ↓{}",
                format_tokens(input),
                format_tokens(output)
            ));
        }
    }
    let cost = info
        .get("cost")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    if cost > 0.0 {
        parts.push(
            format!("${:.4}", cost)
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_string(),
        );
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn describe_part(state: &serde_json::Value, name: &str) -> String {
    let Some(value) = state.get(name) else {
        return String::new();
    };
    if value.is_null() {
        return String::new();
    }
    let text = if let Some(s) = value.as_str() {
        // string payloads that are themselves JSON (the server sometimes
        // ships input encoded) pretty-print like structured ones
        let trimmed = s.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            serde_json::from_str::<serde_json::Value>(s)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .unwrap_or_else(|| s.to_string())
        } else {
            s.to_string()
        }
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

/// Short label for a tool call for the stretch before the server sends a
/// state.title: the first meaningful value from state.input (command, file
/// path, pattern, …) so a pending row says more than the tool name.
fn tool_label_from_input(state: &serde_json::Value) -> Option<String> {
    let input = state.get("input").and_then(serde_json::Value::as_object)?;
    const CANDIDATES: [&str; 12] = [
        "command",
        "filePath",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "description",
        "prompt",
        "args",
        "skill",
        "name",
    ];
    let mut pick: Option<String> = None;
    for key in CANDIDATES {
        let Some(value) = input.get(key) else {
            continue;
        };
        let text = match value {
            serde_json::Value::String(s) => Some(s.trim().to_string()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Array(a) => Some(
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .to_string(),
            ),
            _ => None,
        };
        if text.as_deref().is_some_and(|t| !t.is_empty()) {
            pick = text;
            break;
        }
    }
    let pick = pick.or_else(|| {
        input
            .values()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(String::from)
    })?;
    let label = pick.split_whitespace().collect::<Vec<_>>().join(" ");
    if label.is_empty() {
        return None;
    }
    if label.len() <= 120 {
        return Some(label);
    }
    let mut cut = 120;
    while cut > 0 && !label.is_char_boundary(cut) {
        cut -= 1;
    }
    Some(format!("{}…", &label[..cut]))
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
    } else if let Some(label) = tool_label_from_input(state) {
        tool.title = label;
    }
    tool.details = ToolDetails {
        input: describe_part(state, "input"),
        output: describe_part(state, "output"),
        error: describe_part(state, "error"),
    };
    tool.error_preview = tool
        .details
        .error
        .split('\n')
        .next()
        .unwrap_or("")
        .to_string();
}

fn apply_tool_metadata(state: &serde_json::Value, tool: &mut ToolItem) {
    let Some(md) = state.get("metadata").and_then(serde_json::Value::as_object) else {
        return;
    };
    if let Some(file_path) = md.get("filepath").and_then(serde_json::Value::as_str)
        && !file_path.is_empty()
    {
        tool.file_path = file_path.into();
    }
    if let Some(fd) = md.get("filediff").and_then(serde_json::Value::as_object) {
        tool.diff = fd
            .get("patch")
            .and_then(serde_json::Value::as_str)
            .map(String::from);
        tool.numstat = Some(Numstat {
            additions: fd
                .get("additions")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0),
            deletions: fd
                .get("deletions")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0),
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
        let status = todo
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("pending");
        result.push(TaskItem {
            content: content.into(),
            status: status.into(),
        });
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

/// Sidebar display name for a workspace folder: "General" for the built-in
/// workspace, otherwise the folder's basename.
fn workspace_display_name(path: &str) -> String {
    if path == crate::paths::AppPaths::general_workspace_path().to_string_lossy() {
        return "General".to_string();
    }
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .unwrap_or(path)
        .to_string()
}

/// Sort key: General leads, the rest alphabetical by name (case-insensitive).
/// Single-sourced so every workspace-list sort keeps General on top.
fn workspace_order_key(path: &str) -> (bool, String) {
    let general = path == crate::paths::AppPaths::general_workspace_path().to_string_lossy();
    (!general, workspace_display_name(path).to_lowercase())
}

/// Sidebar order: General leads, the rest alphabetical by name (case-insensitive).
fn sort_workspaces(workspaces: &mut [WorkspaceState]) {
    workspaces.sort_by_key(|w| workspace_order_key(&w.path));
}

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
        let name = workspace_display_name(&state.path);
        Self {
            state,
            name,
            is_running: false,
            is_selected: false,
            chats: Vec::new(),
        }
    }

    pub fn folder_path(&self) -> &str {
        &self.state.path
    }

    pub fn is_general(&self) -> bool {
        self.state.path == crate::paths::AppPaths::general_workspace_path().to_string_lossy()
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
    pub created_ms: i64,
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

/// Sidebar chat order: pinned first, then newest first — by last activity,
/// falling back to creation time when a server never fills `updated`.
fn session_recency(s: &SessionItem) -> std::cmp::Reverse<(bool, i64, i64)> {
    std::cmp::Reverse((s.is_pinned, s.updated_ms, s.created_ms))
}

// ---- the store -----------------------------------------------------------

struct WorkspaceRuntime {
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
    /// the destructive menu row the user armed; the window shows Confirm
    pub menu_armed: Option<String>,
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
    /// last prompt per session, replayed when a turn dies retryable
    last_prompt_by_session: HashMap<String, PromptPayload>,
    /// sessions whose turn died retryable; their next session.idle re-sends
    retry_due_by_session: HashSet<String>,
    /// consecutive crash restarts per workspace, with the last crash time;
    /// the count decays after a long stable stretch
    crash_restarts: HashMap<String, (u32, Instant)>,
    /// The one evolving "connection lost" note per workspace: (tile key, note
    /// key, last event). Drops within oc::RECONNECT_RESET_AFTER of the last
    /// event update that note in place (its count climbs) instead of stacking
    /// a new card per drop.
    reconnect_notes: HashMap<String, (String, usize, Instant)>,
    pub queue: Vec<QueueEntry>,
    workspace_generation: u64,
    running_by_session: HashSet<String>,
    /// Every catalog entry with its harness, including the two harnesses'
    /// same-named models; `models` stays the de-duplicated picker list.
    model_infos: Vec<ModelInfo>,
    /// Harnesses whose catalog came from a live server this session. A
    /// cache-loaded list still answers one door request with a refresh, so
    /// clients see real variants instead of the cache's empty ones.
    models_live: HashSet<HarnessKind>,
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
    /// harness of each in-flight session create, FIFO-aligned with
    /// pending_sends so a dsh create is recognized on its reply
    pending_create_harness: std::collections::VecDeque<(String, HarnessKind)>,
    /// dsh drafts parked while the dsh server starts: (tile key, text,
    /// attachments, workspace folder). ServerStarted(DSH) converts them into
    /// pending_sends + CreateDshSession pairs.
    pending_dsh_drafts: Vec<(String, String, Vec<AttachmentItem>, String)>,
    /// workspaces started by Restore whose open-tile lists reload on ServerStarted
    pending_restores: Vec<(String, Vec<String>)>,
    /// chats requested while their workspace server was still starting
    pending_opens: Vec<(String, String)>,
    /// sends parked because the chat's server had idled out; they flow when
    /// ServerStarted lands for their net key
    pending_session_sends: Vec<(String, String, String, Vec<AttachmentItem>)>,
    /// last user/turn activity per net key; the idle sweep stops opencode
    /// servers that stay quiet past the configured minutes
    server_activity: HashMap<String, Instant>,
    /// last idle-server sweep, so the 1 Hz tick only runs it every 30 s
    last_idle_sweep: Option<Instant>,
    /// tile whose flyout should spring open after a live todo batch
    pub todos_spring: Option<String>,
    /// when the last coalesced stream repaint ran; None = never
    stream_repaint_last: Option<Instant>,
    /// a deferred stream repaint is already scheduled
    stream_repaint_scheduled: bool,
    /// tiles whose transcript changed since the last stream flush; flushed
    /// as per-tile revision bumps, never as a store-level notify
    stream_dirty: Vec<String>,
    /// one-shot prompts DevTools runs through the door, keyed by wire id
    ipc_runs: HashMap<u64, ipc::IpcRun>,
    /// the door's models command parked while a server spins up, with the
    /// harness it asked for (None = any)
    ipc_models_waiter: Option<(Option<HarnessKind>, crate::ipc::Reply)>,
    /// reused checkout dummy for `with_tile` — hot stream events would
    /// otherwise build a throwaway ChatTile each
    tile_placeholder: Option<ChatTile>,

    // ---- composer suggestions (/commands, @files) ----
    /// The workspace's server commands + skills, keyed by workspace folder
    /// path; filled on ServerStarted and refreshed when a slash menu opens.
    pub commands: HashMap<String, Vec<CommandInfo>>,
    /// The workspace's configured MCP server names, keyed by workspace folder
    /// path; filled on ServerStarted (GET /config "mcp" section). Drives the
    /// composer's MCP toggle.
    pub mcp_names: HashMap<String, Vec<String>>,
    /// Workspace folder paths whose MCP servers the user keeps disconnected;
    /// re-asserted with runtime disconnects after every server (re)start.
    pub mcp_disabled: HashSet<String>,
    /// Latest @-mention filename search answer (last request wins); the query
    /// rides along so a tile can tell stale answers from its current token.
    pub file_results: Option<FileSuggest>,
}

/// A /find/file answer as delivered to the @ mention menu.
pub struct FileSuggest {
    pub ws_key: String,
    pub query: String,
    pub paths: Vec<String>,
}

impl Store {
    pub fn new(cx: &mut App, net: crate::oc::NetSender) -> Entity<Self> {
        let settings = AppSettings::load();
        let mut ui = UiState::load();
        // one-time migration: the pre-settings auto-allow choice moves into settings.json
        if settings.auto_allow_always.is_none() && ui.auto_allow_always.is_some() {
            // keep reading from ui state like the C# fallback until settings has it
        }
        let accent_preset = theme::normalize_accent(settings.accent.as_deref());
        let fonts = theme::active_tokens();
        let auto_allow = ui.auto_allow_always.unwrap_or(true);

        if ui.workspaces.is_empty()
            && let Some(folder) = ui.folder.clone().filter(|f| !f.trim().is_empty())
        {
            ui.workspaces.insert(
                0,
                WorkspaceState {
                    path: normalize_dir(&folder),
                    ..Default::default()
                },
            );
        }

        let mut startup_models = vec![DEFAULT_MODEL_ENTRY.to_string()];
        let mut model_infos = Vec::new();
        let mut variants_by_model = HashMap::new();
        let mut context_limit_by_model = HashMap::new();
        for model in crate::settings::load_models_cache() {
            let key = model.key.trim().to_string();
            if key.is_empty() || key.starts_with('(') {
                continue;
            }
            if !startup_models.contains(&key) {
                startup_models.push(key.clone());
            }
            if !model.variants.is_empty() {
                variants_by_model.insert(key.clone(), model.variants.clone());
            }
            context_limit_by_model.insert(key, model.context_limit);
            model_infos.push(model);
        }
        if let Some(model) = settings
            .default_model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            && !startup_models.iter().any(|known| known == model)
        {
            startup_models.push(model.to_string());
        }

        let mut store = Self {
            settings: settings.clone(),
            ui: ui.clone(),
            accent: AccentColors::for_preset(accent_preset),
            fonts,
            tool_tips_visible: settings.show_tool_tips,
            workspaces: Vec::new(),
            selected_workspace: None,
            sessions: Vec::new(),
            models: startup_models,
            agents: vec![DEFAULT_AGENT_ENTRY.into()],
            running_agents: Vec::new(),
            open_tiles: Vec::new(),
            focused_tile: None,
            is_server_running: false,
            auto_allow,
            is_settings_open: false,
            menu_armed: None,
            show_restore_prompt: false,
            restore_prompt_text: String::new(),
            sidebar_shown_width: ui
                .sidebar_width
                .unwrap_or(theme::SIDEBAR_DEFAULT_WIDTH as f64)
                .clamp(
                    theme::SIDEBAR_MIN_WIDTH as f64,
                    theme::SIDEBAR_MAX_WIDTH as f64,
                ) as f32,
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
            last_prompt_by_session: HashMap::new(),
            retry_due_by_session: HashSet::new(),
            crash_restarts: HashMap::new(),
            reconnect_notes: HashMap::new(),
            queue: Vec::new(),
            workspace_generation: 0,
            running_by_session: HashSet::new(),
            model_infos,
            models_live: HashSet::new(),
            variants_by_model,
            context_limit_by_model,
            subagent_by_session: HashMap::new(),
            running_subagents: HashSet::new(),
            countdowns: HashMap::new(),
            prompt_history: Vec::new(),
            pending_sends: Vec::new(),
            pending_create_harness: std::collections::VecDeque::new(),
            pending_dsh_drafts: Vec::new(),
            pending_restores: Vec::new(),
            pending_opens: Vec::new(),
            pending_session_sends: Vec::new(),
            server_activity: HashMap::new(),
            last_idle_sweep: None,
            todos_spring: None,
            stream_repaint_last: None,
            stream_repaint_scheduled: false,
            stream_dirty: Vec::new(),
            tile_placeholder: None,
            ipc_runs: HashMap::new(),
            ipc_models_waiter: None,
            commands: HashMap::new(),
            file_results: None,
            mcp_names: HashMap::new(),
            mcp_disabled: ui.mcp_disabled_ws.iter().cloned().collect(),
        };

        // heal paths saved by older builds: fs::canonicalize used to leak the
        // Windows verbatim `\\?\` prefix into stored workspaces, which explorer
        // and Windows Terminal reject
        for ws in &mut store.ui.workspaces {
            ws.path = normalize_dir(&ws.path);
        }

        // ensure the General workspace exists and leads the list
        let general_path = crate::paths::AppPaths::general_workspace_path()
            .to_string_lossy()
            .to_string();
        if !store.ui.workspaces.iter().any(|w| w.path == general_path) {
            store.ui.workspaces.insert(
                0,
                WorkspaceState {
                    path: general_path.clone(),
                    ..Default::default()
                },
            );
        } else if store.ui.workspaces[0].path != general_path {
            let index = store
                .ui
                .workspaces
                .iter()
                .position(|w| w.path == general_path)
                .unwrap();
            let general = store.ui.workspaces.remove(index);
            store.ui.workspaces.insert(0, general);
        }
        let _ = std::fs::create_dir_all(&general_path);

        sort_workspaces(&mut store.ui.workspaces);

        // every launch starts with the workspace sections collapsed, whatever was saved
        for ws in &mut store.ui.workspaces {
            ws.collapsed = true;
            // saved open-tile ids of chats that no longer exist could never restore
            ws.open_sessions.retain(|id| ws.sessions.contains(id));
        }
        let pending_tiles: usize = store
            .ui
            .workspaces
            .iter()
            .map(|w| w.open_sessions.len())
            .sum();
        store.restore_prompt_text = if pending_tiles == 1 {
            "1 chat was open last time".into()
        } else {
            format!("{pending_tiles} chats were open last time")
        };
        store.show_restore_prompt = pending_tiles > 0;

        store.workspaces = store
            .ui
            .workspaces
            .clone()
            .into_iter()
            .map(WorkspaceItem::new)
            .collect();
        for ws in &store.workspaces {
            for sid in &ws.state.sessions {
                store
                    .workspace_by_session
                    .insert(sid.clone(), ws.folder_path().to_string());
            }
        }

        // restore the last workspace selection but never auto-start a server
        let selected = store
            .workspaces
            .iter()
            .find(|w| Some(w.folder_path()) == store.ui.active_workspace.as_deref())
            .map(|w| w.folder_path().to_string())
            .or_else(|| {
                store
                    .workspaces
                    .first()
                    .map(|w| w.folder_path().to_string())
            });
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
        self.ui
            .model
            .clone()
            .or_else(|| self.settings.default_model.clone())
    }
    pub fn seed_variant(&self) -> Option<String> {
        self.settings.default_variant.clone()
    }
    pub fn seed_agent(&self) -> Option<String> {
        self.ui
            .agent
            .clone()
            .or_else(|| self.settings.default_agent.clone())
    }

    pub fn save_state(&mut self) {
        self.ui.active_workspace = self.selected_workspace.clone();
        self.ui.auto_allow_always = Some(self.auto_allow);
        self.ui.mcp_disabled_ws = {
            let mut paths: Vec<String> = self.mcp_disabled.iter().cloned().collect();
            paths.sort();
            paths
        };
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
            let Some(sid) = &tile.session_id else {
                continue;
            };
            if tile.input.trim().is_empty() {
                self.ui.drafts.remove(sid);
            } else {
                self.ui.drafts.insert(sid.clone(), tile.input.clone());
            }
        }
        self.ui.drafts.retain(|sid, _| {
            self.tile_by_session.contains_key(sid)
                || self
                    .workspaces
                    .iter()
                    .any(|w| w.state.sessions.contains(sid))
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
                .filter(|sid| {
                    self.workspace_by_session
                        .get(sid)
                        .map(|p| p == ws.folder_path())
                        .unwrap_or(false)
                })
                .collect();
            ws.state.open_sessions = ids;
        }
        if let Some(focus_key) = self.focused_tile.clone() {
            let focus_session = self
                .tile_by_key(&focus_key)
                .and_then(|t| t.session_id.clone());
            if let Some(sid) = focus_session {
                let owner = self.workspace_by_session.get(&sid).cloned();
                if let Some(ws) = self.selected_workspace_mut()
                    && owner.as_deref() == Some(ws.folder_path())
                {
                    ws.state.active_session = Some(sid);
                }
            }
        }
    }

    pub fn persist_composer(&mut self) {
        let (model, variant, agent) = {
            let tile = self.focused_tile();
            (
                tile.and_then(|t| t.selected_model.clone())
                    .filter(|m| m != DEFAULT_MODEL_ENTRY),
                tile.and_then(|t| t.selected_variant.clone())
                    .filter(|v| v != DEFAULT_VARIANT_ENTRY),
                tile.and_then(|t| t.selected_agent.clone())
                    .filter(|a| a != DEFAULT_AGENT_ENTRY),
            )
        };
        self.ui.model = model;
        self.ui.variant = variant;
        self.ui.agent = agent;
        self.ui.save();
    }

    // ---- composer suggestions (/commands, @files) ----

    /// Slash commands for the workspace a tile talks to (its own session's
    /// server, else the selected one). Empty when the server has none.
    pub fn tile_commands(&self, tile_key: &str) -> &[CommandInfo] {
        self.tile_net_key_or(tile_key)
            .and_then(|ws| self.commands.get(&ws).map(Vec::as_slice))
            .unwrap_or(&[])
    }

    /// Fire a /command refresh for the tile's workspace (menu open).
    pub fn refresh_commands(&mut self, tile_key: &str) {
        if let Some(ws) = self.tile_net_key_or(tile_key) {
            self.net.send(NetCommand::Request {
                ws_key: ws,
                request: ApiRequest::ListCommands,
            });
        }
    }

    /// Fire a filename search for the tile's workspace (@ menu keystroke).
    pub fn search_files(&mut self, tile_key: &str, query: &str) {
        if let Some(ws) = self.tile_net_key_or(tile_key) {
            self.net.send(NetCommand::Request {
                ws_key: ws,
                request: ApiRequest::FindFiles {
                    query: query.to_string(),
                },
            });
        }
    }

    /// The file-search answer a tile's menu should show right now: only when
    /// it answers this workspace's exact query (older answers don't count).
    pub fn file_results_for(&self, tile_key: &str, query: &str) -> Option<&Vec<String>> {
        let result = self.file_results.as_ref()?;
        if result.query == query
            && self.tile_ws_key(tile_key).as_deref() == Some(result.ws_key.as_str())
        {
            Some(&result.paths)
        } else {
            None
        }
    }

    /// Which workspace's server a tile talks to: its own session's workspace,
    /// or the selected one for drafts.
    pub fn tile_ws_key(&self, tile_key: &str) -> Option<String> {
        self.tile_by_key(tile_key)
            .and_then(|t| t.session_id.clone())
            .and_then(|sid| self.workspace_by_session.get(&sid).cloned())
            .or_else(|| self.selected_workspace.clone())
    }

    /// The MCP toggle's state for a tile: `(has servers, enabled)` — `has` is
    /// false when the workspace's server is down or has no `mcp` config, and
    /// the button renders dimmed. The settings master switch overrides.
    pub fn mcp_state(&self, tile_key: &str) -> (bool, bool) {
        let Some(ws_key) = self.tile_ws_key(tile_key) else {
            return (false, false);
        };
        let has = self
            .mcp_names
            .get(&ws_key)
            .is_some_and(|names| !names.is_empty());
        let on = has && !self.settings.mcp_disabled && !self.mcp_disabled.contains(&ws_key);
        (has, on)
    }

    /// The composer's MCP toggle: runtime-disconnects (or reconnects) every
    /// configured MCP server of the tile's workspace. Instant, touches no
    /// config file; the choice persists in ui.json and is re-asserted after
    /// every server (re)start. The settings master switch wins over it.
    pub fn toggle_mcp(&mut self, tile_key: &str, cx: &mut Context<Self>) {
        let Some(ws_key) = self.tile_ws_key(tile_key) else {
            return;
        };
        if self.settings.mcp_disabled {
            self.announce("MCP is disabled for all workspaces in Settings.", true, cx);
            return;
        }
        let names = self.mcp_names.get(&ws_key).cloned().unwrap_or_default();
        if names.is_empty() {
            self.announce("No MCP servers configured for this workspace.", true, cx);
            return;
        }
        let disabling = !self.mcp_disabled.contains(&ws_key);
        for name in names {
            self.net.send(NetCommand::Request {
                ws_key: ws_key.clone(),
                request: if disabling {
                    ApiRequest::McpDisconnect { name }
                } else {
                    ApiRequest::McpConnect { name }
                },
            });
        }
        if disabling {
            self.mcp_disabled.insert(ws_key.clone());
        } else {
            self.mcp_disabled.remove(&ws_key);
        }
        tracing::info!(
            "mcp {} for {ws_key}",
            if disabling { "disabled" } else { "enabled" }
        );
        self.save_state();
        self.repaint(cx);
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

    /// Bump one tile's revision: its cached view re-renders (via its own
    /// observer) while every other tile keeps its recycled subtree. Fires no
    /// store-level notify — use `repaint` when global chrome changed too.
    pub fn touch_tile(&self, key: &str, cx: &mut Context<Self>) {
        if let Some(rev) = self.tile_by_key(key).and_then(|t| t.rev.clone()) {
            rev.update(cx, |(), cx| cx.notify());
        }
    }

    /// Bump every open tile's revision (settings/accent/global-state style
    /// changes that all tiles render).
    pub fn touch_all_tiles(&self, cx: &mut Context<Self>) {
        for rev in self.open_tiles.iter().filter_map(|t| t.rev.clone()) {
            rev.update(cx, |(), cx| cx.notify());
        }
    }

    /// The full-store repaint: sidebar/titlebar chrome AND every tile view.
    /// The default for store notifications; the hot stream path touches
    /// single tiles instead.
    pub fn repaint(&self, cx: &mut Context<Self>) {
        self.touch_all_tiles(cx);
        cx.notify();
    }

    pub fn focused_tile(&self) -> Option<&ChatTile> {
        self.focused_tile
            .as_deref()
            .and_then(|key| self.tile_by_key(key))
    }

    pub fn tile_for_session(&self, session_id: &str) -> Option<&ChatTile> {
        self.tile_by_session
            .get(session_id)
            .and_then(|key| self.tile_by_key(key))
    }

    pub fn has_runtime(&self, session_id: &str) -> bool {
        self.net_key_for_session(session_id)
            .map(|p| self.runtimes.contains_key(&p))
            .unwrap_or(false)
    }

    /// Whether this session's workspace event stream is currently reconnecting.
    /// A session without a workspace (fresh draft) falls back to the selected
    /// workspace; a stopped workspace has no runtime and never reads true.
    pub fn stream_reconnecting(&self, session_id: Option<&str>) -> bool {
        let path = session_id
            .and_then(|id| self.net_key_for_session(id))
            .or_else(|| self.selected_workspace.clone());
        path.and_then(|p| self.runtimes.get(&p))
            .map(|rt| matches!(rt.stream_status, oc::StreamStatus::Reconnecting { .. }))
            .unwrap_or(false)
    }

    /// Gauge inputs over every running workspace; any reconnecting server
    /// outranks a merely unhealthy one (checked first at the call site).
    pub fn any_stream_reconnecting(&self) -> bool {
        self.runtimes
            .values()
            .any(|rt| matches!(rt.stream_status, oc::StreamStatus::Reconnecting { .. }))
    }

    pub fn any_stream_unhealthy(&self) -> bool {
        self.runtimes.values().any(|rt| rt.healthy == Some(false))
    }

    pub fn is_busy(&self, session_id: &str) -> bool {
        self.running_by_session.contains(session_id)
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
        self.tiles_waiting() > 0 || self.running_agents.iter().any(|r| r.needs_attention)
    }

    /// Open chats that hold a card only a person can answer.
    pub fn tiles_waiting(&self) -> usize {
        self.open_tiles
            .iter()
            .filter(|tile| tile.is_waiting())
            .count()
    }

    /// Whether this chat's open tile is waiting on a person.
    pub fn session_waiting(&self, session_id: &str) -> bool {
        self.open_tiles
            .iter()
            .any(|tile| tile.session_id.as_deref() == Some(session_id) && tile.is_waiting())
    }

    fn effective_workspace_path(&self, session_id: &str) -> Option<String> {
        if let Some(path) = self.workspace_by_session.get(session_id) {
            return Some(path.clone());
        }
        if let Some(sub) = self.subagent_by_session.get(session_id)
            && let Some(path) = self.workspace_by_session.get(&sub.parent_session_id)
        {
            return Some(path.clone());
        }
        None
    }

    /// Which backend a session runs on. Sessions predate per-chat harnesses,
    /// so anything unrecorded is opencode.
    pub fn session_harness(&self, session_id: &str) -> HarnessKind {
        match self.ui.harness_by_session.get(session_id) {
            Some(HarnessKind::Dsh) => HarnessKind::Dsh,
            _ => HarnessKind::OpenCode,
        }
    }

    /// The net-thread key a session's server lives on: the workspace folder
    /// for opencode, [`DSH_SERVER_KEY`] for dsh (one shared process).
    fn net_key_for_session(&self, session_id: &str) -> Option<String> {
        if self.session_harness(session_id) == HarnessKind::Dsh {
            return Some(DSH_SERVER_KEY.into());
        }
        self.effective_workspace_path(session_id)
    }

    /// Same for a tile: an existing chat routes by its session; a draft by
    /// the harness pill the composer is showing.
    fn tile_net_key_or(&self, tile_key: &str) -> Option<String> {
        if let Some(tile) = self.tile_by_key(tile_key) {
            if tile.harness == HarnessKind::Dsh {
                return Some(DSH_SERVER_KEY.into());
            }
            if let Some(sid) = &tile.session_id {
                return self.net_key_for_session(sid);
            }
        }
        self.tile_ws_key(tile_key)
    }

    /// Starts the shared dsh server unless it is already up. Replies arrive
    /// as ServerStarted(DSH_SERVER_KEY); parked work drains there.
    pub fn ensure_dsh(&mut self, cx: &mut Context<Self>) {
        self.touch_server_activity(DSH_SERVER_KEY);
        if self.runtimes.contains_key(DSH_SERVER_KEY) {
            return;
        }
        let executable = self.settings.dsh_executable.clone();
        tracing::info!("starting the shared dsh server");
        self.net.send(NetCommand::StartServer {
            ws_key: DSH_SERVER_KEY.into(),
            kind: HarnessKind::Dsh,
            executable: if executable.is_empty() {
                crate::settings::DEFAULT_DSH_EXECUTABLE.into()
            } else {
                executable
            },
            folder: ".".into(),
        });
        let _ = cx;
    }

    /// Refreshes every workspace's dsh chat list from the shared server. Each
    /// request filters by the workspace folder server-side (dsh sessions
    /// carry their own cwd).
    fn refresh_dsh_lists(&mut self) {
        if !self.runtimes.contains_key(DSH_SERVER_KEY) {
            return;
        }
        let folders: Vec<String> = self
            .workspaces
            .iter()
            .filter(|ws| {
                ws.state
                    .sessions
                    .iter()
                    .any(|sid| self.session_harness(sid) == HarnessKind::Dsh)
            })
            .map(|ws| ws.folder_path().to_string())
            .collect();
        for folder in folders {
            self.net.send(NetCommand::Request {
                ws_key: DSH_SERVER_KEY.into(),
                request: ApiRequest::ListDshSessions { folder },
            });
        }
    }

    /// Restarts whichever server a net key names — a workspace's opencode
    /// process or the shared dsh singleton.
    fn start_server_for_key(&mut self, key: &str, cx: &mut Context<Self>) {
        if key == DSH_SERVER_KEY {
            self.ensure_dsh(cx);
        } else {
            self.start_workspace(key, cx);
        }
    }

    fn touch_server_activity(&mut self, key: &str) {
        self.server_activity.insert(key.to_string(), Instant::now());
    }

    /// Stops opencode workspace servers whose last chat activity is older
    /// than the configured idle minutes — each serve owns a port, and an
    /// expanded workspace used to leave it running forever. The shared dsh
    /// process is exempt (one singleton, started on demand).
    fn sweep_idle_servers(&mut self, cx: &mut Context<Self>) {
        let idle_min = self.settings.server_idle_stop_min;
        if idle_min <= 0 {
            return;
        }
        // in-flight creates park work outside the runtimes; wait for them
        if !self.pending_sends.is_empty()
            || !self.pending_create_harness.is_empty()
            || !self.pending_dsh_drafts.is_empty()
        {
            return;
        }
        let now = Instant::now();
        let keys: Vec<String> = self
            .runtimes
            .keys()
            .filter(|key| key.as_str() != DSH_SERVER_KEY)
            .cloned()
            .collect();
        for key in keys {
            let busy = self
                .workspaces
                .iter()
                .find(|ws| ws.folder_path() == key)
                .map(|ws| {
                    ws.state
                        .sessions
                        .iter()
                        .any(|sid| self.running_by_session.contains(sid))
                })
                .unwrap_or(false)
                || self
                    .pending_opens
                    .iter()
                    .any(|(open_key, _)| open_key == &key)
                || self
                    .pending_restores
                    .iter()
                    .any(|(open_key, _)| open_key == &key);
            if busy {
                self.touch_server_activity(&key);
                continue;
            }
            let Some(last) = self.server_activity.get(&key) else {
                continue; // never touched: not eligible this sweep
            };
            if now.duration_since(*last).as_secs() >= idle_min as u64 * 60 {
                self.idle_stop_workspace(&key, cx);
            }
        }
    }

    /// The idle counterpart of a manual stop: the runtime and its port go
    /// away, but the sidebar chat list stays for the next expansion (nothing
    /// is running, so no queue/pending teardown is needed).
    fn idle_stop_workspace(&mut self, path: &str, cx: &mut Context<Self>) {
        let name = self
            .workspaces
            .iter()
            .find(|ws| ws.folder_path() == path)
            .map(|ws| ws.name.clone())
            .unwrap_or_else(|| path.to_string());
        tracing::info!(
            "stopping the {name} server ({} min idle)",
            self.settings.server_idle_stop_min
        );
        self.net.send(NetCommand::StopServer {
            ws_key: path.into(),
        });
        self.runtimes.remove(path);
        self.crash_restarts.remove(path);
        self.reconnect_notes.remove(path);
        self.server_activity.remove(path);
        if let Some(ws) = self.workspace_by_path_mut(path) {
            ws.is_running = false;
        }
        if self.selected_workspace.as_deref() == Some(path) {
            self.is_server_running = false;
        }
        self.ipc_push_status();
        self.repaint(cx);
    }

    /// Flows the sends parked while a chat's server was restarting.
    fn drain_pending_session_sends(&mut self, key: &str, cx: &mut Context<Self>) {
        let due: Vec<(String, String, String, Vec<AttachmentItem>)> = self
            .pending_session_sends
            .extract_if(.., |(net_key, ..)| net_key == key)
            .collect();
        for (_, session_id, text, attachments) in due {
            self.send_core(&session_id, text, attachments, cx);
        }
    }

    /// ServerStarted for the shared dsh server: pickers fill from it, parked
    /// dsh work starts flowing, every workspace's dsh chat list refreshes.
    fn on_dsh_started(&mut self, cx: &mut Context<Self>) {
        self.touch_server_activity(DSH_SERVER_KEY);
        self.runtimes.insert(
            DSH_SERVER_KEY.into(),
            WorkspaceRuntime {
                stream_status: oc::StreamStatus::Connected,
                healthy: None,
            },
        );
        self.net.send(NetCommand::Request {
            ws_key: DSH_SERVER_KEY.into(),
            request: ApiRequest::ListModels,
        });
        self.net.send(NetCommand::Request {
            ws_key: DSH_SERVER_KEY.into(),
            request: ApiRequest::ListAgents,
        });
        self.refresh_dsh_lists();
        // drafts parked while the server spun up become real creates now
        let parked: Vec<(String, String, Vec<AttachmentItem>, String)> =
            std::mem::take(&mut self.pending_dsh_drafts);
        for (tile_key, text, attachments, folder) in parked {
            self.pending_sends.push((tile_key, text, attachments));
            self.pending_create_harness
                .push_back((folder.clone(), HarnessKind::Dsh));
            self.net.send(NetCommand::Request {
                ws_key: DSH_SERVER_KEY.into(),
                request: ApiRequest::CreateDshSession {
                    folder,
                    title: DRAFT_TITLE.into(),
                },
            });
        }
        // sends parked while the shared server's port was closed
        self.drain_pending_session_sends(DSH_SERVER_KEY, cx);
        let pending_restores: Vec<(String, Vec<String>)> = self
            .pending_restores
            .extract_if(.., |(path, _)| path == DSH_SERVER_KEY)
            .collect();
        for (_, wanted) in pending_restores {
            self.restore_open_tiles(DSH_SERVER_KEY, wanted, cx);
        }
        let pending_opens: Vec<(String, String)> = self
            .pending_opens
            .extract_if(.., |(path, _)| path == DSH_SERVER_KEY)
            .collect();
        for (_, session_id) in pending_opens {
            self.net.send(NetCommand::Request {
                ws_key: DSH_SERVER_KEY.into(),
                request: ApiRequest::GetMessages { session_id },
            });
        }
        self.ipc_push_status();
        self.repaint(cx);
    }

    fn workspace_name_for(&self, session_id: &str) -> String {
        self.effective_workspace_path(session_id)
            .and_then(|path| {
                self.workspaces
                    .iter()
                    .find(|w| w.folder_path() == path)
                    .map(|w| w.name.clone())
            })
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
        if self
            .workspaces
            .iter()
            .any(|w| w.folder_path() == normalized)
        {
            self.announce("That folder is already a workspace.", false, cx);
            return;
        }
        let state = WorkspaceState {
            path: normalized.clone(),
            ..Default::default()
        };
        self.ui.workspaces.push(state.clone());
        let item = WorkspaceItem::new(state);
        let path_key = item.folder_path().to_string();
        self.workspaces.push(item);
        sort_workspaces(&mut self.ui.workspaces);
        self.workspaces
            .sort_by_key(|w| workspace_order_key(w.folder_path()));
        self.save_state();
        self.select_workspace(&path_key, cx);
        self.ipc_push_status();
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
            self.net.send(NetCommand::StopServer {
                ws_key: path.into(),
            });
        }
        self.drop_subagent_rows(path);
        let session_ids: Vec<String> = self.workspaces[index].state.sessions.clone();
        for sid in &session_ids {
            self.set_running(Some(sid), false);
        }
        self.drop_queue(&session_ids);
        self.drop_pending(&session_ids);
        self.drop_retries(&session_ids);
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
        self.ipc_push_status();
    }

    pub fn start_workspace(&mut self, path: &str, cx: &mut Context<Self>) {
        self.touch_server_activity(path);
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
            kind: HarnessKind::OpenCode,
            executable: if executable.is_empty() {
                DEFAULT_EXECUTABLE.into()
            } else {
                executable
            },
            folder: path.into(),
        });
        let _ = cx;
    }

    /// Drops the store-side state of a workspace's runtime; the net-side
    /// entry (process, pump, poll) dies via the StopServer command.
    fn teardown_runtime(&mut self, path: &str) {
        self.runtimes.remove(path);
        self.crash_restarts.remove(path);
        self.reconnect_notes.remove(path);
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
        self.drop_retries(&session_ids);
        if self.selected_workspace.as_deref() == Some(path) {
            self.is_server_running = false;
            self.sessions.clear();
        }
        self.save_state();
        self.ipc_push_status();
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
        // the shared dsh server lists per folder independent of the
        // workspace's opencode server; refresh this folder's dsh chats
        // whenever it is up
        if self.runtimes.contains_key(DSH_SERVER_KEY) {
            self.net.send(NetCommand::Request {
                ws_key: DSH_SERVER_KEY.into(),
                request: ApiRequest::ListDshSessions {
                    folder: path.to_string(),
                },
            });
        }
        if !running {
            return;
        }
        self.workspace_generation += 1;
        if let Some(ws) = self.selected_workspace() {
            let path = ws.folder_path().to_string();
            self.net.send(NetCommand::Request {
                ws_key: path.clone(),
                request: ApiRequest::ListSessions,
            });
            self.net.send(NetCommand::Request {
                ws_key: path.clone(),
                request: ApiRequest::ListModels,
            });
            self.net.send(NetCommand::Request {
                ws_key: path,
                request: ApiRequest::ListAgents,
            });
        }
        let _ = cx;
    }

    /// Expanding a workspace's chat list shows intent to work in it: connect its
    /// server so the chat list fills; an already-running workspace is left alone.
    /// dsh is one shared server: warm it when the workspace already holds dsh
    /// chats, refresh the folder's dsh list whenever it is up.
    pub fn connect_expanded_workspace(&mut self, path: &str, cx: &mut Context<Self>) {
        self.touch_server_activity(path);
        let running = self
            .workspaces
            .iter()
            .find(|w| w.folder_path() == path)
            .map(|w| self.runtimes.contains_key(w.folder_path()))
            .unwrap_or(false);
        if !running {
            self.start_workspace(path, cx);
        }
        if self.runtimes.contains_key(DSH_SERVER_KEY) {
            self.net.send(NetCommand::Request {
                ws_key: DSH_SERVER_KEY.into(),
                request: ApiRequest::ListDshSessions {
                    folder: path.to_string(),
                },
            });
        } else if self
            .workspaces
            .iter()
            .find(|w| w.folder_path() == path)
            .is_some_and(|w| {
                w.state
                    .sessions
                    .iter()
                    .any(|sid| self.session_harness(sid) == HarnessKind::Dsh)
            })
        {
            // the list (and every pending open) flows once ServerStarted lands
            self.ensure_dsh(cx);
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
                    self.ipc_fail_parked_runs(&ws_key, &format!("server failed to start: {err:#}"));
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
                    if let Some(ws) = self.workspace_by_path_mut(&ws_key) {
                        ws.state.sessions.insert(0, info.id.clone());
                    }
                    // only the selected workspace's sidebar mirror lives in
                    // self.sessions; other workspaces refill from their own
                    // session list (session.idle triggers that refresh)
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
                    self.announce(&format!("Delete failed: {err:#}"), true, cx);
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

    pub fn tile_for_session_mut(&mut self, session_id: &str) -> Option<&mut ChatTile> {
        let key = self.tile_by_session.get(session_id)?.clone();
        self.tile_by_key_mut(&key)
    }

    fn create_tile(&mut self, cx: &mut Context<Self>) -> &mut ChatTile {
        let key = format!("t{}", self.next_tile_key);
        self.next_tile_key += 1;
        let mut tile = ChatTile::new(
            key.clone(),
            self.seed_model(),
            self.seed_agent(),
            self.seed_variant(),
        );
        tile.harness = self.settings.default_harness;
        if tile.harness == HarnessKind::Dsh {
            // the seed names an opencode model ("provider/model" of the
            // workspace server); the dsh catalog has its own — start from the
            // server default and warm the shared server so the picker lists
            // the real catalog by the time it opens
            tile.selected_model = None;
            tile.selected_variant = None;
            self.ensure_dsh(cx);
        }
        tile.apply_variant_list(
            &self.variants_by_model,
            self.settings.default_variant_highest,
        );
        tile.rev = Some(cx.new(|_| ()));
        self.open_tiles.push(tile);
        self.tile_by_key_mut(&key).unwrap()
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

    fn detach_tile(&mut self, key: &str, cx: &mut Context<Self>) {
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
    fn prune_untouched_drafts(&mut self, cx: &mut Context<Self>) {
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

    fn show_queued_for(&mut self, tile_key: &str) {
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
                let folder = self.selected_workspace.clone().unwrap_or_default();
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
        } else if !self.is_server_running || self.selected_workspace.is_none() {
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
            let ws_key = self.selected_workspace.clone().unwrap();
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

    fn send_core(
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
    fn arm_auto_retry(&mut self, session_id: &str) -> Option<u32> {
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
    fn fire_auto_retry(&mut self, session_id: &str, cx: &mut Context<Self>) {
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

    // ---- session list maintenance ----

    fn apply_session_list(
        &mut self,
        ws_key: &str,
        sessions: Vec<SessionInfo>,
        is_dsh_list: bool,
        cx: &mut Context<Self>,
    ) {
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
                    .filter(|s| !known.contains(&s.id))
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

    fn sync_workspace_chats(&mut self, cx: &mut Context<Self>) {
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
            .map(|c| (c.draft_title.trim().to_string(), c.title.clone()));
        let Some((title, current)) = draft else {
            return;
        };
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
        let Some(ws_key) = self.net_key_for_session(session_id) else {
            return;
        };
        self.net.send(NetCommand::Request {
            ws_key,
            request: ApiRequest::DeleteSession {
                session_id: session_id.to_string(),
            },
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
        self.repaint(cx);
    }

    fn forget_session(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(tile_key) = self.tile_by_session.get(id).cloned() {
            self.detach_tile(&tile_key, cx);
        }
        self.turn_start_by_session.remove(id);
        self.user_stopped_by_session.remove(id);
        self.workspace_by_session.remove(id);
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
    fn restore_open_tiles(&mut self, ws_key: &str, wanted: Vec<String>, cx: &mut Context<Self>) {
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

    // ---- pending permission/question parking ----

    fn drop_pending(&mut self, session_ids: &[String]) {
        for sid in session_ids {
            if self.pending_by_session.remove(sid).is_some() {
                self.mark_attention(sid, false);
            }
            self.pending_questions_by_session.remove(sid);
        }
    }

    /// Drops the auto-retry bookkeeping for sessions whose workspace went away.
    fn drop_retries(&mut self, session_ids: &[String]) {
        for sid in session_ids {
            self.last_prompt_by_session.remove(sid);
            self.retry_due_by_session.remove(sid);
        }
    }

    fn flush_pending_permissions(&mut self, session_id: &str, cx: &mut Context<Self>) {
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

    fn flush_pending_questions(&mut self, session_id: &str, cx: &mut Context<Self>) {
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

    fn pull_pending_questions(&mut self, ws_key: &str, session_id: &str) {
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
        let existing_index = self
            .running_agents
            .iter()
            .position(|r| r.session_id == session_id);
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
    fn track_session_info(
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

    // ---- stream events (HandleEvent port) ----

    fn handle_stream_event(
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
                    // refresh the chat list of the workspace the chat belongs to
                    if self.session_harness(sid) == HarnessKind::Dsh {
                        self.touch_server_activity(DSH_SERVER_KEY);
                        self.refresh_dsh_lists();
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
                    for run in watched {
                        match attempt {
                            Some(attempt) => {
                                self.ipc_run_log(run, &format!("connection lost — retrying (attempt {attempt}/{MAX_AUTO_RETRY})"));
                            }
                            None => {
                                if let Some(entry) = self.ipc_runs.remove(&run) {
                                    let _ = entry.reply.send(Err(message.clone()));
                                }
                            }
                        }
                    }
                    match attempt {
                        Some(attempt) => self.post_note_for_session(
                            sid,
                            &format!("Connection lost — re-sending the last prompt (attempt {attempt}/{MAX_AUTO_RETRY})…"),
                            false,
                        ),
                        None => self.post_note_for_session(sid, &message, true),
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

    fn resolve_permission(&mut self, permission_id: &str, label: String, is_auto: bool) {
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

    fn on_permission_replied(&mut self, data: &serde_json::Value) {
        let Some(request_id) = data.get("requestID").and_then(serde_json::Value::as_str) else {
            return;
        };
        self.resolve_permission(request_id, "answered elsewhere".into(), false);
    }

    // ---- question tool asks ----
    // The agent's `question` tool parks the turn until the user answers; never auto-answered.

    fn on_question_asked(&mut self, data: &serde_json::Value, cx: &mut Context<Self>) {
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

    fn on_question_resolved(&mut self, data: &serde_json::Value) {
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

    fn resolve_question(&mut self, request_id: &str, summary: &str) {
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

    /// Same note, but only into a tile that actually talks to `ws_key` — a
    /// server's connect/status notes must not surface in a chat that runs on
    /// another server (a dsh draft must not greet with opencode's note).
    fn announce_for_key(&mut self, ws_key: &str, text: &str, cx: &mut Context<Self>) {
        let matches_key =
            |this: &Self, key: &str| this.tile_net_key_or(key).as_deref() == Some(ws_key);
        let target = self
            .focused_tile
            .clone()
            .filter(|key| matches_key(self, key))
            .or_else(|| {
                self.open_tiles
                    .iter()
                    .map(|t| t.key.clone())
                    .find(|key| matches_key(self, key))
            });
        if let Some(target) = target {
            self.tile_note(&target, text, false, cx);
        }
    }

    pub fn tile_note(&mut self, tile_key: &str, text: &str, error: bool, cx: &mut Context<Self>) {
        let note_key = self.next_key;
        self.next_key += 1;
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            tile.items.push(Arc::new(ChatItem::SystemNote {
                key: note_key,
                text: text.into(),
                is_error: error,
            }));
        }
        self.repaint(cx);
    }

    /// Create or evolve the workspace's single "connection lost" note. A drop
    /// within oc::RECONNECT_RESET_AFTER of the previous event rewrites that
    /// note in place (its attempt count climbs); an older drop starts a new
    /// one, so a stable stretch closes the series.
    fn reconnect_note(&mut self, ws_key: &str, text: &str, error: bool, cx: &mut Context<Self>) {
        let now = Instant::now();
        let expired = match self.reconnect_notes.get(ws_key) {
            Some((_, _, last)) => now.duration_since(*last) >= oc::RECONNECT_RESET_AFTER,
            None => true,
        };
        if expired {
            let Some(tile_key) = self
                .focused_tile
                .clone()
                .or_else(|| self.open_tiles.first().map(|t| t.key.clone()))
            else {
                return;
            };
            let note_key = self.next_key;
            self.next_key += 1;
            if let Some(tile) = self.tile_by_key_mut(&tile_key) {
                tile.items.push(Arc::new(ChatItem::SystemNote {
                    key: note_key,
                    text: text.into(),
                    is_error: error,
                }));
            }
            self.reconnect_notes
                .insert(ws_key.to_string(), (tile_key.clone(), note_key, now));
            self.touch_tile(&tile_key, cx);
        } else {
            let Some((tile_key, note_key, last)) = self.reconnect_notes.get_mut(ws_key) else {
                return;
            };
            *last = now;
            let (tile_key, note_key) = (tile_key.clone(), *note_key);
            self.rewrite_note(&tile_key, note_key, text, error);
            self.touch_tile(&tile_key, cx);
        }
    }

    /// Flip the workspace's live reconnect note to its settled "reconnected"
    /// text. Only a series with a prior "connection lost" answers: the pump
    /// reports Connected on every attach, including the quiet first one.
    fn settle_reconnect_note(&mut self, ws_key: &str, cx: &mut Context<Self>) {
        let Some((tile_key, note_key)) = self
            .reconnect_notes
            .get(ws_key)
            .map(|(t, n, _)| (t.clone(), *n))
        else {
            return;
        };
        self.reconnect_notes.get_mut(ws_key).unwrap().2 = Instant::now();
        self.rewrite_note(&tile_key, note_key, "Event stream reconnected.", false);
        self.touch_tile(&tile_key, cx);
    }

    /// Rewrite one SystemNote's text/severity inside its tile's item list.
    fn rewrite_note(&mut self, tile_key: &str, note_key: usize, text: &str, error: bool) {
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            for item in &mut tile.items {
                if matches!(item.as_ref(), ChatItem::SystemNote { key, .. } if *key == note_key) {
                    if let ChatItem::SystemNote {
                        text: t,
                        is_error: e,
                        ..
                    } = Arc::make_mut(item)
                    {
                        *t = text.into();
                        *e = error;
                    }
                    break;
                }
            }
        }
    }

    fn post_note_for_session(&mut self, session_id: &str, text: &str, error: bool) {
        let note_key = self.next_key;
        self.next_key += 1;
        if let Some(tile_key) = self.tile_by_session.get(session_id).cloned()
            && let Some(tile) = self.tile_by_key_mut(&tile_key)
        {
            tile.items.push(Arc::new(ChatItem::SystemNote {
                key: note_key,
                text: text.into(),
                is_error: error,
            }));
            return;
        }
        let title = self.title_for(session_id);
        let target = self
            .focused_tile
            .clone()
            .or_else(|| self.open_tiles.first().map(|t| t.key.clone()));
        if let Some(target) = target
            && let Some(tile) = self.tile_by_key_mut(&target)
        {
            tile.items.push(Arc::new(ChatItem::SystemNote {
                key: note_key,
                text: format!("{title}: {text}"),
                is_error: error,
            }));
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
            tile.attachments.push(AttachmentItem {
                file_name,
                mime,
                data: Arc::new(data),
                key: akey,
            });
        }
    }

    /// Files dragged onto the composer from the file manager; mirrors the
    /// picker path (directories skipped, over-512 KB announced and skipped).
    pub fn attach_paths(
        &mut self,
        tile_key: &str,
        paths: &[std::path::PathBuf],
        cx: &mut Context<Self>,
    ) {
        for path in paths {
            if path.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string());
            let Ok(data) = std::fs::read(path) else {
                self.tile_note(
                    tile_key,
                    &format!("{name} could not be read — skipped."),
                    true,
                    cx,
                );
                continue;
            };
            if data.len() > 512 * 1024 {
                self.tile_note(
                    tile_key,
                    &format!("{name} is over 512 KB — skipped."),
                    true,
                    cx,
                );
                continue;
            }
            let mime = crate::ui::mime_for_path(&name);
            self.attach_file(tile_key, name, mime.into(), data);
        }
        self.repaint(cx);
    }

    pub fn remove_attachment(
        &mut self,
        tile_key: &str,
        attachment_key: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(tile) = self.tile_by_key_mut(tile_key) {
            tile.attachments.retain(|a| a.key != attachment_key);
        }
        self.repaint(cx);
    }

    // ---- composer pickers ----

    pub fn set_tile_model(&mut self, tile_key: &str, model: Option<String>) {
        let variants_by_model = self.variants_by_model.clone();
        let highest = if self.settings.default_variant_highest {
            model
                .as_ref()
                .and_then(|m| variants_by_model.get(m))
                .and_then(|vs| highest_variant(vs))
        } else {
            None
        };
        let Some(tile) = self.tile_by_key_mut(tile_key) else {
            return;
        };
        tile.selected_model = model;
        tile.variants = variant_list_for(&variants_by_model, &tile.selected_model);
        tile.selected_variant = highest.or_else(|| Some(DEFAULT_VARIANT_ENTRY.into()));
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

    pub fn set_sidebar_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.sidebar_visible == visible {
            return;
        }
        self.sidebar_visible = visible;
        self.ui.sidebar_visible = Some(visible);
        self.ui.save();
        self.repaint(cx);
    }

    /// Persists the dragged sidebar width after a resize gesture ends.
    pub fn commit_sidebar_width(&mut self) {
        self.ui.sidebar_width = Some(self.sidebar_shown_width as f64);
        self.ui.save();
    }

    /// The text size or the desktop text scaling moved: re-reads the ladder
    /// and re-maps the kit, which sizes its own text in rem off the theme's
    /// font_size.
    pub fn rescale_fonts(&mut self, cx: &mut Context<Self>) {
        self.fonts = theme::active_tokens();
        theme::apply_kit_theme(self.accent, &self.fonts, cx);
        crate::ui::tooltip::sync_settings(cx, self.tool_tips_visible, self.fonts.font_meta);
        cx.notify();
    }

    /// Settings page applied: pushes persisted values back into live state.
    pub fn apply_saved_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(allowed) = self.settings.auto_allow_always {
            self.auto_allow = allowed;
        }
        self.ui.auto_allow_always = Some(self.auto_allow);
        self.accent =
            AccentColors::for_preset(theme::normalize_accent(self.settings.accent.as_deref()));
        // re-map the kit theme so input selection/ring/primary follow the preset
        // (Theme::update refreshes every window)
        theme::apply_kit_theme(self.accent, &self.fonts, cx);
        self.tool_tips_visible = self.settings.show_tool_tips;
        warpui_kit_core::motion::set_reduced(
            self.settings.reduce_motion || warpui_kit_core::motion::system_prefers_reduced(),
        );
        crate::ui::tooltip::sync_settings(cx, self.tool_tips_visible, self.fonts.font_meta);
        // push the MCP master switch into running servers right away: disable
        // severs every configured server, re-enabling reconnects all but the
        // ones the user individually toggled off (best-effort, ignores 404)
        for (ws_key, names) in &self.mcp_names {
            if !self.runtimes.contains_key(ws_key) {
                continue;
            }
            let disabled = self.settings.mcp_disabled || self.mcp_disabled.contains(ws_key);
            for name in names.clone() {
                self.net.send(NetCommand::Request {
                    ws_key: ws_key.clone(),
                    request: if disabled {
                        ApiRequest::McpDisconnect { name }
                    } else {
                        ApiRequest::McpConnect { name }
                    },
                });
            }
        }
        self.settings.save();
        self.save_state();
        self.repaint(cx);
    }
}

// extra helpers: the placeholder pattern lets tile methods borrow &Store while
// the tile itself is checked out of the vec
impl Store {
    fn placeholder_tile() -> ChatTile {
        ChatTile::new("::placeholder".into(), None, None, None)
    }

    /// Checks a tile out of the grid, runs `f` with shared store access, then puts it back.
    /// The checkout dummy is recycled across calls: message/part events hit
    /// this at token rate.
    pub fn with_tile<R>(
        &mut self,
        key: &str,
        f: impl FnOnce(&mut ChatTile, &Store, &mut usize) -> R,
    ) -> Option<R> {
        let index = self.open_tiles.iter().position(|t| t.key == key)?;
        let mut tile = std::mem::replace(
            &mut self.open_tiles[index],
            self.tile_placeholder
                .take()
                .unwrap_or_else(Self::placeholder_tile),
        );
        let mut next_key = self.next_key;
        let result = f(&mut tile, self, &mut next_key);
        self.next_key = next_key;
        self.tile_placeholder = Some(std::mem::replace(&mut self.open_tiles[index], tile));
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

/// (message, retryable) from a session.error payload. `retryable` mirrors the
/// AI SDK's own flag on the error — true for connection failures, false for
/// real API errors (auth, quota, bad request) which must not be re-sent.
fn read_error_info(data: &serde_json::Value) -> (String, bool) {
    let retryable = data
        .pointer("/error/data/isRetryable")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    (read_error(data), retryable)
}

fn read_error(data: &serde_json::Value) -> String {
    if let Some(error) = data.get("error").filter(|e| e.is_object()) {
        let name = error
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if name.to_lowercase().contains("abort") {
            return "Turn stopped.".into();
        }
        // API errors carry the human message nested in data; everything else is transport noise
        let mut message = String::new();
        if let Some(inner) = error
            .pointer("/data/message")
            .and_then(serde_json::Value::as_str)
            && !inner.is_empty()
        {
            message = inner.to_string();
        }
        if message.is_empty()
            && let Some(outer) = error.get("message").and_then(serde_json::Value::as_str)
            && !outer.is_empty()
        {
            message = outer.to_string();
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
                    .map(|o| QuestionOptionItem {
                        label: o.label,
                        description: o.description,
                        is_selected: false,
                    })
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
    let full = std::fs::canonicalize(trimmed)
        .map(|full| {
            crate::services::plain_path(&full)
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|_| trimmed.to_string());
    #[cfg(windows)]
    let full = full.trim_end_matches(|c| c == '/' || c == '\\').to_string();
    #[cfg(not(windows))]
    let full = full.trim_end_matches('/').to_string();
    // a bare drive ("C:") points at the drive's forgotten current directory;
    // keep its root form
    if cfg!(windows) && full.len() == 2 && full.as_bytes()[1] == b':' {
        format!("{full}\\")
    } else {
        full
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

#[cfg(test)]
mod suggest_tests {
    use super::{at_token_start, parse_slash_command, read_error_info, session_chat_settings};
    use crate::oc::{CommandInfo, StoredMessage};
    use serde_json::json;

    fn cmd(name: &str) -> CommandInfo {
        CommandInfo {
            name_lower: name.to_lowercase(),
            name: name.into(),
            description: "".into(),
            source: "command".into(),
        }
    }

    fn stored(id: &str, role: &str, info: serde_json::Value) -> StoredMessage {
        StoredMessage {
            id: id.into(),
            role: role.into(),
            created_ms: 0,
            info,
            parts: Vec::new(),
        }
    }

    #[test]
    fn at_token_positions() {
        assert_eq!(
            at_token_start("hello @src/ma"),
            Some(("hello ".len(), "src/ma".into()))
        );
        assert_eq!(at_token_start("@"), Some((0, "".into())));
        assert_eq!(at_token_start("mail me a@b.com"), None);
        assert_eq!(at_token_start("done @a b"), None);
        assert_eq!(at_token_start("plain"), None);
    }

    #[test]
    fn slash_parsing() {
        let commands = vec![cmd("init"), cmd("review")];
        assert_eq!(
            parse_slash_command("/init", &commands),
            Some(("init".into(), "".into()))
        );
        // args ride along, whitespace-normalized
        assert_eq!(
            parse_slash_command("/init fast", &commands),
            Some(("init".into(), "fast".into()))
        );
        assert_eq!(
            parse_slash_command("/review  the   diff ", &commands),
            Some(("review".into(), "the   diff".into()))
        );
        // unknown command and non-slash text are plain prompts
        assert_eq!(parse_slash_command("/nope", &commands), None);
        assert_eq!(parse_slash_command("plain text", &commands), None);
    }

    #[test]
    fn session_settings_come_from_last_assistant() {
        // the last assistant turn wins, even with later user messages
        let messages = vec![
            stored(
                "m1",
                "assistant",
                json!({"providerID": "a", "modelID": "old", "variant": "low"}),
            ),
            stored("m2", "user", json!({})),
            stored(
                "m3",
                "assistant",
                json!({"providerID": "a", "modelID": "new", "variant": "max"}),
            ),
            stored("m4", "user", json!({})),
        ];
        assert_eq!(
            session_chat_settings(&messages),
            Some(("a/new".into(), Some("max".into())))
        );
        // a model without a variant pick reads as the "(default)" sentinel case
        let messages = vec![stored(
            "m1",
            "assistant",
            json!({"providerID": "a", "modelID": "new"}),
        )];
        assert_eq!(
            session_chat_settings(&messages),
            Some(("a/new".into(), None))
        );
        // no assistant reply yet → keep the global seeds
        let messages = vec![stored("m1", "user", json!({}))];
        assert_eq!(session_chat_settings(&messages), None);
        assert_eq!(session_chat_settings(&[]), None);
        // malformed model fields fall back to the seeds too
        let messages = vec![stored(
            "m1",
            "assistant",
            json!({"providerID": "", "modelID": "new"}),
        )];
        assert_eq!(session_chat_settings(&messages), None);
    }

    #[test]
    fn error_reader_reports_retryable() {
        // real v1.19.4 shape: connection failures carry isRetryable in error.data
        let connection = json!({
            "sessionID": "s1",
            "error": {
                "name": "APIError",
                "data": {
                    "message": "Cannot connect to API: ECONNREFUSED: Unable to connect.",
                    "isRetryable": true,
                    "metadata": {"url": "http://127.0.0.1:1/v1/chat/completions"}
                }
            }
        });
        assert_eq!(
            read_error_info(&connection),
            (
                "Error: Cannot connect to API: ECONNREFUSED: Unable to connect.".into(),
                true
            )
        );
        // hard errors never retry, with or without the flag
        let quota = json!({"error": {"name": "APIError", "data": {"message": "quota exceeded", "isRetryable": false}}});
        assert!(!read_error_info(&quota).1);
        let auth = json!({"error": {"name": "ProviderAuthError", "message": "invalid api key"}});
        assert!(!read_error_info(&auth).1);
        assert!(!read_error_info(&json!({})).1);
    }
}
