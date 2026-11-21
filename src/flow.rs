//! warp-agent's half of warp-flow's door: the Flow surface's client, with no
//! UI in it.
//!
//! warp-flow publishes its own loopback door (`~/.warp/flow/ipc.port` +
//! `ipc.token`, token-authenticated NDJSON, protocol 1) and warp-suite's
//! `README.md#cross-app-contracts` is the contract. This module speaks it and
//! nothing else: the frames are [ADR-0012]'s, the verbs are [ADR-0012]'s, and
//! starting a run has no verb there on purpose — starting stays warp-flow's own
//! action.
//!
//! ## The two connections
//!
//! One request/response connection carries `runs`, `run`, `answer`, `pause`,
//! `resume` and `abandon`; one subscribed connection carries `subscribe` and
//! then nothing but pushes (the door reads no more requests on a subscribed
//! connection). Pushes are live only, so the board is polled: `runs` is
//! pull-only in the contract and `{"push": "board"}` is a contentless "ask
//! again" notification. A `{"push": "run"}` carries the whole host run, so
//! phase and log land between polls.
//!
//! ## Where it runs
//!
//! The driver is spawned on the net thread's tokio runtime (`crate::oc`), never
//! on a second runtime and never on the UI thread. It reports through the
//! app's one event pump as [`FlowEvent`]s; commands arrive over an
//! `async_channel` the store owns ([`FlowSender`]). No file read, no connect
//! and no socket wait happens in a render path: the render reads what the
//! driver already put in the store.
//!
//! ## Absent is a first-class answer
//!
//! No endpoint files, a refused connection, an auth refusal and a door that
//! closes all mean "warp-flow is not running" ([`Absent`]). The driver never
//! panics and never gives up: it re-reads the endpoint files (the token is
//! regenerated every launch) on a slow timer and reconnects, so warp-flow may
//! start after warp-agent, close, and come back.
//!
//! ## What is deserialized
//!
//! Only what the surface draws. The board's `column`, a run's `status`,
//! `step`, `workflow`, attempts, tasks, spec and graph all ride the wire and
//! are deliberately not in [`Run`]: the surface shows what asks a person
//! (an open question) and what moves (the host phase and its log), and a
//! second kanban is exactly what ADR-0012 says not to build. Every field is
//! `#[serde(default)]`, so an unknown or missing field never fails a read and
//! a newer door stays readable.
//!
//! [ADR-0012]: https://github.com/enginkirmaci/warp-suite/blob/main/docs/adr/0012-warp-flow-publishes-a-door-of-its-own.md

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use warpui_kit_platform::door as kit;
use warpui_kit_platform::paths;

/// The app the door belongs to; the surface names it in one line.
pub const APP: &str = "warp-flow";
/// warp-flow's own wire version (not warp-agent's `ipc::PROTOCOL`). Each door's
/// version moves with its own client; the client asks for nothing newer.
pub const PROTOCOL: u32 = 1;
/// The schema version of every document the door answers.
pub const SCHEMA_VERSION: u32 = 1;

/// How often `runs` is asked while the subscription is up. The board is
/// pull-only in the contract; this is the slow timer the surface leans on.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// How long the driver waits before re-reading the endpoint files after an
/// absent answer. Within the 2–5 s band the surface promises.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(4);
/// How long one request may take before the connection counts as lost. All
/// verbs answer from memory in warp-flow, so this is a hung-socket budget, not
/// a work budget.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// A loopback connect either lands at once or is refused; a hung connect is a
/// dead door.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// The door's own folder: `~/.warp/flow`, warp-flow's — not this app's
/// `~/.warp/agent`. Built through the kit's path owner, never by hand.
pub fn endpoint_dir() -> PathBuf {
    paths::app_dir("flow")
}

/// The published endpoint in `dir`, through the family's reader: the port
/// file then the token file, treated as one publication.
pub fn read_endpoint_in(dir: &Path) -> Option<(u16, String)> {
    kit::read_endpoint_in(dir)
}

// ---------------------------------------------------------------------------
// Timing
// ---------------------------------------------------------------------------

/// The driver's clocks. Probes pass a fast one so a reconnect does not cost
/// real seconds; the app uses [`Timing::default`].
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    pub poll: Duration,
    pub retry: Duration,
    pub call: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            poll: POLL_INTERVAL,
            retry: RETRY_INTERVAL,
            call: CALL_TIMEOUT,
        }
    }
}

// ---------------------------------------------------------------------------
// Absence and call errors
// ---------------------------------------------------------------------------

/// Why there is no door to talk to. `absent` is a first-class answer: the
/// surface says "warp-flow is not running" in one line instead of showing an
/// error card, and the driver keeps looking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Absent {
    /// For the log and the store's status; the surface's line is fixed.
    pub reason: String,
}

impl Absent {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for Absent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

/// Why one request did not produce a typed payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The door answered `{"ok": false}`; the sentence is its own and is shown
    /// unchanged (a refused pause, resume or abandon is the door's to explain).
    Refused(String),
    /// The connection is gone: EOF, a broken write, or an answer with another
    /// request's id (a desynchronized stream is a dead one).
    Closed(String),
    /// The door did not answer inside [`Timing::call`].
    Timeout,
    /// A frame arrived but is not a document this client can read.
    Unreadable(String),
}

impl CallError {
    /// One sentence for the surface or the log.
    pub fn sentence(&self) -> String {
        match self {
            Self::Refused(error) => error.clone(),
            Self::Closed(reason) => reason.clone(),
            Self::Timeout => "warp-flow did not answer its door in time.".to_owned(),
            Self::Unreadable(reason) => reason.clone(),
        }
    }

    /// Whether the connection died; such an error drops it and reconnects.
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed(_) | Self::Timeout)
    }
}

// ---------------------------------------------------------------------------
// The typed payloads the surface uses
// ---------------------------------------------------------------------------

/// `runs`: the board warp-flow's window is showing plus its host's live runs.
/// `{"schema_version": 1, "repos": [...], "host": {"schema_version": 1,
/// "runs": [...]}}`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Board {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub repos: Vec<Repo>,
    #[serde(default)]
    pub host: HostSnapshot,
}

impl Board {
    /// Every board run, in repository then folder order (the door's order).
    pub fn runs(&self) -> impl Iterator<Item = &Run> {
        self.repos.iter().flat_map(|repo| repo.runs.iter())
    }

    /// How many repositories the board is watching, for the quiet empty line.
    pub fn repo_count(&self) -> usize {
        self.repos.len()
    }

    /// The board run for one issue folder.
    pub fn run(&self, issue: &Path) -> Option<&Run> {
        self.runs().find(|run| run.dir == issue)
    }
}

/// One configured repository. Only its runs are read; the rail, the path and
/// the dirty mark belong to warp-flow's own window.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Repo {
    #[serde(default)]
    pub runs: Vec<Run>,
}

/// One issue folder as the board shows it. `column`, `status`, `step`,
/// `workflow`, attempts, tasks, spec and graph are on the wire and
/// deliberately absent here: the Flow surface shows the open question and the
/// host phase, never a second board.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Run {
    #[serde(default)]
    pub key: RunKey,
    /// The issue folder, canonical — the identity every verb takes.
    #[serde(default)]
    pub dir: PathBuf,
    #[serde(default)]
    pub title: String,
    /// The first open question. The board puts a run here exactly when one is
    /// open; `open_questions` keeps the rest.
    #[serde(default)]
    pub question: Option<Question>,
    /// Every open question, in file-name order.
    #[serde(default)]
    pub open_questions: Vec<Question>,
}

impl Run {
    /// Every open question; `question` is the board's shortcut for the first
    /// of `open_questions` and is used only when the list is empty.
    pub fn questions(&self) -> Vec<&Question> {
        if self.open_questions.is_empty() {
            self.question.iter().collect()
        } else {
            self.open_questions.iter().collect()
        }
    }

    /// Whether this run asks a person something.
    pub fn waits(&self) -> bool {
        self.question.is_some() || !self.open_questions.is_empty()
    }

    /// The repository's folder name, for the card's meta line.
    pub fn repo_name(&self) -> String {
        self.key
            .repo
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.key.repo.display().to_string())
    }

    /// The issue folder's name (the folder id the card shows).
    pub fn folder_id(&self) -> String {
        if !self.key.id.is_empty() {
            return self.key.id.clone();
        }
        self.dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.dir.display().to_string())
    }
}

/// One issue folder's identity: its repository and its folder slug.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Hash)]
pub struct RunKey {
    #[serde(default)]
    pub repo: PathBuf,
    #[serde(default)]
    pub id: String,
}

/// An open question in `questions/<NN>.md`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Question {
    /// The file the answer is written back into; the door resolves an
    /// absolute path, `questions/01.md` or just the file name.
    #[serde(default)]
    pub path: PathBuf,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    /// The full markdown body, rendered read-only.
    #[serde(default)]
    pub body: String,
    /// Who asked: a step id, or `a step` when the file does not say.
    #[serde(default)]
    pub asked_by: String,
    /// The comma-separated `options:` entries, split and trimmed.
    #[serde(default)]
    pub options: Vec<String>,
}

impl Question {
    /// The kind the answer controls are built from. A `select` or `multi`
    /// without options cannot be answered by picking, so it falls back to the
    /// free-text control the contract keeps for an unknown kind.
    pub fn control_kind(&self) -> QuestionKind {
        let kind = QuestionKind::from_wire(&self.kind);
        match kind {
            QuestionKind::Select | QuestionKind::Multi if self.options.is_empty() => {
                QuestionKind::Text
            }
            kind => kind,
        }
    }

    /// The rows an `approve`, `select` or `multi` question offers. An approve
    /// question usually drives `options: yes, no` from its file; a bare
    /// approve still offers the two answers a person means.
    pub fn choices(&self) -> Vec<String> {
        if !self.options.is_empty() {
            return self.options.clone();
        }
        match self.control_kind() {
            QuestionKind::Approve => vec!["yes".to_owned(), "no".to_owned()],
            _ => Vec::new(),
        }
    }

    /// Who asked, in words the card prints.
    pub fn asked_by_label(&self) -> String {
        let asked_by = self.asked_by.trim();
        if asked_by.is_empty() || asked_by == "a step" {
            "A step".to_owned()
        } else {
            format!("Step `{asked_by}`")
        }
    }
}

/// The question kinds the contract names. An unknown kind is free text, which
/// is the only control that cannot lose an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionKind {
    Approve,
    Select,
    Multi,
    Text,
}

impl QuestionKind {
    pub fn from_wire(kind: &str) -> Self {
        match kind.trim().to_ascii_lowercase().as_str() {
            "approve" => Self::Approve,
            "select" => Self::Select,
            "multi" => Self::Multi,
            _ => Self::Text,
        }
    }
}

/// The answer text one choice writes into `answer:`. `approve` and `select`
/// write the chosen row's own label; `multi` writes the chosen labels joined
/// the way the question file lists options.
pub fn choice_answer(choice: &str) -> String {
    choice.trim().to_owned()
}

/// The `multi` answer: the chosen labels, comma-separated like `options:`.
pub fn multi_answer(choices: &[String], picked: &[bool]) -> String {
    choices
        .iter()
        .zip(picked.iter())
        .filter(|(_, picked)| **picked)
        .map(|(choice, _)| choice.trim())
        .filter(|choice| !choice.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A `runs`/`run` answer's host side: one run the app is driving, as the host
/// snapshot carries it (phases and the newest engine log lines).
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct HostSnapshot {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub runs: Vec<HostRun>,
}

impl HostSnapshot {
    /// The host's run for one issue folder.
    pub fn run(&self, issue: &Path) -> Option<&HostRun> {
        self.runs.iter().find(|run| run.issue == issue)
    }
}

/// One run warp-flow's own host is driving. The log is the engine's lines,
/// never a harness transcript.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct HostRun {
    #[serde(default)]
    pub issue: PathBuf,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub phase: Phase,
    /// When the host started driving, RFC 3339 UTC. The only timestamp the
    /// door publishes for a run; the waiting list's newest-first order leans
    /// on it when it is there.
    #[serde(default)]
    pub started: String,
    /// Why the worker failed, when it did.
    #[serde(default)]
    pub error: Option<String>,
    /// The newest engine log lines, oldest first.
    #[serde(default)]
    pub log: Vec<String>,
}

impl HostRun {
    /// The folder id the card shows.
    pub fn folder_id(&self) -> String {
        self.issue
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.issue.display().to_string())
    }
}

/// `run`: one folder's board detail and its host run. Either side may be
/// `null`: a folder the 2 s scan has not adopted yet, or a run this app never
/// drove.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct RunDetail {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub run: Option<Run>,
    #[serde(default)]
    pub host: Option<HostRun>,
}

/// One run's phase, from warp-flow's point of view. Unknown wire words are
/// kept as [`Phase::Unknown`] instead of failing the read: a newer door must
/// stay readable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Running,
    Pausing,
    Paused,
    Waiting,
    Finishing,
    Done,
    Blocked,
    Abandoned,
    Failed,
    /// A word this client does not know.
    #[default]
    Unknown,
}

impl Phase {
    pub fn from_wire(word: &str) -> Self {
        match word.trim().to_ascii_lowercase().as_str() {
            "starting" => Self::Starting,
            "running" => Self::Running,
            "pausing" => Self::Pausing,
            "paused" => Self::Paused,
            "waiting" => Self::Waiting,
            "finishing" => Self::Finishing,
            "done" => Self::Done,
            "blocked" => Self::Blocked,
            "abandoned" => Self::Abandoned,
            "failed" => Self::Failed,
            _ => Self::Unknown,
        }
    }

    /// The wire word the snapshot and the door use.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Pausing => "pausing",
            Self::Paused => "paused",
            Self::Waiting => "waiting",
            Self::Finishing => "finishing",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Abandoned => "abandoned",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    /// Whether the Flow surface's "moving" band shows this phase. `waiting`
    /// belongs to the waiting band instead, and `done`/`abandoned` are not
    /// moving anywhere.
    pub fn moves(self) -> bool {
        matches!(
            self,
            Self::Starting
                | Self::Running
                | Self::Pausing
                | Self::Paused
                | Self::Finishing
                | Self::Failed
                | Self::Blocked
        )
    }

    /// Whether a worker is driving the run, which is what Abandon needs.
    pub fn is_driving(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Pausing)
    }
}

impl<'de> Deserialize<'de> for Phase {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let word = String::deserialize(deserializer)?;
        Ok(Self::from_wire(&word))
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The verbs that change a run's movement. The door's `answer` is not one of
/// them: an answer is a document write, not a control signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveAction {
    Pause,
    Resume,
    Abandon,
}

impl DriveAction {
    /// The door verb.
    pub fn verb(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Abandon => "abandon",
        }
    }

    /// The word on the button.
    pub fn label(self) -> &'static str {
        match self {
            Self::Pause => "Pause",
            Self::Resume => "Resume",
            Self::Abandon => "Abandon",
        }
    }

    /// Whether the phase allows this verb. The rules are warp-flow's own (its
    /// run screen derives them from the same host phase), so the surface never
    /// offers a button the door would only refuse; a refusal that still
    /// happens is shown as the door's sentence.
    pub fn offer(self, phase: Phase) -> Offer {
        match self {
            Self::Pause => match phase {
                Phase::Running => Offer::Yes,
                Phase::Starting => Offer::No("The engine is starting."),
                Phase::Pausing => Offer::No("The run is pausing at the next step boundary."),
                _ => Offer::No("This run is not moving."),
            },
            Self::Resume => match phase {
                Phase::Paused => Offer::Yes,
                Phase::Failed => Offer::Yes,
                Phase::Blocked => Offer::Yes,
                _ => Offer::No("This run is not stopped."),
            },
            Self::Abandon => match phase {
                Phase::Running | Phase::Pausing => Offer::Yes,
                Phase::Starting => Offer::No("The engine is starting."),
                _ => Offer::No("This run is not being driven."),
            },
        }
    }
}

/// Whether a verb is offered for a phase, and why not when it is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offer {
    Yes,
    No(&'static str),
}

/// What `answer` did once the file was written: this window's engine continued
/// the run (`resumed`), or the answer was only recorded for whoever drives it
/// (`recorded`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnswerOutcome {
    Resumed,
    Recorded,
}

impl AnswerOutcome {
    fn from_wire(word: &str) -> Option<Self> {
        match word {
            "resumed" => Some(Self::Resumed),
            "recorded" => Some(Self::Recorded),
            _ => None,
        }
    }

    /// The outcome in a word for the card's line.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Resumed => "resumed",
            Self::Recorded => "recorded",
        }
    }
}

// ---------------------------------------------------------------------------
// The wire connection
// ---------------------------------------------------------------------------

/// One authenticated NDJSON connection: an owned write half and a buffered
/// read half, plus the request-id counter one connection's frames share.
struct LineConn {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl LineConn {
    async fn write_frame(&mut self, frame: &Value) -> Result<(), CallError> {
        let mut line = frame.to_string();
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|err| CallError::Closed(format!("could not write to warp-flow: {err}")))?;
        self.writer
            .flush()
            .await
            .map_err(|err| CallError::Closed(format!("could not write to warp-flow: {err}")))
    }

    /// One NDJSON line, capped at the family's [`kit::MAX_LINE`]. A blank line
    /// is not a frame and is skipped; EOF and an over-long line are errors.
    async fn read_frame(&mut self) -> Result<Value, CallError> {
        loop {
            let mut limited = (&mut self.reader).take(kit::MAX_LINE as u64 + 1);
            let mut line = String::new();
            match limited.read_line(&mut line).await {
                Ok(0) => {
                    return Err(CallError::Closed("warp-flow closed its door.".to_owned()));
                }
                Ok(_) if line.len() > kit::MAX_LINE => {
                    return Err(CallError::Closed(
                        "warp-flow sent a frame over its line cap.".to_owned(),
                    ));
                }
                Ok(_) => {}
                Err(err) => {
                    return Err(CallError::Closed(format!(
                        "could not read warp-flow's door: {err}"
                    )));
                }
            }
            if line.trim().is_empty() {
                continue;
            }
            return serde_json::from_str(line.trim()).map_err(|err| {
                CallError::Unreadable(format!(
                    "warp-flow sent a frame this client cannot read: {err}"
                ))
            });
        }
    }
}

/// How many attempts to connect before calling the door absent. A loopback
/// connect either lands or is refused; two tries cover a listener that is
/// between `bind` and `accept`.
const CONNECT_ATTEMPTS: u32 = 2;

/// Read the endpoint and open an authenticated connection. Every failure is
/// [`Absent`], because every one of them means the same thing to the surface.
async fn open_authed(dir: &Path) -> Result<LineConn, Absent> {
    let Some((port, token)) = read_endpoint_in(dir) else {
        return Err(Absent::new(format!(
            "warp-flow has not published its door in {}.",
            dir.display()
        )));
    };
    let mut last = String::new();
    for attempt in 0..CONNECT_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        let stream =
            match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(("127.0.0.1", port)))
                .await
            {
                Ok(Ok(stream)) => stream,
                Ok(Err(err)) => {
                    last =
                        format!("warp-flow's door at 127.0.0.1:{port} refused a connection: {err}");
                    continue;
                }
                Err(_) => {
                    last = format!(
                        "warp-flow's door at 127.0.0.1:{port} did not accept a connection."
                    );
                    continue;
                }
            };
        let _ = stream.set_nodelay(true);
        let (read, write) = stream.into_split();
        let mut conn = LineConn {
            reader: BufReader::new(read),
            writer: write,
            next_id: 0,
        };
        // The family's handshake: one token line, one verdict. A wrong token
        // is refused and the connection then ends.
        if let Err(err) = conn.write_frame(&json!({"auth": token})).await {
            return Err(Absent::new(err.sentence()));
        }
        let verdict = tokio::time::timeout(CONNECT_TIMEOUT, conn.read_frame()).await;
        let authed = match verdict {
            Ok(Ok(frame)) => frame.get("ok").and_then(Value::as_bool),
            Ok(Err(err)) => {
                return Err(Absent::new(err.sentence()));
            }
            Err(_) => {
                last = format!("warp-flow's door at 127.0.0.1:{port} did not answer its token.");
                continue;
            }
        };
        return match authed {
            Some(true) => Ok(conn),
            _ => Err(Absent::new(
                "warp-flow refused this session's token; it restarted, and the next pass reads the new one.",
            )),
        };
    }
    Err(Absent::new(last))
}

/// One request/response connection. The verbs mirror the contract; each takes
/// the issue folder the board's `dir` carries.
pub struct Connection {
    conn: LineConn,
}

impl Connection {
    /// The board: every repository's runs plus the host's live runs.
    pub async fn runs(&mut self) -> Result<Board, CallError> {
        let payload = self.call("runs", json!({})).await?;
        read_payload::<Board>(&payload, "board")
    }

    /// One folder's board detail and its host run.
    pub async fn run(&mut self, issue: &Path) -> Result<RunDetail, CallError> {
        let payload = self.call("run", json!({"issue": wire_path(issue)})).await?;
        read_payload::<RunDetail>(&payload, "run")
    }

    /// Write an answer into one question file. `outcome` says whether this
    /// window's engine resumed the run or the answer was only recorded.
    pub async fn answer(
        &mut self,
        issue: &Path,
        question: &Path,
        text: &str,
    ) -> Result<AnswerOutcome, CallError> {
        let payload = self
            .call(
                "answer",
                json!({
                    "issue": wire_path(issue),
                    "question": wire_path(question),
                    "text": text,
                }),
            )
            .await?;
        payload
            .get("outcome")
            .and_then(Value::as_str)
            .and_then(AnswerOutcome::from_wire)
            .ok_or_else(|| {
                CallError::Unreadable("warp-flow answered an answer without an outcome.".to_owned())
            })
    }

    /// Pause, resume or abandon a run. The answer is the phase the run was
    /// left in; a refusal is the door's own sentence in
    /// [`CallError::Refused`].
    pub async fn drive(&mut self, issue: &Path, action: DriveAction) -> Result<Phase, CallError> {
        let payload = self
            .call(action.verb(), json!({"issue": wire_path(issue)}))
            .await?;
        payload
            .get("phase")
            .and_then(Value::as_str)
            .map(Phase::from_wire)
            .ok_or_else(|| {
                CallError::Unreadable(format!(
                    "warp-flow answered {} without a phase.",
                    action.verb()
                ))
            })
    }

    /// One request and its answer. The answer's `id` must be this request's:
    /// a mismatched frame is a desynchronized stream, which is treated as a
    /// closed connection instead of being decoded as this call's payload.
    async fn call(&mut self, cmd: &str, params: Value) -> Result<Value, CallError> {
        self.conn.next_id += 1;
        let id = self.conn.next_id;
        let frame = kit::request_frame(id, cmd, params);
        self.conn.write_frame(&frame).await?;
        let answer = self.conn.read_frame().await?;
        if answer.get("id").and_then(Value::as_u64) != Some(id) {
            return Err(CallError::Closed(
                "warp-flow answered a different request than the one asked.".to_owned(),
            ));
        }
        match answer.get("ok").and_then(Value::as_bool) {
            Some(true) => Ok(answer),
            Some(false) => Err(CallError::Refused(
                answer
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("warp-flow refused the request.")
                    .to_owned(),
            )),
            None => Err(CallError::Unreadable(
                "warp-flow answered without ok.".to_owned(),
            )),
        }
    }
}

/// Open the request/response connection. `dir` carries the endpoint files.
pub async fn connect_in(dir: &Path) -> Result<Connection, Absent> {
    Ok(Connection {
        conn: open_authed(dir).await?,
    })
}

/// One subscribed connection: `subscribe` has been acked and the connection is
/// a stream from then on.
pub struct Subscription {
    conn: LineConn,
}

impl Subscription {
    /// The next pushed frame. Only returns once the door speaks or closes, so
    /// the caller should select it against its own work (as the driver does).
    pub async fn next(&mut self) -> Result<Value, CallError> {
        self.conn.read_frame().await
    }
}

/// Subscribe to every push: run changes and board notifications. A filtered
/// subscription (`{"issue": …}`) exists in the contract; the surface wants
/// the whole board, so no filter is sent.
pub async fn subscribe_in(dir: &Path) -> Result<Subscription, Absent> {
    let mut conn = open_authed(dir).await?;
    conn.next_id += 1;
    let frame = kit::request_frame(conn.next_id, "subscribe", json!({}));
    if let Err(err) = conn.write_frame(&frame).await {
        return Err(Absent::new(err.sentence()));
    }
    match conn.read_frame().await {
        Ok(answer) if answer.get("ok").and_then(Value::as_bool) == Some(true) => {
            Ok(Subscription { conn })
        }
        Ok(answer) => Err(Absent::new(
            answer
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("warp-flow refused the subscription.")
                .to_owned(),
        )),
        Err(err) => Err(Absent::new(err.sentence())),
    }
}

fn read_payload<T: for<'de> Deserialize<'de>>(frame: &Value, what: &str) -> Result<T, CallError> {
    serde_json::from_value(frame.clone()).map_err(|err| {
        CallError::Unreadable(format!(
            "warp-flow answered a {what} this client cannot read: {err}"
        ))
    })
}

/// The door compares issue folders as paths, so a path goes over the wire as
/// a string. A non-UTF-8 path is the one case a loopback door cannot name;
/// the lossy form is what warp-flow's own store would print.
fn wire_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

// ---------------------------------------------------------------------------
// The driver: one task, both connections, reconnect
// ---------------------------------------------------------------------------

/// What the surface learns from the client.
#[derive(Clone, Debug, PartialEq)]
pub enum FlowEvent {
    /// The door's standing: looking, live, or absent with the reason the last
    /// attempt failed.
    Status(FlowStatus),
    /// The board answered a `runs` ask.
    Board(Box<Board>),
    /// One host run changed (a push), or a `run` answer carried it.
    Run(Box<HostRun>),
    /// An `answer` came back; `result` is the door's outcome or its sentence.
    Answered {
        id: u64,
        issue: PathBuf,
        result: Result<AnswerOutcome, String>,
    },
    /// A `pause`/`resume`/`abandon` came back; `result` is the phase the run
    /// was left in, or the door's refusal.
    Driven {
        id: u64,
        issue: PathBuf,
        action: DriveAction,
        result: Result<Phase, String>,
    },
}

/// The door's standing for the surface's head and its absent state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FlowStatus {
    /// No answer yet: the first connect attempt is in flight.
    #[default]
    Looking,
    /// The door answered and the subscription is up.
    Ready,
    /// No door: the reason the last attempt failed. The surface shows one
    /// fixed line and a retry; the reason is for the log and the probes.
    Absent { reason: String },
}

/// What the surface asks the client to do. `id` is the surface's own token,
/// echoed on the matching event so a pending row can be resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlowCommand {
    /// Ask `runs` now (opening the sheet, the retry button, a board push).
    Refresh,
    /// Write an answer and report the outcome.
    Answer {
        id: u64,
        issue: PathBuf,
        question: PathBuf,
        text: String,
    },
    /// Pause, resume or abandon a run.
    Drive {
        id: u64,
        issue: PathBuf,
        action: DriveAction,
    },
}

/// The UI's end of the client: commands in. The driver never blocks on this
/// channel; sending from a render/click path is a non-blocking channel send.
#[derive(Clone)]
pub struct FlowSender(pub async_channel::Sender<FlowCommand>);

impl FlowSender {
    /// Queue one command. A closed channel means the driver is gone (app
    /// shutdown), which is a log line and never a panic.
    pub fn send(&self, command: FlowCommand) {
        if let Err(err) = self.0.send_blocking(command) {
            tracing::error!("flow client gone: {err}");
        }
    }
}

/// Run the client until the app drops the command channel. Spawned on the net
/// thread's runtime by `oc::net_main`.
pub async fn drive(
    endpoint: PathBuf,
    events: async_channel::Sender<FlowEvent>,
    commands: async_channel::Receiver<FlowCommand>,
) {
    drive_with(endpoint, events, commands, Timing::default()).await
}

/// [`drive`] with explicit clocks: the seam the loopback probes drive so a
/// poll or a reconnect does not cost real seconds.
#[doc(hidden)]
pub async fn drive_with(
    endpoint: PathBuf,
    events: async_channel::Sender<FlowEvent>,
    commands: async_channel::Receiver<FlowCommand>,
    timing: Timing,
) {
    let mut status = FlowStatus::default();
    loop {
        // One attempt: connect both halves, then serve until the door closes
        // or the app shuts down.
        let opened = Session::open(&endpoint).await;
        let mut session = match opened {
            Ok(session) => session,
            Err(absent) => {
                publish_status(
                    &events,
                    &mut status,
                    FlowStatus::Absent {
                        reason: absent.reason,
                    },
                )
                .await;
                if wait_retry(&events, &commands, &timing).await {
                    return;
                }
                continue;
            }
        };
        publish_status(&events, &mut status, FlowStatus::Ready).await;
        match session.serve(&events, &commands, &timing).await {
            SessionEnd::Closed(reason) => {
                publish_status(&events, &mut status, FlowStatus::Absent { reason }).await
            }
            SessionEnd::ShuttingDown => return,
        }
        if wait_retry(&events, &commands, &timing).await {
            return;
        }
    }
}

/// Wait one retry interval before looking for the door again. A `Refresh`
/// retries immediately; any other command is answered with "warp-flow is not
/// running" so a click during an outage never leaves a pending row.
///
/// Returns true when the app is shutting down.
async fn wait_retry(
    events: &async_channel::Sender<FlowEvent>,
    commands: &async_channel::Receiver<FlowCommand>,
    timing: &Timing,
) -> bool {
    let deadline = tokio::time::Instant::now() + timing.retry;
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Ok(FlowCommand::Refresh) => return false,
                Ok(command) => {
                    // No door: answer the click instead of parking it, so the
                    // surface can show the refusal and clear its pending row.
                    let lost = |id: u64, issue: PathBuf, action: Option<DriveAction>| {
                        let reason = format!("{APP} is not running.");
                        match action {
                            Some(action) => FlowEvent::Driven {
                                id,
                                issue,
                                action,
                                result: Err(reason),
                            },
                            None => FlowEvent::Answered {
                                id,
                                issue,
                                result: Err(reason),
                            },
                        }
                    };
                    let event = match command {
                        FlowCommand::Refresh => unreachable!("handled above"),
                        FlowCommand::Answer { id, issue, .. } => lost(id, issue, None),
                        FlowCommand::Drive { id, issue, action } => {
                            lost(id, issue, Some(action))
                        }
                    };
                    if events.send(event).await.is_err() {
                        return true;
                    }
                }
                Err(_) => return true,
            },
            _ = tokio::time::sleep_until(deadline) => return false,
        }
    }
}

/// Send one status, but only when it changed: the surface repaints on the
/// store's notify, and an unchanged standing is not news.
async fn publish_status(
    events: &async_channel::Sender<FlowEvent>,
    current: &mut FlowStatus,
    next: FlowStatus,
) {
    if *current == next {
        return;
    }
    *current = next.clone();
    let _ = events.send(FlowEvent::Status(next)).await;
}

/// A live pair of connections: the verb connection, the subscription, and the
/// reader task that turns subscription frames into a channel so no blocking
/// read is ever cancelled mid-line.
struct Session {
    connection: Connection,
    subscription: Option<Subscription>,
}

impl Session {
    async fn open(dir: &Path) -> Result<Self, Absent> {
        let connection = connect_in(dir).await?;
        let subscription = subscribe_in(dir).await?;
        Ok(Self {
            connection,
            subscription: Some(subscription),
        })
    }

    /// Serve one live session until it closes or the app shuts down.
    async fn serve(
        &mut self,
        events: &async_channel::Sender<FlowEvent>,
        commands: &async_channel::Receiver<FlowCommand>,
        timing: &Timing,
    ) -> SessionEnd {
        let mut pushes = spawn_push_reader(self.subscription.take());
        // Ask once on attach, then every poll: `runs` is pull-only.
        let mut next_poll = tokio::time::Instant::now();
        loop {
            let wake = tokio::select! {
                push = pushes.recv() => Wake::Push(push.flatten()),
                command = commands.recv() => Wake::Command(command.ok()),
                _ = tokio::time::sleep_until(next_poll) => Wake::Poll,
            };
            match wake {
                Wake::Push(push) => {
                    let Some(push) = push else {
                        return SessionEnd::Closed("warp-flow closed its subscription.".to_owned());
                    };
                    match handle_push(push, &mut self.connection, events).await {
                        Ok(PushOutcome::Board) => {
                            next_poll = tokio::time::Instant::now() + timing.poll;
                        }
                        Ok(PushOutcome::Ignored) => {}
                        Err(end) => return end,
                    }
                }
                Wake::Command(None) => return SessionEnd::ShuttingDown,
                Wake::Command(Some(command)) => {
                    match serve_command(&mut self.connection, events, command).await {
                        Ok(()) => {}
                        Err(end) => return end,
                    }
                }
                Wake::Poll => match self.connection.runs().await {
                    Ok(board) => {
                        next_poll = tokio::time::Instant::now() + timing.poll;
                        if events
                            .send(FlowEvent::Board(Box::new(board)))
                            .await
                            .is_err()
                        {
                            return SessionEnd::ShuttingDown;
                        }
                    }
                    Err(err) => return SessionEnd::Closed(err.sentence()),
                },
            }
        }
    }
}

enum Wake {
    Push(Option<Value>),
    Command(Option<FlowCommand>),
    Poll,
}

/// What a push meant to the session loop: a board notification asks `runs`
/// again now; a run push (and anything this client does not know) only reaches
/// the UI.
enum PushOutcome {
    Board,
    Ignored,
}

async fn handle_push(
    push: Value,
    connection: &mut Connection,
    events: &async_channel::Sender<FlowEvent>,
) -> Result<PushOutcome, SessionEnd> {
    match push.get("push").and_then(Value::as_str) {
        Some("board") => {
            // A contentless notification: board content stays pull-only, so
            // the answer is another `runs` ask.
            match connection.runs().await {
                Ok(board) => {
                    if events
                        .send(FlowEvent::Board(Box::new(board)))
                        .await
                        .is_err()
                    {
                        return Err(SessionEnd::ShuttingDown);
                    }
                    Ok(PushOutcome::Board)
                }
                Err(err) => Err(SessionEnd::Closed(err.sentence())),
            }
        }
        Some("run") => {
            let Some(run) = push.get("run") else {
                return Ok(PushOutcome::Ignored);
            };
            match serde_json::from_value::<HostRun>(run.clone()) {
                Ok(run) => {
                    if events.send(FlowEvent::Run(Box::new(run))).await.is_err() {
                        return Err(SessionEnd::ShuttingDown);
                    }
                    Ok(PushOutcome::Ignored)
                }
                Err(err) => {
                    tracing::warn!("flow: could not read a run push: {err}");
                    Ok(PushOutcome::Ignored)
                }
            }
        }
        // A newer door may push a frame this client does not know; it is not
        // a protocol error and never drops the connection.
        _ => Ok(PushOutcome::Ignored),
    }
}

async fn serve_command(
    connection: &mut Connection,
    events: &async_channel::Sender<FlowEvent>,
    command: FlowCommand,
) -> Result<(), SessionEnd> {
    match command {
        FlowCommand::Refresh => match connection.runs().await {
            Ok(board) => {
                if events
                    .send(FlowEvent::Board(Box::new(board)))
                    .await
                    .is_err()
                {
                    return Err(SessionEnd::ShuttingDown);
                }
                Ok(())
            }
            Err(err) => Err(SessionEnd::Closed(err.sentence())),
        },
        FlowCommand::Answer {
            id,
            issue,
            question,
            text,
        } => {
            let result = connection.answer(&issue, &question, &text).await;
            // A refusal is the door's sentence, not a dead door: report it and
            // keep the session. Only a broken connection is reconnected.
            let closed = result.as_ref().is_err_and(CallError::is_closed);
            if events
                .send(FlowEvent::Answered {
                    id,
                    issue: issue.clone(),
                    result: result.map_err(|err| err.sentence()),
                })
                .await
                .is_err()
            {
                return Err(SessionEnd::ShuttingDown);
            }
            if closed {
                return Err(SessionEnd::Closed(
                    "warp-flow closed its door while an answer was in flight.".to_owned(),
                ));
            }
            Ok(())
        }
        FlowCommand::Drive { id, issue, action } => {
            let result = connection.drive(&issue, action).await;
            let closed = result.as_ref().is_err_and(CallError::is_closed);
            if events
                .send(FlowEvent::Driven {
                    id,
                    issue: issue.clone(),
                    action,
                    result: result.map_err(|err| err.sentence()),
                })
                .await
                .is_err()
            {
                return Err(SessionEnd::ShuttingDown);
            }
            if closed {
                return Err(SessionEnd::Closed(
                    "warp-flow closed its door while a run action was in flight.".to_owned(),
                ));
            }
            Ok(())
        }
    }
}

enum SessionEnd {
    Closed(String),
    ShuttingDown,
}

/// Read the subscribed connection on its own task, so the select loop never
/// cancels a partially-read line (a cancelled `read_line` would drop bytes it
/// had already consumed and desynchronize the stream).
fn spawn_push_reader(
    subscription: Option<Subscription>,
) -> tokio::sync::mpsc::Receiver<Option<Value>> {
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        let Some(mut subscription) = subscription else {
            return;
        };
        loop {
            let next = subscription.next().await;
            let stop = next.is_err();
            let frame = next.ok();
            if tx.send(frame).await.is_err() {
                return;
            }
            if stop {
                return;
            }
        }
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_phase_word_roundtrips_and_unknown_words_stay_readable() {
        for phase in [
            Phase::Starting,
            Phase::Running,
            Phase::Pausing,
            Phase::Paused,
            Phase::Waiting,
            Phase::Finishing,
            Phase::Done,
            Phase::Blocked,
            Phase::Abandoned,
            Phase::Failed,
        ] {
            assert_eq!(Phase::from_wire(phase.as_str()), phase);
        }
        assert_eq!(Phase::from_wire("invented"), Phase::Unknown);
        assert_eq!(Phase::from_wire(" Running "), Phase::Running);
    }

    #[test]
    fn a_board_reads_with_unknown_and_missing_fields() {
        let board: Board = serde_json::from_str(
            r##"{
                "schema_version": 1,
                "new_field": {"anything": true},
                "repos": [{
                    "path": "/tmp/repo", "name": "repo", "dirty": false,
                    "runs": [{
                        "key": {"repo": "/tmp/repo", "id": "001-demo"},
                        "dir": "/tmp/repo/.scratch/001-demo",
                        "title": "A demo",
                        "status": "waiting", "column": "waiting",
                        "step": "analyze", "attempts": 2,
                        "questions_from_the_future": [],
                        "open_questions": [{
                            "path": "/tmp/repo/.scratch/001-demo/questions/01.md",
                            "kind": "approve", "title": "Q1", "body": "# Q1",
                            "asked_by": "analyze", "options": ["yes", "no"]
                        }]
                    }]
                }],
                "host": {"schema_version": 1, "runs": [{
                    "issue": "/tmp/repo/.scratch/001-demo",
                    "title": "A demo", "phase": "paused",
                    "started": "2026-01-01T00:00:00Z",
                    "log": ["one", "two"], "future": 1
                }]}
            }"##,
        )
        .expect("a board with unknown fields reads");
        assert_eq!(board.repo_count(), 1);
        let run = board
            .run(Path::new("/tmp/repo/.scratch/001-demo"))
            .expect("run");
        assert!(run.waits());
        assert_eq!(run.title, "A demo");
        assert_eq!(run.folder_id(), "001-demo");
        assert_eq!(run.repo_name(), "repo");
        assert_eq!(
            run.questions()[0].choices(),
            vec!["yes".to_owned(), "no".to_owned()]
        );
        assert_eq!(board.host.runs[0].phase, Phase::Paused);
        assert_eq!(board.host.runs[0].log, ["one", "two"]);
    }

    #[test]
    fn a_board_without_any_field_reads_as_empty() {
        let board: Board = serde_json::from_str("{}").expect("empty board");
        assert_eq!(board.repo_count(), 0);
        assert!(!board.host.runs.iter().any(|run| run.phase.moves()));
    }

    #[test]
    fn question_kinds_map_to_the_controls_they_need() {
        let question = |kind: &str, options: &[&str]| Question {
            kind: kind.to_owned(),
            options: options.iter().map(|option| (*option).to_owned()).collect(),
            ..Default::default()
        };
        assert_eq!(
            question("approve", &["yes", "no"]).control_kind(),
            QuestionKind::Approve
        );
        assert_eq!(
            question("select", &["a", "b"]).control_kind(),
            QuestionKind::Select
        );
        assert_eq!(
            question("multi", &["a", "b"]).control_kind(),
            QuestionKind::Multi
        );
        assert_eq!(question("text", &[]).control_kind(), QuestionKind::Text);
        assert_eq!(
            question("unheard-of", &[]).control_kind(),
            QuestionKind::Text
        );
        // A pick-one question without options can only be answered by typing.
        assert_eq!(question("select", &[]).control_kind(), QuestionKind::Text);
        // A bare approve still offers the two answers a person means.
        assert_eq!(
            question("approve", &[]).choices(),
            vec!["yes".to_owned(), "no".to_owned()]
        );
    }

    #[test]
    fn multi_answers_join_the_chosen_labels_like_the_file() {
        let choices = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        assert_eq!(multi_answer(&choices, &[true, false, true]), "a, c");
        assert_eq!(multi_answer(&choices, &[false, false, false]), "");
        assert_eq!(choice_answer("  yes "), "yes");
    }

    #[test]
    fn the_phase_offers_only_the_verbs_it_allows() {
        assert_eq!(DriveAction::Pause.offer(Phase::Running), Offer::Yes);
        assert!(matches!(
            DriveAction::Pause.offer(Phase::Paused),
            Offer::No(_)
        ));
        assert_eq!(DriveAction::Resume.offer(Phase::Paused), Offer::Yes);
        assert_eq!(DriveAction::Resume.offer(Phase::Failed), Offer::Yes);
        assert_eq!(DriveAction::Resume.offer(Phase::Blocked), Offer::Yes);
        assert_eq!(
            DriveAction::Resume.offer(Phase::Running),
            Offer::No("This run is not stopped.")
        );
        assert_eq!(DriveAction::Abandon.offer(Phase::Running), Offer::Yes);
        assert!(matches!(
            DriveAction::Abandon.offer(Phase::Paused),
            Offer::No(_)
        ));
        assert!(matches!(
            DriveAction::Pause.offer(Phase::Starting),
            Offer::No("The engine is starting.")
        ));
    }

    #[test]
    fn only_the_contracts_moving_phases_are_shown_moving() {
        for phase in [
            Phase::Starting,
            Phase::Running,
            Phase::Pausing,
            Phase::Paused,
            Phase::Finishing,
            Phase::Failed,
            Phase::Blocked,
        ] {
            assert!(phase.moves(), "{phase} moves");
        }
        for phase in [
            Phase::Waiting,
            Phase::Done,
            Phase::Abandoned,
            Phase::Unknown,
        ] {
            assert!(!phase.moves(), "{phase} does not move");
        }
    }

    #[test]
    fn an_answer_outcome_reads_both_words_and_nothing_else() {
        assert_eq!(
            AnswerOutcome::from_wire("resumed"),
            Some(AnswerOutcome::Resumed)
        );
        assert_eq!(
            AnswerOutcome::from_wire("recorded"),
            Some(AnswerOutcome::Recorded)
        );
        assert_eq!(AnswerOutcome::from_wire("maybe"), None);
    }

    #[test]
    fn an_absent_answer_names_the_door() {
        let absent = Absent::new("no endpoint");
        assert_eq!(absent.to_string(), "no endpoint");
        assert!(CallError::Timeout.is_closed());
        assert!(!CallError::Refused("nope".into()).is_closed());
        assert_eq!(CallError::Refused("nope".into()).sentence(), "nope");
    }
}
