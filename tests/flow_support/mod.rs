//! A fake warp-flow door for the Flow probes: it speaks the published contract
//! (`~/.warp/flow/ipc.port` + `ipc.token`, token-authenticated NDJSON,
//! protocol 1) on a loopback port, and publishes its endpoint into a directory
//! the probe owns.
//!
//! It is deliberately std-only: the client under test is the real one, and the
//! door's side must not borrow it. What it answers is the contract's frames —
//! `runs`, `run`, `answer`, `pause`/`resume`/`abandon`, `subscribe` with `run`
//! and `board` pushes — so a probe can watch the client's shapes without
//! warp-flow running.
//!
//! The state setters (`set_board`, `set_host_runs`, `fail`, `clear_question`)
//! let one scenario move the door the way warp-flow's own scan would.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

/// One answer the door received, as the wire carried it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedAnswer {
    pub issue: String,
    pub question: String,
    pub text: String,
}

/// One run action (`pause`, `resume`, `abandon`) the door received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceivedAction {
    pub verb: String,
    pub issue: String,
}

/// What the fake door answers and what it has heard.
#[derive(Default)]
pub struct DoorState {
    /// The board's `repos` array, as warp-flow's window would publish it.
    pub repos: Value,
    /// The host's `runs` array.
    pub host_runs: Value,
    pub answers: Vec<ReceivedAnswer>,
    pub actions: Vec<ReceivedAction>,
    /// `resumed` or `recorded`.
    pub answer_outcome: String,
    /// The phase a `pause`/`resume`/`abandon` answers with.
    pub action_phase: String,
    /// A verb that answers `{"ok": false, "error": …}` instead.
    pub failures: Vec<(String, String)>,
    /// How many `runs` asks this door has served.
    pub runs_calls: usize,
    /// The token this door published; a presented token must match it.
    pub token: String,
    /// The published endpoint files are in this folder.
    pub endpoint: PathBuf,
}

impl DoorState {
    fn failure(&self, verb: &str) -> Option<String> {
        self.failures
            .iter()
            .find(|(failed, _)| failed == verb)
            .map(|(_, sentence)| sentence.clone())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A bound fake door. [`FakeDoor::close`] drops the published endpoint and
/// closes every connection, so a probe can prove the client reconnects.
pub struct FakeDoor {
    dir: PathBuf,
    port: u16,
    token: String,
    running: Arc<AtomicBool>,
    state: Arc<Mutex<DoorState>>,
    /// Subscribed connections' write halves, for pushes.
    subscribers: Arc<Mutex<Vec<TcpStream>>>,
    accept: Option<JoinHandle<()>>,
}

impl FakeDoor {
    /// Bind on a free loopback port and publish the endpoint under `dir`.
    pub fn start(dir: &Path) -> Self {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).expect("fake door dir");
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind the fake door");
        let port = listener
            .local_addr()
            .expect("the fake door's address")
            .port();
        let token = format!("probe-token-{port}");
        publish_endpoint(dir, port, &token).expect("publish the fake endpoint");
        let running = Arc::new(AtomicBool::new(true));
        let state = Arc::new(Mutex::new(DoorState {
            answer_outcome: "resumed".to_owned(),
            action_phase: "pausing".to_owned(),
            repos: json!([]),
            host_runs: json!([]),
            token: token.clone(),
            endpoint: dir.to_path_buf(),
            ..Default::default()
        }));
        let subscribers: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
        let accept = {
            let running = Arc::clone(&running);
            let state = Arc::clone(&state);
            let subscribers = Arc::clone(&subscribers);
            std::thread::Builder::new()
                .name("fake-flow-door".to_owned())
                .spawn(move || accept_loop(listener, running, state, subscribers))
                .expect("fake door thread")
        };
        Self {
            dir: dir.to_path_buf(),
            port,
            token,
            running,
            state,
            subscribers,
            accept: Some(accept),
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn endpoint_dir(&self) -> &Path {
        &self.dir
    }

    /// Replace the board's `repos` array (the shape `runs` answers).
    pub fn set_board(&self, repos: Value) {
        lock(&self.state).repos = repos;
    }

    /// Replace the host's live `runs` array.
    pub fn set_host_runs(&self, runs: Value) {
        lock(&self.state).host_runs = runs;
    }

    /// Remove a run's open question from the board, as warp-flow's scan would
    /// once the answer is written and the run resumes.
    pub fn clear_questions(&self) {
        let mut state = lock(&self.state);
        if let Some(repos) = state.repos.as_array_mut() {
            for repo in repos {
                if let Some(runs) = repo.get_mut("runs").and_then(Value::as_array_mut) {
                    for run in runs {
                        if let Some(object) = run.as_object_mut() {
                            object.insert("question".to_owned(), Value::Null);
                            object.insert("open_questions".to_owned(), json!([]));
                        }
                    }
                }
            }
        }
    }

    /// Make one verb refuse with the door's own sentence.
    pub fn fail(&self, verb: &str, sentence: &str) {
        lock(&self.state)
            .failures
            .push((verb.to_owned(), sentence.to_owned()));
    }

    pub fn set_answer_outcome(&self, outcome: &str) {
        lock(&self.state).answer_outcome = outcome.to_owned();
    }

    pub fn set_action_phase(&self, phase: &str) {
        lock(&self.state).action_phase = phase.to_owned();
    }

    pub fn answers(&self) -> Vec<ReceivedAnswer> {
        lock(&self.state).answers.clone()
    }

    pub fn actions(&self) -> Vec<ReceivedAction> {
        lock(&self.state).actions.clone()
    }

    pub fn runs_calls(&self) -> usize {
        lock(&self.state).runs_calls
    }

    /// Push one `{"push": "run", "run": …}` frame to every subscriber.
    pub fn push_run(&self, run: Value) {
        self.push(json!({"push": "run", "run": run}));
    }

    /// Push the contentless board notification.
    pub fn push_board(&self) {
        self.push(json!({"push": "board"}));
    }

    fn push(&self, frame: Value) {
        let line = format!("{frame}\n");
        let mut subscribers = lock(&self.subscribers);
        subscribers.retain_mut(|stream| {
            stream.write_all(line.as_bytes()).is_ok() && stream.flush().is_ok()
        });
    }

    /// Stop the door: drop the endpoint files and close every connection. The
    /// client sees the files gone and both sockets closed, exactly as a quit
    /// warp-flow would look.
    pub fn close(mut self) {
        self.running.store(false, Ordering::SeqCst);
        for stream in lock(&self.subscribers).drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        // wake the blocking accept
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        // only ours: the folder may already hold a newer door's files
        remove_if_ours(&self.dir, self.port, &self.token);
    }
}

impl Drop for FakeDoor {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        for stream in lock(&self.subscribers).drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        remove_if_ours(&self.dir, self.port, &self.token);
    }
}

fn publish_endpoint(dir: &Path, port: u16, token: &str) -> std::io::Result<()> {
    // The real door publishes both files 0600; a probe's endpoint only needs
    // to be readable by the client under test, so the mode is a Unix nicety
    // and the write is plain on Windows (the app's CI runs both).
    let write = |name: &str, value: &str| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(dir.join(name))?;
        file.write_all(value.as_bytes())
    };
    // token first, the way the family publishes
    write("ipc.token", token)?;
    write("ipc.port", &format!("{port}\n"))
}

fn remove_if_ours(dir: &Path, port: u16, token: &str) {
    let ours = |name: &str, value: &str| {
        std::fs::read_to_string(dir.join(name)).is_ok_and(|current| current.trim() == value)
    };
    let port = port.to_string();
    if ours("ipc.port", &port) {
        let _ = std::fs::remove_file(dir.join("ipc.port"));
    }
    if ours("ipc.token", token) {
        let _ = std::fs::remove_file(dir.join("ipc.token"));
    }
}

fn accept_loop(
    listener: TcpListener,
    running: Arc<AtomicBool>,
    state: Arc<Mutex<DoorState>>,
    subscribers: Arc<Mutex<Vec<TcpStream>>>,
) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { break };
        if !running.load(Ordering::SeqCst) {
            break;
        }
        let running = Arc::clone(&running);
        let state = Arc::clone(&state);
        let subscribers = Arc::clone(&subscribers);
        let _ = std::thread::Builder::new()
            .name("fake-flow-conn".to_owned())
            .spawn(move || serve_connection(stream, running, state, subscribers));
    }
}

fn serve_connection(
    stream: TcpStream,
    running: Arc<AtomicBool>,
    state: Arc<Mutex<DoorState>>,
    subscribers: Arc<Mutex<Vec<TcpStream>>>,
) {
    let _ = stream.set_nodelay(true);
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let mut writer = write_half;
    let mut reader = BufReader::new(stream);
    // The family's handshake: one token line, one verdict. The door compares
    // against the token it published, not against the file — a probe proving
    // the auth refusal overwrites the file with a wrong one.
    let Some(line) = read_line(&mut reader) else {
        return;
    };
    let expected = lock(&state).token.clone();
    let presented = serde_json::from_str::<Value>(&line)
        .ok()
        .and_then(|frame| frame.get("auth").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default();
    if presented != expected {
        let _ = writeln!(writer, "{}", json!({"ok": false, "error": "auth"}));
        let _ = writer.flush();
        return;
    }
    if writeln!(writer, "{}", json!({"ok": true})).is_err() {
        return;
    }
    let _ = writer.flush();

    loop {
        let Some(line) = read_line(&mut reader) else {
            return;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            let _ = writeln!(
                writer,
                "{}",
                json!({"id": 0, "ok": false, "error": "bad frame"})
            );
            continue;
        };
        let id = request.get("id").and_then(Value::as_u64).unwrap_or(0);
        let verb = request.get("cmd").and_then(Value::as_str).unwrap_or("");
        if verb == "subscribe" {
            let _ = writeln!(writer, "{}", json!({"id": id, "ok": true, "protocol": 1}));
            let _ = writer.flush();
            // A subscribed connection is a stream from now on: keep it open
            // (and keep writing pushes through the registered clone) until the
            // door is closed.
            lock(&subscribers).push(writer.try_clone().expect("clone the subscriber"));
            while running.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(20));
            }
            return;
        }
        let answer = match verb {
            "ping" => Ok(json!({"app": "warp-flow", "protocol": 1, "schema_version": 1})),
            "runs" => {
                let mut state = lock(&state);
                state.runs_calls += 1;
                Ok(json!({
                    "schema_version": 1,
                    "repos": state.repos,
                    "host": {"schema_version": 1, "runs": state.host_runs},
                }))
            }
            "run" => {
                let issue = request
                    .get("issue")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let state = lock(&state);
                let board_run = find_run(&state.repos, &issue);
                let host_run = state
                    .host_runs
                    .as_array()
                    .and_then(|runs| {
                        runs.iter().find(|run| {
                            run.get("issue").and_then(Value::as_str) == Some(issue.as_str())
                        })
                    })
                    .cloned();
                if board_run.is_none() && host_run.is_none() {
                    Err(format!("no run for issue `{issue}`"))
                } else {
                    Ok(json!({
                        "schema_version": 1,
                        "run": board_run.unwrap_or(Value::Null),
                        "host": host_run.unwrap_or(Value::Null),
                    }))
                }
            }
            "answer" => {
                let mut state = lock(&state);
                let issue = request
                    .get("issue")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let question = request
                    .get("question")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let text = request
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if let Some(error) = state.failure("answer") {
                    Err(error)
                } else {
                    let outcome = state.answer_outcome.clone();
                    state.answers.push(ReceivedAnswer {
                        issue,
                        question,
                        text,
                    });
                    Ok(json!({"outcome": outcome}))
                }
            }
            "pause" | "resume" | "abandon" => {
                let mut state = lock(&state);
                let issue = request
                    .get("issue")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if let Some(error) = state.failure(verb) {
                    Err(error)
                } else {
                    let phase = state.action_phase.clone();
                    state.actions.push(ReceivedAction {
                        verb: verb.to_owned(),
                        issue,
                    });
                    Ok(json!({"phase": phase}))
                }
            }
            other => Err(format!("unknown command: {other}")),
        };
        match answer {
            Ok(payload) => {
                let mut frame = json!({"id": id, "ok": true});
                if let (Some(target), Some(extra)) = (frame.as_object_mut(), payload.as_object()) {
                    for (key, value) in extra {
                        target.insert(key.clone(), value.clone());
                    }
                }
                let _ = writeln!(writer, "{frame}");
            }
            Err(error) => {
                let _ = writeln!(writer, "{}", json!({"id": id, "ok": false, "error": error}));
            }
        }
        let _ = writer.flush();
    }
}

fn find_run(repos: &Value, issue: &str) -> Option<Value> {
    repos
        .as_array()?
        .iter()
        .flat_map(|repo| {
            repo.get("runs")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .find(|run| run.get("dir").and_then(Value::as_str) == Some(issue))
        .cloned()
}

fn read_line(reader: &mut BufReader<TcpStream>) -> Option<String> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => None,
        Ok(_) => Some(line),
        Err(_) => None,
    }
}

// ---- payload builders ------------------------------------------------------

/// The `repos` array the board answers, with one run.
pub fn repos_with_run(repo: &Path, run: Value) -> Value {
    json!([{
        "path": repo.to_string_lossy(),
        "name": repo.file_name().map(|name| name.to_string_lossy().into_owned()),
        "runs": [run],
    }])
}

/// One board run, as warp-flow's scan publishes it: only the fields the Flow
/// surface reads plus the ones it deliberately ignores (`column`, `status`,
/// `step`) so the client's tolerance is exercised.
pub fn board_run(repo: &Path, id: &str, dir: &Path, title: &str, questions: Value) -> Value {
    json!({
        "key": {"repo": repo.to_string_lossy(), "id": id},
        "dir": dir.to_string_lossy(),
        "title": title,
        "has_run": true,
        "status": "waiting",
        "column": "waiting",
        "step": "analyze",
        "workflow": "default@1",
        "attempts": 1,
        "tasks_total": 0,
        "tasks_done": 0,
        "task_rows": [],
        "task_levels": [],
        "question": questions.as_array().and_then(|all| all.first().cloned()),
        "open_questions": questions,
        "rows": [],
        "steps": [],
        "spec": "# A demo\n",
        "tasks": "",
        "note": Value::Null,
    })
}

/// One open question file, as the board carries it.
pub fn question(
    path: &Path,
    kind: &str,
    title: &str,
    body: &str,
    asked_by: &str,
    options: &[&str],
) -> Value {
    json!({
        "path": path.to_string_lossy(),
        "kind": kind,
        "title": title,
        "body": body,
        "asked_by": asked_by,
        "task": Value::Null,
        "options": options,
    })
}

/// One `answer` frame: what the door's `host` carries for a run it drives.
pub fn host_run(issue: &Path, title: &str, phase: &str, started: &str, log: &[&str]) -> Value {
    json!({
        "issue": issue.to_string_lossy(),
        "repo": Value::Null,
        "title": title,
        "phase": phase,
        "status": Value::Null,
        "started": started,
        "finished": Value::Null,
        "error": Value::Null,
        "log": log,
    })
}
