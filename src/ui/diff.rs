//! Small view helper: the unified diff renderer (DiffTextBlock port).

use crate::theme;
use gpui::prelude::*;
use gpui::{div, px, AbsoluteLength, AnyElement, HighlightStyle, StyledText, Window};
use std::ops::Range;

/// Renders a unified diff GitHub-style: muted old/new line-number gutter,
/// green/red per-line background tints, gray @@ hunk separators, muted metadata.
/// One StyledText so selection still spans lines.
pub fn render_diff(text: &str, font_detail: f32, window: &mut Window) -> AnyElement {
    if text.is_empty() {
        return div().into_any_element();
    }
    let base_style = window.text_style();
    let mut style = base_style.clone();
    style.font_family = theme::code_font().into();
    style.font_size = AbsoluteLength::Pixels(px(font_detail));

    let mut combined = String::new();
    let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();

    let add_run = |combined: &mut String,
                   highlights: &mut Vec<(Range<usize>, HighlightStyle)>,
                   s: &str,
                   color: gpui::Rgba,
                   bg: Option<gpui::Rgba>| {
        let start = combined.len();
        combined.push_str(s);
        highlights.push((
            start..combined.len(),
            HighlightStyle {
                color: Some(color.into()),
                background_color: bg.map(Into::into),
                ..Default::default()
            },
        ));
    };

    let add_bg_run = |combined: &mut String,
                      highlights: &mut Vec<(Range<usize>, HighlightStyle)>,
                      s: &str,
                      color: gpui::Rgba,
                      bg: gpui::Hsla| {
        let start = combined.len();
        combined.push_str(s);
        highlights.push((
            start..combined.len(),
            HighlightStyle { color: Some(color.into()), background_color: Some(bg), ..Default::default() },
        ));
    };

    let gutter: gpui::Rgba = theme::DIFF_GUTTER;
    let add_fg: gpui::Rgba = theme::ADD;
    let del_fg: gpui::Rgba = theme::DEL;
    let muted: gpui::Rgba = theme::TEXT_MUTED;
    let text_fg: gpui::Rgba = theme::CODE_FG;
    let add_bg = theme::DIFF_ADD_BG;
    let del_bg = theme::DIFF_DEL_BG;
    let hunk_bg = theme::DIFF_HUNK_BG;

    let mut old: usize = 0;
    let mut cur: usize = 0;
    for (li, raw) in text.replace('\r', "").split('\n').enumerate() {
        if li > 0 {
            combined.push('\n');
        }
        if let Some(hunk) = hunk_header(raw) {
            old = hunk.0;
            cur = hunk.1;
            add_bg_run(&mut combined, &mut highlights, raw, muted, hunk_bg);
            continue;
        }
        if is_meta(raw) {
            let shown = if raw.is_empty() { " " } else { raw };
            add_run(&mut combined, &mut highlights, shown, muted, None);
            continue;
        }
        if let Some(rest) = raw.strip_prefix('+') {
            add_run(&mut combined, &mut highlights, &format!("{cur:>5}  "), gutter, None);
            cur += 1;
            let shown = if rest.is_empty() { " " } else { raw };
            add_bg_run(&mut combined, &mut highlights, shown, add_fg, add_bg);
        } else if let Some(rest) = raw.strip_prefix('-') {
            add_run(&mut combined, &mut highlights, &format!("{old:>5}      "), gutter, None);
            old += 1;
            let shown = if rest.is_empty() { " " } else { raw };
            add_bg_run(&mut combined, &mut highlights, shown, del_fg, del_bg);
        } else {
            add_run(&mut combined, &mut highlights, &format!("{old:>5} {cur:>5}  "), gutter, None);
            old += 1;
            cur += 1;
            if !raw.is_empty() {
                add_run(&mut combined, &mut highlights, raw, text_fg, None);
            }
        }
    }

    let styled = StyledText::new(combined).with_default_highlights(&style, highlights);
    div().w_full().child(styled).into_any_element()
}

fn hunk_header(line: &str) -> Option<(usize, usize)> {
    // @@ -old[,n] +cur[,n] @@
    let rest = line.strip_prefix("@@ -")?;
    let plus = rest.find(" +")?;
    let old: usize = rest[..plus].split(',').next()?.trim().parse().ok()?;
    let tail = &rest[plus + 2..];
    let end = tail.find(" @@")?;
    let cur: usize = tail[..end].split(',').next()?.trim().parse().ok()?;
    Some((old, cur))
}

fn is_meta(line: &str) -> bool {
    ["diff --git", "index ", "--- ", "+++ ", "new file", "deleted file", "old mode", "new mode", "similarity", "rename ", "Binary files", "GIT binary patch", "\\"]
        .iter()
        .any(|p| line.starts_with(p))
}

