//! The main window: titlebar, resizable sidebar with workspace/chat trees, the
//! shared tile grid, the settings page and the restore banner.

use crate::store::{Store, WorkspaceItem};
use crate::theme;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use crate::ui::controls::{self, menu_row, popover_card, BtnKind};
use crate::ui::filtered_picker::FilteredPicker;
use crate::ui::settings::SettingsBuffers;
use crate::ui::tile::TileView;
use crate::ui::tooltip::{self, tip};
use gpui::prelude::*;
use gpui::{canvas, div, px, App, Context, Entity, MouseButton, Render, Role, SharedString, StyleRefinement, Toggled, Window, WindowControlArea};
use std::collections::HashMap;

#[derive(Clone)]
pub enum SideMenuAction {
    OpenFolder(String),
    OpenInOpenCode(String),
    StartServer(String),
    StopServer(String),
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
            SideMenuAction::OpenInOpenCode(_) => "Open in opencode",
            SideMenuAction::StartServer(_) => "Start server",
            SideMenuAction::StopServer(_) => "Stop server",
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

fn ws_menu_rows(store: &Store, path: &str) -> Vec<SideMenuAction> {
    let Some(ws) = store.workspaces.iter().find(|w| w.folder_path() == path) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    if !ws.is_general() {
        rows.push(SideMenuAction::OpenFolder(path.to_string()));
        rows.push(SideMenuAction::OpenInOpenCode(path.to_string()));
    }
    if ws.is_running {
        rows.push(SideMenuAction::StopServer(path.to_string()));
    } else {
        rows.push(SideMenuAction::StartServer(path.to_string()));
    }
    if !ws.is_general() {
        rows.push(SideMenuAction::RemoveWorkspace(path.to_string()));
    }
    rows
}

fn chat_menu_rows(store: &Store, id: &str) -> Vec<SideMenuAction> {
    let pinned = store.ui.pins.contains_key(id);
    vec![
        if pinned { SideMenuAction::UnpinChat(id.into()) } else { SideMenuAction::PinChat(id.into()) },
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
        let font_xsmall = store.read(cx).fonts.font_xsmall;
        let tooltip_overlay = tooltip::install(cx, font_xsmall);
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

fn execute_menu_action(store: &Entity<Store>, action: &SideMenuAction, cx: &mut App) {
    store.update(cx, |s, cx| match action {
        SideMenuAction::OpenFolder(path) => {
            if let Err(err) = crate::services::open_folder(path) {
                tracing::warn!("open folder failed: {err}");
            }
        }
        SideMenuAction::OpenInOpenCode(path) => {
            let executable = s.settings.open_code_executable.clone();
            if let Err(err) = crate::services::launch_in_terminal(path, &executable) {
                s.announce(&format!("Could not open a terminal: {err}"), true, cx);
            }
        }
        SideMenuAction::StartServer(path) => s.start_workspace(path, cx),
        SideMenuAction::StopServer(path) => s.stop_workspace(path, cx),
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

        let mut content = div().flex().flex_row().flex_1().min_h_0();
        if sidebar_visible {
            content = content.child(self.render_sidebar(window, cx));
            content = content.child(self.render_splitter());
        }
        content = content.child(self.render_main(window, cx));

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::WINDOW_CHROME)
            .text_color(theme::TEXT)
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
            .child(self.render_titlebar(window, cx))
            .child(content);

        // context menu overlay
        if let Some(menu) = self.menu.clone() {
            let store_handle = self.store.clone();
            let mut rows = div().flex().flex_col();
            for (i, row) in menu.rows.iter().enumerate() {
                let action = row.clone();
                let destructive = matches!(action, SideMenuAction::DeleteChat(_) | SideMenuAction::RemoveWorkspace(_));
                rows = rows.child(menu_row(
                    gpui::ElementId::NamedInteger("menu".into(), i as u64),
                    action.label(),
                    destructive,
                    fonts.font_body,
                    {
                        let store_handle = store_handle.clone();
                        move |_, _, cx| {
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

impl WarpWindow {
    fn sync_rename_field(&mut self, editing: Option<String>, draft: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
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
                let leaf = cx.new(|_| InputLeaf { state: entity.clone(), disabled: false });
                self.rename = Some((id, entity, leaf));
            }
            (None, Some(_)) => {
                self.rename = None;
            }
            _ => {}
        }
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
                theme::TEXT_MUTED
            }
        };
        let store_handle = self.store.clone();
        div()
            .h(px(38.))
            .flex()
            .items_center()
            .px_2()
            .gap_1()
            // titlebar chrome owns no selectable text either (see sidebar note)
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                gpui_kit::base::GlobalState::suppress_text_selection(cx);
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(30.))
                    .child(crate::ui::icon("icons/spark.svg", px(fonts.icon_large), theme::TEXT)),
            )
            .child(
                tip(
                    div()
                        .id("toggle-sidebar")
                        .role(Role::Button)
                        .aria_label(if visible { "Hide sidebar" } else { "Show sidebar" })
                        .cursor_pointer()
                        .size(px(26.))
                        .rounded_md()
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|s| s.bg(theme::HOVER))
                        .on_click(move |_, _, cx| {
                            store_handle.update(cx, |s, cx| s.set_sidebar_visible(!s.sidebar_visible, cx));
                        }),
                    if visible { "Hide sidebar" } else { "Show sidebar" },
                )
                .child(if visible {
                    crate::ui::icon("icons/panel-left.svg", px(fonts.icon_medium), theme::TEXT_MUTED)
                } else {
                    crate::ui::icon("icons/panel-left-open.svg", px(fonts.icon_medium), theme::TEXT_MUTED)
                }),
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
            .child(
                tip(
                    div()
                        .id("minimize-window")
                        .role(Role::Button)
                        .aria_label("Minimize")
                        .cursor_pointer()
                        .size(px(26.))
                        .rounded_md()
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|s| s.bg(theme::HOVER))
                        .on_click(|_, window, _| window.minimize_window()),
                    "Minimize",
                )
                .child(crate::ui::icon("icons/dash.svg", px(fonts.icon_medium), theme::TEXT_MUTED)),
            )
            .child(
                tip(
                    div()
                        .id("zoom")
                        .role(Role::Button)
                        .aria_label("Zoom · server health")
                        .cursor_pointer()
                        .size(px(26.))
                        .rounded_md()
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|s| s.bg(theme::HOVER))
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
                        .size(px(26.))
                        .rounded_md()
                        .flex()
                        .items_center()
                        .justify_center()
                        .hover(|s| s.bg(theme::tint(theme::DANGER)))
                        .on_click(|_, window, _| window.remove_window()),
                    "Close",
                )
                .child(crate::ui::icon("icons/close.svg", px(fonts.icon_medium), theme::TEXT_MUTED)),
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
            self.sidebar_filter_leaf = Some(cx.new(|_| InputLeaf { state, disabled: false }));
        }
        let filter = self.sidebar_filter.to_lowercase();
        let filter_leaf = self.sidebar_filter_leaf.clone().unwrap();

        let mut workspaces = div().flex().flex_col().gap_2p5();
        let ws_count = self.store.read(cx).workspaces.len();
        let mut shown = 0usize;
        for ws_index in 0..ws_count {
            let ws = self.store.read(cx).workspaces[ws_index].clone();
            if !filter.is_empty() && !ws.name.to_lowercase().contains(&filter) {
                continue;
            }
            shown += 1;
            workspaces = workspaces.child(self.render_workspace_row(&ws, cx));
            if ws.is_expanded() {
                if ws.show_empty_hint() {
                    workspaces = workspaces.child(
                        div()
                            .ml(px(26.))
                            .text_size(px(fonts.font_small))
                            .italic()
                            .text_color(theme::TEXT_FAINT)
                            .child("No chats yet"),
                    );
                }
                // borrow the chats from the workspace clone: re-cloning each
                // SessionItem here doubles the sidebar copy work per repaint
                for chat in &ws.chats {
                    workspaces = workspaces.child(self.render_session_row(chat, cx));
                }
            }
        }
        if !filter.is_empty() && shown == 0 {
            workspaces = workspaces.child(
                div()
                    .px_2()
                    .text_size(px(fonts.font_small))
                    .italic()
                    .text_color(theme::TEXT_FAINT)
                    .child("No matching workspace"),
            );
        }

        div()
            .w(px(width))
            .h_full()
            .flex()
            .flex_col()
            .px_2p5()
            .pb_2()
            // the sidebar owns no selectable text: a press here (row, + button)
            // must not seed a window drag-selection — otherwise the gesture
            // that starts on a row can land on transcript text and select it
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                gpui_kit::base::GlobalState::suppress_text_selection(cx);
            })
            .child(
                div()
                    .group("wsheader")
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(fonts.font_small))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::TEXT_DIM)
                            .child("Workspaces"),
                    )
                    .child(
                        // hover-reveal safety: an opacity(0) control is still
                        // hit-tested in gpui, but it can only be clicked while
                        // the pointer is over its row — which is exactly when
                        // group_hover has revealed it. Keep that invariant if
                        // this pattern ever moves somewhere pointer-less input
                        // (keyboard/touch) can reach.
                        tip(
                            div()
                                .id("add-workspace")
                                .role(Role::Button)
                                .aria_label("Add a workspace folder")
                                .cursor_pointer()
                                .p_1()
                                .rounded_sm()
                                .opacity(0.)
                                .group_hover("wsheader", |s| s.opacity(1.))
                                .hover(|s| s.bg(theme::HOVER))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.pick_folder(cx);
                                })),
                            "Add a workspace folder",
                        )
                        .child(crate::ui::icon("icons/plus.svg", px(fonts.icon_medium), theme::TEXT_MUTED)),
                    ),
            )
            .child(
                // the kit Input has no a11y of its own — the wrapper carries the
                // role/value; an AT click lands at the wrapper's center, which is
                // the input itself, so focus still reaches the real field
                div()
                    .id("sidebar-filter")
                    .role(Role::SearchInput)
                    .aria_label("Filter workspaces")
                    .aria_placeholder("Filter workspaces")
                    .aria_value(self.sidebar_filter.clone())
                    .w_full()
                    .min_w_0()
                    .mt_1()
                    .child(filter_leaf),
            )
            .child(
                div()
                    .id("sidebar-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pt_2()
                    .child(workspaces),
            )
            .child(
                tip(
                    div()
                        .id("open-settings")
                        .role(Role::Button)
                        .aria_label("Settings")
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .px_2()
                        .py_1p5()
                        .rounded_lg()
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::HOVER))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.store.update(cx, |s, _| {
                                s.is_settings_open = true;
                            });
                            this.settings_buffers = SettingsBuffers::from_settings(&this.store.read(cx).settings);
                            let (fields, leafs, model_picker) = this.make_settings_fields(window, cx);
                            this.settings_fields = fields;
                            this.settings_leafs = leafs;
                            this.settings_model_picker = model_picker;
                            cx.notify();
                        })),
                    "Settings",
                )
                .child(crate::ui::icon("icons/cog.svg", px(fonts.icon_medium), theme::TEXT_MUTED))
                .child(
                    div()
                        .text_size(px(fonts.font_small))
                        .text_color(theme::TEXT)
                        .child("Settings"),
                ),
            )
            .into_any_element()
    }

    fn render_workspace_row(&mut self, ws: &WorkspaceItem, cx: &mut Context<Self>) -> gpui::AnyElement {
        let fonts = self.store.read(cx).fonts;
        let path = ws.folder_path().to_string();
        let glyph: gpui::AnyElement = if ws.is_general() {
            crate::ui::icon("icons/chat-outline.svg", px(fonts.icon_body), theme::TEXT_MUTED).into_any_element()
        } else if ws.is_expanded() {
            crate::ui::icon("icons/folder-open-outline.svg", px(fonts.icon_body), theme::TEXT_MUTED).into_any_element()
        } else {
            crate::ui::icon("icons/folder-outline.svg", px(fonts.icon_body), theme::TEXT_MUTED).into_any_element()
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
            .id(gpui::ElementId::NamedInteger("ws".into(), TileView::hash(&path)))
            .role(Role::Button)
            .aria_label(SharedString::from(ws.name.clone()))
            .aria_expanded(ws.is_expanded())
            .relative()
            .group("wsrow")
            .flex()
            .items_center()
            .w_full()
            .px_2()
            .py_1p5()
            .rounded_lg()
            .hover(|s| s.bg(theme::HOVER))
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                let expand_path = path_click.clone();
                ws_store.update(cx, |s, cx| {
                    s.select_workspace(&expand_path, cx);
                    if let Some(ws) = s.workspaces.iter_mut().find(|w| w.folder_path() == expand_path) {
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
            .on_mouse_down(MouseButton::Right, move |event: &gpui::MouseDownEvent, _, cx| {
                let rows = {
                    let s = menu_store.read(cx);
                    ws_menu_rows(s, &menu_path)
                };
                let _ = menu_tx.send_blocking(MenuState { x: event.position.x.as_f32(), y: event.position.y.as_f32(), rows });
                // right-click alone doesn't repaint — nudge the store so the menu shows now
                menu_store.update(cx, |_, _| {});
            })
            .child(glyph)
            // no flex_1: the row packs left so the count hugs the name; a long
            // name shrinks (min_w_0) and truncates instead of pushing the
            // count to the row's far edge
            .child(
                div()
                    .min_w_0()
                    .ml_1p5()
                    .text_size(px(fonts.font_body))
                    .text_color(theme::TEXT)
                    .truncate()
                    .child(SharedString::from(ws.name.clone())),
            )
            .when(ws.has_chats(), |d| {
                d.child(
                    div()
                        .flex_shrink_0()
                        .ml_1p5()
                        .text_size(px(fonts.font_small))
                        .text_color(theme::TEXT_FAINT)
                        .child(SharedString::from(format!("({})", ws.chats.len()))),
                )
            })
            // running dot pinned to the right edge; hovering the row fades it
            // out while the kebab/new-chat buttons fade in at the same spot.
            // Lit by RUNNING sessions in this workspace — server-up alone
            // must not read as "work in progress"
            .child(
                div()
                    .absolute()
                    .right_1()
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
                    .right_1()
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
                                        .id(gpui::ElementId::NamedInteger("ws-kebab".into(), TileView::hash(&path)))
                                        .role(Role::Button)
                                        .aria_label(SharedString::from(format!("{} — workspace actions", ws.name)))
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
                                            let (x, y) = match event {
                                                gpui::ClickEvent::Mouse(click) => (click.up.position.x.as_f32(), click.up.position.y.as_f32()),
                                                _ => (40.0, 60.0),
                                            };
                                            let _ = menu_tx2.send_blocking(MenuState { x, y, rows });
                                            // menu paints on the next store-driven repaint — same nudge as right-click
                                            menu_store2.update(cx, |_, _| {});
                                        }),
                                    "Workspace actions",
                                )
                                .child(crate::ui::icon("icons/kebab.svg", px(fonts.icon_medium), theme::TEXT_MUTED)),
                            )
                            .child(
                                tip(
                                    div()
                                        .id(gpui::ElementId::NamedInteger("ws-new".into(), TileView::hash(&path)))
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
                                            ws_store_new.update(cx, |s, cx| s.new_chat_for_workspace(&path_new, cx));
                                        }),
                                    "New chat",
                                )
                                .child(crate::ui::icon("icons/chat-outline.svg", px(fonts.icon_medium), theme::TEXT_MUTED)),
                            ),
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
                self.sidebar_labels.insert(chat.id.clone(), (chat.updated_ms, label.clone()));
                label
            }
        }
    }

    fn render_session_row(&mut self, chat: &crate::store::SessionItem, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (fonts, accent_dim) = {
            let store = self.store.read(cx);
            (store.fonts, store.accent.dim)
        };
        let id = chat.id.clone();
        let store_click = self.store.clone();
        let id_click = id.clone();
        let menu_tx = self.menu_sender_cell.clone();
        let menu_store = self.store.clone();
        let id_menu = id.clone();
        let store_del = self.store.clone();
        let id_del = id.clone();
        let gutter: gpui::AnyElement = if chat.is_running {
            crate::ui::icon("icons/spinner.svg", px(fonts.icon_small), theme::RUNNING).into_any_element()
        } else if chat.pin_visible() {
            crate::ui::icon("icons/pin.svg", px(fonts.icon_xsmall), theme::TEXT_MUTED).into_any_element()
        } else {
            div().into_any_element()
        };

        let mut row = div()
            .id(gpui::ElementId::NamedInteger("chat".into(), TileView::hash(&id)))
            .role(Role::Button)
            .aria_label(SharedString::from(chat.title.clone()))
            .aria_selected(chat.is_selected)
            .relative()
            .group("chatrow")
            .flex()
            .items_center()
            // no w_full: with the 26px indent, w_full overflowed the sidebar and
            // clipped the right-pinned time/delete layers; flex-col stretch
            // already sizes the row to the parent minus the indent
            .ml(px(26.))
            .px_2()
            .py_1p5()
            .rounded_lg()
            .when(chat.is_selected, |d| d.bg(accent_dim))
            .hover(|s| s.bg(theme::HOVER))
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                tracing::info!("chat-row clicked {}", id_click);
                store_click.update(cx, |s, cx| s.select_session(&id_click, cx));
            })
            .on_mouse_down(MouseButton::Right, move |event: &gpui::MouseDownEvent, _, cx| {
                let rows = {
                    let s = menu_store.read(cx);
                    chat_menu_rows(s, &id_menu)
                };
                let _ = menu_tx.send_blocking(MenuState { x: event.position.x.as_f32(), y: event.position.y.as_f32(), rows });
                // right-click alone doesn't repaint — nudge the store so the menu shows now
                menu_store.update(cx, |_, _| {});
            })
            .child(div().w(px(13.)).flex().items_center().justify_center().child(gutter));

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
                        .text_color(theme::TEXT)
                        .truncate()
                        .child(SharedString::from(chat.title.clone())),
                )
                // time label pinned right; hovering the row fades it out and
                // fades the delete button in at the same spot (overlapping
                // absolute layers, so nothing shifts)
                .child(
                    div()
                        .absolute()
                        .right_1()
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .group_hover("chatrow", |s| s.opacity(0.))
                        .text_size(px(fonts.font_micro))
                        .text_color(theme::TEXT_FAINT)
                        .child(self.cached_time_label(chat)),
                )
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
                                    .id(gpui::ElementId::NamedInteger("chat-del".into(), TileView::hash(&id)))
                                    .role(Role::Button)
                                    .aria_label(SharedString::from(format!("Delete {}", chat.title)))
                                    .cursor_pointer()
                                    .p_1()
                                    .rounded_sm()
                                    .hover(|s| s.bg(theme::HOVER))
                                    .on_click(move |_, _, cx| {
                                        // delete only — don't select the row we're removing
                                        cx.stop_propagation();
                                        store_del.update(cx, |s, cx| s.delete_session(&id_del, cx));
                                    }),
                                "Delete chat",
                            )
                            .child(crate::ui::icon("icons/trash.svg", px(fonts.icon_medium), theme::TEXT_MUTED)),
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
            .cursor_col_resize()
            .hover(|s| s.bg(theme::HOVER))
            .on_mouse_down(MouseButton::Left, move |event: &gpui::MouseDownEvent, _, cx| {
                let start_width = store_handle.read(cx).sidebar_shown_width;
                drag_state.set(Some((event.position.x, start_width)));
                cx.stop_propagation();
            })
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
                                    let Some((start_x, start_width)) = drag.get() else { return };
                                    match event.pressed_button {
                                        Some(MouseButton::Left) => {
                                            let delta = (event.position.x - start_x).as_f32();
                                            store.update(cx, |s, cx| {
                                                s.sidebar_shown_width = (start_width + delta)
                                                    .clamp(theme::SIDEBAR_MIN_WIDTH, theme::SIDEBAR_MAX_WIDTH);
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
            prompt: Some("Workspace folder for opencode".into()),
        });
        let tx = self.add_folder_sender.clone();
        cx.spawn(async move |_this, _cx| {
            if let Ok(Ok(Some(paths))) = task.await {
                if let Some(path) = paths.first() {
                    let _ = tx.send_blocking(path.display().to_string());
                }
            }
        })
        .detach();
    }

    fn make_settings_fields(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (
        Vec<(&'static str, Entity<InputState>)>,
        Vec<(&'static str, Entity<InputLeaf>)>,
        Option<Entity<FilteredPicker>>,
    ) {
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
                let state = cx.new(|cx| InputState::new(window, cx).placeholder("").default_value(value));
                let store = self.store.clone();
                let picker = cx.new(|cx| FilteredPicker::new(state.clone(), store, window, cx));
                fields.push((key, state));
                model_picker = Some(picker);
            } else {
                let entity = cx.new(|cx| InputState::new(window, cx).placeholder("").default_value(value));
                let leaf = cx.new(|_| InputLeaf { state: entity.clone(), disabled: false });
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
                let value = self
                    .settings_fields
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, field)| field.read(cx).value().to_string())
                    .unwrap_or_default();
                div()
                    .id(gpui::ElementId::Name(SharedString::from(format!("field-{key}"))))
                    .role(Role::TextInput)
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
            .text_size(px(fonts.font_small))
            .italic()
            .text_color(theme::TEXT_FAINT)
            .child(text.to_string())
            .into_any_element()
    }

    fn settings_section(&self, text: &str, fonts: &theme::FontTokens) -> gpui::AnyElement {
        div()
            .text_size(px(fonts.font_small))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(theme::TEXT_MUTED)
            .child(text.to_string())
            .into_any_element()
    }

    fn settings_card(&self, children: Vec<gpui::AnyElement>) -> gpui::AnyElement {
        div()
            .w_full()
            .rounded_lg()
            .border_1()
            .border_color(theme::HAIRLINE)
            .bg(theme::SETTINGS_CARD)
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
                .child(crate::ui::icon("icons/check.svg", px(fonts.icon_nano), theme::SURFACE))
                .into_any_element()
        } else {
            div().size(px(14.)).rounded_sm().border_1().border_color(theme::TEXT_MUTED).into_any_element()
        };
        div()
            .id(gpui::ElementId::NamedInteger("chk".into(), TileView::hash(text)))
            .role(Role::CheckBox)
            .aria_label(text)
            .aria_toggled(if checked { Toggled::True } else { Toggled::False })
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
                    .text_size(px(fonts.font_body))
                    .text_color(theme::TEXT)
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
                    .id(gpui::ElementId::NamedInteger("radio".into(), TileView::hash(key)))
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
                        div().size(px(11.)).rounded_full().bg(accent).into_any_element()
                    } else {
                        div().size(px(11.)).rounded_full().border_1().border_color(theme::TEXT_MUTED).into_any_element()
                    })
                    .child(
                        div()
                            .text_size(px(fonts.font_body))
                            .text_color(theme::TEXT)
                            .child((*label_text).to_string()),
                    ),
            );
        }
        row.into_any_element()
    }

    // ---- main area -------------------------------------------------------------

    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let fonts = self.store.read(cx).fonts;

        // keep TileView entities in sync with the store's tile list
        let keys: Vec<String> = self.store.read(cx).open_tiles.iter().map(|t| t.key.clone()).collect();
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
        let mut grid = div()
            .grid()
            .grid_cols(tile_columns as u16)
            .grid_rows(tile_rows)
            .gap_2()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .p_1();
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
            // no h_full here: the row's stretch must subtract my_2p5 so the
            // bottom margin isn't clipped past the window edge
            .flex()
            .flex_col()
            .my_2p5()
            .mr_2p5()
            .rounded_xl()
            .bg(theme::SURFACE)
            .overflow_hidden()
            .relative()
            .child(div().flex_1().min_h_0().flex().flex_col().child(grid));

        let (has_tiles, has_no_ws, has_idle_ws, has_running_ws) = {
            let store = self.store.read(cx);
            (store.has_tiles(), store.has_no_workspace(), store.has_idle_workspace(), store.has_running_workspace())
        };
        if !has_tiles {
            let hint = if has_no_ws {
                Some("Add a workspace folder to begin.")
            } else if has_idle_ws {
                Some("Start the server to load your chats.")
            } else if has_running_ws {
                Some("Open a chat from the sidebar, or start a new one with the + next to a workspace.")
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
                            .child(div().mb_4().child(crate::ui::icon("icons/spark.svg", px(64.), theme::TEXT)))
                            .child(
                                div()
                                    .text_size(px(fonts.font_small))
                                    .italic()
                                    .text_color(theme::TEXT_FAINT)
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
                    .absolute()
                    .top_3()
                    .left(px(24.))
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::HAIRLINE)
                    .bg(theme::CARD)
                    .px_3p5()
                    .py_1p5()
                    .shadow_lg()
                    .child(crate::ui::icon("icons/clock.svg", px(fonts.icon_body), theme::TEXT_MUTED))
                    .child(
                        div()
                            .text_size(px(fonts.font_body))
                            .text_color(theme::TEXT)
                            .child(SharedString::from(restore_text.clone())),
                    )
                    .child(controls::button(
                        "restore-btn",
                        "Restore",
                        BtnKind::Outline,
                        accent,
                        fonts.font_body,
                        true,
                        {
                            let store = store_handle.clone();
                            move |_, _, cx| {
                                store.update(cx, |s, cx| s.restore_previous_session(cx));
                            }
                        },
                    ))
                    .child(controls::button(
                        "ignore-btn",
                        "Ignore",
                        BtnKind::Outline,
                        accent,
                        fonts.font_body,
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

        let settings_open = self.store.read(cx).is_settings_open;
        if settings_open {
            main = main.child(self.render_settings(window, cx));
        }

        main.into_any_element()
    }

    // ---- settings page ----

    fn render_settings(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let fonts = self.store.read(cx).fonts;
        let buffers = self.settings_buffers.clone();

        let page = div()
            .id("settings-page")
            .absolute()
            .inset_0()
            // opaque sheet: without it the chat tiles bleed through the cards
            .bg(theme::SETTINGS_BG)
            .overflow_y_scroll()
            .child(
            div()
                .max_w(px(1040.))
                .m(px(28.))
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                                .child(
                                    div()
                                        .id("settings-back")
                                        .role(Role::Button)
                                        .aria_label("Back to chats")
                                        .cursor_pointer()
                                .p_1()
                                .rounded_md()
                                .hover(|s| s.bg(theme::HOVER))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    // leaving the page commits: text fields land
                                    // in the buffers, then persist like Save did
                                    this.commit_settings(cx);
                                    this.store.update(cx, |s, _| s.is_settings_open = false);
                                }))
                                .child(crate::ui::icon("icons/arrow-left.svg", px(fonts.icon_medium), theme::TEXT_MUTED)),
                        )
                        .child(
                            div()
                                .text_size(px(fonts.font_title))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme::TEXT)
                                .child("Settings"),
                        ),
                )
                .child(
                    div()
                        .grid()
                        .grid_cols(2)
                        .gap_4()                        .child(
                            div().flex().flex_col().gap_4()
                                .child(self.settings_section("Server", &fonts))
                                .child(self.settings_card(vec![
                                    div().flex().items_center().gap_2p5()
                                        .child(settings_label("OpenCode executable", &fonts))
                                        .child(self.settings_field_wrap("executable", cx))
                                        .into_any_element(),
                                    div().flex().items_center().gap_2p5()
                                        .child(settings_label("DSH executable", &fonts))
                                        .child(self.settings_field_wrap("dsh_executable", cx))
                                        .into_any_element(),
                                    self.settings_hint("Running servers keep their current binary until stopped and started again.", &fonts),
                                ]))
                                .child(self.settings_section("Harness", &fonts))
                                .child(self.settings_card(vec![
                                    div().flex().items_center().gap_2p5()
                                        .child(settings_label("Default for new chats", &fonts))
                                        .child(self.radio_row(
                                            &[("opencode", "opencode"), ("dsh", "DeepSeek (dsh)")],
                                            if buffers.default_harness_dsh { "dsh" } else { "opencode" },
                                            &fonts,
                                            |b, key| b.default_harness_dsh = key == "dsh",
                                            cx,
                                        ))
                                        .into_any_element(),
                                    self.settings_hint("The composer's harness pill switches each chat before its first message.", &fonts),
                                ]))
                                .child(self.settings_section("Chat defaults", &fonts))
                                .child(self.settings_card(vec![
                                    div().flex().items_center().gap_2p5()
                                        .child(settings_label("Model", &fonts))
                                        .child(self.settings_model_picker.clone()
                                            .map(|picker| picker.into_any_element())
                                            .unwrap_or_else(|| div().into_any_element()))
                                        .into_any_element(),
                                    div().flex().items_center().gap_2p5()
                                        .child(settings_label("Variant", &fonts))
                                        .child(self.settings_field_wrap("variant", cx)).into_any_element(),
                                    self.check_row(
                                        "Always select the highest variant by default",
                                        buffers.variant_highest,
                                        &fonts,
                                        |b| b.variant_highest = !b.variant_highest,
                                        cx,
                                    ),
                                    div().flex().items_center().gap_2p5()
                                        .child(settings_label("Agent", &fonts))
                                        .child(self.settings_field_wrap("agent", cx)).into_any_element(),
                                ])),
                        )
                        .child(
                            div().flex().flex_col().gap_4()
                                .child(self.settings_section("Appearance", &fonts))
                                .child(self.settings_card(vec![
                                    div().flex().items_center().gap_2p5()
                                        .child(settings_label("Text size", &fonts))
                                        .child(self.radio_row(
                                            &[("compact", "Small"), ("normal", "Normal"), ("large", "Large")],
                                            if buffers.text_compact { "compact" } else if buffers.text_large { "large" } else { "normal" },
                                            &fonts,
                                            |b, key| {
                                                b.text_compact = key == "compact";
                                                b.text_normal = key == "normal";
                                                b.text_large = key == "large";
                                            },
                                            cx,
                                        ))
                                        .into_any_element(),
                                    self.settings_hint("Applied across the app as soon as it's saved.", &fonts),
                                    div().h(px(1.)).bg(theme::HAIRLINE).into_any_element(),
                                    div().flex().flex_wrap().items_center().gap_2p5()
                                        .child(settings_label("Accent", &fonts))
                                        .child({
                                            // wrap keeps all presets reachable at any card width
                                            let mut swatches = div().flex().flex_wrap().gap_2p5();
                                            for preset in theme::ACCENT_PRESETS.iter() {
                                                let key = preset.key.to_string();
                                                let selected = buffers.accent_key == preset.key;
                                                swatches = swatches.child(
                                                    div()
                                                        .id(gpui::ElementId::NamedInteger("accent".into(), TileView::hash(preset.key)))
                                                        .role(Role::RadioButton)
                                                        .aria_label(SharedString::from(format!("Accent {}", preset.key)))
                                                        .aria_selected(selected)
                                                        .size(px(24.))
                                                        .flex_shrink_0()
                                                        .rounded_full()
                                                        .bg(theme::rgba_u32(preset.accent))
                                                        .when(selected, |d| d.border_2().border_color(theme::TEXT_MUTED))
                                                        .cursor_pointer()
                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                            this.settings_buffers.accent_key = key.clone();
                                                            cx.notify();
                                                        })),
                                                );
                                            }
                                            swatches.into_any_element()
                                        })
                                        .into_any_element(),
                                    self.settings_hint("Green, blue, amber, salmon and four gray tones; recolors selected states and accents.", &fonts),
                                    div().h(px(1.)).bg(theme::HAIRLINE).into_any_element(),
                                    self.check_row("Show tooltips on hover", buffers.show_tool_tips, &fonts, |b| b.show_tool_tips = !b.show_tool_tips, cx),
                                ]))
                                .child(self.settings_section("Permissions", &fonts))
                                .child({
                                    let mut perm_rows: Vec<gpui::AnyElement> = Vec::new();
                                    perm_rows.push(self.check_row("Auto-allow tool permissions (edit, bash, webfetch)", buffers.auto_allow, &fonts, |b| b.auto_allow = !b.auto_allow, cx));
                                    if buffers.auto_allow {
                                        perm_rows.push(self.check_row("Allow immediately (skip the random wait)", buffers.auto_allow_instant, &fonts, |b| b.auto_allow_instant = !b.auto_allow_instant, cx));
                                        if !buffers.auto_allow_instant {
                                            perm_rows.push(div().flex().items_center().gap_2p5()
                                                .child(settings_label("Working hours", &fonts))
                                                .child(self.settings_field_wrap("min", cx))
                                                .child(div().text_color(theme::TEXT_MUTED).child("–"))
                                                .child(self.settings_field_wrap("max", cx))
                                                .child(self.settings_hint("seconds", &fonts))
                                                .into_any_element());
                                            perm_rows.push(div().flex().items_center().gap_2p5()
                                                .child(settings_label("Lunch", &fonts))
                                                .child(self.settings_field_wrap("lunchstart", cx))
                                                .child(div().text_color(theme::TEXT_MUTED).child("–"))
                                                .child(self.settings_field_wrap("lunchend", cx))
                                                .child(div().text_color(theme::TEXT_MUTED).child("→"))
                                                .child(self.settings_field_wrap("awaymin", cx))
                                                .child(div().text_color(theme::TEXT_MUTED).child("–"))
                                                .child(self.settings_field_wrap("awaymax", cx))
                                                .into_any_element());
                                            perm_rows.push(div().flex().items_center().gap_2p5()
                                                .child(settings_label("Off hours", &fonts))
                                                .child(self.settings_field_wrap("offstart", cx))
                                                .child(div().text_color(theme::TEXT_MUTED).child("→"))
                                                .child(self.settings_field_wrap("workdaystart", cx))
                                                .into_any_element());
                                            perm_rows.push(self.settings_hint("Random wait before auto-allow, picked by local time.", &fonts));
                                        }
                                    }
                                    perm_rows.push(self.check_row("Disable MCP servers (all workspaces)", buffers.mcp_disabled, &fonts, |b| b.mcp_disabled = !b.mcp_disabled, cx));
                                    perm_rows.push(self.settings_hint("Disconnects every workspace's MCP servers at once and overrides the composer's per-workspace toggle.", &fonts));
                                    self.settings_card(perm_rows)
                                }),
                        ),
                ),
        );
        page.into_any_element()
    }

    /// Persists the settings page: text fields land in the buffers, then the
    /// buffers parse and push into live state. Runs when the page closes —
    /// there is no separate Save button.
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
        .text_size(px(fonts.font_body))
        .text_color(theme::TEXT_DIM)
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
        Input::new(&self.state).disabled(self.disabled)
    }
}

