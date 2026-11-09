use super::*;
use warpui_kit_diff::{DiffDocument, HunkText, patch_from_hunks};

// ---- JSON shape parsing (ChatParsing port) -------------------------------

/// Context limit for a model key: exact `provider/model` match first, then a
/// model-name-only match — message info (`providerID`) and the provider list
/// can disagree on the provider segment (custom providers, forks). 0 when the
/// model is unknown to every running server.
pub(super) fn lookup_context_limit(limits: &HashMap<String, i64>, key: &str) -> Option<i64> {
    if let Some(limit) = limits.get(key) {
        return if *limit > 0 { Some(*limit) } else { None };
    }
    let name = key.rsplit('/').next()?;
    limits
        .iter()
        .filter(|(k, _)| k.rsplit('/').next() == Some(name))
        .map(|(_, limit)| *limit)
        .find(|limit| *limit > 0)
}

pub fn read_token_total(info: &serde_json::Value) -> i64 {
    let Some(tokens) = info.get("tokens").and_then(serde_json::Value::as_object) else {
        return 0;
    };
    let sum = |name: &str| {
        tokens
            .get(name)
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
    };
    let (mut cache_read, mut cache_write) = (0, 0);
    if let Some(cache) = tokens.get("cache").and_then(serde_json::Value::as_object) {
        cache_read = cache
            .get("read")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        cache_write = cache
            .get("write")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
    }
    sum("input") + sum("output") + sum("reasoning") + cache_read + cache_write
}

/// Compact per-message detail line: token flow, cost.
pub fn build_meta(info: &serde_json::Value) -> Option<String> {
    if !info.is_object() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(tokens) = info.get("tokens").and_then(serde_json::Value::as_object) {
        let input = tokens
            .get("input")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        let output = tokens
            .get("output")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0);
        if input > 0 || output > 0 {
            parts.push(format!(
                "↑{} ↓{}",
                format_tokens(input),
                format_tokens(output)
            ));
        }
    }
    let cost = info
        .get("cost")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(0.0);
    if cost > 0.0 {
        parts.push(
            format!("${:.4}", cost)
                .trim_end_matches('0')
                .trim_end_matches('.')
                .to_string(),
        );
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn describe_part(state: &serde_json::Value, name: &str) -> String {
    let Some(value) = state.get(name) else {
        return String::new();
    };
    if value.is_null() {
        return String::new();
    }
    let text = if let Some(s) = value.as_str() {
        // string payloads that are themselves JSON (the server sometimes
        // ships input encoded) pretty-print like structured ones
        let trimmed = s.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            serde_json::from_str::<serde_json::Value>(s)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .unwrap_or_else(|| s.to_string())
        } else {
            s.to_string()
        }
    } else {
        serde_json::to_string_pretty(value).unwrap_or_default()
    };
    if text.is_empty() {
        String::new()
    } else if text.len() <= 4000 {
        text
    } else {
        // cut on a char boundary — byte 4000 can split a multibyte char
        let mut cut = 4000;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}\n… (truncated)", &text[..cut])
    }
}

/// Short label for a tool call for the stretch before the server sends a
/// state.title: the first meaningful value from state.input (command, file
/// path, pattern, …) so a pending row says more than the tool name.
fn tool_label_from_input(state: &serde_json::Value) -> Option<String> {
    let input = state.get("input").and_then(serde_json::Value::as_object)?;
    const CANDIDATES: [&str; 12] = [
        "command",
        "filePath",
        "file_path",
        "path",
        "pattern",
        "query",
        "url",
        "description",
        "prompt",
        "args",
        "skill",
        "name",
    ];
    let mut pick: Option<String> = None;
    for key in CANDIDATES {
        let Some(value) = input.get(key) else {
            continue;
        };
        let text = match value {
            serde_json::Value::String(s) => Some(s.trim().to_string()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Array(a) => Some(
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .to_string(),
            ),
            _ => None,
        };
        if text.as_deref().is_some_and(|t| !t.is_empty()) {
            pick = text;
            break;
        }
    }
    let pick = pick.or_else(|| {
        input
            .values()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            // a body with newlines (an apply_patch payload, a file's content)
            // is not a label: keep the tool's own name instead
            .find(|s| !s.is_empty() && !s.contains('\n'))
            .map(String::from)
    })?;
    let label = pick.split_whitespace().collect::<Vec<_>>().join(" ");
    if label.is_empty() {
        return None;
    }
    if label.len() <= 120 {
        return Some(label);
    }
    let mut cut = 120;
    while cut > 0 && !label.is_char_boundary(cut) {
        cut -= 1;
    }
    Some(format!("{}…", &label[..cut]))
}

pub(super) fn apply_tool_state(state: &serde_json::Value, tool: &mut ToolItem) {
    if let Some(status) = state.get("status").and_then(serde_json::Value::as_str) {
        tool.status = status.into();
        tool.is_error = status.eq_ignore_ascii_case("error");
    }
    if let Some(title) = state.get("title").and_then(serde_json::Value::as_str) {
        if !title.is_empty() {
            tool.title = title.into();
        }
    } else if let Some(label) = tool_label_from_input(state) {
        tool.title = label;
    }
    tool.details = ToolDetails {
        input: describe_part(state, "input"),
        output: describe_part(state, "output"),
        error: describe_part(state, "error"),
    };
    tool.error_preview = tool
        .details
        .error
        .split('\n')
        .next()
        .unwrap_or("")
        .to_string();
}

pub(super) fn apply_tool_metadata(state: &serde_json::Value, tool: &mut ToolItem) {
    let Some(md) = state.get("metadata") else {
        return;
    };
    if let Some(file_path) = md.get("filepath").and_then(serde_json::Value::as_str)
        && !file_path.is_empty()
    {
        tool.file_path = file_path.into();
    }
    if let Some(fd) = md.get("filediff").and_then(serde_json::Value::as_object) {
        tool.diff = fd
            .get("patch")
            .and_then(serde_json::Value::as_str)
            .map(String::from);
        tool.numstat = Some(Numstat {
            additions: fd
                .get("additions")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0),
            deletions: fd
                .get("deletions")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0),
        });
    }
    if tool.diff.is_none() {
        apply_applied_hunks(state, md, tool);
    }
}

/// dsh reports a file change as the before/after text of each applied hunk
/// (`meta.diffs`) instead of opencode's `metadata.filediff.patch`. Turn the
/// hunks into a patch so both harnesses land in `ToolItem::diff` and the tile
/// draws the same kit diff for either. A created file's metadata carries no
/// hunks (there is no before-image), so its whole content comes from the call's
/// own arguments.
fn apply_applied_hunks(state: &serde_json::Value, md: &serde_json::Value, tool: &mut ToolItem) {
    let mut hunks: Vec<HunkText<'_>> = Vec::new();
    let entries = md
        .get("diffs")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for entry in entries {
        let Some(path) = entry.get("path").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let new = entry
            .get("newText")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        // null is a pure insertion (or a created file): there is no old text
        let old = entry.get("oldText").and_then(serde_json::Value::as_str);
        hunks.push(HunkText { path, old, new });
    }
    if hunks.is_empty() {
        if md.get("operation").and_then(serde_json::Value::as_str) != Some("create") {
            return;
        }
        let Some(path) = md.get("path").and_then(serde_json::Value::as_str) else {
            return;
        };
        let Some(content) = state
            .pointer("/input/content")
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        if content.is_empty() {
            return;
        }
        hunks.push(HunkText {
            path,
            old: None,
            new: content,
        });
    }
    if let Some(path) = hunks.first().map(|hunk| hunk.path) {
        tool.file_path = path.to_string();
    }
    let patch = patch_from_hunks(&hunks);
    let doc = DiffDocument::parse(&patch);
    tool.numstat = Some(Numstat {
        additions: i64::from(doc.added()),
        deletions: i64::from(doc.removed()),
    });
    tool.diff = Some(patch);
}

/// Last todowrite tool part in the restored transcript carries the session's todos.
pub(super) fn todos_from_part(part: &serde_json::Value) -> Option<Vec<TaskItem>> {
    let state = part.get("state")?;
    let list = state
        .pointer("/input/todos")
        .filter(|v| v.is_array())
        .or_else(|| state.pointer("/metadata/todos").filter(|v| v.is_array()))?;
    let mut result = Vec::new();
    for todo in list.as_array()? {
        let Some(content) = todo.get("content").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if content.is_empty() {
            continue;
        }
        let status = todo
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("pending");
        result.push(TaskItem {
            content: content.into(),
            status: status.into(),
        });
    }
    Some(result)
}

pub(super) fn decode_data_url(url: &str) -> Option<Vec<u8>> {
    let rest = url.strip_prefix("data:image")?;
    let comma = rest.find(',')?;
    let base64_part = &rest[comma + 1..];
    decode_base64(base64_part)
}

pub fn decode_base64(input: &str) -> Option<Vec<u8>> {
    const INVALID: u8 = 255;
    fn table() -> [u8; 256] {
        let mut t = [INVALID; 256];
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        for (i, &c) in alphabet.iter().enumerate() {
            t[c as usize] = i as u8;
        }
        t
    }
    let table = table();
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0;
    for &c in input.as_bytes() {
        if c == b'=' || c == b'\n' || c == b'\r' {
            continue;
        }
        let v = table[c as usize];
        if v == INVALID {
            return None;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

pub fn derive_title(text: &str) -> String {
    let mut first_line = text.split('\n').next().unwrap_or("").trim().to_string();
    for extra in ["\r", "\t"] {
        first_line = first_line.replace(extra, " ");
    }
    while first_line.contains("  ") {
        first_line = first_line.replace("  ", " ");
    }
    if first_line.chars().count() <= 48 {
        first_line
    } else {
        let cut: String = first_line.chars().take(48).collect();
        format!("{}…", cut.trim_end())
    }
}

/// The fork seed: the transcript up to and including the chosen user message
/// (`stop_key`) flattened to a plain-text block for the new session's
/// composer. User lines keep their text, assistant replies get a role tag;
/// reasoning/tools stay out — the block is context for a manual send, not a
/// replay.
pub(super) fn fork_transcript(tile: &ChatTile, stop_key: usize, source_title: &str) -> String {
    let mut out = format!("— forked from {source_title} —\n");
    for item in &tile.items {
        match item.as_ref() {
            ChatItem::UserMessage { key, text, .. } => {
                out.push_str("\nYou: ");
                out.push_str(text);
                if *key == stop_key {
                    return out;
                }
            }
            ChatItem::AssistantText { text, .. } => {
                out.push_str("\nAssistant: ");
                out.push_str(text);
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_after(state: &serde_json::Value) -> ToolItem {
        let mut tool = ToolItem::stub("t1");
        apply_tool_state(state, &mut tool);
        apply_tool_metadata(state, &mut tool);
        tool
    }

    #[test]
    fn an_opencode_filediff_fills_the_diff_and_counts() {
        let tool = tool_after(&json!({
            "status": "completed",
            "metadata": {
                "filepath": "src/main.rs",
                "filediff": {
                    "patch": "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n",
                    "additions": 1,
                    "deletions": 1,
                },
            },
        }));
        assert!(tool.has_diff());
        assert_eq!(tool.file_path, "src/main.rs");
        let stats = tool.numstat.expect("numstat");
        assert_eq!((stats.additions, stats.deletions), (1, 1));
    }

    #[test]
    fn a_dsh_edit_meta_becomes_a_patch() {
        // the shape dsh's `edit` result carries: applied hunks as text pairs
        let tool = tool_after(&json!({
            "status": "completed",
            "input": { "file_path": "src/lib.rs", "old_string": "let a = 1;", "new_string": "let a = 2;" },
            "metadata": {
                "path": "/repo/src/lib.rs",
                "diffs": [{
                    "path": "/repo/src/lib.rs",
                    "oldText": "fn main() {\n    let a = 1;\n}\n",
                    "newText": "fn main() {\n    let a = 2;\n}\n",
                }],
            },
        }));
        assert!(tool.has_diff(), "a dsh edit must reach the diff fold");
        assert_eq!(tool.file_path, "/repo/src/lib.rs");
        let stats = tool.numstat.expect("numstat");
        assert_eq!((stats.additions, stats.deletions), (1, 1));
        let diff = tool.diff.expect("patch");
        assert!(diff.contains("-    let a = 1;"), "{diff}");
        assert!(diff.contains("+    let a = 2;"), "{diff}");
        assert!(diff.contains(" fn main() {"), "{diff}");
    }

    #[test]
    fn a_dsh_created_file_is_the_call_content() {
        let tool = tool_after(&json!({
            "status": "completed",
            "input": { "file_path": "notes/new.md", "content": "# hi\nbody\n" },
            "metadata": { "operation": "create", "path": "/repo/notes/new.md", "diffs": [] },
        }));
        assert!(tool.has_diff());
        assert_eq!(tool.file_path, "/repo/notes/new.md");
        let stats = tool.numstat.expect("numstat");
        assert_eq!((stats.additions, stats.deletions), (2, 0));
    }

    #[test]
    fn a_dsh_read_meta_does_not_invent_a_diff() {
        let tool = tool_after(&json!({
            "status": "completed",
            "metadata": {
                "path": "/repo/src/main.rs",
                "offset": 10,
                "lines": [{ "number": 10, "text": "fn main() {}" }],
                "totalLines": 20,
                "lang": "rust",
            },
        }));
        assert!(!tool.has_diff());
    }

    #[test]
    fn a_tool_row_titles_itself_from_its_input() {
        let mut tool = ToolItem::stub("t1");
        tool.tool = "bash".into();
        tool.title = "bash".into();
        apply_tool_state(
            &json!({ "status": "running", "input": { "command": "cargo test -p warp-agent" } }),
            &mut tool,
        );
        assert_eq!(tool.title, "cargo test -p warp-agent");
    }

    #[test]
    fn a_patch_body_is_not_a_row_title() {
        let mut tool = ToolItem::stub("t1");
        tool.tool = "apply_patch".into();
        tool.title = "apply_patch".into();
        apply_tool_state(
            &json!({
                "status": "running",
                "input": { "patch": "*** Begin Patch\n*** Update File: a.rs\n*** End Patch" },
            }),
            &mut tool,
        );
        assert_eq!(tool.title, "apply_patch");
    }
}
