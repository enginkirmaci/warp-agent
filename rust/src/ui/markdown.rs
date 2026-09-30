//! Chat-answer markdown rendering via gpui-kit's TextView: markdown-rs
//! (mdast) parsing + the kit text layout/render pipeline, with optional
//! tree-sitter syntax highlighting for fenced code blocks (enabled per
//! language via the gpui-kit cargo features).
//!
//! The tile re-renders on every stream event and TextView re-parses from the
//! source each time — the same rebuild-on-change contract the old hand-rolled
//! renderer had.

use gpui::{AnyElement, IntoElement};
use gpui_kit::component::text::markdown as kit_markdown;

/// Renders a full markdown document as a selectable TextView.
pub fn render_markdown(text: &str) -> AnyElement {
    kit_markdown(text.to_string())
        .selectable(true)
        .into_any_element()
}
