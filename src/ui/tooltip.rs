//! Hover tooltips built on gpui_base's public TooltipOverlay. The kit's own
//! `.tooltip()` support is component-internal (and only wired on kit
//! components), and our windows wrap content in base::Root — so this module
//! provides the same behavior for our hand-built controls: `tip(el, "…")`
//! shows a styled popup after the overlay's 500 ms hover delay, with the
//! same 300 ms grace period when moving between adjacent triggers.

use gpui::prelude::*;
use gpui::{
    AnyView, App, Bounds, Context, Entity, Global, IntoElement, MouseButton, Pixels, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, px,
};
use gpui_kit::base::{ElementExt, TooltipOverlay, TooltipRequest};
use std::cell::Cell;
use std::rc::Rc;

use crate::theme;

/// The window's tooltip overlay plus the presentation bits read from hover
/// handlers (the store is not a gpui global, so its snapshot rides here).
/// Installed once by WarpWindow; `apply_saved_settings` refreshes it.
pub struct TooltipHost {
    pub overlay: Entity<TooltipOverlay>,
    pub enabled: bool,
    pub font_meta: f32,
}

impl Global for TooltipHost {}

/// Installs the overlay; the caller renders the returned entity as the
/// window's last child so tooltips paint above everything.
pub fn install(cx: &mut App, font_meta: f32) -> Entity<TooltipOverlay> {
    let overlay = cx.new(|_| TooltipOverlay::new());
    cx.set_global(TooltipHost {
        overlay: overlay.clone(),
        enabled: true,
        font_meta,
    });
    overlay
}

/// Pushes the settings-page toggle and text-size changes into the host; a
/// no-op before the window exists.
pub fn sync_settings(cx: &mut App, enabled: bool, font_meta: f32) {
    if cx.has_global::<TooltipHost>() {
        let host = cx.global_mut::<TooltipHost>();
        host.enabled = enabled;
        host.font_meta = font_meta;
    }
}

/// The tooltip popup in the app's popover language.
struct Tip {
    text: SharedString,
}

impl Render for Tip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let size = cx
            .try_global::<TooltipHost>()
            .map(|h| h.font_meta)
            .unwrap_or(11.0);
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme::HAIRLINE)
            .bg(theme::POPUP)
            .shadow_md()
            .text_size(px(size))
            .text_color(theme::TEXT_DIM)
            .child(self.text.clone())
    }
}

/// Attaches a tooltip to a stateful element. The host's `enabled` flag is
/// checked at hover time, so the settings toggle acts without waiting for a
/// re-render.
pub fn tip<E>(el: E, text: impl Into<SharedString>) -> E
where
    E: StatefulInteractiveElement + ElementExt,
{
    let text: SharedString = text.into();
    let bounds: Rc<Cell<Bounds<Pixels>>> = Rc::new(Cell::new(Bounds::default()));
    let hover_bounds = bounds.clone();
    el.on_prepaint(move |at, _, _| bounds.set(at))
        .on_hover(move |hovered, window, cx| {
            let overlay = match cx.try_global::<TooltipHost>() {
                Some(host) if host.enabled => host.overlay.clone(),
                _ => return,
            };
            if *hovered {
                let at = hover_bounds.get();
                overlay.update(cx, |o, cx| {
                    o.request_show(
                        TooltipRequest::new(at, {
                            let text = text.clone();
                            move |_, cx| -> AnyView {
                                cx.new(|_| Tip { text: text.clone() }).into()
                            }
                        }),
                        window,
                        cx,
                    );
                });
            } else {
                overlay.update(cx, |o, cx| o.request_hide(window, cx));
            }
        })
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            if let Some(host) = cx.try_global::<TooltipHost>() {
                let overlay = host.overlay.clone();
                overlay.update(cx, |o, cx| o.hide(cx));
            }
        })
}
