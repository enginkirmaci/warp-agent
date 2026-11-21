//! The Flow surface's state in the store: the board warp-flow's door answers,
//! the host runs its subscription pushes, and the state of the one person
//! answering or driving a run.
//!
//! Everything here arrives already parsed from the client on the net thread
//! (`crate::flow`), through the app's one `UiEvent` pump. A render only reads
//! this struct, so the sheet never touches a socket, a file or a lock.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use gpui::Context;

use crate::flow::{
    AnswerOutcome, Board, DriveAction, FlowCommand, FlowEvent, FlowStatus, HostRun, Phase, Run,
};
use crate::store::Store;

/// How many of the newest engine log lines a moving card prints. The host
/// keeps the production tail; the surface shows the end a person reads.
pub const FLOW_LOG_LINES: usize = 6;

/// The store's whole Flow state.
#[derive(Clone, Debug, Default)]
pub struct FlowData {
    status: FlowStatus,
    board: Board,
    /// Answers this surface sent, by issue folder: pending until the door
    /// answers, then the outcome that line shows. Dropped when the next board
    /// says the run no longer waits.
    answers: HashMap<String, AnswerState>,
    /// Run actions this surface sent, by issue folder. Dropped when the run
    /// leaves the moving band.
    actions: HashMap<String, ActionState>,
    /// The next request token; the door's answers carry it back.
    next_id: u64,
}

impl FlowData {
    /// The door's standing, for the sheet's head and its absent body.
    pub fn status(&self) -> &FlowStatus {
        &self.status
    }

    /// The board the door last answered (empty until the first `runs`).
    pub fn board(&self) -> &Board {
        &self.board
    }

    /// How many repositories warp-flow is watching, for the empty line.
    pub fn repo_count(&self) -> usize {
        self.board.repo_count()
    }

    /// Every run that waits on a person, newest first.
    ///
    /// The only time the door publishes for a run is the host snapshot's
    /// `started` (the board payload itself carries no timestamp), so a run the
    /// host is driving sorts by it and a run driven elsewhere keeps the
    /// board's folder order. Within one band the order never shuffles on a
    /// poll: equal instants fall back to the folder id.
    pub fn waiting(&self) -> Vec<&Run> {
        let mut runs: Vec<&Run> = self.board.runs().filter(|run| run.waits()).collect();
        runs.sort_by(|left, right| {
            let left_started = self.started(&left.dir);
            let right_started = self.started(&right.dir);
            right_started
                .cmp(left_started)
                .then_with(|| left.folder_id().cmp(&right.folder_id()))
        });
        runs
    }

    /// Every run the host reports as moving, in the host's own (folder) order.
    pub fn moving(&self) -> Vec<&HostRun> {
        self.board
            .host
            .runs
            .iter()
            .filter(|run| run.phase.moves())
            .collect()
    }

    /// The host's run for one issue folder, when it has one.
    pub fn host_run(&self, issue: &Path) -> Option<&HostRun> {
        self.board.host.run(issue)
    }

    /// The answer this surface sent for one issue folder.
    pub fn answer(&self, issue: &Path) -> Option<&AnswerState> {
        self.answers.get(&issue_key(issue))
    }

    /// The run action this surface sent for one issue folder.
    pub fn action(&self, issue: &Path) -> Option<&ActionState> {
        self.actions.get(&issue_key(issue))
    }

    /// The host's start instant for one issue folder, or an empty string so a
    /// run driven elsewhere sorts last in a newest-first list.
    fn started(&self, issue: &Path) -> &str {
        self.host_run(issue)
            .map(|run| run.started.as_str())
            .unwrap_or_default()
    }

    /// Take one request token; the surface echoes it on commands and the door
    /// answers carry it back.
    pub(crate) fn allocate(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// The waiting folder keys, for the retain after a board.
    fn waiting_keys(&self) -> HashSet<String> {
        self.board
            .runs()
            .filter(|run| run.waits())
            .map(|run| issue_key(&run.dir))
            .collect()
    }

    /// The moving folder keys, for the retain after a board or a run push.
    fn moving_keys(&self) -> HashSet<String> {
        self.board
            .host
            .runs
            .iter()
            .filter(|run| run.phase.moves())
            .map(|run| issue_key(&run.issue))
            .collect()
    }

    /// Replace the board and drop the state of runs it no longer lists in the
    /// band that state belongs to. Returns whether anything the surface draws
    /// changed.
    fn apply_board(&mut self, board: Board) -> bool {
        let changed = self.board != board;
        self.board = board;
        let waiting = self.waiting_keys();
        self.answers.retain(|issue, _| waiting.contains(issue));
        let moving = self.moving_keys();
        self.actions.retain(|issue, _| moving.contains(issue));
        changed
    }

    /// Merge one pushed host run into the board's host list, replacing the
    /// entry the door already had.
    fn apply_run(&mut self, run: HostRun) -> bool {
        let slot = self
            .board
            .host
            .runs
            .iter_mut()
            .find(|existing| existing.issue == run.issue);
        let changed = match slot {
            Some(existing) => {
                if *existing == run {
                    false
                } else {
                    *existing = run;
                    true
                }
            }
            None => {
                self.board.host.runs.push(run);
                true
            }
        };
        // A run that stopped moving gives its action line back.
        let moving = self.moving_keys();
        self.actions.retain(|issue, _| moving.contains(issue));
        changed
    }

    /// A pending row must not outlive the door that would answer it.
    fn drop_pending(&mut self, reason: &str) {
        for state in self.answers.values_mut() {
            if state.pending.take().is_some() {
                state.outcome = Some(Err(reason.to_owned()));
            }
        }
        for state in self.actions.values_mut() {
            if state.pending.take().is_some() {
                state.outcome = Some(Err(reason.to_owned()));
            }
        }
    }
}

/// One answer's life on the surface: pending, then the door's outcome or its
/// sentence (a refusal, or the door closing under the request).
#[derive(Clone, Debug, Default)]
pub struct AnswerState {
    pending: Option<u64>,
    question: Option<PathBuf>,
    outcome: Option<Result<AnswerOutcome, String>>,
}

impl AnswerState {
    /// Whether this answer is still in flight.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The question the answer was for.
    pub fn question(&self) -> Option<&Path> {
        self.question.as_deref()
    }

    /// The one line under the question: in flight, the outcome, or the
    /// refusal. `None` before the first answer.
    pub fn line(&self) -> Option<String> {
        if self.is_pending() {
            return Some("Sending your answer…".to_owned());
        }
        match &self.outcome {
            Some(Ok(AnswerOutcome::Resumed)) => {
                Some("Answer sent — warp-flow resumed the run.".to_owned())
            }
            Some(Ok(AnswerOutcome::Recorded)) => Some(
                "Answer sent — warp-flow recorded it; the driver on that run continues it."
                    .to_owned(),
            ),
            Some(Err(error)) => Some(format!("warp-flow refused the answer: {error}")),
            None => None,
        }
    }

    /// Whether the line is a failure, which the card paints as one.
    pub fn failed(&self) -> bool {
        matches!(self.outcome, Some(Err(_)))
    }
}

/// One run action's life: pending, then the phase the door left the run in, or
/// the door's own refusal.
#[derive(Clone, Debug, Default)]
pub struct ActionState {
    pending: Option<u64>,
    action: Option<DriveAction>,
    outcome: Option<Result<Phase, String>>,
}

impl ActionState {
    /// The verb that is in flight or was answered.
    pub fn action(&self) -> Option<DriveAction> {
        self.action
    }

    /// The one line under the moving card.
    pub fn line(&self) -> Option<String> {
        if self.pending.is_some() {
            return self
                .action
                .map(|action| format!("{} requested…", action.label()));
        }
        match &self.outcome {
            Some(Ok(phase)) => Some(format!("warp-flow: {}", phase_line(*phase))),
            Some(Err(error)) => Some(error.clone()),
            None => None,
        }
    }

    /// Whether the line is a failure, which the card paints as one.
    pub fn failed(&self) -> bool {
        matches!(self.outcome, Some(Err(_)))
    }
}

/// A phase in the words a person reads (`Running`, not `running`).
pub fn phase_line(phase: Phase) -> &'static str {
    match phase {
        Phase::Starting => "starting",
        Phase::Running => "running",
        Phase::Pausing => "pausing at the next step",
        Phase::Paused => "paused",
        Phase::Waiting => "waiting on you",
        Phase::Finishing => "finishing",
        Phase::Done => "done",
        Phase::Blocked => "blocked",
        Phase::Abandoned => "abandoned",
        Phase::Failed => "failed",
        Phase::Unknown => "in a phase this build does not know",
    }
}

/// One issue folder as a map key. Every spelling of a folder comes from the
/// door's own snapshot (canonical), so the lossy form is stable.
fn issue_key(issue: &Path) -> String {
    issue.to_string_lossy().into_owned()
}

impl Store {
    /// The Flow state the sheet renders.
    pub fn flow(&self) -> &FlowData {
        &self.flow
    }

    /// Ask the client for the board now (the sheet just opened, or Retry).
    pub fn flow_refresh(&mut self, _cx: &mut Context<Self>) {
        self.flow_tx.send(FlowCommand::Refresh);
    }

    /// Send an answer for one open question and remember that it is in flight.
    pub fn flow_answer(
        &mut self,
        issue: &Path,
        question: &Path,
        text: String,
        cx: &mut Context<Self>,
    ) -> u64 {
        let id = self.flow.allocate();
        self.flow.answers.insert(
            issue_key(issue),
            AnswerState {
                pending: Some(id),
                question: Some(question.to_path_buf()),
                outcome: None,
            },
        );
        self.flow_tx.send(FlowCommand::Answer {
            id,
            issue: issue.to_path_buf(),
            question: question.to_path_buf(),
            text,
        });
        cx.notify();
        id
    }

    /// Pause, resume or abandon a run and remember that the verb is in flight.
    pub fn flow_drive(&mut self, issue: &Path, action: DriveAction, cx: &mut Context<Self>) -> u64 {
        let id = self.flow.allocate();
        self.flow.actions.insert(
            issue_key(issue),
            ActionState {
                pending: Some(id),
                action: Some(action),
                outcome: None,
            },
        );
        self.flow_tx.send(FlowCommand::Drive {
            id,
            issue: issue.to_path_buf(),
            action,
        });
        cx.notify();
        id
    }

    /// One event from the Flow client. Notifies only when something the
    /// surface draws moved: the board is polled every few seconds and an
    /// unchanged answer must not repaint the window.
    pub(crate) fn on_flow_event(&mut self, event: FlowEvent, cx: &mut Context<Self>) {
        let changed = match event {
            FlowEvent::Status(status) => {
                let changed = self.flow.status != status;
                if let FlowStatus::Absent { reason } = &status {
                    tracing::debug!("flow: {reason}");
                    self.flow
                        .drop_pending("warp-flow is not running, so the request was not sent.");
                }
                self.flow.status = status;
                changed
            }
            FlowEvent::Board(board) => self.flow.apply_board(*board),
            FlowEvent::Run(run) => self.flow.apply_run(*run),
            FlowEvent::Answered { id, issue, result } => {
                let key = issue_key(&issue);
                let state = self.flow.answers.entry(key).or_default();
                // A late answer for a request this surface already gave up on
                // still records the door's sentence; only a live one clears
                // its pending mark from the id.
                if state.pending == Some(id) {
                    state.pending = None;
                }
                state.outcome = Some(result);
                true
            }
            FlowEvent::Driven {
                id,
                issue,
                action,
                result,
            } => {
                let key = issue_key(&issue);
                let state = self.flow.actions.entry(key).or_default();
                if state.pending == Some(id) {
                    state.pending = None;
                }
                state.action = Some(action);
                state.outcome = Some(result);
                true
            }
        };
        if changed {
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::{self, Question, RunKey};

    /// A waiting run driven by the host, starting at `started`.
    fn run(id: &str, started: Option<&str>) -> (Run, Option<HostRun>) {
        let dir = PathBuf::from(format!("/repo/.scratch/{id}"));
        let board = Run {
            key: RunKey {
                repo: PathBuf::from("/repo"),
                id: id.to_owned(),
            },
            dir: dir.clone(),
            title: format!("run {id}"),
            question: Some(Question {
                path: dir.join("questions/01.md"),
                kind: "approve".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let host = started.map(|started| HostRun {
            issue: dir,
            title: format!("run {id}"),
            phase: Phase::Waiting,
            started: started.to_owned(),
            ..Default::default()
        });
        (board, host)
    }

    fn data(runs: Vec<(Run, Option<HostRun>)>) -> FlowData {
        let mut data = FlowData::default();
        let mut board = Board::default();
        let mut repo = flow::Repo::default();
        for (run, host) in runs {
            repo.runs.push(run);
            if let Some(host) = host {
                board.host.runs.push(host);
            }
        }
        board.repos.push(repo);
        assert!(data.apply_board(board));
        data
    }

    #[test]
    fn the_waiting_list_is_newest_first_by_the_only_instant_the_door_publishes() {
        let flow = data(vec![
            run("001-old", Some("2026-01-01T00:00:00Z")),
            run("002-new", Some("2026-03-01T00:00:00Z")),
            run("003-undated", None),
        ]);
        let ids: Vec<String> = flow.waiting().iter().map(|run| run.folder_id()).collect();
        assert_eq!(ids, ["002-new", "001-old", "003-undated"]);
    }

    #[test]
    fn a_board_replaces_and_drops_the_state_of_runs_that_left_the_band() {
        let mut flow = data(vec![run("001-demo", Some("2026-01-01T00:00:00Z"))]);
        let issue = PathBuf::from("/repo/.scratch/001-demo");
        flow.allocate();
        flow.answers.insert(
            issue_key(&issue),
            AnswerState {
                pending: None,
                question: None,
                outcome: Some(Ok(AnswerOutcome::Resumed)),
            },
        );
        assert_eq!(flow.waiting().len(), 1);
        // The next board says the run answered: it no longer waits, and its
        // outcome line goes with it.
        let mut board = Board::default();
        let mut repo = flow::Repo::default();
        repo.runs.push(Run {
            key: RunKey {
                repo: PathBuf::from("/repo"),
                id: "001-demo".into(),
            },
            dir: issue.clone(),
            title: "answered".into(),
            ..Default::default()
        });
        board.repos.push(repo);
        assert!(flow.apply_board(board));
        assert!(flow.waiting().is_empty());
        assert!(flow.answer(&issue).is_none());
    }

    #[test]
    fn a_push_replaces_one_host_run_and_only_reports_a_real_change() {
        let mut flow = data(vec![run("001-demo", Some("2026-01-01T00:00:00Z"))]);
        let issue = PathBuf::from("/repo/.scratch/001-demo");
        let mut pushed = flow.board.host.runs[0].clone();
        pushed.phase = Phase::Paused;
        assert!(flow.apply_run(pushed.clone()));
        assert!(!flow.apply_run(pushed));
        assert_eq!(
            flow.host_run(&issue).map(|run| run.phase),
            Some(Phase::Paused)
        );
    }

    #[test]
    fn an_outage_clears_every_pending_row() {
        let mut flow = data(vec![run("001-demo", None)]);
        let issue = PathBuf::from("/repo/.scratch/001-demo");
        flow.answers.insert(
            issue_key(&issue),
            AnswerState {
                pending: Some(7),
                question: None,
                outcome: None,
            },
        );
        flow.drop_pending("warp-flow is not running.");
        let state = flow.answer(&issue).expect("state");
        assert!(!state.is_pending());
        assert!(state.failed());
    }

    #[test]
    fn answer_and_action_lines_say_what_the_door_said() {
        let resumed = AnswerState {
            pending: None,
            question: None,
            outcome: Some(Ok(AnswerOutcome::Resumed)),
        };
        assert_eq!(
            resumed.line().as_deref(),
            Some("Answer sent — warp-flow resumed the run.")
        );
        let recorded = AnswerState {
            pending: None,
            question: None,
            outcome: Some(Ok(AnswerOutcome::Recorded)),
        };
        assert!(
            recorded
                .line()
                .is_some_and(|line| line.contains("recorded"))
        );
        let refused = AnswerState {
            pending: None,
            question: None,
            outcome: Some(Err("the file is gone".into())),
        };
        assert!(refused.failed());
        assert_eq!(
            refused.line().as_deref(),
            Some("warp-flow refused the answer: the file is gone")
        );
        let moved = ActionState {
            pending: None,
            action: Some(DriveAction::Pause),
            outcome: Some(Ok(Phase::Pausing)),
        };
        assert_eq!(
            moved.line().as_deref(),
            Some("warp-flow: pausing at the next step")
        );
    }
}
