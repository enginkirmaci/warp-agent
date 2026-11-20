//! Shared vocabulary for the harness interface.
//!
//! These are the app-level types the store, the UI and the door will speak once
//! the adapters land. The old opencode and dsh modules keep their own types
//! during the expand phase; the contract ticket replaces them.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Stable wire id of a harness (`opencode`, `opencode-v2`, `dsh`).
///
/// The registry, not a fixed enum, is the list of ids that exist; a string
/// keeps a descriptor entry the only thing a new harness needs.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HarnessId(String);

impl HarnessId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for HarnessId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for HarnessId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for HarnessId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// A typed failure from a harness operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HarnessError {
    /// The id is not in the registry.
    UnknownHarness(String),
    /// A descriptor with this id is already registered.
    DuplicateHarness(HarnessId),
    /// The host already owns an instance with this id.
    DuplicateInstance(HarnessId),
    /// The adapter does not implement this operation.
    Unsupported(&'static str),
    /// The adapter exists but cannot be reached right now.
    Unavailable(String),
    /// The harness is registered but disabled in settings.
    Disabled(HarnessId),
    /// Anything else the adapter wants to report.
    Failed(String),
}

impl fmt::Display for HarnessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownHarness(id) => write!(f, "unknown harness: {id}"),
            Self::DuplicateHarness(id) => write!(f, "duplicate harness: {id}"),
            Self::DuplicateInstance(id) => write!(f, "duplicate harness instance: {id}"),
            Self::Unsupported(operation) => {
                write!(f, "unsupported harness operation: {operation}")
            }
            Self::Unavailable(reason) => write!(f, "harness unavailable: {reason}"),
            Self::Disabled(id) => write!(f, "harness disabled: {id}"),
            Self::Failed(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for HarnessError {}

/// How the app reaches a harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionKind {
    /// The app spawns and owns the process (opencode v1, dsh).
    Managed,
    /// The app attaches to a service someone else runs (opencode v2).
    Attached,
}

/// How many instances a harness has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceScope {
    /// One instance for the whole app (dsh, opencode v2).
    App,
    /// One instance per workspace folder (opencode v1).
    Workspace,
}

/// What an adapter can do, as data.
///
/// The UI and the door render or enable from this descriptor; it is not a user
/// switch, and an adapter that lacks a feature returns [`HarnessError::Unsupported`]
/// from the corresponding method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HarnessCapabilities {
    pub commands: bool,
    pub skills: bool,
    pub skill_activation: bool,
    /// The harness asks multi-field structured forms (opencode v2).
    pub forms: bool,
    /// The harness asks single-choice or free-text questions (opencode v1 and
    /// dsh); they travel as a one-field form and are answered through the form
    /// reply operation.
    pub questions: bool,
    pub permissions: bool,
    pub diffs: bool,
    pub subagents: bool,
    pub inbox: bool,
    pub revert: bool,
    pub shell: bool,
    pub background_sessions: bool,
    pub mcp_read: bool,
    /// The add/connect/disconnect/remove surface. opencode v1's existing
    /// per-workspace MCP toggle stays an app-level control until the contract
    /// ticket.
    pub mcp_management: bool,
    pub compaction: bool,
    pub fork: bool,
    pub multi_location: bool,
}

impl HarnessCapabilities {
    pub const NONE: Self = Self {
        commands: false,
        skills: false,
        skill_activation: false,
        forms: false,
        questions: false,
        permissions: false,
        diffs: false,
        subagents: false,
        inbox: false,
        revert: false,
        shell: false,
        background_sessions: false,
        mcp_read: false,
        mcp_management: false,
        compaction: false,
        fork: false,
        multi_location: false,
    };

    /// opencode v1 keeps the app's existing feature set.
    pub const OPENCODE_V1: Self = Self {
        commands: true,
        skills: true,
        skill_activation: false,
        forms: false,
        questions: true,
        permissions: true,
        diffs: true,
        subagents: true,
        inbox: false,
        revert: false,
        shell: false,
        background_sessions: false,
        mcp_read: true,
        mcp_management: false,
        compaction: true,
        fork: true,
        multi_location: false,
    };

    /// opencode v2 adds forms, the shared-service lifecycle and its own surfaces.
    pub const OPENCODE_V2: Self = Self {
        commands: true,
        skills: true,
        skill_activation: true,
        forms: true,
        questions: false,
        permissions: true,
        diffs: true,
        subagents: true,
        inbox: true,
        revert: true,
        shell: true,
        background_sessions: true,
        mcp_read: true,
        mcp_management: true,
        compaction: true,
        fork: true,
        multi_location: true,
    };

    /// dsh keeps the app's existing feature set, minus opencode-only surfaces.
    pub const DSH: Self = Self {
        commands: true,
        skills: true,
        skill_activation: false,
        forms: false,
        questions: true,
        permissions: true,
        diffs: true,
        subagents: true,
        inbox: false,
        revert: false,
        shell: false,
        background_sessions: false,
        mcp_read: false,
        mcp_management: false,
        compaction: true,
        fork: true,
        multi_location: false,
    };
}

/// Per-instance construction input: the configured executable and, for a
/// per-workspace harness, the folder it serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessConfig {
    pub executable: String,
    /// The folder this instance serves, when the harness is per-workspace.
    pub directory: Option<PathBuf>,
}

impl HarnessConfig {
    pub fn new(executable: impl Into<String>) -> Self {
        Self {
            executable: executable.into(),
            directory: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthState {
    Ready,
    Starting,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessHealth {
    pub state: HealthState,
    pub detail: Option<String>,
}

impl HarnessHealth {
    pub fn ready() -> Self {
        Self {
            state: HealthState::Ready,
            detail: None,
        }
    }

    pub fn starting(detail: impl Into<String>) -> Self {
        Self {
            state: HealthState::Starting,
            detail: Some(detail.into()),
        }
    }

    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self {
            state: HealthState::Unavailable,
            detail: Some(detail.into()),
        }
    }
}

/// The running state of one session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    Busy,
    Retry,
}

/// The author of one message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageRole {
    User,
    Assistant,
}

impl MessageRole {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// Token totals one message reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: i64,
    pub output: i64,
    pub reasoning: i64,
    pub cache_read: i64,
    pub cache_write: i64,
}

/// A full message snapshot; adapters send the whole message, not a delta.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageSnapshot {
    pub id: String,
    pub session: SessionId,
    pub role: MessageRole,
    pub created_ms: i64,
    pub completed_ms: Option<i64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub error: Option<String>,
    pub tokens: Option<TokenUsage>,
    /// Cost in millionths of a currency unit; `None` when unknown.
    pub cost_micros: Option<i64>,
}

/// One tool call's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolState {
    Pending,
    Running,
    Completed { output: String },
    Failed { message: String },
}

/// One tool call carried by a part snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSnapshot {
    pub name: String,
    pub state: ToolState,
    pub input: Option<serde_json::Value>,
    pub title: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// One file/image part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSnapshot {
    pub url: String,
    pub filename: String,
}

/// A part's payload; the variant says which fields exist.
#[derive(Clone, Debug, PartialEq)]
pub enum PartPayload {
    Text(String),
    Reasoning(String),
    Tool(ToolSnapshot),
    StepStart,
    StepFinish,
    File(FileSnapshot),
}

impl PartPayload {
    pub fn as_wire(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Reasoning(_) => "reasoning",
            Self::Tool(_) => "tool",
            Self::StepStart => "step-start",
            Self::StepFinish => "step-finish",
            Self::File(_) => "file",
        }
    }
}

/// A full part snapshot; the v2 adapter accumulates deltas into this shape.
#[derive(Clone, Debug, PartialEq)]
pub struct PartSnapshot {
    pub id: String,
    pub session: SessionId,
    pub message_id: String,
    pub payload: PartPayload,
}

/// What a harness instance reports on its event stream.
#[derive(Clone, Debug, PartialEq)]
pub enum HarnessEvent {
    Connected,
    Disconnected {
        reason: Option<String>,
    },
    Health(HarnessHealth),
    /// A session was created; the parent establishes subagent lineage.
    SessionCreated {
        session: SessionId,
        title: Option<String>,
        parent: Option<SessionId>,
        agent: Option<String>,
    },
    /// A session's title or agent moved.
    SessionChanged {
        session: SessionId,
        title: Option<String>,
        /// The parent when the update carries it; `None` leaves the known
        /// lineage in place.
        parent: Option<SessionId>,
        agent: Option<String>,
    },
    /// A session was deleted.
    SessionDeleted {
        session: SessionId,
    },
    /// The session started, retried or went idle.
    SessionStatus {
        session: SessionId,
        state: SessionState,
    },
    /// A full message snapshot.
    MessageSnapshot {
        message: MessageSnapshot,
    },
    /// A full part snapshot (text, reasoning, tool, step or file).
    PartSnapshot {
        part: PartSnapshot,
    },
    /// A part disappeared from a message.
    PartRemoved {
        session: SessionId,
        part_id: String,
    },
    /// An approval request.
    PermissionAsked {
        request: PermissionRequest,
    },
    /// An approval was answered, possibly elsewhere.
    PermissionReplied {
        session: Option<SessionId>,
        request: String,
        decision: PermissionDecision,
    },
    /// A structured question or form.
    QuestionAsked {
        request: FormRequest,
    },
    /// A question or form was answered, possibly elsewhere.
    QuestionResolved {
        session: Option<SessionId>,
        request: String,
        summary: Option<String>,
    },
    /// The model catalog landed.
    ModelsLoaded {
        models: Vec<Model>,
    },
    /// The agent catalog landed.
    AgentsLoaded {
        agents: Vec<Agent>,
    },
    /// The command/skill catalog landed.
    CommandsLoaded {
        /// The workspace the catalog belongs to, for a per-workspace harness.
        directory: Option<PathBuf>,
        commands: Vec<Command>,
    },
    /// The skill catalog landed.
    SkillsLoaded {
        /// The workspace the catalog belongs to, for a per-workspace harness.
        directory: Option<PathBuf>,
        skills: Vec<Skill>,
    },
    /// A structured error; `session` is None for a server-level failure.
    Error {
        session: Option<SessionId>,
        message: String,
        retryable: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Model {
    pub key: String,
    pub provider: String,
    pub variants: Vec<String>,
    pub context_limit: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    pub name: String,
    pub mode: String,
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Stable session id inside one harness.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for SessionId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<String> for SessionId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: SessionId,
    pub title: String,
    pub parent: Option<SessionId>,
    pub directory: Option<PathBuf>,
    pub updated_ms: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelChoice {
    pub model: Option<String>,
    pub agent: Option<String>,
    pub variant: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewSession {
    pub title: Option<String>,
    pub parent: Option<SessionId>,
    pub directory: Option<PathBuf>,
    pub choice: ModelChoice,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prompt {
    pub text: String,
    pub choice: ModelChoice,
    pub background: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRun {
    pub name: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    Once,
    Always,
    Reject,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionRequest {
    pub id: String,
    pub session: SessionId,
    pub action: String,
    pub resources: Vec<String>,
    pub message: Option<String>,
    pub diff: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormFieldKind {
    Text,
    Number,
    Integer,
    Boolean,
    Multiselect,
    External,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormField {
    pub id: String,
    pub title: String,
    pub kind: FormFieldKind,
    pub required: bool,
    pub options: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormRequest {
    pub id: String,
    pub session: SessionId,
    pub title: String,
    pub fields: Vec<FormField>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FormAnswer {
    pub values: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    pub patch: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxKind {
    User,
    Synthetic,
    Compaction,
    Move,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxDelivery {
    Queue,
    Steer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxItem {
    pub id: String,
    pub session: SessionId,
    pub kind: InboxKind,
    pub delivery: InboxDelivery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpStatus {
    Connected,
    Pending,
    Disabled,
    Failed,
    NeedsAuth,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServer {
    pub name: String,
    pub status: McpStatus,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpConfig {
    pub name: String,
    pub command: Option<String>,
    pub url: Option<String>,
    pub env: BTreeMap<String, String>,
}
