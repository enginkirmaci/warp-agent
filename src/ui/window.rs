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
    App, Context, Entity, Focusable, MouseButton, Render, Role, SharedString, StyleRefinement,
    Toggled, Window, WindowControlArea, canvas, div, px, uniform_list,
};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use std::collections::HashMap;
use std::rc::Rc;
use warpui_kit_core::A11y as _;
use warpui_kit_core::shell;
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

/// One track of the grid inside `grid_width`.
fn tile_track_width(grid_width: f32, columns: usize) -> f32 {
    let columns = columns.max(1) as f32;
    (grid_width - GRID_GAP * (columns + 1.)) / columns
}

#[derive(Clone)]
pub enum SideMenuAction {
    OpenFolder(String),
    RemoveWorkspace(String),
    PinChat(String),
    UnpinChat(String),
    RenameChat(String),
    UnlinkChat(String),
    DeleteChat(String),
}

impl SideMenuAction {
    fn label(&self) -> &'static str {
        match self {
            SideMenuAction::OpenFolder(_) => "Open folder",
            SideMenuAction::RemoveWorkspace(_) => "Remove workspace",
            SideMenuAction::PinChat(_) => "Pin",
            SideMenuAction::UnpinChat(_) => "Unpin",
            SideMenuAction::RenameChat(_) => "Rename",
            SideMenuAction::UnlinkChat(_) => "Remove",
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

/// Harness-independent workspace menu: servers are auto-managed (expanding a
/// workspace connects them), so the only entries are the folder itself and
/// removal. The built-in General workspace has neither — no menu at all.
fn ws_menu_rows(store: &Store, path: &str) -> Vec<SideMenuAction> {
    let Some(ws) = store.workspaces.iter().find(|w| w.folder_path() == path) else {
        return Vec::new();
    };
    if ws.is_general() {
        return Vec::new();
    }
    vec![
        SideMenuAction::OpenFolder(path.to_string()),
        SideMenuAction::RemoveWorkspace(path.to_string()),
    ]
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
        SideMenuAction::UnlinkChat(id.into()),
        SideMenuAction::DeleteChat(id.into()),
    ]
}

pub struct WarpWindow {
    pub store: Entity<Store>,
    tile_views: HashMap<String, Entity<TileView>>,
    /// the app's tooltip overlay (also reachable via the TooltipHost global);
    /// rendered as the last child so popups paint above everything
    tooltip_overlay: Entity<gpui_kit::base::TooltipOverlay>,
    menu_sender_cell: async_channel::Sender<MenuState>,
    menu_receiver: async_channel::Receiver<MenuState>,
    menu: Option<MenuState>,
    add_folder_sender: async_channel::Sender<String>,
    add_folder_receiver: async_channel::Receiver<String>,
    settings_buffers: SettingsBuffers,
    /// The Settings page's open section.
    settings_section: SettingsSection,
    /// E2E hook: WARP_AGENT_PAGE=settings[-chat|-permissions|-server] opens
    /// the rail's Settings section after the first window exists (the pointer
    /// cannot reach a hidden window, and probes need the page).
    open_settings_on_first_render: Option<SettingsSection>,
    settings_fields: Vec<(&'static str, Entity<InputState>)>,
    /// leaf renderers for the plain settings fields — keystrokes repaint
    /// only the field, never the window (see InputLeaf below)
    settings_leafs: Vec<(&'static str, Entity<InputLeaf>)>,
    /// the Model field + its standalone picker component
    settings_model_picker: Option<Entity<FilteredPicker>>,
    /// session being renamed + its edit field (state for the Enter
    /// subscription, leaf for the per-keystroke renderer)
    rename: Option<(String, Entity<InputState>, Entity<InputLeaf>)>,
    /// workspace-name filter text; keystrokes land in the sidebar filter
    /// leaf (InputLeaf) and only a Change event copies the text out and
    /// nudges the window to refilter the list
    sidebar_filter: String,
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
}

impl WarpWindow {
    pub fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        let (menu_tx, menu_rx) = async_channel::unbounded::<MenuState>();
        let (folder_tx, folder_rx) = async_channel::unbounded::<String>();
        let settings_buffers = SettingsBuffers::from_settings(&store.read(cx).settings);
        let font_meta = store.read(cx).fonts.font_meta;
        let tooltip_overlay = tooltip::install(cx, font_meta);
        Self {
            store,
            tile_views: HashMap::new(),
            tooltip_overlay,
            menu_sender_cell: menu_tx,
            menu_receiver: menu_rx,
            menu: None,
            add_folder_sender: folder_tx,
            add_folder_receiver: folder_rx,
            settings_buffers,
            settings_section: SettingsSection::Appearance,
            open_settings_on_first_render: match std::env::var("WARP_AGENT_PAGE").as_deref() {
                Ok("settings") => Some(SettingsSection::Appearance),
                Ok("settings-chat") => Some(SettingsSection::ChatDefaults),
                Ok("settings-permissions") => Some(SettingsSection::Permissions),
                Ok("settings-server") => Some(SettingsSection::Server),
                _ => None,
            },
            settings_fields: Vec::new(),
            settings_leafs: Vec::new(),
            settings_model_picker: None,
            rename: None,
            sidebar_filter: String::new(),
            sidebar_filter_leaf: None,
            sidebar_labels: HashMap::new(),
            sidebar_labels_minute: -1,
            splitter_drag: std::rc::Rc::new(std::cell::Cell::new(None)),
        }
    }
}

/// One stable key per menu action, so arming a destructive row survives the
/// re-render that shows Confirm.
fn menu_action_key(action: &SideMenuAction) -> String {
    match action {
        SideMenuAction::OpenFolder(path) => format!("open-folder:{path}"),
        SideMenuAction::RemoveWorkspace(path) => format!("remove-workspace:{path}"),
        SideMenuAction::PinChat(id) | SideMenuAction::UnpinChat(id) => format!("pin:{id}"),
        SideMenuAction::RenameChat(id) => format!("rename:{id}"),
        SideMenuAction::UnlinkChat(id) => format!("unlink:{id}"),
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
        SideMenuAction::PinChat(id) => s.toggle_pin(id, cx),
        SideMenuAction::UnpinChat(id) => s.toggle_pin(id, cx),
        SideMenuAction::RenameChat(id) => s.begin_rename(id, cx),
        SideMenuAction::UnlinkChat(id) => s.remove_from_workspace(id, cx),
        SideMenuAction::DeleteChat(id) => s.delete_session(id, cx),
    });
}

impl Render for WarpWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        if let Some(section) = self.open_settings_on_first_render.take() {
            self.open_settings(window, cx);
            self.settings_section = section;
        }
        // pending folder picks + menu requests from App-only closures
        while let Ok(path) = self.add_folder_receiver.try_recv() {
            self.store.update(cx, |s, cx| s.add_workspace(&path, cx));
        }
        while let Ok(menu) = self.menu_receiver.try_recv() {
            self.menu = Some(menu);
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

        let list_shown = sidebar_visible && !settings_open && !self.grid_takes_the_list(window, cx);
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
            // mounts the window's selection machinery: without this layer the
            // kit's global mouse handlers never register and selectable
            // TextViews (chat transcripts) ignore drags entirely
            .child(gpui_kit::base::TextSelectionLayer)
            // Ctrl+C fallback for transcript selections when no input is
            // focused (mirrors gpui-base's Root)
            .on_action(|_: &gpui_kit::base::input::Copy, window, cx| {
                let text = gpui_kit::base::TextSelection::selected_text(window, cx)
                    .trim()
                    .to_string();
                if text.is_empty() {
                    cx.propagate();
                    return;
                }
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            })
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
                        div()
                            .absolute()
                            .left(px(menu.x))
                            .top(px(menu.y))
                            .child(popover_card().min_w(px(180.)).child(rows)),
                    ),
            );
        }

        // tooltips paint above everything, menus included
        root.child(self.tooltip_overlay.clone())
    }
}

/// Windows-style window-state glyph for the titlebar zoom button: a single
/// square while windowed, overlapping squares (restore) while maximized. The
/// stroke doubles as the server-health gauge: its color follows the running
/// workspaces (amber reconnecting, salmon unhealthy, neutral otherwise).
fn zoom_icon(maximized: bool, color: gpui::Rgba) -> gpui::AnyElement {
    let stroke = 1.1_f32;
    let s = 9.5_f32;
    // one Bounds per painted edge: [x, y, w, h] in the 14x14 icon box
    let mut edges: Vec<(f32, f32, f32, f32)> = Vec::new();
    if maximized {
        // back square offset up-right; only its top and right edges peek out
        edges.push((3.5, 1., s, stroke));
        edges.push((3.5 + s - stroke, 1., stroke, s));
        // front square offset down-left
        let (fx, fy) = (1.25, 4.);
        edges.push((fx, fy, s, stroke));
        edges.push((fx, fy + s - stroke, s, stroke));
        edges.push((fx, fy, stroke, s));
        edges.push((fx + s - stroke, fy, stroke, s));
    } else {
        let (x, y) = (2.25, 2.25);
        edges.push((x, y, s, stroke));
        edges.push((x, y + s - stroke, s, stroke));
        edges.push((x, y, stroke, s));
        edges.push((x + s - stroke, y, stroke, s));
    }
    let gauge = canvas(
        move |_, _, _| edges,
        move |bounds, edges, window, _| {
            for (ex, ey, ew, eh) in edges {
                window.paint_quad(gpui::quad(
                    gpui::Bounds {
                        origin: gpui::point(bounds.origin.x + px(ex), bounds.origin.y + px(ey)),
                        size: gpui::size(px(ew), px(eh)),
                    },
                    gpui::Corners::all(px(0.)),
                    color,
                    gpui::Edges::all(px(0.)),
                    gpui::transparent_black(),
                    gpui::BorderStyle::default(),
                ));
            }
        },
    );
    div().size(px(14.)).child(gauge).into_any_element()
}

/// The settings-page composer state: labeled buffers, labeled leaves and the
/// optional model picker.
type SettingsFields = (
    Vec<(&'static str, Entity<InputState>)>,
    Vec<(&'static str, Entity<InputLeaf>)>,
    Option<Entity<FilteredPicker>>,
);

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
                    if matches!(event, InputEvent::PressEnter { .. }) {
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
                    disabled: false,
                });
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
            && self.store.read(cx).open_tiles.len() >= 2
    }

    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.store.read(cx).is_settings_open {
            return;
        }
        self.store.update(cx, |s, _| {
            s.is_settings_open = true;
        });
        self.settings_buffers = SettingsBuffers::from_settings(&self.store.read(cx).settings);
        let (fields, leafs, model_picker) = self.make_settings_fields(window, cx);
        self.settings_fields = fields;
        self.settings_leafs = leafs;
        self.settings_model_picker = model_picker;
        cx.notify();
    }

    /// Leaving the page discards unsaved edits: the design gives Settings a
    /// Save button, and Appearance applies on its own.
    fn close_settings(&mut self, cx: &mut Context<Self>) {
        if !self.store.read(cx).is_settings_open {
            return;
        }
        self.store.update(cx, |s, _| s.is_settings_open = false);
        cx.notify();
    }

    fn render_rail(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (in_settings, ink, attention) = {
            let store = self.store.read(cx);
            (
                store.is_settings_open,
                store.accent.text,
                store.has_attention_needed(),
            )
        };
        shell::rail()
            .debug_selector(|| "app-rail".into())
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                gpui_kit::base::GlobalState::suppress_text_selection(cx);
            })
            .child(
                shell::rail_item_inked(
                    "rail-chats",
                    "icons/chat-outline.svg",
                    "Chats",
                    !in_settings,
                    ink,
                )
                .debug_selector(|| "rail-chats".into())
                .relative()
                .on_click(cx.listener(|this, _, _, cx| this.close_settings(cx)))
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
            store.open_tiles.iter().filter(|t| t.is_busy(store)).count()
        };
        shell::title_bar()
            .pl_3()
            // titlebar chrome owns no selectable text either (see sidebar note)
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                gpui_kit::base::GlobalState::suppress_text_selection(cx);
            })
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(28.))
                    .rounded(px(6.))
                    .bg(theme::brand_bg())
                    .border_1()
                    .border_color(theme::BRAND_BORDER)
                    // the app's own mark, in the family's currentColor glyph
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
                d.child(
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
                )
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
                .child(zoom_icon(window.is_maximized(), gauge_color)),
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
                disabled: false,
            }));
        }
        let filter = self.sidebar_filter.to_lowercase();
        let filter_leaf = self.sidebar_filter_leaf.clone().unwrap();

        // The sidebar flattened to its rows and virtualized: a workspace can
        // hold hundreds of chats and every row here is one fixed line, so the
        // old loop built the whole tree — each row's listeners, icons and
        // menus — on every repaint. `uniform_list` builds a screenful.
        let rows = Rc::new(self.sidebar_rows(&filter, cx));

        let (need, running, open) = {
            let store = self.store.read(cx);
            (
                store.tiles_waiting(),
                store.open_tiles.iter().filter(|t| t.is_busy(store)).count(),
                store.open_tiles.len(),
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

        shell::list_pane(width)
            .debug_selector(|| "agent-list".into())
            // the sidebar owns no selectable text: a press here (row, + button)
            // must not seed a window drag-selection — otherwise the gesture
            // that starts on a row can land on transcript text and select it
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                gpui_kit::base::GlobalState::suppress_text_selection(cx);
            })
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
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(fonts.font_list_title))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .child("Workspaces"),
                            )
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
                                    px(fonts.icon_medium),
                                    theme::text_dim(),
                                )),
                            ),
                    )
                    .child(summary)
                    .child(
                        // the kit Input has no a11y of its own — the wrapper carries the
                        // role/value; an AT click lands at the wrapper's center, which is
                        // the input itself, so focus still reaches the real field
                        div()
                            .id("sidebar-filter")
                            .role(Role::SearchInput)
                            .track_focus(&filter_leaf.read(cx).state.read(cx).focus_handle(cx))
                            .aria_label("Filter workspaces")
                            .aria_placeholder("Filter workspaces")
                            .aria_value(self.sidebar_filter.clone())
                            .w_full()
                            .min_w_0()
                            .child(filter_leaf),
                    ),
            )
            .child({
                // "nothing to show" is about real rows only: a placeholder
                // hint inside the list is not a workspace, and with no
                // workspaces at all (or none matching) the pane says so
                // instead of drawing an empty virtualized list
                let has_workspaces = rows.iter().any(|r| matches!(r, SidebarRow::Workspace(_)));
                let hint = if filter.is_empty() {
                    "No workspaces yet"
                } else {
                    "No matching workspace"
                };
                if !has_workspaces {
                    div()
                        .id("sidebar-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .pt_1()
                        .pb_3()
                        .child(
                            div()
                                .pl(px(40.))
                                .py_2()
                                .text_size(px(fonts.font_meta))
                                .italic()
                                .text_color(theme::text_muted())
                                .child(hint),
                        )
                        .into_any_element()
                } else {
                    uniform_list(
                        SharedString::from("sidebar-rows"),
                        rows.len(),
                        cx.processor(move |this, range: std::ops::Range<usize>, _window, cx| {
                            range
                                .map(|ix| this.render_sidebar_row(&rows[ix], cx))
                                .collect()
                        }),
                    )
                    .id("sidebar-list")
                    .flex_1()
                    .min_h_0()
                    .pt_1()
                    .pb_3()
                    .w_full()
                    .into_any_element()
                }
            })
            .into_any_element()
    }

    /// The sidebar as one flat row list: a workspace, the hint under an empty
    /// one, then its chats. Flattening keeps the virtualized list indexable
    /// while the filter still hides whole workspaces.
    fn sidebar_rows(&self, filter: &str, cx: &App) -> Vec<SidebarRow> {
        let mut rows = Vec::new();
        for ws in &self.store.read(cx).workspaces {
            if !filter.is_empty() && !ws.name.to_lowercase().contains(filter) {
                continue;
            }
            rows.push(SidebarRow::Workspace(ws.clone()));
            if !ws.is_expanded() {
                continue;
            }
            if ws.show_empty_hint() {
                rows.push(SidebarRow::Placeholder("No chats yet"));
            }
            for chat in &ws.chats {
                rows.push(SidebarRow::Chat(chat.clone()));
            }
        }
        rows
    }

    /// One sidebar row, at the virtualized list's fixed pitch.
    fn render_sidebar_row(&mut self, row: &SidebarRow, cx: &mut Context<Self>) -> gpui::AnyElement {
        match row {
            SidebarRow::Workspace(ws) => self.render_workspace_row(ws, cx),
            SidebarRow::Chat(chat) => self.render_session_row(chat, cx),
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
        }
    }

    fn render_workspace_row(
        &mut self,
        ws: &WorkspaceItem,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let fonts = self.store.read(cx).fonts;
        let path = ws.folder_path().to_string();
        let glyph: gpui::AnyElement = if ws.is_general() {
            crate::ui::icon(
                "icons/chat-outline.svg",
                px(fonts.icon_medium),
                theme::text_muted(),
            )
            .into_any_element()
        } else if ws.is_expanded() {
            crate::ui::icon(
                "icons/folder-open-outline.svg",
                px(fonts.icon_medium),
                theme::text_muted(),
            )
            .into_any_element()
        } else {
            crate::ui::icon(
                "icons/folder-outline.svg",
                px(fonts.icon_medium),
                theme::text_muted(),
            )
            .into_any_element()
        };

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
                    cx.notify();
                });
            })
            .on_mouse_down(
                MouseButton::Right,
                move |event: &gpui::MouseDownEvent, _, cx| {
                    let rows = {
                        let s = menu_store.read(cx);
                        ws_menu_rows(s, &menu_path)
                    };
                    if rows.is_empty() {
                        return; // General: no workspace menu at all
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
            .child(glyph)
            // no flex_1: the row packs left so the count hugs the name; a long
            // name shrinks (min_w_0) and truncates instead of pushing the
            // count to the row's far edge
            .child(
                div()
                    .min_w_0()
                    .ml_2()
                    .text_size(px(fonts.font_name))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .truncate()
                    .child(SharedString::from(ws.name.clone())),
            )
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
            // running dot pinned to the right edge; hovering the row fades it
            // out while the kebab/new-chat buttons fade in at the same spot.
            // Lit by RUNNING sessions in this workspace — server-up alone
            // must not read as "work in progress"
            .child(
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
            .child(
                div()
                    .absolute()
                    .right_2()
                    .top_0()
                    .bottom_0()
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .opacity(0.)
                    .group_hover("wsrow", |s| s.opacity(1.))
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
                    .when(!ws.is_general(), |d| {
                        d.child(
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
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let (fonts, accent, harness_label, waiting) = {
            let store = self.store.read(cx);
            (
                store.fonts,
                store.accent.accent,
                store.session_harness(&chat.id).label(),
                store.session_waiting(&chat.id),
            )
        };
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
        } else if chat.pin_visible() {
            crate::ui::icon("icons/pin.svg", px(fonts.icon_small), theme::text_muted())
                .into_any_element()
        } else {
            div().into_any_element()
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
            .h(px(SIDEBAR_ROW_HEIGHT))
            .pl(px(24.))
            .pr_3()
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
            .on_click(move |_, _, cx| {
                tracing::info!("chat-row clicked {}", id_click);
                store_click.update(cx, |s, cx| s.select_session(&id_click, cx));
            })
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
                row = row.child(div().flex_1().min_w_0().child(leaf.clone()));
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
                // right meta cluster in flow: which backend the chat runs on,
                // then the time. Hovering the row fades the cluster out and
                // fades the delete button in at the same spot (absolute layer,
                // so nothing shifts)
                .child(
                    div()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .gap_1()
                        .group_hover("chatrow", |s| s.opacity(0.))
                        .ml_2()
                        .text_size(px(fonts.font_meta))
                        .text_color(theme::text_muted())
                        .child(SharedString::from(harness_label))
                        .child(self.cached_time_label(chat)),
                )
                // the design's ⋯: Pin, Rename, Remove and Delete chat (which
                // arms before it runs), the same menu the right-click opens
                .child(
                    div()
                        .absolute()
                        .right_1()
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .opacity(0.)
                        .group_hover("chatrow", |s| s.opacity(1.))
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
                                    .occlude()
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
            ("executable", b.open_code_executable),
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
        for (key, value) in rows {
            if key == "model" {
                let state = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("")
                        .default_value(value)
                });
                let store = self.store.clone();
                let picker = settings_model_picker(state.clone(), store, window, cx);
                fields.push((key, state));
                model_picker = Some(picker);
            } else {
                let entity = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("")
                        .default_value(value)
                });
                let leaf = cx.new(|_| InputLeaf {
                    state: entity.clone(),
                    disabled: false,
                });
                fields.push((key, entity));
                leafs.push((key, leaf));
            }
        }
        (fields, leafs, model_picker)
    }

    fn settings_field_wrap(&self, key: &str, cx: &mut App) -> gpui::AnyElement {
        // the Variant default is inert while "always highest" is checked;
        // pushed into the leaf with a click-rate notify when it flips
        let disabled = key == "variant" && self.settings_buffers.variant_highest;
        self.settings_leafs
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, leaf)| {
                leaf.update(cx, |leaf, cx| {
                    if leaf.disabled != disabled {
                        leaf.disabled = disabled;
                        cx.notify();
                    }
                });
                // the kit Input has no a11y of its own — the wrapper carries the
                // role/value; an AT click lands at the wrapper's center, which
                // is the input itself
                let state = leaf.read(cx).state.clone();
                let value = state.read(cx).value().to_string();
                div()
                    .id(gpui::ElementId::Name(SharedString::from(format!(
                        "field-{key}"
                    ))))
                    .role(Role::TextInput)
                    .track_focus(&state.read(cx).focus_handle(cx))
                    .aria_label(SharedString::from(format!("{key} field")))
                    .aria_value(value)
                    .flex_1()
                    .min_w_0()
                    .child(leaf.clone())
                    .into_any_element()
            })
            .unwrap_or_else(|| div().into_any_element())
    }

    fn settings_hint(&self, text: &str, fonts: &theme::FontTokens) -> gpui::AnyElement {
        div()
            .text_size(px(fonts.font_body))
            .italic()
            .text_color(theme::text_faint())
            .child(text.to_string())
            .into_any_element()
    }

    fn settings_card(&self, children: Vec<gpui::AnyElement>) -> gpui::AnyElement {
        div()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(theme::hairline())
            .bg(theme::settings_card())
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .children(children)
            .into_any_element()
    }

    fn check_row(
        &self,
        text: &str,
        checked: bool,
        fonts: &theme::FontTokens,
        on_toggle: impl Fn(&mut SettingsBuffers) + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let accent = self.store.read(cx).accent.accent;
        let marker: gpui::AnyElement = if checked {
            div()
                .size(px(14.))
                .rounded_sm()
                .bg(accent)
                .flex()
                .items_center()
                .justify_center()
                .child(crate::ui::icon(
                    "icons/check.svg",
                    px(fonts.icon_nano),
                    theme::surface(),
                ))
                .into_any_element()
        } else {
            div()
                .size(px(14.))
                .rounded_sm()
                .border_1()
                .border_color(theme::text_muted())
                .into_any_element()
        };
        div()
            .id(gpui::ElementId::NamedInteger(
                "chk".into(),
                TileView::hash(text),
            ))
            .role(Role::CheckBox)
            .aria_label(text)
            .aria_toggled(if checked {
                Toggled::True
            } else {
                Toggled::False
            })
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                on_toggle(&mut this.settings_buffers);
                cx.notify();
            }))
            .child(marker)
            .child(
                div()
                    .text_size(px(fonts.font_heading))
                    .text_color(theme::text())
                    .child(text.to_string()),
            )
            .into_any_element()
    }

    fn radio_row(
        &self,
        options: &[(&'static str, &'static str)],
        selected: &str,
        fonts: &theme::FontTokens,
        on_pick: impl Fn(&mut SettingsBuffers, &'static str) + Copy + 'static,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let mut row = div().flex().items_center().gap_3p5();
        let accent = self.store.read(cx).accent.accent;
        for (key_ref, label_text) in options {
            let key: &'static str = key_ref;
            let is_on = key == selected;
            row = row.child(
                div()
                    .id(gpui::ElementId::NamedInteger(
                        "radio".into(),
                        TileView::hash(key),
                    ))
                    .role(Role::RadioButton)
                    .aria_label(*label_text)
                    .aria_selected(is_on)
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        on_pick(&mut this.settings_buffers, key);
                        cx.notify();
                    }))
                    .child(if is_on {
                        div()
                            .size(px(11.))
                            .rounded_full()
                            .bg(accent)
                            .into_any_element()
                    } else {
                        div()
                            .size(px(11.))
                            .rounded_full()
                            .border_1()
                            .border_color(theme::text_muted())
                            .into_any_element()
                    })
                    .child(
                        div()
                            .text_size(px(fonts.font_heading))
                            .text_color(theme::text())
                            .child((*label_text).to_string()),
                    ),
            );
        }
        row.into_any_element()
    }

    // ---- main area -------------------------------------------------------------

    fn render_main(
        &mut self,
        list_shown: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        // Settings is a rail section of its own: it replaces the grid rather
        // than floating over it
        if self.store.read(cx).is_settings_open {
            return self.render_settings(window, cx);
        }
        let fonts = self.store.read(cx).fonts;

        // keep TileView entities in sync with the store's tile list
        let keys: Vec<String> = self
            .store
            .read(cx)
            .open_tiles
            .iter()
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
            .child(rows);

        let mut head = div()
            .flex_shrink_0()
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
        if active == SettingsSection::Appearance {
            head = head.child(
                div()
                    .text_size(px(fonts.font_meta))
                    .text_color(theme::text_faint())
                    .child("Applied at once"),
            );
        } else {
            head = head.child(
                div()
                    .id("settings-save")
                    .a11y_button("Save settings")
                    .cursor_pointer()
                    .h(px(shell::scaled(shell::CONTROL_HEIGHT)))
                    .px_4()
                    .rounded(px(6.))
                    .flex()
                    .items_center()
                    .text_size(px(fonts.font_control))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .bg(theme::accent())
                    .text_color(theme::on_accent())
                    .hover(|s| s.opacity(0.9))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.commit_settings(cx);
                        cx.notify();
                    }))
                    .child("Save"),
            );
        }

        let mut cards = div()
            .w_full()
            .max_w(px(shell::READING_WIDTH))
            .flex()
            .flex_col()
            .gap_3();
        for card in self.settings_cards(active, &fonts, cx) {
            cards = cards.child(card);
        }

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
                            .min_h_0()
                            .overflow_y_scroll()
                            .p(px(shell::DETAIL_PADDING))
                            .child(cards),
                    ),
            )
            .into_any_element()
    }

    /// One section's rows, grouped into the design's cards.
    fn settings_cards(
        &mut self,
        active: SettingsSection,
        fonts: &theme::FontTokens,
        cx: &mut Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let buffers = self.settings_buffers.clone();
        match active {
            SettingsSection::Appearance => vec![
                self.settings_card(vec![
                    div()
                        .flex()
                        .items_center()
                        .gap_2p5()
                        .child(settings_label("Text size", fonts))
                        .child(warpui_kit_core::text_size_control())
                        .into_any_element(),
                    self.settings_hint(
                        "Applied right away. The other Warp apps follow when they next open.",
                        fonts,
                    ),
                    div().h(px(1.)).bg(theme::hairline()).into_any_element(),
                    div()
                        .flex()
                        .items_center()
                        .gap_2p5()
                        .child(settings_label("Palette", fonts))
                        .child(warpui_kit_core::palette_control())
                        .into_any_element(),
                    self.settings_hint(
                        "Applied right away. The other Warp apps follow when they next open.",
                        fonts,
                    ),
                    div().h(px(1.)).bg(theme::hairline()).into_any_element(),
                    div()
                        .flex()
                        .items_center()
                        .gap_2p5()
                        .child(settings_label("Typeface", fonts))
                        .child(
                            div()
                                .text_size(px(fonts.font_body))
                                .text_color(theme::text())
                                .child("Inter + JetBrains Mono"),
                        )
                        .into_any_element(),
                    self.settings_hint(
                        "The family's two faces, bundled with the app; code, paths and titles set in mono.",
                        fonts,
                    ),
                ]),
                self.settings_card(vec![
                    self.appearance_check(
                        "Reduce motion",
                        buffers.reduce_motion,
                        fonts,
                        |b| b.reduce_motion = !b.reduce_motion,
                        cx,
                    ),
                    self.appearance_check(
                        "Show tooltips on hover",
                        buffers.show_tool_tips,
                        fonts,
                        |b| b.show_tool_tips = !b.show_tool_tips,
                        cx,
                    ),
                    self.settings_hint(
                        "Reduce motion turns animation off, spinners included; busy labels stay.",
                        fonts,
                    ),
                ]),
            ],
            SettingsSection::ChatDefaults => vec![
                self.settings_card(vec![
                    div()
                        .flex()
                        .items_center()
                        .gap_2p5()
                        .child(settings_label("Harness", fonts))
                        .child(self.radio_row(
                            &crate::settings::HarnessKind::ALL
                                .map(|h| (h.label(), h.option_label())),
                            if buffers.default_harness_dsh { "dsh" } else { "opencode" },
                            fonts,
                            |b, key| b.default_harness_dsh = key == "dsh",
                            cx,
                        ))
                        .into_any_element(),
                    self.settings_hint(
                        "Runs new chats until the composer's harness pill switches one before its first message.",
                        fonts,
                    ),
                ]),
                self.settings_card(vec![
                    div()
                        .flex()
                        .items_center()
                        .gap_2p5()
                        .child(settings_label("Model", fonts))
                        .child(
                            self.settings_model_picker
                                .clone()
                                .map(|picker| picker.into_any_element())
                                .unwrap_or_else(|| div().into_any_element()),
                        )
                        .into_any_element(),
                    div()
                        .flex()
                        .items_center()
                        .gap_2p5()
                        .child(settings_label("Variant", fonts))
                        .child(self.settings_field_wrap("variant", cx))
                        .into_any_element(),
                    self.check_row(
                        "Always select the highest variant by default",
                        buffers.variant_highest,
                        fonts,
                        |b| b.variant_highest = !b.variant_highest,
                        cx,
                    ),
                    div()
                        .flex()
                        .items_center()
                        .gap_2p5()
                        .child(settings_label("Agent", fonts))
                        .child(self.settings_field_wrap("agent", cx))
                        .into_any_element(),
                ]),
            ],
            SettingsSection::Permissions => {
                let mut perm_rows: Vec<gpui::AnyElement> = Vec::new();
                perm_rows.push(self.check_row(
                    "Auto-allow tool permissions (edit, bash, webfetch)",
                    buffers.auto_allow,
                    fonts,
                    |b| b.auto_allow = !b.auto_allow,
                    cx,
                ));
                if buffers.auto_allow {
                    perm_rows.push(self.check_row(
                        "Allow immediately (skip the random wait)",
                        buffers.auto_allow_instant,
                        fonts,
                        |b| b.auto_allow_instant = !b.auto_allow_instant,
                        cx,
                    ));
                    if !buffers.auto_allow_instant {
                        perm_rows.push(
                            div()
                                .flex()
                                .items_center()
                                .gap_2p5()
                                .child(settings_label("Working hours", fonts))
                                .child(self.settings_field_wrap("min", cx))
                                .child(div().text_color(theme::text_muted()).child("–"))
                                .child(self.settings_field_wrap("max", cx))
                                .child(self.settings_hint("seconds", fonts))
                                .into_any_element(),
                        );
                        perm_rows.push(
                            div()
                                .flex()
                                .items_center()
                                .gap_2p5()
                                .child(settings_label("Lunch", fonts))
                                .child(self.settings_field_wrap("lunchstart", cx))
                                .child(div().text_color(theme::text_muted()).child("–"))
                                .child(self.settings_field_wrap("lunchend", cx))
                                .into_any_element(),
                        );
                        perm_rows.push(
                            div()
                                .flex()
                                .items_center()
                                .gap_2p5()
                                .child(settings_label("Off hours", fonts))
                                .child(div().text_color(theme::text_muted()).child("→"))
                                .child(self.settings_field_wrap("offstart", cx))
                                .into_any_element(),
                        );
                        perm_rows.push(
                            div()
                                .flex()
                                .items_center()
                                .gap_2p5()
                                .child(settings_label("Workday starts", fonts))
                                .child(div().text_color(theme::text_muted()).child("→"))
                                .child(self.settings_field_wrap("workdaystart", cx))
                                .into_any_element(),
                        );
                        // one range for both away windows: Store::auto_allow_range
                        perm_rows.push(
                            div()
                                .flex()
                                .items_center()
                                .gap_2p5()
                                .child(settings_label("Lunch and off hours", fonts))
                                .child(self.settings_field_wrap("awaymin", cx))
                                .child(div().text_color(theme::text_muted()).child("–"))
                                .child(self.settings_field_wrap("awaymax", cx))
                                .child(self.settings_hint("seconds", fonts))
                                .into_any_element(),
                        );
                        perm_rows.push(self.settings_hint(
                            "Random wait before auto-allow, picked by local time.",
                            fonts,
                        ));
                    }
                }
                perm_rows.push(self.check_row(
                    "Disable MCP servers (all workspaces)",
                    buffers.mcp_disabled,
                    fonts,
                    |b| b.mcp_disabled = !b.mcp_disabled,
                    cx,
                ));
                perm_rows.push(self.settings_hint(
                    "Disconnects every workspace's MCP servers at once and overrides the composer's per-workspace toggle.",
                    fonts,
                ));
                vec![self.settings_card(perm_rows)]
            }
            SettingsSection::Server => vec![self.settings_card(vec![
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(settings_label("OpenCode executable", fonts))
                    .child(self.settings_field_wrap("executable", cx))
                    .into_any_element(),
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(settings_label("DSH executable", fonts))
                    .child(self.settings_field_wrap("dsh_executable", cx))
                    .into_any_element(),
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(settings_label("Idle stop", fonts))
                    .child(self.settings_field_wrap("idlemin", cx))
                    .child(self.settings_hint("minutes", fonts))
                    .into_any_element(),
                self.settings_hint(
                    "A workspace server with no running chat closes after this many idle minutes; the next chat restarts it. 0 keeps servers running.",
                    fonts,
                ),
            ])],
        }
    }

    /// An Appearance row: it applies the moment it is pressed, the design's
    /// one exception to Save. The row itself reads the buffer, so the check
    /// repaints from the same value the settings file gets.
    fn appearance_check(
        &self,
        text: &str,
        checked: bool,
        fonts: &theme::FontTokens,
        flip: fn(&mut SettingsBuffers),
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .id(gpui::ElementId::NamedInteger(
                "appearance-check".into(),
                TileView::hash(text),
            ))
            .on_click(cx.listener(move |this, _, _, cx| {
                flip(&mut this.settings_buffers);
                this.commit_settings(cx);
                cx.notify();
            }))
            .child(self.check_row(text, checked, fonts, |_b| {}, cx))
            .into_any_element()
    }

    /// Persists the settings page: text fields land in the buffers, then the
    /// buffers parse and push into live state. Runs from the design's Save
    /// button; Appearance rows call it themselves so they apply at once.
    fn commit_settings(&self, cx: &mut App) {
        let mut buffers = self.settings_buffers.clone();
        for (key, field) in &self.settings_fields {
            let text = field.read(cx).value().to_string();
            match *key {
                "executable" => buffers.open_code_executable = text,
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

/// Fixed-width label shared by the settings rows.
fn settings_label(text: &str, fonts: &theme::FontTokens) -> gpui::AnyElement {
    div()
        .w(px(150.))
        .flex_shrink_0()
        .text_size(px(fonts.font_heading))
        .text_color(theme::text_dim())
        .child(SharedString::from(text.to_string()))
        .into_any_element()
}

/// Leaf renderer for a plain settings/rename input: gpui re-renders every
/// view that read an InputState during its own render, so rendering these
/// inputs from WarpWindow made each keystroke repaint the whole window —
/// sidebar and tile grid included. The owner keeps the shared InputState
/// handle for reading values; this view is only the renderer.
struct InputLeaf {
    state: Entity<InputState>,
    disabled: bool,
}

impl Render for InputLeaf {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        warpui_kit_core::shell::field(
            "input-leaf",
            Input::new(&self.state)
                .with_size(theme::field_size())
                .disabled(self.disabled),
        )
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
        refresh: false,
        refresh_label: "",
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
    cx.new(|cx| {
        let data = {
            let state = state.clone();
            let store = store.clone();
            Box::new(move |filter: &str, _window: &Window, cx: &App| {
                model_data(&state, &store, filter, cx)
            }) as Box<dyn Fn(&str, &Window, &App) -> PickerData>
        };
        let mut picker =
            FilteredPicker::new(spec, PickerTarget::Field(state.clone()), data, window, cx);
        picker.observe_entity(&store, cx);
        picker.observe_entity(&state, cx);
        picker
    })
}

/// One render's worth of the model catalog: the server-default row, provider
/// groups and the filtered, deduped models.
fn model_data(
    state: &Entity<InputState>,
    store: &Entity<Store>,
    filter: &str,
    cx: &App,
) -> PickerData {
    let fonts = store.read(cx).fonts;
    let accent = store.read(cx).accent.accent;
    let selected = state.read(cx).value().trim().to_string();
    let filter = filter.to_lowercase();
    let mut models: Vec<String> = store
        .read(cx)
        .models
        .iter()
        .filter(|model| {
            let model = model.trim();
            !model.is_empty()
                && !model.starts_with('(')
                && (filter.is_empty() || model.to_lowercase().contains(&filter))
        })
        .cloned()
        .collect();
    models.sort_by(|a, b| {
        model_provider(a)
            .to_lowercase()
            .cmp(&model_provider(b).to_lowercase())
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()))
    });
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
            label: "No models yet. Start a workspace to load models.".to_string(),
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
    }
}
