//! The main window: titlebar, resizable sidebar with workspace/chat trees, the
//! shared tile grid, the settings page, restore banner and the error overlay.

use crate::store::{Store, WorkspaceItem};
use crate::theme;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use crate::ui::settings::SettingsBuffers;
use crate::ui::tile::TileView;
use gpui::prelude::*;
use gpui::{canvas, div, px, AnimationExt, App, Context, Entity, MouseButton, Render, SharedString, Window};
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
            SideMenuAction::UnlinkChat(_) => "Remove from workspace",
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
    menu_sender_cell: async_channel::Sender<MenuState>,
    menu_receiver: async_channel::Receiver<MenuState>,
    menu: Option<MenuState>,
    add_folder_sender: async_channel::Sender<String>,
    add_folder_receiver: async_channel::Receiver<String>,
    settings_buffers: SettingsBuffers,
    settings_fields: Vec<(&'static str, Entity<InputState>)>,
    /// session being renamed + its edit field
    rename: Option<(String, Entity<InputState>)>,
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
        Self {
            store,
            tile_views: HashMap::new(),
            menu_sender_cell: menu_tx,
            menu_receiver: menu_rx,
            menu: None,
            add_folder_sender: folder_tx,
            add_folder_receiver: folder_rx,
            settings_buffers,
            settings_fields: Vec::new(),
            rename: None,
            splitter_drag: std::rc::Rc::new(std::cell::Cell::new(None)),
        }
    }
}

fn execute_menu_action(store: &Entity<Store>, action: &SideMenuAction, cx: &mut App) {
    store.update(cx, |s, cx| match action {
        SideMenuAction::OpenFolder(path) => {
            let _ = std::process::Command::new("xdg-open").arg(path).spawn();
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
            content = content.child(self.render_sidebar(cx));
            content = content.child(self.render_splitter(cx));
        }
        content = content.child(self.render_main(window, cx));

        let mut root = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::WINDOW_CHROME)
            .text_color(theme::TEXT)
            .font_family("Inter")
            .child(self.render_titlebar(cx))
            .child(content);

        // context menu overlay
        if let Some(menu) = self.menu.clone() {
            let mut column = div().flex().flex_col().min_w(px(180.));
            for (i, row) in menu.rows.iter().enumerate() {
                let action = row.clone();
                let destructive = matches!(action, SideMenuAction::DeleteChat(_) | SideMenuAction::RemoveWorkspace(_));
                column = column.child(
                    div()
                        .id(gpui::ElementId::NamedInteger("menu".into(), i as u64))
                        .px_3()
                        .py_1p5()
                        .rounded_md()
                        .text_size(px(fonts.font_body))
                        .cursor_pointer()
                        .when(destructive, |d| d.text_color(theme::DANGER))
                        .when(!destructive, |d| d.text_color(theme::TEXT))
                        .hover(|s| s.bg(theme::HOVER))
                        .on_click({
                            let store_handle = self.store.clone();
                            let action = action.clone();
                            move |_, _, cx| {
                                execute_menu_action(&store_handle, &action, cx);
                            }
                        })
                        .child(action.label()),
                );
            }
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                        this.menu = None;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .absolute()
                            .left(px(menu.x))
                            .top(px(menu.y))
                            .rounded_lg()
                            .border_1()
                            .border_color(theme::HAIRLINE)
                            .bg(theme::SETTINGS_BG)
                            .p_1()
                            .shadow_lg()
                            .child(column),
                    ),
            );
        }

        root
    }
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
                self.rename = Some((id, entity));
            }
            (None, Some(_)) => {
                self.rename = None;
            }
            _ => {}
        }
    }

    fn render_titlebar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (fonts, visible) = {
            let store = self.store.read(cx);
            (store.fonts, store.sidebar_visible)
        };
        let store_handle = self.store.clone();
        div()
            .h(px(38.))
            .flex()
            .items_center()
            .px_2()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(30.))
                    .child(crate::ui::icon("icons/spark.svg", px(fonts.icon_large), theme::TEXT)),
            )
            .child(
                div()
                    .id("titlebar-drag")
                    .flex_1()
                    .h_full()
                    .on_mouse_down(MouseButton::Left, |_, window, _| {
                        window.start_window_move();
                    }),
            )
            .child(
                div()
                    .id("toggle-sidebar")
                    .cursor_pointer()
                    .size(px(26.))
                    .rounded_md()
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|s| s.bg(theme::HOVER))
                    .on_click(move |_, _, cx| {
                        store_handle.update(cx, |s, cx| s.set_sidebar_visible(!s.sidebar_visible, cx));
                    })
                    .child(if visible {
                        crate::ui::icon("icons/panel-left.svg", px(fonts.icon_medium), theme::TEXT_MUTED)
                    } else {
                        crate::ui::icon("icons/panel-left-open.svg", px(fonts.icon_medium), theme::TEXT_MUTED)
                    }),
            )
            .child(
                div()
                    .id("zoom")
                    .cursor_pointer()
                    .size(px(26.))
                    .rounded_md()
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|s| s.bg(theme::HOVER))
                    .on_click(|_, window, _| window.zoom_window())
                    .child(div().size(px(9.)).rounded_full().border_1().border_color(theme::TEXT_MUTED)),
            )
            .child(
                div()
                    .id("close-window")
                    .cursor_pointer()
                    .size(px(26.))
                    .rounded_md()
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|s| s.bg(theme::DANGER_DIM))
                    .on_click(|_, window, _| window.remove_window())
                    .child(crate::ui::icon("icons/close.svg", px(fonts.icon_small), theme::TEXT_MUTED)),
            )
            .into_any_element()
    }

    // ---- sidebar ---------------------------------------------------------------

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (fonts, width) = {
            let store = self.store.read(cx);
            (store.fonts, store.sidebar_shown_width)
        };
        let mut workspaces = div().flex().flex_col().gap_2p5();
        let ws_count = self.store.read(cx).workspaces.len();
        for ws_index in 0..ws_count {
            let ws = self.store.read(cx).workspaces[ws_index].clone();
            workspaces = workspaces.child(self.render_workspace_row(&ws, cx));
            if ws.is_expanded() {
                if ws.show_empty_hint() {
                    workspaces = workspaces.child(
                        div()
                            .ml(px(26.))
                            .text_size(px(fonts.font_small))
                            .italic()
                            .text_color(theme::TEXT_MUTED)
                            .opacity(0.5)
                            .child("No chats yet"),
                    );
                }
                let chat_count = ws.chats.len();
                for chat_index in 0..chat_count {
                    let chat = ws.chats[chat_index].clone();
                    workspaces = workspaces.child(self.render_session_row(&chat, cx));
                }
            }
        }

        div()
            .w(px(width))
            .h_full()
            .flex()
            .flex_col()
            .px_2p5()
            .pb_2()
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
                            .text_color(theme::TEXT)
                            .opacity(0.7)
                            .child("Workspaces"),
                    )
                    .child(
                        div()
                            .id("add-workspace")
                            .cursor_pointer()
                            .p_1()
                            .rounded_sm()
                            .opacity(0.)
                            .group_hover("wsheader", |s| s.opacity(1.))
                            .hover(|s| s.bg(theme::HOVER))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pick_folder(cx);
                            }))
                            .child(crate::ui::icon("icons/plus.svg", px(fonts.icon_body), theme::TEXT_MUTED)),
                    ),
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
                div()
                    .id("open-settings")
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
                        this.settings_fields = this.make_settings_fields(window, cx);
                        cx.notify();
                    }))
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
        let (fonts, accent_dim) = {
            let store = self.store.read(cx);
            (store.fonts, store.accent.dim)
        };
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
            .group("wsrow")
            .flex()
            .items_center()
            .w_full()
            .px_2()
            .py_1p5()
            .rounded_lg()
            .when(ws.is_selected, |d| d.bg(accent_dim))
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
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .ml_1p5()
                    .text_size(px(fonts.font_body))
                    .text_color(theme::TEXT)
                    .truncate()
                    .child(SharedString::from(ws.name.clone())),
            )
            .when(ws.is_running, |d| d.child(div().size(px(6.)).rounded_full().bg(theme::WORKING)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .when(ws.has_chats(), |d| {
                        d.child(
                            div()
                                .group_hover("wsrow", |s| s.opacity(0.))
                                .text_size(px(fonts.font_micro))
                                .text_color(theme::TEXT_MUTED)
                                .opacity(0.45)
                                .child(format!("{}", ws.chats.len())),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .gap_0p5()
                            .opacity(0.)
                            .group_hover("wsrow", |s| s.opacity(1.))
                            .child(
                                div()
                                    .id(gpui::ElementId::NamedInteger("ws-kebab".into(), TileView::hash(&path)))
                                    .cursor_pointer()
                                    .p_1()
                                    .rounded_sm()
                                    .hover(|s| s.bg(theme::HOVER))
                                    .on_click(move |event, _, cx| {
                                        let rows = {
                                            let s = menu_store2.read(cx);
                                            ws_menu_rows(s, &menu_path2)
                                        };
                                        let (x, y) = match event {
                                            gpui::ClickEvent::Mouse(click) => (click.up.position.x.as_f32(), click.up.position.y.as_f32()),
                                            _ => (40.0, 60.0),
                                        };
                                        let _ = menu_tx2.send_blocking(MenuState { x, y, rows });
                                    })
                                    .child(crate::ui::icon("icons/kebab.svg", px(fonts.icon_body), theme::TEXT_MUTED)),
                            )
                            .child(
                                div()
                                    .id(gpui::ElementId::NamedInteger("ws-new".into(), TileView::hash(&path)))
                                    .cursor_pointer()
                                    .p_1()
                                    .rounded_sm()
                                    .hover(|s| s.bg(theme::HOVER))
                                    .on_click(move |_, _, cx| {
                                        ws_store_new.update(cx, |s, cx| s.new_chat_for_workspace(&path_new, cx));
                                    })
                                    .child(crate::ui::icon("icons/chat-outline.svg", px(fonts.icon_body), theme::TEXT_MUTED)),
                            ),
                    ),
            )
            .into_any_element()
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
            let spin = div().child(crate::ui::icon("icons/spinner.svg", px(fonts.icon_small), theme::WORKING)).with_animation(
                gpui::ElementId::NamedInteger("spin".into(), TileView::hash(&id)),
                gpui::Animation::new(std::time::Duration::from_millis(1200)).repeat(),
                |el, delta| el.opacity(0.25f32 + 0.75f32 * (1.0f32 - (delta - 0.5).abs() * 2.0).max(0.0)),
            );
            div().child(spin).into_any_element()
        } else if chat.pin_visible() {
            crate::ui::icon("icons/pin.svg", px(fonts.icon_xsmall), theme::TEXT_MUTED).into_any_element()
        } else {
            div().into_any_element()
        };

        let mut row = div()
            .id(gpui::ElementId::NamedInteger("chat".into(), TileView::hash(&id)))
            .group("chatrow")
            .flex()
            .items_center()
            .ml(px(26.))
            .w_full()
            .px_2()
            .py_1p5()
            .rounded_lg()
            .when(chat.is_selected, |d| d.bg(accent_dim))
            .hover(|s| s.bg(theme::HOVER))
            .cursor_pointer()
            .on_click(move |_, _, cx| {
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
            if let Some((_, field)) = &self.rename {
                row = row.child(div().flex_1().min_w_0().child(Input::new(field)));
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
                .child(
                    div()
                        .group_hover("chatrow", |s| s.opacity(0.))
                        .text_size(px(fonts.font_micro))
                        .text_color(theme::TEXT_MUTED)
                        .opacity(0.45)
                        .child(SharedString::from(chat.updated_label())),
                )
                .child(
                    div()
                        .id(gpui::ElementId::NamedInteger("chat-del".into(), TileView::hash(&id)))
                        .cursor_pointer()
                        .p_1()
                        .rounded_sm()
                        .opacity(0.)
                        .group_hover("chatrow", |s| s.opacity(1.))
                        .hover(|s| s.bg(theme::HOVER))
                        .on_click(move |_, _, cx| {
                            store_del.update(cx, |s, cx| s.delete_session(&id_del, cx));
                        })
                        .child(crate::ui::icon("icons/trash.svg", px(fonts.icon_xsmall), theme::TEXT_MUTED)),
                );
        }
        row.into_any_element()
    }

    fn render_splitter(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
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
        cx.spawn(async move |_this, cx| {
            if let Ok(Ok(Some(paths))) = task.await {
                if let Some(path) = paths.first() {
                    let _ = tx.send_blocking(path.display().to_string());
                }
            }
        })
        .detach();
    }

    fn make_settings_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Vec<(&'static str, Entity<InputState>)> {
        let b = self.settings_buffers.clone();
        let rows: Vec<(&'static str, String)> = vec![
            ("executable", b.open_code_executable),
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
        rows.into_iter()
            .map(|(key, value)| {
                let entity = cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder("")
                        .default_value(value)
                });
                (key, entity)
            })
            .collect()
    }

    fn settings_field_wrap(&self, key: &str) -> gpui::AnyElement {
        self.settings_fields
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, f)| div().flex_1().min_w_0().child(Input::new(f)).into_any_element())
            .unwrap_or_else(|| div().into_any_element())
    }

    fn small_button(&self, label: &str, on_click: impl Fn(&mut App) + 'static) -> gpui::AnyElement {
        div()
            .id(gpui::ElementId::NamedInteger("sbtn".into(), TileView::hash(label)))
            .px_3()
            .py_1()
            .min_h(px(26.))
            .rounded_lg()
            .border_1()
            .border_color(theme::HAIRLINE)
            .text_size(px(12.5))
            .text_color(theme::TEXT)
            .cursor_pointer()
            .hover(|s| s.bg(theme::HOVER))
            .on_click(move |_, _, cx| on_click(cx))
            .child(label.to_string())
            .into_any_element()
    }

    fn settings_label(&self, text: &str, fonts: &theme::FontTokens) -> gpui::AnyElement {
        div()
            .w(px(150.))
            .flex_shrink_0()
            .text_size(px(fonts.font_body))
            .text_color(theme::TEXT)
            .opacity(0.9)
            .child(text.to_string())
            .into_any_element()
    }

    fn settings_hint(&self, text: &str, fonts: &theme::FontTokens) -> gpui::AnyElement {
        div()
            .text_size(px(fonts.font_small))
            .italic()
            .text_color(theme::TEXT_MUTED)
            .opacity(0.5)
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
        let marker: gpui::AnyElement = if checked {
            div()
                .size(px(14.))
                .rounded_sm()
                .bg(theme::GREEN_ACCENT.accent)
                .flex()
                .items_center()
                .justify_center()
                .child(crate::ui::icon("icons/check.svg", px(9.), theme::SURFACE))
                .into_any_element()
        } else {
            div().size(px(14.)).rounded_sm().border_1().border_color(theme::TEXT_MUTED).into_any_element()
        };
        div()
            .id(gpui::ElementId::NamedInteger("chk".into(), TileView::hash(text)))
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
        for (key_ref, label_text) in options {
            let key: &'static str = key_ref;
            let is_on = key == selected;
            row = row.child(
                div()
                    .id(gpui::ElementId::NamedInteger("radio".into(), TileView::hash(key)))
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        on_pick(&mut this.settings_buffers, key);
                        cx.notify();
                    }))
                    .child(if is_on {
                        div().size(px(11.)).rounded_full().bg(theme::GREEN_ACCENT.accent).into_any_element()
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
        self.tile_views.retain(|key, _| keys.contains(key));

        let tile_columns = self.store.read(cx).tile_columns();
        let mut grid = div()
            .grid()
            .grid_cols(tile_columns.max(1) as u16)
            .gap_2()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .p_1();
        for key in &keys {
            if let Some(view) = self.tile_views.get(key) {
                grid = grid.child(view.clone());
            }
        }

        let mut main = div()
            .flex_1()
            .min_w_0()
            .h_full()
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
                        .child(div().mb_4().child(crate::ui::icon("icons/spark.svg", px(56.), theme::TEXT)))
                        .child(
                            div()
                                .text_size(px(fonts.font_small))
                                .italic()
                                .text_color(theme::TEXT_MUTED)
                                .opacity(0.5)
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
                    .child(crate::ui::icon("icons/clock.svg", px(fonts.icon_medium), theme::TEXT_MUTED))
                    .child(
                        div()
                            .text_size(px(fonts.font_body))
                            .text_color(theme::TEXT)
                            .child(SharedString::from(restore_text.clone())),
                    )
                    .child(self.small_button("Restore", {
                        let store = store_handle.clone();
                        move |cx| {
                            store.update(cx, |s, cx| s.restore_previous_session(cx));
                        }
                    }))
                    .child(self.small_button("Ignore", {
                        let store = store_handle.clone();
                        move |cx| {
                            store.update(cx, |s, cx| s.dismiss_restore_prompt(cx));
                        }
                    })),
            );
        }

        let (settings_open, error_visible) = {
            let store = self.store.read(cx);
            (store.is_settings_open, store.error.is_visible)
        };
        if settings_open {
            main = main.child(self.render_settings(cx));
        }

        if error_visible {
            main = main.child(self.render_error_overlay(cx));
        }

        main.into_any_element()
    }

    // ---- settings page ----

    fn render_settings(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let fonts = self.store.read(cx).fonts;
        let buffers = self.settings_buffers.clone();
        let close_store = self.store.clone();
        let save_store = self.store.clone();

        let mut page = div()
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
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(fonts.font_title))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme::TEXT)
                                .child("Settings"),
                        )
                        .child(self.small_button("Back", {
                            move |cx| {
                                close_store.update(cx, |s, _| s.is_settings_open = false);
                            }
                        })),
                )
                .child(
                    div()
                        .grid()
                        .grid_cols(2)
                        .gap_4()
                        .child(
                            div().flex().flex_col().gap_4()
                                .child(self.settings_section("Server", &fonts))
                                .child(self.settings_card(vec![
                                    div().flex().items_center().gap_2p5()
                                        .child(self.settings_label("OpenCode executable", &fonts))
                                        .child(self.settings_field_wrap("executable"))
                                        .into_any_element(),
                                    self.settings_hint("Running servers keep their current binary until stopped and started again.", &fonts),
                                ]))
                                .child(self.settings_section("Chat defaults", &fonts))
                                .child(self.settings_card(vec![
                                    div().flex().items_center().gap_2p5()
                                        .child(self.settings_label("Model", &fonts))
                                        .child(self.settings_field_wrap("model")).into_any_element(),
                                    div().flex().items_center().gap_2p5()
                                        .child(self.settings_label("Variant", &fonts))
                                        .child(self.settings_field_wrap("variant")).into_any_element(),
                                    div().flex().items_center().gap_2p5()
                                        .child(self.settings_label("Agent", &fonts))
                                        .child(self.settings_field_wrap("agent")).into_any_element(),
                                ])),
                        )
                        .child(
                            div().flex().flex_col().gap_4()
                                .child(self.settings_section("Appearance", &fonts))
                                .child(self.settings_card(vec![
                                    div().flex().items_center().gap_2p5()
                                        .child(self.settings_label("Text size", &fonts))
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
                                    div().flex().items_center().gap_2p5()
                                        .child(self.settings_label("Accent", &fonts))
                                        .child({
                                            let mut swatches = div().flex().gap_2p5();
                                            for preset in theme::ACCENT_PRESETS.iter() {
                                                let key = preset.key.to_string();
                                                let selected = buffers.accent_key == preset.key;
                                                swatches = swatches.child(
                                                    div()
                                                        .id(gpui::ElementId::NamedInteger("accent".into(), TileView::hash(preset.key)))
                                                        .size(px(24.))
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
                                    self.settings_hint("Green, amber, salmon and four gray tones; recolors selected states and accents.", &fonts),
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
                                                .child(self.settings_label("Working hours", &fonts))
                                                .child(self.settings_field_wrap("min"))
                                                .child(div().text_color(theme::TEXT_MUTED).child("–"))
                                                .child(self.settings_field_wrap("max"))
                                                .child(self.settings_hint("seconds", &fonts))
                                                .into_any_element());
                                            perm_rows.push(div().flex().items_center().gap_2p5()
                                                .child(self.settings_label("Lunch", &fonts))
                                                .child(self.settings_field_wrap("lunchstart"))
                                                .child(div().text_color(theme::TEXT_MUTED).child("–"))
                                                .child(self.settings_field_wrap("lunchend"))
                                                .child(div().text_color(theme::TEXT_MUTED).child("→"))
                                                .child(self.settings_field_wrap("awaymin"))
                                                .child(div().text_color(theme::TEXT_MUTED).child("–"))
                                                .child(self.settings_field_wrap("awaymax"))
                                                .into_any_element());
                                            perm_rows.push(div().flex().items_center().gap_2p5()
                                                .child(self.settings_label("Off hours", &fonts))
                                                .child(self.settings_field_wrap("offstart"))
                                                .child(div().text_color(theme::TEXT_MUTED).child("→"))
                                                .child(self.settings_field_wrap("workdaystart"))
                                                .into_any_element());
                                            perm_rows.push(self.settings_hint("Random wait before auto-allow, picked by local time.", &fonts));
                                        }
                                    }
                                    self.settings_card(perm_rows)
                                }),
                        ),
                )
                .child(self.small_button("Save", {
                    let fields = self.settings_fields.clone();
                    let buffers = self.settings_buffers.clone();
                    move |cx| {
                        let mut buffers = buffers.clone();
                        for (key, field) in &fields {
                            let text = field.read(cx).value().to_string();
                            match *key {
                                "executable" => buffers.open_code_executable = text,
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
                        buffers.save(&save_store, cx);
                    }
                })),
        );
        page.into_any_element()
    }

    // ---- error overlay ----

    fn render_error_overlay(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let fonts = self.store.read(cx).fonts;
        let (source, is_fatal, message, details, has_more, more_label) = {
            let store = self.store.read(cx);
            let e = &store.error;
            (e.source.clone(), e.is_fatal, e.error_message.clone(), e.error_details.clone(), e.has_more(), e.more_label())
        };
        let store_handle = self.store.clone();
        div()
            .absolute()
            .inset_0()
            .bg(theme::OVERLAY_SCRIM)
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .max_w(px(560.))
                    .rounded_xl()
                    .border_1()
                    .border_color(theme::HAIRLINE)
                    .bg(theme::CARD)
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_2p5()
                            .child(crate::ui::icon("icons/warn.svg", px(fonts.icon_large), theme::DANGER))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .flex_1()
                                    .child(
                                        div()
                                            .text_size(px(fonts.font_body_large))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(theme::TEXT)
                                            .child("Something went wrong"),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(fonts.font_xsmall))
                                            .text_color(theme::TEXT_MUTED)
                                            .child(SharedString::from(source.clone())),
                                    ),
                            ),
                    )
                    .when(is_fatal, |d| {
                        d.child(
                            div()
                                .text_size(px(fonts.font_xsmall))
                                .text_color(theme::DANGER)
                                .child("This error is fatal — the app closes when you continue."),
                        )
                    })
                    .child(
                        div()
                            .text_size(px(fonts.font_body))
                            .text_color(theme::TEXT)
                            .child(SharedString::from(message.clone())),
                    )
                    .child(
                        div()
                            .id("error-details")
                            .rounded_lg()
                            .bg(theme::DIFF_BG)
                            .p_2p5()
                            .max_h(px(220.))
                            .overflow_y_scroll()
                            .text_size(px(fonts.font_detail))
                            .font_family(theme::CODE_FONT)
                            .text_color(theme::TEXT)
                            .opacity(0.75)
                            .child(SharedString::from(details.clone())),
                    )
                    .when(has_more, |d| {
                        d.child(
                            div()
                                .text_size(px(fonts.font_xsmall))
                                .text_color(theme::TEXT_MUTED)
                                .child(SharedString::from(more_label.clone())),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(self.small_button("Copy details", {
                                let store = store_handle.clone();
                                move |cx| {
                                    let text = store.read(cx).error_copy_text();
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                                }
                            }))
                            .child(self.small_button("Continue", {
                                let store = store_handle.clone();
                                move |cx| {
                                    store.update(cx, |s, _| s.close_error_report());
                                }
                            })),
                    ),
            )
            .into_any_element()
    }
}
