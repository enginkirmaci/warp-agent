//! The Flow client against a fake warp-flow door: absence, the auth refusal,
//! the `runs`/`run`/`answer`/action round trips, the subscription's pushes, the
//! board notification's re-ask, and a reconnect after the door closes and
//! comes back.
//!
//! The door is `tests/flow_support`: std threads speaking the published
//! contract, so these tests prove the client's shapes without warp-flow
//! running. The sheet's own probe drives the same client through the real
//! window.

mod flow_support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_channel::Receiver;
use serde_json::json;
use warp_agent::flow::{
    self, AnswerOutcome, CallError, DriveAction, FlowEvent, FlowStatus, Phase, Timing,
};

use flow_support::{FakeDoor, board_run, host_run, question, repos_with_run};

/// A disposable endpoint folder under the system temp dir.
fn endpoint_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "warp-agent-flow-client-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch endpoint dir");
    dir
}

/// Fast clocks: a probe waits in tens of milliseconds, not in seconds.
fn timing() -> Timing {
    Timing {
        poll: Duration::from_millis(80),
        retry: Duration::from_millis(80),
        call: Duration::from_secs(2),
    }
}

/// The repository and issue folder one scenario seeds.
struct Seed {
    repo: PathBuf,
    issue: PathBuf,
    question: PathBuf,
}

/// Seed one repository with one run that waits on an `approve` question.
fn seed(door: &FakeDoor, home: &Path, title: &str) -> Seed {
    let repo = home.join("repo");
    let issue = repo.join(".scratch").join("001-demo");
    let question_path = issue.join("questions").join("01.md");
    std::fs::create_dir_all(question_path.parent().expect("question dir"))
        .expect("seed the question folder");
    std::fs::write(
        &question_path,
        "---\nkind: approve\noptions: yes, no\nstate: open\n---\n\n# Question 01\n\nGo on?\n",
    )
    .expect("seed the question file");
    let questions = json!([question(
        &question_path,
        "approve",
        "Question 01",
        "# Question 01\n\nGo on?",
        "analyze",
        &["yes", "no"],
    )]);
    door.set_board(repos_with_run(
        &repo,
        board_run(&repo, "001-demo", &issue, title, questions),
    ));
    Seed {
        repo,
        issue,
        question: question_path,
    }
}

async fn next_event(events: &Receiver<FlowEvent>) -> FlowEvent {
    tokio::time::timeout(Duration::from_secs(3), events.recv())
        .await
        .expect("the client says something")
        .expect("the client's event channel stays open")
}

async fn wait_for(
    events: &Receiver<FlowEvent>,
    mut wanted: impl FnMut(&FlowEvent) -> bool,
) -> FlowEvent {
    for _ in 0..80 {
        let event = next_event(events).await;
        if wanted(&event) {
            return event;
        }
    }
    panic!("the event never arrived");
}

fn board_title(event: &FlowEvent) -> Option<String> {
    match event {
        FlowEvent::Board(board) => board.runs().next().map(|run| run.title.clone()),
        _ => None,
    }
}

#[tokio::test]
async fn an_endpoint_that_was_never_published_is_absent() {
    let dir = endpoint_dir("absent");
    let error = match flow::connect_in(&dir).await {
        Ok(_) => panic!("no endpoint, no connection"),
        Err(error) => error,
    };
    assert!(
        error.reason.contains("has not published"),
        "the absence names the door: {}",
        error.reason
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_wrong_token_is_refused_as_absence() {
    let dir = endpoint_dir("auth");
    let door = FakeDoor::start(&dir);
    // The door keeps the token it published; overwriting the file makes the
    // client present a token the door does not know, as a stale file after a
    // restart would.
    std::fs::write(dir.join("ipc.token"), "an-old-launch-token\n").expect("wrong token file");
    let error = match flow::connect_in(&dir).await {
        Ok(_) => panic!("a wrong token is refused"),
        Err(error) => error,
    };
    assert!(
        error.reason.contains("refused this session's token"),
        "the refusal names the cause: {}",
        error.reason
    );
    door.close();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn runs_and_run_answer_the_board_and_its_detail() {
    let dir = endpoint_dir("runs");
    let home = endpoint_dir("runs-home");
    let door = FakeDoor::start(&dir);
    let seed = seed(&door, &home, "A demo");
    door.set_host_runs(json!([host_run(
        &seed.issue,
        "A demo",
        "running",
        "2026-01-01T00:00:00Z",
        &["step analyze", "waiting for an answer"],
    )]));

    let mut connection = flow::connect_in(&dir).await.expect("the door is there");
    let board = connection.runs().await.expect("the board");
    assert_eq!(board.repo_count(), 1);
    let run = board.run(&seed.issue).expect("the run");
    assert_eq!(run.title, "A demo");
    assert_eq!(run.folder_id(), "001-demo");
    assert_eq!(run.repo_name(), "repo");
    assert!(run.waits());
    let open = run.questions();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].kind, "approve");
    assert_eq!(open[0].asked_by_label(), "Step `analyze`");
    assert_eq!(open[0].control_kind(), flow::QuestionKind::Approve);
    assert_eq!(open[0].choices(), vec!["yes".to_owned(), "no".to_owned()]);
    assert_eq!(board.host.runs.len(), 1);
    assert_eq!(board.host.runs[0].phase, Phase::Running);

    let detail = connection.run(&seed.issue).await.expect("the detail");
    assert_eq!(
        detail.run.as_ref().map(|run| run.title.as_str()),
        Some("A demo")
    );
    assert_eq!(
        detail.host.as_ref().map(|run| run.phase),
        Some(Phase::Running)
    );

    // A folder the door does not know is the door's sentence, not a read
    // failure.
    let refusal = connection
        .run(Path::new("/no/such/issue"))
        .await
        .expect_err("an unknown issue is refused");
    assert!(
        matches!(refusal, CallError::Refused(ref sentence) if sentence.contains("no run for issue"))
    );

    door.close();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn answer_reports_the_doors_outcome_and_carries_the_text() {
    let dir = endpoint_dir("answer");
    let home = endpoint_dir("answer-home");
    let door = FakeDoor::start(&dir);
    let seed = seed(&door, &home, "A demo");
    let mut connection = flow::connect_in(&dir).await.expect("the door is there");

    door.set_answer_outcome("resumed");
    let outcome = connection
        .answer(&seed.issue, &seed.question, "yes")
        .await
        .expect("the answer is written");
    assert_eq!(outcome, AnswerOutcome::Resumed);
    let received = door.answers();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].issue, seed.issue.to_string_lossy().into_owned());
    assert_eq!(
        received[0].question,
        seed.question.to_string_lossy().into_owned()
    );
    assert_eq!(received[0].text, "yes");

    // The other outcome the contract names reads the same way.
    door.set_answer_outcome("recorded");
    let outcome = connection
        .answer(&seed.issue, &seed.question, "no")
        .await
        .expect("the answer is recorded");
    assert_eq!(outcome, AnswerOutcome::Recorded);

    // A refusal stays the door's sentence and does not look like a dead door.
    door.fail("answer", "questions/01.md is not a question in this folder");
    let refusal = connection
        .answer(&seed.issue, &seed.question, "yes")
        .await
        .expect_err("a refusal is an error");
    assert!(!refusal.is_closed(), "a refusal keeps the connection");
    assert!(
        refusal.sentence().contains("not a question"),
        "the door's own sentence rides through: {}",
        refusal.sentence()
    );

    door.close();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn run_actions_answer_the_phase_and_refusals_keep_the_connection() {
    let dir = endpoint_dir("actions");
    let home = endpoint_dir("actions-home");
    let door = FakeDoor::start(&dir);
    let seed = seed(&door, &home, "A demo");
    let mut connection = flow::connect_in(&dir).await.expect("the door is there");

    door.set_action_phase("pausing");
    assert_eq!(
        connection
            .drive(&seed.issue, DriveAction::Pause)
            .await
            .expect("pause is accepted"),
        Phase::Pausing
    );
    assert_eq!(
        door.actions(),
        vec![flow_support::ReceivedAction {
            verb: "pause".to_owned(),
            issue: seed.issue.to_string_lossy().into_owned(),
        }]
    );

    door.set_action_phase("running");
    assert_eq!(
        connection
            .drive(&seed.issue, DriveAction::Resume)
            .await
            .expect("resume is accepted"),
        Phase::Running
    );

    door.fail(
        "abandon",
        "This run is not being driven; there is nothing to abandon.",
    );
    let refusal = connection
        .drive(&seed.issue, DriveAction::Abandon)
        .await
        .expect_err("abandon is refused");
    assert!(!refusal.is_closed());
    assert!(refusal.sentence().contains("nothing to abandon"));

    door.close();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_subscription_receives_a_run_push() {
    let dir = endpoint_dir("subscribe");
    let home = endpoint_dir("subscribe-home");
    let door = FakeDoor::start(&dir);
    let seed = seed(&door, &home, "A demo");
    let mut subscription = flow::subscribe_in(&dir)
        .await
        .expect("the subscription is acked");
    // The door acks before it registers the stream, so give its connection
    // thread the moment the contract gives it.
    tokio::time::sleep(Duration::from_millis(60)).await;

    door.push_run(host_run(
        &seed.issue,
        "A demo",
        "paused",
        "2026-01-01T00:00:00Z",
        &["step analyze paused"],
    ));
    let frame = tokio::time::timeout(Duration::from_secs(2), subscription.next())
        .await
        .expect("the push arrives")
        .expect("the subscription stays open");
    assert_eq!(frame["push"], "run");
    assert_eq!(
        frame["run"]["issue"].as_str(),
        Some(seed.issue.to_string_lossy().as_ref())
    );
    assert_eq!(frame["run"]["phase"], "paused");
    assert_eq!(frame["run"]["log"][0], "step analyze paused");

    door.close();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_board_push_asks_runs_again() {
    let dir = endpoint_dir("board-push");
    let home = endpoint_dir("board-push-home");
    let door = FakeDoor::start(&dir);
    let seed = seed(&door, &home, "Before the push");
    let (events_tx, events_rx) = async_channel::unbounded();
    let (commands_tx, commands_rx) = async_channel::unbounded();
    // A poll far slower than the test: any second board must come from the
    // push, not from the timer.
    let slow = Timing {
        poll: Duration::from_secs(30),
        retry: Duration::from_millis(80),
        call: Duration::from_secs(2),
    };
    tokio::spawn(flow::drive_with(dir.clone(), events_tx, commands_rx, slow));

    let first = wait_for(&events_rx, |event| {
        board_title(event).as_deref() == Some("Before the push")
    })
    .await;
    assert!(matches!(first, FlowEvent::Board(_)));
    let calls_before = door.runs_calls();

    // The board changes, and the door only says "ask again".
    let questions = json!([question(
        &seed.question,
        "approve",
        "Question 01",
        "# Question 01\n\nGo on?",
        "analyze",
        &["yes", "no"],
    )]);
    door.set_board(repos_with_run(
        &seed.repo,
        board_run(
            &seed.repo,
            "001-demo",
            &seed.issue,
            "After the push",
            questions,
        ),
    ));
    door.push_board();
    wait_for(&events_rx, |event| {
        board_title(event).as_deref() == Some("After the push")
    })
    .await;
    assert!(
        door.runs_calls() > calls_before,
        "the board notification is a re-ask: {} calls before, {} after",
        calls_before,
        door.runs_calls()
    );

    drop(commands_tx);
    door.close();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn the_client_reconnects_after_the_door_closes_and_comes_back() {
    let dir = endpoint_dir("reconnect");
    let home = endpoint_dir("reconnect-home");
    let door = FakeDoor::start(&dir);
    let seed = seed(&door, &home, "Before the restart");

    let (events_tx, events_rx) = async_channel::unbounded();
    let (commands_tx, commands_rx) = async_channel::unbounded();
    tokio::spawn(flow::drive_with(
        dir.clone(),
        events_tx,
        commands_rx,
        timing(),
    ));
    wait_for(&events_rx, |event| {
        board_title(event).as_deref() == Some("Before the restart")
    })
    .await;

    // warp-flow quits: the endpoint files go and both sockets close.
    door.close();
    let absent = wait_for(&events_rx, |event| {
        matches!(event, FlowEvent::Status(FlowStatus::Absent { .. }))
    })
    .await;
    assert!(matches!(
        absent,
        FlowEvent::Status(FlowStatus::Absent { .. })
    ));

    // A new launch publishes a new port and token; the client re-reads them.
    let door = FakeDoor::start(&dir);
    let title = "After the restart";
    let questions = json!([question(
        &seed.question,
        "approve",
        "Question 01",
        "# Question 01\n\nGo on?",
        "analyze",
        &["yes", "no"],
    )]);
    door.set_board(repos_with_run(
        &seed.repo,
        board_run(&seed.repo, "001-demo", &seed.issue, title, questions),
    ));
    let ready = wait_for(&events_rx, |event| {
        matches!(event, FlowEvent::Status(FlowStatus::Ready))
    })
    .await;
    assert!(matches!(ready, FlowEvent::Status(FlowStatus::Ready)));
    wait_for(&events_rx, |event| {
        board_title(event).as_deref() == Some(title)
    })
    .await;

    drop(commands_tx);
    door.close();
    let _ = std::fs::remove_dir_all(home);
    let _ = std::fs::remove_dir_all(dir);
}
