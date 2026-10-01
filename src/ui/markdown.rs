//! Chat-answer markdown rendering via gpui-kit's TextView: markdown-rs
//! (mdast) parsing + the kit text layout/render pipeline, with optional
//! tree-sitter syntax highlighting for fenced code blocks (enabled per
//! language via the gpui-kit cargo features).
//!
//! The tile re-renders on every stream event and TextView re-parses from the
//! source each time — the same rebuild-on-change contract the old hand-rolled
//! renderer had.

use gpui::{div, px, AnyElement, IntoElement, InteractiveElement, ParentElement, StatefulInteractiveElement, Styled};
use gpui_kit::component::text::markdown as kit_markdown;

/// Renders a full markdown document as a selectable TextView. Every fenced
/// code block gets a boxed copy button in its top-right corner (the kit
/// positions and scopes the element; the flash-less copy matches the muted
/// message-copy style).
pub fn render_markdown(text: &str, icon_px: f32) -> AnyElement {
    kit_markdown(text.to_string())
        .selectable(true)
        .code_block_actions(move |block, _, _| {
            let code = block.code();
            div()
                .id("copy-code")
                .cursor_pointer()
                .p_1()
                .rounded_sm()
                .border_1()
                .border_color(crate::theme::HAIRLINE)
                .bg(crate::theme::CHIP)
                .hover(|s| s.bg(crate::theme::HOVER))
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.to_string()));
                })
                .child(crate::ui::icon(
                    "icons/copy.svg",
                    px(icon_px),
                    crate::theme::TEXT_MUTED,
                ))
        })
        .into_any_element()
}
