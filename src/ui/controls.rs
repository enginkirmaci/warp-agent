//! Shared control builders: the one button look, the one popover/menu chrome,
//! the one fold body — pieces every surface used to re-state by hand. The
//! variants keep the settled visuals (accent pill, chip pill, outlined
//! settings button); the chrome stops the min-width/radius drift between the
//! window and tile menus.

use crate::theme;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, ClickEvent, Div, ElementId, FontWeight, Role, SharedString, Window, div, px,
};

/// The button looks: the accent fill, and the two quiet ones, which are the
/// design's one outlined control. Primary carries the preset's on-accent text
/// color.
pub enum BtnKind {
    Primary(gpui::Rgba),
    Chip,
    Outline,
}

/// The shared button. Hover dims Primary/Chip to 0.9 and tints Outline with
/// the hover wash; `enabled=false` parks the button at half opacity with no
/// hover (the click handler stays wired — callers that need to ignore the
/// click guard it in the closure, like Answer does).
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: BtnKind,
    accent: gpui::Rgba,
    text_size: f32,
    enabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let label: SharedString = label.into();
    let mut el = div()
        .id(id.into())
        .role(Role::Button)
        .aria_label(label.clone())
        .flex_shrink_0()
        .h(px(warpui_kit_core::shell::CONTROL_HEIGHT))
        .px_3()
        .flex()
        .items_center()
        .rounded(px(6.))
        .text_size(px(text_size))
        .cursor_pointer();
    match kind {
        BtnKind::Primary(on_accent) => {
            el = el
                .font_weight(FontWeight::SEMIBOLD)
                .bg(accent)
                .text_color(on_accent);
        }
        BtnKind::Chip | BtnKind::Outline => {
            el = el
                .font_weight(FontWeight::MEDIUM)
                .border_1()
                .border_color(theme::line_strong())
                .bg(theme::panel())
                .text_color(theme::text());
        }
    }
    el.when(enabled, |d| match kind {
        BtnKind::Primary(_) => d.hover(|s| s.opacity(0.9)),
        BtnKind::Chip | BtnKind::Outline => d.hover(|s| s.bg(theme::raised())),
    })
    .when(!enabled, |d| d.opacity(0.5))
    .on_click(on_click)
    .child(label)
    .into_any_element()
}

/// One context-menu row (window and tile menus share it); destructive rows
/// render in the danger color.
pub fn menu_row(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    destructive: bool,
    text_size: f32,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let label: SharedString = label.into();
    div()
        .id(id.into())
        .role(Role::MenuItem)
        .aria_label(label.clone())
        .px_3()
        .py_1p5()
        .rounded_md()
        .text_size(px(text_size))
        .cursor_pointer()
        .when(destructive, |d| d.text_color(theme::DANGER))
        .when(!destructive, |d| d.text_color(theme::text()))
        .hover(|s| s.bg(theme::HOVER))
        .on_click(on_click)
        .child(label)
        .into_any_element()
}

/// The shared popover card chrome: the design's menu edge, overlay bg, soft
/// shadow. Returns a Div so callers keep chaining (p_2, min_w, children).
pub fn popover_card() -> Div {
    div()
        .rounded(px(8.))
        .border_1()
        .border_color(theme::line_strong())
        .bg(theme::overlay())
        .p_1()
        .shadow_lg()
}

/// The collapsible fold body: inset dark panel, 260px cap, scrollable.
/// Returns a Stateful div so callers add their text styling.
pub fn fold_body(id: impl Into<ElementId>) -> gpui::Stateful<Div> {
    div()
        .id(id.into())
        .rounded(px(6.))
        .bg(theme::diff_bg())
        .p_2()
        .max_h(px(260.))
        .overflow_y_scroll()
}
