//! The app-level harness contract.
//!
//! One method per app-level operation, not one per HTTP endpoint: the adapter
//! owns the protocol mapping. Every adapter implements every method; an
//! unsupported operation returns [`HarnessError::Unsupported`]. The capability
//! descriptor says which of them the UI should offer.

use async_channel::Receiver;
use std::path::PathBuf;

use super::types::{
    Agent, Command, CommandRun, FileDiff, FormAnswer, FormRequest, HarnessError, HarnessEvent,
    HarnessHealth, HarnessId, InboxDelivery, InboxItem, McpConfig, McpServer, Model, NewSession,
    PermissionDecision, PermissionRequest, Prompt, Provider, SessionId, SessionSummary, Skill,
};

/// One harness instance: its lifecycle, its catalogs, its sessions and turns,
/// its approvals and its one event stream.
///
/// Every method is called on the app's net thread: it may block on the
/// harness's I/O, and it must never run on the GPUI thread. The store and the
/// UI reach the host, not an adapter directly.
pub trait Harness: Send + Sync {
    /// The stable id this instance was created for.
    fn id(&self) -> HarnessId;

    /// The workspace this instance serves, for a per-workspace harness.
    fn scope(&self) -> Option<PathBuf> {
        None
    }

    /// The last known health state; cheap, never blocks on the network.
    fn health(&self) -> HarnessHealth;

    /// Start the managed process or attach to the shared service.
    fn start(&mut self) -> Result<(), HarnessError>;

    /// Stop the managed process, or disconnect from the attached service.
    fn stop(&mut self) -> Result<(), HarnessError>;

    /// The provider catalog, when the harness exposes one separately.
    fn providers(&mut self) -> Result<Vec<Provider>, HarnessError>;

    /// The model catalog with each model's variants.
    fn models(&mut self) -> Result<Vec<Model>, HarnessError>;

    /// The agent list.
    fn agents(&mut self) -> Result<Vec<Agent>, HarnessError>;

    /// The slash-command list.
    fn commands(&mut self) -> Result<Vec<Command>, HarnessError>;

    /// The skill list.
    fn skills(&mut self) -> Result<Vec<Skill>, HarnessError>;

    /// The session list.
    fn sessions(&mut self) -> Result<Vec<SessionSummary>, HarnessError>;

    /// One session's summary.
    fn session(&mut self, session: &SessionId) -> Result<SessionSummary, HarnessError>;

    /// Create a session, optionally as a child of another.
    fn create_session(&mut self, spec: NewSession) -> Result<SessionSummary, HarnessError>;

    /// Fork a session, optionally before a message.
    fn fork_session(
        &mut self,
        session: &SessionId,
        before: Option<&str>,
    ) -> Result<SessionSummary, HarnessError>;

    /// Rename a session.
    fn rename_session(&mut self, session: &SessionId, title: &str) -> Result<(), HarnessError>;

    /// Delete a session and its children.
    fn delete_session(&mut self, session: &SessionId) -> Result<(), HarnessError>;

    /// Send one prompt to a session.
    fn send_prompt(&mut self, session: &SessionId, prompt: Prompt) -> Result<(), HarnessError>;

    /// Interrupt the session's running turn.
    fn interrupt(&mut self, session: &SessionId) -> Result<(), HarnessError>;

    /// Compact the session's context.
    fn compact(&mut self, session: &SessionId) -> Result<(), HarnessError>;

    /// Run a slash command in a session.
    fn run_command(&mut self, session: &SessionId, command: CommandRun)
    -> Result<(), HarnessError>;

    /// Activate a skill for a session.
    fn activate_skill(&mut self, session: &SessionId, skill: &str) -> Result<(), HarnessError>;

    /// The pending permission requests for a session.
    fn permissions(&mut self, session: &SessionId) -> Result<Vec<PermissionRequest>, HarnessError>;

    /// Answer one permission request.
    fn reply_permission(
        &mut self,
        session: &SessionId,
        request: &str,
        decision: PermissionDecision,
    ) -> Result<(), HarnessError>;

    /// The pending forms (structured questions) for a session.
    fn forms(&mut self, session: &SessionId) -> Result<Vec<FormRequest>, HarnessError>;

    /// Answer one form.
    fn reply_form(
        &mut self,
        session: &SessionId,
        form: &str,
        answer: FormAnswer,
    ) -> Result<(), HarnessError>;

    /// The session's file diffs.
    fn diff(&mut self, session: &SessionId) -> Result<Vec<FileDiff>, HarnessError>;

    /// The session's queued inbox items.
    fn inbox(&mut self, session: &SessionId) -> Result<Vec<InboxItem>, HarnessError>;

    /// Change an inbox item's delivery mode.
    fn update_inbox(
        &mut self,
        session: &SessionId,
        item: &str,
        delivery: InboxDelivery,
    ) -> Result<(), HarnessError>;

    /// Cancel an inbox item that has not been delivered yet.
    fn cancel_inbox(&mut self, session: &SessionId, item: &str) -> Result<(), HarnessError>;

    /// Stage a revert before a message.
    fn stage_revert(&mut self, session: &SessionId, before: &str) -> Result<(), HarnessError>;

    /// Commit a staged revert.
    fn commit_revert(&mut self, session: &SessionId) -> Result<(), HarnessError>;

    /// Clear a staged revert.
    fn clear_revert(&mut self, session: &SessionId) -> Result<(), HarnessError>;

    /// Run one shell command in the session's directory.
    fn run_shell(&mut self, session: &SessionId, command: &str) -> Result<(), HarnessError>;

    /// Keep the session running in the background.
    fn background_session(&mut self, session: &SessionId) -> Result<(), HarnessError>;

    /// The MCP server list.
    fn mcp_servers(&mut self) -> Result<Vec<McpServer>, HarnessError>;

    /// Add an MCP server.
    fn add_mcp(&mut self, config: McpConfig) -> Result<(), HarnessError>;

    /// Connect a known MCP server.
    fn connect_mcp(&mut self, name: &str) -> Result<(), HarnessError>;

    /// Disconnect a known MCP server.
    fn disconnect_mcp(&mut self, name: &str) -> Result<(), HarnessError>;

    /// Remove an MCP server.
    fn remove_mcp(&mut self, name: &str) -> Result<(), HarnessError>;

    /// The instance's one event stream.
    fn events(&self) -> Receiver<HarnessEvent>;
}
