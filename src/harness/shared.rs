//! The small state every adapter shares: pending approval/question maps and
//! the wire-to-typed catalog/filediff conversions.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use super::types::{
    Command, FileDiff, FormAnswer, FormRequest, HarnessEvent, PermissionRequest, SessionId, Skill,
};
use crate::oc::CommandInfo;
use warpui_kit_diff::{HunkText, patch_from_hunks};

/// The approvals and questions a session is waiting on, fed by the adapter's
/// typed event stream.
#[derive(Default)]
pub(crate) struct PendingApprovals {
    permissions: Mutex<HashMap<String, PermissionRequest>>,
    forms: Mutex<HashMap<String, FormRequest>>,
}

impl PendingApprovals {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn observe(&self, event: &HarnessEvent) {
        match event {
            HarnessEvent::PermissionAsked { request } => {
                self.permissions
                    .lock()
                    .expect("pending permissions")
                    .insert(request.id.clone(), request.clone());
            }
            HarnessEvent::PermissionReplied { request, .. } => {
                self.permissions
                    .lock()
                    .expect("pending permissions")
                    .remove(request);
            }
            HarnessEvent::QuestionAsked { request } => {
                self.forms
                    .lock()
                    .expect("pending forms")
                    .insert(request.id.clone(), request.clone());
            }
            HarnessEvent::QuestionResolved { request, .. } => {
                self.forms.lock().expect("pending forms").remove(request);
            }
            _ => {}
        }
    }

    pub(crate) fn permissions_for(&self, session: &SessionId) -> Vec<PermissionRequest> {
        let mut pending: Vec<PermissionRequest> = self
            .permissions
            .lock()
            .expect("pending permissions")
            .values()
            .filter(|request| &request.session == session)
            .cloned()
            .collect();
        pending.sort_by(|left, right| left.id.cmp(&right.id));
        pending
    }

    pub(crate) fn forms_for(&self, session: &SessionId) -> Vec<FormRequest> {
        let mut pending: Vec<FormRequest> = self
            .forms
            .lock()
            .expect("pending forms")
            .values()
            .filter(|form| &form.session == session)
            .cloned()
            .collect();
        pending.sort_by(|left, right| left.id.cmp(&right.id));
        pending
    }

    pub(crate) fn remove_permission(&self, request: &str) {
        self.permissions
            .lock()
            .expect("pending permissions")
            .remove(request);
    }

    pub(crate) fn form(&self, id: &str) -> Option<FormRequest> {
        self.forms.lock().expect("pending forms").get(id).cloned()
    }

    pub(crate) fn remove_form(&self, id: &str) {
        self.forms.lock().expect("pending forms").remove(id);
    }
}

/// The opencode `Vec<Vec<String>>` answer shape from a typed form answer.
pub(crate) fn form_answers(form: &FormRequest, answer: FormAnswer) -> Vec<Vec<String>> {
    let mut answers = Vec::with_capacity(form.fields.len());
    for field in &form.fields {
        let values = match answer.values.get(&field.id) {
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect(),
            Some(serde_json::Value::String(value)) => vec![value.clone()],
            Some(serde_json::Value::Bool(value)) => vec![value.to_string()],
            Some(serde_json::Value::Number(value)) => vec![value.to_string()],
            Some(other) => vec![other.to_string()],
            None => Vec::new(),
        };
        answers.push(values);
    }
    answers
}

/// Split a harness's command catalog into typed commands and skills.
pub(crate) fn partition_commands(commands: Vec<CommandInfo>) -> (Vec<Command>, Vec<Skill>) {
    let mut typed_commands = Vec::new();
    let mut skills = Vec::new();
    for command in commands {
        if command.source == "skill" {
            skills.push(Skill {
                id: command.name.clone(),
                name: command.name,
                description: command.description,
            });
        } else {
            typed_commands.push(Command {
                name: command.name,
                description: command.description,
            });
        }
    }
    (typed_commands, skills)
}

/// The filediff patches in a stored history, keyed by path.
pub(crate) fn filediffs(messages: Vec<crate::oc::StoredMessage>) -> Vec<FileDiff> {
    let mut diffs: BTreeMap<String, String> = BTreeMap::new();
    for message in messages {
        for part in message.parts {
            let Some(state) = part.get("state") else {
                continue;
            };
            let Some(metadata) = state.get("metadata") else {
                continue;
            };
            if let Some(filediff) = metadata.get("filediff") {
                let path = filediff
                    .get("filepath")
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| metadata.get("filepath").and_then(serde_json::Value::as_str));
                let patch = filediff.get("patch").and_then(serde_json::Value::as_str);
                if let (Some(path), Some(patch)) = (path, patch) {
                    diffs.insert(path.to_string(), patch.to_string());
                }
                continue;
            }
            // dsh reports applied hunks as before/after text, not a patch.
            let mut hunks: Vec<HunkText<'_>> = Vec::new();
            for entry in metadata
                .get("diffs")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(path) = entry.get("path").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let new = entry
                    .get("newText")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let old = entry.get("oldText").and_then(serde_json::Value::as_str);
                hunks.push(HunkText { path, old, new });
            }
            if hunks.is_empty() {
                if metadata
                    .get("operation")
                    .and_then(serde_json::Value::as_str)
                    != Some("create")
                {
                    continue;
                }
                let Some(path) = metadata.get("path").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let Some(content) = state
                    .pointer("/input/content")
                    .and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                if content.is_empty() {
                    continue;
                }
                hunks.push(HunkText {
                    path,
                    old: None,
                    new: content,
                });
            }
            if let Some(path) = hunks.first().map(|hunk| hunk.path) {
                diffs.insert(path.to_string(), patch_from_hunks(&hunks));
            }
        }
    }
    diffs
        .into_iter()
        .map(|(path, patch)| FileDiff { path, patch })
        .collect()
}
