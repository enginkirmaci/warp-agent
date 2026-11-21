//! The full application state: workspaces, servers, the shared tile grid,
//! transcripts, queues, permissions, questions and subagent tracking. A 1:1
//! port of MainViewModel + ChatTileViewModel + the ChatItems model.

use crate::harness::{Harness, HarnessConfig, HarnessId};
use crate::oc::{self, ApiRequest, CommandInfo, ModelInfo, NetCommand, SessionInfo, StoredMessage};
// the store's event intake is public, so its event type is reachable from here
pub use crate::oc::UiEvent;
use crate::settings::{AppSettings, UiState, WorkspaceState};
pub mod ipc;
// the impl Store continuation modules, one per section
mod chat_flow;
mod events;
mod harness_events;
mod items;
mod notes;
mod parsing;
mod permissions;
mod runs;
mod sessions;
mod sidebar;
mod streaming;
mod tile;
use crate::theme::{self, AccentColors, FontTokens};
use chrono::{Local, TimeZone, Timelike};
use gpui::prelude::*;
use gpui::{App, Context, Entity};
pub use items::*;
use items::{
    CRASH_RESTART_RESET_AFTER, MAX_AUTO_RETRY, MAX_CRASH_RESTARTS, PromptPayload, retry_backoff,
};
pub use parsing::*;
use parsing::{fork_transcript, lookup_context_limit};
pub use sidebar::*;
use sidebar::{session_recency, sort_workspaces, workspace_order_key};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
pub use tile::*;
use warpui_kit_platform::config::KeyMap;

pub const DEFAULT_MODEL_ENTRY: &str = "(server default)";
pub const DEFAULT_VARIANT_ENTRY: &str = "(default)";
pub const DEFAULT_AGENT_ENTRY: &str = "(default build)";
pub const DRAFT_TITLE: &str = "New chat";

/// What the composer's worktree toggle and restore affordance read for a
/// tile. The branch name is carried for restore and probes; the composer
/// never displays it.
#[derive(Clone, Debug, Default)]
pub struct WorktreeView {
    pub visible: bool,
    pub enabled: bool,
    pub branch: String,
    pub locked: bool,
    pub creating: bool,
    /// The checkout's folder is gone: the chat offers a restore instead of
    /// sending.
    pub missing: bool,
}

/// One linked worktree of a repository, as the sidebar nests it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub folder: String,
    pub branch: String,
}

/// The title a draft's branch name derives from: the chat's own title when it
/// has one, else the first line the user typed.
fn draft_title(tile: &ChatTile) -> String {
    let title = tile.title.trim();
    if !title.is_empty() && title != DRAFT_TITLE {
        return title.to_string();
    }
    tile.input.lines().next().unwrap_or("").trim().to_string()
}

/// The repository a checkout sits in: the nearest ancestor that still exists
/// and carries a `.git`. The checkout itself is gone whenever this runs.
fn repository_of(folder: &str) -> Option<std::path::PathBuf> {
    std::path::Path::new(folder)
        .ancestors()
        .skip(1)
        .find(|ancestor| ancestor.join(".git").exists())
        .map(|ancestor| ancestor.to_path_buf())
}

/// Minimum spacing between stream-driven repaints; 10 fps is plenty for
/// streaming text and keeps a busy turn off the compositor's frame rate.
const STREAM_REPAINT_INTERVAL: Duration = Duration::from_millis(100);

/// How many parent links a subagent lineage walk follows before treating the
/// chain as malformed. A real tree is shallow; a cycle must not hang the store.
const MAX_LINEAGE_DEPTH: usize = 64;

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

/// The variants one harness/model pair reports; empty until that harness's
/// catalog lands (or when the model has no tiers).
fn variants_for_entry(model_infos: &[ModelInfo], harness: &HarnessId, model: &str) -> Vec<String> {
    model_infos
        .iter()
        .find(|entry| &entry.harness == harness && entry.key == model)
        .map(|entry| entry.variants.clone())
        .unwrap_or_default()
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
    } else if span.num_days() < 30 {
        format!("{}w", span.num_days() / 7)
    } else if span.num_days() < 365 {
        format!("{}mo", span.num_days() / 30)
    } else {
        format!("{}y", span.num_days() / 365)
    }
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

/// One live chat in the title bar's running panel, worded for display. The
/// chip shows the only job's title when one runs; the panel lists every job
/// in started order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningJob {
    /// Stable id for the row (also the probe selector): "chat:<session id>".
    pub key: String,
    /// The chat's title, e.g. "Fix the login bug".
    pub title: String,
    /// The workspace the chat belongs to.
    pub detail: Option<String>,
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
    /// Whether the typed harness-event path is live; every adapter, managed or
    /// attached, feeds it.
    harness_ingest: bool,
    /// Attached harness adapters while the app has them, by registry id; each
    /// event stream is pumped into the typed store path by a spawned task.
    /// Managed harnesses live on the net thread's host instead.
    attached: BTreeMap<HarnessId, Box<dyn Harness>>,
    /// Last attach failure per attached harness, surfaced by settings/door.
    pub attached_status: BTreeMap<HarnessId, String>,
    pub is_settings_open: bool,
    /// The rail's About section: the family table, not a chat surface.
    pub is_about_open: bool,
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
    /// The start order behind the title bar's running panel: session id →
    /// the sequence the chat began its run in. A key already live keeps its
    /// original position, so rows never reshuffle while a run is on.
    running_order: HashMap<String, u64>,
    running_seq: u64,
    /// Every catalog entry with its harness, including the two harnesses'
    /// same-named models; `models` stays the de-duplicated picker list.
    model_infos: Vec<ModelInfo>,
    /// Harnesses whose catalog came from a live server this session. A
    /// cache-loaded list still answers one door request with a refresh, so
    /// clients see real variants instead of the cache's empty ones.
    models_live: HashSet<HarnessId>,
    /// The harness whose settings refresh is waiting on `ModelsLoaded`; the
    /// picker's refresh button spins until the event clears it.
    pub models_refreshing: Option<HarnessId>,
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
    /// pending_sends so the create reply can record the right backend
    pending_create_harness: std::collections::VecDeque<(String, HarnessId)>,
    /// shared-server drafts parked while its process starts: (tile key, text,
    /// attachments, workspace folder). ServerStarted converts them into
    /// pending_sends + CreateSessionIn pairs.
    pending_shared_drafts: Vec<(String, String, Vec<AttachmentItem>, String)>,
    /// workspaces started by Restore whose open-tile lists reload on ServerStarted
    pending_restores: Vec<(String, Vec<String>)>,
    /// chats requested while their workspace server was still starting
    pending_opens: Vec<(String, String)>,
    /// sends parked because the chat's server had idled out; they flow when
    /// ServerStarted lands for their net key
    pending_session_sends: Vec<(String, String, String, Vec<AttachmentItem>)>,
    /// Whether a workspace folder is a git repository, for the worktree
    /// toggle's visibility: filled by the net thread, cached per folder.
    repository_probes: HashMap<String, bool>,
    /// Folders with a repository probe in flight.
    repository_probing: HashSet<String>,
    /// Linked worktrees by their repository's main checkout, branch-sorted.
    worktrees_by_parent: HashMap<String, Vec<WorktreeEntry>>,
    /// Every linked-worktree folder: its own row is hidden, and it never
    /// offers a worktree of its own.
    worktree_folders: HashSet<String>,
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
    /// Sessions a background door run owns: never listed in a workspace's
    /// sidebar section even when a server session list names them (their
    /// list refresh races the run), and forgotten when the run retires.
    hidden_sessions: HashSet<String>,
    /// Chat ids the user deleted, persisted in ui.json. dsh's archive set is
    /// per-process, so its `session/list` can still name a deleted chat after
    /// the app restarts; these ids are filtered out of every list.
    deleted_sessions: HashSet<String>,
    /// the door's models command parked while a server spins up, with the
    /// harness it asked for (None = any)
    /// Door `models` requests parked on a server start or a refresh. A list,
    /// not a single slot: a second client (or the settings refresh while the
    /// page's first load is still in flight) must not drop the first reply.
    ipc_models_waiters: Vec<(Option<HarnessId>, crate::ipc::Reply)>,
    /// Door `harness` calls parked on a net-thread answer, keyed by call id;
    /// a timeout task sweeps one that the net thread never answers.
    ipc_harness_calls: std::collections::BTreeMap<u64, crate::ipc::Reply>,
    /// Door `harness start` calls parked on a ServerStarted fact:
    /// `(harness, call id in ipc_harness_calls)`.
    ipc_harness_start_calls: Vec<(HarnessId, u64)>,
    /// The next door harness call id.
    next_ipc_harness_call: u64,
    /// reused checkout dummy for `with_tile` — hot stream events would
    /// otherwise build a throwaway ChatTile each
    tile_placeholder: Option<ChatTile>,

    // ---- composer suggestions (/commands, @files) ----
    /// The workspace's server commands + skills, keyed by workspace folder
    /// path; filled on ServerStarted and refreshed when a slash menu opens.
    pub commands: HashMap<String, Vec<CommandInfo>>,
    /// Net keys whose command list is in flight (the menu says loading until
    /// the answer lands, instead of the false "no server commands").
    commands_loading: HashSet<String>,
    /// shared-server drafts whose slash menu asked for the agent-scoped catalog before
    /// the chat had a session; the session-create reply drains them.
    pending_command_tiles: HashSet<String>,
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
        let mut settings = AppSettings::load();
        let mut ui = UiState::load();
        // one-time migration: the pre-settings auto-allow choice moves into
        // settings.json, which owns the switch from then on. The ui key is
        // read here and never again.
        let legacy_auto_allow = ui.auto_allow_always.take();
        if settings.auto_allow_always.is_none()
            && let Some(legacy) = legacy_auto_allow
        {
            settings.auto_allow_always = Some(legacy);
            settings.save();
        }
        let fonts = theme::active_tokens();
        let auto_allow = settings.auto_allow_always.unwrap_or(true);

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
            accent: theme::accent_colors(),
            fonts,
            tool_tips_visible: settings.show_tooltips,
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
            harness_ingest: true,
            attached: BTreeMap::new(),
            attached_status: BTreeMap::new(),
            is_settings_open: false,
            is_about_open: false,
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
            running_order: HashMap::new(),
            running_seq: 0,
            model_infos,
            models_live: HashSet::new(),
            models_refreshing: None,
            variants_by_model,
            context_limit_by_model,
            subagent_by_session: HashMap::new(),
            running_subagents: HashSet::new(),
            countdowns: HashMap::new(),
            prompt_history: Vec::new(),
            pending_sends: Vec::new(),
            pending_create_harness: std::collections::VecDeque::new(),
            pending_shared_drafts: Vec::new(),
            pending_restores: Vec::new(),
            pending_opens: Vec::new(),
            pending_session_sends: Vec::new(),
            repository_probes: HashMap::new(),
            repository_probing: HashSet::new(),
            worktrees_by_parent: HashMap::new(),
            worktree_folders: HashSet::new(),
            server_activity: HashMap::new(),
            last_idle_sweep: None,
            todos_spring: None,
            stream_repaint_last: None,
            stream_repaint_scheduled: false,
            stream_dirty: Vec::new(),
            tile_placeholder: None,
            ipc_runs: HashMap::new(),
            hidden_sessions: HashSet::new(),
            deleted_sessions: ui.deleted_sessions.iter().cloned().collect(),
            ipc_models_waiters: Vec::new(),
            ipc_harness_calls: std::collections::BTreeMap::new(),
            ipc_harness_start_calls: Vec::new(),
            next_ipc_harness_call: 1,
            commands: HashMap::new(),
            commands_loading: HashSet::new(),
            pending_command_tiles: HashSet::new(),
            file_results: None,
            mcp_names: HashMap::new(),
            mcp_disabled: ui.mcp_disabled_workspaces.iter().cloned().collect(),
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

        // favorites are workspace-level now; an old ui.json stored session ids
        // in the same map (chats use pins), so drop everything that is not a
        // known workspace path
        let workspace_paths: HashSet<String> =
            store.ui.workspaces.iter().map(|w| w.path.clone()).collect();
        store
            .ui
            .favorites
            .retain(|key, _| workspace_paths.contains(key));

        sort_workspaces(&mut store.ui.workspaces, &store.ui.favorites);

        // saved open-tile ids of chats that no longer exist could never restore
        for ws in &mut store.ui.workspaces {
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
            .map(|state| WorkspaceItem::new(state, &store.ui.favorites))
            .collect();
        // Every launch starts with the sections folded: an open section is a
        // transient view choice (and auto-connects its server), never a
        // startup state, so a saved flag is ignored here.
        for ws in &mut store.workspaces {
            ws.set_expanded(false);
        }
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
            // the sidebar nests worktree chats under their repository: ask for
            // every open workspace's worktree list once, off the UI thread
            let folders: Vec<String> = store
                .workspaces
                .iter()
                .map(|workspace| workspace.folder_path().to_string())
                .collect();
            for folder in folders {
                store.probe_repository(&folder);
            }
        });
        entity
    }

    /// A store over a dead net channel: headless probes drive the real views
    /// without a backend. The receiver is dropped with this call, so any
    /// network command the probe causes is a no-op.
    #[doc(hidden)]
    pub fn for_probe(cx: &mut App) -> Entity<Self> {
        let (tx, _rx) = async_channel::unbounded();
        Self::new(cx, crate::oc::NetSender(tx))
    }

    /// The same dead store, with its net channel handed to the test: a probe
    /// that must read the commands a door call puts on the wire.
    #[doc(hidden)]
    pub fn for_probe_with_net(
        cx: &mut App,
    ) -> (Entity<Self>, async_channel::Receiver<crate::oc::NetCommand>) {
        let (tx, rx) = async_channel::unbounded();
        (Self::new(cx, crate::oc::NetSender(tx)), rx)
    }

    // ---- seeds / persistence ----

    pub fn seed_model(&self) -> Option<String> {
        self.ui
            .model
            .clone()
            .or_else(|| self.settings.default_model.clone())
    }

    /// The model keys one harness lists, in load order. The settings picker
    /// scopes its rows to the harness the segmented control shows; the
    /// merged `models` list cannot distinguish the two catalogs.
    pub fn model_keys_for(&self, harness: &HarnessId) -> Vec<String> {
        if !self.harness_enabled(harness) {
            return Vec::new();
        }
        self.model_infos
            .iter()
            .filter(|model| &model.harness == harness)
            .map(|model| model.key.clone())
            .collect()
    }

    /// The variants one harness/model pair reports. Empty until that
    /// harness's catalog lands (or when the model has no tiers), which the
    /// settings Variant picker explains instead of showing an empty list.
    pub fn variants_for(&self, harness: &HarnessId, model: &str) -> Vec<String> {
        if !self.harness_enabled(harness) {
            return Vec::new();
        }
        variants_for_entry(&self.model_infos, harness, model)
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
        self.ui.mcp_disabled_workspaces = {
            let mut paths: Vec<String> = self.mcp_disabled.iter().cloned().collect();
            paths.sort();
            paths
        };
        self.ui.deleted_sessions = {
            let mut ids: Vec<String> = self.deleted_sessions.iter().cloned().collect();
            ids.sort();
            ids
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
                // a background run's chat is never a tile the next launch
                // should restore
                .filter(|t| !t.background)
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

    /// Fire a /command refresh for the tile's workspace (menu open). A dsh
    /// draft has no session yet, so ask for one first: both `commands/list`
    /// and `skills/list` are agent-scoped, and the menu would otherwise stay
    /// empty until the first message creates the session.
    pub fn refresh_commands(&mut self, tile_key: &str, cx: &mut Context<Self>) {
        let Some(ws) = self.tile_net_key_or(tile_key) else {
            return;
        };
        self.commands_loading.insert(ws.clone());
        let tile = self.tile_by_key(tile_key);
        let harness = tile.map(|t| t.harness.clone()).unwrap_or_default();
        let session_id = tile.and_then(|t| t.session_id.clone());
        if crate::harness::workspace_scoped(&harness) {
            self.ensure_local_opencode_commands(&ws);
        }
        if crate::harness::shared_server(&harness) && session_id.is_none() {
            if self.pending_command_tiles.contains(tile_key) {
                // a create is already in flight; a second menu open must not
                // mint a second empty session
                self.repaint(cx);
                return;
            }
            self.pending_command_tiles.insert(tile_key.to_string());
            let shared = self.shared_key();
            if self.runtimes.contains_key(&shared) {
                self.create_command_session(tile_key, cx);
            } else {
                self.ensure_shared_server(cx);
            }
            self.repaint(cx);
            return;
        }
        let request = if crate::harness::shared_server(&harness) {
            ApiRequest::ListAgentCommands { session_id }
        } else {
            ApiRequest::ListCommands
        };
        self.net.send(NetCommand::Request {
            ws_key: ws,
            request,
        });
        self.repaint(cx);
    }

    /// opencode's `/compact` is a TUI command, not part of `GET /command`.
    /// Keep it in the display list so the menu has it before the server
    /// answers; a server-provided `compact` (custom config) wins.
    fn ensure_local_opencode_commands(&mut self, ws: &str) {
        let entry = self.commands.entry(ws.to_string()).or_default();
        if !entry.iter().any(|command| command.name == "compact") {
            entry.push(CommandInfo {
                name: "compact".into(),
                name_lower: "compact".into(),
                description: "Compact older conversation history".into(),
                source: "local".into(),
            });
        }
    }

    /// True while the tile's command list is still on the wire; the menu shows
    /// a loading row instead of claiming the server has none.
    pub fn tile_commands_loading(&self, tile_key: &str) -> bool {
        self.tile_net_key_or(tile_key)
            .is_some_and(|ws| self.commands_loading.contains(&ws))
    }

    /// Opens the shared server's session a draft's slash menu needs. The first
    /// real send finds `session_id` set and reuses it, so it is not wasted.
    fn create_command_session(&mut self, tile_key: &str, cx: &mut Context<Self>) {
        if self.tile_by_key(tile_key).is_none() {
            // the chat closed while waiting for the server
            self.pending_command_tiles.remove(tile_key);
            return;
        }
        let Some(folder) = self.tile_ws_key(tile_key) else {
            return;
        };
        let shared = self.shared_key();
        self.net.send(NetCommand::Request {
            ws_key: shared,
            request: ApiRequest::CreateCommandSession {
                tile_key: tile_key.to_string(),
                folder,
            },
        });
        let _ = cx;
    }

    /// Runs a listed shared-server command instead of sending it as a model
    /// prompt. The result comes back as `UiEvent::CommandExecuted`.
    pub(super) fn execute_harness_command(
        &mut self,
        session_id: &str,
        line: String,
        cx: &mut Context<Self>,
    ) {
        let shared = self.shared_key();
        self.net.send(NetCommand::Request {
            ws_key: shared,
            request: ApiRequest::ExecuteCommand {
                session_id: session_id.to_string(),
                line,
            },
        });
        self.repaint(cx);
    }

    /// opencode's on-demand compaction. The route needs a real model and a
    /// session with history; both are checked here so an empty chat never
    /// spends a model turn on a summary of nothing.
    pub(super) fn compact_opencode_session(
        &mut self,
        session_id: &str,
        args: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(tile_key) = self.tile_by_session.get(session_id).cloned() else {
            return;
        };
        if !args.is_empty() {
            self.tile_note(&tile_key, "Usage: /compact (no arguments)", true, cx);
            return;
        }
        let busy = self.running_by_session.contains(session_id);
        let (history, model) = match self.tile_by_key(&tile_key) {
            Some(tile) => (
                tile.items.iter().any(|item| {
                    matches!(
                        item.as_ref(),
                        ChatItem::UserMessage { .. } | ChatItem::AssistantText { .. }
                    )
                }),
                tile.selected_model
                    .clone()
                    .filter(|model| model != DEFAULT_MODEL_ENTRY)
                    .or_else(|| tile.last_used_model.clone()),
            ),
            None => return,
        };
        if busy {
            self.tile_note(&tile_key, "Wait for the current turn to finish.", true, cx);
            return;
        }
        if !history {
            self.tile_note(
                &tile_key,
                "Nothing to compact yet — send a message first.",
                true,
                cx,
            );
            return;
        }
        let Some(model) = model else {
            self.tile_note(&tile_key, "Pick a model before compacting.", true, cx);
            return;
        };
        let Some(ws) = self.net_key_for_session(session_id) else {
            return;
        };
        self.net.send(NetCommand::Request {
            ws_key: ws,
            request: ApiRequest::CompactSession {
                session_id: session_id.to_string(),
                model,
            },
        });
        self.repaint(cx);
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

    /// The workspace an unsent draft will create its session on: the
    /// selected one, unless the draft belongs to a background door run —
    /// that run's own workspace, so a hidden send never rides the user's
    /// selection.
    fn draft_workspace(&self, tile_key: &str) -> Option<String> {
        self.tile_by_key(tile_key)
            .and_then(|tile| tile.worktree.as_ref())
            .and_then(|worktree| worktree.folder.clone())
            .or_else(|| self.draft_parent_workspace(tile_key))
    }

    /// The workspace a draft is filed under, before any worktree exists: the
    /// selected one, unless the draft belongs to a background door run.
    fn draft_parent_workspace(&self, tile_key: &str) -> Option<String> {
        self.ipc_runs
            .values()
            .find(|run| run.tile_key == tile_key)
            .map(|run| run.workspace.clone())
            .or_else(|| self.selected_workspace.clone())
    }

    /// What the composer's worktree toggle and restore affordance read for a
    /// tile. The branch name is carried for restore and probes; the composer
    /// never displays it.
    pub fn worktree_view(&self, tile_key: &str) -> WorktreeView {
        let Some(tile) = self.tile_by_key(tile_key) else {
            return WorktreeView::default();
        };
        let parent_is_repository = self.draft_parent_workspace(tile_key).is_some_and(|folder| {
            self.repository_probes
                .get(&folder)
                .copied()
                .unwrap_or(false)
                // a linked worktree is not a place for another worktree
                && !self.worktree_folders.contains(&folder)
        });
        let worktree = tile.worktree.clone().unwrap_or_default();
        // offered on a draft in a repository, and kept afterwards so the chat
        // still shows where it runs
        let visible = worktree.enabled
            || worktree.folder.is_some()
            || (tile.session_id.is_none() && parent_is_repository);
        WorktreeView {
            visible,
            enabled: worktree.enabled || worktree.folder.is_some(),
            branch: worktree.branch,
            // once the worktree exists the choice is spent: the chat runs there
            locked: tile.session_id.is_some() || worktree.creating || worktree.folder.is_some(),
            creating: worktree.creating,
            missing: worktree.missing,
        }
    }

    /// The composer's worktree toggle: only a draft whose workspace is a git
    /// repository can turn it on.
    pub fn toggle_worktree(&mut self, tile_key: &str, cx: &mut Context<Self>) {
        let Some(parent) = self.draft_parent_workspace(tile_key) else {
            return;
        };
        if !self
            .repository_probes
            .get(&parent)
            .copied()
            .unwrap_or(false)
        {
            return;
        }
        if self.worktree_folders.contains(&parent) {
            return;
        }
        let Some(tile) = self.tile_by_key(tile_key) else {
            return;
        };
        if tile.session_id.is_some()
            || tile
                .worktree
                .as_ref()
                .is_some_and(|worktree| worktree.creating || worktree.folder.is_some())
        {
            return;
        }
        let Some(tile) = self.tile_by_key_mut(tile_key) else {
            return;
        };
        let mut worktree = tile.worktree.clone().unwrap_or_default();
        worktree.enabled = !worktree.enabled;
        // the name is not asked for here: the first send derives it from the
        // prompt, and the kit appends a suffix when it is already taken
        tile.worktree = Some(worktree);
        self.repaint(cx);
    }

    /// Probes a folder once: the worktree toggle is hidden until the answer
    /// says the folder is a repository.
    fn probe_repository(&mut self, folder: &str) {
        if folder.trim().is_empty()
            || self.repository_probes.contains_key(folder)
            || !self.repository_probing.insert(folder.to_string())
        {
            return;
        }
        self.net.send(NetCommand::ProbeRepository {
            folder: folder.to_string(),
        });
    }

    fn probe_selected_repository(&mut self) {
        if let Some(folder) = self.selected_workspace.clone() {
            self.probe_repository(&folder);
        }
    }

    /// The linked worktrees of a repository, branch-sorted: the sidebar nests
    /// their chats under the repository's row.
    pub fn worktrees_of(&self, parent: &str) -> Vec<WorktreeEntry> {
        self.worktrees_by_parent
            .get(parent)
            .cloned()
            .unwrap_or_default()
    }

    /// Whether a folder is a linked worktree: its own row stays hidden.
    pub fn is_worktree_workspace(&self, folder: &str) -> bool {
        self.worktree_folders.contains(folder)
    }

    /// Starts a chat in a worktree the sidebar shows: the folder becomes a
    /// workspace first when nobody opened it yet.
    pub fn new_chat_in_folder(&mut self, folder: &str, cx: &mut Context<Self>) {
        if !self
            .workspaces
            .iter()
            .any(|workspace| workspace.folder_path() == folder)
        {
            self.install_workspace(folder, cx);
        }
        self.new_chat_for_workspace(folder, cx);
    }

    /// Restores a chat's missing checkout: the same branch at the same path,
    /// which is the one place an existing branch is attached.
    pub fn recreate_worktree(&mut self, tile_key: &str, cx: &mut Context<Self>) {
        let Some(tile) = self.tile_by_key(tile_key) else {
            return;
        };
        let Some(worktree) = tile.worktree.clone() else {
            return;
        };
        let Some(folder) = worktree.folder.clone() else {
            return;
        };
        let Some(repo) = repository_of(&folder) else {
            self.tile_note(
                tile_key,
                "Could not find the repository this worktree belongs to.",
                true,
                cx,
            );
            return;
        };
        self.net.send(NetCommand::AttachWorktree {
            tile_key: tile_key.to_string(),
            repo: repo.to_string_lossy().into_owned(),
            folder,
            branch: worktree.branch.clone(),
        });
        self.tile_note(
            tile_key,
            &format!("Restoring the worktree {}…", worktree.branch),
            false,
            cx,
        );
        self.repaint(cx);
    }

    /// A worktree chat needs its repository's row to sit under: install the
    /// main checkout when a linked worktree is already an open workspace.
    fn install_missing_repository(&mut self, root: &str, cx: &mut Context<Self>) {
        if self
            .workspaces
            .iter()
            .any(|workspace| workspace.folder_path() == root)
        {
            return;
        }
        let wants_parent = self.worktrees_by_parent.get(root).is_some_and(|children| {
            children.iter().any(|child| {
                self.workspaces
                    .iter()
                    .any(|workspace| workspace.folder_path() == child.folder)
            })
        });
        if wants_parent {
            self.install_workspace(root, cx);
        }
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

    /// Seeds one open chat tile and its session mapping for a real-view probe.
    /// It does not focus the tile and sends nothing on the net channel.
    #[doc(hidden)]
    pub fn seed_probe_tile(
        &mut self,
        workspace: &str,
        session_id: &str,
        title: &str,
        items: Vec<ChatItem>,
        running: bool,
        cx: &mut Context<Self>,
    ) -> String {
        let key = format!("chat:{session_id}");
        let mut tile = ChatTile::new(key.clone(), None, None, None);
        tile.session_id = Some(session_id.to_string());
        tile.title = title.to_string();
        tile.items = items.into_iter().map(Arc::new).collect();
        tile.rev = Some(cx.new(|_| ()));
        self.workspace_by_session
            .insert(session_id.to_string(), workspace.to_string());
        self.tile_by_session
            .insert(session_id.to_string(), key.clone());
        self.open_tiles.retain(|tile| tile.key != key);
        self.open_tiles.push(tile);
        if running {
            self.begin_running(session_id);
            self.running_by_session.insert(session_id.to_string());
        }
        self.repaint(cx);
        key
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

    /// Running chats the sidebar shows. A background run's chat stays hidden
    /// until its run retires it, so it does not count: the door client owns
    /// that result and no row spins for it.
    pub fn running_chats(&self) -> usize {
        self.running_by_session
            .iter()
            .filter(|id| !self.hidden_sessions.contains(*id))
            .count()
    }

    /// Every live chat, in the order it started. The title bar's chip counts
    /// this list and its panel draws it; one caller owns both so the count
    /// and the rows can never disagree. The same hidden filter as
    /// [Self::running_chats] keeps the panel off the door's background runs.
    pub fn running_jobs(&self) -> Vec<RunningJob> {
        let mut jobs: Vec<(u64, RunningJob)> = Vec::new();
        for id in &self.running_by_session {
            if self.hidden_sessions.contains(id) {
                continue;
            }
            let seq = self.running_order.get(id).copied().unwrap_or(u64::MAX);
            let workspace = self.workspace_name_for(id);
            // the chat's visible name: the tile's title (a rename reaches it
            // at once), or the session registry when no tile is open
            let title = self
                .tile_for_session(id)
                .map(|tile| tile.title.clone())
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| self.title_for(id));
            jobs.push((
                seq,
                RunningJob {
                    key: format!("chat:{id}"),
                    title,
                    detail: (!workspace.is_empty()).then_some(workspace),
                },
            ));
        }
        jobs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.key.cmp(&b.1.key)));
        jobs.into_iter().map(|(_, job)| job).collect()
    }

    /// What the title bar says, from the same list the running panel draws.
    pub fn activity(&self) -> Option<String> {
        match self.running_jobs().as_slice() {
            [] => None,
            [job] => Some(job.title.clone()),
            jobs => Some(format!("{} running", jobs.len())),
        }
    }

    /// Records the sequence a chat's run began in; a session already live
    /// keeps its original position.
    pub(super) fn begin_running(&mut self, session_id: &str) {
        if !self.running_order.contains_key(session_id) {
            self.running_seq += 1;
            self.running_order
                .insert(session_id.to_string(), self.running_seq);
        }
    }

    pub(super) fn end_running(&mut self, session_id: &str) {
        self.running_order.remove(session_id);
    }

    pub fn tile_columns(&self) -> usize {
        let visible = self.open_tiles.iter().filter(|t| !t.background).count();
        ((visible as f64).sqrt().ceil() as usize).max(1)
    }

    pub fn has_tiles(&self) -> bool {
        self.open_tiles.iter().any(|t| !t.background)
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

    /// Open chats that hold a card only a person can answer. Background
    /// runs stay out: their client cannot answer a card, and an invisible
    /// tile must not light the attention badge.
    pub fn tiles_waiting(&self) -> usize {
        self.open_tiles
            .iter()
            .filter(|tile| !tile.background && tile.is_waiting())
            .count()
    }

    /// Whether this chat's open tile is waiting on a person.
    pub fn session_waiting(&self, session_id: &str) -> bool {
        self.open_tiles
            .iter()
            .any(|tile| tile.session_id.as_deref() == Some(session_id) && tile.is_waiting())
    }

    fn effective_workspace_path(&self, session_id: &str) -> Option<String> {
        lineage_workspace_path(
            session_id,
            &self.workspace_by_session,
            &self.subagent_by_session,
        )
    }

    /// Which backend a session runs on. Sessions predate per-chat harnesses,
    /// so anything unrecorded follows its owning chat; a tree with no recorded
    /// harness either is opencode.
    pub fn session_harness(&self, session_id: &str) -> HarnessId {
        lineage_harness(
            session_id,
            &self.ui.harness_by_session,
            &self.subagent_by_session,
        )
    }

    /// The registry's managed app-wide harness key (its id).
    pub fn shared_key(&self) -> String {
        crate::harness::shared_server_key()
    }

    /// Whether a net key names the shared app-wide server.
    pub fn is_shared_key(&self, key: &str) -> bool {
        crate::harness::is_shared_key(key)
    }

    /// Whether a session runs on the shared app-wide server.
    pub fn session_is_shared(&self, session_id: &str) -> bool {
        crate::harness::shared_server(&self.session_harness(session_id))
    }

    /// Whether a session runs on an attached harness (opencode v2).
    pub fn session_is_attached(&self, session_id: &str) -> bool {
        crate::harness::attached(&self.session_harness(session_id))
    }

    /// The net-thread key a session's server lives on: the workspace folder
    /// for a per-workspace harness, the harness id for a shared app-wide one.
    fn net_key_for_session(&self, session_id: &str) -> Option<String> {
        let harness = self.session_harness(session_id);
        if crate::harness::shared_server(&harness) {
            return Some(harness.as_str().to_string());
        }
        self.effective_workspace_path(session_id)
    }

    /// Same for a tile: an existing chat routes by its session; a draft by
    /// the harness pill the composer is showing.
    fn tile_net_key_or(&self, tile_key: &str) -> Option<String> {
        if let Some(tile) = self.tile_by_key(tile_key) {
            if crate::harness::shared_server(&tile.harness) {
                return Some(tile.harness.as_str().to_string());
            }
            if let Some(sid) = &tile.session_id {
                return self.net_key_for_session(sid);
            }
        }
        self.tile_ws_key(tile_key)
    }

    /// Starts the registry's shared app-wide server unless it is already up.
    /// Replies arrive as ServerStarted(its id); parked work drains there.
    pub fn ensure_shared_server(&mut self, cx: &mut Context<Self>) {
        let Some(shared) = crate::harness::shared_server_id() else {
            return;
        };
        let key = shared.as_str().to_string();
        self.touch_server_activity(&key);
        if self.runtimes.contains_key(&key) {
            return;
        }
        let executable = self.settings.harness_executable(shared.as_str());
        tracing::info!("starting the shared {shared} server");
        self.net.send(NetCommand::StartServer {
            ws_key: key,
            harness: shared,
            executable,
            folder: ".".into(),
        });
        let _ = cx;
    }

    /// The attached harness adapter for an id, once connected.
    pub fn attached(&self, id: &HarnessId) -> Option<&dyn Harness> {
        self.attached.get(id).map(|adapter| adapter.as_ref())
    }

    /// The mutable attached harness adapter for an id, once connected.
    pub fn attached_mut(&mut self, id: &HarnessId) -> Option<&mut Box<dyn Harness>> {
        self.attached.get_mut(id)
    }

    /// The last attach failure for an attached harness.
    pub fn attached_status(&self, id: &HarnessId) -> Option<&String> {
        self.attached_status.get(id)
    }

    fn set_attached_status(&mut self, id: &HarnessId, detail: impl Into<String>) {
        self.attached_status.insert(id.clone(), detail.into());
    }

    /// Connects one attached harness through its registry factory on first
    /// use and pumps its typed event stream into the store. The adapter stays
    /// connected until the harness is disabled; it never stops the shared
    /// service.
    pub fn ensure_attached(&mut self, id: &HarnessId, cx: &mut Context<Self>) {
        if !self.settings.harness_enabled(id.as_str()) {
            self.set_attached_status(id, format!("{} is disabled", crate::harness::label(id)));
            return;
        }
        if self.attached.contains_key(id) {
            return;
        }
        let Some(descriptor) = crate::harness::registry().get(id) else {
            self.set_attached_status(id, format!("unknown harness: {id}"));
            return;
        };
        let Some(factory) = descriptor.factory else {
            self.set_attached_status(id, format!("{id} has no adapter"));
            return;
        };
        let executable = self.settings.harness_executable(id.as_str());
        let mut adapter = factory(HarnessConfig::new(executable));
        match adapter.start() {
            Ok(()) => {
                self.harness_ingest = true;
                let events = adapter.events();
                self.attached.insert(id.clone(), adapter);
                self.attached_status.remove(id);
                self.load_attached_catalog(id, cx);
                let id = id.clone();
                cx.spawn(async move |store, cx| {
                    while let Ok(event) = events.recv().await {
                        let id = id.clone();
                        let _ = store.update(cx, |store, cx| {
                            store.on_harness_event(&id, None, event, cx);
                        });
                    }
                })
                .detach();
            }
            Err(err) => {
                let detail = err.to_string();
                tracing::warn!("{id} attach failed: {detail}");
                self.set_attached_status(id, detail.clone());
                if let Some(tile) = self.focused_tile.clone() {
                    self.tile_note(
                        &tile,
                        &format!("{} is unavailable: {detail}", crate::harness::label(id)),
                        true,
                        cx,
                    );
                }
                self.repaint(cx);
            }
        }
    }

    pub(super) fn on_harness_event(
        &mut self,
        harness: &HarnessId,
        directory: Option<&str>,
        event: crate::harness::HarnessEvent,
        cx: &mut Context<Self>,
    ) {
        let directory = directory.map(std::path::Path::new);
        if let Err(err) = self.ingest_harness_event_from(harness, event, directory, cx) {
            tracing::warn!("harness event ingest failed: {err}");
        }
    }

    fn load_attached_catalog(&mut self, id: &HarnessId, cx: &mut Context<Self>) {
        let Some(adapter) = self.attached.get_mut(id) else {
            return;
        };
        match adapter.models() {
            Ok(models) => {
                self.model_infos.retain(|model| &model.harness != id);
                for model in models {
                    if !self.models.iter().any(|key| key == &model.key) {
                        self.models.push(model.key.clone());
                    }
                    if !model.variants.is_empty() {
                        self.variants_by_model
                            .insert(model.key.clone(), model.variants.clone());
                    }
                    self.context_limit_by_model
                        .insert(model.key.clone(), model.context_limit);
                    self.model_infos.push(ModelInfo {
                        key: model.key,
                        harness: id.clone(),
                        variants: model.variants,
                        context_limit: model.context_limit,
                    });
                }
                self.models_live.insert(id.clone());
                self.models_refreshing = None;
                self.attached_status.remove(id);
                self.repaint(cx);
            }
            Err(err) => {
                let detail = err.to_string();
                self.set_attached_status(id, detail);
                self.models_refreshing = None;
                self.repaint(cx);
            }
        }
    }

    pub fn harness_enabled(&self, harness: &HarnessId) -> bool {
        self.settings.harness_enabled(harness.as_str())
    }

    pub fn harness_id_enabled(&self, id: &str) -> bool {
        self.settings.harness_enabled(id)
    }

    /// The next enabled harness after `current`, for the composer pill.
    pub fn next_enabled_harness(&self, current: &HarnessId) -> Option<HarnessId> {
        let all = HarnessId::all();
        let start = all
            .iter()
            .position(|harness| harness == current)
            .map(|index| index + 1)
            .unwrap_or(0);
        for offset in 0..all.len() {
            let harness = &all[(start + offset) % all.len()];
            if harness != current && self.harness_enabled(harness) {
                return Some(harness.clone());
            }
        }
        None
    }

    /// Whether a running turn belongs to this harness.
    pub fn harness_has_running_turn(&self, id: &str) -> bool {
        self.running_by_session
            .iter()
            .any(|session| self.session_harness(session).as_str() == id)
    }

    /// Enable or disable one harness. Disabling a managed harness stops its
    /// processes; the last enabled harness cannot be disabled, and the default
    /// falls back to another enabled one. Re-enabling refreshes the catalog.
    pub fn set_harness_enabled(
        &mut self,
        id: &str,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if crate::harness::HarnessRegistry::builtin()
            .resolve(id)
            .is_err()
        {
            return Err(format!("unknown harness: {id}"));
        }
        if self.settings.harness_enabled(id) == enabled {
            return Ok(());
        }
        if !enabled {
            // v2 cannot be the default until the registry-fed default lands;
            // counting only the enum-backed harnesses keeps a usable backend.
            let enabled_count = HarnessId::all()
                .iter()
                .filter(|harness| self.harness_enabled(harness))
                .count();
            if enabled_count <= 1 {
                return Err("The last enabled harness cannot be disabled.".into());
            }
        }
        let executable = self.settings.harness_executable(id);
        self.settings
            .harnesses
            .entry(id.to_string())
            .or_insert(crate::settings::HarnessEntry::new(enabled, executable))
            .enabled = enabled;
        if !enabled {
            let harness = HarnessId::from(id);
            if crate::harness::attached(&harness) {
                if let Some(mut adapter) = self.attached.remove(&harness) {
                    let _ = adapter.stop();
                }
                self.set_attached_status(&harness, "disconnected");
            } else if crate::harness::shared_server(&harness) {
                let key = self.shared_key();
                if self.runtimes.contains_key(&key) {
                    self.net.send(NetCommand::StopServer {
                        ws_key: key.clone(),
                    });
                    self.teardown_runtime(&key);
                }
            } else {
                let keys: Vec<String> = self
                    .runtimes
                    .keys()
                    .filter(|key| !self.is_shared_key(key))
                    .cloned()
                    .collect();
                for key in keys {
                    self.net.send(NetCommand::StopServer {
                        ws_key: key.clone(),
                    });
                    self.teardown_runtime(&key);
                }
            }
            if self.settings.default_harness.as_str() == id {
                self.settings.default_harness = self.settings.default_harness_id();
            }
        }
        self.settings.save();
        self.save_state();
        self.ipc_push_status();
        if enabled && let Some(harness) = HarnessId::from_wire(id) {
            self.refresh_models(harness, cx);
        }
        self.repaint(cx);
        Ok(())
    }

    /// Re-reads one harness's model catalog for the settings picker. Starts
    /// the backend when it is not up; `ModelsLoaded` refreshes the rows. The
    /// dsh path shares the one `dsh web` server; opencode uses the built-in
    /// General workspace, the same place the door's first models request goes.
    pub fn refresh_models(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        // the picker's refresh button spins until the catalog lands
        self.models_refreshing = Some(harness.clone());
        if crate::harness::attached(&harness) {
            self.ensure_attached(&harness, cx);
        } else if crate::harness::shared_server(&harness) {
            let shared = self.shared_key();
            let running = self.runtimes.contains_key(&shared);
            self.ensure_shared_server(cx);
            if running {
                self.net.send(NetCommand::Request {
                    ws_key: shared,
                    request: ApiRequest::ListModels,
                });
            }
            // otherwise ServerStarted asks for the list on its own
        } else {
            let general = crate::paths::AppPaths::general_workspace_path()
                .to_string_lossy()
                .to_string();
            if !self.workspaces.iter().any(|ws| ws.folder_path() == general) {
                self.install_workspace(&general, cx);
            }
            if !self.workspaces.iter().any(|ws| ws.folder_path() == general) {
                // no workspace to host the server: fail the spinner
                // instead of parking until the next unrelated event
                self.models_refreshing = None;
                self.repaint(cx);
                return;
            }
            if self.runtimes.contains_key(&general) {
                self.net.send(NetCommand::Request {
                    ws_key: general,
                    request: ApiRequest::ListModels,
                });
            } else {
                self.start_workspace(&general, cx);
            }
        }
        self.repaint(cx);
    }

    /// Refreshes every workspace's shared-server chat list. Each request
    /// filters by the workspace folder server-side (the shared harness's
    /// sessions carry their own cwd).
    fn refresh_shared_lists(&mut self) {
        let shared = self.shared_key();
        if !self.runtimes.contains_key(&shared) {
            return;
        }
        let folders: Vec<String> = self
            .workspaces
            .iter()
            .filter(|ws| {
                ws.state
                    .sessions
                    .iter()
                    .any(|sid| self.session_is_shared(sid))
            })
            .map(|ws| ws.folder_path().to_string())
            .collect();
        for folder in folders {
            self.net.send(NetCommand::Request {
                ws_key: shared.clone(),
                request: ApiRequest::ListSessionsIn { folder },
            });
        }
    }

    /// Restarts whichever server a net key names — a workspace's managed
    /// process or the shared app-wide singleton.
    fn start_server_for_key(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.is_shared_key(key) {
            self.ensure_shared_server(cx);
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
        let idle_min = self.settings.server_idle_stop_minutes;
        if idle_min <= 0 {
            return;
        }
        // in-flight creates park work outside the runtimes; wait for them
        if !self.pending_sends.is_empty()
            || !self.pending_create_harness.is_empty()
            || !self.pending_shared_drafts.is_empty()
        {
            return;
        }
        let now = Instant::now();
        let keys: Vec<String> = self
            .runtimes
            .keys()
            .filter(|key| !self.is_shared_key(key))
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
            self.settings.server_idle_stop_minutes
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

    /// ServerStarted for the shared app-wide server: pickers fill from it,
    /// parked work starts flowing, every workspace's chat list refreshes.
    fn on_shared_server_started(&mut self, cx: &mut Context<Self>) {
        let shared = self.shared_key();
        self.touch_server_activity(&shared);
        self.runtimes.insert(
            shared.clone(),
            WorkspaceRuntime {
                stream_status: oc::StreamStatus::Connected,
                healthy: None,
            },
        );
        self.net.send(NetCommand::Request {
            ws_key: shared.clone(),
            request: ApiRequest::ListModels,
        });
        self.net.send(NetCommand::Request {
            ws_key: shared.clone(),
            request: ApiRequest::ListAgents,
        });
        // a draft opened its slash menu before the shared server existed:
        // open the session its catalog needs now
        let pending_commands: Vec<String> = self.pending_command_tiles.iter().cloned().collect();
        for tile_key in pending_commands {
            self.create_command_session(&tile_key, cx);
        }
        self.refresh_shared_lists();
        // a restart must refill the menu of chats that already had a session
        self.net.send(NetCommand::Request {
            ws_key: shared.clone(),
            request: ApiRequest::ListAgentCommands { session_id: None },
        });
        // drafts parked while the server spun up become real creates now
        let parked: Vec<(String, String, Vec<AttachmentItem>, String)> =
            std::mem::take(&mut self.pending_shared_drafts);
        for (tile_key, text, attachments, folder) in parked {
            self.pending_sends.push((tile_key, text, attachments));
            self.pending_create_harness.push_back((
                folder.clone(),
                crate::harness::shared_server_id().unwrap_or_default(),
            ));
            self.net.send(NetCommand::Request {
                ws_key: shared.clone(),
                request: ApiRequest::CreateSessionIn {
                    folder,
                    title: DRAFT_TITLE.into(),
                },
            });
        }
        // sends parked while the shared server's port was closed
        self.drain_pending_session_sends(&shared, cx);
        let pending_restores: Vec<(String, Vec<String>)> = self
            .pending_restores
            .extract_if(.., |(path, _)| path == &shared)
            .collect();
        for (_, wanted) in pending_restores {
            self.restore_open_tiles(&shared, wanted, cx);
        }
        let pending_opens: Vec<(String, String)> = self
            .pending_opens
            .extract_if(.., |(path, _)| path == &shared)
            .collect();
        for (_, session_id) in pending_opens {
            self.net.send(NetCommand::Request {
                ws_key: shared.clone(),
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
        for _ in 0..MAX_LINEAGE_DEPTH {
            match self.subagent_by_session.get(&current) {
                Some(sub) if sub.parent_session_id.as_str() != current => {
                    current = sub.parent_session_id.clone();
                }
                _ => break,
            }
        }
        current
    }

    // ---- workspaces ----

    pub fn add_workspace(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.install_workspace(path, cx) {
            return;
        }
        let normalized = normalize_dir(path);
        self.select_workspace(&normalized, cx);
        self.ipc_push_status();
    }

    /// Adds a workspace to the lists without selecting it: the shared body of
    /// [`add_workspace`]. A background door run needs the workspace's server,
    /// but the user's sidebar selection must stay where it was. False when
    /// the folder is missing, empty, or already a workspace.
    fn install_workspace(&mut self, path: &str, cx: &mut Context<Self>) -> bool {
        if path.trim().is_empty() {
            return false;
        }
        let normalized = normalize_dir(path);
        if !std::path::Path::new(&normalized).exists() {
            self.announce(&format!("Folder does not exist: {normalized}"), true, cx);
            return false;
        }
        if self
            .workspaces
            .iter()
            .any(|w| w.folder_path() == normalized)
        {
            self.announce("That folder is already a workspace.", false, cx);
            return false;
        }
        let state = WorkspaceState {
            path: normalized.clone(),
            ..Default::default()
        };
        self.ui.workspaces.push(state.clone());
        let item = WorkspaceItem::new(state, &self.ui.favorites);
        self.workspaces.push(item);
        sort_workspaces(&mut self.ui.workspaces, &self.ui.favorites);
        self.workspaces
            .sort_by_key(|w| workspace_order_key(w.folder_path(), &self.ui.favorites));
        self.save_state();
        self.ipc_push_status();
        true
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
        // the folder is gone; its star would sort nothing
        self.ui.favorites.remove(path);
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
        let Some(harness) = crate::harness::workspace_harness() else {
            return;
        };
        let executable = self.settings.harness_executable(harness.as_str());
        tracing::info!("starting server for {}", ws.name);
        self.net.send(NetCommand::StartServer {
            ws_key: folder,
            harness,
            executable,
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
        self.probe_selected_repository();
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
        let shared = self.shared_key();
        if self.runtimes.contains_key(&shared) {
            self.net.send(NetCommand::Request {
                ws_key: shared,
                request: ApiRequest::ListSessionsIn {
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
        let shared = self.shared_key();
        if self.runtimes.contains_key(&shared) {
            self.net.send(NetCommand::Request {
                ws_key: shared,
                request: ApiRequest::ListSessionsIn {
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
                    .any(|sid| self.session_is_shared(sid))
            })
        {
            // the list (and every pending open) flows once ServerStarted lands
            self.ensure_shared_server(cx);
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
        self.accent = theme::accent_colors();
        theme::apply_kit_theme(self.accent, &self.fonts, cx);
        crate::ui::tooltip::sync_settings(cx, self.tool_tips_visible, self.fonts.font_meta);
        cx.notify();
    }

    /// The composer shield's switch. It writes the one owner, the Settings
    /// key, and persists it, so both switches and every later settings save
    /// read the same value.
    pub fn set_auto_allow(&mut self, allowed: bool, cx: &mut Context<Self>) {
        self.auto_allow = allowed;
        self.settings.auto_allow_always = Some(allowed);
        self.settings.save();
        self.repaint(cx);
    }

    /// Settings page applied: pushes persisted values back into live state.
    pub fn apply_saved_settings(&mut self, cx: &mut Context<Self>) {
        // settings.auto_allow_always owns the switch; the live flag only ever
        // mirrors it, so a settings save cannot re-enable what the composer
        // turned off
        self.auto_allow = self.settings.auto_allow_always.unwrap_or(true);
        self.accent = theme::accent_colors();
        // re-map the kit theme so input selection/ring/primary follow the preset
        // (Theme::update refreshes every window)
        theme::apply_kit_theme(self.accent, &self.fonts, cx);
        self.tool_tips_visible = self.settings.show_tooltips;
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

/// The folder a session's harness runs in: its own workspace entry, else the
/// nearest ancestor's. A subagent session usually has no entry of its own, so
/// a nested run climbs to the chat that spawned it; the depth cap keeps a
/// malformed parent cycle from looping forever.
fn lineage_workspace_path(
    session_id: &str,
    workspaces: &HashMap<String, String>,
    subagents: &HashMap<String, SubagentInfo>,
) -> Option<String> {
    let mut current = session_id;
    for _ in 0..=MAX_LINEAGE_DEPTH {
        if let Some(path) = workspaces.get(current) {
            return Some(path.clone());
        }
        match subagents.get(current) {
            Some(sub) if sub.parent_session_id.as_str() != current => {
                current = sub.parent_session_id.as_str();
            }
            _ => return None,
        }
    }
    None
}

/// The harness a session runs on: its own recorded harness, else the nearest
/// ancestor's. A tree with no recorded harness is opencode, the pre-harness
/// default. Same depth cap as the workspace walk.
fn lineage_harness(
    session_id: &str,
    harnesses: &KeyMap<HarnessId>,
    subagents: &HashMap<String, SubagentInfo>,
) -> HarnessId {
    let mut current = session_id;
    for _ in 0..=MAX_LINEAGE_DEPTH {
        if let Some(kind) = harnesses.get(current) {
            return kind.clone();
        }
        match subagents.get(current) {
            Some(sub) if sub.parent_session_id.as_str() != current => {
                current = sub.parent_session_id.as_str();
            }
            _ => break,
        }
    }
    crate::harness::workspace_harness().unwrap_or_default()
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

#[cfg(test)]
mod time_tests {
    use super::relative_time_label;
    use chrono::Local;

    fn ago(days: i64) -> i64 {
        (Local::now() - chrono::Duration::days(days)).timestamp_millis()
    }

    #[test]
    fn ages_stay_relative() {
        assert_eq!(relative_time_label(ago(3)), "3d");
        assert_eq!(relative_time_label(ago(14)), "2w");
        assert_eq!(relative_time_label(ago(60)), "2mo");
        assert_eq!(relative_time_label(ago(400)), "1y");
    }
}

#[cfg(test)]
mod variant_lookup_tests {
    use super::{HarnessId, ModelInfo, variants_for_entry};

    fn info(key: &str, harness: HarnessId, variants: &[&str]) -> ModelInfo {
        ModelInfo {
            key: key.into(),
            harness,
            variants: variants.iter().map(|v| (*v).to_string()).collect(),
            context_limit: 0,
        }
    }

    /// The same model key exists in both catalogs; a variant pick must stay
    /// scoped to the harness the settings page is showing.
    #[test]
    fn variants_resolve_per_harness_and_model() {
        let infos = [
            info("zai/glm", HarnessId::from("opencode"), &["low", "max"]),
            info("zai/glm", HarnessId::from("dsh"), &["max"]),
            info("no/tiers", HarnessId::from("opencode"), &[]),
        ];
        assert_eq!(
            variants_for_entry(&infos, &HarnessId::from("opencode"), "zai/glm"),
            ["low", "max"]
        );
        assert_eq!(
            variants_for_entry(&infos, &HarnessId::from("dsh"), "zai/glm"),
            ["max"]
        );
        assert!(variants_for_entry(&infos, &HarnessId::from("opencode"), "no/tiers").is_empty());
        assert!(variants_for_entry(&infos, &HarnessId::from("opencode"), "missing").is_empty());
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    /// Tests that read or rewrite `HOME` take turns: the process environment
    /// is shared by every test in this binary.
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn lock_home() -> MutexGuard<'static, ()> {
        HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A scratch `HOME` for a test that reads or writes app state; pair it with
    /// [`lock_home`].
    pub(crate) struct HomeGuard {
        pub(crate) home: PathBuf,
        old_home: Option<OsString>,
        old_profile: Option<OsString>,
    }

    impl HomeGuard {
        pub(crate) fn new() -> Self {
            let home =
                std::env::temp_dir().join(format!("warp-agent-test-home-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home).expect("scratch home");
            let old_home = std::env::var_os("HOME");
            let old_profile = std::env::var_os("USERPROFILE");
            // edition 2024 marks the process environment as unsafe to mutate
            unsafe {
                std::env::set_var("HOME", &home);
                std::env::set_var("USERPROFILE", &home);
            }
            Self {
                home,
                old_home,
                old_profile,
            }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.old_home {
                    Some(value) => std::env::set_var("HOME", value),
                    None => std::env::remove_var("HOME"),
                }
                match &self.old_profile {
                    Some(value) => std::env::set_var("USERPROFILE", value),
                    None => std::env::remove_var("USERPROFILE"),
                }
            }
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }
}

#[cfg(test)]
mod command_flow_tests {
    use super::test_support::HomeGuard;
    use super::*;
    use crate::oc::{ApiRequest, NetCommand, UiEvent};
    use serde_json::json;

    /// A dsh `session/title` arrives as `session.updated`; the tile header and
    /// the sidebar row must both take the server's title, without waiting for
    /// a list refresh.
    #[gpui_kit::gpui::test]
    fn a_streamed_session_title_reaches_the_tile_and_the_sidebar(cx: &mut gpui::TestAppContext) {
        let _lock = super::test_support::lock_home();
        let home = HomeGuard::new();
        let folder = home.home.join("workspace");
        std::fs::create_dir_all(&folder).expect("workspace");
        let folder = folder.to_string_lossy().to_string();
        let store = cx.update(Store::for_probe);
        let key = cx.update(|cx| {
            store.update(cx, |store, cx| {
                store.add_workspace(&folder, cx);
                let key = store.seed_probe_tile(
                    &folder,
                    "probe-session",
                    DRAFT_TITLE,
                    Vec::new(),
                    false,
                    cx,
                );
                store.sessions.push(SessionItem {
                    id: "probe-session".into(),
                    title: DRAFT_TITLE.into(),
                    updated_ms: 1,
                    created_ms: 1,
                    is_pinned: false,
                    is_selected: false,
                    is_running: false,
                    is_editing: false,
                    draft_title: String::new(),
                });
                if let Some(ws) = store.workspace_by_path_mut(&folder) {
                    ws.state.sessions.push("probe-session".into());
                }
                store.sync_workspace_chats(cx);
                key
            })
        });

        store.update(cx, |store, cx| {
            store.on_net_event(
                UiEvent::Event {
                    ws_key: crate::harness::shared_server_key(),
                    event: json!({
                        "type": "session.updated",
                        "properties": { "info": {
                            "id": "probe-session",
                            "title": "Chat bubble polish",
                        } },
                    }),
                },
                cx,
            );
        });

        store.read_with(cx, |store, _| {
            assert_eq!(
                store.tile_by_key(&key).expect("the tile is open").title,
                "Chat bubble polish"
            );
            assert_eq!(store.sessions[0].title, "Chat bubble polish");
            let ws = store
                .workspaces
                .iter()
                .find(|ws| ws.folder_path() == folder)
                .expect("the workspace exists");
            let chat = ws
                .chats
                .iter()
                .find(|chat| chat.id == "probe-session")
                .expect("the sidebar row exists");
            assert_eq!(chat.title, "Chat bubble polish");
        });
    }

    /// A settled assistant message stamps both ends of its window, so the
    /// tile keeps its generated-token pace for the header readout; a later
    /// streaming snapshot must not wipe the number that is already there.
    #[gpui_kit::gpui::test]
    fn a_settled_reply_sets_the_header_pace(cx: &mut gpui::TestAppContext) {
        let _lock = super::test_support::lock_home();
        let home = HomeGuard::new();
        let folder = home.home.join("workspace");
        std::fs::create_dir_all(&folder).expect("workspace");
        let folder = folder.to_string_lossy().to_string();
        let store = cx.update(Store::for_probe);
        let key = cx.update(|cx| {
            store.update(cx, |store, cx| {
                store.add_workspace(&folder, cx);
                store.seed_probe_tile(&folder, "probe-session", "Pace", Vec::new(), false, cx)
            })
        });

        store.update(cx, |store, cx| {
            store.on_net_event(
                UiEvent::Event {
                    ws_key: folder.clone(),
                    event: json!({
                        "type": "message.updated",
                        "properties": {
                            "sessionID": "probe-session",
                            "info": {
                                "id": "msg_1",
                                "role": "assistant",
                                "providerID": "opencode",
                                "modelID": "deepseek-v4.1-flash",
                                "time": { "created": 1_000, "completed": 3_000 },
                                "tokens": { "input": 100, "output": 80, "reasoning": 20 },
                            },
                        },
                    }),
                },
                cx,
            );
        });

        store.read_with(cx, |store, _| {
            let pace = store
                .tile_by_key(&key)
                .and_then(|tile| tile.tok_s)
                .expect("a settled reply leaves a pace on the tile");
            assert_eq!(pace.generated, 100);
            assert!((pace.rate - 50.0).abs() < f64::EPSILON);
        });

        // the same message mid-stream (no completed stamp) keeps the number
        store.update(cx, |store, cx| {
            store.on_net_event(
                UiEvent::Event {
                    ws_key: folder.clone(),
                    event: json!({
                        "type": "message.updated",
                        "properties": {
                            "sessionID": "probe-session",
                            "info": {
                                "id": "msg_1",
                                "role": "assistant",
                                "time": { "created": 1_000 },
                                "tokens": { "output": 10 },
                            },
                        },
                    }),
                },
                cx,
            );
        });
        store.read_with(cx, |store, _| {
            let pace = store
                .tile_by_key(&key)
                .and_then(|tile| tile.tok_s)
                .expect("a streaming snapshot does not clear the pace");
            assert!((pace.rate - 50.0).abs() < f64::EPSILON);
        });
    }

    /// The dsh draft's first `/` must open the agent-scoped catalog: start the
    /// shared server, create the session, list commands, and route `/compact`
    /// through `commands/execute` instead of sending it as a model prompt.
    #[gpui_kit::gpui::test]
    fn a_dsh_draft_opens_its_catalog_and_runs_compact(cx: &mut gpui::TestAppContext) {
        let _lock = super::test_support::lock_home();
        let home = HomeGuard::new();
        let (tx, rx) = async_channel::unbounded();
        let store = cx.update(|cx| Store::new(cx, crate::oc::NetSender(tx)));
        let folder = home.home.join("workspace");
        std::fs::create_dir_all(&folder).expect("workspace");
        let folder = folder.to_string_lossy().to_string();

        let key = store.update(cx, |store, cx| {
            store.add_workspace(&folder, cx);
            let key = store.create_tile(cx).key.clone();
            store.focused_tile = Some(key.clone());
            if let Some(tile) = store.tile_by_key_mut(&key) {
                tile.harness = HarnessId::from("dsh");
            }
            store.refresh_commands(&key, cx);
            assert!(store.tile_commands_loading(&key));
            key
        });

        // no session yet: the menu starts the shared dsh server
        let mut started = false;
        while let Ok(command) = rx.try_recv() {
            if let NetCommand::StartServer {
                ws_key, harness, ..
            } = command
            {
                assert_eq!(ws_key, crate::harness::shared_server_key());
                assert_eq!(harness, HarnessId::from("dsh"));
                started = true;
                break;
            }
        }
        assert!(started, "the draft's first slash starts the dsh server");

        // the server is up now: its start drains the pending menu request
        store.update(cx, |store, cx| store.on_shared_server_started(cx));

        let mut created = false;
        while let Ok(command) = rx.try_recv() {
            if let NetCommand::Request {
                request:
                    ApiRequest::CreateCommandSession {
                        tile_key,
                        folder: requested,
                    },
                ..
            } = command
            {
                assert_eq!(tile_key, key);
                assert_eq!(requested, folder);
                created = true;
                break;
            }
        }
        assert!(created, "the menu asks for the session it needs");

        store.update(cx, |store, cx| {
            store.on_net_event(
                UiEvent::CommandSessionCreated {
                    tile_key: key.clone(),
                    ws_key: folder.clone(),
                    result: Ok(SessionInfo {
                        id: "ses_probe".into(),
                        title: "New chat".into(),
                        updated_ms: 1,
                        created_ms: 1,
                        parent_id: None,
                    }),
                },
                cx,
            );
        });

        let mut listed = false;
        while let Ok(command) = rx.try_recv() {
            if let NetCommand::Request {
                request: ApiRequest::ListAgentCommands { session_id },
                ..
            } = command
                && session_id.as_deref() == Some("ses_probe")
            {
                listed = true;
                break;
            }
        }
        assert!(listed, "the new session's catalog is requested");

        store.update(cx, |store, cx| {
            store.on_net_event(
                UiEvent::CommandsLoaded {
                    ws_key: crate::harness::shared_server_key(),
                    result: Ok(vec![CommandInfo {
                        name: "compact".into(),
                        name_lower: "compact".into(),
                        description: "Compact older conversation history".into(),
                        source: "command".into(),
                    }]),
                },
                cx,
            );
            assert!(!store.tile_commands_loading(&key));
            assert_eq!(
                store
                    .tile_commands(&key)
                    .iter()
                    .map(|command| command.name.as_str())
                    .collect::<Vec<_>>(),
                ["compact"]
            );
            if let Some(tile) = store.tile_by_key_mut(&key) {
                tile.input = "/compact".into();
            }
            store.send_from_tile(&key, cx);
        });

        let mut executed = false;
        while let Ok(command) = rx.try_recv() {
            if let NetCommand::Request {
                request: ApiRequest::ExecuteCommand { session_id, line },
                ..
            } = command
            {
                assert_eq!(session_id, "ses_probe");
                assert_eq!(line, "/compact");
                executed = true;
                break;
            }
        }
        assert!(executed, "/compact runs through commands/execute");
        store.read_with(cx, |store, _| {
            let tile = store.tile_by_key(&key).expect("tile");
            assert!(
                !tile
                    .items
                    .iter()
                    .any(|item| matches!(item.as_ref(), ChatItem::UserMessage { text, .. } if text == "/compact")),
                "a dsh command must not appear as a model message"
            );
        });
    }

    /// opencode's `/compact` is app-owned: it shows up before the server
    /// answers and goes to `/session/{id}/summarize`, not to the model or the
    /// `/command` route. An untouched draft must not create a session for it.
    #[gpui_kit::gpui::test]
    fn an_opencode_chat_offers_compact_and_runs_it(cx: &mut gpui::TestAppContext) {
        let _lock = super::test_support::lock_home();
        let home = HomeGuard::new();
        let (tx, rx) = async_channel::unbounded();
        let store = cx.update(|cx| Store::new(cx, crate::oc::NetSender(tx)));
        let folder = home.home.join("workspace");
        std::fs::create_dir_all(&folder).expect("workspace");
        let folder = folder.to_string_lossy().to_string();

        let key = store.update(cx, |store, cx| {
            store.add_workspace(&folder, cx);
            let key = store.create_tile(cx).key.clone();
            if let Some(tile) = store.tile_by_key_mut(&key) {
                tile.harness = HarnessId::from("opencode");
                tile.session_id = Some("ses_oc".into());
                tile.selected_model = Some("opencode-go/deepseek-v4.1-flash".into());
                tile.items.push(Arc::new(ChatItem::UserMessage {
                    key: 900,
                    text: "hi".into(),
                    created_ms: 0,
                }));
            }
            store
                .workspace_by_session
                .insert("ses_oc".into(), folder.clone());
            store.tile_by_session.insert("ses_oc".into(), key.clone());
            store
                .ui
                .harness_by_session
                .insert("ses_oc".into(), HarnessId::from("opencode"));
            store.runtimes.insert(
                folder.clone(),
                WorkspaceRuntime {
                    stream_status: oc::StreamStatus::Connected,
                    healthy: Some(true),
                },
            );
            store.refresh_commands(&key, cx);
            // the app-owned command is in the menu before GET /command lands
            assert!(
                store
                    .tile_commands(&key)
                    .iter()
                    .any(|command| command.name == "compact" && command.source == "local")
            );
            assert!(store.tile_commands_loading(&key));
            key
        });

        // a server answer without compact keeps the app-owned row exactly once
        store.update(cx, |store, cx| {
            store.on_net_event(
                UiEvent::CommandsLoaded {
                    ws_key: folder.clone(),
                    result: Ok(vec![CommandInfo {
                        name: "init".into(),
                        name_lower: "init".into(),
                        description: "guided AGENTS.md setup".into(),
                        source: "command".into(),
                    }]),
                },
                cx,
            );
            assert_eq!(
                store
                    .tile_commands(&key)
                    .iter()
                    .filter(|command| command.name == "compact")
                    .count(),
                1
            );
            if let Some(tile) = store.tile_by_key_mut(&key) {
                tile.input = "/compact".into();
            }
            store.send_from_tile(&key, cx);
        });

        let mut compacted = None;
        while let Ok(command) = rx.try_recv() {
            if let NetCommand::Request {
                request: ApiRequest::CompactSession { session_id, model },
                ..
            } = command
            {
                compacted = Some((session_id, model));
                break;
            }
        }
        assert_eq!(
            compacted,
            Some(("ses_oc".into(), "opencode-go/deepseek-v4.1-flash".into()))
        );
        store.update(cx, |store, cx| {
            store.on_net_event(
                UiEvent::SessionCompacted {
                    session_id: "ses_oc".into(),
                    result: Ok(()),
                },
                cx,
            );
            let tile = store.tile_by_key(&key).expect("tile");
            assert!(
                !tile
                    .items
                    .iter()
                    .any(|item| matches!(item.as_ref(), ChatItem::UserMessage { text, .. } if text == "/compact")),
                "a compact command must not appear as a model message"
            );
            assert!(tile.items.iter().any(|item| {
                matches!(item.as_ref(), ChatItem::SystemNote { text, .. } if text == "Context compacted.")
            }));
        });

        // an untouched draft has nothing to summarize: no session is created
        let draft = store.update(cx, |store, cx| {
            let draft = store.create_tile(cx).key.clone();
            if let Some(tile) = store.tile_by_key_mut(&draft) {
                tile.harness = HarnessId::from("opencode");
                tile.input = "/compact".into();
            }
            store.focused_tile = Some(draft.clone());
            store.send_from_tile(&draft, cx);
            let tile = store.tile_by_key(&draft).expect("draft");
            assert!(tile.items.iter().any(|item| {
                matches!(item.as_ref(), ChatItem::SystemNote { text, .. } if text == "Nothing to compact yet — send a message first.")
            }));
            draft
        });
        while let Ok(command) = rx.try_recv() {
            assert!(
                !matches!(
                    command,
                    NetCommand::Request {
                        request: ApiRequest::CreateSession { .. },
                        ..
                    }
                ),
                "an empty /compact must not create a session"
            );
        }
        let _ = draft;
    }

    #[gpui_kit::gpui::test]
    fn running_jobs_orders_chats_by_start_and_activity_follows(cx: &mut gpui::TestAppContext) {
        let _lock = super::test_support::lock_home();
        let home = HomeGuard::new();
        let folder = home.home.join("workspace");
        std::fs::create_dir_all(&folder).expect("workspace");
        let folder = folder.to_string_lossy().to_string();
        let store = cx.update(Store::for_probe);
        store.update(cx, |store, cx| {
            store.add_workspace(&folder, cx);
            // the panel titles chats from their visible tile titles
            store.seed_probe_tile(&folder, "session-one", "First chat", Vec::new(), true, cx);
            store.seed_probe_tile(&folder, "session-two", "Second chat", Vec::new(), true, cx);

            let jobs = store.running_jobs();
            assert_eq!(
                jobs.iter()
                    .map(|job| job.title.as_str())
                    .collect::<Vec<_>>(),
                vec!["First chat", "Second chat"],
                "the panel lists chats in the order they started"
            );
            assert_eq!(
                jobs.iter()
                    .map(|job| job.detail.as_deref())
                    .collect::<Vec<_>>(),
                // the workspace name comes from the folder's basename
                vec![Some("workspace"), Some("workspace")],
            );
            // several jobs count; one names itself
            assert_eq!(store.activity().as_deref(), Some("2 running"));
            store.set_running(Some("session-one"), false);
            assert_eq!(store.activity().as_deref(), Some("Second chat"));
            assert_eq!(
                store.running_jobs()[0].title,
                "Second chat",
                "the surviving row keeps the list without reshuffling the gap"
            );
            store.set_running(Some("session-two"), false);
            assert_eq!(store.activity(), None);
        });
    }
}

#[cfg(test)]
mod lineage_tests {
    use super::{
        HarnessId, HashMap, KeyMap, SubagentInfo, lineage_harness, lineage_workspace_path,
    };

    fn subagent(parent: &str) -> SubagentInfo {
        SubagentInfo {
            parent_session_id: parent.to_string(),
            agent: "Explore".to_string(),
        }
    }

    #[test]
    fn a_nested_subagent_climbs_to_the_root_workspace() {
        let mut workspaces = HashMap::new();
        workspaces.insert("session-root".to_string(), "/repo".to_string());
        let mut subagents = HashMap::new();
        subagents.insert("child".to_string(), subagent("session-root"));
        subagents.insert("grandchild".to_string(), subagent("child"));

        assert_eq!(
            lineage_workspace_path("grandchild", &workspaces, &subagents).as_deref(),
            Some("/repo"),
            "a depth-2 session answers with its root chat's folder"
        );
    }

    #[test]
    fn a_workspace_of_its_own_wins_over_the_lineage() {
        let mut workspaces = HashMap::new();
        workspaces.insert("session-root".to_string(), "/repo".to_string());
        workspaces.insert("child".to_string(), "/worktree".to_string());
        let mut subagents = HashMap::new();
        subagents.insert("child".to_string(), subagent("session-root"));

        assert_eq!(
            lineage_workspace_path("child", &workspaces, &subagents).as_deref(),
            Some("/worktree")
        );
    }

    #[test]
    fn an_unknown_session_has_no_folder() {
        assert_eq!(
            lineage_workspace_path("stray", &HashMap::new(), &HashMap::new()),
            None
        );
    }

    #[test]
    fn a_parent_cycle_stops_instead_of_looping() {
        let mut subagents = HashMap::new();
        subagents.insert("a".to_string(), subagent("b"));
        subagents.insert("b".to_string(), subagent("a"));

        assert_eq!(
            lineage_workspace_path("a", &HashMap::new(), &subagents),
            None,
            "the depth cap ends a malformed chain"
        );
    }

    #[test]
    fn a_subagent_inherits_its_owners_harness() {
        let mut harnesses = KeyMap::new();
        harnesses.insert("session-root".to_string(), HarnessId::from("dsh"));
        let mut subagents = HashMap::new();
        subagents.insert("child".to_string(), subagent("session-root"));
        subagents.insert("grandchild".to_string(), subagent("child"));

        assert_eq!(
            lineage_harness("grandchild", &harnesses, &subagents),
            HarnessId::from("dsh"),
            "an unmarked subagent follows the chat that spawned it"
        );
    }

    #[test]
    fn a_subagents_own_harness_wins_over_the_lineage() {
        let mut harnesses = KeyMap::new();
        harnesses.insert("session-root".to_string(), HarnessId::from("dsh"));
        harnesses.insert("child".to_string(), HarnessId::from("opencode"));
        let mut subagents = HashMap::new();
        subagents.insert("child".to_string(), subagent("session-root"));

        assert_eq!(
            lineage_harness("child", &harnesses, &subagents),
            HarnessId::from("opencode")
        );
    }
}

#[cfg(test)]
mod harness_settings_tests {
    use super::*;
    use crate::store::test_support::{HomeGuard, lock_home};

    #[gpui_kit::gpui::test]
    fn disabling_a_harness_falls_back_the_default_and_keeps_one(cx: &mut gpui::TestAppContext) {
        let _lock = lock_home();
        let _home = HomeGuard::new();
        let store = cx.update(Store::for_probe);
        store.update(cx, |store, _| {
            store.settings.default_harness = HarnessId::from("dsh");
        });
        store.update(cx, |store, cx| {
            store
                .set_harness_enabled("opencode", false, cx)
                .expect("opencode can be disabled while dsh is on");
        });
        store.read_with(cx, |store, _| {
            assert!(!store.harness_id_enabled("opencode"));
            assert_eq!(store.settings.default_harness, HarnessId::from("dsh"));
        });
        store.update(cx, |store, cx| {
            assert!(store.set_harness_enabled("dsh", false, cx).is_err());
            assert!(store.harness_id_enabled("dsh"));
        });
    }

    #[gpui_kit::gpui::test]
    fn a_disabled_harness_refuses_sends_with_a_note(cx: &mut gpui::TestAppContext) {
        let _lock = lock_home();
        let home = HomeGuard::new();
        let folder = home.home.join("workspace");
        std::fs::create_dir_all(&folder).expect("workspace");
        let folder = folder.to_string_lossy().to_string();
        let store = cx.update(Store::for_probe);
        let key = cx.update(|cx| {
            store.update(cx, |store, cx| {
                store.add_workspace(&folder, cx);
                store.seed_probe_tile(&folder, "s1", "Probe", Vec::new(), false, cx)
            })
        });
        store.update(cx, |store, cx| {
            if let Some(tile) = store.tile_by_key_mut(&key) {
                tile.input = "hello".into();
            }
            store
                .set_harness_enabled("opencode", false, cx)
                .expect("dsh keeps the app usable");
            store.send_from_tile(&key, cx);
        });
        store.read_with(cx, |store, _| {
            let tile = store.tile_by_key(&key).expect("the tile is open");
            assert!(tile.items.iter().any(|item| matches!(
                item.as_ref(),
                ChatItem::SystemNote { text, .. } if text.contains("disabled")
            )));
            assert!(!store.is_busy("s1"));
        });
    }
}
