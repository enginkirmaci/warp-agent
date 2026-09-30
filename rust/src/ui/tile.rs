//! One chat tile in the grid: transcript, composer and stream state for a
//! single session — the ChatTile view port.

use crate::store::{ChatItem, QuestionEntry, Store, ToolItem};
use crate::theme;
use crate::ui::diff::render_diff;
use crate::ui::input::TextField;
use crate::ui::markdown::render_markdown;
use gpui::prelude::*;
use gpui::{
    div, img, px, App, AnimationExt, ClickEvent, Context, Entity, Hsla, ImageFormat, MouseButton,
    Pixels, Render, ScrollHandle, SharedString, TextAlign, Window,
};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
    Copy(String),
    /// user message → back into the composer
    EditInput(String),
    /// copy image bytes
    CopyImage { file_name: String, png: Vec<u8> },
}

pub struct TileView {
    pub key: String,
    store: Entity<Store>,
    scroll: ScrollHandle,
    pinned: bool,
    last_max: Pixels,
    expanded: HashSet<String>, // fold state by item id
    picker: Option<PickerKind>,
    menu: Option<ContextMenu>,
    composer: Entity<TextField>,
    question_inputs: HashMap<String, Entity<TextField>>,
    last_composer_text: String,
    flyout_open: bool,
    last_spring_seen: bool,
}

impl TileView {
    pub fn new(store: Entity<Store>, key: String, cx: &mut Context<Self>) -> Self {
        let composer_focus = cx.focus_handle();
        let composer = cx.new(|_| TextField::multiline("Ask opencode…", composer_focus));
        let this = cx.weak_entity();
        let store_handle = store.clone();
        let key_for_composer = key.clone();
        composer.update(cx, |field, _| {
            field.hooks.on_change = Some(Box::new(move |_, cx, text| {
                let text = text.to_string();
                let key = key_for_composer.clone();
                store_handle.update(cx, move |s, _| {
                    if let Some(tile) = s.tile_by_key_mut(&key) {
                        if tile.input != text {
                            tile.input = text;
                            tile.reset_history();
                        }
                    }
                });
            }));
            let store_handle = store.clone();
            let key_for_send = key.clone();
            let this = this.clone();
            field.hooks.on_send = Some(Box::new(move |_, cx| {
                let key = key_for_send.clone();
                store_handle.update(cx, move |s, cx| s.send_from_tile(&key, cx));
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| {
                        this.pinned = true;
                        this.scroll.scroll_to_bottom();
                        cx.notify();
                    });
                }
            }));
            let store_handle = store.clone();
            let key_for_history = key.clone();
            field.hooks.on_history = Some(Box::new(move |_, cx, up| {
                let key = key_for_history.clone();
                let mut handled = false;
                store_handle.update(cx, |s, _| {
                    handled = s.cycle_prompt_history(&key, up);
                });
                handled
            }));
            let store_handle = store.clone();
            let key_for_agent = key.clone();
            field.hooks.on_cycle_agent = Some(Box::new(move |_, cx| {
                let key = key_for_agent.clone();
                store_handle.update(cx, move |s, _| s.cycle_agent(&key));
            }));
            let store_handle = store.clone();
            let key_for_paste = key.clone();
            field.hooks.on_paste = Some(Box::new(move |_, cx| {
                let key = key_for_paste.clone();
                let mut attached_image = false;
                if let Some(item) = cx.read_from_clipboard() {
                    let image = item.entries().iter().find_map(|entry| match entry {
                        gpui::ClipboardEntry::Image(image) => Some(image),
                        _ => None,
                    });
                    if let Some(image) = image {
                        let png = image.bytes.clone();
                        store_handle.update(cx, move |s, _| s.attach_image(&key, png));
                        attached_image = true;
                    }
                }
                attached_image
            }));
        });
        Self {
            key,
            store,
            scroll: ScrollHandle::new(),
            pinned: true,
            last_max: px(0.),
            expanded: HashSet::new(),
            picker: None,
            menu: None,
            composer,
            question_inputs: HashMap::new(),
            last_composer_text: String::new(),
            flyout_open: false,
            last_spring_seen: false,
        }
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
                let text = text.clone();
                let time = crate::store::message_time_label(*created_ms);
                div()
                    .w_full()
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            .max_w(px(560.))
                            .rounded_lg()
                            .bg(theme::BUBBLE_USER)
                            .p_2()
                            .px_3()
                            .on_mouse_down(MouseButton::Right, {
                                let menu_text = text.clone();
                                cx.listener(move |this, _, _, _| {
                                    this.menu = Some(ContextMenu::Copy(menu_text.clone()));
                                })
                            })
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(fonts.font_body_large))
                                    .text_color(theme::TEXT)
                                    .child(SharedString::from(text.clone())),
                            )
                            .child(
                                div().flex().justify_end().items_center().gap_1().child(
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
                let markdown = render_markdown(text, fonts, accent, window, Self::hash(item_id) as usize);
                div()
                    .w_full()
                    .flex()
                    .justify_start()
                    .child(
                        div()
                            .max_w(px(640.))
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
                                div()
                                    .flex()
                                    .justify_end()
                                    .items_center()
                                    .gap_1()
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
                let body = text.clone();
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .child(self.fold_header(item_id, "thinking", None, open, fonts, cx))
                    .when(open, |d| {
                        d.child(
                            div()
                                .id("fold-thinking")
                                .rounded_lg()
                                .bg(theme::DIFF_BG)
                                .p_2()
                                .max_h(px(260.))
                                .overflow_y_scroll()
                                .text_size(px(fonts.font_detail))
                                .text_color(theme::TEXT)
                                .opacity(0.75)
                                .child(SharedString::from(body)),
                        )
                    })
                    .into_any_element()
            }
            ChatItem::Tool(tool) => self.render_tool(item_id, tool, fonts, accent, window, cx),
            ChatItem::Image { file_name, png, created_ms, .. } => {
                let time = crate::store::message_time_label(*created_ms);
                let name = file_name.clone();
                let render = decode_png_render(png);
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
                                cx.listener(move |this, _, _, _| {
                                    this.menu = Some(ContextMenu::CopyImage {
                                        file_name: menu_name.clone(),
                                        png: menu_png.clone(),
                                    });
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
                            .font_family(theme::CODE_FONT)
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
                                .font_family(theme::CODE_FONT)
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
                }
                cx.notify();
            }))
            .child(
                crate::ui::icon(
                    if open { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" },
                    px(11.),
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
                                    .font_family(theme::CODE_FONT)
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
                                        .child(self.pill_button("Allow", true, accent, {
                                            let store = store.clone();
                                            let id = id.clone();
                                            move |cx| {
                                                store.update(cx, |s, _| s.respond_permission(&id, "once", false));
                                            }
                                        }))
                                        .child(self.pill_button("Always", false, accent, {
                                            let store = store.clone();
                                            let id = id.clone();
                                            move |cx| {
                                                store.update(cx, |s, _| s.respond_permission(&id, "always", false));
                                            }
                                        }))
                                        .child(self.pill_button("Deny", false, accent, {
                                            let store = store.clone();
                                            let id = id.clone();
                                            move |cx| {
                                                store.update(cx, |s, _| s.respond_permission(&id, "reject", false));
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
            .text_size(px(12.))
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
            card = card.child(self.render_question_entry(self.key.clone(), item_id, question, qi, entry, fonts, accent, cx));
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
                            .text_size(px(12.))
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
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let input_key = format!("{}#{qi}", question.id);
        // custom-answer text field, created lazily and wired to the store
        if let std::collections::hash_map::Entry::Vacant(slot) = self.question_inputs.entry(input_key.clone()) {
            let mut field = TextField::single_line("custom answer…", cx.focus_handle());
            field.text_color = theme::TEXT.into();
            let store = self.store.clone();
            let qid = question.id.clone();
            let this_key = self.key.clone();
            field.hooks.on_change = Some(Box::new(move |_, cx, text| {
                let text = text.to_string();
                let this_key = this_key.clone();
                let qid = qid.clone();
                store.update(cx, move |s, _| {
                    s.set_question_custom(&this_key, &qid, qi, text);
                });
            }));
            slot.insert(cx.new(|_| field));
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
                                store.update(cx, |s, _| {
                                    s.toggle_question_option(&this_key2, &qid, qi, oi, is_multi);
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
                                    .child(crate::ui::icon("icons/check.svg", px(9.), theme::SURFACE))
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
            column = column.child(div().px_1().child(field.clone()));
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
            model: String,
            variant: String,
            ctx: String,
            has_ctx: bool,
            items: Vec<(String, ChatItem)>,
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
        let snap = {
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
                model: tile.selected_model.clone().unwrap_or_default(),
                variant: tile.selected_variant.clone().unwrap_or_default(),
                ctx: tile.context_text.clone().unwrap_or_default(),
                has_ctx: tile.has_context(),
                items: tile.items.iter().map(|i| (i.id(), i.clone())).collect(),
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
        if snap.input != self.last_composer_text && self.composer.read(cx).text != snap.input {
            let text = snap.input.clone();
            self.composer.update(cx, |field, _| field.set_text(text));
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
            self.scroll.scroll_to_bottom();
        }
        if max_now != self.last_max {
            self.last_max = max_now;
        }

        let items = snap.items.clone();
        let mut transcript = div()
            .id("transcript")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll.clone())
            .on_scroll_wheel(cx.listener(|this, _, _, cx| {
                let offset = this.scroll.offset();
                let max = this.scroll.max_offset();
                let at_bottom = offset.y >= max.y - px(1.);
                this.pinned = at_bottom;
                cx.notify();
            }))
            .p_1()
            .flex()
            .flex_col()
            .gap_1();
        for (item_id, item) in &items {
            transcript = transcript.child(self.render_item(item_id, item, fonts, accent, window, cx));
        }

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
                    if let Some(render) = decode_png_render(&att.data) {
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
        let composer_focus_is = self.composer.read(cx).focus.is_focused(window);
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
                    .child(div().text_size(px_size(fonts.font_small)).child(SharedString::from(snap.selected_agent.clone())))
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
                    .child(div().text_size(px_size(fonts.font_small)).child(SharedString::from(snap.selected_model_short.clone())))
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
        header_right = header_right
            .child(
                div()
                    .max_w(px(220.))
                    .text_size(px_size(fonts.font_micro))
                    .text_color(theme::TEXT_MUTED)
                    .opacity(0.5)
                    .truncate()
                    .child(SharedString::from(snap.model.clone())),
            )
            .child(
                div()
                    .text_size(px_size(fonts.font_micro))
                    .text_color(theme::TEXT_MUTED)
                    .opacity(0.3)
                    .child("·"),
            )
            .child(
                div()
                    .max_w(px(120.))
                    .text_size(px_size(fonts.font_micro))
                    .text_color(theme::TEXT_MUTED)
                    .opacity(0.5)
                    .truncate()
                    .child(SharedString::from(snap.variant.clone())),
            );
        if snap.has_ctx {
            header_right = header_right
                .child(
                    div()
                        .text_size(px_size(fonts.font_micro))
                        .text_color(theme::TEXT_MUTED)
                        .opacity(0.3)
                        .child("·"),
                )
                .child(
                    div()
                        .text_size(px_size(fonts.font_micro))
                        .text_color(theme::TEXT_MUTED)
                        .opacity(0.5)
                        .child(SharedString::from(snap.ctx.clone())),
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
        let mut menu_el = div();
        if let Some(menu) = self.menu.clone() {
            let label = match &menu {
                ContextMenu::Copy(_) => "Copy",
                ContextMenu::EditInput(_) => "Edit in input",
                ContextMenu::CopyImage { .. } => "Copy image",
            };
            let store_handle = self.store.clone();
            let menu_clone = menu.clone();
            menu_el = div().absolute().bottom(px(96.)).left(px(160.)).child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::HAIRLINE)
                    .bg(theme::SETTINGS_BG)
                    .p_1()
                    .shadow_lg()
                    .min_w(px(160.))
                    .child(
                        div()
                            .id("menu-row")
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .text_size(px_size(fonts.font_small))
                            .text_color(theme::TEXT)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::HOVER))
                            .on_click(move |_, _, cx| {
                                match &menu_clone {
                                    ContextMenu::Copy(text) => {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
                                    }
                                    ContextMenu::EditInput(text) => {
                                        let text = text.clone();
                                        let key = store_handle.read(cx).focused_tile.clone().unwrap_or_default();
                                        store_handle.update(cx, move |s, _| {
                                            if let Some(tile) = s.tile_by_key_mut(&key) {
                                                tile.input = text.clone();
                                                tile.reset_history();
                                            }
                                        });
                                    }
                                    ContextMenu::CopyImage { file_name: _, png } => {
                                        let image = gpui::Image {
                                            format: ImageFormat::Png,
                                            bytes: png.clone(),
                                            id: 0,
                                        };
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_image(&image));
                                    }
                                }
                            })
                            .child(label),
                    ),
            );
        }

        // ---- compose ----
        let composer = self.composer.clone();
        let root = div()
            .id(gpui::ElementId::NamedInteger("tile".into(), TileView::hash(&self.key)))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .rounded_xl()
            .border_1()
            .when(snap.is_focused, |d| d.border_color(accent))
            .when(!snap.is_focused, |d| d.border_color(theme::HAIRLINE))
            .bg(theme::CARD)
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, {
                let store = self.store.clone();
                let key = self.key.clone();
                move |_, _, cx| {
                    store.update(cx, |s, cx| s.focus_tile(&key, cx));
                }
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
                            this.scroll.scroll_to_bottom();
                            cx.notify();
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
                    .on_mouse_down(MouseButton::Left, {
                        let composer = self.composer.clone();
                        move |_, window, cx| {
                            let handle = composer.read(cx).focus.clone();
                            window.focus(&handle, cx);
                        }
                    })
                    .child(attachments_el)
                    .child(div().flex_1().min_h_0().child(composer))
                    .child(bottom_row),
            )
            .child(flyout)
            .child(picker_el)
            .child(menu_el);

        root.into_any_element()
    }
}
