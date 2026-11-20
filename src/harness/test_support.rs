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
pub struct FakeHarness {
    id: HarnessId,
    tx: Sender<HarnessEvent>,
    rx: Receiver<HarnessEvent>,
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    script: Vec<HarnessEvent>,
}

impl FakeHarness {
    pub fn new(id: &str) -> Self {
        Self::with_flags(
            id,
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        )
    }

    pub fn with_flags(id: &str, started: Arc<AtomicBool>, stopped: Arc<AtomicBool>) -> Self {
        let (tx, rx) = unbounded();
        Self {
            id: HarnessId::new(id),
            tx,
            rx,
            started,
            stopped,
            script: Vec::new(),
        }
    }

    /// A fake whose `send_prompt` emits these events in order.
    pub fn with_script(id: &str, script: Vec<HarnessEvent>) -> Self {
        let mut fake = Self::new(id);
        fake.script = script;
        fake
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
                self.rx.clone()
            }

            fn send_prompt(
                &mut self,
                _session: &SessionId,
                _prompt: Prompt,
            ) -> Result<(), HarnessError> {
                for event in self.script.drain(..) {
                    let _ = self.tx.try_send(event);
                }
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
    models() -> Vec<Model>;
    agents() -> Vec<Agent>;
    commands() -> Vec<Command>;
    skills() -> Vec<Skill>;
    sessions() -> Vec<SessionSummary>;
    session(session: &SessionId) -> SessionSummary;
    create_session(spec: NewSession) -> SessionSummary;
    fork_session(session: &SessionId, before: Option<&str>) -> SessionSummary;
    rename_session(session: &SessionId, title: &str) -> ();
    delete_session(session: &SessionId) -> ();
    interrupt(session: &SessionId) -> ();
    compact(session: &SessionId) -> ();
    run_command(session: &SessionId, command: CommandRun) -> ();
    activate_skill(session: &SessionId, skill: &str) -> ();
    permissions(session: &SessionId) -> Vec<PermissionRequest>;
    reply_permission(session: &SessionId, request: &str, decision: PermissionDecision) -> ();
    forms(session: &SessionId) -> Vec<FormRequest>;
    reply_form(session: &SessionId, form: &str, answer: FormAnswer) -> ();
    diff(session: &SessionId) -> Vec<FileDiff>;
    inbox(session: &SessionId) -> Vec<InboxItem>;
    update_inbox(session: &SessionId, item: &str, delivery: InboxDelivery) -> ();
    cancel_inbox(session: &SessionId, item: &str) -> ();
    stage_revert(session: &SessionId, before: &str) -> ();
    commit_revert(session: &SessionId) -> ();
    clear_revert(session: &SessionId) -> ();
    run_shell(session: &SessionId, command: &str) -> ();
    background_session(session: &SessionId) -> ();
    mcp_servers() -> Vec<McpServer>;
    add_mcp(config: McpConfig) -> ();
    connect_mcp(name: &str) -> ();
    disconnect_mcp(name: &str) -> ();
    remove_mcp(name: &str) -> ();
}
