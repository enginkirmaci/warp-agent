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
    static BUFFER_CACHE: RefCell<HashMap<(Arc<HighlightTheme>, String, u64), Arc<Vec<(Range<usize>, HighlightStyle)>>>> = RefCell::new(HashMap::new());
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
        (*highlight_buffer(&code, &lang, &theme)).clone()
    }
}

/// Token styles for a plain code buffer — the entry point for the renderers
/// that draw their own gutters (ui/diff.rs). Cached by (language, text): a
/// re-render of an unchanged block — every repaint of an open fold while its
/// tile streams — skips both the rope rebuild and the tree-sitter style pass.
pub fn highlight_buffer(
    code: &str,
    lang: &str,
    theme: &Arc<HighlightTheme>,
) -> Arc<Vec<(Range<usize>, HighlightStyle)>> {
    // The theme Arc sits IN the key: accent swaps replace the global theme,
    // and holding the old Arc alive keeps its address from being reused, so
    // cached styles can never outlive their palette. 64 blocks cover every
    // open fold with slack; a full clear on overflow is fine — entries only
    // get expensive to rebuild, never stale.
    const CAP: usize = 64;
    let key = (theme.clone(), lang.to_string(), hash_text(code));
    if let Some(hit) = BUFFER_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }
    let styles = HIGHLIGHTERS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let highlighter = cache
            .entry(lang.to_string().into())
            .or_insert_with(|| SyntaxHighlighter::new(lang));
        let rope = Rope::from_str(code);
        highlighter.update(None, &rope, None);
        Arc::new(highlighter.styles(&(0..code.len()), theme.as_ref()))
    });
    BUFFER_CACHE.with(|c| {
        let mut cache = c.borrow_mut();
        if cache.len() >= CAP {
            cache.clear();
        }
        cache.insert(key, styles.clone());
    });
    styles
}

fn hash_text(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
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
