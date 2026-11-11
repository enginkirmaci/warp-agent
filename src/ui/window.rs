//! The main window: titlebar, resizable sidebar with workspace/chat trees, the
//! shared tile grid, the settings page and the restore banner.

use crate::store::{Store, WorkspaceItem};
use crate::theme;
use crate::ui::settings::SettingsBuffers;
use crate::ui::tile::{TileDensity, TileView};
use crate::ui::tooltip::{self, tip};
use crate::ui::{BtnKind, menu_row, popover_card};
use gpui::prelude::*;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, MouseButton, Render, Role, ScrollStrategy,
    SharedString, StyleRefinement, UniformListScrollHandle, Window, WindowControlArea, canvas, div,
    px, uniform_list,
};
use gpui_kit::component::input::{InputEvent, InputState};
use std::collections::HashMap;
use std::rc::Rc;
use warpui_kit_core::A11y as _;
use warpui_kit_core::list_nav;
use warpui_kit_core::settings as kit;
use warpui_kit_core::shell;
use warpui_kit_core::text_menu::{self, TextMenuAnchor};
use warpui_kit_picker::{
    FilteredPicker, OverlayAnchor, PanelChrome, PickerBody, PickerData, PickerIcons, PickerItem,
    PickerRow, PickerSpec, PickerTarget, Selection, TriggerScale, model_name, model_provider,
};

/// The grid's gap, and its margin to the pane: the design's 12 gutter.
const GRID_GAP: f32 = 12.;

/// One sidebar row's pitch: a workspace or a chat, both one fixed line, so the
/// virtualized list can size itself without measuring.
const SIDEBAR_ROW_HEIGHT: f32 = 40.;

/// One flattened sidebar row: the virtualized list renders these by index.
#[derive(Clone)]
enum SidebarRow {
    Workspace(crate::store::WorkspaceItem),
    Chat(crate::store::SessionItem),
    /// The italic hint under a running workspace with no chats.
    Placeholder(&'static str),
    /// The quiet header over the folder workspaces; Tasks sits above it
    /// outside the section.
    SectionLabel(&'static str),
}

/// The row the sidebar's keyboard is on. A folder path or a chat id, so a
/// filter or a scope change can drop the cursor without confusing two rows.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarCursor {
    Workspace(String),
    Chat(String),
}

/// The sidebar's scope chips: which rows the list keeps once a chip is active.
/// All keeps the whole tree; the others narrow it to the chats (and starred
/// workspaces, for Favorites) that match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarScope {
    All,
    NeedsYou,
    Favorites,
    Running,
}

impl SidebarScope {
    fn label(self) -> &'static str {
        match self {
            SidebarScope::All => "All",
            SidebarScope::NeedsYou => "Needs you",
            SidebarScope::Favorites => "Favorites",
            SidebarScope::Running => "Running",
        }
    }

    fn key(self) -> &'static str {
        match self {
            SidebarScope::All => "all",
            SidebarScope::NeedsYou => "needs-you",
            SidebarScope::Favorites => "favorites",
            SidebarScope::Running => "running",
        }
    }

    /// The under-the-label line for a scope with nothing in it. All never
    /// needs one (Tasks always leads), but the match keeps the call total.
    fn empty_hint(self) -> &'static str {
        match self {
            SidebarScope::All => "No workspaces yet",
            SidebarScope::NeedsYou => "Nothing needs you",
            SidebarScope::Favorites => "No favorites yet",
            SidebarScope::Running => "Nothing running",
        }
    }
}

/// Whether one chat row belongs to a scope. `waiting` is the store's answer,
/// passed in so the rule stays pure and testable. Favorites is a
/// workspace-level scope, so every chat of a favorite workspace belongs.
fn chat_in_scope(chat: &crate::store::SessionItem, scope: SidebarScope, waiting: bool) -> bool {
    match scope {
        SidebarScope::All | SidebarScope::Favorites => true,
        SidebarScope::NeedsYou => waiting,
        SidebarScope::Running => chat.is_running,
    }
}

/// A workspace row stays in a scoped list when it matches the scope itself
/// (Favorites) or as the parent of matching chats (Needs you / Running).
fn workspace_in_scope(
    ws: &crate::store::WorkspaceItem,
    scope: SidebarScope,
    matched_chats: usize,
) -> bool {
    match scope {
        SidebarScope::All => true,
        SidebarScope::NeedsYou | SidebarScope::Running => matched_chats > 0,
        SidebarScope::Favorites => ws.is_favorite,
    }
}

/// The counts on the scope chips: every row for All, the favorite workspaces,
/// and the chats each status scope keeps.
fn sidebar_scope_counts(store: &Store) -> [(SidebarScope, usize); 4] {
    let mut all = store.workspaces.len();
    let mut need = 0;
    let mut favorites = 0;
    let mut running = 0;
    for ws in &store.workspaces {
        if ws.is_favorite {
            favorites += 1;
        }
        all += ws.chats.len();
        for chat in &ws.chats {
            if store.session_waiting(&chat.id) {
                need += 1;
            }
            if chat.is_running {
                running += 1;
            }
        }
    }
    [
        (SidebarScope::All, all),
        (SidebarScope::NeedsYou, need),
        (SidebarScope::Favorites, favorites),
        (SidebarScope::Running, running),
    ]
}

/// The Settings page's sections: the design's list of four, each with its own
/// group of rows in the pane beside it.
#[derive(Clone, Copy, Debug, PartialEq)]
enum SettingsSection {
    Appearance,
    ChatDefaults,
    Permissions,
    Server,
}

impl SettingsSection {
    const ALL: [SettingsSection; 4] = [
        SettingsSection::Appearance,
        SettingsSection::ChatDefaults,
        SettingsSection::Permissions,
        SettingsSection::Server,
    ];

    fn label(self) -> &'static str {
        match self {
            SettingsSection::Appearance => "Appearance",
            SettingsSection::ChatDefaults => "Chat defaults",
            SettingsSection::Permissions => "Permissions",
            SettingsSection::Server => "Server",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            SettingsSection::Appearance => "Palette, type and text size",
            SettingsSection::ChatDefaults => "Harness, model and variant",
            SettingsSection::Permissions => "Auto-allow and working hours",
            SettingsSection::Server => "Executables and idle stop",
        }
    }
}

/// The Chat defaults harness control: the key is the wire id (the callback
/// and the button's element id use it), the label is the product name the
/// button draws. Keeping this separate from the render is what the unit test
/// pins: a display label must never leak into the key.
fn harness_choices(current: crate::settings::HarnessKind) -> Vec<kit::Choice> {
    crate::settings::HarnessKind::ALL
        .iter()
        .map(|harness| {
            kit::Choice::new(harness.label(), harness.option_label(), *harness == current)
        })
        .collect()
}

/// One track of the grid inside `grid_width`.
fn tile_track_width(grid_width: f32, columns: usize) -> f32 {
    let columns = columns.max(1) as f32;
    (grid_width - GRID_GAP * (columns + 1.)) / columns
}

#[derive(Clone)]
pub enum SideMenuAction {
    OpenFolder(String),
    RemoveWorkspace(String),
    FavoriteWorkspace(String),
    UnfavoriteWorkspace(String),
    PinChat(String),
    UnpinChat(String),
    RenameChat(String),
    DeleteChat(String),
}

impl SideMenuAction {
    fn label(&self) -> &'static str {
        match self {
            SideMenuAction::OpenFolder(_) => "Open folder",
            SideMenuAction::RemoveWorkspace(_) => "Remove workspace",
            SideMenuAction::FavoriteWorkspace(_) => "Add to favorites",
            SideMenuAction::UnfavoriteWorkspace(_) => "Remove from favorites",
            SideMenuAction::PinChat(_) => "Pin",
            SideMenuAction::UnpinChat(_) => "Unpin",
            SideMenuAction::RenameChat(_) => "Rename",
            SideMenuAction::DeleteChat(_) => "Delete",
        }
    }
}

/// A context menu request produced by row closures (App-only context) and
/// consumed by the window render through a channel.
#[derive(Clone)]
pub struct MenuState {
    pub x: f32,
    pub y: f32,
    pub rows: Vec<SideMenuAction>,
}

/// Shaped width of one line in the UI face. The sidebar uses it to decide
/// whether a row still has room for its optional harness label before the
/// title has to truncate.
fn line_width(window: &Window, text: &str, font_size: f32, weight: gpui::FontWeight) -> f32 {
    // layout_line only lays out one line; a server title that slipped a
    // newline in is measured up to that break (the row clips it anyway)
    let text = text.split(['\n', '\r']).next().unwrap_or(text);
    if text.is_empty() {
        return 0.;
    }
    let mut font = gpui::font(theme::ui_font());
    font.weight = weight;
    let run = gpui::TextRun {
        len: text.len(),
        font,
        color: gpui::Hsla::default(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .layout_line(text, px(font_size), &[run], None)
        .width
        .as_f32()
}

/// The star of the station repos list, in a sidebar row's hover actions:
/// filled in the accent when the item is a favorite, quiet and dimmed until
/// the row is hovered otherwise. The click must stop propagation — the press
/// is not on the row.
fn star_toggle(
    id: gpui::ElementId,
    favorite: bool,
    icon_px: f32,
    row_group: &'static str,
    on_toggle: impl Fn(&mut gpui::App) + 'static,
) -> gpui::AnyElement {
    let label = if favorite {
        "Remove from favorites"
    } else {
        "Add to favorites"
    };
    tip(
        div()
            .id(id)
            .a11y_toggle(label, favorite)
            .cursor_pointer()
            .p_1()
            .rounded_sm()
            .hover(|s| s.bg(theme::HOVER))
            .when(!favorite, |star| {
                star.opacity(0.62).group_hover(row_group, |s| s.opacity(1.))
            })
            .on_click(move |_, _, cx| {
                // the star is not a press on the row
                cx.stop_propagation();
                on_toggle(cx);
            })
            .child(crate::ui::icon(
                if favorite {
                    "icons/star-filled.svg"
                } else {
                    "icons/star.svg"
                },
                px(icon_px),
                if favorite {
                    theme::accent_ink()
                } else {
                    theme::text_muted()
                },
            )),
        label,
    )
    .into_any_element()
}

/// Harness-independent workspace menu: servers are auto-managed (expanding a
/// workspace connects them), so the entries are the star and the folder.
/// Only folder rows open it — Tasks carries no menu — and the guard below
/// keeps the built-in workspace unremovable should a caller slip through.
fn ws_menu_rows(store: &Store, path: &str) -> Vec<SideMenuAction> {
    let Some(ws) = store.workspaces.iter().find(|w| w.folder_path() == path) else {
        return Vec::new();
    };
    let mut rows = vec![
        if ws.is_favorite {
            SideMenuAction::UnfavoriteWorkspace(path.into())
        } else {
            SideMenuAction::FavoriteWorkspace(path.into())
        },
        SideMenuAction::OpenFolder(path.to_string()),
    ];
    if !ws.is_general() {
        rows.push(SideMenuAction::RemoveWorkspace(path.to_string()));
    }
    rows
}

fn chat_menu_rows(store: &Store, id: &str) -> Vec<SideMenuAction> {
    let pinned = store.ui.pins.contains_key(id);
    vec![
        if pinned {
            SideMenuAction::UnpinChat(id.into())
        } else {
            SideMenuAction::PinChat(id.into())
        },
        SideMenuAction::RenameChat(id.into()),
        SideMenuAction::DeleteChat(id.into()),
    ]
}

pub struct WarpWindow {
    pub store: Entity<Store>,
    tile_views: HashMap<String, Entity<TileView>>,
    /// the app's tooltip overlay (also reachable via the TooltipHost global);
    /// rendered as the last child so popups paint above everything
    tooltip_overlay: Entity<gpui_kit::base::TooltipOverlay>,
    /// the read-only text menu's action anchor; rendered once at the root
    text_menu: Entity<TextMenuAnchor>,
    menu_sender_cell: async_channel::Sender<MenuState>,
    menu_receiver: async_channel::Receiver<MenuState>,
    menu: Option<MenuState>,
    add_folder_sender: async_channel::Sender<String>,
    add_folder_receiver: async_channel::Receiver<String>,
    settings_buffers: SettingsBuffers,
    /// The Settings page's open section.
    settings_section: SettingsSection,
    /// About: which suite member the detail shows; the app you are in until
    /// another is picked.
    pub(crate) about_selected: warpui_kit_core::about::AppId,
    /// The license card is up over the About screen.
    pub(crate) about_license_open: bool,
    /// The license document, built the first time the card opens: read-only
    /// markdown, so the GPL reads as prose and its text is selectable.
    pub(crate) about_license: Option<gpui::Entity<warpui_kit_markdown::LiveEditorState>>,
    /// The harness the Chat defaults page is configuring: the segmented
    /// control writes it and the model picker scopes its catalog to it.
    settings_harness: Entity<crate::ui::settings::HarnessChoice>,
    /// E2E hook: WARP_AGENT_PAGE=settings[-chat|-permissions|-server] opens
    /// the rail's Settings section after the first window exists (the pointer
    /// cannot reach a hidden window, and probes need the page).
    open_settings_on_first_render: Option<SettingsSection>,
    /// E2E hook: WARP_AGENT_PAGE=about opens the rail's About section, for the
    /// same reason.
    open_about_on_first_render: bool,
    settings_fields: Vec<(&'static str, Entity<InputState>)>,
    /// The pending debounced settings commit: every field keystroke bumps it,
    /// the task that fired for an older sequence stands down.
    settings_commit_seq: usize,
    /// The last sequence a commit actually landed for: leaving the settings
    /// page flushes only a commit that is still pending.
    settings_committed_seq: usize,
    /// leaf renderers for the plain settings fields — keystrokes repaint
    /// only the field, never the window (see InputLeaf below)
    settings_leafs: Vec<(&'static str, Entity<InputLeaf>)>,
    /// the Model field + its standalone picker component
    settings_model_picker: Option<Entity<FilteredPicker>>,
    /// the Variant field + its standalone picker component; scoped to the
    /// selected model's variants
    settings_variant_picker: Option<Entity<FilteredPicker>>,
    /// session being renamed + its edit field (state for the Enter
    /// subscription, leaf for the per-keystroke renderer)
    rename: Option<(String, Entity<InputState>, Entity<InputLeaf>)>,
    /// workspace-name filter text; keystrokes land in the sidebar filter
    /// leaf (InputLeaf) and only a Change event copies the text out and
    /// nudges the window to refilter the list
    sidebar_filter: String,
    /// the scope chip under the filter field: All, Needs you, Favorites or
    /// Running. Kept in the window, like the filter text, so it is a view
    /// state and never touches the saved layout.
    sidebar_scope: SidebarScope,
    /// the filter input isolated in a leaf so typing doesn't repaint the
    /// whole window (created on first sidebar render — InputState needs
    /// the window)
    sidebar_filter_leaf: Option<Entity<InputLeaf>>,
    /// sidebar relative-time labels cached per chat (updated_ms → label);
    /// entries older than the current minute bucket are rebuilt wholesale —
    /// the label text changes at most once a minute, so the store's
    /// chrono-format runs once per minute per chat instead of per repaint
    sidebar_labels: HashMap<String, (i64, SharedString)>,
    sidebar_labels_minute: i64,
    /// active sidebar resize: (start pointer x, start sidebar width); shared with
    /// the paint-phase listener registration because window.on_mouse_event may
    /// only be called during paint in this gpui rev
    splitter_drag: std::rc::Rc<std::cell::Cell<Option<(gpui::Pixels, f32)>>>,
    /// the chat row's trash button that is one click from deleting: its chat
    /// id. First click arms (the icon turns red), the second deletes — the
    /// same arm-then-run dance the ⋯ menu's Delete row plays.
    delete_armed: Option<String>,
    /// The sidebar's keyboard cursor and focus, and the offset that keeps a
    /// stepped row in view.
    sidebar_cursor: Option<SidebarCursor>,
    sidebar_focus: FocusHandle,
    sidebar_scroll: UniformListScrollHandle,
    /// The OS title the last frame set: the same activity line as the
    /// titlebar chip, written only when it changes.
    window_title: String,
}

impl WarpWindow {
    pub fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        let (menu_tx, menu_rx) = async_channel::unbounded::<MenuState>();
        let (folder_tx, folder_rx) = async_channel::unbounded::<String>();
        let settings_buffers = SettingsBuffers::from_settings(&store.read(cx).settings);
        let settings_harness =
            cx.new(|_| crate::ui::settings::HarnessChoice(settings_buffers.harness()));
        let font_meta = store.read(cx).fonts.font_meta;
        let tooltip_overlay = tooltip::install(cx, font_meta);
        let text_menu = text_menu::install(cx);
        Self {
            store,
            tile_views: HashMap::new(),
            tooltip_overlay,
            text_menu,
            menu_sender_cell: menu_tx,
            menu_receiver: menu_rx,
            menu: None,
            add_folder_sender: folder_tx,
            add_folder_receiver: folder_rx,
            settings_buffers,
            settings_section: SettingsSection::Appearance,
            about_selected: warpui_kit_core::about::AppId::Agent,
            about_license_open: false,
            about_license: None,
            settings_harness,
            open_settings_on_first_render: match std::env::var("WARP_AGENT_PAGE").as_deref() {
                Ok("settings") => Some(SettingsSection::Appearance),
                Ok("settings-chat") => Some(SettingsSection::ChatDefaults),
                Ok("settings-permissions") => Some(SettingsSection::Permissions),
                Ok("settings-server") => Some(SettingsSection::Server),
                _ => None,
            },
            open_about_on_first_render: matches!(
                std::env::var("WARP_AGENT_PAGE").as_deref(),
                Ok("about")
            ),
            settings_fields: Vec::new(),
            settings_commit_seq: 0,
            settings_committed_seq: 0,
            settings_leafs: Vec::new(),
            settings_model_picker: None,
            settings_variant_picker: None,
            rename: None,
            sidebar_filter: String::new(),
            sidebar_scope: SidebarScope::All,
            sidebar_filter_leaf: None,
            sidebar_labels: HashMap::new(),
            sidebar_labels_minute: -1,
            splitter_drag: std::rc::Rc::new(std::cell::Cell::new(None)),
            delete_armed: None,
            sidebar_cursor: None,
            sidebar_focus: cx.focus_handle(),
            sidebar_scroll: UniformListScrollHandle::default(),
            window_title: String::new(),
        }
    }
}

/// One stable key per menu action, so arming a destructive row survives the
/// re-render that shows Confirm.
fn menu_action_key(action: &SideMenuAction) -> String {
    match action {
        SideMenuAction::OpenFolder(path) => format!("open-folder:{path}"),
        SideMenuAction::RemoveWorkspace(path) => format!("remove-workspace:{path}"),
        SideMenuAction::FavoriteWorkspace(path) => format!("favorite-ws:{path}"),
        SideMenuAction::UnfavoriteWorkspace(path) => format!("unfavorite-ws:{path}"),
        SideMenuAction::PinChat(id) | SideMenuAction::UnpinChat(id) => format!("pin:{id}"),
        SideMenuAction::RenameChat(id) => format!("rename:{id}"),
        SideMenuAction::DeleteChat(id) => format!("delete-chat:{id}"),
    }
}

fn execute_menu_action(store: &Entity<Store>, action: &SideMenuAction, cx: &mut App) {
    store.update(cx, |s, cx| match action {
        SideMenuAction::OpenFolder(path) => {
            if let Err(err) = crate::services::open_folder(path) {
                tracing::warn!("open folder failed: {err}");
            }
        }
        SideMenuAction::RemoveWorkspace(path) => s.remove_workspace(path, cx),
        SideMenuAction::FavoriteWorkspace(path) => s.toggle_workspace_favorite(path, cx),
        SideMenuAction::UnfavoriteWorkspace(path) => s.toggle_workspace_favorite(path, cx),
        SideMenuAction::PinChat(id) => s.toggle_pin(id, cx),
        SideMenuAction::UnpinChat(id) => s.toggle_pin(id, cx),
        SideMenuAction::RenameChat(id) => s.begin_rename(id, cx),
        SideMenuAction::DeleteChat(id) => s.delete_session(id, cx),
    });
}

impl Render for WarpWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let running = {
            let store = self.store.read(cx);
            store
                .open_tiles
                .iter()
                .filter(|tile| !tile.background && tile.is_busy(store))
                .count()
        };
        let activity = (running > 0).then(|| format!("{running} running"));
        let title = warpui_kit_core::activity::activity_title("Warp Agent", activity.as_deref());
        if self.window_title != title {
            window.set_window_title(&title);
            self.window_title = title;
        }
        if let Some(section) = self.open_settings_on_first_render.take() {
            self.open_settings(window, cx);
            self.settings_section = section;
        }
        if self.open_about_on_first_render {
            self.open_about_on_first_render = false;
            self.open_about(cx);
        }
        // pending folder picks + menu requests from App-only closures
        while let Ok(path) = self.add_folder_receiver.try_recv() {
            self.store.update(cx, |s, cx| s.add_workspace(&path, cx));
        }
        while let Ok(menu) = self.menu_receiver.try_recv() {
            self.menu = Some(menu);
            // a fresh menu starts unarmed: an old Confirm state belongs to a
            // menu that is no longer on screen. The row trash's arm resets
            // with it — an armed button behind an open menu is a trap.
            self.delete_armed = None;
            self.store.update(cx, |s, _| s.menu_armed = None);
        }

        // rename field lifecycle: create when a row enters edit, drop when it leaves
        let (editing, draft) = {
            let store = self.store.read(cx);
            store
                .workspaces
                .iter()
                .flat_map(|w| w.chats.iter())
                .find(|c| c.is_editing)
                .map(|c| (Some(c.id.clone()), Some(c.draft_title.clone())))
                .unwrap_or((None, None))
        };
        self.sync_rename_field(editing, draft, window, cx);

        let fonts = self.store.read(cx).fonts;
        let sidebar_visible = self.store.read(cx).sidebar_visible;
        // Settings brings its own section list; the chat tree steps aside
        let settings_open = self.store.read(cx).is_settings_open;
        let page_open = settings_open || self.store.read(cx).is_about_open;

        let list_shown = sidebar_visible && !page_open && !self.grid_takes_the_list(window, cx);
        let mut content = shell::panes().child(self.render_rail(window, cx));
        if list_shown {
            content = content.child(self.render_sidebar(window, cx));
            content = content.child(self.render_splitter());
        }
        content = content.child(self.render_main(list_shown, window, cx));

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::window_chrome())
            .text_color(theme::text())
            .font_family(theme::ui_font())
            // the shell's Ctrl+\ hides the list; the title bar button does the same
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                let modifiers = event.keystroke.modifiers;
                if modifiers.control && !modifiers.shift && event.keystroke.key == "\\" {
                    let visible = this.store.read(cx).sidebar_visible;
                    this.store
                        .update(cx, |s, cx| s.set_sidebar_visible(!visible, cx));
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            // mounts the window's selection machinery: the diff bodies and
            // markdown views register with it for drag selection and copy
            .child(gpui_kit::base::TextSelectionLayer)
            .child(self.text_menu.clone())
            .child(self.render_titlebar(window, cx))
            .child(content);

        // context menu overlay
        if let Some(menu) = self.menu.clone() {
            let store_handle = self.store.clone();
            let mut rows = div().flex().flex_col();
            for (i, row) in menu.rows.iter().enumerate() {
                let action = row.clone();
                let destructive = matches!(
                    action,
                    SideMenuAction::DeleteChat(_) | SideMenuAction::RemoveWorkspace(_)
                );
                let key = menu_action_key(&action);
                let armed = destructive
                    && self
                        .store
                        .read(cx)
                        .menu_armed
                        .as_deref()
                        .is_some_and(|k| k == key);
                rows = rows.child(menu_row(
                    gpui::ElementId::NamedInteger("menu".into(), i as u64),
                    if armed { "Confirm" } else { action.label() },
                    destructive,
                    fonts.font_heading,
                    {
                        let store_handle = store_handle.clone();
                        let key = key.clone();
                        move |_, _, cx| {
                            // the design's second click: a destructive row arms
                            // in place, the next click on it runs
                            let already = store_handle
                                .read(cx)
                                .menu_armed
                                .as_deref()
                                .is_some_and(|k| k == key);
                            if destructive && !already {
                                store_handle.update(cx, |s, cx| {
                                    s.menu_armed = Some(key.clone());
                                    cx.notify();
                                });
                                return;
                            }
                            store_handle.update(cx, |s, _| s.menu_armed = None);
                            execute_menu_action(&store_handle, &action, cx);
                        }
                    },
                ));
            }
            root = root.child(
                div()
                    .id("menu-catcher")
                    .absolute()
                    .inset_0()
                    // hitbox hover is geometric in gpui: without occlusion the
                    // sidebar row under a menu item receives the same click
                    .occlude()
                    // dismiss on full click, not mouse-down: a down-handler here
                    // unmounts the menu rows before their on_click (mouse-up) runs
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        this.store.update(cx, |s, _| s.menu_armed = None);
                        cx.notify();
                    }))
                    .child(
                        div().absolute().left(px(menu.x)).top(px(menu.y)).child(
                            popover_card()
                                .id("menu-card")
                                .min_w(px(180.))
                                // A row click bubbles up here; stop it before
                                // the full-screen catcher sees it. A destructive
                                // row's first click only arms the row (Confirm),
                                // so keep the menu mounted then; any other action
                                // has run by now and the menu closes.
                                .on_click(cx.listener(|this, _, _, cx| {
                                    cx.stop_propagation();
                                    if this.store.read(cx).menu_armed.is_none() {
                                        this.menu = None;
                                        cx.notify();
                                    }
                                }))
                                .child(rows),
                        ),
                    ),
            );
        }

        if self.about_license_open {
            let editor = self.about_license.get_or_insert_with(|| {
                warpui_kit_markdown::read_only_document(
                    crate::ui::about::LICENSE.into(),
                    window,
                    cx,
                )
            });
            let body = warpui_kit_markdown::render_read_only_document_in_card(editor, cx);
            root = root.child(warpui_kit_core::about::license_dialog(
                "about-license",
                "GNU General Public License v3.0",
                body,
                cx.listener(|this, _, _, cx| {
                    this.about_license_open = false;
                    cx.notify();
                }),
            ));
        }

        // tooltips paint above everything, menus included
        root.child(self.tooltip_overlay.clone())
    }
}

/// The settings-page composer state: labeled buffers, labeled leaves and the
/// two catalog pickers (Model and its dependent Variant).
struct SettingsFields {
    fields: Vec<(&'static str, Entity<InputState>)>,
    leafs: Vec<(&'static str, Entity<InputLeaf>)>,
    model_picker: Option<Entity<FilteredPicker>>,
    variant_picker: Option<Entity<FilteredPicker>>,
}

impl WarpWindow {
    fn sync_rename_field(
        &mut self,
        editing: Option<String>,
        draft: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match (editing, &self.rename) {
            (Some(id), None) => {
                let entity = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("")
                        .default_value(draft.clone().unwrap_or_default())
                });
                let store_handle = self.store.clone();
                let id_enter = id.clone();
                cx.subscribe(&entity, move |_, field, event: &InputEvent, cx| {
                    // Enter commits at once; losing focus commits too, so
                    // clicking another row does not throw the edit away
                    if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        let text = field.read(cx).value().to_string();
                        let id = id_enter.clone();
                        store_handle.update(cx, |s, cx| {
                            s.set_rename_draft(&id, text);
                            s.commit_rename(&id, cx);
                        });
                    }
                })
                .detach();
                // per-keystroke renderer as its own leaf: typing in the
                // rename field must not re-render the window
                let leaf = cx.new(|_| InputLeaf {
                    state: entity.clone(),
                    mono: false,
                    icon: None,
                });
                // editing starts focused: the row's ⋯ -> Rename should put the
                // caret in the title, not ask for a second click
                let focus = entity.read(cx).focus_handle(cx);
                window.focus(&focus, cx);
                self.rename = Some((id, entity, leaf));
            }
            (None, Some(_)) => {
                self.rename = None;
            }
            _ => {}
        }
    }

    /// Below the compact width, a grid of two or more takes the list's room.
    fn grid_takes_the_list(&self, window: &Window, cx: &App) -> bool {
        window.viewport_size().width < px(shell::scaled(shell::COMPACT_WINDOW_WIDTH))
            && self
                .store
                .read(cx)
                .open_tiles
                .iter()
                .filter(|t| !t.background)
                .count()
                >= 2
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.store.read(cx).is_settings_open {
            return;
        }
        self.close_about(cx);
        self.store.update(cx, |s, _| {
            s.is_settings_open = true;
        });
        self.settings_buffers = SettingsBuffers::from_settings(&self.store.read(cx).settings);
        let harness = self.settings_buffers.harness();
        self.settings_harness.update(cx, |scope, cx| {
            scope.0 = harness;
            cx.notify();
        });
        let settings = self.make_settings_fields(window, cx);
        self.settings_fields = settings.fields;
        self.settings_leafs = settings.leafs;
        self.settings_model_picker = settings.model_picker;
        self.settings_variant_picker = settings.variant_picker;
        cx.notify();
    }

    /// Leaving the page flushes a pending field commit — the page has no
    /// Save, so the debounced write must not be lost to the section switch.
    pub(crate) fn close_settings(&mut self, cx: &mut Context<Self>) {
        if !self.store.read(cx).is_settings_open {
            return;
        }
        if self.settings_commit_seq != self.settings_committed_seq {
            self.commit_settings(cx);
            self.settings_committed_seq = self.settings_commit_seq;
        }
        self.store.update(cx, |s, _| s.is_settings_open = false);
        cx.notify();
    }

    fn render_rail(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (in_settings, in_about, ink, attention) = {
            let store = self.store.read(cx);
            (
                store.is_settings_open,
                store.is_about_open,
                store.accent.text,
                store.has_attention_needed(),
            )
        };
        shell::rail()
            .debug_selector(|| "app-rail".into())
            .child(
                shell::rail_item_inked(
                    "rail-chats",
                    "icons/chat-outline.svg",
                    "Chats",
                    !in_settings && !in_about,
                    ink,
                )
                .debug_selector(|| "rail-chats".into())
                .relative()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.close_settings(cx);
                    this.close_about(cx);
                }))
                .when(attention, |d| {
                    d.child(
                        div()
                            .absolute()
                            .top_1p5()
                            .right(px(12.))
                            .size(px(8.))
                            .rounded_full()
                            .bg(theme::CHANGES),
                    )
                }),
            )
            .child(div().flex_1())
            .child(
                shell::rail_item_inked("open-about", "icons/about.svg", "About", in_about, ink)
                    .debug_selector(|| "rail-about".into())
                    .on_click(cx.listener(|this, _, _, cx| this.open_about(cx))),
            )
            .child(
                shell::rail_item_inked(
                    "open-settings",
                    "icons/cog.svg",
                    "Settings",
                    in_settings,
                    ink,
                )
                .debug_selector(|| "rail-settings".into())
                .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
            )
            .into_any_element()
    }

    fn render_titlebar(&mut self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (fonts, visible) = {
            let store = self.store.read(cx);
            (store.fonts, store.sidebar_visible)
        };
        // gauge color: reconnecting beats unhealthy beats the neutral chrome;
        // healthy and unknown both stay muted, like the plain window glyph
        let gauge_color = {
            let store = self.store.read(cx);
            if store.any_stream_reconnecting() {
                theme::WARN
            } else if store.any_stream_unhealthy() {
                theme::DANGER
            } else {
                theme::text_muted()
            }
        };
        let store_handle = self.store.clone();
        let running = {
            let store = self.store.read(cx);
            store
                .open_tiles
                .iter()
                .filter(|t| !t.background && t.is_busy(store))
                .count()
        };
        shell::title_bar()
            .pl_3()
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(28.))
                    .rounded(px(6.))
                    .bg(theme::brand_bg())
                    // the app's own mark, in the family's currentColor glyph —
                    // a plain accent tile like the siblings', no ring
                    .child(crate::ui::icon(
                        "icons/logo.svg",
                        px(fonts.icon_small),
                        theme::accent_ink(),
                    )),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(fonts.font_brand))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child("Warp Agent"),
            )
            // the family: the sibling apps this window can open, right after
            // its own mark and name, behind one separator
            .child(
                shell::app_links().child(tip(
                    shell::app_link(
                        "launch-station",
                        "icons/logo-station.svg",
                        "Open Warp Station",
                    )
                    .on_click(|_, _, _| {
                        if let Err(err) = crate::services::launch_sibling("warp-station") {
                            tracing::warn!("could not launch warp-station: {err}");
                        }
                    }),
                    "Open Warp Station",
                )),
            )
            .child(
                div()
                    .id("titlebar-drag")
                    .flex_1()
                    .h_full()
                    // Windows drags via the hit-test path (this hitbox answers
                    // HTCAPTION); start_window_move is a no-op there. Linux
                    // keeps the explicit move call.
                    .window_control_area(WindowControlArea::Drag)
                    .on_mouse_down(MouseButton::Left, |_, window, _| {
                        window.start_window_move();
                    }),
            )
            .when(running > 0, |d| {
                d.child(tip(
                    div()
                        .id("activity-chip")
                        .debug_selector(|| "activity-chip".into())
                        .role(Role::Status)
                        .aria_label(SharedString::from(format!("{running} running")))
                        .flex_shrink_0()
                        .h(px(28.))
                        .px_2p5()
                        .rounded(px(14.))
                        .bg(theme::panel())
                        .flex()
                        .items_center()
                        .gap(px(7.))
                        .text_size(px(fonts.font_meta))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme::BEHIND)
                        .child(warpui_kit_core::motion::spinner(
                            "activity-spinner",
                            "icons/spinner.svg",
                            px(fonts.icon_tiny),
                            theme::BEHIND,
                        ))
                        .child(format!("{running} running")),
                    format!("{running} running"),
                ))
            })
            .child(
                tip(
                    div()
                        .id("toggle-sidebar")
                        .role(Role::Button)
                        .aria_label(if visible {
                            "Hide sidebar"
                        } else {
                            "Show sidebar"
                        })
                        .cursor_pointer()
                        .w(px(36.))
                        .h(px(shell::scaled(shell::TITLE_BAR_HEIGHT)))
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|s| s.bg(theme::raised()))
                        .on_click(move |_, _, cx| {
                            store_handle
                                .update(cx, |s, cx| s.set_sidebar_visible(!s.sidebar_visible, cx));
                        }),
                    if visible {
                        "Hide sidebar"
                    } else {
                        "Show sidebar"
                    },
                )
                .child(if visible {
                    crate::ui::icon(
                        "icons/panel-left.svg",
                        px(fonts.icon_medium),
                        theme::text_muted(),
                    )
                } else {
                    crate::ui::icon(
                        "icons/panel-left-open.svg",
                        px(fonts.icon_medium),
                        theme::text_muted(),
                    )
                }),
            )
            .child(div().w(px(1.)).h(px(20.)).mx_1().bg(theme::hairline()))
            .child(
                tip(
                    div()
                        .id("minimize-window")
                        .role(Role::Button)
                        .aria_label("Minimize")
                        .cursor_pointer()
                        .w(px(36.))
                        .h(px(shell::scaled(shell::TITLE_BAR_HEIGHT)))
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|s| s.bg(theme::raised()))
                        .on_click(|_, window, _| window.minimize_window()),
                    "Minimize",
                )
                .child(crate::ui::icon(
                    "icons/dash.svg",
                    px(fonts.icon_medium),
                    theme::text_muted(),
                )),
            )
            .child(
                tip(
                    div()
                        .id("zoom")
                        .role(Role::Button)
                        .aria_label("Zoom · server health")
                        .cursor_pointer()
                        .w(px(36.))
                        .h(px(shell::scaled(shell::TITLE_BAR_HEIGHT)))
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|s| s.bg(theme::raised()))
                        .on_click(cx.listener(|_, _, window, cx| {
                            window.zoom_window();
                            // the compositor toggles asynchronously — nudge a
                            // re-render so the maximize/restore glyph keeps up
                            cx.notify();
                        })),
                    "Zoom · server health",
                )
                // the family's window-state glyph; its stroke doubles as the
                // server-health gauge: amber reconnecting, salmon unhealthy,
                // neutral otherwise
                .child(shell::zoom_glyph(window.is_maximized(), gauge_color)),
            )
            .child(
                tip(
                    div()
                        .id("close-window")
                        .role(Role::Button)
                        .aria_label("Close")
                        .cursor_pointer()
                        .w(px(36.))
                        .h(px(shell::scaled(shell::TITLE_BAR_HEIGHT)))
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|s| s.bg(theme::tint(theme::DANGER)))
                        .on_click(|_, window, _| window.remove_window()),
                    "Close",
                )
                .child(crate::ui::icon(
                    "icons/close.svg",
                    px(fonts.icon_medium),
                    theme::text_muted(),
                )),
            )
            .into_any_element()
    }

    /// The sidebar's focus handle, for the real-view probe.
    #[doc(hidden)]
    pub fn sidebar_focus_handle(&self) -> FocusHandle {
        self.sidebar_focus.clone()
    }

    /// `workspace:<path>` or `chat:<id>`: the private cursor enum stays
    /// private while a probe can still assert where the keyboard is.
    #[doc(hidden)]
    pub fn sidebar_cursor_for_probe(&self) -> Option<String> {
        self.sidebar_cursor.as_ref().map(|cursor| match cursor {
            SidebarCursor::Workspace(path) => format!("workspace:{path}"),
            SidebarCursor::Chat(id) => format!("chat:{id}"),
        })
    }

    /// The sidebar filter's focus handle, so the probe can prove the list
    /// cursor stays put while a field owns the keys.
    #[doc(hidden)]
    pub fn sidebar_filter_focus_for_probe(&self, cx: &App) -> Option<FocusHandle> {
        self.sidebar_filter_leaf.as_ref().map(|leaf| {
            let leaf = leaf.read(cx);
            leaf.state.read(cx).focus_handle(cx)
        })
    }

    /// The sidebar's row keys, dispatched by the focused list only. The
    /// cursor is separate from the open chat: `Enter` activates the row, an
    /// arrow only moves the keyboard's place.
    fn handle_sidebar_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if event.keystroke.modifiers.modified() {
            return false;
        }
        let Some(key) = list_nav::list_key(event) else {
            return false;
        };
        let rows = self.sidebar_rows(&self.sidebar_filter, self.sidebar_scope, cx);
        let entries: Vec<(SidebarCursor, usize)> = rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| match row {
                SidebarRow::Workspace(workspace) => Some((
                    SidebarCursor::Workspace(workspace.folder_path().to_string()),
                    index,
                )),
                SidebarRow::Chat(chat) => Some((SidebarCursor::Chat(chat.id.clone()), index)),
                SidebarRow::Placeholder(_) | SidebarRow::SectionLabel(_) => None,
            })
            .collect();
        match key {
            list_nav::ListKey::Up
            | list_nav::ListKey::Down
            | list_nav::ListKey::Home
            | list_nav::ListKey::End
            | list_nav::ListKey::PageUp
            | list_nav::ListKey::PageDown => {
                let current = self
                    .sidebar_cursor
                    .as_ref()
                    .and_then(|cursor| entries.iter().position(|(entry, _)| entry == cursor));
                let Some(next) =
                    list_nav::step(current, entries.len(), key, list_nav::DEFAULT_PAGE)
                else {
                    return true;
                };
                let (cursor, row) = entries[next].clone();
                self.sidebar_cursor = Some(cursor);
                self.sidebar_scroll
                    .scroll_to_item(row, ScrollStrategy::Nearest);
                cx.notify();
                true
            }
            list_nav::ListKey::Enter => {
                let Some(cursor) = self.sidebar_cursor.clone() else {
                    return false;
                };
                self.activate_sidebar_cursor(cursor, cx);
                true
            }
            list_nav::ListKey::Left | list_nav::ListKey::Right | list_nav::ListKey::Menu => false,
        }
    }

    /// Runs the cursor row's click: a chat opens, a folder selects and folds
    /// the way its row does.
    fn activate_sidebar_cursor(&mut self, cursor: SidebarCursor, cx: &mut Context<Self>) {
        match cursor {
            SidebarCursor::Chat(id) => {
                self.store
                    .update(cx, |store, cx| store.select_session(&id, cx));
            }
            SidebarCursor::Workspace(path) => {
                self.store.update(cx, |store, cx| {
                    store.select_workspace(&path, cx);
                    let expanding = store
                        .workspaces
                        .iter_mut()
                        .find(|workspace| workspace.folder_path() == path)
                        .map(|workspace| {
                            let expanding = !workspace.is_expanded();
                            workspace.set_expanded(expanding);
                            expanding
                        })
                        .unwrap_or(false);
                    if expanding {
                        store.connect_expanded_workspace(&path, cx);
                    }
                    store.save_state();
                    cx.notify();
                });
            }
        }
    }

    // ---- sidebar ---------------------------------------------------------------

    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (fonts, width) = {
            let store = self.store.read(cx);
            (store.fonts, store.sidebar_shown_width)
        };
        // the filter input is created on first sidebar render (InputState
        // needs the window) and renders from a leaf: keystrokes repaint only
        // the leaf, a Change event copies the text out and nudges the window
        // to refilter the list
        if self.sidebar_filter_leaf.is_none() {
            let state = cx.new(|cx| InputState::new(window, cx).placeholder("Filter workspaces"));
            cx.subscribe(&state, |this: &mut Self, field, event: &InputEvent, cx| {
                if let InputEvent::Change = event {
                    this.sidebar_filter = field.read(cx).value().to_string();
                    cx.notify();
                }
            })
            .detach();
            self.sidebar_filter_leaf = Some(cx.new(|_| InputLeaf {
                state,
                // the prototype's filter is prose with a search icon, not a
                // mono value field
                mono: false,
                icon: Some("icons/search.svg"),
            }));
        }
        let filter = self.sidebar_filter.to_lowercase();
        let filter_leaf = self.sidebar_filter_leaf.clone().unwrap();

        // The sidebar flattened to its rows and virtualized: a workspace can
        // hold hundreds of chats and every row here is one fixed line, so the
        // old loop built the whole tree — each row's listeners, icons and
        // menus — on every repaint. `uniform_list` builds a screenful.
        let rows = Rc::new(self.sidebar_rows(&filter, self.sidebar_scope, cx));

        let (need, running, open) = {
            let store = self.store.read(cx);
            (
                store.tiles_waiting(),
                store
                    .open_tiles
                    .iter()
                    .filter(|t| !t.background && t.is_busy(store))
                    .count(),
                store.open_tiles.iter().filter(|t| !t.background).count(),
            )
        };
        let mut summary = div()
            .flex()
            .items_center()
            .gap_1()
            .text_size(px(fonts.font_meta))
            .text_color(theme::text_dim());
        if need > 0 {
            summary = summary.child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::CHANGES)
                    .child(if need == 1 {
                        "1 needs you".to_string()
                    } else {
                        format!("{need} need you")
                    }),
            );
            summary = summary.child("·");
        }
        if running > 0 {
            summary = summary.child(format!("{running} running ·"));
        }
        summary = summary.child(format!("{open} open"));

        let scope_counts = sidebar_scope_counts(self.store.read(cx));
        let scope_strip = self.render_scope_strip(&fonts, scope_counts, cx);

        shell::list_pane(width)
            .debug_selector(|| "agent-list".into())
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap_2p5()
                    .px_4()
                    .pt_3p5()
                    .pb_3()
                    .border_b_1()
                    .border_color(theme::hairline())
                    .child(
                        // no pane title and no add button up here: the list's
                        // own "Tasks" and "Workspaces" labels name the groups,
                        // and add-folder lives on the Workspaces label row
                        summary,
                    )
                    .child(
                        // the field box owns the focus/a11y node; this wrapper
                        // only names the filter — tracking the same handle here
                        // made two nodes claim a11y focus in one frame
                        div()
                            .id("sidebar-filter")
                            .role(Role::SearchInput)
                            .aria_label("Filter workspaces")
                            .aria_placeholder("Filter workspaces")
                            .aria_value(self.sidebar_filter.clone())
                            .w_full()
                            .min_w_0()
                            .child(filter_leaf),
                    )
                    // the design's repo filter strip, here over the chat tree:
                    // one trough, a chip and count per scope
                    .child(scope_strip),
            )
            .child({
                // the row list is never empty: Tasks always leads, and an
                // empty Workspaces section shows the hint under its label
                let list = uniform_list(
                    SharedString::from("sidebar-rows"),
                    rows.len(),
                    cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                        range
                            .map(|ix| this.render_sidebar_row(&rows[ix], window, cx))
                            .collect()
                    }),
                )
                .track_scroll(&self.sidebar_scroll)
                .id("sidebar-list")
                .flex_1()
                .min_h_0()
                .pt_1()
                .pb_3()
                .w_full()
                .into_any_element();
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .track_focus(&self.sidebar_focus)
                    .on_key_down(cx.listener(|this, event, window, cx| {
                        if this.handle_sidebar_key(event, window, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            if !this.sidebar_focus.is_focused(window) {
                                window.focus(&this.sidebar_focus, cx);
                            }
                        }),
                    )
                    .child(list)
                    .into_any_element()
            })
            .into_any_element()
    }

    /// The scope chips under the filter field: one trough, a chip and its
    /// count per scope, the active one lifted onto `raised` with its count in
    /// the accent — the design's repo filter strip.
    fn render_scope_strip(
        &self,
        fonts: &theme::FontTokens,
        counts: [(SidebarScope, usize); 4],
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut trough = div()
            .id("sidebar-scopes")
            .debug_selector(|| "sidebar-scopes".into())
            .w_full()
            .min_w_0()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(8.))
            .bg(theme::well());
        for (scope, count) in counts {
            let active = self.sidebar_scope == scope;
            let selector = format!("sidebar-scope-{}", scope.key());
            trough = trough.child(
                div()
                    .id(gpui::ElementId::Name(selector.clone().into()))
                    .debug_selector({
                        let selector = selector.clone();
                        move || selector.clone()
                    })
                    .a11y_toggle(format!("{} ({count})", scope.label()), active)
                    .cursor_pointer()
                    .h(px(26.))
                    .px(px(4.))
                    .rounded(px(6.))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(4.))
                    .text_size(px(fonts.font_row_meta))
                    .font_weight(if active {
                        gpui::FontWeight::SEMIBOLD
                    } else {
                        gpui::FontWeight::MEDIUM
                    })
                    .when(active, |d| d.bg(theme::raised()).text_color(theme::text()))
                    .when(!active, |d| {
                        d.text_color(theme::text_dim())
                            .hover(|s| s.text_color(theme::text()))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.sidebar_scope != scope {
                            this.sidebar_scope = scope;
                            cx.notify();
                        }
                    }))
                    .child(scope.label())
                    .child(
                        div()
                            .text_size(px(fonts.font_micro))
                            .text_color(if active {
                                theme::accent_ink()
                            } else {
                                theme::text_muted()
                            })
                            .child(count.to_string()),
                    ),
            );
        }
        trough.into_any_element()
    }

    /// The sidebar as one flat row list: Tasks on its own, the Workspaces
    /// label (with the add-folder button), then a workspace, the hint under
    /// an empty one, and its chats. Flattening keeps the virtualized list
    /// indexable while the filter still hides whole workspaces and a scope
    /// keeps only the rows it matches.
    fn sidebar_rows(&self, filter: &str, scope: SidebarScope, cx: &App) -> Vec<SidebarRow> {
        let store = self.store.read(cx);
        let mut rows = Vec::new();
        let mut section = Vec::new();
        for ws in &store.workspaces {
            if !filter.is_empty() && !ws.name.to_lowercase().contains(filter) {
                continue;
            }
            if scope == SidebarScope::All {
                if ws.is_general() {
                    Self::push_workspace_rows(&mut rows, ws);
                } else {
                    section.push((ws, Vec::new()));
                }
                continue;
            }
            let matched: Vec<crate::store::SessionItem> = ws
                .chats
                .iter()
                .filter(|chat| chat_in_scope(chat, scope, store.session_waiting(&chat.id)))
                .cloned()
                .collect();
            if !workspace_in_scope(ws, scope, matched.len()) {
                continue;
            }
            if ws.is_general() {
                Self::push_scoped_workspace(&mut rows, ws, &matched);
            } else {
                section.push((ws, matched));
            }
        }
        let tasks_shown = !rows.is_empty();
        // the label always leads the section: its + is the only add-folder
        // affordance now, so a fresh install (no folder workspaces yet)
        // still shows it, with the hint under it
        rows.push(SidebarRow::SectionLabel("Workspaces"));
        if section.is_empty() {
            rows.push(SidebarRow::Placeholder(if scope != SidebarScope::All {
                if tasks_shown {
                    "No matching workspaces"
                } else {
                    scope.empty_hint()
                }
            } else if filter.is_empty() {
                "No workspaces yet"
            } else {
                "No matching workspace"
            }));
        } else {
            for (ws, matched) in &section {
                if scope == SidebarScope::All {
                    Self::push_workspace_rows(&mut rows, ws);
                } else {
                    Self::push_scoped_workspace(&mut rows, ws, matched);
                }
            }
        }
        rows
    }

    /// A workspace row, the hint under an empty one, then its chats.
    fn push_workspace_rows(rows: &mut Vec<SidebarRow>, ws: &crate::store::WorkspaceItem) {
        rows.push(SidebarRow::Workspace(ws.clone()));
        if !ws.is_expanded() {
            return;
        }
        if ws.show_empty_hint() {
            rows.push(SidebarRow::Placeholder("No chats yet"));
        }
        for chat in &ws.chats {
            rows.push(SidebarRow::Chat(chat.clone()));
        }
    }

    /// A workspace row in a scoped list. It reads open and carries only the
    /// matching chats — the filter brought them into view even when the
    /// folder itself is folded — and its count is the matches, not the
    /// folder's total.
    fn push_scoped_workspace(
        rows: &mut Vec<SidebarRow>,
        ws: &crate::store::WorkspaceItem,
        matched: &[crate::store::SessionItem],
    ) {
        let mut row = ws.clone();
        row.state.collapsed = false;
        row.chats = matched.to_vec();
        rows.push(SidebarRow::Workspace(row));
        for chat in matched {
            rows.push(SidebarRow::Chat(chat.clone()));
        }
    }

    /// One sidebar row, at the virtualized list's fixed pitch.
    fn render_sidebar_row(
        &mut self,
        row: &SidebarRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match row {
            SidebarRow::Workspace(ws) => self.render_workspace_row(ws, cx),
            SidebarRow::Chat(chat) => self.render_session_row(chat, window, cx),
            SidebarRow::Placeholder(text) => {
                let fonts = self.store.read(cx).fonts;
                div()
                    .flex_shrink_0()
                    .h(px(SIDEBAR_ROW_HEIGHT))
                    .pl(px(40.))
                    .flex()
                    .items_center()
                    .text_size(px(fonts.font_meta))
                    .italic()
                    .text_color(theme::text_muted())
                    .child(*text)
                    .into_any_element()
            }
            SidebarRow::SectionLabel(text) => {
                let fonts = self.store.read(cx).fonts;
                div()
                    .flex_shrink_0()
                    .w_full()
                    .h(px(SIDEBAR_ROW_HEIGHT))
                    .pl_4()
                    .pr_2()
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(fonts.font_meta))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::text_dim())
                            .child(*text),
                    )
                    // the add-folder button lives here, on the section it adds to
                    .child(
                        tip(
                            div()
                                .id("add-workspace")
                                .role(Role::Button)
                                .aria_label("Add a workspace folder")
                                .cursor_pointer()
                                .p_1p5()
                                .rounded(px(6.))
                                .hover(|s| s.bg(theme::raised()))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.pick_folder(cx);
                                })),
                            "Add a workspace folder",
                        )
                        .child(crate::ui::icon(
                            "icons/plus.svg",
                            px(fonts.icon_small),
                            theme::text_dim(),
                        )),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_workspace_row(
        &mut self,
        ws: &WorkspaceItem,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let fonts = self.store.read(cx).fonts;
        let path = ws.folder_path().to_string();
        let cursor = self.sidebar_cursor.as_ref() == Some(&SidebarCursor::Workspace(path.clone()));
        // Tasks is the tree's group label, not a folder: no star, no glyph,
        // no actions menu — its new chat button is the one affordance, and
        // it never hides
        let general = ws.is_general();
        // folder rows lead with the folder glyph, open while their chats show
        let glyph: Option<gpui::AnyElement> = (!general).then(|| {
            crate::ui::icon(
                if ws.is_expanded() {
                    "icons/folder-open-outline.svg"
                } else {
                    "icons/folder-outline.svg"
                },
                px(fonts.icon_medium),
                theme::text_muted(),
            )
            .into_any_element()
        });

        let ws_store = self.store.clone();
        let path_click = path.clone();
        let menu_tx = self.menu_sender_cell.clone();
        let menu_store = self.store.clone();
        let menu_path = path.clone();
        let menu_tx2 = self.menu_sender_cell.clone();
        let menu_store2 = self.store.clone();
        let menu_path2 = path.clone();
        let ws_store_new = self.store.clone();
        let path_new = path.clone();
        let ws_store_fav = self.store.clone();
        let path_fav = path.clone();

        div()
            .id(gpui::ElementId::NamedInteger(
                "ws".into(),
                TileView::hash(&path),
            ))
            .role(Role::Button)
            .aria_label(SharedString::from(ws.name.clone()))
            .aria_expanded(ws.is_expanded())
            .relative()
            .group("wsrow")
            .flex()
            .items_center()
            .w_full()
            .h(px(SIDEBAR_ROW_HEIGHT))
            .pl_4()
            .pr_3()
            .hover(|s| s.bg(theme::raised()))
            .when(cursor, |row| row.bg(theme::panel()))
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                let expand_path = path_click.clone();
                ws_store.update(cx, |s, cx| {
                    s.select_workspace(&expand_path, cx);
                    if let Some(ws) = s
                        .workspaces
                        .iter_mut()
                        .find(|w| w.folder_path() == expand_path)
                    {
                        let expanding = !ws.is_expanded();
                        ws.set_expanded(expanding);
                        if expanding {
                            let connect_path = expand_path.clone();
                            s.connect_expanded_workspace(&connect_path, cx);
                        }
                    }
                    // an open section is a transient view choice (the next
                    // launch folds every section again); save so the
                    // selection and the rest of the sidebar state survive
                    s.save_state();
                    cx.notify();
                });
            })
            .when(!general, |row| {
                row.on_mouse_down(
                    MouseButton::Right,
                    move |event: &gpui::MouseDownEvent, _, cx| {
                        let rows = {
                            let s = menu_store.read(cx);
                            ws_menu_rows(s, &menu_path)
                        };
                        if rows.is_empty() {
                            return; // the workspace left the list between frames
                        }
                        let _ = menu_tx.send_blocking(MenuState {
                            x: event.position.x.as_f32(),
                            y: event.position.y.as_f32(),
                            rows,
                        });
                        // right-click alone doesn't repaint — nudge the store so the menu shows now
                        menu_store.update(cx, |_, _| {});
                    },
                )
            })
            // folder rows keep the star, station's repos-list style: filled
            // in the accent while a favorite, dimmed until the row hover else
            .when(!general, |row| {
                row.child(star_toggle(
                    gpui::ElementId::NamedInteger("ws-star".into(), TileView::hash(&path)),
                    ws.is_favorite,
                    fonts.icon_small,
                    "wsrow",
                    move |cx| {
                        ws_store_fav.update(cx, |s, cx| s.toggle_workspace_favorite(&path_fav, cx));
                    },
                ))
            })
            .when_some(glyph, |row, glyph| row.child(glyph))
            // no flex_1: the row packs left so the count hugs the name; a long
            // name shrinks (min_w_0) and truncates instead of pushing the
            // count to the row's far edge. Tasks is the group label of the
            // tree, so it uses the Workspaces label's quiet type rather than
            // the folder rows' 15px name.
            .child({
                let group = ws.is_general();
                div()
                    .min_w_0()
                    // the margin spaced the name from the star/glyph; Tasks
                    // has neither, so it aligns flush with the Workspaces label
                    .when(!group, |d| d.ml_2())
                    .text_size(px(if group {
                        fonts.font_meta
                    } else {
                        fonts.font_name
                    }))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(if group {
                        theme::text_dim()
                    } else {
                        theme::text()
                    })
                    .truncate()
                    .child(SharedString::from(ws.name.clone()))
            })
            .when(ws.has_chats(), |d| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .ml_1p5()
                        .text_size(px(fonts.font_meta))
                        .text_color(theme::text_muted())
                        .child(SharedString::from(format!("{}", ws.chats.len()))),
                )
            })
            // folder rows pin the running dot to the right edge; hovering the
            // row fades it out while the kebab/new-chat buttons fade in at
            // the same spot. Tasks has no hover actions left, so its dot
            // lives beside the always-visible new chat instead (cluster
            // below). Lit by RUNNING sessions in this workspace — server-up
            // alone must not read as "work in progress"
            .when(!general, |row| {
                row.child(
                    div()
                        .absolute()
                        .right_3()
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .group_hover("wsrow", |s| s.opacity(0.))
                        .when(ws.chats.iter().any(|c| c.is_running), |d| {
                            d.child(div().size(px(6.)).rounded_full().bg(theme::RUNNING))
                        }),
                )
            })
            .child(
                div()
                    .absolute()
                    .right_2()
                    .top_0()
                    .bottom_0()
                    .flex()
                    .items_center()
                    .gap_0p5()
                    // Tasks: dot + new chat, never hidden. Folder rows: the
                    // whole cluster waits for the hover.
                    .when(general, |d| {
                        d.when(ws.chats.iter().any(|c| c.is_running), |d| {
                            d.child(div().size(px(6.)).rounded_full().bg(theme::RUNNING))
                        })
                    })
                    .when(!general, |d| {
                        d.opacity(0.).group_hover("wsrow", |s| s.opacity(1.))
                    })
                    .child(
                        tip(
                            div()
                                .id(gpui::ElementId::NamedInteger(
                                    "ws-new".into(),
                                    TileView::hash(&path),
                                ))
                                .role(Role::Button)
                                .aria_label(SharedString::from(format!("New chat in {}", ws.name)))
                                .cursor_pointer()
                                .p_1()
                                .rounded_sm()
                                .hover(|s| s.bg(theme::HOVER))
                                .on_click(move |_, _, cx| {
                                    // row under this button must not see the
                                    // click — it toggles workspace expansion
                                    cx.stop_propagation();
                                    ws_store_new.update(cx, |s, cx| {
                                        s.new_chat_for_workspace(&path_new, cx)
                                    });
                                }),
                            "New chat",
                        )
                        .child(crate::ui::icon(
                            "icons/plus.svg",
                            px(fonts.icon_small),
                            theme::text_dim(),
                        )),
                    )
                    .when(!general, |row| {
                        row.child(
                            tip(
                                div()
                                    .id(gpui::ElementId::NamedInteger(
                                        "ws-kebab".into(),
                                        TileView::hash(&path),
                                    ))
                                    .role(Role::Button)
                                    .aria_label(SharedString::from(format!(
                                        "{} — workspace actions",
                                        ws.name
                                    )))
                                    .cursor_pointer()
                                    .p_1()
                                    .rounded_sm()
                                    .hover(|s| s.bg(theme::HOVER))
                                    .on_click(move |event, _, cx| {
                                        // menu only — the row must not toggle expansion
                                        cx.stop_propagation();
                                        let rows = {
                                            let s = menu_store2.read(cx);
                                            ws_menu_rows(s, &menu_path2)
                                        };
                                        if rows.is_empty() {
                                            return;
                                        }
                                        let (x, y) = match event {
                                            gpui::ClickEvent::Mouse(click) => (
                                                click.up.position.x.as_f32(),
                                                click.up.position.y.as_f32(),
                                            ),
                                            _ => (40.0, 60.0),
                                        };
                                        let _ = menu_tx2.send_blocking(MenuState { x, y, rows });
                                        // menu paints on the next store-driven repaint — same nudge as right-click
                                        menu_store2.update(cx, |_, _| {});
                                    }),
                                "Workspace actions",
                            )
                            .child(crate::ui::icon(
                                "icons/kebab.svg",
                                px(fonts.icon_medium),
                                theme::text_muted(),
                            )),
                        )
                    }),
            )
            .into_any_element()
    }

    /// Cached `SessionItem::updated_label`: sidebar rows re-render on every
    /// store repaint, and the label only moves on minute boundaries.
    fn cached_time_label(&mut self, chat: &crate::store::SessionItem) -> SharedString {
        let minute = chrono::Local::now().timestamp() / 60;
        if minute != self.sidebar_labels_minute {
            self.sidebar_labels.clear();
            self.sidebar_labels_minute = minute;
        }
        match self.sidebar_labels.get(&chat.id) {
            Some((ms, label)) if *ms == chat.updated_ms => label.clone(),
            _ => {
                let label: SharedString = chat.updated_label().into();
                self.sidebar_labels
                    .insert(chat.id.clone(), (chat.updated_ms, label.clone()));
                label
            }
        }
    }

    fn render_session_row(
        &mut self,
        chat: &crate::store::SessionItem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let time_label = self.cached_time_label(chat);
        let cursor = self.sidebar_cursor.as_ref() == Some(&SidebarCursor::Chat(chat.id.clone()));
        let (fonts, accent, harness_label, waiting, row_width) = {
            let store = self.store.read(cx);
            (
                store.fonts,
                store.accent.accent,
                store.session_harness(&chat.id).label(),
                store.session_waiting(&chat.id),
                store.sidebar_shown_width,
            )
        };
        // The harness label is the row's optional metadata: as soon as the
        // title no longer fits beside it, drop the harness and give its room
        // to the title (the age stays — it is the row's timing signal).
        let title_weight = if chat.is_selected {
            gpui::FontWeight::SEMIBOLD
        } else {
            gpui::FontWeight::NORMAL
        };
        // pl 24 + pr 12 + the 16px gutter and its 4px gap, plus a little room
        // for the list's overlay scrollbar before the row trims
        let fixed = 24. + 12. + 16. + 4. + 8.;
        let meta_overhead = 8. + 4.; // the meta's ml_2 and the gap before the age
        let room_with_harness = row_width
            - fixed
            - meta_overhead
            - line_width(
                window,
                harness_label,
                fonts.font_meta,
                gpui::FontWeight::NORMAL,
            )
            - line_width(
                window,
                &time_label,
                fonts.font_meta,
                gpui::FontWeight::NORMAL,
            )
            - 2.; // a pixel of cushion before truncation kicks in
        let show_harness =
            line_width(window, &chat.title, fonts.font_body, title_weight) <= room_with_harness;
        let id = chat.id.clone();
        let store_click = self.store.clone();
        let id_click = id.clone();
        let menu_tx = self.menu_sender_cell.clone();
        let menu_store = self.store.clone();
        let id_menu = id.clone();
        // the hover ⋯ needs its own handles: the right-click closure already
        // moved the first set
        let more_tx = self.menu_sender_cell.clone();
        let more_store = self.store.clone();
        let more_id = id.clone();
        // the gutter pin is the row's pin/unpin switch — accent while pinned
        // like the star's favorite state, dim until the row hover else — so
        // the ⋯ menu's Pin entry is the fallback, not the primary affordance
        let pin_store = self.store.clone();
        let pin_id = id.clone();
        let pinned = chat.is_pinned;
        // the trash beside the ⋯ arms red on the first click, deletes on the
        // second (window state — the arm must survive the repaint that shows it)
        let delete_armed = self.delete_armed.as_deref() == Some(id.as_str());
        let id_del = id.clone();
        let gutter: gpui::AnyElement = if waiting {
            crate::ui::icon("icons/shield.svg", px(fonts.icon_tiny), theme::CHANGES)
                .into_any_element()
        } else if chat.is_running {
            warpui_kit_core::motion::spinner(
                SharedString::from(format!("chat-spinner-{id}")),
                "icons/spinner.svg",
                px(fonts.icon_tiny),
                theme::BEHIND,
            )
        } else {
            tip(
                div()
                    .id(gpui::ElementId::NamedInteger(
                        "chat-pin".into(),
                        TileView::hash(&id),
                    ))
                    .a11y_toggle(if pinned { "Unpin chat" } else { "Pin chat" }, pinned)
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(!pinned, |pin| {
                        pin.opacity(0.62).group_hover("chatrow", |s| s.opacity(1.))
                    })
                    .on_click(move |_, _, cx| {
                        // the row must not select behind the button
                        cx.stop_propagation();
                        pin_store.update(cx, |s, cx| s.toggle_pin(&pin_id, cx));
                    })
                    .child(crate::ui::icon(
                        "icons/pin.svg",
                        px(fonts.icon_small),
                        if pinned { accent } else { theme::text_muted() },
                    )),
                if pinned { "Unpin" } else { "Pin" },
            )
            .into_any_element()
        };

        let mut row = div()
            .id(gpui::ElementId::NamedInteger(
                "chat".into(),
                TileView::hash(&id),
            ))
            .role(Role::Button)
            .aria_label(SharedString::from(chat.title.clone()))
            .aria_selected(chat.is_selected)
            .relative()
            .group("chatrow")
            .flex()
            .items_center()
            // the row owns the full list width: without this its hover and
            // selected fill stopped at the text, not at the pane edge
            .w_full()
            .h(px(SIDEBAR_ROW_HEIGHT))
            .pl(px(24.))
            .pr_3()
            .when(cursor && !chat.is_selected, |d| d.bg(theme::panel()))
            .when(chat.is_selected, |d| {
                d.bg(theme::raised()).child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .w(px(3.))
                        .bg(accent),
                )
            })
            .hover(|s| s.bg(theme::raised()))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                tracing::info!("chat-row clicked {}", id_click);
                // a click anywhere else in the row stands a pending delete down
                if this.delete_armed.take().is_some() {
                    cx.notify();
                }
                store_click.update(cx, |s, cx| s.select_session(&id_click, cx));
            }))
            .on_mouse_down(
                MouseButton::Right,
                move |event: &gpui::MouseDownEvent, _, cx| {
                    let rows = {
                        let s = menu_store.read(cx);
                        chat_menu_rows(s, &id_menu)
                    };
                    let _ = menu_tx.send_blocking(MenuState {
                        x: event.position.x.as_f32(),
                        y: event.position.y.as_f32(),
                        rows,
                    });
                    // right-click alone doesn't repaint — nudge the store so the menu shows now
                    menu_store.update(cx, |_, _| {});
                },
            )
            .child(
                div()
                    .flex_shrink_0()
                    .w(px(16.))
                    .mr_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(gutter),
            );

        if chat.is_editing {
            if let Some((_, _, leaf)) = &self.rename {
                // the field owns the press: the row underneath must not select
                // the chat (and steal focus back) while the title is edited
                row = row.child(
                    div()
                        .id(gpui::ElementId::NamedInteger(
                            "rename".into(),
                            TileView::hash(&id),
                        ))
                        .flex_1()
                        .min_w_0()
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(leaf.clone()),
                );
            }
        } else {
            row = row
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(fonts.font_body))
                        .when(chat.is_selected, |d| {
                            d.font_weight(gpui::FontWeight::SEMIBOLD)
                        })
                        .text_color(theme::text())
                        .truncate()
                        .child(SharedString::from(chat.title.clone())),
                )
                // right meta cluster in flow: which backend the chat runs on
                // (dropped when the title needs its room), then the age.
                // Hovering the row fades the cluster out and fades the ⋯ in
                // at the same spot (absolute layer, so nothing shifts)
                .child({
                    let mut meta = div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap_1()
                        .group_hover("chatrow", |s| s.opacity(0.))
                        .ml_2()
                        .text_size(px(fonts.font_meta))
                        .text_color(theme::text_muted());
                    if show_harness {
                        meta = meta.child(SharedString::from(harness_label));
                    }
                    meta.child(time_label.clone())
                })
                // the design's ⋯: favorite (the star), Pin, Rename and Delete
                // chat, the same menu the right-click opens. Pinning lives on
                // the row's gutter pin; deleting has a direct hover button
                // beside the ⋯ — first click arms it red, the second deletes.
                // No occlude on the buttons: an occluding hitbox drops the
                // row's group hover, which fades this whole cluster out under
                // the cursor.
                .child(
                    div()
                        .absolute()
                        .right_1()
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .gap_0p5()
                        .opacity(0.)
                        .group_hover("chatrow", |s| s.opacity(1.))
                        .child(
                            tip(
                                div()
                                    .id(gpui::ElementId::NamedInteger(
                                        "chat-del".into(),
                                        TileView::hash(&id),
                                    ))
                                    .a11y_button(if delete_armed {
                                        "Confirm delete"
                                    } else {
                                        "Delete chat"
                                    })
                                    .cursor_pointer()
                                    .p_1()
                                    .rounded_sm()
                                    .hover(|s| s.bg(theme::HOVER))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        // the row must not select behind the button
                                        cx.stop_propagation();
                                        if this.delete_armed.as_deref() == Some(id_del.as_str()) {
                                            this.delete_armed = None;
                                            let store = this.store.clone();
                                            store.update(cx, |s, cx| s.delete_session(&id_del, cx));
                                        } else {
                                            this.delete_armed = Some(id_del.clone());
                                            cx.notify();
                                        }
                                    })),
                                if delete_armed {
                                    "Confirm delete"
                                } else {
                                    "Delete chat"
                                },
                            )
                            .child(crate::ui::icon(
                                "icons/trash.svg",
                                px(fonts.icon_medium),
                                if delete_armed {
                                    theme::DEL
                                } else {
                                    theme::text_muted()
                                },
                            )),
                        )
                        .child(
                            tip(
                                div()
                                    .id(gpui::ElementId::NamedInteger(
                                        "chat-more".into(),
                                        TileView::hash(&id),
                                    ))
                                    .role(Role::Button)
                                    .aria_label(SharedString::from(format!(
                                        "{} — chat actions",
                                        chat.title
                                    )))
                                    .cursor_pointer()
                                    .p_1()
                                    .rounded_sm()
                                    .hover(|s| s.bg(theme::HOVER))
                                    .on_click(move |event, _, cx| {
                                        // menu only — the row must not select
                                        cx.stop_propagation();
                                        let rows = {
                                            let s = more_store.read(cx);
                                            chat_menu_rows(s, &more_id)
                                        };
                                        let (x, y) = match event {
                                            gpui::ClickEvent::Mouse(click) => (
                                                click.up.position.x.as_f32(),
                                                click.up.position.y.as_f32(),
                                            ),
                                            _ => (40.0, 60.0),
                                        };
                                        let _ = more_tx.send_blocking(MenuState { x, y, rows });
                                        // menu paints on the next store-driven repaint
                                        more_store.update(cx, |_, _| {});
                                    }),
                                "Chat actions",
                            )
                            .child(crate::ui::icon(
                                "icons/kebab.svg",
                                px(fonts.icon_medium),
                                theme::text_muted(),
                            )),
                        ),
                );
        }
        row.into_any_element()
    }

    fn render_splitter(&mut self) -> gpui::AnyElement {
        let store_handle = self.store.clone();
        let drag_state = self.splitter_drag.clone();
        let drag_for_paint = self.splitter_drag.clone();
        let store_for_paint = self.store.clone();
        div()
            .id("splitter")
            .relative()
            .w(px(theme::SPLITTER_WIDTH))
            .h_full()
            .flex_shrink_0()
            .bg(theme::well())
            .cursor_col_resize()
            .hover(|s| s.bg(theme::line_strong()))
            .on_mouse_down(
                MouseButton::Left,
                move |event: &gpui::MouseDownEvent, _, cx| {
                    let start_width = store_handle.read(cx).sidebar_shown_width;
                    drag_state.set(Some((event.position.x, start_width)));
                    cx.stop_propagation();
                },
            )
            .child(
                canvas(
                    // this gpui rev: canvas(prepaint, paint) — on_mouse_event must
                    // register from the second closure, prepaint panics on it
                    |_, _, _| {},
                    move |_, _, window, _| {
                        // registered during paint, as this gpui rev requires
                        if drag_for_paint.get().is_some() {
                            let drag = drag_for_paint.clone();
                            let store = store_for_paint.clone();
                            window.on_mouse_event(
                                move |event: &gpui::MouseMoveEvent, phase, _window, cx| {
                                    if phase != gpui::DispatchPhase::Bubble {
                                        return;
                                    }
                                    let Some((start_x, start_width)) = drag.get() else {
                                        return;
                                    };
                                    match event.pressed_button {
                                        Some(MouseButton::Left) => {
                                            let delta = (event.position.x - start_x).as_f32();
                                            store.update(cx, |s, cx| {
                                                s.sidebar_shown_width = (start_width + delta)
                                                    .clamp(
                                                        theme::SIDEBAR_MIN_WIDTH,
                                                        theme::SIDEBAR_MAX_WIDTH,
                                                    );
                                                cx.notify();
                                            });
                                        }
                                        _ => {
                                            drag.set(None);
                                            store.update(cx, |s, _| s.commit_sidebar_width());
                                            cx.stop_propagation();
                                        }
                                    }
                                },
                            );
                        }
                    },
                )
                .absolute()
                .size_full(),
            )
            .into_any_element()
    }

    fn pick_folder(&mut self, cx: &mut Context<Self>) {
        let task = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Workspace folder".into()),
        });
        let tx = self.add_folder_sender.clone();
        cx.spawn(async move |_this, _cx| {
            if let Ok(Ok(Some(paths))) = task.await
                && let Some(path) = paths.first()
            {
                let _ = tx.send_blocking(path.display().to_string());
            }
        })
        .detach();
    }

    fn make_settings_fields(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> SettingsFields {
        let b = self.settings_buffers.clone();
        let rows: Vec<(&'static str, String)> = vec![
            ("executable", b.opencode_executable),
            ("dsh_executable", b.dsh_executable),
            ("model", b.default_model),
            ("variant", b.default_variant),
            ("agent", b.default_agent),
            ("min", b.auto_allow_default_min),
            ("max", b.auto_allow_default_max),
            ("awaymin", b.auto_allow_away_min),
            ("awaymax", b.auto_allow_away_max),
            ("idlemin", b.server_idle_min),
            ("lunchstart", b.lunch_start),
            ("lunchend", b.lunch_end),
            ("offstart", b.off_hours_start),
            ("workdaystart", b.workday_start),
        ];
        let mut fields = Vec::new();
        let mut leafs = Vec::new();
        let mut model_picker = None;
        let mut model_state = None;
        let mut variant_state = None;
        for (key, value) in rows {
            if key == "model" {
                let state = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("")
                        .default_value(value)
                });
                // the design has no Save on the page anymore: a keystroke
                // schedules the debounced commit that parses and persists
                cx.subscribe(&state, |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.schedule_settings_commit(cx);
                    }
                })
                .detach();
                let store = self.store.clone();
                let harness = self.settings_harness.clone();
                let picker = settings_model_picker(state.clone(), store, harness, window, cx);
                model_state = Some(state.clone());
                fields.push((key, state));
                model_picker = Some(picker);
            } else if key == "variant" {
                let state = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("")
                        .default_value(value)
                });
                cx.subscribe(&state, |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.schedule_settings_commit(cx);
                    }
                })
                .detach();
                // the picker is the visible control; the field only carries
                // the picked value into `commit_settings`
                variant_state = Some(state.clone());
                fields.push((key, state));
            } else {
                let entity = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("")
                        .default_value(value)
                });
                cx.subscribe(&entity, |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.schedule_settings_commit(cx);
                    }
                })
                .detach();
                let leaf = cx.new(|_| InputLeaf {
                    state: entity.clone(),
                    // a settings value is the prototype's `.field text`
                    mono: true,
                    icon: None,
                });
                fields.push((key, entity));
                leafs.push((key, leaf));
            }
        }
        // the Variant picker lists the selected model's tiers; a model switch
        // clears the field, and both pickers observe it
        let variant_picker = match (model_state, variant_state) {
            (Some(model), Some(variant)) => Some(settings_variant_picker(
                variant,
                model,
                self.store.clone(),
                self.settings_harness.clone(),
                window,
                cx,
            )),
            _ => None,
        };
        SettingsFields {
            fields,
            leafs,
            model_picker,
            variant_picker,
        }
    }

    /// One commit a beat after the last keystroke: the buffers re-parse on
    /// every schedule but only the last one lands, so typing costs one
    /// settings write and the page never asks for a Save press.
    fn schedule_settings_commit(&mut self, cx: &mut Context<Self>) {
        self.settings_commit_seq += 1;
        let seq = self.settings_commit_seq;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(600))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.settings_commit_seq == seq {
                    this.commit_settings(cx);
                    this.settings_committed_seq = seq;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn settings_field_wrap(&self, key: &str, cx: &mut App) -> gpui::AnyElement {
        self.settings_leafs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, leaf)| {
                // the field box owns the focus/a11y node; this wrapper only
                // names and sizes the field (a second track_focus here made
                // two nodes claim a11y focus in one frame)
                let state = leaf.read(cx).state.clone();
                let value = state.read(cx).value().to_string();
                div()
                    .id(gpui::ElementId::Name(SharedString::from(format!(
                        "field-{key}"
                    ))))
                    .role(Role::TextInput)
                    .aria_label(SharedString::from(format!("{key} field")))
                    .aria_value(value)
                    .flex_1()
                    // numeric rows seat this wrap inside content-sized
                    // controls (min – max seconds): flex-basis 0 plus
                    // min-width 0 collapsed the box to its padding, and the
                    // kit input's value painted in a zero-wide viewport —
                    // every small settings field rendered empty. A floor
                    // keeps the box usable; wide fields outgrow it anyway.
                    .min_w(px(warpui_kit_core::shell::scaled(72.)))
                    .child(leaf.clone())
                    .into_any_element()
            })
            .unwrap_or_else(|| div().into_any_element())
    }

    fn render_main(
        &mut self,
        list_shown: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // Settings and About are rail sections of their own: they replace the
        // grid rather than floating over it
        if self.store.read(cx).is_settings_open {
            return self.render_settings(window, cx);
        }
        if self.store.read(cx).is_about_open {
            return self.render_about(cx);
        }
        let fonts = self.store.read(cx).fonts;

        // keep TileView entities in sync with the store's tile list
        let keys: Vec<String> = self
            .store
            .read(cx)
            .open_tiles
            .iter()
            // a background door run's tile executes hidden — it never
            // mounts a view, and dropping the mapping frees nothing it holds
            .filter(|t| !t.background)
            .map(|t| t.key.clone())
            .collect();
        for key in &keys {
            if !self.tile_views.contains_key(key) {
                let store_handle = self.store.clone();
                let key_clone = key.clone();
                let view = cx.new(|cx| TileView::new(store_handle, key_clone, window, cx));
                self.tile_views.insert(key.clone(), view);
            }
        }
        self.tile_views.retain(|key, view| {
            if keys.contains(key) {
                return true;
            }
            // a closing tile's decoded images must leave the sprite atlas,
            // not just the Rust-side cache
            for img in view.read(cx).held_images() {
                cx.drop_image(img, Some(window));
            }
            false
        });

        let tile_columns = self.store.read(cx).tile_columns().max(1);
        // explicit equal rows: implicit grid rows are content-sized (auto),
        // which let a second-row tile end up shorter than the first-row pair
        let tile_rows = keys.len().div_ceil(tile_columns).max(1) as u16;
        // tiles are cached views: the window measures them, and a density change
        // repaints the one tile it touches
        let list_width = if list_shown {
            self.store.read(cx).sidebar_shown_width + theme::SPLITTER_WIDTH
        } else {
            0.
        };
        let grid_width =
            window.viewport_size().width.as_f32() - shell::scaled(shell::RAIL_WIDTH) - list_width;
        let tile_width = tile_track_width(grid_width, tile_columns);
        let density = TileDensity::for_width(tile_width, shell::scaled(1.));
        for view in self.tile_views.values() {
            view.update(cx, |tile, cx| tile.set_density(density, cx));
        }

        let mut grid = div()
            .debug_selector(|| "tile-grid".into())
            .grid()
            .grid_cols(tile_columns as u16)
            .grid_rows(tile_rows)
            .gap(px(GRID_GAP))
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .p(px(GRID_GAP));
        for key in &keys {
            if let Some(view) = self.tile_views.get(key) {
                // cached: the tile's render only runs when its own revision
                // entity fired (store.touch_tile) — one streaming tile no
                // longer rebuilds every open transcript per coalesced frame.
                // The grid stretch sizes the node; contents aren't measured.
                grid = grid.child(view.clone().cached(StyleRefinement::default()));
            }
        }

        let mut main = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            // the grid is the same detail pane every app has; the tiles are
            // base cards that sit on it
            .bg(theme::panel())
            .overflow_hidden()
            .relative();

        let (has_tiles, has_no_ws, has_idle_ws, has_running_ws) = {
            let store = self.store.read(cx);
            (
                store.has_tiles(),
                store.has_no_workspace(),
                store.has_idle_workspace(),
                store.has_running_workspace(),
            )
        };
        if !has_tiles {
            let hint = if has_no_ws {
                Some("Add a workspace folder to begin.")
            } else if has_idle_ws {
                Some("Start the server to load your chats.")
            } else if has_running_ws {
                Some(
                    "Open a chat from the sidebar, or start a new one with the + next to a workspace.",
                )
            } else {
                None
            };
            if let Some(hint) = hint {
                main = main.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_size(px(fonts.font_body))
                                .text_color(theme::text_faint())
                                .child(hint),
                        ),
                );
            }
        }

        let (show_restore, restore_text) = {
            let store = self.store.read(cx);
            (store.show_restore_prompt, store.restore_prompt_text.clone())
        };
        if show_restore {
            let store_handle = self.store.clone();
            let accent = self.store.read(cx).accent.accent;
            main = main.child(
                div()
                    .flex_shrink_0()
                    .mx(px(GRID_GAP))
                    .mt(px(GRID_GAP))
                    .mr_auto()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme::hairline())
                    .bg(theme::surface())
                    .px_3()
                    .py_2()
                    .child(crate::ui::icon(
                        "icons/clock.svg",
                        px(fonts.icon_small),
                        theme::text_muted(),
                    ))
                    .child(
                        div()
                            .text_size(px(fonts.font_body))
                            .text_color(theme::text())
                            .child(SharedString::from(restore_text.clone())),
                    )
                    .child(crate::ui::button(
                        "restore-btn",
                        "Restore",
                        BtnKind::Outline,
                        accent,
                        fonts.font_control,
                        true,
                        {
                            let store = store_handle.clone();
                            move |_, _, cx| {
                                store.update(cx, |s, cx| s.restore_previous_session(cx));
                            }
                        },
                    ))
                    .child(crate::ui::button(
                        "ignore-btn",
                        "Ignore",
                        BtnKind::Outline,
                        accent,
                        fonts.font_control,
                        true,
                        {
                            let store = store_handle.clone();
                            move |_, _, cx| {
                                store.update(cx, |s, cx| s.dismiss_restore_prompt(cx));
                            }
                        },
                    )),
            );
        }

        main = main.child(div().flex_1().min_h_0().flex().flex_col().child(grid));

        main.into_any_element()
    }

    // ---- settings page ----

    fn render_settings(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let fonts = self.store.read(cx).fonts;
        let active = self.settings_section;
        let width = self.store.read(cx).sidebar_shown_width;

        // the section list: Settings is a rail section like every other page,
        // so it is a list beside a detail pane, not a sheet over the grid
        let mut rows = div().flex().flex_col();
        for section in SettingsSection::ALL {
            rows = rows.child(
                shell::list_row(
                    gpui::ElementId::Name(format!("settings-section-{}", section.label()).into()),
                    section == active,
                )
                .debug_selector(move || format!("settings-section-{}", section.label()))
                .a11y_button(section.label())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.settings_section = section;
                    cx.notify();
                }))
                .child(
                    div()
                        .text_size(px(fonts.font_name))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme::text())
                        .child(section.label()),
                )
                .child(
                    div()
                        .mt_0p5()
                        .text_size(px(fonts.font_row_meta))
                        .text_color(theme::text_dim())
                        .child(section.hint()),
                ),
            );
        }
        let list = shell::list_pane(width)
            .child(
                div()
                    .flex_shrink_0()
                    .px_4()
                    .pt_3p5()
                    .pb_3()
                    .border_b_1()
                    .border_color(theme::hairline())
                    .text_size(px(fonts.font_list_title))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child("Settings"),
            )
            .child(
                div()
                    .id("settings-sections-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(rows),
            );

        let head = div()
            .flex_shrink_0()
            // the row measures against the pane, not against its own content:
            // without these a long hint widened it and pushed the actions off
            .w_full()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px(px(shell::DETAIL_PADDING))
            .py_4()
            .border_b_1()
            .border_color(theme::hairline())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_size(px(fonts.font_screen_title))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child(active.label()),
                    )
                    .child(
                        div()
                            .mt_0p5()
                            .text_size(px(fonts.font_meta))
                            .text_color(theme::text_dim())
                            .child(active.hint()),
                    ),
            );
        // no Save on the page: every section applies as its controls change
        // (toggles commit at once, fields commit a beat after the last
        // keystroke), so the head stays just the section title
        let rows = self.settings_rows(active, &fonts, cx);

        shell::panes()
            .child(list)
            .child(
                shell::detail_pane()
                    .debug_selector(|| "settings-detail".into())
                    .child(head)
                    .child(
                        div()
                            .id("settings-scroll")
                            .flex_1()
                            .w_full()
                            .min_w_0()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p(px(shell::DETAIL_PADDING))
                            .child(
                                // the design's `.measure`: rows, not cards
                                div()
                                    .w_full()
                                    .min_w_0()
                                    .max_w(px(shell::READING_WIDTH))
                                    .child(warpui_kit_core::settings::rows(rows)),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// One section's rows: the design's flat `.setting` list, not cards.
    fn settings_rows(
        &mut self,
        active: SettingsSection,
        fonts: &theme::FontTokens,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let buffers = self.settings_buffers.clone();
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        let entity = cx.entity();
        macro_rules! field {
            ($label:expr, $key:expr, $desc:expr) => {{
                let desc: Option<&str> = $desc;
                let control = div()
                    .w(px(240.))
                    .max_w(px(0.46 * warpui_kit_core::shell::READING_WIDTH))
                    .min_w_0()
                    .child(self.settings_field_wrap($key, cx));
                rows.push(match desc {
                    Some(desc) => kit::row_with_desc(
                        format!("settings-row-{}", $label),
                        $label,
                        desc,
                        control,
                        rows.is_empty(),
                    ),
                    None => kit::row(
                        format!("settings-row-{}", $label),
                        $label,
                        control,
                        rows.is_empty(),
                    ),
                });
            }};
        }
        macro_rules! toggle {
            ($label:expr, $desc:expr, $checked:expr, $field:ident) => {{
                let entity = entity.clone();
                rows.push(match $desc {
                    Some(desc) => kit::switch_row_with_desc(
                        SharedString::from(format!("set-{}", $label)),
                        $label,
                        format!("settings-row-{}", $label),
                        desc,
                        $checked,
                        rows.is_empty(),
                        move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.settings_buffers.$field = !this.settings_buffers.$field;
                                this.commit_settings(cx);
                                cx.notify();
                            });
                        },
                    ),
                    None => kit::switch_row(
                        SharedString::from(format!("set-{}", $label)),
                        $label,
                        format!("settings-row-{}", $label),
                        $checked,
                        rows.is_empty(),
                        move |_, _, cx| {
                            entity.update(cx, |this, cx| {
                                this.settings_buffers.$field = !this.settings_buffers.$field;
                                this.commit_settings(cx);
                                cx.notify();
                            });
                        },
                    ),
                });
            }};
        }
        match active {
            SettingsSection::Appearance => {
                rows.push(kit::row_with_desc(
                    "settings-row-Text size",
                    "Text size",
                    "Applied right away. The other Warp apps follow when they next open.",
                    warpui_kit_core::text_size_control(),
                    rows.is_empty(),
                ));
                rows.push(kit::row_with_desc(
                    "settings-row-Palette",
                    "Palette",
                    "Shared by every app in the suite",
                    warpui_kit_core::palette_control(),
                    rows.is_empty(),
                ));
                toggle!(
                    "Reduce motion",
                    Some("Turns off transitions and keeps spinners still. Busy labels stay"),
                    buffers.reduce_motion,
                    reduce_motion
                );
                toggle!(
                    "Show tooltips on hover",
                    None::<&str>,
                    buffers.show_tooltips,
                    show_tooltips
                );
            }
            SettingsSection::ChatDefaults => {
                rows.push(kit::row(
                    "settings-row-Harness",
                    "Harness",
                    self.harness_choices_control(fonts, cx),
                    rows.is_empty(),
                ));
                if let Some(picker) = self.settings_model_picker.clone() {
                    rows.push(kit::row(
                        "settings-row-Model",
                        "Model",
                        // long provider/model keys (opencode-go/deepseek-v4.1-flash)
                        // were truncated at 320; 440 keeps the full name readable
                        div().w(px(440.)).flex_shrink_0().child(picker),
                        rows.is_empty(),
                    ));
                }
                if let Some(picker) = self.settings_variant_picker.clone() {
                    // "always highest" owns the default: the list stays
                    // readable but does not open while the switch is on
                    picker.update(cx, |picker, cx| {
                        picker.set_disabled(buffers.variant_highest, cx);
                    });
                    rows.push(kit::row(
                        "settings-row-Variant",
                        "Variant",
                        div().w(px(240.)).flex_shrink_0().child(picker),
                        rows.is_empty(),
                    ));
                }
                toggle!(
                    "Always select the highest variant by default",
                    None::<&str>,
                    buffers.variant_highest,
                    variant_highest
                );
                field!("Agent", "agent", None::<&str>);
            }
            SettingsSection::Permissions => {
                toggle!(
                    "Auto-allow tool permissions",
                    Some("Edits, shell commands and the rest, answered after a short wait"),
                    buffers.auto_allow,
                    auto_allow
                );
                if buffers.auto_allow {
                    toggle!(
                        "Allow immediately",
                        Some("Skips the random wait"),
                        buffers.auto_allow_instant,
                        auto_allow_instant
                    );
                    if !buffers.auto_allow_instant {
                        for (label, first_key, second_key, suffix) in [
                            ("Working hours", "min", "max", Some("seconds")),
                            ("Lunch", "lunchstart", "lunchend", None),
                            ("Lunch and off hours", "awaymin", "awaymax", Some("seconds")),
                        ] {
                            let mut control = div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(self.settings_field_wrap(first_key, cx))
                                .child(div().text_color(theme::text_muted()).child("–"))
                                .child(self.settings_field_wrap(second_key, cx));
                            if let Some(suffix) = suffix {
                                control = control.child(
                                    div()
                                        .text_size(px(fonts.font_body))
                                        .text_color(theme::text_faint())
                                        .child(suffix),
                                );
                            }
                            rows.push(kit::row(
                                SharedString::from(format!("settings-row-{label}")),
                                label,
                                control,
                                rows.is_empty(),
                            ));
                        }
                        rows.push(kit::row(
                            "settings-row-Off hours",
                            "Off hours",
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(div().text_color(theme::text_muted()).child("→"))
                                .child(self.settings_field_wrap("offstart", cx)),
                            rows.is_empty(),
                        ));
                    }
                }
                toggle!(
                    "Disable MCP servers (all workspaces)",
                    Some("Disconnects every workspace's MCP servers at once"),
                    buffers.mcp_disabled,
                    mcp_disabled
                );
            }
            SettingsSection::Server => {
                field!("OpenCode executable", "executable", None::<&str>);
                field!("DSH executable", "dsh_executable", None::<&str>);
                let control = div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .max_w(px(0.46 * warpui_kit_core::shell::READING_WIDTH))
                    .child(self.settings_field_wrap("idlemin", cx))
                    .child(
                        div()
                            .text_size(px(fonts.font_body))
                            .text_color(theme::text_faint())
                            .child("minutes"),
                    );
                rows.push(kit::row_with_desc(
                    "settings-row-Idle stop",
                    "Idle stop",
                    "A workspace server with no running chat closes after this many idle minutes",
                    control,
                    rows.is_empty(),
                ));
            }
        }
        rows
    }

    /// The harness picker as the design's segmented control.
    fn harness_choices_control(
        &self,
        _fonts: &theme::FontTokens,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let current = self.settings_buffers.harness();
        let choices = harness_choices(current);
        let entity = cx.entity();
        let scope = self.settings_harness.clone();
        kit::segmented(
            "settings-harness",
            choices,
            move |key, window, cx: &mut gpui::App| {
                let picked = crate::settings::HarnessKind::from_wire(key.as_ref())
                    .unwrap_or(crate::settings::HarnessKind::OpenCode);
                entity.update(cx, |this, cx| {
                    let was = this.settings_buffers.harness();
                    if was == picked {
                        return;
                    }
                    this.settings_buffers.default_harness_dsh =
                        picked == crate::settings::HarnessKind::Dsh;
                    // the model and variant belong to the harness they were
                    // listed under; a switch starts at the new server default
                    for key in ["model", "variant"] {
                        if let Some((_, field)) =
                            this.settings_fields.iter().find(|(name, _)| *name == key)
                        {
                            field.update(cx, |state, cx| state.set_value("", window, cx));
                        }
                    }
                    this.settings_buffers.default_model.clear();
                    this.settings_buffers.default_variant.clear();
                    this.commit_settings(cx);
                    scope.update(cx, |scope, cx| {
                        scope.0 = picked;
                        cx.notify();
                    });
                    let store = this.store.clone();
                    store.update(cx, |store, cx| {
                        store.retarget_untouched_drafts(was, picked, cx);
                        if picked == crate::settings::HarnessKind::Dsh {
                            store.ensure_dsh(cx);
                        }
                    });
                    cx.notify();
                });
            },
        )
    }

    /// Persists the settings page: text fields land in the buffers, then the
    /// buffers parse and push into live state. Runs debounced from field
    /// keystrokes and at once from the toggles, so every mutation lands on
    /// disk without a Save press.
    fn commit_settings(&self, cx: &mut App) {
        let mut buffers = self.settings_buffers.clone();
        for (key, field) in &self.settings_fields {
            let text = field.read(cx).value().to_string();
            match *key {
                "executable" => buffers.opencode_executable = text,
                "dsh_executable" => buffers.dsh_executable = text,
                "model" => buffers.default_model = text,
                "variant" => buffers.default_variant = text,
                "agent" => buffers.default_agent = text,
                "min" => buffers.auto_allow_default_min = text,
                "max" => buffers.auto_allow_default_max = text,
                "awaymin" => buffers.auto_allow_away_min = text,
                "awaymax" => buffers.auto_allow_away_max = text,
                "idlemin" => buffers.server_idle_min = text,
                "lunchstart" => buffers.lunch_start = text,
                "lunchend" => buffers.lunch_end = text,
                "offstart" => buffers.off_hours_start = text,
                "workdaystart" => buffers.workday_start = text,
                _ => {}
            }
        }
        buffers.save(&self.store, cx);
    }
}

/// Leaf renderer for a plain settings/rename input: gpui re-renders every
/// view that read an InputState during its own render, so rendering these
/// inputs from WarpWindow made each keystroke repaint the whole window —
/// sidebar and tile grid included. The owner keeps the shared InputState
/// handle for reading values; this view is only the renderer.
struct InputLeaf {
    state: Entity<InputState>,
    /// The prototype's `.field text`: a settings value (a path, an executable,
    /// a model) is mono, while a filter or a chat title is the UI face.
    mono: bool,
    /// A leading icon inside the box — the sidebar filter's search.
    icon: Option<&'static str>,
}

impl Render for InputLeaf {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // the family's one field; the wrapper around this leaf owns the width
        let handle = self.state.read(cx).focus_handle(cx);
        let mono = self.mono;
        let input = warpui_kit_core::field::input(&self.state);
        let field = warpui_kit_core::field::boxed(
            "input-leaf",
            &handle,
            window,
            warpui_kit_core::shell::field_height(),
        )
        .when_some(self.icon, |field, path| {
            field.child(crate::ui::icon(
                path,
                px(theme::active_tokens().icon_small),
                theme::text_muted(),
            ))
        })
        .child(if mono {
            warpui_kit_core::field::mono_face(input)
        } else {
            input
        });
        // a disabled field keeps the box's edge and its fill (the widget
        // already dims its own value), so it still reads as a field
        field
    }
}

// ---- the settings model picker ---------------------------------------------

/// The app's icon paths for the shared picker shell.
const PICKER_ICONS: PickerIcons = PickerIcons {
    check: "icons/check.svg",
    chevron_down: "icons/chevron-down.svg",
    trash: "icons/trash.svg",
    refresh: "icons/refresh.svg",
};

/// The settings "model" slot: a shared FilteredPicker whose pick writes the
/// model into `state`; the catalog comes from the store.
fn settings_model_picker(
    state: Entity<InputState>,
    store: Entity<Store>,
    harness: Entity<crate::ui::settings::HarnessChoice>,
    window: &mut Window,
    cx: &mut Context<WarpWindow>,
) -> Entity<FilteredPicker> {
    let spec = PickerSpec {
        key: "default",
        trigger_label: "Default model",
        filter_placeholder: "Filter models",
        icons: PICKER_ICONS,
        trigger_icon: None,
        provider_badge: true,
        refresh: true,
        refresh_label: "Refresh models",
        stop_trigger_mousedown: false,
        visible_rows: 10,
        selected_accent: false,
        // a settings slot floats: the model list is taller than the card and
        // must cover the rows under it, not stretch the form around itself
        overlay: true,
        anchor: OverlayAnchor::Field,
        trigger_scale: TriggerScale::Regular,
        trigger_rounded_lg: true,
        trigger_min_h: Some(26.),
        panel_chrome: PanelChrome::Cards,
        trigger_id: Some("model-dd-toggle"),
        panel_id: None,
        filter_selector: Some("model-dd-filter"),
        list_id: Some("model-dd-list"),
    };
    // the page that owns the field: a pick must commit through it, since
    // set_value emits no Change event for the field's subscription. Weak, so
    // the picker's callback cannot keep the window (and itself) alive.
    let page = cx.entity().downgrade();
    cx.new(|cx| {
        let data = {
            let state = state.clone();
            let store = store.clone();
            let harness = harness.clone();
            Box::new(move |filter: &str, _window: &Window, cx: &App| {
                model_data(&state, &harness, &store, filter, cx)
            }) as Box<dyn Fn(&str, &Window, &App) -> PickerData>
        };
        // a pick lands through set_value, which emits no Change event: write
        // the field, clear the variant (it belonged to the old model) and
        // commit the page here, or the pick would vanish on reopen
        let mut picker = FilteredPicker::new(
            spec,
            PickerTarget::Callback(Box::new({
                let state = state.clone();
                move |value, window, cx| {
                    state.update(cx, |state, cx| state.set_value(value, window, cx));
                    let Some(page) = page.upgrade() else {
                        return;
                    };
                    page.update(cx, |this, cx| {
                        let variant = this
                            .settings_fields
                            .iter()
                            .find(|(key, _)| *key == "variant")
                            .map(|(_, field)| field.clone());
                        if let Some(variant) = variant {
                            variant.update(cx, |state, cx| state.set_value("", window, cx));
                        }
                        this.commit_settings(cx);
                        cx.notify();
                    });
                }
            })),
            data,
            window,
            cx,
        );
        let refresh_store = store.clone();
        let refresh_harness = harness.clone();
        picker.set_on_refresh(Box::new(move |cx| {
            let harness = refresh_harness.read(cx).0;
            refresh_store.update(cx, |store, cx| store.refresh_models(harness, cx));
        }));
        picker.observe_entity(&store, cx);
        picker.observe_entity(&state, cx);
        picker.observe_entity(&harness, cx);
        picker
    })
}

/// One render's worth of the model catalog: the server-default row, provider
/// groups and the filtered, deduped models.
fn model_data(
    state: &Entity<InputState>,
    harness: &Entity<crate::ui::settings::HarnessChoice>,
    store: &Entity<Store>,
    filter: &str,
    cx: &App,
) -> PickerData {
    let fonts = store.read(cx).fonts;
    let accent = store.read(cx).accent.accent;
    let selected = state.read(cx).value().trim().to_string();
    let harness = harness.read(cx).0;
    let refreshing = store.read(cx).models_refreshing == Some(harness);
    let filter = filter.to_lowercase();
    let mut models: Vec<String> = store
        .read(cx)
        .model_keys_for(harness)
        .into_iter()
        .filter(|model| {
            let model = model.trim();
            !model.is_empty()
                && !model.starts_with('(')
                && (filter.is_empty() || model.to_lowercase().contains(&filter))
        })
        .collect();
    // one lowercase allocation per model, not per comparison
    models.sort_by_cached_key(|model| (model_provider(model).to_lowercase(), model.to_lowercase()));
    models.dedup_by(|a, b| a.eq_ignore_ascii_case(b));

    let mut rows = Vec::with_capacity(models.len() + 4);
    rows.push(PickerRow::Item(PickerItem {
        id: "model-default".into(),
        selector: "model-default".into(),
        value: String::new(),
        label: "Server default".to_string(),
        selected: selected.is_empty(),
        muted: false,
        trailing_delete: None,
    }));
    if models.is_empty() {
        rows.push(PickerRow::Message {
            id: "model-message".into(),
            label: match harness {
                crate::settings::HarnessKind::Dsh => {
                    "No dsh models yet. Refresh once dsh is running.".to_string()
                }
                crate::settings::HarnessKind::OpenCode => {
                    "No models yet. Start a workspace or refresh.".to_string()
                }
            },
        });
    } else {
        let mut last_provider = String::new();
        for model in models {
            let provider = model_provider(&model).to_string();
            if !provider.is_empty() && provider != last_provider {
                rows.push(PickerRow::Group {
                    id: format!("model-provider-{provider}"),
                    label: provider.to_uppercase(),
                    selector: None,
                });
                last_provider = provider;
            }
            rows.push(PickerRow::Item(PickerItem {
                id: format!("model-{model}"),
                selector: format!("model-{model}"),
                value: model.clone(),
                label: model_name(&model).to_string(),
                selected: model.eq_ignore_ascii_case(&selected),
                muted: false,
                trailing_delete: None,
            }));
        }
    }

    PickerData {
        body: PickerBody::Rows(rows),
        selected: Selection {
            badge: (!model_provider(&selected).is_empty())
                .then(|| model_provider(&selected).to_string()),
            label: if selected.is_empty() {
                "Server default".to_string()
            } else {
                model_name(&selected).to_string()
            },
            value: selected,
        },
        footer: None,
        fonts,
        accent,
        show_tooltips: false,
        refreshing,
    }
}

/// The settings "variant" slot: the selected model's variants (as the
/// catalog reports them), written straight into the hidden variant field.
fn settings_variant_picker(
    state: Entity<InputState>,
    model: Entity<InputState>,
    store: Entity<Store>,
    harness: Entity<crate::ui::settings::HarnessChoice>,
    window: &mut Window,
    cx: &mut Context<WarpWindow>,
) -> Entity<FilteredPicker> {
    let spec = PickerSpec {
        key: "variant",
        trigger_label: "Variant",
        filter_placeholder: "Filter variants",
        icons: PICKER_ICONS,
        trigger_icon: None,
        provider_badge: false,
        refresh: false,
        refresh_label: "",
        stop_trigger_mousedown: false,
        visible_rows: 10,
        selected_accent: false,
        // a settings slot floats: the list covers the rows under it instead
        // of stretching the form around itself
        overlay: true,
        anchor: OverlayAnchor::Field,
        trigger_scale: TriggerScale::Regular,
        trigger_rounded_lg: true,
        trigger_min_h: Some(26.),
        panel_chrome: PanelChrome::Cards,
        trigger_id: Some("variant-dd-toggle"),
        panel_id: None,
        filter_selector: Some("variant-dd-filter"),
        list_id: Some("variant-dd-list"),
    };
    // the page that owns the field: a pick must commit through it, since
    // set_value emits no Change event for the field's subscription. Weak, so
    // the picker's callback cannot keep the window (and itself) alive.
    let page = cx.entity().downgrade();
    cx.new(|cx| {
        let data = {
            let state = state.clone();
            let model = model.clone();
            let store = store.clone();
            let harness = harness.clone();
            Box::new(move |filter: &str, _window: &Window, cx: &App| {
                variant_data(&state, &model, &harness, &store, filter, cx)
            }) as Box<dyn Fn(&str, &Window, &App) -> PickerData>
        };
        // a pick lands through set_value, which emits no Change event: write
        // the field and commit the page here, or the pick would vanish on
        // reopen
        let mut picker = FilteredPicker::new(
            spec,
            PickerTarget::Callback(Box::new({
                let state = state.clone();
                move |value, window, cx| {
                    state.update(cx, |state, cx| state.set_value(value, window, cx));
                    let Some(page) = page.upgrade() else {
                        return;
                    };
                    page.update(cx, |this, cx| {
                        this.commit_settings(cx);
                        cx.notify();
                    });
                }
            })),
            data,
            window,
            cx,
        );
        picker.observe_entity(&store, cx);
        picker.observe_entity(&state, cx);
        picker.observe_entity(&model, cx);
        picker.observe_entity(&harness, cx);
        picker
    })
}

/// One render's worth of the Variant picker: the "Default" row, then the
/// selected model's variants, or a message saying why there are none.
fn variant_data(
    state: &Entity<InputState>,
    model: &Entity<InputState>,
    harness: &Entity<crate::ui::settings::HarnessChoice>,
    store: &Entity<Store>,
    filter: &str,
    cx: &App,
) -> PickerData {
    let fonts = store.read(cx).fonts;
    let accent = store.read(cx).accent.accent;
    let model = model.read(cx).value().trim().to_string();
    let selected = state.read(cx).value().trim().to_string();
    let harness = harness.read(cx).0;
    let filter = filter.to_lowercase();
    let variants: Vec<String> = store
        .read(cx)
        .variants_for(harness, &model)
        .into_iter()
        .filter(|variant| filter.is_empty() || variant.to_lowercase().contains(&filter))
        .collect();

    PickerData {
        body: PickerBody::Rows(variant_rows(&model, &variants, &selected)),
        selected: Selection {
            badge: None,
            label: if selected.is_empty() {
                "Default".to_string()
            } else {
                selected.clone()
            },
            value: selected,
        },
        footer: None,
        fonts,
        accent,
        show_tooltips: false,
        refreshing: false,
    }
}

/// The rows under the Variant filter. An empty model says what to pick first;
/// a model with no tiers says so instead of showing an empty list.
fn variant_rows(model: &str, variants: &[String], selected: &str) -> Vec<PickerRow> {
    let mut rows = Vec::with_capacity(variants.len() + 2);
    rows.push(PickerRow::Item(PickerItem {
        id: "variant-row-default".into(),
        selector: "variant-row-default".into(),
        value: String::new(),
        label: "Default".to_string(),
        selected: selected.is_empty(),
        muted: false,
        trailing_delete: None,
    }));
    if model.is_empty() {
        rows.push(PickerRow::Message {
            id: "variant-message-pick-model".into(),
            label: "Pick a model to see its variants.".to_string(),
        });
    } else if variants.is_empty() {
        rows.push(PickerRow::Message {
            id: "variant-message-empty".into(),
            label: "This model reports no variants.".to_string(),
        });
    } else {
        for variant in variants {
            rows.push(PickerRow::Item(PickerItem {
                id: format!("variant-row-{variant}"),
                selector: format!("variant-row-{variant}"),
                value: variant.clone(),
                label: variant.clone(),
                selected: variant.eq_ignore_ascii_case(selected),
                muted: false,
                trailing_delete: None,
            }));
        }
    }
    rows
}

#[cfg(test)]
mod scope_tests {
    use super::{SidebarScope, chat_in_scope, workspace_in_scope};
    use crate::settings::WorkspaceState;
    use crate::store::{SessionItem, WorkspaceItem};

    fn chat(id: &str, running: bool) -> SessionItem {
        SessionItem {
            id: id.into(),
            title: id.into(),
            updated_ms: 0,
            created_ms: 0,
            is_pinned: false,
            is_selected: false,
            is_running: running,
            is_editing: false,
            draft_title: String::new(),
        }
    }

    fn workspace(path: &str, favorite: bool, chats: Vec<SessionItem>) -> WorkspaceItem {
        WorkspaceItem {
            state: WorkspaceState {
                path: path.into(),
                ..Default::default()
            },
            name: path.rsplit('/').next().unwrap_or(path).into(),
            is_running: false,
            is_selected: false,
            is_favorite: favorite,
            chats,
        }
    }

    #[test]
    fn each_scope_keeps_its_own_chats() {
        let plain = chat("plain", false);
        let running = chat("running", true);

        assert!(chat_in_scope(&plain, SidebarScope::All, false));
        assert!(!chat_in_scope(&plain, SidebarScope::NeedsYou, false));

        assert!(chat_in_scope(&plain, SidebarScope::NeedsYou, true));
        assert!(!chat_in_scope(&running, SidebarScope::NeedsYou, false));

        // favorites are workspace-level: every chat of a favorite workspace
        // belongs to that scope
        assert!(chat_in_scope(&plain, SidebarScope::Favorites, false));
        assert!(chat_in_scope(&running, SidebarScope::Favorites, false));

        assert!(chat_in_scope(&running, SidebarScope::Running, false));
        assert!(!chat_in_scope(&plain, SidebarScope::Running, false));
    }

    #[test]
    fn favorites_keep_a_starred_workspace_without_matching_chats() {
        let starred = workspace("/tmp/starred", true, vec![chat("plain", false)]);
        let other = workspace("/tmp/other", false, vec![chat("plain", false)]);

        assert!(workspace_in_scope(&starred, SidebarScope::Favorites, 0));
        // a chat-level match no longer pulls a non-favorite workspace in
        assert!(!workspace_in_scope(&other, SidebarScope::Favorites, 1));
    }

    #[test]
    fn attention_and_running_keep_only_parents_with_matches() {
        let waiting = workspace("/tmp/waiting", false, vec![chat("waiting", false)]);
        let idle = workspace("/tmp/idle", false, vec![chat("idle", false)]);

        assert!(workspace_in_scope(&waiting, SidebarScope::NeedsYou, 1));
        assert!(!workspace_in_scope(&idle, SidebarScope::NeedsYou, 0));
        assert!(workspace_in_scope(&waiting, SidebarScope::Running, 1));
        assert!(!workspace_in_scope(&idle, SidebarScope::Running, 0));
        assert!(workspace_in_scope(&idle, SidebarScope::All, 0));
    }
}

#[cfg(test)]
mod harness_tests {
    use super::harness_choices;
    use crate::settings::HarnessKind;

    /// The regression this pins: a display label ("DeepSeek (dsh)") used as
    /// the choice key made every click select opencode, and the dsh button
    /// never rendered as selected.
    #[test]
    fn the_harness_choice_keys_are_wire_ids_and_exactly_one_is_selected() {
        for (current, expected) in [
            (HarnessKind::OpenCode, "opencode"),
            (HarnessKind::Dsh, "dsh"),
        ] {
            let choices = harness_choices(current);
            assert_eq!(choices.len(), 2);
            let selected: Vec<&str> = choices
                .iter()
                .filter(|choice| choice.selected)
                .map(|choice| choice.key.as_ref())
                .collect();
            assert_eq!(selected, [expected], "current = {current:?}");

            let dsh = choices
                .iter()
                .find(|choice| choice.key == "dsh")
                .expect("the dsh choice is keyed by its wire id");
            assert_eq!(dsh.label.as_ref(), "DeepSeek (dsh)");
            // the display label is not a wire id: a past regression compared
            // against "DeepSeek (dsh)" and could never select the harness
            assert_eq!(HarnessKind::from_wire("DeepSeek (dsh)"), None);
        }
    }
}

#[cfg(test)]
mod variant_rows_tests {
    use super::{PickerRow, variant_rows};

    fn labels(rows: &[PickerRow]) -> Vec<String> {
        rows.iter()
            .filter_map(|row| match row {
                PickerRow::Item(item) => Some(item.label.clone()),
                _ => None,
            })
            .collect()
    }

    fn message(rows: &[PickerRow]) -> Option<&str> {
        rows.iter().find_map(|row| match row {
            PickerRow::Message { label, .. } => Some(label.as_str()),
            _ => None,
        })
    }

    #[test]
    fn an_empty_model_asks_for_one_instead_of_listing_nothing() {
        let rows = variant_rows("", &[], "");
        assert_eq!(labels(&rows), ["Default"]);
        assert_eq!(message(&rows), Some("Pick a model to see its variants."));
    }

    #[test]
    fn a_model_without_tiers_explains_the_empty_list() {
        let rows = variant_rows("zai/glm", &[], "");
        assert_eq!(labels(&rows), ["Default"]);
        assert_eq!(message(&rows), Some("This model reports no variants."));
    }

    #[test]
    fn the_models_variants_follow_default_and_the_saved_pick_is_selected() {
        let rows = variant_rows("zai/glm", &["low".into(), "max".into()], "max");
        assert_eq!(labels(&rows), ["Default", "low", "max"]);
        assert_eq!(message(&rows), None);
        let selected: Vec<&str> = rows
            .iter()
            .filter_map(|row| match row {
                PickerRow::Item(item) if item.selected => Some(item.value.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(selected, ["max"], "Default is not selected once a tier is");
    }
}
