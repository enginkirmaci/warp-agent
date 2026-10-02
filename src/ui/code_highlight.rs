//! Code-block highlighting for chat markdown: unified diffs get the ui/diff.rs
//! palette (green/red line washes, gray hunk bands, muted metadata), every other
//! language goes through the same tree-sitter pass the kit installs — re-hosted
//! here on the public SyntaxHighlighter API because the component's own
//! highlighter is pub(crate) and its TextView facade doesn't forward the
//! per-instance override hook.
//!
//! The returned Arc must stay pointer-stable across frames: a code block
//! reuses its computed highlights only while the highlighter Arc is the same
//! (gpui-base node.rs), so a fresh closure per render would re-highlight every
//! block every frame. The cache is keyed by the theme Arc, which only changes
//! when the accent preset swaps.

use std::{cell::RefCell, collections::HashMap, ops::Range, sync::Arc};

use gpui::{HighlightStyle, SharedString};
use gpui_kit::base::input::Rope;
use gpui_kit::base::text::CodeBlock;
use gpui_kit::component::highlighter::{HighlightTheme, SyntaxHighlighter};

/// The highlighter signature gpui-base TextView expects.
pub type CodeBlockHighlighter = dyn Fn(&CodeBlock) -> Vec<(Range<usize>, HighlightStyle)> + Send + Sync;

/// The shared highlighter for the live component theme. Call per render; the
/// returned Arc only changes when the theme itself does.
pub fn shared_highlighter(cx: &gpui::App) -> Arc<CodeBlockHighlighter> {
    let theme = gpui_kit::component::Theme::global(cx).highlight_theme.clone();
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((cached_theme, shared)) = cache.as_ref()
            && Arc::ptr_eq(cached_theme, &theme)
        {
            return shared.clone();
        }
        let shared: Arc<CodeBlockHighlighter> = Arc::new(build(theme.clone()));
        *cache = Some((theme, shared.clone()));
        shared
    })
}

thread_local! {
    static CACHE: RefCell<Option<(Arc<HighlightTheme>, Arc<CodeBlockHighlighter>)>> = const { RefCell::new(None) };
    static HIGHLIGHTERS: RefCell<HashMap<SharedString, SyntaxHighlighter>> = RefCell::new(HashMap::new());
}

fn build(
    theme: Arc<HighlightTheme>,
) -> impl Fn(&CodeBlock) -> Vec<(Range<usize>, HighlightStyle)> + Send + Sync {
    move |block| {
        let code = block.code();
        let lang = block.lang().map(|l| l.to_lowercase());
        if matches!(lang.as_deref(), Some("diff" | "patch" | "udiff")) {
            return diff_styles(&code);
        }
        // Untagged fences that are really diffs: a hunk header is unambiguous,
        // and header + both edit kinds beats a prose bullet list.
        if lang.is_none() && looks_like_diff(&code) {
            return diff_styles(&code);
        }
        let Some(lang) = lang else {
            return Vec::new();
        };
        HIGHLIGHTERS.with(|cache| {
            let mut cache = cache.borrow_mut();
            let highlighter = cache
                .entry(lang.clone().into())
                .or_insert_with(|| SyntaxHighlighter::new(&lang));
            let rope = Rope::from_str(&code);
            highlighter.update(None, &rope, None);
            highlighter.styles(&(0..code.len()), theme.as_ref())
        })
    }
}

fn looks_like_diff(code: &str) -> bool {
    let mut hunk = false;
    let mut header = false;
    let mut add = 0;
    let mut del = 0;
    for line in code.lines() {
        if line.starts_with("@@ -") {
            hunk = true;
        }
        if line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("diff --git") {
            header = true;
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            add += 1;
        }
        if line.starts_with('-') && !line.starts_with("---") {
            del += 1;
        }
    }
    hunk || (header && add >= 2 && del >= 2)
}

/// Per-line ranges mirroring ui/diff.rs, minus the gutter the kit pipeline
/// can't inject: ADD/DEL foreground + background wash, gray hunk band, muted
/// metadata. Meta prefixes must win over +/-: "--- x"/"+++ x" also strip as
/// deletions/additions.
fn diff_styles(code: &str) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut out = Vec::new();
    let mut offset = 0;
    for line in code.split('\n') {
        let start = offset;
        offset += line.len() + 1;
        let style = if line.starts_with("@@") {
            Some(HighlightStyle {
                color: Some(crate::theme::TEXT_MUTED.into()),
                background_color: Some(crate::theme::DIFF_HUNK_BG),
                ..Default::default()
            })
        } else if is_meta(line) {
            Some(HighlightStyle {
                color: Some(crate::theme::TEXT_MUTED.into()),
                ..Default::default()
            })
        } else if line.starts_with('+') {
            Some(HighlightStyle {
                color: Some(crate::theme::ADD.into()),
                background_color: Some(crate::theme::DIFF_ADD_BG),
                ..Default::default()
            })
        } else if line.starts_with('-') {
            Some(HighlightStyle {
                color: Some(crate::theme::DEL.into()),
                background_color: Some(crate::theme::DIFF_DEL_BG),
                ..Default::default()
            })
        } else {
            None
        };
        if let (Some(style), false) = (style, line.is_empty()) {
            out.push((start..start + line.len(), style));
        }
    }
    out
}

fn is_meta(line: &str) -> bool {
    ["diff --git", "index ", "--- ", "+++ ", "new file", "deleted file", "old mode", "new mode", "similarity", "rename ", "Binary files", "GIT binary patch", "\\"]
        .iter()
        .any(|p| line.starts_with(p))
}
