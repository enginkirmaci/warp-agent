//! One chat tile in the grid: transcript, composer and stream state for a
//! single session — the ChatTile view port.

use crate::store::{ChatItem, QuestionEntry, Store, ToolItem};
use crate::theme;
use crate::ui::diff::render_diff;
use crate::ui::markdown::render_markdown;
use gpui::prelude::*;
use gpui::{
    canvas, div, img, px, actions, App, AnimationExt, Context, Entity, Focusable, ImageFormat,
    MouseButton, Pixels, Render, ScrollHandle, SharedString, Window,
};
use gpui_kit::component::input::{
    Input, InputEvent, InputState, MoveDown, MoveUp, Textarea, TextareaState,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

// composer-only key overrides, bound in the "Input" context after
// gpui_kit::init so the later binding wins over the editor's own up/down
actions!(warp_agent, [ComposerUp, ComposerDown, ComposerCycleAgent]);

pub fn decode_png_render(png: &[u8]) -> Option<Arc<gpui::RenderImage>> {
    let decoded = image::load_from_memory(png).ok()?;
    let rgba = decoded.to_rgba8();
    Some(Arc::new(gpui::RenderImage::new(smallvec::smallvec![image::Frame::new(rgba)])))
}

enum PickerKind {
    Model,
    Variant,
    Agent,
}

#[derive(Clone)]
enum ContextMenu {
    /// copy text
    Copy(SharedString),
    /// user message → back into the composer
    EditInput(SharedString),
    /// copy image bytes
    CopyImage { file_name: String, png: Arc<Vec<u8>> },
}

pub struct TileView {
    pub key: String,
    store: Entity<Store>,
    scroll: ScrollHandle,
    /// wheel animation target; None when the scroll is at rest
    scroll_target: Option<Pixels>,
    /// whether the smooth-scroll animation loop is running
    scroll_anim: bool,
    pinned: bool,
    last_max: Pixels,
    expanded: HashSet<String>, // fold state by item id
    picker: Option<PickerKind>,
    menu: Option<TileMenu>,
    /// window-space bounds of the tile root, captured each paint; converts
    /// click positions into tile-local menu coordinates
    origin: std::rc::Rc<std::cell::Cell<(f32, f32, f32, f32)>>,
    composer: Entity<TextareaState>,
    question_inputs: HashMap<String, Entity<InputState>>,
    last_composer_text: String,
    flyout_open: bool,
    last_spring_seen: bool,
    /// item id whose copy button is flashing its "copied" state right now
    copied_flash: Option<String>,
    /// message text as pointer-stable SharedStrings by item id — the kit's
    /// text view recognizes the same allocation across frames and skips
    /// re-parsing; entries are replaced when a message's text changes
    md_cache: HashMap<String, SharedString>,
    /// decoded images by item id / attachment key — PNG decode is far too
    /// expensive to repeat on every repaint; evicted with the transcript
    img_cache: HashMap<String, Arc<gpui::RenderImage>>,
}

impl TileView {
    /// Start the one-second "copied" flash for `key` (a message item id, or
    /// "{item_id}#code{n}" for a fenced code block inside it). Reverts itself
    /// after the second unless a newer click has taken over the slot.
    pub fn flash_copied(&mut self, key: String, cx: &mut Context<Self>) {
        self.copied_flash = Some(key.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1000))
                .await;
            let _ = this.update(cx, |this, cx| this.clear_copied_flash(&key, cx));
        })
        .detach();
    }

    /// Revert the flash only if `key` still owns it (a newer click keeps its flash)
    pub fn clear_copied_flash(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.copied_flash.as_deref() == Some(key) {
            self.copied_flash = None;
            cx.notify();
        }
    }

    /// Message text as a pointer-stable SharedString for the kit's text view,
    /// which recognizes the same allocation across frames and skips re-parsing
    /// an unchanged document. Replaced whenever the store's copy has moved on
    /// (the streaming tail).
    fn md_text(&mut self, item_id: &str, text: &str) -> SharedString {
        if let Some(cached) = self.md_cache.get(item_id) {
            if cached.as_ref() == text {
                return cached.clone();
            }
        }
        let fresh = SharedString::from(text.to_string());
        self.md_cache.insert(item_id.to_string(), fresh.clone());
        fresh
    }

    /// Decoded image for a transcript item or composer attachment, cached by a
    /// stable key (item id / "att{key}") — both name immutable bytes, so an
    /// entry never goes stale, only unreachable (pruned after each render).
    fn image_for(&mut self, cache_key: &str, png: &[u8]) -> Option<Arc<gpui::RenderImage>> {
        if let Some(cached) = self.img_cache.get(cache_key) {
            return Some(cached.clone());
        }
        let decoded = decode_png_render(png)?;
        self.img_cache.insert(cache_key.to_string(), decoded.clone());
        Some(decoded)
    }

    /// Drops cache entries whose transcript item or attachment is gone.
    fn prune_caches(&mut self, live: &HashSet<String>) {
        self.md_cache.retain(|k, _| live.contains(k));
        self.img_cache.retain(|k, _| live.contains(k));
    }
}

/// A context menu request: the actions to offer plus the tile-local anchor point.
#[derive(Clone)]
struct TileMenu {
    items: Vec<ContextMenu>,
    x: f32,
    y: f32,
}

/// Miniature context-usage gauge: a ring of dots whose arc fills clockwise
/// from 12 o'clock in proportion to the context window consumed. Green below
/// 60%, amber below 85%, salmon at the danger line. Absolutely-positioned dot
/// divs, not canvas — the canvas variant painted with its bounds shifted by
/// half the container, floating the ring high beside the header text.
fn context_ring(frac: f32) -> gpui::AnyElement {
    let frac = frac.clamp(0., 1.);
    let filled_color = if frac >= 0.85 {
        theme::DANGER
    } else if frac >= 0.6 {
        theme::WARN
    } else {
        theme::WORKING
    };
    let track_color = theme::TEXT_MUTED;
    let dot = 2.6_f32;
    let ring_size = 13.0_f32;
    let radius = (ring_size - dot) / 2.;
    let segments = 28;
    let mut ring = div().relative().flex_shrink_0().size(px(ring_size));
    for i in 0..segments {
        let phase = i as f32 / segments as f32;
        let theta = phase * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
        let (sin, cos) = theta.sin_cos();
        let color = if phase < frac { filled_color } else { track_color };
        ring = ring.child(
            div()
                .absolute()
                .left(px(ring_size / 2. + sin * radius - dot / 2.))
                .top(px(ring_size / 2. - cos * radius - dot / 2.))
                .size(px(dot))
                .rounded_full()
                .bg(color),
        );
    }
    ring.into_any_element()
}

impl TileView {
    pub fn new(store: Entity<Store>, key: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // kit textarea: submit_on_enter gives Warp-style Enter=send, Shift+Enter=newline;
        // a persisted draft (ui.json) is restored straight into the field
        let initial = store
            .read(cx)
            .tile_by_key(&key)
            .map(|t| t.input.clone())
            .unwrap_or_default();
        let composer = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx)
                .placeholder("Ask opencode…")
                .submit_on_enter(true)
                .auto_grow(2, 9);
            if !initial.is_empty() {
                state.set_value(initial.as_str(), window, cx);
            }
            state
        });

        let store_handle = store.clone();
        let key_for_change = key.clone();
        cx.subscribe(&composer, move |_, field, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let text = field.read(cx).value().to_string();
                let key = key_for_change.clone();
                store_handle.update(cx, move |s, _| {
                    if let Some(tile) = s.tile_by_key_mut(&key) {
                        if tile.input != text {
                            tile.input = text;
                            tile.reset_history();
                        }
                    }
                });
            }
        })
        .detach();

        let store_handle = store.clone();
        let key_for_send = key.clone();
        // the first arg is this TileView, already mid-update — mutating it
        // directly is required; this.update() here would re-enter the entity
        // and gpui panics ("cannot update TileView while it is already being
        // updated")
        cx.subscribe(&composer, move |this, _, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                let key = key_for_send.clone();
                store_handle.update(cx, move |s, cx| s.send_from_tile(&key, cx));
                this.pinned = true;
                this.scroll_target = None;
                this.scroll.scroll_to_bottom();
                cx.notify();
            }
        })
        .detach();

        Self {
            key,
            store,
            scroll: ScrollHandle::new(),
            scroll_target: None,
            scroll_anim: false,
            pinned: true,
            last_max: px(0.),
            expanded: HashSet::new(),
            picker: None,
            menu: None,
            origin: std::rc::Rc::new(std::cell::Cell::new((0., 0., 0., 0.))),
            composer,
            question_inputs: HashMap::new(),
            last_composer_text: String::new(),
            flyout_open: false,
            last_spring_seen: false,
            copied_flash: None,
            md_cache: HashMap::new(),
            img_cache: HashMap::new(),
        }
    }

    /// Window click position → tile-local menu anchor, kept inside the tile.
    fn menu_anchor(&self, event: &gpui::MouseDownEvent) -> (f32, f32) {
        let (ox, oy, w, h) = self.origin.get();
        let x = (event.position.x.as_f32() - ox).clamp(8., (w - 185.).max(8.));
        let y = (event.position.y.as_f32() - oy).clamp(8., (h - 64.).max(8.));
        (x, y)
    }

    /// File attach via the platform picker; files over 512 KB are skipped like the C# app.
    fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let task = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach files".into()),
        });
        let store = self.store.clone();
        let key = self.key.clone();
        cx.spawn(async move |_this, cx| {
            if let Ok(Ok(Some(paths))) = task.await {
                for path in paths {
                    let Ok(meta) = std::fs::metadata(&path) else { continue };
                    if meta.len() > 512 * 1024 {
                        let name = path.display().to_string();
                        store.update(cx, |s, cx| s.announce(&format!("{name} is over 512 KB — skipped."), true, cx));
                        continue;
                    }
                    if let Ok(data) = std::fs::read(&path) {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| path.display().to_string());
                        let mime = crate::ui::mime_for_path(&name);
                        let key = key.clone();
                        store.update(cx, move |s, _| s.attach_file(&key, name, mime.into(), data));
                    }
                }
            }
        })
        .detach();
    }

    fn tile<'a>(&self, store: &'a Store) -> Option<&'a crate::store::ChatTile> {
        store.tile_by_key(&self.key)
    }

    /// Glides the transcript toward `target_y` with exponential smoothing
    /// (~60 fps steps). Safe to call per wheel event: the target updates and
    /// a single animation loop drives the handle until it settles.
    fn animate_scroll_to(&mut self, target_y: Pixels, cx: &mut Context<Self>) {
        self.scroll_target = Some(target_y);
        if self.scroll_anim {
            return;
        }
        self.scroll_anim = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(16))
                    .await;
                let settled = this.update(cx, |this, cx| {
                    let Some(target) = this.scroll_target else {
                        this.scroll_anim = false;
                        return true;
                    };
                    let max = this.scroll.max_offset();
                    let target_y = target.min(max.y);
                    let offset = this.scroll.offset();
                    // gpui stores the handle offset negative-down (0 at top,
                    // -max at bottom); scroll_target counts distance scrolled
                    let current = -offset.y;
                    let diff = target_y - current;
                    if diff.abs() < px(1.) {
                        this.scroll.set_offset(gpui::point(offset.x, -target_y));
                        this.scroll_target = None;
                        this.scroll_anim = false;
                        this.pinned = target_y >= max.y - px(1.);
                    } else {
                        this.scroll
                            .set_offset(gpui::point(offset.x, -(current + diff * 0.35)));
                    }
                    cx.notify();
                    // the loop keeps gliding while the animation flag is set;
                    // it reads as "settled" only once it has been cleared
                    !this.scroll_anim
                });
                let Ok(settled) = settled else { break };
                if settled {
                    break;
                }
            }
        })
        .detach();
    }

    fn render_item(
        &mut self,
        item_id: &str,
        item: &ChatItem,
        fonts: theme::FontTokens,
        accent: gpui::Rgba,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        match item {
            ChatItem::UserMessage { text, created_ms, .. } => {
                // one backing allocation per render; the clones below are refcounts
                let text = SharedString::from(text.as_str());
                let time = crate::store::message_time_label(*created_ms);
                let copy_text = text.clone();
                let flash_key = item_id.to_string();
                let flashed = self.copied_flash.as_deref() == Some(item_id);
                let (copy_icon_path, copy_icon_color, copy_border) = if flashed {
                    ("icons/check.svg", accent, accent)
                } else {
                    ("icons/copy.svg", theme::TEXT_MUTED, theme::HAIRLINE)
                };
                div()
                    .w_full()
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            .group("umsg")
                            .max_w(px(560.))
                            .rounded_lg()
                            .bg(theme::BUBBLE_USER)
                            .p_2()
                            .px_3()
                            .on_mouse_down(MouseButton::Right, {
                                let menu_text = text.clone();
                                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                    let (x, y) = this.menu_anchor(event);
                                    this.menu = Some(TileMenu {
                                        items: vec![
                                            ContextMenu::Copy(menu_text.clone()),
                                            ContextMenu::EditInput(menu_text.clone()),
                                        ],
                                        x,
                                        y,
                                    });
                                    cx.notify();
                                })
                            })
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(fonts.font_body_large))
                                    .text_color(theme::TEXT)
                                    .child(text.clone()),
                            )
                            .child(
                                // copy button shares the time line: pinned left,
                                // time pushed to the right (mirrors the assistant
                                // meta line)
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div()
                                            .id(gpui::ElementId::NamedInteger(
                                                "ucopy".into(),
                                                TileView::hash(item_id),
                                            ))
                                            .cursor_pointer()
                                            .p_0p5()
                                            .rounded_sm()
                                            .border_1()
                                            .border_color(copy_border)
                                            .bg(theme::BUBBLE_USER)
                                            .opacity(0.)
                                            .group_hover("umsg", |s| s.opacity(1.))
                                            .hover(|s| s.bg(theme::HOVER))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_text.to_string()));
                                                this.flash_copied(flash_key.clone(), cx);
                                            }))
                                            .child(crate::ui::icon(copy_icon_path, px(fonts.icon_small), copy_icon_color)),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .text_size(px(fonts.font_micro))
                                            .text_color(theme::TEXT_MUTED)
                                            .opacity(0.55)
                                            .child(time),
                                    ),
                            ),
                    )
                    .into_any_element()
            }
            ChatItem::AssistantText { text, meta_label, created_ms, .. } => {
                let time = crate::store::message_time_label(*created_ms);
                let meta = meta_label.clone();
                // cached, pointer-stable: the kit skips re-parsing unchanged text,
                // and the copy button's string is a refcount, not a fresh copy
                let text = self.md_text(item_id, text);
                let copy_text = text.clone();
                let flash_key = item_id.to_string();
                let flashed = self.copied_flash.as_deref() == Some(item_id);
                let (copy_icon_path, copy_icon_color, copy_border) = if flashed {
                    ("icons/check.svg", accent, accent)
                } else {
                    ("icons/copy.svg", theme::TEXT_MUTED, theme::HAIRLINE)
                };
                let markdown = render_markdown(
                    text,
                    fonts.icon_small,
                    item_id.to_string(),
                    self.copied_flash.as_deref(),
                    cx.entity().downgrade(),
                    accent,
                );
                div()
                    .w_full()
                    .flex()
                    .justify_start()
                    .child(
                        div()
                            .max_w(px(640.))
                            .group("amsg")
                            .rounded_lg()
                            .bg(theme::BUBBLE_ASSISTANT)
                            .border_1()
                            .border_color(theme::HAIRLINE)
                            .p_2()
                            .px_3()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(markdown)
                            .child(
                                // copy button shares the meta line: pinned left,
                                // token flow + time pushed to the right
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div()
                                            .id(gpui::ElementId::NamedInteger(
                                                "acopy".into(),
                                                TileView::hash(item_id),
                                            ))
                                            .cursor_pointer()
                                            .p_0p5()
                                            .rounded_sm()
                                            .border_1()
                                            .border_color(copy_border)
                                            .bg(theme::BUBBLE_ASSISTANT)
                                            .opacity(0.)
                                            .group_hover("amsg", |s| s.opacity(1.))
                                            .hover(|s| s.bg(theme::HOVER))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_text.to_string()));
                                                this.flash_copied(flash_key.clone(), cx);
                                            }))
                                            .child(crate::ui::icon(copy_icon_path, px(fonts.icon_small), copy_icon_color)),
                                    )
                                    .child(div().flex_1())
                                    .child(div().text_size(px(fonts.font_micro)).text_color(theme::TEXT_MUTED).opacity(0.7).child(meta))
                                    .child(
                                        div()
                                            .text_size(px(fonts.font_micro))
                                            .text_color(theme::TEXT_MUTED)
                                            .opacity(0.55)
                                            .child(time),
                                    ),
                            ),
                    )
                    .into_any_element()
            }
            ChatItem::Reasoning { text, .. } => {
                let open = self.expanded.contains(item_id);
                let body = SharedString::from(text.as_str());
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(self.fold_header(item_id, "thinking", None, open, fonts, cx))
                    .when(open, |d| {
                        d.child(
                            div()
                                .id(gpui::ElementId::NamedInteger(
                                    "fold-thinking".into(),
                                    TileView::hash(item_id),
                                ))
                                .rounded_lg()
                                .bg(theme::DIFF_BG)
                                .p_2()
                                .max_h(px(260.))
                                .overflow_y_scroll()
                                .text_size(px(fonts.font_detail))
                                .text_color(theme::TEXT)
                                .opacity(0.75)
                                .child(body),
                        )
                    })
                    .into_any_element()
            }
            ChatItem::Tool(tool) => self.render_tool(item_id, tool, fonts, accent, window, cx),
            ChatItem::Image { file_name, png, created_ms, .. } => {
                let time = crate::store::message_time_label(*created_ms);
                // decoded once per item; PNG decode on every repaint dominated
                // long transcripts with screenshots
                let render = self.image_for(item_id, png);
                let mut image_el = div().child(div());
                if let Some(render) = render {
                    image_el = div().child(
                        img(render)
                            .max_w(px(360.))
                            .max_h(px(240.))
                            .object_fit(gpui::ObjectFit::Contain),
                    );
                }
                let menu_png = png.clone();
                let menu_name = file_name.clone();
                div()
                    .w_full()
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            .max_w(px(380.))
                            .rounded_lg()
                            .bg(theme::BUBBLE_USER)
                            .p_2()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .on_mouse_down(MouseButton::Right, {
                                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                                    let (x, y) = this.menu_anchor(event);
                                    this.menu = Some(TileMenu {
                                        items: vec![ContextMenu::CopyImage {
                                            file_name: menu_name.clone(),
                                            png: menu_png.clone(),
                                        }],
                                        x,
                                        y,
                                    });
                                    cx.notify();
                                })
                            })
                            .child(image_el)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(fonts.font_xsmall))
                                            .text_color(theme::TEXT)
                                            .opacity(0.7)
                                            .child(SharedString::from(file_name.clone())),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(fonts.font_micro))
                                            .text_color(theme::TEXT_MUTED)
                                            .opacity(0.55)
                                            .child(time),
                                    ),
                            ),
                    )
                    .into_any_element()
            }
            ChatItem::Permission(perm) => self.render_permission(item_id, perm, fonts, accent, window, cx),
            ChatItem::Question(question) => self.render_question(item_id, question, fonts, accent, window, cx),
            ChatItem::SystemNote { text, is_error, .. } => {
                if *is_error {
                    div()
                        .w_full()
                        .flex()
                        .child(
                            div()
                                .max_w(px(640.))
                                .rounded_lg()
                                .bg(theme::DANGER_DIM)
                                .flex()
                                .overflow_hidden()
                                .child(
                                    div()
                                        .w(px(3.))
                                        .my_2()
                                        .mx_2()
                                        .rounded_sm()
                                        .bg(theme::DANGER),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .py_2()
                                        .pr_3()
                                        .child(
                                            div()
                                                .size(px(7.))
                                                .rounded_full()
                                                .bg(theme::DANGER),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(fonts.font_small))
                                                .text_color(theme::DANGER)
                                                .child(SharedString::from(text.clone())),
                                        ),
                                ),
                        )
                        .into_any_element()
                } else {
                    div()
                        .w_full()
                        .text_align(gpui::TextAlign::Center)
                        .text_size(px(fonts.font_detail))
                        .text_color(theme::TEXT_MUTED)
                        .italic()
                        .py_0p5()
                        .child(SharedString::from(text.clone()))
                        .into_any_element()
                }
            }
        }
    }

    fn render_tool(
        &mut self,
        item_id: &str,
        tool: &ToolItem,
        fonts: theme::FontTokens,
        accent: gpui::Rgba,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let dot_color: gpui::Hsla = if tool.is_error {
            theme::DANGER.into()
        } else if tool.is_running() {
            accent.into()
        } else {
            theme::TEXT_MUTED.into()
        };
        let open_diff = self.expanded.contains(&format!("{item_id}:diff"));
        let open_details = self.expanded.contains(&format!("{item_id}:details"));
        let diff = tool.diff.clone().unwrap_or_default();
        let details = tool.details.clone();
        let has_diff = tool.has_diff();
        let has_details = tool.has_details();

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .mr_11()
                    .child(div().size(px(7.)).rounded_full().bg(dot_color))
                    .child(
                        div()
                            .rounded_md()
                            .px_1p5()
                            .py_0p5()
                            .bg(theme::CHIP)
                            .text_size(px(fonts.font_xsmall))
                            .font_family(theme::code_font())
                            .text_color(theme::TEXT)
                            .child(SharedString::from(tool.tool.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(fonts.font_body))
                            .text_color(theme::TEXT)
                            .opacity(0.8)
                            .truncate()
                            .child(SharedString::from(tool.title.clone())),
                    )
                    .child(
                        div()
                            .text_size(px(fonts.font_xsmall))
                            .text_color(theme::TEXT_MUTED)
                            .opacity(0.5)
                            .child(SharedString::from(tool.status.clone())),
                    ),
            )
            .when(tool.is_error && !tool.error_preview.is_empty(), |d| {
                d.child(
                    div()
                        .max_w(px(520.))
                        .text_size(px(fonts.font_detail))
                        .text_color(theme::DANGER)
                        .truncate()
                        .child(SharedString::from(tool.error_preview.clone())),
                )
            })
            .when(has_diff, |d| {
                d.child(self.fold_header(
                    &format!("{item_id}:diff"),
                    "Changes",
                    Some((tool.numstat.as_ref().map(|n| n.additions).unwrap_or(0), tool.numstat.as_ref().map(|n| n.deletions).unwrap_or(0))),
                    open_diff,
                    fonts,
                    cx,
                ))
                .when(open_diff, |d| {
                    d.child(
                        div()
                            .id(gpui::ElementId::NamedInteger("fold-diff".into(), Self::hash(item_id)))
                            .rounded_lg()
                            .bg(theme::DIFF_BG)
                            .p_2()
                            .max_h(px(260.))
                            .overflow_y_scroll()
                            .child(render_diff(&diff, fonts.font_detail, window)),
                    )
                })
            })
            .when(has_details, |d| {
                d.child(self.fold_header(&format!("{item_id}:details"), "details", None, open_details, fonts, cx)).when(
                    open_details,
                    |d| {
                        d.child(
                            div()
                                .id(gpui::ElementId::NamedInteger("fold-details".into(), Self::hash(item_id)))
                                .rounded_lg()
                                .bg(theme::DIFF_BG)
                                .p_2()
                                .max_h(px(260.))
                                .overflow_y_scroll()
                                .text_size(px(fonts.font_detail))
                                .font_family(theme::code_font())
                                .text_color(theme::TEXT)
                                .opacity(0.75)
                                .child(SharedString::from(details)),
                        )
                    },
                )
            })
            .into_any_element()
    }

    /// A disclosure row: chevron + label, full row is the click target.
    fn fold_header(
        &self,
        key: &str,
        label: &str,
        numstat: Option<(i64, i64)>,
        open: bool,
        fonts: theme::FontTokens,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let key = key.to_string();
        let label = label.to_string();
        div()
            .id(gpui::ElementId::NamedInteger("fold".into(), Self::hash(key.as_str())))
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_lg()
            .hover(|s| s.bg(theme::HOVER))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.expanded.contains(&key) {
                    this.expanded.remove(&key);
                } else {
                    this.expanded.insert(key.clone());
                    // the body grows below the header — stop pinning to the tail
                    // or the expanded content lands above the viewport
                    this.pinned = false;
                }
                cx.notify();
            }))
            .child(
                crate::ui::icon(
                    if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" },
                    px(fonts.icon_xsmall),
                    theme::TEXT_MUTED,
                ),
            )
            .child(
                div()
                    .text_size(px(fonts.font_xsmall))
                    .text_color(theme::TEXT_MUTED)
                    .opacity(0.75)
                    .child(label),
            )
            .when_some(numstat, |d, (adds, dels)| {
                d.when(adds > 0 || dels > 0, |d| {
                    d.child(div().flex().gap_1().child(
                        div().text_size(px(fonts.font_xsmall)).text_color(theme::ADD).child(format!("{adds}")),
                    ))
                    .child(div().text_size(px(fonts.font_xsmall)).text_color(theme::DEL).child(format!("{dels}")))
                })
            })
            .into_any_element()
    }

    fn render_permission(
        &mut self,
        item_id: &str,
        perm: &crate::store::PermissionItem,
        fonts: theme::FontTokens,
        accent: gpui::Rgba,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let open_diff = self.expanded.contains(&format!("{item_id}:diff"));
        let diff = perm.diff.clone().unwrap_or_default();
        div()
            .w_full()
            .flex()
            .child(
                div()
                    .max_w(px(640.))
                    .rounded_lg()
                    .bg(theme::WARN_BG)
                    .mr_10()
                    .overflow_hidden()
                    .flex()
                    .child(div().w(px(3.)).my_2().mx_1p5().rounded_sm().bg(theme::WARN))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .py_2()
                            .pr_3()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        crate::ui::icon("icons/shield.svg", px(fonts.icon_medium), theme::WARN),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(fonts.font_body))
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .text_color(theme::WARN)
                                            .child(SharedString::from(perm.kind.clone())),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(fonts.font_detail))
                                    .font_family(theme::code_font())
                                    .text_color(theme::TEXT)
                                    .opacity(0.9)
                                    .child(SharedString::from(perm.detail.clone())),
                            )
                            .when(perm.is_pending(), |d| {
                                let id = perm.id.clone();
                                let store = self.store.clone();
                                let auto_text = perm
                                    .has_auto_countdown()
                                    .then(|| perm.auto_countdown_text())
                                    .unwrap_or_default();
                                d.child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(self.pill_button("Allow", true, fonts, accent, {
                                            let store = store.clone();
                                            let id = id.clone();
                                            move |cx| {
                                                store.update(cx, |s, cx| s.respond_permission(&id, "once", false, cx));
                                            }
                                        }))
                                        .child(self.pill_button("Always", false, fonts, accent, {
                                            let store = store.clone();
                                            let id = id.clone();
                                            move |cx| {
                                                store.update(cx, |s, cx| s.respond_permission(&id, "always", false, cx));
                                            }
                                        }))
                                        .child(self.pill_button("Deny", false, fonts, accent, {
                                            let store = store.clone();
                                            let id = id.clone();
                                            move |cx| {
                                                store.update(cx, |s, cx| s.respond_permission(&id, "reject", false, cx));
                                            }
                                        }))
                                        .when(!auto_text.is_empty(), |d| {
                                            d.child(
                                                div()
                                                    .italic()
                                                    .text_size(px(fonts.font_xsmall))
                                                    .text_color(theme::TEXT_MUTED)
                                                    .opacity(0.7)
                                                    .child(auto_text),
                                            )
                                        }),
                                )
                            })
                            .when(!perm.is_pending(), |d| {
                                d.child(
                                    div()
                                        .italic()
                                        .text_size(px(fonts.font_detail))
                                        .text_color(theme::TEXT)
                                        .opacity(0.85)
                                        .child(SharedString::from(perm.answer.clone().unwrap_or_default())),
                                )
                            })
                            .when(perm.has_diff(), |d| {
                                d.child(self.fold_header(&format!("{item_id}:diff"), "View diff", None, open_diff, fonts, cx))
                                    .when(open_diff, |d| {
                                        d.child(
                                            div()
                                                .id(gpui::ElementId::NamedInteger("perm-diff".into(), Self::hash(item_id)))
                                                .rounded_lg()
                                                .bg(theme::DIFF_BG)
                                                .p_2()
                                                .max_h(px(260.))
                                                .overflow_y_scroll()
                                                .child(render_diff(&diff, fonts.font_detail, window)),
                                        )
                                    })
                            }),
                    ),
            )
            .into_any_element()
    }

    fn pill_button(
        &self,
        label: &str,
        filled: bool,
        fonts: theme::FontTokens,
        accent: gpui::Rgba,
        on_click: impl Fn(&mut App) + 'static,
    ) -> gpui::AnyElement {
        let (bg, fg) = if filled {
            (accent.into(), gpui::Hsla::from(theme::ON_ACCENT))
        } else {
            (gpui::Hsla::from(theme::CHIP), gpui::Hsla::from(theme::TEXT))
        };
        div()
            .id(gpui::ElementId::NamedInteger("pill".into(), Self::hash(label)))
            .px_3()
            .py_1()
            .rounded_md()
            .text_size(px(fonts.font_body))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .bg(bg)
            .text_color(fg)
            .cursor_pointer()
            .hover(|s| s.opacity(0.9))
            .on_click(move |_, _, cx| on_click(cx))
            .child(label.to_string())
            .into_any_element()
    }

    fn render_question(
        &mut self,
        item_id: &str,
        question: &crate::store::QuestionItem,
        fonts: theme::FontTokens,
        accent: gpui::Rgba,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let pending = question.is_pending();
        let mut card = div().flex().flex_col().gap_1().flex_1().min_w_0();
        if pending {
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(7.)).rounded_full().bg(accent))
                    .child(
                        div()
                            .text_size(px(fonts.font_body))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::TEXT)
                            .child("question"),
                    )
                    .child(
                        div()
                            .text_size(px(fonts.font_xsmall))
                            .text_color(theme::TEXT_MUTED)
                            .opacity(0.5)
                            .child("pick an answer for each question"),
                    ),
            );
        }
        for (qi, entry) in question.questions.iter().enumerate() {
            card = card.child(self.render_question_entry(self.key.clone(), item_id, question, qi, entry, fonts, accent, window, cx));
        }
        if pending {
            let id = question.id.clone();
            let can_submit = question.can_submit();
            let store = self.store.clone();
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pt_1()
                    .child(
                        div()
                            .id(gpui::ElementId::NamedInteger("answer".into(), Self::hash(&id)))
                            .px_3()
                            .py_1()
                            .rounded_md()
                            .text_size(px(fonts.font_body))
                            .bg(accent)
                            .text_color(theme::ON_ACCENT)
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .when(!can_submit, |d| d.opacity(0.5))
                            .when(can_submit, |d| d.hover(|s| s.opacity(0.9)))
                            .on_click({
                                let store = store.clone();
                                let id = id.clone();
                                move |_, _, cx| {
                                    if can_submit {
                                        store.update(cx, |s, cx| s.submit_question(&id, cx));
                                    }
                                }
                            })
                            .child("Answer"),
                    )
                    .child(
                        div()
                            .id(gpui::ElementId::NamedInteger("dismiss".into(), Self::hash(&id)))
                            .cursor_pointer()
                            .p_1()
                            .rounded_sm()
                            .hover(|s| s.bg(theme::HOVER))
                            .on_click({
                                let store = store.clone();
                                let id = id.clone();
                                move |_, _, cx| {
                                    store.update(cx, |s, cx| s.dismiss_question(&id, cx));
                                }
                            })
                            .child(crate::ui::icon(
                                "icons/close.svg",
                                px(fonts.icon_small),
                                theme::TEXT_MUTED,
                            )),
                    ),
            );
        } else if let Some(resolution) = &question.resolution {
            card = card.child(
                div()
                    .italic()
                    .text_size(px(fonts.font_small))
                    .text_color(theme::TEXT_MUTED)
                    .opacity(0.75)
                    .child(SharedString::from(resolution.clone())),
            );
        }
        div()
            .w_full()
            .flex()
            .child(
                div()
                    .max_w(px(640.))
                    .mr_10()
                    .rounded_lg()
                    .bg(theme::BUBBLE_ASSISTANT)
                    .border_1()
                    .border_color(theme::HAIRLINE)
                    .p_2()
                    .flex()
                    .child(card),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_question_entry(
        &mut self,
        this_tile_key: String,
        item_id: &str,
        question: &crate::store::QuestionItem,
        qi: usize,
        entry: &QuestionEntry,
        fonts: theme::FontTokens,
        accent: gpui::Rgba,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let input_key = format!("{}#{qi}", question.id);
        // custom-answer field, created lazily and wired to the store
        if let std::collections::hash_map::Entry::Vacant(slot) = self.question_inputs.entry(input_key.clone()) {
            let store = self.store.clone();
            let qid = question.id.clone();
            let this_key = self.key.clone();
            let field = cx.new(|cx| InputState::new(window, cx).placeholder("custom answer…"));
            cx.subscribe(&field, move |_, field, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let text = field.read(cx).value().to_string();
                    let this_key = this_key.clone();
                    let qid = qid.clone();
                    store.update(cx, move |s, cx| {
                        s.set_question_custom(&this_key, &qid, qi, text, cx);
                    });
                }
            })
            .detach();
            slot.insert(field);
        }
        let field = self.question_inputs.get(&input_key).cloned().unwrap();
        let entry_locked = entry.locked;
        let mut column = div().flex().flex_col().gap_1().pb_1();
        column = column
            .when(!entry.header.is_empty(), |d| {
                d.child(
                    div()
                        .text_size(px(fonts.font_xsmall))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme::TEXT_MUTED)
                        .opacity(0.55)
                        .child(SharedString::from(entry.header.clone())),
                )
            })
            .child(
                div()
                    .text_size(px(fonts.font_body))
                    .text_color(theme::TEXT)
                    .child(SharedString::from(entry.question.clone())),
            );
        for (oi, option) in entry.options.iter().enumerate() {
            let store = self.store.clone();
            let qid = question.id.clone();
            let this_key2 = this_tile_key.clone();
            let is_multi = entry.multiple;
            let selected = option.is_selected;
            let label = option.label.clone();
            let description = option.description.clone();
            column = column.child(
                div()
                    .id(gpui::ElementId::NamedInteger("qopt".into(), Self::hash(&format!("{item_id}:{qi}:{oi}"))))
                    .w_full()
                    .flex()
                    .items_start()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::HAIRLINE)
                    .when(selected, |d| d.bg(theme::Q_OPT_SELECTED))
                    .when(!selected, |d| d.hover(|s| s.bg(theme::HOVER)))
                    .cursor_pointer()
                    .when(!entry_locked, |d| {
                        d.on_click({
                            let store = store.clone();
                            let qid = qid.clone();
                            move |_, _, cx| {
                                store.update(cx, |s, cx| {
                                    s.toggle_question_option(&this_key2, &qid, qi, oi, is_multi, cx);
                                });
                            }
                        })
                    })
                    .child(
                        // marker: checkbox for multi, radio for single
                        if is_multi {
                            if selected {
                                div()
                                    .size(px(13.))
                                    .mt_1()
                                    .rounded_sm()
                                    .bg(accent)
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(crate::ui::icon("icons/check.svg", px(fonts.icon_nano), theme::SURFACE))
                                    .into_any_element()
                            } else {
                                div()
                                    .size(px(13.))
                                    .mt_1()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(theme::TEXT_MUTED)
                                    .into_any_element()
                            }
                        } else if selected {
                            crate::ui::icon("icons/check.svg", px(fonts.icon_small), accent).into_any_element()
                        } else {
                            div()
                                .size(px(12.))
                                .mt_0p5()
                                .rounded_full()
                                .border_1()
                                .border_color(theme::TEXT_MUTED)
                                .into_any_element()
                        },
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(fonts.font_body))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme::TEXT)
                                    .child(SharedString::from(label)),
                            )
                            .when(!description.is_empty(), |d| {
                                d.child(
                                    div()
                                        .text_size(px(fonts.font_detail))
                                        .text_color(theme::TEXT_MUTED)
                                        .opacity(0.6)
                                        .child(SharedString::from(description)),
                                )
                            }),
                    ),
            );
        }
        if !entry_locked {
            column = column.child(div().px_1().child(Input::new(&field)));
        }
        column.into_any_element()
    }

    pub fn hash(s: &str) -> u64 {
        let mut hash: u64 = 5381;
        for b in s.as_bytes() {
            hash = hash.wrapping_mul(33) ^ *b as u64;
        }
        hash
    }
}


impl Render for TileView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        // snapshot everything the render needs from the store, releasing the borrow
        struct Snap {
            fonts: theme::FontTokens,
            accent: gpui::Rgba,
            input: String,
            is_busy: bool,
            is_connected: bool,
            is_focused: bool,
            title: String,
            ctx: String,
            ctx_frac: f32,
            has_ctx: bool,
            items: Vec<(String, Arc<ChatItem>)>,
            queue_keys: Vec<usize>,
            queue: Vec<crate::store::QueueEntry>,
            attachments: Vec<crate::store::AttachmentItem>,
            has_queue: bool,
            has_messages: bool,
            has_variants: bool,
            selected_variant: String,
            selected_model_short: String,
            selected_agent: String,
            has_agents: bool,
            has_tasks_or_agents: bool,
            has_tasks: bool,
            tasks_progress: String,
            tasks: Vec<crate::store::TaskItem>,
            has_tile_agents: bool,
            agents_summary: String,
            tile_agents: Vec<crate::store::RunningAgentItem>,
            multiple_workspaces: bool,
            has_attention: bool,
            allow_on: bool,
            auto_allow: bool,
            has_variants_enabled: bool,
            spring: bool,
        }
        let mut snap = {
            let store = self.store.read(cx);
            let Some(tile) = store.tile_by_key(&self.key) else {
                return div().into_any_element();
            };
            Snap {
                fonts: store.fonts,
                accent: store.accent.accent,
                input: tile.input.clone(),
                is_busy: tile.is_busy(store),
                is_connected: tile
                    .session_id
                    .as_ref()
                    .map(|id| store.has_runtime(id))
                    .unwrap_or(store.is_server_running),
                is_focused: tile.is_focused,
                title: tile.title.clone(),
            ctx: tile.context_text.clone().unwrap_or_default(),
            ctx_frac: tile.context_frac,
            has_ctx: tile.has_context(),
            // Arc'd items: this snapshot is pointer copies, the payloads stay put
            items: tile.items.iter().map(|i| (i.id(), Arc::clone(i))).collect(),
                queue_keys: tile.queue_rows.clone(),
                queue: store.queue.clone(),
                attachments: tile.attachments.clone(),
                has_queue: tile.has_queue_rows(),
                has_messages: tile.has_messages(),
                has_variants: tile.has_variants(),
                selected_variant: tile.selected_variant.clone().unwrap_or_default(),
                selected_model_short: tile.selected_model_short(),
                selected_agent: tile.selected_agent.clone().unwrap_or_default(),
                has_agents: store.has_agents(),
                has_tasks_or_agents: tile.has_tasks_or_agents(),
                has_tasks: tile.has_tasks(),
                tasks_progress: tile.tasks_progress(),
                tasks: tile.tasks.clone(),
                has_tile_agents: tile.has_tile_agents(),
                agents_summary: tile.tile_agents_summary(),
                tile_agents: tile.tile_agents.clone(),
                multiple_workspaces: store.multiple_workspaces(),
                has_attention: store.has_attention_needed(),
                allow_on: store.auto_allow,
                auto_allow: store.auto_allow,
                has_variants_enabled: tile.has_variants(),
                spring: store.todos_spring.as_deref() == Some(self.key.as_str()),
            }
        };
        let fonts = snap.fonts;
        let accent = snap.accent;
        let px_size = |v: f32| px(v);

        // external input changes (history, edit-queued) sync into the field
        if snap.input != self.last_composer_text && self.composer.read(cx).value().as_ref() != snap.input {
            let text = snap.input.clone();
            self.composer.update(cx, |field, cx| field.set_value(text, window, cx));
        }
        self.last_composer_text = snap.input.clone();

        // a live todo batch springs the flyout open
        if snap.spring {
            if !self.last_spring_seen {
                self.flyout_open = true;
                self.last_spring_seen = true;
            }
        } else {
            self.last_spring_seen = false;
        }

        // transcript auto-scroll: keep pinned while content grows
        let max_now = self.scroll.max_offset().y;
        if self.pinned && max_now > self.last_max + px(0.5) && max_now > px(0.) {
            self.scroll_target = None;
            self.scroll.scroll_to_bottom();
        }
        if max_now != self.last_max {
            self.last_max = max_now;
        }

        // moved out rather than cloned — snap's other fields stay usable
        let items = std::mem::take(&mut snap.items);
        let mut transcript = div()
            .id("transcript")
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .track_scroll(&self.scroll.clone())
            .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, window, cx| {
                // the wheel no longer scrolls natively (overflow_hidden keeps
                // the handle-driven offset) — deltas accumulate into a target
                // and a 60 fps animation glides the handle there
                let delta = event.delta.pixel_delta(window.line_height()).y;
                if delta == px(0.) {
                    return;
                }
                let max = this.scroll.max_offset();
                let base = this.scroll_target.unwrap_or_else(|| -this.scroll.offset().y);
                // wheel deltas are positive-up; the target counts down
                let target = (base - delta).clamp(px(0.), max.y);
                this.pinned = target >= max.y - px(1.);
                this.animate_scroll_to(target, cx);
            }))
            .p_1()
            .flex()
            .flex_col()
            .gap_1();
        for (item_id, item) in &items {
            transcript = transcript.child(self.render_item(item_id, item, fonts, accent, window, cx));
        }
        // cache entries only live as long as the transcript/composer entries they serve
        let mut live: HashSet<String> = items.iter().map(|(id, _)| id.clone()).collect();
        live.extend(snap.attachments.iter().map(|a| format!("att{}", a.key)));
        self.prune_caches(&live);

        let jump_visible = !self.pinned && snap.has_messages;

        // ---- queue rows ----
        let mut queue_rows_el = div().flex().flex_col().gap_1();
        for qkey in &snap.queue_keys {
            if let Some(entry) = snap.queue.iter().find(|e| e.key == *qkey) {
                let text = entry.text.clone();
                let count = entry.attachments.len();
                let store_handle = self.store.clone();
                let steer_key = entry.key;
                let edit_key = entry.key;
                let remove_key = entry.key;
                queue_rows_el = queue_rows_el.child(
                    div()
                        .w_full()
                        .rounded_lg()
                        .border_1()
                        .border_color(theme::QUEUED_BORDER)
                        .bg(theme::BUBBLE_QUEUED)
                        .p_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_size(px_size(fonts.font_body_large))
                                        .text_color(theme::TEXT)
                                        .child(SharedString::from(text)),
                                )
                                .when(count > 0, |d| {
                                    d.child(
                                        div()
                                            .text_size(px_size(fonts.font_xsmall))
                                            .text_color(theme::TEXT_MUTED)
                                            .opacity(0.6)
                                            .child(format!("{count} attachment(s)")),
                                    )
                                }),
                        )
                        .child(
                            div()
                                .id(gpui::ElementId::NamedInteger("steer".into(), steer_key as u64))
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(theme::CHIP)
                                .hover(|s| s.bg(theme::QUEUED_BORDER))
                                .cursor_pointer()
                                .text_size(px_size(fonts.font_xsmall))
                                .text_color(theme::TEXT)
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(crate::ui::icon("icons/arrow-up.svg", px_size(fonts.icon_tiny), theme::TEXT))
                                .child("Steer")
                                .on_click({
                                    let store = store_handle.clone();
                                    move |_, _, cx| {
                                        store.update(cx, |s, cx| s.steer_queued(steer_key, cx));
                                    }
                                }),
                        )
                        .child(
                            div()
                                .id(gpui::ElementId::NamedInteger("qedit".into(), edit_key as u64))
                                .cursor_pointer()
                                .p_1()
                                .rounded_sm()
                                .hover(|s| s.bg(theme::HOVER))
                                .on_click({
                                    let store = store_handle.clone();
                                    move |_, _, cx| {
                                        store.update(cx, |s, cx| s.edit_queued(edit_key, cx));
                                    }
                                })
                                .child(crate::ui::icon("icons/edit.svg", px_size(fonts.icon_small), theme::TEXT_MUTED)),
                        )
                        .child(
                            div()
                                .id(gpui::ElementId::NamedInteger("qrm".into(), remove_key as u64))
                                .cursor_pointer()
                                .p_1()
                                .rounded_sm()
                                .hover(|s| s.bg(theme::HOVER))
                                .on_click({
                                    let store = store_handle.clone();
                                    move |_, _, cx| {
                                        store.update(cx, |s, cx| s.remove_queued(remove_key, cx));
                                    }
                                })
                                .child(crate::ui::icon("icons/close.svg", px_size(fonts.icon_xsmall), theme::TEXT_MUTED)),
                        ),
                );
            }
        }

        // ---- attachments strip ----
        let mut attachments_el = div();
        if !snap.attachments.is_empty() {
            let mut strip = div().flex().gap_1p5();
            for att in &snap.attachments {
                let store_handle = self.store.clone();
                let akey = att.key;
                let key = self.key.clone();
                let mut chip = div()
                    .rounded_lg()
                    .bg(theme::CHIP)
                    .p_1()
                    .flex()
                    .flex_col()
                    .items_center();
                if att.is_image() {
                    if let Some(render) = self.image_for(&format!("att{akey}"), &att.data) {
                        chip = chip.child(
                            img(render).size(px(56.)).object_fit(gpui::ObjectFit::Cover).rounded_sm(),
                        );
                    }
                } else {
                    chip = chip.child(crate::ui::icon("icons/doc.svg", px(56.), theme::TEXT_MUTED));
                }
                chip = chip.child(
                    div()
                        .id(gpui::ElementId::NamedInteger("att-rm".into(), akey as u64))
                        .cursor_pointer()
                        .on_click({
                            let store_handle = store_handle.clone();
                            let key = key.clone();
                            move |_, _, cx| {
                                let key = key.clone();
                                store_handle.update(cx, move |s, cx| s.remove_attachment(&key, akey, cx));
                            }
                        })
                        .child(crate::ui::icon("icons/close.svg", px_size(fonts.icon_xsmall), theme::TEXT_MUTED)),
                );
                strip = strip.child(chip);
            }
            attachments_el = strip;
        }

        // ---- bottom row ----
        let allow_on = snap.allow_on;
        let composer_focus_is = self.composer.read(cx).focus_handle(cx).is_focused(window);
        let mut pills = div().flex().items_center().gap_1();
        if snap.has_agents {
            pills = pills.child(
                div()
                    .id("pill-agent")
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_lg()
                    .cursor_pointer()
                    .text_color(theme::TEXT_MUTED)
                    .hover(|s| s.bg(theme::HOVER).text_color(accent))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.picker = if matches!(this.picker, Some(PickerKind::Agent)) { None } else { Some(PickerKind::Agent) };
                        cx.notify();
                    }))
                    .child(div().text_size(px_size(fonts.font_small)).child(SharedString::from(
                        if snap.selected_agent.is_empty() {
                            crate::store::DEFAULT_AGENT_ENTRY.to_string()
                        } else {
                            snap.selected_agent.clone()
                        },
                    )))
                    .child(crate::ui::icon("icons/chevron-down.svg", px_size(fonts.icon_tiny), theme::TEXT_MUTED)),
            );
        }
        pills = pills
            .child(
                div()
                    .id("pill-model")
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_lg()
                    .cursor_pointer()
                    .text_color(theme::TEXT_MUTED)
                    .hover(|s| s.bg(theme::HOVER).text_color(accent))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.picker = if matches!(this.picker, Some(PickerKind::Model)) { None } else { Some(PickerKind::Model) };
                        cx.notify();
                    }))
                    .child(div().text_size(px_size(fonts.font_small)).child(SharedString::from(
                        if snap.selected_model_short.is_empty() {
                            crate::store::DEFAULT_MODEL_ENTRY.to_string()
                        } else {
                            snap.selected_model_short.clone()
                        },
                    )))
                    .child(crate::ui::icon("icons/chevron-down.svg", px_size(fonts.icon_tiny), theme::TEXT_MUTED)),
            )
            .child(
                div()
                    .id("pill-variant")
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .rounded_lg()
                    .cursor_pointer()
                    .when(snap.has_variants, |d| {
                        d.text_color(theme::TEXT_MUTED).hover(|s| s.bg(theme::HOVER).text_color(accent))
                    })
                    .when(!snap.has_variants, |d| d.opacity(0.4))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.store.read(cx).tile_by_key(&this.key).map(|t| t.has_variants()).unwrap_or(false) {
                            this.picker = if matches!(this.picker, Some(PickerKind::Variant)) { None } else { Some(PickerKind::Variant) };
                            cx.notify();
                        }
                    }))
                    .child(crate::ui::icon("icons/gauge.svg", px_size(fonts.icon_body), theme::TEXT_MUTED))
                    .child(div().text_size(px_size(fonts.font_small)).child(SharedString::from(snap.selected_variant.clone())))
                    .child(crate::ui::icon("icons/chevron-down.svg", px_size(fonts.icon_tiny), theme::TEXT_MUTED)),
            );

        let send_store = self.store.clone();
        let send_key = self.key.clone();
        let abort_store = self.store.clone();
        let abort_key = self.key.clone();

        let bottom_row = div()
            .flex()
            .items_center()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .id("attach")
                            .cursor_pointer()
                            .p_1()
                            .rounded_md()
                            .text_color(theme::TEXT_MUTED)
                            .hover(|s| s.text_color(accent))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.pick_files(window, cx);
                            }))
                            .child(crate::ui::icon("icons/plus.svg", px_size(fonts.icon_medium), theme::TEXT_MUTED)),
                    )
                    .child(
                        div()
                            .id("allow-toggle")
                            .cursor_pointer()
                            .p_1()
                            .rounded_md()
                            .hover(|s| s.bg(theme::HOVER))
                            .on_click({
                                let store = self.store.clone();
                                move |_, _, cx| {
                                    store.update(cx, |s, cx| {
                                        s.auto_allow = !s.auto_allow;
                                        s.ui.auto_allow_always = Some(s.auto_allow);
                                        s.save_state();
                                        cx.notify();
                                    });
                                }
                            })
                            .child(if allow_on {
                                crate::ui::icon("icons/shield-filled.svg", px_size(fonts.icon_medium), theme::WARN)
                            } else {
                                crate::ui::icon("icons/shield.svg", px_size(fonts.icon_medium), theme::TEXT_MUTED)
                            }),
                    ),
            )
            .child(div().flex_1())
            .child(pills)
            .child(if snap.is_busy {
                div()
                    .id("stop")
                    .size(px(34.))
                    .rounded_lg()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_color(theme::TEXT_MUTED)
                    .hover(|s| s.bg(theme::HOVER).text_color(theme::DANGER))
                    .on_click(move |_, _, cx| {
                        abort_store.update(cx, |s, cx| s.abort_tile(&abort_key, cx));
                    })
                    .child(crate::ui::icon("icons/stop.svg", px_size(fonts.icon_medium), theme::TEXT_MUTED))
                    .into_any_element()
            } else {
                div()
                    .id("send")
                    .size(px(34.))
                    .rounded_lg()
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .when(snap.is_connected, |d| {
                        d.text_color(theme::TEXT_MUTED)
                            .hover(|s| s.bg(theme::HOVER).text_color(accent))
                    })
                    .when(!snap.is_connected, |d| d.opacity(0.4))
                    .on_click(move |_, _, cx| {
                        send_store.update(cx, |s, cx| s.send_from_tile(&send_key, cx));
                    })
                    .child(crate::ui::icon("icons/arrow-up.svg", px_size(fonts.icon_xl), theme::TEXT_MUTED))
                    .into_any_element()
            });

        // ---- header ----
        let close_store = self.store.clone();
        let close_key = self.key.clone();

        let mut header_right = div().flex().items_center().gap_1p5();
        // context usage ring + numeric readout (no "Ctx" label)
        if snap.has_ctx {
            header_right = header_right.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(context_ring(snap.ctx_frac))
                    .child(
                        div()
                            .max_w(px(160.))
                            .text_size(px_size(fonts.font_micro))
                            .text_color(theme::TEXT_MUTED)
                            .opacity(0.5)
                            .truncate()
                            .child(SharedString::from(snap.ctx.clone())),
                    ),
            );
        }
        if snap.has_tasks_or_agents {
            header_right = header_right.child(
                div()
                    .id("tasks-btn")
                    .relative()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .p_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::HOVER))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.flyout_open = !this.flyout_open;
                        cx.notify();
                    }))
                    .when(snap.has_attention, |d| {
                        d.child(crate::ui::icon("icons/warn.svg", px_size(fonts.icon_xsmall), theme::WARN))
                    })
                    .child(crate::ui::icon("icons/expand.svg", px_size(fonts.icon_xsmall), theme::TEXT)),
            );
        }

        let header = div()
            .flex()
            .items_center()
            .gap_1()
            .px_2p5()
            .pt_2()
            .child(
                div()
                    .id("close-tile")
                    .cursor_pointer()
                    .p_1()
                    .rounded_md()
                    .hover(|s| s.bg(theme::HOVER))
                    .on_click(move |_, _, cx| {
                        close_store.update(cx, |s, cx| s.close_tile(&close_key, cx));
                    })
                    .child(crate::ui::icon("icons/close.svg", px_size(fonts.icon_xsmall), theme::TEXT_MUTED)),
            )
            .when(snap.is_busy, |d| {
                let pulse = div()
                    .size(px(5.))
                    .rounded_full()
                    .bg(theme::WORKING)
                    .with_animation("pulse", gpui::Animation::new(std::time::Duration::from_secs(2)).repeat(), |el, delta| {
                        let opacity = 0.2f32 + 0.8f32 * (1.0f32 - (delta - 0.5).abs() * 2.0).max(0.0);
                        el.opacity(opacity)
                    });
                d.child(div().ml_1().child(pulse))
            })
            .child(
                div()
                    .ml_2()
                    .flex_1()
                    .min_w_0()
                    .text_size(px_size(fonts.font_body))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::TEXT)
                    .truncate()
                    .child(SharedString::from(snap.title.clone())),
            )
            .child(header_right);

        // ---- flyout ----
        let mut flyout = div();
        if self.flyout_open && snap.has_tasks_or_agents {
            let mut content = div().flex().flex_col().gap_2().min_w(px(300.)).max_w(px(440.));
            if snap.has_tasks {
                content = content
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_size(px_size(fonts.font_body_large))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme::TEXT)
                                    .child("Progress"),
                            )
                            .child(
                                div()
                                    .text_size(px_size(fonts.font_detail))
                                    .text_color(theme::TEXT_MUTED)
                                    .opacity(0.6)
                                    .child(SharedString::from(snap.tasks_progress.clone())),
                            ),
                    )
                    .child({
                        let mut list = div().flex().flex_col();
                        for task in &snap.tasks {
                            let icon: gpui::AnyElement = if task.completed() {
                                crate::ui::icon("icons/circle-check.svg", px_size(fonts.icon_body), accent).into_any_element()
                            } else if task.in_progress() {
                                crate::ui::icon("icons/play.svg", px_size(fonts.icon_xsmall), accent).into_any_element()
                            } else if task.cancelled() {
                                crate::ui::icon("icons/dash.svg", px_size(fonts.icon_body), theme::TEXT_MUTED).into_any_element()
                            } else {
                                crate::ui::icon("icons/circle.svg", px_size(fonts.icon_body), theme::TEXT_MUTED).into_any_element()
                            };
                            let done = task.done();
                            list = list.child(
                                div()
                                    .flex()
                                    .items_start()
                                    .gap_2()
                                    .py_1()
                                    .child(icon)
                                    .child(
                                        div()
                                            .text_size(px_size(fonts.font_body))
                                            .text_color(theme::TEXT)
                                            .when(done, |d| d.opacity(0.5))
                                            .child(SharedString::from(task.content.clone())),
                                    ),
                            );
                        }
                        list
                    });
            }
            if snap.has_tasks && snap.has_tile_agents {
                content = content.child(div().h(px(1.)).bg(theme::HAIRLINE));
            }
            if snap.has_tile_agents {
                content = content
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_size(px_size(fonts.font_body_large))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme::TEXT)
                                    .child("Agents"),
                            )
                            .child(
                                div()
                                    .text_size(px_size(fonts.font_detail))
                                    .text_color(theme::TEXT_MUTED)
                                    .opacity(0.6)
                                    .child(SharedString::from(snap.agents_summary.clone())),
                            ),
                    )
                    .child({
                        let mut list = div().flex().flex_col();
                        for agent in &snap.tile_agents {
                            let store_handle = self.store.clone();
                            let sid = agent.session_id.clone();
                            let sid_open = agent.session_id.clone();
                            let sid_stop = agent.session_id.clone();
                            list = list.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .py_1()
                                    .child(div().size(px(5.)).rounded_full().bg(accent))
                                    .when(agent.is_subagent, |d| {
                                        d.child(crate::ui::icon("icons/play.svg", px_size(fonts.icon_nano), accent))
                                    })
                                    .when(agent.needs_attention, |d| {
                                        d.child(crate::ui::icon("icons/warn.svg", px_size(fonts.icon_body), theme::WARN))
                                    })
                                    .child(
                                        div()
                                            .id(gpui::ElementId::NamedInteger("open-agent".into(), TileView::hash(&sid_open)))
                                            .cursor_pointer()
                                            .on_click({
                                                let store = store_handle.clone();
                                                move |_, _, cx| {
                                                    store.update(cx, |s, cx| s.open_running(&sid_open, cx));
                                                }
                                            })
                                            .flex()
                                            .flex_col()
                                            .child(
                                                div()
                                                    .max_w(px(220.))
                                                    .text_size(px_size(fonts.font_body))
                                                    .text_color(theme::TEXT)
                                                    .truncate()
                                                    .child(SharedString::from(agent.title.clone())),
                                            )
                                            .when(snap.multiple_workspaces, |d| {
                                                d.child(
                                                    div()
                                                        .text_size(px_size(fonts.font_micro))
                                                        .text_color(theme::TEXT_MUTED)
                                                        .opacity(0.5)
                                                        .child(SharedString::from(agent.workspace.clone())),
                                                )
                                            }),
                                    )
                                    .child(div().flex_1())
                                    .child(
                                        div()
                                            .id(gpui::ElementId::NamedInteger("stop-agent".into(), TileView::hash(&sid_stop)))
                                            .cursor_pointer()
                                            .p_1()
                                            .rounded_sm()
                                            .hover(|s| s.bg(theme::HOVER))
                                            .on_click({
                                                let store = store_handle.clone();
                                                move |_, _, cx| {
                                                    store.update(cx, |s, cx| s.stop_running(&sid_stop, cx));
                                                }
                                            })
                                            .child(crate::ui::icon("icons/stop.svg", px_size(fonts.icon_xsmall), theme::TEXT_MUTED)),
                                    ),
                            );
                        }
                        list
                    });
            }
            flyout = div().absolute().top_8().right_2().child(
                div()
                    .rounded_xl()
                    .border_1()
                    .border_color(theme::HAIRLINE)
                    .bg(theme::FLYOUT_SURFACE)
                    .p_3()
                    .shadow_lg()
                    .id("flyout-body")
                    .max_h(px(460.))
                    .overflow_y_scroll()
                    .child(content),
            );
        }

        // ---- pickers (model/variant/agent dropdowns) ----
        let mut picker_el = div();
        if let Some(kind) = &self.picker {
            let entries: Vec<String> = match kind {
                PickerKind::Model => {
                    let store = self.store.read(cx);
                    store.models.clone()
                }
                PickerKind::Variant => {
                    let store = self.store.read(cx);
                    store.tile_by_key(&self.key).map(|t| t.variants.clone()).unwrap_or_default()
                }
                PickerKind::Agent => {
                    let store = self.store.read(cx);
                    store.agents.clone()
                }
            };
            let kind_tag = match kind {
                PickerKind::Model => 0u64,
                PickerKind::Variant => 1,
                PickerKind::Agent => 2,
            };
            let mut menu = div().id("picker-list").flex().flex_col().min_w(px(220.)).max_h(px(320.)).overflow_y_scroll();
            for entry in &entries {
                let value = entry.clone();
                let is_sentinel = value.starts_with('(');
                let key = format!("pk{kind_tag}{value}");
                menu = menu.child(
                    div()
                        .id(gpui::ElementId::NamedInteger("pkrow".into(), TileView::hash(&key)))
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .text_size(px_size(fonts.font_small))
                        .text_color(theme::TEXT)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::HOVER))
                        .on_click({
                            let value = value.clone();
                            cx.listener(move |this, _, _, cx| {
                            let kind = this.picker.take();
                            let key = this.key.clone();
                            let store = this.store.clone();
                            let value = value.clone();
                            store.update(cx, move |s, _| {
                                match kind {
                                    Some(PickerKind::Model) => {
                                        s.set_tile_model(&key, if is_sentinel { Some("(server default)".into()) } else { Some(value) });
                                    }
                                    Some(PickerKind::Variant) => {
                                        s.set_tile_variant(&key, if is_sentinel { Some("(default)".into()) } else { Some(value) });
                                    }
                                    Some(PickerKind::Agent) => {
                                        s.set_tile_agent(&key, if is_sentinel { Some("(default build)".into()) } else { Some(value) });
                                    }
                                    None => {}
                                }
                            });
                            cx.notify();
                            })
                        })
                        .child(SharedString::from(value)),
                );
            }
            picker_el = div().absolute().bottom_10().right_8().child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::HAIRLINE)
                    .bg(theme::SETTINGS_BG)
                    .p_1()
                    .shadow_lg()
                    .child(menu),
            );
        }

        // context menu overlay
        let tile_entity = cx.entity();
        let menu_owner_key = self.key.clone();
        let mut menu_el: gpui::AnyElement = div().into_any_element();
        if let Some(menu) = self.menu.clone() {
            let store_handle = self.store.clone();
            let composer_handle = self.composer.clone();
            // click-elsewhere catcher closes the menu. Must close on full click,
            // not mouse-down: a down-handler unmounts the menu rows before
            // their on_click (mouse-up) can run, so actions never fire.
            let close_entity = tile_entity.clone();
            let mut rows = div().flex().flex_col();
            for (ix, item) in menu.items.iter().enumerate() {
                let label = match item {
                    ContextMenu::Copy(_) => "Copy",
                    ContextMenu::EditInput(_) => "Edit in input",
                    ContextMenu::CopyImage { .. } => "Copy image",
                };
                let store_handle = store_handle.clone();
                let tile_entity = tile_entity.clone();
                let menu_owner_key = menu_owner_key.clone();
                let composer_handle = composer_handle.clone();
                let item = item.clone();
                rows = rows.child(
                    div()
                        .id(gpui::ElementId::NamedInteger("menu-row".into(), ix as u64))
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .text_size(px_size(fonts.font_small))
                        .text_color(theme::TEXT)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::HOVER))
                        .on_click(move |_, window, cx| {
                            match &item {
                                ContextMenu::Copy(text) => {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string()));
                                }
                                ContextMenu::EditInput(text) => {
                                    let text = text.clone();
                                    // the tile that showed the menu, not wherever focus happens to be
                                    let key = menu_owner_key.clone();
                                    let composer = composer_handle.clone();
                                    store_handle.update(cx, {
                                        let text = text.clone();
                                        move |s, _| {
                                            if let Some(tile) = s.tile_by_key_mut(&key) {
                                                tile.input = text.to_string();
                                                tile.reset_history();
                                            }
                                        }
                                    });
                                    // set_value emits no Change, so the store copy above stands
                                    composer.update(cx, |field, cx| field.set_value(text.as_ref(), window, cx));
                                }
                                ContextMenu::CopyImage { file_name: _, png } => {
                                    let image = gpui::Image {
                                        format: ImageFormat::Png,
                                        bytes: png.as_ref().clone(),
                                        id: 0,
                                    };
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image));
                                }
                            }
                            tile_entity.update(cx, |tv, cx| {
                                tv.menu = None;
                                cx.notify();
                            });
                        })
                        .child(label),
                );
            }
            menu_el = div()
                .id("menu-catcher")
                .absolute()
                .inset_0()
                .on_click(move |_, _, cx| {
                    close_entity.update(cx, |tv, cx| {
                        tv.menu = None;
                        cx.notify();
                    });
                })
                .child(
                div()
                    .absolute()
                    .left(px(menu.x))
                    .top(px(menu.y))
                    .child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::HAIRLINE)
                    .bg(theme::SETTINGS_BG)
                    .p_1()
                    .shadow_lg()
                    .min_w(px(160.))
                    .child(rows),
            ),
            )
            .into_any_element();
        }

        // ---- compose ----
        let composer = self.composer.clone();
        let root = div()
            .id(gpui::ElementId::NamedInteger("tile".into(), TileView::hash(&self.key)))
            .relative()
            .size_full()
            // let the grid track shrink below long-word min-content
            .min_w_0()
            .flex()
            .flex_col()
            .rounded_xl()
            .border_1()
            .when(snap.is_focused, |d| d.border_color(accent))
            .when(!snap.is_focused, |d| d.border_color(theme::HAIRLINE))
            .bg(theme::CARD)
            .overflow_hidden()
            // composer-only key overrides (bound over the kit's "Input" context):
            // forward up/down to the editor, and when the caret cannot move
            // anymore cycle the prompt history instead — Warp-style
            .on_action(cx.listener(|this, _: &ComposerUp, window, cx| {
                let state = this.composer.clone();
                if !state.read(cx).focus_handle(cx).is_focused(window) {
                    return;
                }
                let before = state.read(cx).cursor();
                let focus = state.read(cx).focus_handle(cx).clone();
                focus.dispatch_action(&MoveUp, window, cx);
                let moved = state.read(cx).cursor() != before;
                if !moved {
                    let key = this.key.clone();
                    this.store.update(cx, |s, _| s.cycle_prompt_history(&key, true));
                }
            }))
            .on_action(cx.listener(|this, _: &ComposerDown, window, cx| {
                let state = this.composer.clone();
                if !state.read(cx).focus_handle(cx).is_focused(window) {
                    return;
                }
                let before = state.read(cx).cursor();
                let focus = state.read(cx).focus_handle(cx).clone();
                focus.dispatch_action(&MoveDown, window, cx);
                let moved = state.read(cx).cursor() != before;
                if !moved {
                    let key = this.key.clone();
                    this.store.update(cx, |s, _| s.cycle_prompt_history(&key, false));
                }
            }))
            .on_action(cx.listener(|this, _: &ComposerCycleAgent, _window, cx| {
                let key = this.key.clone();
                this.store.update(cx, |s, _| s.cycle_agent(&key));
            }))
            .on_mouse_down(MouseButton::Left, {
                let store = self.store.clone();
                let key = self.key.clone();
                move |_, _, cx| {
                    store.update(cx, |s, cx| s.focus_tile(&key, cx));
                }
            })
            // paint-phase capture of the tile's window bounds, so right-click
            // positions can be turned into tile-local context-menu anchors
            // (same paint-listener pattern as the sidebar splitter)
            .child({
                let origin = self.origin.clone();
                canvas(
                    |_, _, _| {},
                    move |bounds, _, _, _| {
                        origin.set((
                            bounds.origin.x.as_f32(),
                            bounds.origin.y.as_f32(),
                            bounds.size.width.as_f32(),
                            bounds.size.height.as_f32(),
                        ));
                    },
                )
                .absolute()
                .size_full()
            })
            .child(header)
            .child(div().h(px(1.)).mt_1p5().mx_2p5().bg(theme::TILE_HEADER_SEPARATOR))
            .child(transcript)
            .when(jump_visible, |d| {
                d.child(
                    div()
                        .absolute()
                        .bottom(px(120.))
                        .right_2()
                        .id("jump")
                        .rounded_full()
                        .border_1()
                        .border_color(theme::HAIRLINE)
                        .bg(theme::CHIP)
                        .p_2()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.pinned = true;
                            let target = this.scroll.max_offset().y;
                            this.animate_scroll_to(target, cx);
                        }))
                        .child(crate::ui::icon("icons/arrow-down.svg", px_size(fonts.icon_small), theme::TEXT)),
                )
            })
            .when(snap.has_queue, |d| {
                d.child(div().id("queue-strip").max_h(px(150.)).mx_1().mb_0p5().overflow_y_scroll().child(queue_rows_el))
            })
            .child(
                div()
                    .mx_1()
                    .mb_1()
                    .rounded_xl()
                    .border_1()
                    .when(composer_focus_is, |d| d.border_color(accent))
                    .when(!composer_focus_is, |d| d.border_color(theme::HAIRLINE))
                    .bg(theme::INPUT)
                    .p_1p5()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    // files dragged from the file manager land here as attachments
                    .drag_over::<gpui::ExternalPaths>(move |style, _, _, _| style.border_color(accent))
                    .on_drop({
                        let store = self.store.clone();
                        let key = self.key.clone();
                        move |paths: &gpui::ExternalPaths, _, cx| {
                            let paths: Vec<std::path::PathBuf> = paths.paths().to_vec();
                            let key = key.clone();
                            store.update(cx, move |s, cx| s.attach_paths(&key, &paths, cx));
                        }
                    })
                    .on_mouse_down(MouseButton::Left, {
                        let composer = self.composer.clone();
                        move |_, window, cx| {
                            let handle = composer.read(cx).focus_handle(cx).clone();
                            window.focus(&handle, cx);
                        }
                    })
                    .child(attachments_el)
                    .child(
                        div().flex_1().min_h_0().child(
                            // chrome-less inside: the composer frame (border/bg)
                            // is ours, the textarea only edits.
                            // image paste → attachment instead of text insertion
                            Textarea::new(&composer).appearance(false).bordered(false).on_paste({
                                let store = self.store.clone();
                                let key = self.key.clone();
                                move |item: &gpui::ClipboardItem, _, cx| {
                                    let image = item.entries().iter().find_map(|entry| match entry {
                                        gpui::ClipboardEntry::Image(image) => Some(image),
                                        _ => None,
                                    });
                                    if let Some(image) = image {
                                        let png = image.bytes.clone();
                                        let key = key.clone();
                                        store.update(cx, move |s, _| s.attach_image(&key, png));
                                        true
                                    } else {
                                        false
                                    }
                                }
                            }),
                        ),
                    )
                    .child(bottom_row),
            )
            .child(flyout)
            .child(picker_el)
            .child(menu_el);

        root.into_any_element()
    }
}
