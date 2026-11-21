//! The door's app-level harness calls.
//!
//! One place maps the door's `harness` params to the [`Harness`] trait's typed
//! inputs and back: a [`Call`] is parsed before any adapter is touched (a typo
//! cannot spawn a server), then executed against one instance. The store
//! validates the harness id and the settings switch first; the net thread owns
//! the instance. Neither repeats this vocabulary.
//!
//! Wire method names are the trait's snake_case names; a `-` spelling is
//! accepted too. Param names are the typed fields' (`session`, `title`,
//! `model`, …), with the optional instance scope as `directory` and the
//! harness id as `harness`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::interface::Harness;
use super::types::{
    Agent, Command, CommandRun, FileDiff, FormAnswer, FormRequest, HarnessError, HarnessHealth,
    HistoryMessage, InboxDelivery, InboxItem, McpConfig, McpServer, Model, ModelChoice, NewSession,
    PermissionDecision, PermissionRequest, Prompt, Provider, SessionId, SessionSummary, Skill,
};

/// One parsed app-level harness call.
#[derive(Debug)]
pub enum Call {
    Providers,
    Models,
    Agents,
    Commands {
        session: Option<SessionId>,
    },
    Skills {
        session: Option<SessionId>,
    },
    Sessions,
    Session {
        session: SessionId,
    },
    Transcript {
        session: SessionId,
    },
    Create {
        spec: NewSession,
    },
    Fork {
        session: SessionId,
        before: Option<String>,
    },
    Rename {
        session: SessionId,
        title: String,
    },
    Delete {
        session: SessionId,
    },
    SendPrompt {
        session: SessionId,
        prompt: Prompt,
    },
    Interrupt {
        session: SessionId,
    },
    Compact {
        session: SessionId,
    },
    RunCommand {
        session: SessionId,
        command: CommandRun,
    },
    ActivateSkill {
        session: SessionId,
        skill: String,
    },
    Permissions {
        session: SessionId,
    },
    ReplyPermission {
        session: SessionId,
        request: String,
        decision: PermissionDecision,
    },
    Forms {
        session: SessionId,
    },
    ReplyForm {
        session: SessionId,
        form: String,
        answer: FormAnswer,
    },
    Diff {
        session: SessionId,
    },
    FindFiles {
        query: String,
    },
    ForgetSession {
        session: SessionId,
    },
    Inbox {
        session: SessionId,
    },
    UpdateInbox {
        session: SessionId,
        item: String,
        delivery: InboxDelivery,
    },
    CancelInbox {
        session: SessionId,
        item: String,
    },
    StageRevert {
        session: SessionId,
        before: String,
    },
    CommitRevert {
        session: SessionId,
    },
    ClearRevert {
        session: SessionId,
    },
    RunShell {
        session: SessionId,
        command: String,
    },
    BackgroundSession {
        session: SessionId,
    },
    McpServers,
    AddMcp {
        config: McpConfig,
    },
    ConnectMcp {
        name: String,
    },
    DisconnectMcp {
        name: String,
    },
    RemoveMcp {
        name: String,
    },
}

impl Call {
    /// Read one call off the wire, without touching an adapter.
    ///
    /// `directory` is the instance scope the store resolved when the params
    /// did not name one; session-creating calls inherit it as the new
    /// session's working directory.
    pub fn parse(method: &str, params: &Value, directory: Option<&Path>) -> Result<Self, String> {
        let raw = method.trim();
        let method = raw.replace('-', "_");
        let session = || required(params, "session").map(SessionId::new);
        Ok(match method.as_str() {
            "providers" => Self::Providers,
            "models" => Self::Models,
            "agents" => Self::Agents,
            "commands" => Self::Commands {
                session: optional_session(params)?,
            },
            "skills" => Self::Skills {
                session: optional_session(params)?,
            },
            "sessions" => Self::Sessions,
            "session" => Self::Session {
                session: session()?,
            },
            "transcript" => Self::Transcript {
                session: session()?,
            },
            "create" => Self::Create {
                spec: NewSession {
                    title: optional_text(params, "title"),
                    parent: optional_text(params, "parent").map(SessionId::new),
                    directory: optional_text(params, "session_directory")
                        .or_else(|| optional_text(params, "directory"))
                        .or_else(|| directory.map(|dir| dir.to_string_lossy().into_owned()))
                        .map(PathBuf::from),
                    choice: choice(params),
                },
            },
            "fork" => Self::Fork {
                session: session()?,
                before: optional_text(params, "before"),
            },
            "rename" => Self::Rename {
                session: session()?,
                title: required(params, "title")?,
            },
            "delete" => Self::Delete {
                session: session()?,
            },
            "send_prompt" => {
                let command = optional_command(params)?;
                // a command-only prompt is legal: the adapter sends the slash
                // route instead of a plain message, so its text may be empty
                let text = match &command {
                    Some(_) => optional_text(params, "text")
                        .or_else(|| optional_text(params, "prompt"))
                        .unwrap_or_default(),
                    None => required_any(params, &["text", "prompt"])?,
                };
                Self::SendPrompt {
                    session: session()?,
                    prompt: Prompt {
                        text,
                        choice: choice(params),
                        background: params
                            .get("background")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        attachments: attachments(params)?,
                        command,
                    },
                }
            }
            "interrupt" => Self::Interrupt {
                session: session()?,
            },
            "compact" => Self::Compact {
                session: session()?,
            },
            "run_command" => Self::RunCommand {
                session: session()?,
                command: CommandRun {
                    name: required_any(params, &["command", "name"])?,
                    text: optional_text(params, "text")
                        .or_else(|| optional_text(params, "args"))
                        .unwrap_or_default(),
                },
            },
            "activate_skill" => Self::ActivateSkill {
                session: session()?,
                skill: required(params, "skill")?,
            },
            "permissions" => Self::Permissions {
                session: session()?,
            },
            "reply_permission" => Self::ReplyPermission {
                session: session()?,
                request: required_any(params, &["request", "permission", "id"])?,
                decision: decision(params)?,
            },
            "forms" => Self::Forms {
                session: session()?,
            },
            "reply_form" => Self::ReplyForm {
                session: session()?,
                form: required_any(params, &["form", "request", "id"])?,
                answer: FormAnswer {
                    values: params
                        .get("values")
                        .and_then(Value::as_object)
                        .map(|values| {
                            values
                                .iter()
                                .map(|(key, value)| (key.clone(), value.clone()))
                                .collect()
                        })
                        .unwrap_or_default(),
                },
            },
            "diff" => Self::Diff {
                session: session()?,
            },
            "find_files" => Self::FindFiles {
                query: optional_text(params, "query").unwrap_or_default(),
            },
            "forget_session" => Self::ForgetSession {
                session: session()?,
            },
            "inbox" => Self::Inbox {
                session: session()?,
            },
            "update_inbox" => Self::UpdateInbox {
                session: session()?,
                item: required_any(params, &["item", "id"])?,
                delivery: match required(params, "delivery")?.as_str() {
                    "queue" => InboxDelivery::Queue,
                    "steer" => InboxDelivery::Steer,
                    other => {
                        return Err(format!("delivery must be queue or steer, not {other}"));
                    }
                },
            },
            "cancel_inbox" => Self::CancelInbox {
                session: session()?,
                item: required_any(params, &["item", "id"])?,
            },
            "stage_revert" => Self::StageRevert {
                session: session()?,
                before: required(params, "before")?,
            },
            "commit_revert" => Self::CommitRevert {
                session: session()?,
            },
            "clear_revert" => Self::ClearRevert {
                session: session()?,
            },
            "run_shell" => Self::RunShell {
                session: session()?,
                command: required(params, "command")?,
            },
            "background_session" => Self::BackgroundSession {
                session: session()?,
            },
            "mcp_list" => Self::McpServers,
            "mcp_add" => Self::AddMcp {
                config: McpConfig {
                    name: required(params, "name")?,
                    command: optional_text(params, "command"),
                    url: optional_text(params, "url"),
                    env: params
                        .get("env")
                        .and_then(Value::as_object)
                        .map(|env| {
                            env.iter()
                                .filter_map(|(key, value)| {
                                    value.as_str().map(|value| (key.clone(), value.to_string()))
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                },
            },
            "mcp_connect" => Self::ConnectMcp {
                name: required(params, "name")?,
            },
            "mcp_disconnect" => Self::DisconnectMcp {
                name: required(params, "name")?,
            },
            "mcp_remove" => Self::RemoveMcp {
                name: required(params, "name")?,
            },
            _ => return Err(format!("unsupported: harness method {raw}")),
        })
    }

    /// Run one parsed call against a single instance, returning the typed
    /// result as JSON. Unit results answer `{"ok": true}`; adapter failures
    /// keep their typed door prefix.
    pub fn execute(self, harness: &mut dyn Harness) -> Result<Value, String> {
        match self {
            Self::Providers => {
                let providers = harness.providers().map_err(error_string)?;
                Ok(json!({
                    "providers": providers.iter().map(provider_json).collect::<Vec<_>>(),
                }))
            }
            Self::Models => {
                let models = harness.models().map_err(error_string)?;
                Ok(json!({
                    "models": models.iter().map(model_json).collect::<Vec<_>>(),
                }))
            }
            Self::Agents => {
                let agents = harness.agents().map_err(error_string)?;
                Ok(json!({
                    "agents": agents.iter().map(agent_json).collect::<Vec<_>>(),
                }))
            }
            Self::Commands { session } => {
                let commands = harness.commands(session.as_ref()).map_err(error_string)?;
                Ok(json!({
                    "commands": commands.iter().map(command_json).collect::<Vec<_>>(),
                }))
            }
            Self::Skills { session } => {
                let skills = harness.skills(session.as_ref()).map_err(error_string)?;
                Ok(json!({
                    "skills": skills.iter().map(skill_json).collect::<Vec<_>>(),
                }))
            }
            Self::Sessions => {
                let sessions = harness.sessions().map_err(error_string)?;
                Ok(json!({
                    "sessions": sessions.iter().map(session_json).collect::<Vec<_>>(),
                }))
            }
            Self::Session { session } => {
                let summary = harness.session(&session).map_err(error_string)?;
                Ok(session_json(&summary))
            }
            Self::Transcript { session } => {
                let history = harness.transcript(&session).map_err(error_string)?;
                Ok(json!({
                    "messages": history.iter().map(history_json).collect::<Vec<_>>(),
                }))
            }
            Self::Create { spec } => {
                let summary = harness.create_session(spec).map_err(error_string)?;
                Ok(session_json(&summary))
            }
            Self::Fork { session, before } => {
                let summary = harness
                    .fork_session(&session, before.as_deref())
                    .map_err(error_string)?;
                Ok(session_json(&summary))
            }
            Self::Rename { session, title } => {
                harness
                    .rename_session(&session, &title)
                    .map_err(error_string)?;
                Ok(ok())
            }
            Self::Delete { session } => {
                harness.delete_session(&session).map_err(error_string)?;
                Ok(ok())
            }
            Self::SendPrompt { session, prompt } => {
                harness
                    .send_prompt(&session, prompt)
                    .map_err(error_string)?;
                Ok(ok())
            }
            Self::Interrupt { session } => {
                harness.interrupt(&session).map_err(error_string)?;
                Ok(ok())
            }
            Self::Compact { session } => {
                harness.compact(&session).map_err(error_string)?;
                Ok(ok())
            }
            Self::RunCommand { session, command } => {
                harness
                    .run_command(&session, command)
                    .map_err(error_string)?;
                Ok(ok())
            }
            Self::ActivateSkill { session, skill } => {
                harness
                    .activate_skill(&session, &skill)
                    .map_err(error_string)?;
                Ok(ok())
            }
            Self::Permissions { session } => {
                let permissions = harness.permissions(&session).map_err(error_string)?;
                Ok(json!({
                    "permissions": permissions.iter().map(permission_json).collect::<Vec<_>>(),
                }))
            }
            Self::ReplyPermission {
                session,
                request,
                decision,
            } => {
                harness
                    .reply_permission(&session, &request, decision)
                    .map_err(error_string)?;
                Ok(ok())
            }
            Self::Forms { session } => {
                let forms = harness.forms(&session).map_err(error_string)?;
                Ok(json!({
                    "forms": forms.iter().map(form_json).collect::<Vec<_>>(),
                }))
            }
            Self::ReplyForm {
                session,
                form,
                answer,
            } => {
                harness
                    .reply_form(&session, &form, answer)
                    .map_err(error_string)?;
                Ok(ok())
            }
            Self::Diff { session } => {
                let diffs = harness.diff(&session).map_err(error_string)?;
                Ok(json!({
                    "diffs": diffs.iter().map(diff_json).collect::<Vec<_>>(),
                }))
            }
            Self::FindFiles { query } => {
                let files = harness.find_files(&query).map_err(error_string)?;
                Ok(json!({ "files": files }))
            }
            Self::ForgetSession { session } => {
                harness.forget_session(&session).map_err(error_string)?;
                Ok(ok())
            }
            Self::Inbox { session } => {
                let inbox = harness.inbox(&session).map_err(error_string)?;
                Ok(json!({
                    "inbox": inbox.iter().map(inbox_json).collect::<Vec<_>>(),
                }))
            }
            Self::UpdateInbox {
                session,
                item,
                delivery,
            } => {
                harness
                    .update_inbox(&session, &item, delivery)
                    .map_err(error_string)?;
                Ok(ok())
            }
            Self::CancelInbox { session, item } => {
                harness
                    .cancel_inbox(&session, &item)
                    .map_err(error_string)?;
                Ok(ok())
            }
            Self::StageRevert { session, before } => {
                harness
                    .stage_revert(&session, &before)
                    .map_err(error_string)?;
                Ok(ok())
            }
            Self::CommitRevert { session } => {
                harness.commit_revert(&session).map_err(error_string)?;
                Ok(ok())
            }
            Self::ClearRevert { session } => {
                harness.clear_revert(&session).map_err(error_string)?;
                Ok(ok())
            }
            Self::RunShell { session, command } => {
                harness
                    .run_shell(&session, &command)
                    .map_err(error_string)?;
                Ok(ok())
            }
            Self::BackgroundSession { session } => {
                harness.background_session(&session).map_err(error_string)?;
                Ok(ok())
            }
            Self::McpServers => {
                let servers = harness.mcp_servers().map_err(error_string)?;
                Ok(json!({
                    "servers": servers.iter().map(mcp_json).collect::<Vec<_>>(),
                }))
            }
            Self::AddMcp { config } => {
                harness.add_mcp(config).map_err(error_string)?;
                Ok(ok())
            }
            Self::ConnectMcp { name } => {
                harness.connect_mcp(&name).map_err(error_string)?;
                Ok(ok())
            }
            Self::DisconnectMcp { name } => {
                harness.disconnect_mcp(&name).map_err(error_string)?;
                Ok(ok())
            }
            Self::RemoveMcp { name } => {
                harness.remove_mcp(&name).map_err(error_string)?;
                Ok(ok())
            }
        }
    }
}

/// Whether a method is one this module dispatches, without parsing its params.
pub fn known(method: &str) -> bool {
    let method = normalize(method);
    matches!(
        method.as_str(),
        "providers"
            | "models"
            | "agents"
            | "commands"
            | "skills"
            | "sessions"
            | "session"
            | "transcript"
            | "create"
            | "fork"
            | "rename"
            | "delete"
            | "send_prompt"
            | "interrupt"
            | "compact"
            | "run_command"
            | "activate_skill"
            | "permissions"
            | "reply_permission"
            | "forms"
            | "reply_form"
            | "diff"
            | "find_files"
            | "forget_session"
            | "inbox"
            | "update_inbox"
            | "cancel_inbox"
            | "stage_revert"
            | "commit_revert"
            | "clear_revert"
            | "run_shell"
            | "background_session"
            | "mcp_list"
            | "mcp_add"
            | "mcp_connect"
            | "mcp_disconnect"
            | "mcp_remove"
    )
}

/// The door error string for a typed harness failure: the same
/// `unknown:` / `disabled:` / `unavailable:` / `unsupported:` vocabulary the
/// earlier door answers already used.
pub fn error_string(error: HarnessError) -> String {
    match error {
        HarnessError::UnknownHarness(id) => format!("unknown: harness {id}"),
        HarnessError::Disabled(id) => format!("disabled: harness {id} is disabled"),
        HarnessError::Unavailable(reason) => format!("unavailable: {reason}"),
        HarnessError::Unsupported(operation) => {
            format!("unsupported: harness operation {operation}")
        }
        HarnessError::DuplicateHarness(id) => format!("duplicate: harness {id}"),
        HarnessError::DuplicateInstance(id) => format!("duplicate: harness instance {id}"),
        HarnessError::Failed(reason) => reason,
    }
}

/// The door error string for an id that is not in the registry.
pub fn unknown(id: &str) -> String {
    format!("unknown: harness {id}")
}

/// The door error string for a harness that is switched off in settings.
pub fn disabled(id: &str) -> String {
    format!("disabled: harness {id} is disabled")
}

/// One harness health as the door reports it.
pub fn health_json(health: &HarnessHealth) -> Value {
    json!({
        "state": match health.state {
            crate::harness::HealthState::Ready => "ready",
            crate::harness::HealthState::Starting => "starting",
            crate::harness::HealthState::Unavailable => "unavailable",
        },
        "detail": health.detail,
    })
}

fn normalize(method: &str) -> String {
    method.trim().replace('-', "_")
}

fn ok() -> Value {
    json!({ "ok": true })
}

fn text(params: &Value, name: &str) -> Option<String> {
    params
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn optional_text(params: &Value, name: &str) -> Option<String> {
    text(params, name)
}

fn required(params: &Value, name: &str) -> Result<String, String> {
    text(params, name).ok_or_else(|| format!("{name} is required"))
}

fn required_any(params: &Value, names: &[&str]) -> Result<String, String> {
    for name in names {
        if let Some(value) = text(params, name) {
            return Ok(value);
        }
    }
    Err(format!("{} is required", names.join(" or ")))
}

fn optional_session(params: &Value) -> Result<Option<SessionId>, String> {
    Ok(text(params, "session").map(SessionId::new))
}

fn choice(params: &Value) -> ModelChoice {
    ModelChoice {
        model: text(params, "model"),
        agent: text(params, "agent"),
        variant: text(params, "variant"),
    }
}

fn decision(params: &Value) -> Result<PermissionDecision, String> {
    match required(params, "decision")?.as_str() {
        "once" => Ok(PermissionDecision::Once),
        "always" => Ok(PermissionDecision::Always),
        "reject" => Ok(PermissionDecision::Reject),
        other => Err(format!(
            "decision must be once, always or reject, not {other}"
        )),
    }
}

fn optional_command(params: &Value) -> Result<Option<CommandRun>, String> {
    let Some(command) = params.get("command") else {
        return Ok(None);
    };
    if let Some(name) = command
        .as_str()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        return Ok(Some(CommandRun {
            name: name.to_string(),
            text: optional_text(params, "command_text").unwrap_or_default(),
        }));
    }
    let object = command
        .as_object()
        .ok_or_else(|| "command must be a name or an object".to_string())?;
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "command name is required".to_string())?;
    Ok(Some(CommandRun {
        name: name.to_string(),
        text: object
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }))
}

/// A prompt's file attachments: `[{filename, mime, data}]`, `data` base64.
fn attachments(params: &Value) -> Result<Vec<crate::harness::Attachment>, String> {
    let Some(items) = params.get("attachments") else {
        return Ok(Vec::new());
    };
    let items = items
        .as_array()
        .ok_or_else(|| "attachments must be a list".to_string())?;
    let mut attachments = Vec::with_capacity(items.len());
    for item in items {
        let filename = item
            .get("filename")
            .and_then(Value::as_str)
            .filter(|filename| !filename.is_empty())
            .ok_or_else(|| "attachment filename is required".to_string())?;
        let mime = item
            .get("mime")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_string();
        let data = item
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| "attachment data is required".to_string())?;
        let bytes = crate::store::decode_base64(data)
            .ok_or_else(|| format!("attachment {filename} is not valid base64"))?;
        attachments.push(crate::harness::Attachment {
            filename: filename.to_string(),
            mime,
            bytes: std::sync::Arc::new(bytes),
        });
    }
    Ok(attachments)
}

fn provider_json(provider: &Provider) -> Value {
    json!({ "id": provider.id, "name": provider.name })
}

fn model_json(model: &Model) -> Value {
    json!({
        "key": model.key,
        "provider": model.provider,
        "variants": model.variants,
        "context_limit": model.context_limit,
    })
}

fn agent_json(agent: &Agent) -> Value {
    json!({
        "name": agent.name,
        "mode": agent.mode,
        "hidden": agent.hidden,
    })
}

fn command_json(command: &Command) -> Value {
    json!({
        "name": command.name,
        "description": command.description,
    })
}

fn skill_json(skill: &Skill) -> Value {
    json!({
        "id": skill.id,
        "name": skill.name,
        "description": skill.description,
    })
}

fn session_json(session: &SessionSummary) -> Value {
    json!({
        "id": session.id.as_str(),
        "title": session.title,
        "parent": session.parent.as_ref().map(SessionId::as_str),
        "directory": session.directory.as_ref().map(|path| path.to_string_lossy()),
        "updated_ms": session.updated_ms,
        "created_ms": session.created_ms,
    })
}

fn history_json(history: &HistoryMessage) -> Value {
    json!({
        "message": {
            "id": history.message.id,
            "session": history.message.session.as_str(),
            "role": history.message.role.as_wire(),
            "created_ms": history.message.created_ms,
            "completed_ms": history.message.completed_ms,
            "provider": history.message.provider,
            "model": history.message.model,
            "error": history.message.error,
            "tokens": history.message.tokens.map(|tokens| json!({
                "input": tokens.input,
                "output": tokens.output,
                "reasoning": tokens.reasoning,
                "cache_read": tokens.cache_read,
                "cache_write": tokens.cache_write,
            })),
            "cost_micros": history.message.cost_micros,
        },
        "parts": history.parts.iter().map(|part| json!({
            "id": part.id,
            "payload": payload_json(&part.payload),
        })).collect::<Vec<_>>(),
    })
}

fn payload_json(payload: &super::types::PartPayload) -> Value {
    use super::types::{PartPayload, ToolState};
    match payload {
        PartPayload::Text(text) => json!({ "kind": "text", "text": text }),
        PartPayload::Reasoning(text) => json!({ "kind": "reasoning", "text": text }),
        PartPayload::Tool(tool) => json!({
            "kind": "tool",
            "name": tool.name,
            "state": match &tool.state {
                ToolState::Pending => json!({ "kind": "pending" }),
                ToolState::Running => json!({ "kind": "running" }),
                ToolState::Completed { output } => {
                    json!({ "kind": "completed", "output": output })
                }
                ToolState::Failed { message } => {
                    json!({ "kind": "failed", "message": message })
                }
            },
            "input": tool.input,
            "title": tool.title,
            "metadata": tool.metadata,
        }),
        PartPayload::StepStart => json!({ "kind": "step-start" }),
        PartPayload::StepFinish => json!({ "kind": "step-finish" }),
        PartPayload::File(file) => {
            json!({ "kind": "file", "url": file.url, "filename": file.filename })
        }
    }
}

fn permission_json(permission: &PermissionRequest) -> Value {
    json!({
        "id": permission.id,
        "session": permission.session.as_str(),
        "action": permission.action,
        "resources": permission.resources,
        "message": permission.message,
        "diff": permission.diff,
    })
}

fn form_json(form: &FormRequest) -> Value {
    use super::types::FormFieldKind;
    json!({
        "id": form.id,
        "session": form.session.as_str(),
        "title": form.title,
        "fields": form.fields.iter().map(|field| json!({
            "id": field.id,
            "title": field.title,
            "kind": match field.kind {
                FormFieldKind::Text => "text",
                FormFieldKind::Number => "number",
                FormFieldKind::Integer => "integer",
                FormFieldKind::Boolean => "boolean",
                FormFieldKind::Multiselect => "multiselect",
                FormFieldKind::External => "external",
            },
            "required": field.required,
            "options": field.options,
        })).collect::<Vec<_>>(),
    })
}

fn diff_json(diff: &FileDiff) -> Value {
    json!({ "path": diff.path, "patch": diff.patch })
}

fn inbox_json(item: &InboxItem) -> Value {
    use super::types::InboxKind;
    json!({
        "id": item.id,
        "session": item.session.as_str(),
        "kind": match item.kind {
            InboxKind::User => "user",
            InboxKind::Synthetic => "synthetic",
            InboxKind::Compaction => "compaction",
            InboxKind::Move => "move",
        },
        "delivery": match item.delivery {
            InboxDelivery::Queue => "queue",
            InboxDelivery::Steer => "steer",
        },
    })
}

fn mcp_json(server: &McpServer) -> Value {
    use super::types::McpStatus;
    json!({
        "name": server.name,
        "status": match server.status {
            McpStatus::Connected => "connected",
            McpStatus::Pending => "pending",
            McpStatus::Disabled => "disabled",
            McpStatus::Failed => "failed",
            McpStatus::NeedsAuth => "needs-auth",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::test_support::SurfaceHarness;
    use serde_json::json;

    fn parse(method: &str, params: Value) -> Result<Call, String> {
        Call::parse(method, &params, None)
    }

    #[test]
    fn the_method_names_accept_both_spellings() {
        assert!(known("send_prompt"));
        assert!(known("send-prompt"));
        assert!(known("stage-revert"));
        assert!(known("mcp-list"));
        assert!(known("mcp_list"));
        assert!(!known("no-such-method"));
        let dashed = parse(
            "reply-permission",
            json!({
                "session": "s1", "request": "r1", "decision": "always"
            }),
        )
        .expect("a dashed name parses");
        assert!(matches!(
            dashed,
            Call::ReplyPermission {
                decision: PermissionDecision::Always,
                ..
            }
        ));
    }

    #[test]
    fn a_call_without_its_params_is_rejected_before_an_adapter_sees_it() {
        let error = parse("send_prompt", json!({"session": "s1"})).unwrap_err();
        assert_eq!(error, "text or prompt is required");
        let error = parse("rename", json!({"session": "s1"})).unwrap_err();
        assert!(error.contains("title is required"), "{error}");
        let error = parse(
            "reply_permission",
            json!({
                "session": "s1", "request": "r1", "decision": "maybe"
            }),
        )
        .unwrap_err();
        assert!(error.contains("decision must be"), "{error}");
        let error = parse(
            "update_inbox",
            json!({
                "session": "s1", "item": "i1", "delivery": "later"
            }),
        )
        .unwrap_err();
        assert!(error.contains("delivery must be"), "{error}");
        let error = parse("no-such-method", json!({})).unwrap_err();
        assert_eq!(error, "unsupported: harness method no-such-method");
    }

    #[test]
    fn the_catalogs_from_an_adapter_come_back_typed() {
        let mut harness = SurfaceHarness::new("fake");
        let providers = dispatch(
            &mut harness,
            "providers",
            &json!({}),
            Some(Path::new("/tmp/fake-ws")),
        )
        .expect("providers");
        assert_eq!(providers["providers"][0]["id"], "provider-1");
        let models = dispatch(&mut harness, "models", &json!({}), None).expect("models");
        assert_eq!(models["models"][0]["key"], "provider-1/model-1");
        assert_eq!(models["models"][0]["variants"], json!(["low", "high"]));
        let agents = dispatch(&mut harness, "agents", &json!({}), None).expect("agents");
        assert_eq!(agents["agents"][0]["name"], "build");
    }

    fn dispatch(
        harness: &mut dyn Harness,
        method: &str,
        params: &Value,
        directory: Option<&Path>,
    ) -> Result<Value, String> {
        Call::parse(method, params, directory)?.execute(harness)
    }

    #[test]
    fn a_create_call_carries_the_session_directory_and_choice() {
        let mut harness = SurfaceHarness::new("fake");
        let summary = dispatch(
            &mut harness,
            "create",
            &json!({"title": "New chat", "model": "p/m", "variant": "high"}),
            Some(Path::new("/tmp/fake-ws")),
        )
        .expect("create");
        assert_eq!(summary["id"], "session-1");
        assert_eq!(summary["title"], "New chat");
        assert_eq!(summary["directory"], "/tmp/fake-ws");
        let recorded = harness.last_new_session().expect("a recorded spec");
        assert_eq!(recorded.choice.model.as_deref(), Some("p/m"));
        assert_eq!(recorded.choice.variant.as_deref(), Some("high"));
    }

    #[test]
    fn a_prompt_call_carries_its_choice_attachments_and_command() {
        let mut harness = SurfaceHarness::new("fake");
        dispatch(
            &mut harness,
            "send-prompt",
            &json!({
                "session": "s1",
                "text": "fix the build",
                "model": "p/m",
                "agent": "build",
                "background": true,
                "attachments": [{
                    "filename": "a.txt",
                    "mime": "text/plain",
                    "data": "aGVsbG8="
                }],
                "command": {"name": "plan", "text": "the migration"},
            }),
            None,
        )
        .expect("send prompt");
        let prompt = harness.last_prompt().expect("a recorded prompt");
        assert_eq!(prompt.text, "fix the build");
        assert_eq!(prompt.choice.agent.as_deref(), Some("build"));
        assert!(prompt.background);
        assert_eq!(prompt.attachments.len(), 1);
        assert_eq!(prompt.attachments[0].filename, "a.txt");
        assert_eq!(&*prompt.attachments[0].bytes, b"hello");
        let command = prompt.command.expect("a command");
        assert_eq!(command.name, "plan");
        assert_eq!(command.text, "the migration");
    }

    #[test]
    fn the_mutations_answer_ok_and_reach_the_adapter() {
        let mut harness = SurfaceHarness::new("fake");
        let calls = [
            ("interrupt", json!({"session": "s1"})),
            ("compact", json!({"session": "s1"})),
            (
                "activate_skill",
                json!({"session": "s1", "skill": "review"}),
            ),
            ("rename", json!({"session": "s1", "title": "Renamed"})),
            ("delete", json!({"session": "s1"})),
            ("stage_revert", json!({"session": "s1", "before": "m1"})),
            ("commit_revert", json!({"session": "s1"})),
            ("clear_revert", json!({"session": "s1"})),
            ("run_shell", json!({"session": "s1", "command": "ls"})),
            ("background_session", json!({"session": "s1"})),
            (
                "reply_form",
                json!({"session": "s1", "form": "f1", "values": {"0": "yes"}}),
            ),
            (
                "update_inbox",
                json!({"session": "s1", "item": "i1", "delivery": "steer"}),
            ),
            ("cancel_inbox", json!({"session": "s1", "item": "i1"})),
            ("mcp_connect", json!({"name": "docs"})),
            ("mcp_disconnect", json!({"name": "docs"})),
            ("mcp_remove", json!({"name": "docs"})),
        ];
        for (method, params) in calls {
            let result = dispatch(&mut harness, method, &params, None)
                .unwrap_or_else(|error| panic!("{method}: {error}"));
            assert_eq!(result, json!({"ok": true}), "{method}");
        }
    }

    #[test]
    fn the_approval_and_inbox_surfaces_come_back_typed() {
        let mut harness = SurfaceHarness::new("fake");
        let permissions = dispatch(&mut harness, "permissions", &json!({"session": "s1"}), None)
            .expect("permissions");
        assert_eq!(permissions["permissions"][0]["action"], "bash");
        assert_eq!(permissions["permissions"][0]["resources"][0], "ls -la");
        let forms =
            dispatch(&mut harness, "forms", &json!({"session": "s1"}), None).expect("forms");
        assert_eq!(forms["forms"][0]["title"], "Proceed?");
        assert_eq!(forms["forms"][0]["fields"][0]["kind"], "text");
        let inbox =
            dispatch(&mut harness, "inbox", &json!({"session": "s1"}), None).expect("inbox");
        assert_eq!(inbox["inbox"][0]["delivery"], "queue");
        let diffs = dispatch(&mut harness, "diff", &json!({"session": "s1"}), None).expect("diff");
        assert_eq!(diffs["diffs"][0]["path"], "src/main.rs");
    }

    #[test]
    fn the_catalogs_and_lifecycle_surfaces_come_back_typed() {
        let mut harness = SurfaceHarness::new("fake");
        let commands =
            dispatch(&mut harness, "commands", &json!({"session": "s1"}), None).expect("commands");
        assert_eq!(commands["commands"][0]["name"], "plan");
        let skills = dispatch(&mut harness, "skills", &json!({}), None).expect("skills");
        assert_eq!(skills["skills"][0]["id"], "review");
        let transcript = dispatch(&mut harness, "transcript", &json!({"session": "s1"}), None)
            .expect("transcript");
        assert_eq!(transcript["messages"][0]["message"]["role"], "assistant");
        assert_eq!(
            transcript["messages"][0]["parts"][0]["payload"]["kind"],
            "text"
        );
        let files = dispatch(&mut harness, "find_files", &json!({"query": "main"}), None)
            .expect("find files");
        assert_eq!(files["files"][0], "src/main.rs");
        let servers = dispatch(&mut harness, "mcp_list", &json!({}), None).expect("mcp");
        assert_eq!(servers["servers"][0]["status"], "connected");
    }

    #[test]
    fn an_unsupported_adapter_operation_keeps_the_door_prefix() {
        let mut harness = crate::harness::test_support::FakeHarness::new("fake");
        let error = dispatch(&mut harness, "agents", &json!({}), None).unwrap_err();
        assert_eq!(error, "unsupported: harness operation agents");
        let error = dispatch(&mut harness, "diff", &json!({"session": "s1"}), None).unwrap_err();
        assert_eq!(error, "unsupported: harness operation diff");
    }

    #[test]
    fn a_typed_failure_maps_to_the_door_errors() {
        assert_eq!(
            error_string(HarnessError::UnknownHarness("nope".into())),
            "unknown: harness nope"
        );
        assert_eq!(
            error_string(HarnessError::Disabled("dsh".into())),
            "disabled: harness dsh is disabled"
        );
        assert_eq!(
            error_string(HarnessError::Unavailable("not attached".into())),
            "unavailable: not attached"
        );
        let health = health_json(&HarnessHealth::ready());
        assert_eq!(health["state"], "ready");
        assert_eq!(health["detail"], Value::Null);
    }
}
