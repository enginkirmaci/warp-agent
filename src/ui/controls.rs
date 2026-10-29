//! Shared control builders: the one button look, the one popover/menu chrome,
//! the one fold body — pieces every surface used to re-state by hand. The
//! variants keep the settled visuals (accent pill, chip pill, outlined
//! settings button); the chrome stops the min-width/radius drift between the
//! window and tile menus.

use crate::theme;
use gpui::prelude::*;
use gpui::{div, px, AnyElement, App, ClickEvent, Div, ElementId, FontWeight, Role, SharedString, Window};

/// The button looks: accent-filled pill, quiet chip pill, and the outlined
/// settings/restore style (hairline border, 26px min height). Primary carries
/// the preset's on-accent text color (white under the default blue).
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
        .px_3()
        .py_1()
        .text_size(px(text_size))
        .cursor_pointer();
    match kind {
        BtnKind::Primary(on_accent) => {
            el = el
                .rounded_md()
                .font_weight(FontWeight::SEMIBOLD)
                .bg(accent)
                .text_color(on_accent);
        }
        BtnKind::Chip => {
            el = el
                .rounded_md()
                .font_weight(FontWeight::SEMIBOLD)
                .bg(theme::CHIP)
                .text_color(theme::TEXT);
        }
        BtnKind::Outline => {
            el = el
                .min_h(px(26.))
                .rounded_lg()
                .border_1()
                .border_color(theme::HAIRLINE)
                .text_color(theme::TEXT);
        }
    }
    el.when(enabled, |d| match kind {
        BtnKind::Primary(_) | BtnKind::Chip => d.hover(|s| s.opacity(0.9)),
        BtnKind::Outline => d.hover(|s| s.bg(theme::HOVER)),
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
        .when(!destructive, |d| d.text_color(theme::TEXT))
        .hover(|s| s.bg(theme::HOVER))
        .on_click(on_click)
        .child(label)
        .into_any_element()
}

/// The shared popover card chrome: rounded_lg, hairline border, popup bg,
/// soft shadow. Returns a Div so callers keep chaining (p_2, min_w, children).
pub fn popover_card() -> Div {
    div()
        .rounded_lg()
        .border_1()
        .border_color(theme::HAIRLINE)
        .bg(theme::POPUP)
        .p_1()
        .shadow_lg()
}

/// The collapsible fold body: inset dark panel, 260px cap, scrollable.
/// Returns a Stateful div so callers add their text styling.
pub fn fold_body(id: impl Into<ElementId>) -> gpui::Stateful<Div> {
    div()
        .id(id.into())
        .rounded_lg()
        .bg(theme::DIFF_BG)
        .p_2()
        .max_h(px(260.))
        .overflow_y_scroll()
}
