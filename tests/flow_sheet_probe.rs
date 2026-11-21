//! The real window's Flow sheet over a fake warp-flow door: the waiting band
//! renders a question with its options, a click sends `answer` through the real
//! client and the door's outcome shows, the moving band shows a phase and
//! offers the verb that phase allows, and an absent door is one line with a
//! retry that reconnects without a restart.
//!
//! The wire itself is `flow_client_probe`; this probe proves the surface, the
//! store and the client agree, through the library boundary the app ships.

mod flow_support;

use gpui::{Modifiers, TestAppContext, VisualTestContext};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use warp_agent::flow::{self, FlowEvent, Timing};
use warp_agent::store::{Store, UiEvent};
use warp_agent::ui::window::WarpWindow;

use flow_support::{FakeDoor, board_run, host_run, question, repos_with_run};

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// A scratch home so `Store::new` neither reads nor writes the developer's
/// `~/.warp/agent`. Tests in one binary share the process environment, so the
/// scenarios take turns behind `HOME_LOCK`.
struct HomeGuard {
    home: PathBuf,
    old_home: Option<OsString>,
    old_profile: Option<OsString>,
}

impl HomeGuard {
    fn new(name: &str) -> Self {
        let home =
            std::env::temp_dir().join(format!("warp-agent-probe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("scratch home");
        let old_home = std::env::var_os("HOME");
        let old_profile = std::env::var_os("USERPROFILE");
        // edition 2024 marks the process environment as unsafe to mutate
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("USERPROFILE", &home);
        }
        Self {
            home,
            old_home,
            old_profile,
        }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.old_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &self.old_profile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// `WARP_AGENT_FLOW=1` for the length of one scenario; the window reads it
/// once in `WarpWindow::new`.
struct FlowEnv {
    old: Option<OsString>,
}

impl FlowEnv {
    fn on() -> Self {
        let old = std::env::var_os("WARP_AGENT_FLOW");
        unsafe { std::env::set_var("WARP_AGENT_FLOW", "1") };
        Self { old }
    }
}

impl Drop for FlowEnv {
    fn drop(&mut self) {
        unsafe {
            match &self.old {
                Some(value) => std::env::set_var("WARP_AGENT_FLOW", value),
                None => std::env::remove_var("WARP_AGENT_FLOW"),
            }
        }
    }
}

/// The client, its runtime and the fake door one scenario uses. The command
/// channel is the store's own (`Store::for_probe_with_flow`), so a click in
/// the real view reaches the real client.
struct FlowProbe {
    door: FakeDoor,
    _runtime: tokio::runtime::Runtime,
    events: async_channel::Receiver<FlowEvent>,
}

impl FlowProbe {
    /// Start the real client against `door` on its own runtime, the way
    /// `oc::net_main` spawns it on the app's.
    fn new(door: FakeDoor, commands: async_channel::Receiver<flow::FlowCommand>) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("the probe's client runtime");
        let (event_tx, events) = async_channel::unbounded();
        runtime.spawn(flow::drive_with(
            door.endpoint_dir().to_path_buf(),
            event_tx,
            commands,
            Timing {
                poll: Duration::from_millis(80),
                retry: Duration::from_millis(80),
                call: Duration::from_secs(2),
            },
        ));
        Self {
            door,
            _runtime: runtime,
            events,
        }
    }
}

/// The app's first-frame setup, in the order `run()` does it: the kit, the
/// theme, and the Flow sheet's Escape binding (which the app registers in
/// `run()`, outside the library boundary a probe drives).
fn init_kit(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        let fonts = warp_agent::theme::active_tokens();
        warp_agent::theme::apply_kit_theme(warp_agent::theme::accent_colors(), &fonts, cx);
        cx.bind_keys([
            gpui::KeyBinding::new("escape", warp_agent::ui::flow::FlowClose, Some("Flow")),
            gpui::KeyBinding::new(
                "escape",
                warp_agent::ui::flow::FlowClose,
                Some("Flow > Input"),
            ),
        ]);
    });
}

fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, cx| drop(window.draw(cx)));
}

/// Apply every client event the store has not seen yet, on the UI thread, the
/// way the app's one pump does.
fn pump(
    cx: &mut VisualTestContext,
    store: &gpui::Entity<Store>,
    events: &async_channel::Receiver<FlowEvent>,
) {
    while let Ok(event) = events.try_recv() {
        cx.update(|_, cx| {
            store.update(cx, |store, cx| {
                store.on_net_event(UiEvent::Flow { event }, cx);
            });
        });
    }
}

/// Pump and draw until `wanted` holds, or fail after `timeout`.
fn wait_until(
    cx: &mut VisualTestContext,
    store: &gpui::Entity<Store>,
    events: &async_channel::Receiver<FlowEvent>,
    mut wanted: impl FnMut(&mut VisualTestContext) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        pump(cx, store, events);
        draw(cx);
        if wanted(cx) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the condition never held; the client or the sheet is stuck"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The waiting run and the moving run one scenario seeds.
struct Seed {
    issue: PathBuf,
    question: PathBuf,
    moving_issue: PathBuf,
}

/// Seed a door with a waiting run on an `approve` question and a paused run
/// that this window's host reports.
fn seed(door: &FakeDoor, home: &std::path::Path) -> Seed {
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
    let moving_issue = repo.join(".scratch").join("002-moving");
    let questions = serde_json::json!([question(
        &question_path,
        "approve",
        "Question 01",
        "# Question 01\n\nGo on?",
        "analyze",
        &["yes", "no"],
    )]);
    door.set_board(repos_with_run(
        &repo,
        board_run(&repo, "001-demo", &issue, "Waiting on a person", questions),
    ));
    door.set_host_runs(serde_json::json!([host_run(
        &moving_issue,
        "A run that moves",
        "paused",
        "2026-01-01T00:00:00Z",
        &["step analyze paused at a boundary"],
    )]));
    Seed {
        issue,
        question: question_path,
        moving_issue,
    }
}

/// The sheet entity and its open state, for the probe.
fn sheet(
    cx: &VisualTestContext,
    window: &gpui::Entity<WarpWindow>,
) -> gpui::Entity<warp_agent::ui::flow::FlowSheet> {
    window.read_with(cx, |window, _| window.flow_sheet_for_probe())
}

fn click_at(cx: &mut VisualTestContext, selector: &'static str) {
    let bounds = cx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} draws"));
    cx.simulate_click(bounds.center(), Modifiers::default());
    cx.run_until_parked();
}

#[gpui::test]
fn the_flow_sheet_shows_what_waits_and_what_moves(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("flow-sheet");
    init_kit(cx);
    let door = FakeDoor::start(&home.home.join("flow-door"));
    let seed = seed(&door, &home.home);
    let _env = FlowEnv::on();

    let (store, flow_rx) = cx.update(Store::for_probe_with_flow);
    let probe = FlowProbe::new(door, flow_rx);
    // Let the board land before the first frame, so the sheet opens on real
    // content rather than on the looking line.
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        while let Ok(event) = probe.events.try_recv() {
            cx.update(|cx| {
                store.update(cx, |store, cx| {
                    store.on_net_event(UiEvent::Flow { event }, cx);
                });
            });
        }
        let waiting = cx.update(|cx| store.read(cx).flow().waiting().len());
        if waiting == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "the board arrives");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (window, cx) = cx.add_window_view(|_, cx| WarpWindow::new(store.clone(), cx));
    draw(cx);

    // The card is up and holds the keyboard.
    assert!(sheet(cx, &window).read_with(cx, |sheet, _| sheet.is_open()));
    assert!(cx.debug_bounds("flow-card").is_some(), "the card draws");
    assert!(
        cx.debug_bounds("flow-waiting-card").is_some(),
        "the waiting run draws a card"
    );
    assert!(
        cx.debug_bounds("flow-answer-001-demo-0").is_some(),
        "the approve question offers yes"
    );
    assert!(
        cx.debug_bounds("flow-answer-001-demo-1").is_some(),
        "the approve question offers no"
    );
    assert!(
        cx.debug_bounds("flow-question-body").is_some(),
        "the question body renders read-only markdown"
    );

    // The moving band: the phase word and only the verb the phase allows.
    assert!(cx.debug_bounds("flow-moving-card").is_some());
    assert!(cx.debug_bounds("flow-phase").is_some());
    assert!(
        cx.debug_bounds("flow-action-resume-002-moving").is_some(),
        "a paused run offers Resume"
    );
    assert!(
        cx.debug_bounds("flow-action-pause-002-moving").is_none(),
        "a paused run offers no Pause"
    );
    assert!(
        cx.debug_bounds("flow-action-abandon-002-moving").is_none(),
        "a paused run offers no Abandon"
    );

    // The list owns the arrows: the cursor walks the two approve rows.
    let cursor = |cx: &VisualTestContext| {
        sheet(cx, &window).read_with(cx, |sheet, _| sheet.cursor_action_for_probe())
    };
    assert_eq!(cursor(cx).as_deref(), Some("answer:yes"));
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    assert_eq!(cursor(cx).as_deref(), Some("answer:no"));
    cx.simulate_keystrokes("home");
    cx.run_until_parked();
    assert_eq!(cursor(cx).as_deref(), Some("answer:yes"));
    cx.simulate_keystrokes("end");
    cx.run_until_parked();
    assert_eq!(
        cursor(cx).as_deref(),
        Some("drive:resume"),
        "End lands on the last action row"
    );

    // Answering sends `answer` and the door's outcome shows on the question.
    click_at(cx, "flow-answer-001-demo-0");
    wait_until(cx, &store, &probe.events, |cx| {
        cx.debug_bounds("flow-answer-outcome").is_some()
    });
    let answers = probe.door.answers();
    assert_eq!(answers.len(), 1, "one answer went over the wire");
    assert_eq!(answers[0].issue, seed.issue.to_string_lossy());
    assert_eq!(answers[0].question, seed.question.to_string_lossy());
    assert_eq!(answers[0].text, "yes");
    let line = store
        .read_with(cx, |store, _| {
            store
                .flow()
                .answer(&seed.issue)
                .and_then(|state| state.line())
        })
        .expect("the outcome line");
    assert!(
        line.contains("resumed"),
        "the door's outcome is shown: {line}"
    );

    // The next poll says the run no longer waits: it leaves the band, and its
    // outcome line with it.
    probe.door.clear_questions();
    wait_until(cx, &store, &probe.events, |cx| {
        cx.debug_bounds("flow-waiting-card").is_none()
    });
    assert!(
        store.read_with(cx, |store, _| store.flow().waiting().is_empty()),
        "the waiting band empties with the question"
    );

    // Resume a paused run through the cursor: Enter runs the row the keyboard
    // is on, and the door's phase answers.
    cx.simulate_keystrokes("end");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    wait_until(cx, &store, &probe.events, |cx| {
        cx.debug_bounds("flow-action-line").is_some()
    });
    let actions = probe.door.actions();
    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].verb, "resume");
    assert_eq!(actions[0].issue, seed.moving_issue.to_string_lossy());

    // Escape closes the card through the bound action.
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    draw(cx);
    assert!(
        !sheet(cx, &window).read_with(cx, |sheet, _| sheet.is_open()),
        "Escape closes the sheet"
    );

    drop(probe);
}

#[gpui::test]
fn an_absent_door_is_one_line_and_its_retry_reconnects(cx: &mut TestAppContext) {
    let _lock = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = HomeGuard::new("flow-absent");
    init_kit(cx);
    let door_dir = home.home.join("flow-door");
    let _env = FlowEnv::on();

    let (store, flow_rx) = cx.update(Store::for_probe_with_flow);
    // No door yet: the client keeps looking and the sheet says so.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("the probe's client runtime");
    let (event_tx, events) = async_channel::unbounded();
    runtime.spawn(flow::drive_with(
        door_dir.clone(),
        event_tx,
        flow_rx,
        Timing {
            poll: Duration::from_millis(80),
            retry: Duration::from_millis(80),
            call: Duration::from_secs(2),
        },
    ));
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        while let Ok(event) = events.try_recv() {
            cx.update(|cx| {
                store.update(cx, |store, cx| {
                    store.on_net_event(UiEvent::Flow { event }, cx);
                });
            });
        }
        let absent = cx.update(|cx| {
            matches!(
                store.read(cx).flow().status(),
                warp_agent::flow::FlowStatus::Absent { .. }
            )
        });
        if absent {
            break;
        }
        assert!(Instant::now() < deadline, "the door is absent");
        std::thread::sleep(Duration::from_millis(20));
    }
    let (_window, cx) = cx.add_window_view(|_, cx| WarpWindow::new(store.clone(), cx));
    draw(cx);
    assert!(
        cx.debug_bounds("flow-absent").is_some(),
        "the absent state draws its one line"
    );
    assert!(
        cx.debug_bounds("flow-waiting-card").is_none(),
        "an absent door shows no stale runs"
    );

    // warp-flow starts: the Retry button reconnects, no restart needed.
    let door = FakeDoor::start(&door_dir);
    let seed = seed(&door, &home.home);
    click_at(cx, "flow-retry");
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        while let Ok(event) = events.try_recv() {
            cx.update(|_, cx| {
                store.update(cx, |store, cx| {
                    store.on_net_event(UiEvent::Flow { event }, cx);
                });
            });
        }
        draw(cx);
        if cx.debug_bounds("flow-waiting-card").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Retry finds the door that just started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        store.read_with(cx, |store, _| store.flow().waiting().len() == 1),
        "the retried board is the door's"
    );
    let _ = seed;
    drop((door, runtime));
}
