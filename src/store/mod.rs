//! The full application state: workspaces, servers, the shared tile grid,
//! transcripts, queues, permissions, questions and subagent tracking. A 1:1
//! port of MainViewModel + ChatTileViewModel + the ChatItems model.

use crate::oc::{
    self, ApiRequest, CommandInfo, DSH_SERVER_KEY, ModelInfo, NetCommand, SessionInfo,
    StoredMessage, UiEvent,
};
use crate::settings::{AppSettings, DEFAULT_EXECUTABLE, HarnessKind, UiState, WorkspaceState};
pub mod ipc;
// the impl Store continuation modules, one per section
mod chat_flow;
mod events;
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
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
pub use tile::*;

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

/// The variants one harness/model pair reports; empty until that harness's
/// catalog lands (or when the model has no tiers).
fn variants_for_entry(model_infos: &[ModelInfo], harness: HarnessKind, model: &str) -> Vec<String> {
    model_infos
        .iter()
        .find(|entry| entry.harness == harness && entry.key == model)
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
    /// The harness whose settings refresh is waiting on `ModelsLoaded`; the
    /// picker's refresh button spins until the event clears it.
    pub models_refreshing: Option<HarnessKind>,
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
    ipc_models_waiters: Vec<(Option<HarnessKind>, crate::ipc::Reply)>,
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
            models_refreshing: None,
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
            hidden_sessions: HashSet::new(),
            deleted_sessions: ui.deleted_sessions.iter().cloned().collect(),
            ipc_models_waiters: Vec::new(),
            commands: HashMap::new(),
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

    /// The model keys one harness lists, in load order. The settings picker
    /// scopes its rows to the harness the segmented control shows; the
    /// merged `models` list cannot distinguish the two catalogs.
    pub fn model_keys_for(&self, harness: HarnessKind) -> Vec<String> {
        self.model_infos
            .iter()
            .filter(|model| model.harness == harness)
            .map(|model| model.key.clone())
            .collect()
    }

    /// The variants one harness/model pair reports. Empty until that
    /// harness's catalog lands (or when the model has no tiers), which the
    /// settings Variant picker explains instead of showing an empty list.
    pub fn variants_for(&self, harness: HarnessKind, model: &str) -> Vec<String> {
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
        self.ui.auto_allow_always = Some(self.auto_allow);
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

    /// The workspace an unsent draft will create its session on: the
    /// selected one, unless the draft belongs to a background door run —
    /// that run's own workspace, so a hidden send never rides the user's
    /// selection.
    fn draft_workspace(&self, tile_key: &str) -> Option<String> {
        self.ipc_runs
            .values()
            .find(|run| run.tile_key == tile_key)
            .map(|run| run.workspace.clone())
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

    /// Re-reads one harness's model catalog for the settings picker. Starts
    /// the backend when it is not up; `ModelsLoaded` refreshes the rows. The
    /// dsh path shares the one `dsh web` server; opencode uses the built-in
    /// General workspace, the same place the door's first models request goes.
    pub fn refresh_models(&mut self, harness: HarnessKind, cx: &mut Context<Self>) {
        // the picker's refresh button spins until the catalog lands
        self.models_refreshing = Some(harness);
        match harness {
            HarnessKind::Dsh => {
                let running = self.runtimes.contains_key(DSH_SERVER_KEY);
                self.ensure_dsh(cx);
                if running {
                    self.net.send(NetCommand::Request {
                        ws_key: DSH_SERVER_KEY.into(),
                        request: ApiRequest::ListModels,
                    });
                }
                // otherwise ServerStarted asks for the list on its own
            }
            HarnessKind::OpenCode => {
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
        }
        self.repaint(cx);
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
        let idle_min = self.settings.server_idle_stop_minutes;
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
        let executable = self.settings.opencode_executable.clone();
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

    /// Settings page applied: pushes persisted values back into live state.
    pub fn apply_saved_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(allowed) = self.settings.auto_allow_always {
            self.auto_allow = allowed;
        }
        self.ui.auto_allow_always = Some(self.auto_allow);
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
    use super::{HarnessKind, ModelInfo, variants_for_entry};

    fn info(key: &str, harness: HarnessKind, variants: &[&str]) -> ModelInfo {
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
            info("zai/glm", HarnessKind::OpenCode, &["low", "max"]),
            info("zai/glm", HarnessKind::Dsh, &["max"]),
            info("no/tiers", HarnessKind::OpenCode, &[]),
        ];
        assert_eq!(
            variants_for_entry(&infos, HarnessKind::OpenCode, "zai/glm"),
            ["low", "max"]
        );
        assert_eq!(
            variants_for_entry(&infos, HarnessKind::Dsh, "zai/glm"),
            ["max"]
        );
        assert!(variants_for_entry(&infos, HarnessKind::OpenCode, "no/tiers").is_empty());
        assert!(variants_for_entry(&infos, HarnessKind::OpenCode, "missing").is_empty());
    }
}
