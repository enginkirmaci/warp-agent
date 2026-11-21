//! Typed harness-event ingestion.
//!
//! Every adapter's typed [`HarnessEvent`] stream lands here and feeds the
//! store's existing handlers through their stream frames, so the transcript,
//! tool, approval and catalog state has one state machine rather than a second
//! one per harness. The net thread forwards managed harness events tagged with
//! their instance; the store never parses a harness wire format.

use std::path::Path;

use super::Store;
use crate::harness::bridge;
use crate::harness::{
    FormFieldKind, HarnessError, HarnessEvent, HarnessId, SessionId, SessionState,
};
use crate::oc::{AgentInfo, CommandInfo, ModelInfo, UiEvent};
use gpui::Context;
use serde_json::{Value, json};

impl Store {
    /// Turn the typed harness-event path on or off; it is on from startup and
    /// only the probe seam ever flips it.
    pub fn set_harness_ingest(&mut self, enabled: bool) {
        self.harness_ingest = enabled;
    }

    /// Feed one typed harness event into the store.
    pub fn ingest_harness_event(
        &mut self,
        harness: &HarnessId,
        event: HarnessEvent,
        cx: &mut Context<Self>,
    ) -> Result<(), HarnessError> {
        self.ingest_harness_event_from(harness, event, None, cx)
    }

    /// Feed one typed harness event, tagged with the per-workspace instance's
    /// folder when the harness has one: the event stream itself never names a
    /// workspace, and the store needs it to address the right chat list.
    pub fn ingest_harness_event_from(
        &mut self,
        harness: &HarnessId,
        event: HarnessEvent,
        directory: Option<&Path>,
        cx: &mut Context<Self>,
    ) -> Result<(), HarnessError> {
        if !self.harness_ingest {
            return Err(HarnessError::Unavailable(
                "typed harness events are off".into(),
            ));
        }
        self.ipc_broadcast_harness_event(harness, &event);
        match event {
            // connection, health and process-exit facts reach the store as
            // their app-level notifications; they are not transcript events
            HarnessEvent::Connected
            | HarnessEvent::Reconnecting { .. }
            | HarnessEvent::Disconnected { .. }
            | HarnessEvent::Exited
            | HarnessEvent::Health(_) => Ok(()),
            HarnessEvent::SessionCreated {
                session,
                title,
                parent,
                agent,
            } => {
                let ws_key = self.harness_ws_key(harness, Some(&session), directory)?;
                let mut info = json!({ "id": session.as_str() });
                if let Some(title) = title {
                    info["title"] = json!(title);
                }
                if let Some(parent) = parent {
                    info["parentID"] = json!(parent.as_str());
                }
                if let Some(agent) = agent {
                    info["agent"] = json!(agent);
                }
                self.stream_event(
                    ws_key,
                    json!({ "type": "session.created", "properties": { "info": info } }),
                    cx,
                );
                Ok(())
            }
            HarnessEvent::SessionChanged {
                session,
                title,
                parent,
                agent,
            } => {
                let ws_key = self.harness_ws_key(harness, Some(&session), directory)?;
                let mut info = json!({ "id": session.as_str() });
                if let Some(title) = title {
                    info["title"] = json!(title);
                }
                if let Some(agent) = agent {
                    info["agent"] = json!(agent);
                }
                // a title update must not detach a subagent: carry the parent
                // the store already knows about when the event omits it
                let parent = parent.or_else(|| {
                    self.subagent_by_session
                        .get(session.as_str())
                        .map(|parent| SessionId::new(parent.parent_session_id.clone()))
                });
                if let Some(parent) = parent {
                    info["parentID"] = json!(parent.as_str());
                }
                self.stream_event(
                    ws_key,
                    json!({ "type": "session.updated", "properties": { "info": info } }),
                    cx,
                );
                Ok(())
            }
            HarnessEvent::SessionDeleted { session } => {
                self.on_net_event(
                    UiEvent::SessionDeleted {
                        session_id: session.as_str().to_string(),
                        result: Ok(()),
                    },
                    cx,
                );
                Ok(())
            }
            HarnessEvent::SessionStatus { session, state } => {
                let ws_key = self.harness_ws_key(harness, Some(&session), directory)?;
                let event = match state {
                    SessionState::Idle => json!({
                        "type": "session.idle",
                        "properties": { "sessionID": session.as_str() },
                    }),
                    SessionState::Busy => json!({
                        "type": "session.status",
                        "properties": {
                            "sessionID": session.as_str(),
                            "status": { "type": "busy" },
                        },
                    }),
                    SessionState::Retry => json!({
                        "type": "session.status",
                        "properties": {
                            "sessionID": session.as_str(),
                            "status": { "type": "retry" },
                        },
                    }),
                };
                self.stream_event(ws_key, event, cx);
                Ok(())
            }
            HarnessEvent::MessageSnapshot { message } => {
                let ws_key = self.harness_ws_key(harness, Some(&message.session), directory)?;
                self.stream_event(
                    ws_key,
                    json!({
                        "type": "message.updated",
                        "properties": {
                            "sessionID": message.session.as_str(),
                            "info": bridge::message_info_json(&message),
                        },
                    }),
                    cx,
                );
                Ok(())
            }
            HarnessEvent::PartSnapshot { part } => {
                let ws_key = self.harness_ws_key(harness, Some(&part.session), directory)?;
                self.stream_event(
                    ws_key,
                    json!({
                        "type": "message.part.updated",
                        "properties": {
                            "sessionID": part.session.as_str(),
                            "part": bridge::part_json(&part),
                        },
                    }),
                    cx,
                );
                Ok(())
            }
            HarnessEvent::Todos { session, todos } => {
                let ws_key = self.harness_ws_key(harness, Some(&session), directory)?;
                let todos: Vec<Value> = todos
                    .into_iter()
                    .map(|todo| {
                        let mut item = json!({
                            "content": todo.content,
                            "status": todo.status,
                        });
                        if let Some(priority) = todo.priority {
                            item["priority"] = json!(priority);
                        }
                        item
                    })
                    .collect();
                self.stream_event(
                    ws_key,
                    json!({
                        "type": "todo.updated",
                        "properties": {
                            "sessionID": session.as_str(),
                            "todos": todos,
                        },
                    }),
                    cx,
                );
                Ok(())
            }
            HarnessEvent::PartRemoved { session, part_id } => {
                let ws_key = self.harness_ws_key(harness, Some(&session), directory)?;
                self.stream_event(
                    ws_key,
                    json!({
                        "type": "message.part.removed",
                        "properties": {
                            "sessionID": session.as_str(),
                            "partID": part_id,
                        },
                    }),
                    cx,
                );
                Ok(())
            }
            HarnessEvent::PermissionAsked { request } => {
                let ws_key = self.harness_ws_key(harness, Some(&request.session), directory)?;
                let event_type =
                    if harness_kind(harness).is_ok_and(|id| crate::harness::attached(&id)) {
                        "permission.v2.asked"
                    } else {
                        "permission.asked"
                    };
                // the legacy v1 card reads `permission`/`patterns`; the exact
                // action/resources shape lands with the v2 adapter
                let mut event = json!({
                    "type": event_type,
                    "properties": {
                        "id": request.id,
                        "sessionID": request.session.as_str(),
                        "permission": request.action,
                        "patterns": request.resources,
                    },
                });
                if request.message.is_some() || request.diff.is_some() {
                    let mut metadata = json!({});
                    if let Some(message) = request.message {
                        metadata["command"] = json!(message);
                    }
                    if let Some(diff) = request.diff {
                        metadata["diff"] = json!(diff);
                    }
                    event["properties"]["metadata"] = metadata;
                }
                self.stream_event(ws_key, event, cx);
                Ok(())
            }
            HarnessEvent::PermissionReplied { request, .. } => {
                // the legacy card resolves by request id and says "answered
                // elsewhere"; the typed decision is for the door/v2 path
                self.on_permission_replied(&json!({ "requestID": request }));
                Ok(())
            }
            HarnessEvent::QuestionAsked { request } => {
                let ws_key = self.harness_ws_key(harness, Some(&request.session), directory)?;
                let questions: Vec<Value> = request
                    .fields
                    .iter()
                    .map(|field| {
                        let options: Vec<Value> = field
                            .options
                            .iter()
                            .map(|option| json!({ "label": option, "description": "" }))
                            .collect();
                        json!({
                            "question": field.title,
                            "header": field.title,
                            "multiple": matches!(field.kind, FormFieldKind::Multiselect),
                            "options": options,
                        })
                    })
                    .collect();
                let event = json!({
                    "type": "question.asked",
                    "properties": {
                        "id": request.id,
                        "sessionID": request.session.as_str(),
                        "questions": questions,
                    },
                });
                self.stream_event(ws_key, event, cx);
                Ok(())
            }
            HarnessEvent::QuestionResolved {
                request, summary, ..
            } => {
                self.resolve_question(&request, summary.as_deref().unwrap_or("dismissed"));
                Ok(())
            }
            HarnessEvent::CommandOutput { session, result } => {
                let result = match result {
                    Ok(text) => Ok(text),
                    Err(error) => Err(anyhow::anyhow!(error)),
                };
                self.on_net_event(
                    UiEvent::CommandExecuted {
                        session_id: session.as_str().to_string(),
                        result,
                    },
                    cx,
                );
                Ok(())
            }
            HarnessEvent::ModelsLoaded { models } => {
                let harness = harness_kind(harness)?;
                let models = models
                    .into_iter()
                    .map(|model| ModelInfo {
                        key: model.key,
                        harness: harness.clone(),
                        variants: model.variants,
                        context_limit: model.context_limit,
                    })
                    .collect();
                self.on_net_event(
                    UiEvent::ModelsLoaded {
                        harness,
                        result: Ok(models),
                    },
                    cx,
                );
                Ok(())
            }
            HarnessEvent::AgentsLoaded { agents } => {
                let _ = harness_kind(harness)?;
                let agents = agents
                    .into_iter()
                    .map(|agent| AgentInfo {
                        name: agent.name,
                        mode: agent.mode,
                        hidden: agent.hidden,
                    })
                    .collect();
                self.on_net_event(UiEvent::AgentsLoaded { result: Ok(agents) }, cx);
                Ok(())
            }
            HarnessEvent::CommandsLoaded {
                directory: catalog_directory,
                commands,
            } => {
                let ws_key =
                    self.harness_ws_key(harness, None, directory.or(catalog_directory.as_deref()))?;
                let mut merged = self.commands.get(&ws_key).cloned().unwrap_or_default();
                merged.retain(|command| command.source != "command");
                merged.extend(commands.into_iter().map(|command| CommandInfo {
                    name_lower: command.name.to_lowercase(),
                    name: command.name,
                    description: command.description,
                    source: "command".into(),
                }));
                self.on_net_event(
                    UiEvent::CommandsLoaded {
                        ws_key,
                        result: Ok(merged),
                    },
                    cx,
                );
                Ok(())
            }
            HarnessEvent::SkillsLoaded {
                directory: catalog_directory,
                skills,
            } => {
                let ws_key =
                    self.harness_ws_key(harness, None, directory.or(catalog_directory.as_deref()))?;
                let mut merged = self.commands.get(&ws_key).cloned().unwrap_or_default();
                merged.retain(|command| command.source != "skill");
                merged.extend(skills.into_iter().map(|skill| CommandInfo {
                    name_lower: skill.name.to_lowercase(),
                    name: skill.name,
                    description: skill.description,
                    source: "skill".into(),
                }));
                self.on_net_event(
                    UiEvent::CommandsLoaded {
                        ws_key,
                        result: Ok(merged),
                    },
                    cx,
                );
                Ok(())
            }
            HarnessEvent::Error {
                session,
                message,
                retryable,
            } => {
                match session {
                    Some(session) => {
                        let ws_key = self.harness_ws_key(harness, Some(&session), directory)?;
                        let event = json!({
                            "type": "session.error",
                            "properties": {
                                "sessionID": session.as_str(),
                                "error": {
                                    "data": {
                                        "message": message,
                                        "isRetryable": retryable,
                                    },
                                },
                            },
                        });
                        self.stream_event(ws_key, event, cx);
                    }
                    // a server-level failure has no session to park it on
                    None => self.announce(&message, true, cx),
                }
                Ok(())
            }
        }
    }

    fn stream_event(&mut self, ws_key: String, event: Value, cx: &mut Context<Self>) {
        self.on_net_event(UiEvent::Event { ws_key, event }, cx);
    }

    /// The net key the legacy store addresses a harness's session with.
    fn harness_ws_key(
        &self,
        harness: &HarnessId,
        session: Option<&SessionId>,
        directory: Option<&Path>,
    ) -> Result<String, HarnessError> {
        let harness = harness_kind(harness)?;
        if crate::harness::shared_server(&harness) {
            return Ok(harness.as_str().to_string());
        }
        if let Some(directory) = directory {
            return Ok(directory.to_string_lossy().into_owned());
        }
        if let Some(session) = session {
            return self
                .workspace_by_session
                .get(session.as_str())
                .cloned()
                .ok_or_else(|| {
                    HarnessError::Unavailable(format!("unknown workspace for session {session}"))
                });
        }
        self.selected_workspace
            .clone()
            .or_else(|| {
                self.workspaces
                    .iter()
                    .find(|workspace| !workspace.folder_path().is_empty())
                    .map(|workspace| workspace.folder_path().to_string())
            })
            .ok_or_else(|| HarnessError::Unavailable("no workspace for the event".into()))
    }
}

fn harness_kind(harness: &HarnessId) -> Result<HarnessId, HarnessError> {
    HarnessId::from_wire(harness.as_str()).ok_or_else(|| {
        HarnessError::Unavailable(format!("harness {} is not registered", harness.as_str()))
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::harness::test_support::FakeHarness;
    use crate::harness::{
        Command, FormField, FormFieldKind, FormRequest, Harness, HarnessError, HarnessEvent,
        HarnessHealth, HarnessId, MessageRole, MessageSnapshot, Model, PartPayload, PartSnapshot,
        PermissionRequest, Prompt, SessionId, SessionState, Skill, TokenUsage, ToolSnapshot,
        ToolState,
    };
    use crate::oc::UiEvent;
    use crate::store::ChatItem;
    use crate::store::test_support::{HomeGuard, lock_home};

    use super::Store;

    fn typed_events(session: &SessionId, folder: &str) -> Vec<HarnessEvent> {
        let directory = PathBuf::from(folder);
        vec![
            HarnessEvent::SessionCreated {
                session: session.clone(),
                title: Some("Typed chat".into()),
                parent: None,
                agent: None,
            },
            HarnessEvent::SessionChanged {
                session: session.clone(),
                title: Some("Typed chat".into()),
                parent: None,
                agent: None,
            },
            HarnessEvent::MessageSnapshot {
                message: MessageSnapshot {
                    id: "m1".into(),
                    session: session.clone(),
                    role: MessageRole::Assistant,
                    created_ms: 1,
                    completed_ms: Some(1000),
                    provider: Some("test".into()),
                    model: Some("model".into()),
                    error: None,
                    tokens: Some(TokenUsage {
                        input: 10,
                        output: 20,
                        reasoning: 0,
                        cache_read: 0,
                        cache_write: 0,
                    }),
                    cost_micros: Some(1000),
                },
            },
            HarnessEvent::PartSnapshot {
                part: PartSnapshot {
                    id: "part1".into(),
                    session: session.clone(),
                    message_id: "m1".into(),
                    payload: PartPayload::Text("hello from the fake".into()),
                },
            },
            // a second snapshot of the same part: the transcript replaces the
            // text in place, the shape a streaming adapter emits
            HarnessEvent::PartSnapshot {
                part: PartSnapshot {
                    id: "part1".into(),
                    session: session.clone(),
                    message_id: "m1".into(),
                    payload: PartPayload::Text("hello from the fake, streamed".into()),
                },
            },
            HarnessEvent::PartSnapshot {
                part: PartSnapshot {
                    id: "tool1".into(),
                    session: session.clone(),
                    message_id: "m1".into(),
                    payload: PartPayload::Tool(ToolSnapshot {
                        name: "bash".into(),
                        state: ToolState::Completed {
                            output: "ok".into(),
                        },
                        input: Some(serde_json::json!({ "command": "ls" })),
                        title: Some("bash".into()),
                        metadata: Some(serde_json::json!({
                            "filediff": {
                                "patch": "diff --git a/x b/x",
                                "additions": 1,
                                "deletions": 0,
                            },
                        })),
                    }),
                },
            },
            HarnessEvent::QuestionAsked {
                request: FormRequest {
                    id: "q1".into(),
                    session: session.clone(),
                    title: "Pick".into(),
                    fields: vec![FormField {
                        id: "choice".into(),
                        title: "Which?".into(),
                        kind: FormFieldKind::Multiselect,
                        required: true,
                        options: vec!["a".into(), "b".into()],
                    }],
                },
            },
            HarnessEvent::PermissionAsked {
                request: PermissionRequest {
                    id: "perm1".into(),
                    session: session.clone(),
                    action: "bash".into(),
                    resources: vec!["ls".into()],
                    message: None,
                    diff: None,
                },
            },
            HarnessEvent::ModelsLoaded {
                models: vec![Model {
                    key: "test/model".into(),
                    provider: "test".into(),
                    variants: vec!["low".into()],
                    context_limit: 1000,
                }],
            },
            HarnessEvent::CommandsLoaded {
                directory: Some(directory.clone()),
                commands: vec![Command {
                    name: "typed-command".into(),
                    description: "typed".into(),
                }],
            },
            HarnessEvent::SkillsLoaded {
                directory: Some(directory),
                skills: vec![Skill {
                    id: "skill-1".into(),
                    name: "typed-skill".into(),
                    description: "typed".into(),
                }],
            },
            HarnessEvent::Todos {
                session: session.clone(),
                todos: vec![crate::harness::Todo {
                    content: "ship the typed path".into(),
                    status: "in_progress".into(),
                    priority: Some("high".into()),
                }],
            },
            HarnessEvent::SessionStatus {
                session: session.clone(),
                state: SessionState::Idle,
            },
        ]
    }

    #[gpui_kit::gpui::test]
    fn the_instance_directory_resolves_an_unknown_session(cx: &mut gpui::TestAppContext) {
        let _lock = lock_home();
        let home = HomeGuard::new();
        let folder = home.home.join("workspace");
        std::fs::create_dir_all(&folder).expect("workspace");
        let folder = folder.to_string_lossy().to_string();

        let store = cx.update(Store::for_probe);
        store.update(cx, |store, cx| {
            store.add_workspace(&folder, cx);
            store.set_harness_ingest(true);
        });

        let harness = HarnessId::new("opencode");
        let event = || HarnessEvent::SessionChanged {
            session: SessionId::new("unmapped"),
            title: Some("Unknown chat".into()),
            parent: None,
            agent: None,
        };
        // without the instance's folder the store cannot address the chat ...
        store.update(cx, |store, cx| {
            assert!(store.ingest_harness_event(&harness, event(), cx).is_err());
        });
        // ... and the net thread's tagged event carries it
        store.update(cx, |store, cx| {
            store
                .ingest_harness_event_from(
                    &harness,
                    event(),
                    Some(std::path::Path::new(&folder)),
                    cx,
                )
                .expect("the directory addresses the event");
        });
    }

    #[gpui_kit::gpui::test]
    fn a_fake_harness_drives_a_chat_through_typed_events(cx: &mut gpui::TestAppContext) {
        let _lock = lock_home();
        let home = HomeGuard::new();
        let folder = home.home.join("workspace");
        std::fs::create_dir_all(&folder).expect("workspace");
        let folder = folder.to_string_lossy().to_string();

        let store = cx.update(Store::for_probe);
        let (key, other_key) = cx.update(|cx| {
            store.update(cx, |store, cx| {
                store.add_workspace(&folder, cx);
                let key = store.seed_probe_tile(&folder, "s1", "Probe", Vec::new(), true, cx);
                let other = store.seed_probe_tile(&folder, "s2", "Other", Vec::new(), false, cx);
                (key, other)
            })
        });

        // every adapter's stream feeds the typed path from startup; the probe
        // seam can still silence it, and says so with a typed failure
        store.update(cx, |store, _| store.set_harness_ingest(false));
        let off = store.update(cx, |store, cx| {
            store
                .ingest_harness_event(
                    &HarnessId::new("opencode"),
                    HarnessEvent::Health(HarnessHealth::ready()),
                    cx,
                )
                .unwrap_err()
        });
        assert!(matches!(off, HarnessError::Unavailable(_)));
        store.update(cx, |store, _| store.set_harness_ingest(true));

        let session = SessionId::new("s1");
        let mut fake = FakeHarness::with_script("opencode", typed_events(&session, &folder));
        let rx = fake.events();
        fake.send_prompt(&session, Prompt::default())
            .expect("the fake accepts the prompt");

        store.update(cx, |store, cx| {
            while let Ok(event) = rx.try_recv() {
                store
                    .ingest_harness_event(&HarnessId::new("opencode"), event, cx)
                    .expect("typed event ingests");
            }
        });

        store.read_with(cx, |store, _| {
            let tile = store.tile_by_key(&key).expect("the tile is open");
            assert_eq!(tile.title, "Typed chat");
            assert!(tile.items.iter().any(|item| matches!(
                item.as_ref(),
                ChatItem::AssistantText { text, meta_label, .. }
                    if text == "hello from the fake, streamed" && meta_label.contains("↑10")
            )));
            assert!(tile.items.iter().any(|item| matches!(
                item.as_ref(),
                ChatItem::Tool(tool)
                    if tool.tool == "bash" && tool.title == "bash" && tool.diff.is_some()
            )));
            assert!(tile.items.iter().any(|item| matches!(
                item.as_ref(),
                ChatItem::Permission(permission) if permission.id == "perm1"
            )));
            assert!(tile.items.iter().any(|item| matches!(
                item.as_ref(),
                ChatItem::Question(question) if question.id == "q1"
            )));
            assert!(
                tile.tasks
                    .iter()
                    .any(|task| task.content == "ship the typed path"
                        && task.status == "in_progress")
            );
            assert!(!store.running_by_session.contains("s1"));

            assert!(
                store
                    .model_keys_for(&crate::harness::workspace_harness().unwrap_or_default())
                    .contains(&"test/model".to_string())
            );
            let commands = store.commands.get(&folder).expect("the catalog landed");
            assert!(
                commands
                    .iter()
                    .any(|command| command.name == "typed-command" && command.source == "command")
            );
            assert!(
                commands
                    .iter()
                    .any(|command| command.name == "typed-skill" && command.source == "skill")
            );

            // an event for s1 must not touch another session's transcript
            let other = store.tile_by_key(&other_key).expect("the other tile");
            assert!(other.items.is_empty());
        });

        // the old net path still feeds the same store alongside the typed path
        store.update(cx, |store, cx| {
            store.on_net_event(
                UiEvent::Event {
                    ws_key: folder.clone(),
                    event: serde_json::json!({
                        "type": "message.part.updated",
                        "properties": {
                            "sessionID": "s1",
                            "part": {
                                "id": "old1",
                                "sessionID": "s1",
                                "messageID": "m1",
                                "type": "text",
                                "text": "old path still works",
                            },
                        },
                    }),
                },
                cx,
            );
        });
        store.read_with(cx, |store, _| {
            let tile = store.tile_by_key(&key).expect("the tile is open");
            assert!(tile.items.iter().any(|item| matches!(
                item.as_ref(),
                ChatItem::AssistantText { text, .. } if text == "old path still works"
            )));
        });
    }
}
