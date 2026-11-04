//! Chat-answer markdown rendering through the suite's own live markdown
//! engine (warpui-kit-markdown) in read-only mode: the same rendered surface
//! the notes editor shows, with the syntax parked everywhere, no caret, no
//! text input, no formatting — but text selection and copy, clickable links
//! and tree-sitter-highlighted code blocks (enabled per language via the
//! kit's cargo features) stay.
//!
//! The editor entity per message lives in TileView.md_editors, created on
//! first render and fed new text as the message streams. Two pitfalls from
//! the TextView era still apply and are honored here:
//!
//! - element ids must be unique per message: a constant id gave every
//!   assistant message in the transcript the SAME global element id, and
//!   their states collided — selection only worked with a single message.
//!   The overlay copy button keys off "{flash_base}#code{n}" (= item id).
//! - the kit re-lays-out only what changed, so the tile feeds a cached,
//!   pointer-stable SharedString per message (TileView.md_text) and the
//!   editor only sees set_value when the text actually moved.

use gpui::prelude::*;
use gpui::{AnyElement, Entity, div, px};
use warpui_kit_markdown::{CodeBlock, LiveEditor, LiveEditorState};

use crate::ui::tile::TileView;

/// Renders one assistant message's markdown. The read-only editor sizes
/// itself to its content (the transcript scrolls as a whole; the editor
/// never becomes an inner scroll area), and every fenced code block hosts a
/// copy button in its top-right corner: clicking flashes the check icon in
/// the accent color for a second — the same animation as the message copy
/// buttons, driven through TileView.copied_flash with keys
/// "{flash_base}#code{n}" (n = fenced block order in this message).
pub fn render_markdown(
    editor: &Entity<LiveEditorState>,
    font_px: f32,
    icon_px: f32,
    flash_base: String,
    flash_active: Option<&str>,
    view: gpui::WeakEntity<TileView>,
    accent: gpui::Rgba,
    cx: &gpui::App,
) -> AnyElement {
    let flash_active = flash_active.map(str::to_string);
    // the overlay closure is re-run on every render of the message, so the
    // flash state reads fresh each time and the per-block counter of the
    // TextView era is replaced by the kit's stable block indices
    let overlay = move |block: &CodeBlock| -> Option<AnyElement> {
        let key = format!("{flash_base}#code{}", block.index);
        let flashed = flash_active.as_deref() == Some(key.as_str());
        let (icon_path, icon_color, border) = if flashed {
            ("icons/check.svg", accent, accent)
        } else {
            (
                "icons/copy.svg",
                crate::theme::text_muted(),
                crate::theme::hairline(),
            )
        };
        let code = block.code.clone();
        let click_view = view.clone();
        let click_key = key.clone();
        Some(
            crate::ui::tooltip::tip(
                div()
                    .id(gpui::ElementId::NamedInteger(
                        "mdcopy".into(),
                        TileView::hash(&key),
                    ))
                    .cursor_pointer()
                    .p_1()
                    .rounded_sm()
                    .border_1()
                    .border_color(border)
                    .bg(crate::theme::chip())
                    .hover(|s| s.bg(crate::theme::HOVER))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.to_string()));
                        // flash_copied owns the one-second revert timer
                        let _ = click_view.update(cx, |this, cx| {
                            this.flash_copied(click_key.clone(), cx)
                        });
                    }),
                "Copy code",
            )
            .child(crate::ui::icon(icon_path, px(icon_px), icon_color))
            .into_any_element(),
        )
    };
    // the wrapper carries the laid-out content height (fit_content): the
    // editor notifies on moves, and TileView re-renders on that notify
    let height = editor.read(cx).content_height().max(1.);
    div()
        .w_full()
        .h(px(height))
        .child(
            LiveEditor::new(editor)
                .read_only(true)
                .fit_content(true)
                .text_size(px(font_px))
                .code_block_overlay(overlay),
        )
        .into_any_element()
}
