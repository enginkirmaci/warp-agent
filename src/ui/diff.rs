//! Unified diff + line-numbered code rendering for chat tiles.
//!
//! render_diff draws a tool's file patch Zed-style: one line-number column
//! (new-file numbers for context and additions, old-file numbers for
//! deletions), a +/- marker cell, and two-tone full-row washes — brighter
//! over the gutter, quieter over the content — with tree-sitter token colors
//! inside the lines (language guessed from the patch's file paths, or the
//! tool's file path hint).
//!
//! render_code is the same presentation for plain code (tool detail input /
//! output): a line-number gutter plus token colors, no washes. Pre-numbered
//! tool output (opencode's read tool prefixes `   12\t…`) gets its prefixes
//! lifted into the real gutter instead of double-numbering.
//!
//! Rows are per-line divs rather than one big StyledText: bare StyledText
//! never participates in window text selection (only kit TextViews register
//! as selection participants), so per-line rows cost nothing and buy
//! full-bleed washes that reach the right edge on every row.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{div, px, AbsoluteLength, AnyElement, App, HighlightStyle, StyledText, TextStyle, Window};

use crate::theme;

/// A unified diff with Zed-style gutter, marker column and two-tone washes.
/// `path_hint` (the tool's file path) is the fallback language guess when the
/// patch itself carries no recognizable file paths.
pub fn render_diff(
    text: &str,
    path_hint: Option<&str>,
    font_detail: f32,
    window: &mut Window,
    cx: &App,
) -> AnyElement {
    if text.is_empty() {
        return div().into_any_element();
    }
    let norm = text.replace('\r', "");
    let mut lines: Vec<&str> = norm.split('\n').collect();
    // a trailing newline terminates the last row, it isn't a row of its own
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let (kinds, numbers, max_number) = plan_lines(&lines);
    let gutter_width = max_number.to_string().len().max(4);
    let lang = detect_diff_lang(&lines).or_else(|| path_hint.and_then(lang_from_path));
    let hl_theme = gpui_kit::component::Theme::global(cx).highlight_theme.clone();
    let tokens = match lang {
        Some(lang) => line_tokens(&lines, &kinds, lang, &hl_theme),
        None => vec![Vec::new(); lines.len()],
    };

    let style = code_style(window, font_detail, theme::CODE_FG);
    let muted_style = code_style(window, font_detail, theme::TEXT_MUTED);
    let num_style = HighlightStyle { color: Some(theme::DIFF_GUTTER.into()), ..Default::default() };

    let mut root = div().w_full().flex().flex_col();
    for i in 0..lines.len() {
        let line = lines[i];
        root = root.child(match kinds[i] {
            Kind::Meta => {
                full_row(styled_text(line, &muted_style, &[]))
            }
            Kind::Hunk => {
                full_row(styled_text(line, &muted_style, &[])).bg(theme::DIFF_HUNK_BG)
            }
            kind => {
                let (mark, mark_color) = match kind {
                    Kind::Add => ("+", theme::ADD),
                    Kind::Del => ("-", theme::DEL),
                    _ => (" ", theme::DIFF_GUTTER),
                };
                let (gutter_bg, content_bg) = match kind {
                    Kind::Add => (Some(theme::DIFF_ADD_BG), Some(theme::DIFF_ADD_BG)),
                    Kind::Del => (Some(theme::DIFF_DEL_BG), Some(theme::DIFF_DEL_BG)),
                    _ => (None, None),
                };
                let number = numbers[i];
                let gutter = styled_text(
                    &format!("{number:>gutter_width$} {mark}"),
                    &style,
                    &[
                        (0..gutter_width, num_style),
                        (
                            gutter_width + 1..gutter_width + 2,
                            HighlightStyle { color: Some(mark_color.into()), ..Default::default() },
                        ),
                    ],
                );
                let content = match kind {
                    Kind::Add | Kind::Del => &line[1..],
                    _ => line,
                };
                div()
                    .w_full()
                    .flex()
                    .child(
                        div()
                            .flex_shrink_0()
                            .when_some(gutter_bg, |d, bg| d.bg(bg))
                            .child(gutter),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .pl_2()
                            .when_some(content_bg, |d, bg| d.bg(bg))
                            .child(styled_text(content, &style, &tokens[i])),
                    )
            }
        });
    }
    root.into_any_element()
}

/// Line-numbered code block body (no washes): used for tool detail input /
/// output. Wraps in the caller's chip block; `color` is the base text color
/// token runs don't cover.
pub fn render_code(
    text: &str,
    lang: Option<&'static str>,
    color: gpui::Rgba,
    font_detail: f32,
    window: &mut Window,
    cx: &App,
) -> AnyElement {
    if text.is_empty() {
        return div().into_any_element();
    }
    let norm = text.replace('\r', "");
    let mut lines: Vec<&str> = norm.split('\n').collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    // opencode's read tool prefixes output lines with `   12\t` — lift those
    // numbers into the gutter instead of rendering them twice
    let mut first_number = 1usize;
    if let Some((base, stripped)) = strip_line_numbers(&lines) {
        first_number = base;
        lines = stripped;
    }

    let gutter_width = (first_number + lines.len()).to_string().len().max(2);
    let hl_theme = gpui_kit::component::Theme::global(cx).highlight_theme.clone();
    let kinds = vec![Kind::Ctx; lines.len()];
    let tokens = match lang {
        Some(lang) => line_tokens(&lines, &kinds, lang, &hl_theme),
        None => vec![Vec::new(); lines.len()],
    };

    let style = code_style(window, font_detail, color);
    let num_style = HighlightStyle { color: Some(theme::DIFF_GUTTER.into()), ..Default::default() };

    let mut root = div().w_full().flex().flex_col();
    for (i, line) in lines.iter().enumerate() {
        let number = first_number + i;
        let gutter = styled_text(
            &format!("{number:>gutter_width$}"),
            &style,
            &[(0..gutter_width, num_style)],
        );
        root = root.child(
            div()
                .w_full()
                .flex()
                .child(div().flex_shrink_0().child(gutter))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .pl_2()
                        .child(styled_text(line, &style, &tokens[i])),
                ),
        );
    }
    root.into_any_element()
}

/// The one language sniff for detail payloads: pretty-printed JSON is the
/// structured shape tool input reliably arrives in.
pub fn sniff_lang(text: &str) -> Option<&'static str> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        serde_json::from_str::<serde_json::Value>(text).ok().map(|_| "json")
    } else {
        None
    }
}

// ---- internals -------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Meta,
    Hunk,
    Add,
    Del,
    Ctx,
}

/// One pass: classify every line and track the old/new file counters the hunk
/// headers carry. Context and additions carry the new-file number, deletions
/// the old-file one.
fn plan_lines(lines: &[&str]) -> (Vec<Kind>, Vec<usize>, usize) {
    let mut kinds = Vec::with_capacity(lines.len());
    let mut numbers = Vec::with_capacity(lines.len());
    let mut max_number = 0usize;
    let mut old = 1usize;
    let mut new = 1usize;
    for &line in lines {
        let (kind, number) = if let Some((o, n)) = hunk_header(line) {
            old = o;
            new = n;
            (Kind::Hunk, 0)
        } else if is_meta(line) {
            (Kind::Meta, 0)
        } else if line.starts_with('+') {
            let n = new;
            new += 1;
            (Kind::Add, n)
        } else if line.starts_with('-') {
            let n = old;
            old += 1;
            (Kind::Del, n)
        } else {
            let n = new;
            old += 1;
            new += 1;
            (Kind::Ctx, n)
        };
        max_number = max_number.max(number);
        kinds.push(kind);
        numbers.push(number);
    }
    (kinds, numbers, max_number)
}

/// Highlights every content line in one buffer pass (block comments and
/// strings run across lines) and splits the result back per line, offsets
/// relative to each line's content start.
fn line_tokens(
    lines: &[&str],
    kinds: &[Kind],
    lang: &str,
    theme: &std::sync::Arc<gpui_kit::component::highlighter::HighlightTheme>,
) -> Vec<Vec<(Range<usize>, HighlightStyle)>> {
    let mut buffer = String::new();
    let mut starts = Vec::with_capacity(lines.len());
    let mut lens = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        starts.push(buffer.len());
        let content = match kinds[i] {
            Kind::Add | Kind::Del => &line[1..],
            Kind::Ctx => *line,
            Kind::Meta | Kind::Hunk => "",
        };
        lens.push(content.len());
        buffer.push_str(content);
        buffer.push('\n');
    }
    let styles = crate::ui::code_highlight::highlight_buffer(&buffer, lang, theme);
    let mut out: Vec<Vec<(Range<usize>, HighlightStyle)>> = vec![Vec::new(); lines.len()];
    let mut si = 0;
    for (i, &start) in starts.iter().enumerate() {
        let end = start + lens[i];
        while si < styles.len() && styles[si].0.end <= start {
            si += 1;
        }
        let mut cursor = si;
        while cursor < styles.len() && styles[cursor].0.start < end {
            let (range, style) = &styles[cursor];
            let lo = range.start.max(start) - start;
            let hi = range.end.min(end) - start;
            if lo < hi {
                out[i].push((lo..hi, style.clone()));
            }
            cursor += 1;
        }
    }
    out
}

/// Language names the kit's SyntaxHighlighter accepts, restricted to the
/// grammars this crate enables (see Cargo.toml).
fn lang_from_path(path: &str) -> Option<&'static str> {
    let name = path.rsplit(['/', '\\']).next()?;
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "py" | "pyi" => "python",
        "go" => "go",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "css" | "scss" | "less" => "css",
        "html" | "htm" => "html",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "yml" | "yaml" => "yaml",
        "sh" | "bash" | "zsh" => "bash",
        _ => return None,
    })
}

/// The patch's own file paths are the best language guess; the first
/// recognizable one wins (multi-file patches keep a single language).
fn detect_diff_lang(lines: &[&str]) -> Option<&'static str> {
    for line in lines.iter().take(64) {
        if let Some(path) = line.strip_prefix("+++ ").or_else(|| line.strip_prefix("--- ")) {
            let path = path
                .strip_prefix("a/")
                .or_else(|| path.strip_prefix("b/"))
                .unwrap_or(path);
            if let Some(lang) = lang_from_path(path.trim()) {
                return Some(lang);
            }
        } else if let Some(rest) = line.strip_prefix("diff --git ") {
            // "diff --git a/x b/x" — the second path is the new file
            if let Some(path) = rest.split_whitespace().nth(1) {
                if let Some(lang) = lang_from_path(path.trim_start_matches("b/")) {
                    return Some(lang);
                }
            }
        }
    }
    None
}

/// Lifts `   12\t`-style prefixes into (base number, stripped lines). Needs a
/// tab separator and digits-only prefixes on most non-empty lines, so prose
/// or tab-separated data never trips it.
fn strip_line_numbers<'a>(lines: &[&'a str]) -> Option<(usize, Vec<&'a str>)> {
    let mut base: Option<usize> = None;
    let mut stripped = Vec::with_capacity(lines.len());
    let mut matched = 0usize;
    let mut total = 0usize;
    for line in lines {
        if line.trim().is_empty() {
            stripped.push(*line);
            continue;
        }
        total += 1;
        let Some((prefix, rest)) = line.split_once('\t') else {
            return None;
        };
        let prefix = prefix.trim();
        if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let number: usize = prefix.parse().ok()?;
        base = Some(match base {
            Some(b) => b.min(number),
            None => number,
        });
        matched += 1;
        stripped.push(rest);
    }
    (total > 1 && matched * 5 >= total * 4).then(|| (base.unwrap_or(1), stripped))
}

fn code_style(window: &mut Window, font_detail: f32, color: gpui::Rgba) -> TextStyle {
    let mut style = window.text_style();
    style.font_family = theme::code_font().into();
    style.font_size = AbsoluteLength::Pixels(px(font_detail));
    style.color = color.into();
    style
}

/// One row of text on the ambient background; empty lines keep a single
/// space so the row still has line height.
fn styled_text(
    text: &str,
    style: &TextStyle,
    highlights: &[(Range<usize>, HighlightStyle)],
) -> StyledText {
    let shown: &str = if text.is_empty() { " " } else { text };
    StyledText::new(shown.to_string()).with_default_highlights(style, highlights.iter().cloned())
}

fn full_row(text: StyledText) -> gpui::Div {
    div().w_full().overflow_hidden().child(text)
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

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH: &str = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -3,4 +3,5 @@ fn a() {
     one
-    two
+    2 + 2
+    three
     four
";

    #[test]
    fn numbers_follow_new_file_except_deletions() {
        // the renderer pops a trailing empty line before planning; mirror it
        let lines: Vec<&str> = PATCH.split('\n').filter(|l| !l.is_empty()).collect();
        let (kinds, numbers, max) = plan_lines(&lines);
        assert_eq!(
            kinds,
            vec![Kind::Meta, Kind::Meta, Kind::Hunk, Kind::Ctx, Kind::Del, Kind::Add, Kind::Add, Kind::Ctx]
        );
        // meta + hunk carry no gutter number; context/additions count the new
        // file, deletions the old one
        assert_eq!(numbers, vec![0, 0, 0, 3, 4, 4, 5, 6]);
        assert_eq!(max, 6);
    }

    #[test]
    fn hunk_header_resets_counters() {
        let (_, numbers, _) = plan_lines(&["@@ -41,2 +7,3 @@", " x", "+y", "-z"]);
        // the @@ line itself: hunk, no number; x is new-file line 7 (and moves
        // the old counter to 42), y is new-file line 8, z is old-file line 42
        assert_eq!(numbers, vec![0, 7, 8, 42]);
    }

    #[test]
    fn diff_language_from_paths() {
        assert_eq!(detect_diff_lang(&["+++ b/crates/x/src/main.rs"]), Some("rust"));
        assert_eq!(detect_diff_lang(&["--- a/dir/thing.py"]), Some("python"));
        assert_eq!(detect_diff_lang(&["diff --git a/a.go b/a.go"]), Some("go"));
        assert_eq!(detect_diff_lang(&["@@ -1 +1 @@"],), None);
    }

    #[test]
    fn path_language_covers_enabled_grammars_only() {
        assert_eq!(lang_from_path("a/b/c.rs"), Some("rust"));
        assert_eq!(lang_from_path("x.JSON"), Some("json"));
        assert_eq!(lang_from_path("note.md"), None);
        assert_eq!(lang_from_path("Makefile"), None);
    }

    #[test]
    fn read_tool_prefixes_lift_into_gutter() {
        let text = "   1\tfn a() {}\n   2\t\n  10\tfn b() {}\n";
        let lines: Vec<&str> = text.split('\n').collect();
        let lines = &lines[..lines.len() - 1];
        assert_eq!(strip_line_numbers(lines), Some((1, vec!["fn a() {}", "", "fn b() {}"])));
        // prose with a stray tab stays untouched
        assert_eq!(strip_line_numbers(&["col1\tcol2", "col3\tcol4"]), None);
        assert_eq!(strip_line_numbers(&["just one line\t1"]), None);
    }
}
