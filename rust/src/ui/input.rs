//! A minimal multi-line text input on raw gpui primitives (gpui ships no editor).
//! Wrapping text, caret navigation, click-to-position, history cycling hooks,
//! send-on-Enter and image-paste hooks — everything the chat composer needs.

use crate::theme;
use gpui::prelude::*;
use gpui::{
    canvas, div, point, px, size, Canvas, ClickEvent, Context, CursorStyle, FocusHandle, Focusable,
    Font, FontStyle, FontWeight, KeyDownEvent, PaintQuad, Pixels, Point, Render, SharedString,
    TextAlign, TextRun, Window, WrappedLine,
};
use std::cell::RefCell;
use std::ops::DerefMut;
use std::rc::Rc;

pub struct InputHooks {
    /// Enter without shift.
    pub on_send: Option<Box<dyn Fn(&mut Window, &mut Context<TextField>) + 'static>>,
    /// Plain Ctrl+V paste; returns true when the clipboard content was consumed (e.g. an image attach).
    pub on_paste: Option<Box<dyn Fn(&mut Window, &mut Context<TextField>) -> bool + 'static>>,
    /// Up/Down from the first/last line; true when the history consumed the key.
    pub on_history: Option<Box<dyn Fn(&mut Window, &mut Context<TextField>, bool) -> bool + 'static>>,
    /// Shift+Tab.
    pub on_cycle_agent: Option<Box<dyn Fn(&mut Window, &mut Context<TextField>) + 'static>>,
    /// The text changed (typing, paste, delete).
    pub on_change: Option<Box<dyn Fn(&mut Window, &mut Context<TextField>, &str) + 'static>>,
    /// Focus left the field.
    pub on_blur: Option<Box<dyn Fn(&mut Window, &mut Context<TextField>) + 'static>>,
}

impl Default for InputHooks {
    fn default() -> Self {
        Self { on_send: None, on_paste: None, on_history: None, on_cycle_agent: None, on_change: None, on_blur: None }
    }
}

/// Layout results from the last paint, shared between paint and click mapping.
pub struct LayoutCache {
    lines: Vec<WrappedLine>,
    /// origin of the text area in window coordinates (from the last paint)
    pub origin: Point<Pixels>,
    /// wrap width used for the current shape
    pub wrap_width: Pixels,
    /// total painted height of the text
    pub height: Pixels,
}

pub struct TextField {
    pub text: String,
    pub caret: usize, // byte index
    pub focus: FocusHandle,
    pub placeholder: SharedString,
    pub font_size: f32,
    pub line_height: f32,
    pub min_height: f32,
    pub max_height: f32,
    pub text_color: gpui::Hsla,
    pub hooks: InputHooks,
    layout: Rc<RefCell<LayoutCache>>,
}

impl TextField {
    pub fn multiline(placeholder: &str, focus: FocusHandle) -> Self {
        Self {
            text: String::new(),
            caret: 0,
            focus,
            placeholder: placeholder.into(),
            font_size: 13.0,
            line_height: 19.0,
            min_height: 52.0,
            max_height: 180.0,
            text_color: theme::TEXT.into(),
            hooks: InputHooks::default(),
            layout: Rc::new(RefCell::new(LayoutCache {
                lines: Vec::new(),
                origin: point(px(0.), px(0.)),
                wrap_width: px(600.),
                height: px(0.),
            })),
        }
    }

    pub fn single_line(placeholder: &str, focus: FocusHandle) -> Self {
        let mut field = Self::multiline(placeholder, focus);
        field.min_height = 24.0;
        field.max_height = 24.0;
        field.line_height = 20.0;
        field.font_size = 12.5;
        field
    }

    pub fn set_text(&mut self, text: String) {
        self.text = text;
        self.caret = self.text.len();
    }

    fn prev_boundary(text: &str, mut i: usize) -> usize {
        while i > 0 && !text.is_char_boundary(i) {
            i -= 1;
        }
        i
    }

    fn next_boundary(text: &str, mut i: usize) -> usize {
        while i < text.len() && !text.is_char_boundary(i) {
            i += 1;
        }
        i
    }

    fn insert(&mut self, s: &str) {
        let caret = self.caret.min(self.text.len());
        self.text.insert_str(caret, s);
        self.caret = caret + s.len();
    }

    /// (line_index, byte_offset_within_line) for the caret.
    fn caret_line(&self) -> (usize, usize) {
        let mut start = 0;
        for (li, line) in self.text.split('\n').enumerate() {
            let end = start + line.len();
            if self.caret <= end {
                return (li, self.caret - start);
            }
            start = end + 1;
        }
        (usize::MAX, 0)
    }

    fn on_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let control = modifiers.control || modifiers.platform;
        let before = self.text.clone();
        match key {
            "enter" if modifiers.shift => {
                self.insert("\n");
                cx.notify();
            }
            "enter" => {
                if let Some(mut hook) = self.hooks.on_send.take() {
                    hook.deref_mut()(window, cx);
                    self.hooks.on_send = Some(hook);
                }
            }
            "backspace" => {
                if self.caret > 0 {
                    let prev = Self::prev_boundary(&self.text, self.caret);
                    self.text.replace_range(prev..self.caret, "");
                    self.caret = prev;
                    cx.notify();
                }
            }
            "delete" => {
                if self.caret < self.text.len() {
                    let next = Self::next_boundary(&self.text, self.caret);
                    self.text.replace_range(self.caret..next, "");
                    cx.notify();
                }
            }
            "left" => {
                self.caret = Self::prev_boundary(&self.text, self.caret);
                cx.notify();
            }
            "right" => {
                self.caret = Self::next_boundary(&self.text, self.caret);
                cx.notify();
            }
            "up" => {
                let (li, _) = self.caret_line();
                let mut handled = false;
                if li == 0 {
                    if let Some(mut hook) = self.hooks.on_history.take() {
                        handled = hook(window, cx, true);
                        self.hooks.on_history = Some(hook);
                    }
                }
                if !handled {
                    if li > 0 {
                        let lines: Vec<&str> = self.text.split('\n').collect();
                        let mut start = 0;
                        for (i, line) in lines.iter().enumerate() {
                            if i == li - 1 {
                                let offset = (self.caret - Self::line_start(&self.text, li)).min(line.len());
                                let mut off = offset;
                                while off > 0 && !line.is_char_boundary(off) {
                                    off -= 1;
                                }
                                self.caret = start + off;
                                break;
                            }
                            start += line.len() + 1;
                        }
                    }
                    cx.notify();
                }
            }
            "down" => {
                let (li, _) = self.caret_line();
                let count = self.text.split('\n').count();
                let mut handled = false;
                if li + 1 >= count {
                    if let Some(mut hook) = self.hooks.on_history.take() {
                        handled = hook(window, cx, false);
                        self.hooks.on_history = Some(hook);
                    }
                }
                if !handled {
                    if li + 1 < count {
                        let target = li + 1;
                        let mut start = 0;
                        for (i, line) in self.text.split('\n').enumerate() {
                            if i == target {
                                let offset = (self.caret - Self::line_start(&self.text, li)).min(line.len());
                                let mut off = offset;
                                while off > 0 && !line.is_char_boundary(off) {
                                    off -= 1;
                                }
                                self.caret = start + off;
                                break;
                            }
                            start += line.len() + 1;
                        }
                    }
                    cx.notify();
                }
            }
            "home" => {
                let mut start = 0;
                for (li, line) in self.text.split('\n').enumerate() {
                    let end = start + line.len();
                    if self.caret <= end {
                        self.caret = start;
                        break;
                    }
                    start = end + 1;
                }
                let _ = li_unused();
                cx.notify();
            }
            "end" => {
                let mut start = 0;
                for (li, line) in self.text.split('\n').enumerate() {
                    let end = start + line.len();
                    if self.caret <= end {
                        self.caret = end;
                        let _ = li;
                        break;
                    }
                    start = end + 1;
                }
                cx.notify();
            }
            "tab" if modifiers.shift => {
                if let Some(mut hook) = self.hooks.on_cycle_agent.take() {
                    hook(window, cx);
                    self.hooks.on_cycle_agent = Some(hook);
                    cx.notify();
                }
            }
            "v" if control => {
                let mut handled = false;
                if let Some(mut hook) = self.hooks.on_paste.take() {
                    handled = hook(window, cx);
                    self.hooks.on_paste = Some(hook);
                }
                if !handled {
                    if let Some(item) = cx.read_from_clipboard() {
                        if let Some(text) = item.text() {
                            self.insert(&text);
                            cx.notify();
                        }
                    }
                }
            }
            _ => {
                if !control && !modifiers.alt && key.chars().count() == 1 {
                    let ch = key.chars().next().unwrap();
                    self.insert(&ch.to_string());
                    cx.notify();
                }
            }
        }
        if self.text != before {
            if let Some(mut hook) = self.hooks.on_change.take() {
                let text = self.text.clone();
                hook(window, cx, &text);
                self.hooks.on_change = Some(hook);
            }
        }
    }

    fn line_start(text: &str, line_index: usize) -> usize {
        let mut start = 0;
        for (i, line) in text.split('\n').enumerate() {
            if i == line_index {
                return start;
            }
            start += line.len() + 1;
        }
        0
    }
}

fn li_unused() -> usize {
    0
}

impl Focusable for TextField {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for TextField {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let font_size = px(self.font_size);
        let line_height = px(self.line_height);
        let layout_paint = self.layout.clone();
        let layout_click = self.layout.clone();
        let text = SharedString::from(self.text.clone());
        let placeholder = self.placeholder.clone();
        let text_color = self.text_color;
        let caret = self.caret;
        let focused = self.focus.is_focused(window);
        let font = Font {
            family: "Inter".into(),
            features: Default::default(),
            fallbacks: None,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        };
        let measured = {
            let cache = self.layout.borrow();
            (cache.height + px(6.)).max(px(self.min_height))
        };

        div()
            .id("composer-input")
            .track_focus(&self.focus.clone())
            .on_key_down({
                let layout_click = layout_click.clone();
                cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    this.on_key(event, window, cx);
                    cx.stop_propagation();
                })
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
                let position = match event {
                    ClickEvent::Mouse(click) => click.up.position,
                    _ => return,
                };
                let cache = layout_click.borrow();
                let local = point(position.x - cache.origin.x, position.y - cache.origin.y);
                let line_count = this.text.split('\n').count();
                let mut char_start = 0;
                let mut new_caret = this.text.len();
                for (li, line) in this.text.split('\n').enumerate() {
                    let end = char_start + line.len();
                    let line_top = px(this.line_height) * li as f32;
                    if local.y < line_top + px(this.line_height) || li == line_count - 1 {
                        if let Some(wrapped) = cache.lines.get(li) {
                            let clamped = point(local.x, (local.y - line_top).max(px(0.)));
                            let index = match wrapped.index_for_position(clamped, px(this.line_height)) {
                                Ok(i) => i,
                                Err(i) => i,
                            };
                            new_caret = (char_start + index).min(end);
                        }
                        break;
                    }
                    char_start = end + 1;
                }
                this.caret = new_caret;
                cx.notify();
            }))
            .cursor(CursorStyle::IBeam)
            .min_h(px(self.min_height))
            .max_h(px(self.max_height))
            .overflow_y_scroll()
            .child(
                div()
                    .relative()
                    .h(measured)
                    .min_h(px(self.min_height))
                    .when(text.is_empty(), |d| {
                        d.child(
                            div()
                                .px_1()
                                .pt_1()
                                .text_size(font_size)
                                .text_color(theme::TEXT_MUTED)
                                .opacity(0.6)
                                .child(placeholder),
                        )
                    })
                    .child(canvas(
                        move |bounds, window, cx| {
                            // paint: shape with the real wrap width, draw text + caret,
                            // then record the layout for click mapping and height feedback
                            let wrap = (bounds.size.width - px(8.)).max(px(40.));
                            let run = TextRun {
                                len: text.len(),
                                font: font.clone(),
                                color: text_color,
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            };
                            let lines: Vec<WrappedLine> = if text.is_empty() {
                                Vec::new()
                            } else {
                                window
                                    .text_system()
                                    .shape_text(text.clone(), font_size, &[run], Some(wrap), None)
                                    .map(|lines| lines.into_iter().collect())
                                    .unwrap_or_default()
                            };
                            let origin = point(bounds.origin.x + px(4.), bounds.origin.y + px(3.));
                            let mut y = origin.y;
                            for line in &lines {
                                let _ = line.paint(point(origin.x, y), line_height, TextAlign::Left, None, window, cx);
                                y += line_height;
                            }
                            if focused && caret <= text.len() {
                                let mut char_start = 0;
                                for (li, line) in text.split('\n').enumerate() {
                                    let end = char_start + line.len();
                                    if caret <= end {
                                        if let Some(wrapped) = lines.get(li) {
                                            if let Some(pos) =
                                                wrapped.position_for_index(caret - char_start, line_height)
                                            {
                                                window.paint_quad(gpui::fill(
                                                    gpui::Bounds {
                                                        origin: point(origin.x + pos.x, origin.y + pos.y),
                                                        size: size(px(1.5), line_height),
                                                    },
                                                    text_color,
                                                ));
                                            }
                                        }
                                        break;
                                    }
                                    char_start = end + 1;
                                }
                            }
                            let mut cache = layout_paint.borrow_mut();
                            cache.lines = lines;
                            cache.origin = origin;
                            cache.wrap_width = wrap;
                            cache.height = y - origin.y;
                        },
                        |_, _, _, _| {},
                    )),
            )
    }
}
