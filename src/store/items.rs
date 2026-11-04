use super::*;

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
    pub(super) state_sig: Option<u64>,
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
pub(super) struct PromptPayload {
    pub(super) text: String,
    pub(super) attachments: Vec<AttachmentItem>,
    pub(super) model: Option<String>,
    pub(super) variant: Option<String>,
    pub(super) agent: Option<String>,
    pub(super) command: Option<(String, String)>,
    /// automatic resends already spent on this prompt
    pub(super) attempts: u32,
}

/// Automatic resends allowed per prompt before the error note stands as-is.
pub(super) const MAX_AUTO_RETRY: u32 = 3;

/// Automatic restarts allowed when a workspace's serve process crashes,
/// before the app gives up and leaves the workspace stopped.
pub(super) const MAX_CRASH_RESTARTS: u32 = 3;
/// A server that stayed up this long earns a fresh crash ladder — otherwise
/// a spawn-that-then-crashes cycle would reset its count on every start.
pub(super) const CRASH_RESTART_RESET_AFTER: Duration = Duration::from_secs(600);

/// Pause before auto-retry attempt n; the server already spent its own
/// backoff ladder (tens of seconds) before the error reached us.
pub(super) fn retry_backoff(attempts: u32) -> Duration {
    Duration::from_secs(5 * attempts.max(1) as u64)
}
