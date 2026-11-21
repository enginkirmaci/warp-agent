//! Wire-shape helpers between the typed harness vocabulary and the store.
//!
//! The adapters speak [`HarnessEvent`]s and typed results only. The store's
//! transcript parser and the door's one-shot answers still read the app's
//! established opencode-shaped JSON, so the net thread rebuilds that shape here
//! from typed snapshots — never from a second protocol client. The typed-event
//! ingest and the history load call the same builders, so a replayed snapshot
//! and a restored transcript cannot drift apart.

use serde_json::{Value, json};

use super::types::{
    Agent, Command, FormRequest, HarnessId, HistoryMessage, HistoryPart, MessageSnapshot, Model,
    PartPayload, PartSnapshot, SessionId, SessionSummary, Skill, TokenUsage, ToolSnapshot,
    ToolState,
};
use crate::oc::{
    AgentInfo, CommandInfo, ModelInfo, QuestionOptionInfo, QuestionPromptInfo, QuestionRequestInfo,
    SessionInfo, StoredMessage,
};

/// The `info` object of one message snapshot.
pub(crate) fn message_info_json(message: &MessageSnapshot) -> Value {
    let mut info = json!({
        "id": message.id,
        "sessionID": message.session.as_str(),
        "role": message.role.as_wire(),
        "time": { "created": message.created_ms },
    });
    if let Some(completed) = message.completed_ms {
        info["time"]["completed"] = json!(completed);
    }
    if let Some(provider) = &message.provider {
        info["providerID"] = json!(provider);
    }
    if let Some(model) = &message.model {
        info["modelID"] = json!(model);
    }
    if let Some(error) = &message.error {
        info["error"] = json!({ "data": { "message": error } });
    }
    if let Some(tokens) = message.tokens {
        info["tokens"] = tokens_json(tokens);
    }
    if let Some(cost) = message.cost_micros {
        info["cost"] = json!(cost as f64 / 1_000_000.0);
    }
    info
}

fn tokens_json(tokens: TokenUsage) -> Value {
    json!({
        "input": tokens.input,
        "output": tokens.output,
        "reasoning": tokens.reasoning,
        "cache": { "read": tokens.cache_read, "write": tokens.cache_write },
    })
}

/// One full part snapshot as the transcript parser reads it.
pub(crate) fn part_json(part: &PartSnapshot) -> Value {
    let mut payload = json!({
        "id": part.id,
        "sessionID": part.session.as_str(),
        "messageID": part.message_id,
        "type": part.payload.as_wire(),
    });
    payload_fields(&mut payload, &part.payload);
    payload
}

fn payload_fields(payload: &mut Value, part: &PartPayload) {
    match part {
        PartPayload::Text(text) | PartPayload::Reasoning(text) => {
            payload["text"] = json!(text);
        }
        PartPayload::Tool(tool) => {
            payload["tool"] = json!(tool.name);
            payload["state"] = tool_state_json(tool);
        }
        PartPayload::File(file) => {
            payload["url"] = json!(file.url);
            payload["filename"] = json!(file.filename);
        }
        PartPayload::StepStart | PartPayload::StepFinish => {}
    }
}

fn tool_state_json(tool: &ToolSnapshot) -> Value {
    let status = match &tool.state {
        ToolState::Pending => "pending",
        ToolState::Running => "running",
        ToolState::Completed { .. } => "completed",
        ToolState::Failed { .. } => "error",
    };
    let mut state = json!({ "status": status });
    if let Some(input) = &tool.input {
        state["input"] = input.clone();
    }
    if let Some(title) = &tool.title {
        state["title"] = json!(title);
    }
    if let Some(metadata) = &tool.metadata {
        state["metadata"] = metadata.clone();
    }
    match &tool.state {
        ToolState::Completed { output } => state["output"] = json!(output),
        ToolState::Failed { message } => state["error"] = json!(message),
        ToolState::Pending | ToolState::Running => {}
    }
    state
}

/// A restored transcript, rebuilt from typed history.
pub(crate) fn stored_messages(history: Vec<HistoryMessage>) -> Vec<StoredMessage> {
    history
        .into_iter()
        .map(|history| {
            let message = &history.message;
            let info = message_info_json(message);
            let parts = history
                .parts
                .into_iter()
                .map(|part| {
                    let mut payload = json!({
                        "id": part.id,
                        "sessionID": message.session.as_str(),
                        "messageID": message.id,
                        "type": part.payload.as_wire(),
                    });
                    payload_fields(&mut payload, &part.payload);
                    payload
                })
                .collect();
            StoredMessage {
                id: message.id.clone(),
                role: message.role.as_wire().to_string(),
                created_ms: message.created_ms,
                info,
                parts,
            }
        })
        .collect()
}

/// Typed history from the app's stored message shape. Reuses the live event
/// translation so a restored part and a streamed one carry the same fields.
pub(crate) fn history_from_stored(
    session: &SessionId,
    messages: Vec<StoredMessage>,
) -> Vec<HistoryMessage> {
    let mut history = Vec::with_capacity(messages.len());
    for stored in messages {
        let mut info = stored.info.clone();
        if info.get("sessionID").is_none() {
            info["sessionID"] = json!(session.as_str());
        }
        let Some(super::types::HarnessEvent::MessageSnapshot { message }) =
            super::opencode::translate_event(&json!({
                "type": "message.updated",
                "properties": { "info": info },
            }))
        else {
            continue;
        };
        let parts = stored
            .parts
            .into_iter()
            .filter_map(|part| {
                let Some(super::types::HarnessEvent::PartSnapshot { part }) =
                    super::opencode::translate_event(&json!({
                        "type": "message.part.updated",
                        "properties": {
                            "sessionID": session.as_str(),
                            "part": part,
                        },
                    }))
                else {
                    return None;
                };
                Some(HistoryPart {
                    id: part.id,
                    payload: part.payload,
                })
            })
            .collect();
        history.push(HistoryMessage { message, parts });
    }
    history
}

/// One session summary as the store's chat lists read it.
pub(crate) fn session_info(summary: SessionSummary) -> SessionInfo {
    SessionInfo {
        id: summary.id.as_str().to_string(),
        title: summary.title,
        updated_ms: summary.updated_ms,
        created_ms: summary.created_ms,
        parent_id: summary.parent.map(|parent| parent.as_str().to_string()),
    }
}

/// The model catalog, tagged with the harness that serves it.
pub(crate) fn model_infos(harness: &HarnessId, models: Vec<Model>) -> Vec<ModelInfo> {
    models
        .into_iter()
        .map(|model| ModelInfo {
            key: model.key,
            harness: harness.clone(),
            variants: model.variants,
            context_limit: model.context_limit,
        })
        .collect()
}

/// The agent catalog.
pub(crate) fn agent_infos(agents: Vec<Agent>) -> Vec<AgentInfo> {
    agents
        .into_iter()
        .map(|agent| AgentInfo {
            name: agent.name,
            mode: agent.mode,
            hidden: agent.hidden,
        })
        .collect()
}

/// One command/skill catalog as the composer menu reads it.
pub(crate) fn command_infos(commands: Vec<Command>, skills: Vec<Skill>) -> Vec<CommandInfo> {
    let mut infos: Vec<CommandInfo> = commands
        .into_iter()
        .map(|command| CommandInfo {
            name_lower: command.name.to_lowercase(),
            name: command.name,
            description: command.description,
            source: "command".into(),
        })
        .collect();
    infos.extend(skills.into_iter().map(|skill| CommandInfo {
        name_lower: skill.name.to_lowercase(),
        name: skill.name,
        description: skill.description,
        source: "skill".into(),
    }));
    infos
}

/// The pending forms as the question card reads them.
pub(crate) fn question_requests(forms: Vec<FormRequest>) -> Vec<QuestionRequestInfo> {
    forms
        .into_iter()
        .map(|form| QuestionRequestInfo {
            id: form.id,
            session_id: form.session.as_str().to_string(),
            questions: form
                .fields
                .into_iter()
                .map(|field| QuestionPromptInfo {
                    question: field.title.clone(),
                    header: field.title,
                    multiple: matches!(field.kind, super::types::FormFieldKind::Multiselect),
                    options: field
                        .options
                        .into_iter()
                        .map(|label| QuestionOptionInfo {
                            label,
                            description: String::new(),
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect()
}

/// The typed forms for the pending questions the server reports; the inverse
/// of [`question_requests`], so a pulled question answers through the same
/// form-reply path as a streamed one.
pub(crate) fn form_requests(requests: Vec<QuestionRequestInfo>) -> Vec<FormRequest> {
    requests
        .into_iter()
        .map(|request| FormRequest {
            id: request.id,
            session: SessionId::new(request.session_id),
            title: request
                .questions
                .first()
                .map(|question| {
                    if question.header.is_empty() {
                        question.question.clone()
                    } else {
                        question.header.clone()
                    }
                })
                .unwrap_or_default(),
            fields: request
                .questions
                .into_iter()
                .enumerate()
                .map(|(index, question)| super::types::FormField {
                    id: index.to_string(),
                    title: if question.question.is_empty() {
                        question.header
                    } else {
                        question.question
                    },
                    kind: if question.multiple {
                        super::types::FormFieldKind::Multiselect
                    } else {
                        super::types::FormFieldKind::Text
                    },
                    required: true,
                    options: question
                        .options
                        .into_iter()
                        .map(|option| option.label)
                        .collect(),
                })
                .collect(),
        })
        .collect()
}

/// The typed answer for a question card's per-question selections.
pub(crate) fn form_answer(answers: Vec<Vec<String>>) -> super::types::FormAnswer {
    let mut values = std::collections::BTreeMap::new();
    for (index, labels) in answers.into_iter().enumerate() {
        let value = if labels.len() == 1 {
            Value::String(labels.into_iter().next().unwrap_or_default())
        } else {
            Value::Array(labels.into_iter().map(Value::String).collect())
        };
        values.insert(index.to_string(), value);
    }
    super::types::FormAnswer { values }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_stored_transcript_round_trips_through_typed_history() {
        let stored = vec![StoredMessage {
            id: "m1".into(),
            role: "assistant".into(),
            created_ms: 5,
            info: json!({
                "id": "m1",
                "sessionID": "s1",
                "role": "assistant",
                "time": { "created": 5, "completed": 9 },
                "providerID": "p",
                "modelID": "m",
                "tokens": { "input": 3, "output": 4, "reasoning": 0,
                            "cache": { "read": 1, "write": 2 } },
                "cost": 0.5,
            }),
            parts: vec![
                json!({ "id": "p1", "sessionID": "s1", "messageID": "m1",
                        "type": "text", "text": "hello" }),
                json!({ "id": "p2", "sessionID": "s1", "messageID": "m1",
                        "type": "tool", "tool": "bash",
                        "state": { "status": "completed", "output": "ok",
                                   "input": { "command": "ls" } } }),
            ],
        }];
        let typed = history_from_stored(&SessionId::new("s1"), stored);
        assert_eq!(typed.len(), 1);
        assert_eq!(typed[0].message.provider.as_deref(), Some("p"));
        assert_eq!(
            typed[0].message.tokens.map(|tokens| tokens.cache_read),
            Some(1)
        );
        assert_eq!(typed[0].parts.len(), 2);

        let back = stored_messages(typed);
        assert_eq!(back[0].id, "m1");
        assert_eq!(back[0].role, "assistant");
        assert_eq!(back[0].info["providerID"], "p");
        assert_eq!(back[0].info["tokens"]["cache"]["write"], 2);
        assert_eq!(back[0].parts[0]["type"], "text");
        assert_eq!(back[0].parts[0]["text"], "hello");
        assert_eq!(back[0].parts[1]["tool"], "bash");
        assert_eq!(back[0].parts[1]["state"]["output"], "ok");
    }

    #[test]
    fn the_catalogs_keep_their_store_shape() {
        let commands = command_infos(
            vec![Command {
                name: "Plan".into(),
                description: "draft a plan".into(),
            }],
            vec![Skill {
                id: "review".into(),
                name: "Review".into(),
                description: "review a change".into(),
            }],
        );
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].name_lower, "plan");
        assert_eq!(commands[0].source, "command");
        assert_eq!(commands[1].source, "skill");

        let answer = form_answer(vec![vec!["a".into(), "b".into()], vec![]]);
        assert_eq!(answer.values["0"], json!(["a", "b"]));
        assert_eq!(answer.values["1"], json!([]));
    }
}
