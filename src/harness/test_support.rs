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
