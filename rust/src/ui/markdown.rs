//! Markdown renderer for chat answers — the Rust replacement for
//! MarkdownViewerKit.Avalonia. Headings, bold/italic, inline code, fenced code
//! (basic syntax highlighting + copy), lists, links, tables, blockquotes and
//! rules. Rebuilds on every text change so streaming updates just work.

use crate::theme;
use crate::ui::highlight::{highlight, TokenKind};
use gpui::prelude::FluentBuilder;
use gpui::prelude::*;
use gpui::{
    div, px, AbsoluteLength, AnyElement, ElementId, Font, FontFeatures, FontStyle, FontWeight,
    HighlightStyle, Hsla, InteractiveText, Pixels, SharedString, StyledText, TextStyle, UnderlineStyle, Window,
};
use std::ops::Range;

// ---- block model ---------------------------------------------------------

enum Block {
    Paragraph(String),
    Heading(usize, String),
    Code { text: String, lang: String },
    List { ordered: bool, items: Vec<String> },
    Quote(String),
    Table(Vec<Vec<String>>),
    Rule,
}

fn is_rule(trimmed: &str) -> bool {
    let mut chars = trimmed.chars();
    match (trimmed.len() >= 3, chars.next()) {
        (true, Some(c @ ('-' | '*' | '_'))) => trimmed.chars().all(|x| x == c),
        _ => false,
    }
}

fn cells(row: &str) -> Vec<String> {
    let mut inner = row.trim();
    if let Some(stripped) = inner.strip_prefix('|') {
        inner = stripped;
    }
    if let Some(stripped) = inner.strip_suffix('|') {
        inner = stripped;
    }
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

fn table_separator(trimmed: &str) -> bool {
    // | :--- | ---: | pattern
    let mut inner = trimmed;
    if let Some(stripped) = inner.strip_prefix('|') {
        inner = stripped;
    }
    if let Some(stripped) = inner.strip_suffix('|') {
        inner = stripped;
    }
    if inner.is_empty() {
        return false;
    }
    inner.split('|').all(|cell| {
        let cell = cell.trim();
        let core = cell.trim_start_matches(':').trim_end_matches(':');
        !core.is_empty() && core.chars().all(|c| c == '-') && (cell.starts_with(':') || cell.ends_with(':') || cell == core)
    })
}

fn parse_blocks(text: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    if text.is_empty() {
        return blocks;
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let mut para: Vec<&str> = Vec::new();
    let mut list_items: Vec<String> = Vec::new();
    let mut list_ordered = false;
    let mut in_list = false;
    let mut in_quote = false;
    let mut quote = String::new();
    let mut fence_char = '\0';
    let mut fence_lang = String::new();
    let mut code = String::new();

    let flush_para = |blocks: &mut Vec<Block>, para: &mut Vec<&str>| {
        if !para.is_empty() {
            blocks.push(Block::Paragraph(para.join("\n")));
            para.clear();
        }
    };
    let flush_list = |blocks: &mut Vec<Block>, in_list: &mut bool, items: &mut Vec<String>, ordered: bool| {
        if *in_list {
            blocks.push(Block::List { ordered, items: std::mem::take(items) });
            *in_list = false;
        }
    };
    let flush_quote = |blocks: &mut Vec<Block>, in_quote: &mut bool, quote: &mut String| {
        if *in_quote {
            blocks.push(Block::Quote(quote.trim_end().to_string()));
            quote.clear();
            *in_quote = false;
        }
    };
    let flush_all = |blocks: &mut Vec<Block>,
                     para: &mut Vec<&str>,
                     in_list: &mut bool,
                     items: &mut Vec<String>,
                     ordered: bool,
                     in_quote: &mut bool,
                     quote: &mut String| {
        flush_para(blocks, para);
        flush_list(blocks, in_list, items, ordered);
        flush_quote(blocks, in_quote, quote);
    };

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end_matches('\r');
        if fence_char != '\0' {
            if line.trim_start().starts_with(fence_char) {
                blocks.push(Block::Code { text: std::mem::take(&mut code), lang: fence_lang.clone() });
                fence_char = '\0';
                fence_lang.clear();
            } else {
                code.push_str(line);
                code.push('\n');
            }
            i += 1;
            continue;
        }
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            flush_all(&mut blocks, &mut para, &mut in_list, &mut list_items, list_ordered, &mut in_quote, &mut quote);
            fence_char = trimmed.chars().next().unwrap();
            fence_lang = trimmed[3..].trim().split([' ', '\t']).next().unwrap_or("").to_lowercase();
            i += 1;
            continue;
        }
        if trimmed.is_empty() {
            flush_all(&mut blocks, &mut para, &mut in_list, &mut list_items, list_ordered, &mut in_quote, &mut quote);
            i += 1;
            continue;
        }
        if is_rule(trimmed) {
            flush_all(&mut blocks, &mut para, &mut in_list, &mut list_items, list_ordered, &mut in_quote, &mut quote);
            blocks.push(Block::Rule);
            i += 1;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('>') {
            flush_para(&mut blocks, &mut para);
            flush_list(&mut blocks, &mut in_list, &mut list_items, list_ordered);
            in_quote = true;
            if let Some(after_space) = rest.strip_prefix(' ') {
                quote.push_str(after_space);
            } else {
                quote.push_str(rest);
            }
            quote.push('\n');
            i += 1;
            continue;
        }
        // table: current row | separator on the next line
        if trimmed.starts_with('|') && i + 1 < lines.len() && table_separator(lines[i + 1].trim_end_matches('\r').trim()) {
            flush_all(&mut blocks, &mut para, &mut in_list, &mut list_items, list_ordered, &mut in_quote, &mut quote);
            let mut rows = vec![cells(trimmed)];
            i += 2; // header + separator
            while i < lines.len() {
                let row = lines[i].trim_end_matches('\r').trim();
                if !row.starts_with('|') {
                    break;
                }
                rows.push(cells(row));
                i += 1;
            }
            blocks.push(Block::Table(rows));
            continue;
        }
        // heading
        let level = trimmed.chars().take_while(|&c| c == '#').count();
        if (1..=6).contains(&level) && trimmed[level..].starts_with(' ') {
            flush_all(&mut blocks, &mut para, &mut in_list, &mut list_items, list_ordered, &mut in_quote, &mut quote);
            blocks.push(Block::Heading(level.min(3), trimmed[level + 1..].trim().to_string()));
            i += 1;
            continue;
        }
        // list item
        let ordered = ordered_item(trimmed);
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") || ordered.is_some() {
            flush_para(&mut blocks, &mut para);
            flush_quote(&mut blocks, &mut in_quote, &mut quote);
            let new_ordered = ordered.is_some();
            if in_list && new_ordered != list_ordered {
                flush_list(&mut blocks, &mut in_list, &mut list_items, list_ordered);
            }
            list_ordered = new_ordered;
            in_list = true;
            let content = if let Some((_, rest)) = ordered {
                rest.to_string()
            } else {
                trimmed[2..].trim().to_string()
            };
            list_items.push(content);
            i += 1;
            continue;
        }
        flush_list(&mut blocks, &mut in_list, &mut list_items, list_ordered);
        flush_quote(&mut blocks, &mut in_quote, &mut quote);
        para.push(trimmed);
        i += 1;
    }
    if fence_char != '\0' {
        blocks.push(Block::Code { text: code, lang: fence_lang });
    }
    flush_all(&mut blocks, &mut para, &mut in_list, &mut list_items, list_ordered, &mut in_quote, &mut quote);
    blocks
}

fn ordered_item(trimmed: &str) -> Option<(u32, &str)> {
    let digits: usize = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &trimmed[digits..];
    let after = rest.strip_prefix(['.', ')'])?;
    let content = after.strip_prefix([' ', '\t']).unwrap_or(after);
    if content.is_empty() && after.is_empty() {
        return None;
    }
    Some((trimmed[..digits].parse().ok()?, content))
}

// ---- inline model --------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum InlineStyle {
    Plain,
    Bold,
    Italic,
    Code,
    Link,
}

struct InlineResult {
    text: String,
    styles: Vec<(Range<usize>, InlineStyle)>,
    links: Vec<(Range<usize>, String)>,
}

/// Parses **bold**, *italic*, `code` and [label](url) — nesting up to depth 2,
/// exactly like the C# renderer's inline scanner.
fn parse_inline(text: &str, bold: bool) -> InlineResult {
    let mut result = InlineResult { text: String::new(), styles: Vec::new(), links: Vec::new() };
    if bold {
        result.styles.push((0..0, InlineStyle::Bold)); // ensure the vec starts styled-aware; ranges fixed below
        result.styles.clear();
    }
    let chars: Vec<char> = text.chars().collect();
    // byte offset of each char for correct Range byte indices
    let mut byte_of: Vec<usize> = Vec::with_capacity(chars.len() + 1);
    let mut acc = 0;
    for c in &chars {
        byte_of.push(acc);
        acc += c.len_utf8();
    }
    byte_of.push(acc);

    let mut push = |out: &mut InlineResult, s: &str, style: InlineStyle| {
        let start = out.text.len();
        out.text.push_str(s);
        out.styles.push((start..out.text.len(), style));
    };

    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '`' {
            if let Some(end) = find_char(&chars, i + 1, '`') {
                let code_text: String = chars[i + 1..end].iter().collect();
                push(&mut result, &code_text, InlineStyle::Code);
                i = end + 1;
                continue;
            }
        } else if ch == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_substring(&chars, i + 2, &['*', '*']) {
                let inner: String = chars[i + 2..end].iter().collect();
                merge_inline(&mut result, &inner, true, bold || true);
                i = end + 2;
                continue;
            }
        } else if ch == '*'
            && (i == 0 || chars[i - 1] != '`')
            && i + 1 < chars.len()
            && !chars[i + 1].is_whitespace()
        {
            if let Some(end) = find_char(&chars, i + 1, '*') {
                let inner: String = chars[i + 1..end].iter().collect();
                merge_inline(&mut result, &inner, bold, true);
                i = end + 1;
                continue;
            }
        } else if ch == '[' {
            if let Some((label, url, total_len)) = parse_link(&chars, i) {
                let start = result.text.len();
                result.text.push_str(&label);
                result.links.push((start..result.text.len(), url));
                result.styles.push((start..result.text.len(), if bold { InlineStyle::Bold } else { InlineStyle::Link }));
                i += total_len;
                continue;
            }
        }
        let next = if ch == '*' { i + 1 } else { i + 1 };
        let slice_end = byte_of.get(next).copied().unwrap_or(text.len());
        let slice_start = byte_of[i];
        push(&mut result, &text[slice_start..slice_end.min(text.len())], if bold { InlineStyle::Bold } else { InlineStyle::Plain });
        i = next;
    }
    result
}

/// Appends another inline-parsed chunk, mapping its styles into `out` and forcing
/// outer bold/italic state.
fn merge_inline(out: &mut InlineResult, inner: &str, bold: bool, italic: bool) {
    let nested = parse_inline(inner, bold);
    let offset = out.text.len();
    let forced = if bold {
        InlineStyle::Bold
    } else if italic {
        InlineStyle::Italic
    } else {
        InlineStyle::Plain
    };
    out.text.push_str(&nested.text);
    for (range, style) in nested.styles {
        let style = if bold { InlineStyle::Bold } else { style.min(forced) };
        out.styles.push((range.start + offset..range.end + offset, style));
    }
    for (range, url) in nested.links {
        out.links.push((range.start + offset..range.end + offset, url));
    }
}

// ordering for .min above: Plain < Italic < Bold < Code < Link is wrong — Plain is weakest.
// InlineStyle::min is only used with bold=true where everything collapses to Bold, so
// implement a strength order that keeps Code/Link intact.
impl InlineStyle {
    fn min(self, other: Self) -> Self {
        use InlineStyle::*;
        let rank = |s: Self| match s {
            Plain => 0,
            Italic => 1,
            Bold => 2,
            Code => 3,
            Link => 4,
        };
        if rank(self) <= rank(other) {
            self
        } else {
            other
        }
    }
}

fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == needle)
}

fn find_substring(chars: &[char], from: usize, needle: &[char]) -> Option<usize> {
    (from..chars.len().saturating_sub(1)).find(|&i| chars[i] == needle[0] && chars[i + 1] == needle[1])
}

/// [label](url) starting at `start` == '['. Returns (label, url, total char length).
fn parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let mut i = start + 1;
    let mut depth = 1;
    while i < chars.len() && depth > 0 {
        match chars[i] {
            '[' => depth += 1,
            ']' => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            break;
        }
        i += 1;
    }
    if i >= chars.len() || depth != 0 {
        return None;
    }
    let label: String = chars[start + 1..i].iter().collect();
    if label.is_empty() || chars.get(i + 1) != Some(&'(') {
        return None;
    }
    let mut j = i + 2;
    while j < chars.len() && chars[j] != ')' && !chars[j].is_whitespace() {
        j += 1;
    }
    if j >= chars.len() || chars[j] != ')' {
        return None;
    }
    let url: String = chars[i + 2..j].iter().collect();
    if url.is_empty() {
        return None;
    }
    Some((label, url, j + 1 - start))
}

// ---- rendering -----------------------------------------------------------

fn text_style(base: &TextStyle, size: Pixels, family: &'static str, weight: FontWeight, style: FontStyle) -> TextStyle {
    let mut ts = base.clone();
    ts.font_size = AbsoluteLength::Pixels(size);
    ts.font_family = family.into();
    ts.font_weight = weight;
    ts.font_style = style;
    ts
}

fn inline_highlights(
    inline: &InlineResult,
    base_color: Hsla,
    muted_color: Hsla,
    accent_color: Hsla,
    chip_bg: Hsla,
) -> (Vec<(Range<usize>, HighlightStyle)>, Vec<Range<usize>>, Vec<String>) {
    let mut highlights = Vec::new();
    for (range, style) in &inline.styles {
        let mut hl = match style {
            InlineStyle::Plain => HighlightStyle { color: Some(base_color), ..Default::default() },
            InlineStyle::Bold => HighlightStyle {
                color: Some(base_color),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
            InlineStyle::Italic => HighlightStyle {
                color: Some(base_color),
                font_style: Some(FontStyle::Italic),
                ..Default::default()
            },
            InlineStyle::Code => HighlightStyle {
                color: Some(theme::CODE_FG.into()),
                background_color: Some(chip_bg),
                ..Default::default()
            },
            InlineStyle::Link => HighlightStyle {
                color: Some(accent_color),
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(accent_color),
                    wavy: false,
                }),
                ..Default::default()
            },
        };
        if hl.color.is_none() {
            hl.color = Some(muted_color);
        }
        highlights.push((range.clone(), hl));
    }
    let link_ranges: Vec<Range<usize>> = inline.links.iter().map(|(r, _)| r.clone()).collect();
    let link_urls: Vec<String> = inline.links.iter().map(|(_, u)| u.clone()).collect();
    (highlights, link_ranges, link_urls)
}

fn styled_paragraph(
    text: &str,
    bold: bool,
    base: &TextStyle,
    size: Pixels,
    weight: FontWeight,
    fonts: &theme::FontTokens,
    accent: Hsla,
    cx_colors: &MarkdownColors,
) -> AnyElement {
    let inline = parse_inline(text, bold);
    let (highlights, link_ranges, link_urls) =
        inline_highlights(&inline, theme::TEXT.into(), theme::TEXT_MUTED.into(), accent, cx_colors.chip_bg);
    let style = text_style(base, size, theme::FONT_UI, weight, FontStyle::Normal);
    let styled = StyledText::new(inline.text.clone()).with_default_highlights(&style, highlights);
    if link_ranges.is_empty() {
        div().w_full().child(styled).into_any_element()
    } else {
        div()
            .w_full()
            .child(
                InteractiveText::new(ElementId::Name("md-links".into()), styled).on_click(
                    link_ranges,
                    move |ix, _, cx| {
                        if let Some(url) = link_urls.get(ix) {
                            if url.starts_with("http://") || url.starts_with("https://") {
                                cx.open_url(url);
                            }
                        }
                    },
                ),
            )
            .into_any_element()
    }
}

/// Colors captured once per render (accent-dependent).
pub struct MarkdownColors {
    pub chip_bg: Hsla,
}

/// Renders a full markdown document; `id_base` keeps element ids unique per chat item.
pub fn render_markdown(
    text: &str,
    fonts: theme::FontTokens,
    accent_color: gpui::Rgba,
    window: &mut Window,
    id_base: usize,
) -> AnyElement {
    let base = window.text_style();
    let fonts = fonts;
    let accent: Hsla = accent_color.into();
    let colors = MarkdownColors { chip_bg: theme::CHIP.into() };
    let mono = theme::CODE_FONT;

    let mut column = div().flex().flex_col().gap_1().w_full();
    for block in parse_blocks(text) {
        let element: AnyElement = match block {
            Block::Paragraph(p) => styled_paragraph(
                &p,
                false,
                &base,
                px(fonts.font_body_large),
                FontWeight::NORMAL,
                &fonts,
                accent,
                &colors,
            ),
            Block::Heading(level, h) => {
                let size = match level {
                    1 => px(fonts.font_h1),
                    2 => px(fonts.font_h2),
                    _ => px(fonts.font_h3),
                };
                div()
                    .pt_1()
                    .child(styled_paragraph(&h, true, &base, size, FontWeight::BOLD, &fonts, accent, &colors))
                    .into_any_element()
            }
            Block::Code { text: code, lang } => {
                let code_text = SharedString::from(code.clone());
                let copy_text = code.clone();
                let tokens = highlight(&code, &lang);
                let mut combined = String::new();
                let mut highlights = Vec::new();
                for (chunk, kind) in tokens {
                    let start = combined.len();
                    combined.push_str(&chunk);
                    let color = match kind {
                        TokenKind::Plain => theme::CODE_FG,
                        TokenKind::Keyword => theme::SYN_KEYWORD,
                        TokenKind::Str => theme::SYN_STRING,
                        TokenKind::Comment => theme::SYN_COMMENT,
                        TokenKind::Num => theme::SYN_NUMBER,
                    };
                    if kind != TokenKind::Plain {
                        highlights.push((
                            start..combined.len(),
                            HighlightStyle { color: Some(color.into()), ..Default::default() },
                        ));
                    }
                }
                let style = text_style(&base, px(fonts.font_body), mono, FontWeight::NORMAL, FontStyle::Normal);
                let body = StyledText::new(combined).with_default_highlights(&style, highlights);
                let header_label = if lang.is_empty() { "code".to_string() } else { lang.clone() };
                div()
                    .rounded_lg()
                    .overflow_hidden()
                    .bg(theme::CODE_BLOCK)
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px_2()
                            .py_1()
                            .bg(theme::CHIP)
                            .child(
                                div()
                                    .text_size(px(fonts.font_micro))
                                    .font_family(mono)
                                    .text_color(theme::TEXT_MUTED)
                                    .child(header_label),
                            )
                            .child(
                                div()
                                    .id(ElementId::NamedInteger("copy-code".into(), id_base as u64))
                                    .cursor_pointer()
                                    .p_1()
                                    .rounded_sm()
                                    .hover(|s| s.bg(theme::HOVER))
                                    .text_color(theme::TEXT_MUTED)
                                    .hover(|s| s.text_color(theme::TEXT))
                                    .child(crate::ui::icon("icons/copy.svg", px(fonts.icon_small), theme::TEXT_MUTED))
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_text.clone()));
                                    }),
                            ),
                    )
                    .child(div().px_2().py_2().child(body))
                    .into_any_element()
            }
            Block::List { ordered, items } => {
                let mut list = div().flex().flex_col().gap_1().pl_4().w_full();
                for (i, item) in items.into_iter().enumerate() {
                    let marker = if ordered { format!("{}.  ", i + 1) } else { "•  ".to_string() };
                    list = list.child(
                        div()
                            .w_full()
                            .flex()
                            .flex_row()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(theme::TEXT)
                                    .child(marker),
                            )
                            .child(styled_paragraph(
                                &item,
                                false,
                                &base,
                                px(fonts.font_body_large),
                                FontWeight::NORMAL,
                                &fonts,
                                accent,
                                &colors,
                            )),
                    );
                }
                list.into_any_element()
            }
            Block::Quote(q) => div()
                .border_l_2()
                .border_color(accent)
                .pl_2()
                .py_1()
                .opacity(0.75)
                .w_full()
                .child(styled_paragraph(&q, false, &base, px(fonts.font_body_large), FontWeight::NORMAL, &fonts, accent, &colors))
                .into_any_element(),
            Block::Table(rows) => {
                let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
                let mut grid = div().flex().flex_col().gap_1().w_full();
                for (ri, row) in rows.into_iter().enumerate() {
                    let mut line = div().flex().flex_row().gap_4().w_full();
                    for ci in 0..cols {
                        let cell = row.get(ci).cloned().unwrap_or_default();
                        let is_header = ri == 0;
                        line = line.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(styled_paragraph(
                                    &cell,
                                    is_header,
                                    &base,
                                    px(fonts.font_body_large),
                                    if is_header { FontWeight::BOLD } else { FontWeight::NORMAL },
                                    &fonts,
                                    accent,
                                    &colors,
                                )),
                        );
                    }
                    grid = grid.child(line);
                    if ri == 0 {
                        grid = grid.child(div().h_px().w_full().bg(theme::HAIRLINE));
                    }
                }
                grid.into_any_element()
            }
            Block::Rule => div().h_px().w_full().my_1().bg(theme::HAIRLINE).opacity(0.7).into_any_element(),
        };
        column = column.child(element);
    }
    column.into_any_element()
}

pub const FONT_UI: &str = "Inter";
const _: Option<FontFeatures> = None;
const _: Option<Font> = None;
