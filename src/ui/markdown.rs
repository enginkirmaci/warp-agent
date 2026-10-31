//! Chat-answer markdown rendering via gpui-kit's TextView: markdown-rs
//! (mdast) parsing + the kit text layout/render pipeline, with optional
//! tree-sitter syntax highlighting for fenced code blocks (enabled per
//! language via the gpui-kit cargo features).
//!
//! The kit keys a TextViewState to the element and only re-parses when the
//! handed-over allocation changes, so the tile feeds a cached, pointer-stable
//! SharedString per message (TileView.md_text) — streaming edits swap the
//! string, everything else skips straight through.

use gpui::{
    AnyElement, ElementId, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, WeakEntity, div, px,
};
use gpui_kit::base::text::TextView;

use crate::ui::tile::TileView;

/// Renders a full markdown document as a selectable TextView. Every fenced
/// code block gets a boxed copy button in its top-right corner (the kit
/// positions and scopes the element). Clicking flashes the check icon in the
/// accent color for a second — the same animation as the message copy
/// buttons, driven through TileView.copied_flash with keys
/// "{flash_base}#code{n}" (n = fenced block order in this message).
///
/// Built on the base-layer TextView (not the component facade) because the
/// facade doesn't forward the per-instance code-block highlighter that styles
/// diff fences; the global TextViewDefaults style still applies to it.
pub fn render_markdown(
    text: SharedString,
    icon_px: f32,
    flash_base: String,
    flash_active: Option<&str>,
    view: WeakEntity<TileView>,
    accent: gpui::Rgba,
    cx: &gpui::App,
) -> AnyElement {
    let flash_active = flash_active.map(str::to_string);
    // the kit's Fn(+Send+Sync) closure can't own a mutable counter — and the
    // keys only stay stable because the tile re-calls render_markdown on every
    // render, giving each pass a fresh counter fed in fenced-block order
    let block_index = std::sync::atomic::AtomicUsize::new(0);
    // flash_base (= transcript item id) makes the element id unique per
    // message: a constant id here gave every assistant TextView in the
    // transcript the SAME global element id, so their kit states and
    // selection participants collided — selection only worked with a
    // single message
    TextView::markdown(
        ElementId::NamedInteger("md".into(), TileView::hash(&flash_base)),
        text,
    )
    .selectable(true)
    .shared_code_block_highlighter(warpui_kit_diff::shared_highlighter(cx))
    .code_block_actions(move |block, _, _| {
        let code = block.code();
        let n = block_index.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key = format!("{flash_base}#code{n}");
        let flashed = flash_active.as_deref() == Some(key.as_str());
        let (icon_path, icon_color, border) = if flashed {
            ("icons/check.svg", accent, accent)
        } else {
            (
                "icons/copy.svg",
                crate::theme::TEXT_MUTED,
                crate::theme::HAIRLINE,
            )
        };
        let click_view = view.clone();
        let click_key = key.clone();
        crate::ui::tooltip::tip(
            div()
                .id("copy-code")
                .cursor_pointer()
                .p_1()
                .rounded_sm()
                .border_1()
                .border_color(border)
                .bg(crate::theme::CHIP)
                .hover(|s| s.bg(crate::theme::HOVER))
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.to_string()));
                    // flash_copied owns the one-second revert timer
                    let _ =
                        click_view.update(cx, |this, cx| this.flash_copied(click_key.clone(), cx));
                }),
            "Copy code",
        )
        .child(crate::ui::icon(icon_path, px(icon_px), icon_color))
    })
    .into_any_element()
}
