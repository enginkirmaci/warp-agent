//! Test doubles for the harness seam.
//!
//! Unit tests and integration probes use the fake to drive the store and the
//! views without a network or a child process.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_channel::{Receiver, Sender, unbounded};

use super::interface::Harness;
use super::types::*;

/// A harness that records lifecycle calls, can be scripted with events and
/// answers every unsupported operation with a typed `Unsupported`.
///
/// Catalogs, sessions and a transcript can be seeded so the net thread's
/// request dispatch is testable without a protocol.
#[derive(Default)]
pub struct FakeHarness {
    id: HarnessId,
    tx: Option<Sender<HarnessEvent>>,
    rx: Option<Receiver<HarnessEvent>>,
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    script: Vec<HarnessEvent>,
    sessions: Vec<SessionSummary>,
    models: Vec<Model>,
    commands: Vec<Command>,
    skills: Vec<Skill>,
    transcript: Vec<HistoryMessage>,
    mcp: Vec<McpServer>,
    files: Vec<String>,
    /// Every prompt the fake accepted, in order.
    pub prompts: Arc<std::sync::Mutex<Vec<Prompt>>>,
    /// Every command the fake accepted, in order.
    pub commands_run: Arc<std::sync::Mutex<Vec<CommandRun>>>,
}

impl FakeHarness {
    pub fn new(id: &str) -> Self {
        let mut harness = Self {
            id: HarnessId::new(id),
            ..Self::default()
        };
        harness.install_channel();
        harness
    }

    pub fn with_flags(id: &str, started: Arc<AtomicBool>, stopped: Arc<AtomicBool>) -> Self {
        let mut harness = Self::new(id);
        harness.started = started;
        harness.stopped = stopped;
        harness
    }

    /// A fake whose `send_prompt` emits these events in order.
    pub fn with_script(id: &str, script: Vec<HarnessEvent>) -> Self {
        Self::new(id).scripted(script)
    }

    /// Make the fake emit these events when it accepts a prompt.
    pub fn scripted(mut self, script: Vec<HarnessEvent>) -> Self {
        self.script = script;
        self
    }

    pub fn with_sessions(id: &str, sessions: Vec<SessionSummary>) -> Self {
        let mut fake = Self::new(id);
        fake.sessions = sessions;
        fake
    }

    pub fn with_models(id: &str, models: Vec<Model>) -> Self {
        let mut fake = Self::new(id);
        fake.models = models;
        fake
    }

    pub fn with_commands(id: &str, commands: Vec<Command>, skills: Vec<Skill>) -> Self {
        let mut fake = Self::new(id);
        fake.commands = commands;
        fake.skills = skills;
        fake
    }

    pub fn with_transcript(id: &str, transcript: Vec<HistoryMessage>) -> Self {
        let mut fake = Self::new(id);
        fake.transcript = transcript;
        fake
    }

    pub fn with_mcp(id: &str, mcp: Vec<McpServer>) -> Self {
        let mut fake = Self::new(id);
        fake.mcp = mcp;
        fake
    }

    pub fn with_files(id: &str, files: Vec<String>) -> Self {
        let mut fake = Self::new(id);
        fake.files = files;
        fake
    }

    pub fn with_created_session(id: &str, session: SessionSummary) -> Self {
        let mut fake = Self::new(id);
        fake.sessions = vec![session];
        fake
    }

    fn install_channel(&mut self) {
        if self.rx.is_none() {
            let (tx, rx) = unbounded();
            self.tx = Some(tx);
            self.rx = Some(rx);
        }
    }
}

macro_rules! impl_fake_harness {
    ($($name:ident($($arg:ident: $ty:ty),* $(,)?) -> $ret:ty;)*) => {
        impl Harness for FakeHarness {
            fn id(&self) -> HarnessId {
                self.id.clone()
            }

            fn health(&self) -> HarnessHealth {
                HarnessHealth::ready()
            }

            fn start(&mut self) -> Result<(), HarnessError> {
                self.started.store(true, Ordering::SeqCst);
                Ok(())
            }

            fn stop(&mut self) -> Result<(), HarnessError> {
                self.stopped.store(true, Ordering::SeqCst);
                Ok(())
            }

            fn events(&self) -> Receiver<HarnessEvent> {
                self.rx
                    .clone()
                    .expect("a fake always has an event channel")
            }

            fn sessions(&mut self) -> Result<Vec<SessionSummary>, HarnessError> {
                Ok(self.sessions.clone())
            }

            fn models(&mut self) -> Result<Vec<Model>, HarnessError> {
                Ok(self.models.clone())
            }

            fn commands(
                &mut self,
                _session: Option<&SessionId>,
            ) -> Result<Vec<Command>, HarnessError> {
                Ok(self.commands.clone())
            }

            fn skills(
                &mut self,
                _session: Option<&SessionId>,
            ) -> Result<Vec<Skill>, HarnessError> {
                Ok(self.skills.clone())
            }

            fn transcript(
                &mut self,
                _session: &SessionId,
            ) -> Result<Vec<HistoryMessage>, HarnessError> {
                Ok(self.transcript.clone())
            }

            fn create_session(
                &mut self,
                spec: NewSession,
            ) -> Result<SessionSummary, HarnessError> {
                let mut summary = self
                    .sessions
                    .first()
                    .cloned()
                    .ok_or(HarnessError::Unsupported("create_session"))?;
                if let Some(title) = spec.title {
                    summary.title = title;
                }
                if spec.directory.is_some() {
                    summary.directory = spec.directory;
                }
                Ok(summary)
            }

            fn find_files(&mut self, _query: &str) -> Result<Vec<String>, HarnessError> {
                Ok(self.files.clone())
            }

            fn forget_session(&mut self, _session: &SessionId) -> Result<(), HarnessError> {
                Ok(())
            }

            fn mcp_servers(&mut self) -> Result<Vec<McpServer>, HarnessError> {
                Ok(self.mcp.clone())
            }

            fn send_prompt(
                &mut self,
                _session: &SessionId,
                prompt: Prompt,
            ) -> Result<(), HarnessError> {
                self.prompts
                    .lock()
                    .expect("recorded prompts")
                    .push(prompt);
                if let Some(tx) = &self.tx {
                    for event in self.script.drain(..) {
                        let _ = tx.try_send(event);
                    }
                }
                Ok(())
            }

            fn run_command(
                &mut self,
                _session: &SessionId,
                command: CommandRun,
            ) -> Result<(), HarnessError> {
                self.commands_run
                    .lock()
                    .expect("recorded commands")
                    .push(command);
                Ok(())
            }

            fn reply_form(
                &mut self,
                _session: &SessionId,
                _form: &str,
                _answer: FormAnswer,
            ) -> Result<(), HarnessError> {
                Ok(())
            }

            $(
                fn $name(&mut self, $($arg: $ty),*) -> Result<$ret, HarnessError> {
                    let _ = ($($arg),*);
                    Err(HarnessError::Unsupported(stringify!($name)))
                }
            )*
        }
    };
}

impl_fake_harness! {
    providers() -> Vec<Provider>;
    agents() -> Vec<Agent>;
    session(session: &SessionId) -> SessionSummary;
    fork_session(session: &SessionId, before: Option<&str>) -> SessionSummary;
    rename_session(session: &SessionId, title: &str) -> ();
    delete_session(session: &SessionId) -> ();
    interrupt(session: &SessionId) -> ();
    compact(session: &SessionId) -> ();
    activate_skill(session: &SessionId, skill: &str) -> ();
    permissions(session: &SessionId) -> Vec<PermissionRequest>;
    reply_permission(session: &SessionId, request: &str, decision: PermissionDecision) -> ();
    forms(session: &SessionId) -> Vec<FormRequest>;
    diff(session: &SessionId) -> Vec<FileDiff>;
    inbox(session: &SessionId) -> Vec<InboxItem>;
    update_inbox(session: &SessionId, item: &str, delivery: InboxDelivery) -> ();
    cancel_inbox(session: &SessionId, item: &str) -> ();
    stage_revert(session: &SessionId, before: &str) -> ();
    commit_revert(session: &SessionId) -> ();
    clear_revert(session: &SessionId) -> ();
    run_shell(session: &SessionId, command: &str) -> ();
    background_session(session: &SessionId) -> ();
    add_mcp(config: McpConfig) -> ();
    connect_mcp(name: &str) -> ();
    disconnect_mcp(name: &str) -> ();
    remove_mcp(name: &str) -> ();
}

/// A harness whose whole trait surface answers with canned typed values and
/// records what it received. The door dispatch tests drive it to prove one
/// mapping per category without a network or a child process.
pub struct SurfaceHarness {
    id: HarnessId,
    rx: Receiver<HarnessEvent>,
    /// Every trait method this fake was asked for, in call order.
    pub calls: Arc<std::sync::Mutex<Vec<String>>>,
    /// Every prompt the fake accepted, in order.
    pub prompts: Arc<std::sync::Mutex<Vec<Prompt>>>,
    /// Every session spec the fake created, in order.
    pub new_sessions: Arc<std::sync::Mutex<Vec<NewSession>>>,
    /// Every slash command the fake ran, in order.
    pub commands_run: Arc<std::sync::Mutex<Vec<CommandRun>>>,
}

impl SurfaceHarness {
    pub fn new(id: &str) -> Self {
        // the fake never emits: its dispatch answers are the typed results
        let (_tx, rx) = unbounded();
        Self {
            id: HarnessId::new(id),
            rx,
            calls: Arc::default(),
            prompts: Arc::default(),
            new_sessions: Arc::default(),
            commands_run: Arc::default(),
        }
    }

    /// The last prompt this fake accepted.
    pub fn last_prompt(&self) -> Option<Prompt> {
        self.prompts
            .lock()
            .expect("recorded prompts")
            .last()
            .cloned()
    }

    /// The last session spec this fake created.
    pub fn last_new_session(&self) -> Option<NewSession> {
        self.new_sessions
            .lock()
            .expect("recorded session specs")
            .last()
            .cloned()
    }

    fn record(&self, call: impl Into<String>) {
        self.calls.lock().expect("recorded calls").push(call.into());
    }

    fn summary(&self, id: &str) -> SessionSummary {
        SessionSummary {
            id: SessionId::new(id),
            title: "Session".into(),
            parent: None,
            directory: None,
            updated_ms: 10,
            created_ms: 5,
        }
    }
}

impl Harness for SurfaceHarness {
    fn id(&self) -> HarnessId {
        self.id.clone()
    }

    fn health(&self) -> HarnessHealth {
        HarnessHealth::ready()
    }

    fn start(&mut self) -> Result<(), HarnessError> {
        self.record("start");
        Ok(())
    }

    fn stop(&mut self) -> Result<(), HarnessError> {
        self.record("stop");
        Ok(())
    }

    fn events(&self) -> Receiver<HarnessEvent> {
        self.rx.clone()
    }

    fn providers(&mut self) -> Result<Vec<Provider>, HarnessError> {
        self.record("providers");
        Ok(vec![Provider {
            id: "provider-1".into(),
            name: "Provider One".into(),
        }])
    }

    fn models(&mut self) -> Result<Vec<Model>, HarnessError> {
        self.record("models");
        Ok(vec![Model {
            key: "provider-1/model-1".into(),
            provider: "provider-1".into(),
            variants: vec!["low".into(), "high".into()],
            context_limit: 1000,
        }])
    }

    fn agents(&mut self) -> Result<Vec<Agent>, HarnessError> {
        self.record("agents");
        Ok(vec![Agent {
            name: "build".into(),
            mode: "primary".into(),
            hidden: false,
        }])
    }

    fn commands(&mut self, session: Option<&SessionId>) -> Result<Vec<Command>, HarnessError> {
        self.record(format!("commands:{}", session.is_some()));
        Ok(vec![Command {
            name: "plan".into(),
            description: "draft a plan".into(),
        }])
    }

    fn skills(&mut self, session: Option<&SessionId>) -> Result<Vec<Skill>, HarnessError> {
        self.record(format!("skills:{}", session.is_some()));
        Ok(vec![Skill {
            id: "review".into(),
            name: "Review".into(),
            description: "review a change".into(),
        }])
    }

    fn sessions(&mut self) -> Result<Vec<SessionSummary>, HarnessError> {
        self.record("sessions");
        Ok(vec![self.summary("session-1")])
    }

    fn session(&mut self, session: &SessionId) -> Result<SessionSummary, HarnessError> {
        self.record("session");
        Ok(self.summary(session.as_str()))
    }

    fn transcript(&mut self, session: &SessionId) -> Result<Vec<HistoryMessage>, HarnessError> {
        self.record("transcript");
        let message = MessageSnapshot {
            id: "message-1".into(),
            session: session.clone(),
            role: MessageRole::Assistant,
            created_ms: 5,
            completed_ms: Some(9),
            provider: Some("provider-1".into()),
            model: Some("model-1".into()),
            error: None,
            tokens: None,
            cost_micros: None,
        };
        Ok(vec![HistoryMessage {
            message,
            parts: vec![HistoryPart {
                id: "part-1".into(),
                payload: PartPayload::Text("hello".into()),
            }],
        }])
    }

    fn create_session(&mut self, spec: NewSession) -> Result<SessionSummary, HarnessError> {
        self.record("create_session");
        let mut summary = self.summary("session-1");
        if let Some(title) = spec.title.clone() {
            summary.title = title;
        }
        summary.directory = spec.directory.clone();
        self.new_sessions
            .lock()
            .expect("recorded session specs")
            .push(spec);
        Ok(summary)
    }

    fn fork_session(
        &mut self,
        session: &SessionId,
        before: Option<&str>,
    ) -> Result<SessionSummary, HarnessError> {
        self.record(format!("fork_session:{}", before.unwrap_or_default()));
        Ok(self.summary(session.as_str()))
    }

    fn rename_session(&mut self, session: &SessionId, title: &str) -> Result<(), HarnessError> {
        self.record(format!("rename_session:{}:{title}", session.as_str()));
        Ok(())
    }

    fn delete_session(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        self.record(format!("delete_session:{}", session.as_str()));
        Ok(())
    }

    fn send_prompt(&mut self, session: &SessionId, prompt: Prompt) -> Result<(), HarnessError> {
        self.record(format!("send_prompt:{}", session.as_str()));
        self.prompts.lock().expect("recorded prompts").push(prompt);
        Ok(())
    }

    fn interrupt(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        self.record(format!("interrupt:{}", session.as_str()));
        Ok(())
    }

    fn compact(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        self.record(format!("compact:{}", session.as_str()));
        Ok(())
    }

    fn run_command(
        &mut self,
        session: &SessionId,
        command: CommandRun,
    ) -> Result<(), HarnessError> {
        self.record(format!("run_command:{}", session.as_str()));
        self.commands_run
            .lock()
            .expect("recorded commands")
            .push(command);
        Ok(())
    }

    fn activate_skill(&mut self, session: &SessionId, skill: &str) -> Result<(), HarnessError> {
        self.record(format!("activate_skill:{}:{skill}", session.as_str()));
        Ok(())
    }

    fn permissions(&mut self, session: &SessionId) -> Result<Vec<PermissionRequest>, HarnessError> {
        self.record("permissions");
        Ok(vec![PermissionRequest {
            id: "permission-1".into(),
            session: session.clone(),
            action: "bash".into(),
            resources: vec!["ls -la".into()],
            message: None,
            diff: None,
        }])
    }

    fn reply_permission(
        &mut self,
        session: &SessionId,
        request: &str,
        decision: PermissionDecision,
    ) -> Result<(), HarnessError> {
        self.record(format!(
            "reply_permission:{}:{request}:{decision:?}",
            session.as_str()
        ));
        Ok(())
    }

    fn forms(&mut self, session: &SessionId) -> Result<Vec<FormRequest>, HarnessError> {
        self.record("forms");
        Ok(vec![FormRequest {
            id: "form-1".into(),
            session: session.clone(),
            title: "Proceed?".into(),
            fields: vec![FormField {
                id: "0".into(),
                title: "Continue?".into(),
                kind: FormFieldKind::Text,
                required: true,
                options: Vec::new(),
            }],
        }])
    }

    fn reply_form(
        &mut self,
        session: &SessionId,
        form: &str,
        answer: FormAnswer,
    ) -> Result<(), HarnessError> {
        self.record(format!(
            "reply_form:{}:{form}:{}",
            session.as_str(),
            answer.values.len()
        ));
        Ok(())
    }

    fn diff(&mut self, session: &SessionId) -> Result<Vec<FileDiff>, HarnessError> {
        self.record(format!("diff:{}", session.as_str()));
        Ok(vec![FileDiff {
            path: "src/main.rs".into(),
            patch: "@@ -1 +1 @@".into(),
        }])
    }

    fn find_files(&mut self, query: &str) -> Result<Vec<String>, HarnessError> {
        self.record(format!("find_files:{query}"));
        Ok(vec!["src/main.rs".into()])
    }

    fn forget_session(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        self.record(format!("forget_session:{}", session.as_str()));
        Ok(())
    }

    fn inbox(&mut self, session: &SessionId) -> Result<Vec<InboxItem>, HarnessError> {
        self.record("inbox");
        Ok(vec![InboxItem {
            id: "item-1".into(),
            session: session.clone(),
            kind: InboxKind::User,
            delivery: InboxDelivery::Queue,
        }])
    }

    fn update_inbox(
        &mut self,
        session: &SessionId,
        item: &str,
        delivery: InboxDelivery,
    ) -> Result<(), HarnessError> {
        self.record(format!(
            "update_inbox:{}:{item}:{delivery:?}",
            session.as_str()
        ));
        Ok(())
    }

    fn cancel_inbox(&mut self, session: &SessionId, item: &str) -> Result<(), HarnessError> {
        self.record(format!("cancel_inbox:{}:{item}", session.as_str()));
        Ok(())
    }

    fn stage_revert(&mut self, session: &SessionId, before: &str) -> Result<(), HarnessError> {
        self.record(format!("stage_revert:{}:{before}", session.as_str()));
        Ok(())
    }

    fn commit_revert(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        self.record(format!("commit_revert:{}", session.as_str()));
        Ok(())
    }

    fn clear_revert(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        self.record(format!("clear_revert:{}", session.as_str()));
        Ok(())
    }

    fn run_shell(&mut self, session: &SessionId, command: &str) -> Result<(), HarnessError> {
        self.record(format!("run_shell:{}:{command}", session.as_str()));
        Ok(())
    }

    fn background_session(&mut self, session: &SessionId) -> Result<(), HarnessError> {
        self.record(format!("background_session:{}", session.as_str()));
        Ok(())
    }

    fn mcp_servers(&mut self) -> Result<Vec<McpServer>, HarnessError> {
        self.record("mcp_servers");
        Ok(vec![McpServer {
            name: "docs".into(),
            status: McpStatus::Connected,
        }])
    }

    fn add_mcp(&mut self, config: McpConfig) -> Result<(), HarnessError> {
        self.record(format!("add_mcp:{}", config.name));
        Ok(())
    }

    fn connect_mcp(&mut self, name: &str) -> Result<(), HarnessError> {
        self.record(format!("connect_mcp:{name}"));
        Ok(())
    }

    fn disconnect_mcp(&mut self, name: &str) -> Result<(), HarnessError> {
        self.record(format!("disconnect_mcp:{name}"));
        Ok(())
    }

    fn remove_mcp(&mut self, name: &str) -> Result<(), HarnessError> {
        self.record(format!("remove_mcp:{name}"));
        Ok(())
    }
}
