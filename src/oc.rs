//! opencode server + REST/SSE protocol client + the network thread.
//!
//! All network work happens on a dedicated thread running its own tokio
//! runtime; the GPUI UI talks to it through two async channels (NetCommand in,
//! UiEvent out). The chat is driven purely by the /event stream: the message
//! POST is fire-and-forget, assistant output arrives as message.part.updated
//! events whose "part" carries the full text-so-far each time.
//!
//! The protocol pieces here are the opencode v1 adapter's transport; the
//! thread itself routes every managed harness through the adapter registry
//! (`crate::harness`) and forwards the adapters' typed event streams.

use anyhow::{Context as _, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::harness::{HarnessHandle, HarnessId, LaunchSpec, Readiness};

pub const ASK_CONFIG: &str = r#"{"$schema":"https://opencode.ai/config.json","permission":{"edit":"ask","bash":"ask","webfetch":"ask"}}"#;

// ---- reconnect policy ------------------------------------------------------

/// Backoff ladder start for the event pump's reconnect loop.
pub(crate) const RECONNECT_INITIAL: Duration = Duration::from_secs(1);
/// Backoff ladder cap.
pub(crate) const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// A connection that survived this long counts as stable: the next drop
/// restarts the ladder at [`RECONNECT_INITIAL`] instead of continuing it, and
/// the store closes the combined "connection lost" note.
pub const RECONNECT_RESET_AFTER: Duration = Duration::from_secs(60);

/// Give up reconnecting after this many consecutive failed tries; a
/// connection that stayed up for [`RECONNECT_RESET_AFTER`] restarts the count.
pub(crate) const RECONNECT_MAX_ATTEMPTS: u32 = 10;

// ---- data types ----------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub id: String,
    pub title: String,
    pub updated_ms: i64,
    pub created_ms: i64,
    pub parent_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StoredMessage {
    pub id: String,
    pub role: String,
    pub created_ms: i64,
    pub info: Value,
    pub parts: Vec<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelInfo {
    pub key: String,
    /// Which backend serves the model; the catalogs overlap on some names, so
    /// a key alone is not enough to resolve it.
    pub harness: HarnessId,
    #[serde(default)]
    pub variants: Vec<String>,
    #[serde(default)]
    pub context_limit: i64,
}

#[derive(Clone, Debug)]
pub struct AgentInfo {
    pub name: String,
    pub mode: String,
    pub hidden: bool,
}

/// A server-side slash command or skill: GET /command. The template never
/// reaches the client — `/name args` runs via POST /session/{id}/command and
/// the server does the expansion (skills included).
#[derive(Clone, Debug)]
pub struct CommandInfo {
    pub name: String,
    /// `name.to_lowercase()`, computed once here: the menu filter runs it
    /// against every command on every keystroke and stream repaint
    pub name_lower: String,
    pub description: String,
    /// "command" (built-in / user markdown) or "skill"
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct QuestionOptionInfo {
    pub label: String,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct QuestionPromptInfo {
    /// The caller-provided question id, echoed back with the answer.
    pub id: String,
    pub question: String,
    pub header: String,
    pub multiple: bool,
    pub options: Vec<QuestionOptionInfo>,
}

/// A pending `question` tool ask: id (que_…), owning session and the questions in order.
#[derive(Clone, Debug)]
pub struct QuestionRequestInfo {
    pub id: String,
    pub session_id: String,
    pub questions: Vec<QuestionPromptInfo>,
}

// ---- server spawn --------------------------------------------------------

pub struct ServerProcess {
    pub base_url: String,
    /// The launch credential a managed child prints on stdout, when the
    /// descriptor's readiness waits for one (None for a server with none).
    pub token: Option<String>,
    /// behind Arc+Mutex so the health poll can watch for an early exit while
    /// the workspace map on the net thread keeps the owning handle; None for
    /// an attached (not spawned) server — there is nothing to watch
    child: Arc<Mutex<Option<tokio::process::Child>>>,
}

/// True when the spawned serve process exited on its own. A taken (stopped)
/// or never-spawned (attached) child reads false.
pub(crate) fn child_exited(probe: &Mutex<Option<tokio::process::Child>>) -> bool {
    let mut guard = probe.lock().expect("child lock");
    match guard.as_mut() {
        Some(child) => matches!(child.try_wait(), Ok(Some(_))),
        None => false,
    }
}

impl ServerProcess {
    /// The child-handle probe the health poll watches for an early exit.
    pub(crate) fn exit_probe(&self) -> Arc<Mutex<Option<tokio::process::Child>>> {
        Arc::clone(&self.child)
    }

    /// Launch the managed child (or attach to an already healthy one) and
    /// wait for it to become reachable. The descriptor's [`LaunchSpec`] owns
    /// everything harness-specific: arguments, environment, what "up" means
    /// and how long to wait.
    pub async fn start(
        spec: LaunchSpec,
        executable: &str,
        folder: &str,
        port: u16,
        on_log: impl Fn(String) + Clone + Send + Sync + 'static,
    ) -> Result<Self> {
        let base_url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()?;
        // a launch credential is per-process: an existing server cannot be
        // attached to (its credential is unknowable), always spawn our own
        if spec.attach_healthy && is_healthy(&client, &base_url).await {
            tracing::info!("attached to a healthy server at {base_url}");
            on_log(format!("attaching to a healthy server at {base_url}"));
            return Ok(ServerProcess {
                base_url,
                token: None,
                child: Arc::new(Mutex::new(None)),
            });
        }

        let mut command = tokio::process::Command::new(executable);
        command.args((spec.args)(port)).envs((spec.env)());
        command
            .current_dir(std::path::Path::new(
                if std::path::Path::new(folder).exists() {
                    folder
                } else {
                    "."
                },
            ))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command.env_remove("ELECTRON_RUN_AS_NODE");
        // GUI app: without this flag Windows opens a visible console window
        // for the console-subsystem child process
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        // No PR_SET_PDEATHSIG here. Linux delivers that signal when the
        // *parent thread* exits, and this start runs on a transient
        // spawn_blocking worker: the worker retiring killed every managed
        // child ~20 s after launch (opencode restarts and dsh connection
        // resets). The app owns the child through `kill_on_drop` and the
        // net thread's StopServer; that is the lifetime that matches the app.

        let mut child = command
            .spawn()
            .with_context(|| format!("'{executable}' failed to start"))?;
        tracing::info!("spawned {executable} (pid {:?}) in {folder}", child.id());

        // some children print a launch credential on stdout; the wait loop
        // below polls it
        let token_slot = Arc::new(Mutex::new(None::<String>));
        // stream stdout/stderr into the log
        let log = std::sync::Arc::new(on_log);
        if let Some(stdout) = child.stdout.take() {
            let log = log.clone();
            let token_slot = Arc::clone(&token_slot);
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let token = spec.capture_line.and_then(|capture| capture(&line));
                    if let Some(token) = &token
                        && token_slot.lock().expect("launch credential").is_none()
                    {
                        *token_slot.lock().expect("launch credential") = Some(token.clone());
                    }
                    // the URL line carries the /connect credential: keep the
                    // URL and port in the log, never the credential value
                    let logged = match &token {
                        Some(token) => line.replace(token, "…"),
                        None => line,
                    };
                    log(logged);
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let log = log.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    log(line);
                }
            });
        }

        let mut process = ServerProcess {
            base_url: base_url.clone(),
            token: None,
            child: Arc::new(Mutex::new(Some(child))),
        };

        let deadline = std::time::Instant::now() + spec.timeout;
        loop {
            if std::time::Instant::now() >= deadline {
                tracing::warn!("{}", spec.timeout_message);
                return Err(anyhow!("{}", spec.timeout_message));
            }
            {
                let mut guard = process.child.lock().expect("child lock");
                if let Some(child) = guard.as_mut()
                    && let Ok(Some(status)) = child.try_wait()
                {
                    tracing::warn!("server exited early with code {status}");
                    return Err(anyhow!("the server exited with code {status}."));
                }
            }
            match spec.readiness {
                Readiness::HttpHealth => {
                    if is_healthy(&client, &base_url).await {
                        return Ok(process);
                    }
                }
                Readiness::LaunchCredential => {
                    let token = token_slot.lock().expect("launch credential").clone();
                    if let Some(token) = token {
                        process.token = Some(token);
                        return Ok(process);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    pub fn stop(&mut self) {
        // take (not just kill): the health poll must not report a crash for a
        // process the user stopped, and kill_on_drop finishes the job
        if let Some(mut child) = self.child.lock().expect("child lock").take() {
            let _ = child.start_kill();
        }
    }
}

// ---- stale managed children ----------------------------------------------

/// A managed harness child recognized from its command line.
///
/// The two shapes are the launch argv the harness descriptors build:
/// `dsh web --no-open --host 127.0.0.1 --port <n>` and
/// `opencode serve --hostname 127.0.0.1 --port <n>`.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagedChild {
    DshWeb { port: u16 },
    OpencodeServe { port: u16 },
}

#[cfg(unix)]
impl ManagedChild {
    /// The harness id, for a log line.
    fn harness(self) -> &'static str {
        match self {
            Self::DshWeb { .. } => "dsh",
            Self::OpencodeServe { .. } => "opencode",
        }
    }

    /// The loopback port the child serves.
    fn port(self) -> u16 {
        match self {
            Self::DshWeb { port } | Self::OpencodeServe { port } => port,
        }
    }
}

/// The launch arguments of the two managed children, in the order the
/// descriptors write them. The token after `--port` is parsed separately.
#[cfg(unix)]
const DSH_WEB_ARGS: &[&str] = &["web", "--no-open", "--host", "127.0.0.1", "--port"];
#[cfg(unix)]
const OPENCODE_SERVE_ARGS: &[&str] = &["serve", "--hostname", "127.0.0.1", "--port"];

/// JavaScript runtimes an npm/bun bin shim uses to exec the harness script.
#[cfg(unix)]
const JS_RUNTIMES: &[&str] = &["node", "nodejs", "bun", "deno"];

/// The managed child one `/proc/<pid>/cmdline` body names, when it is exactly
/// a shape this app spawns.
///
/// The executable part is not fixed to the binary's own name: the app spawns
/// `dsh`, and an npm-installed `dsh` is a shell shim that execs
/// `node …/@deepseek-ai/dsh/lib/bin.js`, so the kernel records the runtime and
/// its script instead. [`managed_program`] accepts both forms. Anything else
/// — another executable, extra or reordered flags, a different host, the
/// user's own `dsh web --patch …` — is `None`.
#[cfg(unix)]
fn managed_child_from_cmdline(cmdline: &str) -> Option<ManagedChild> {
    let argv = argv_from_cmdline(cmdline);
    if let Some(port) = managed_port(&argv, DSH_WEB_ARGS, "dsh") {
        return Some(ManagedChild::DshWeb { port });
    }
    managed_port(&argv, OPENCODE_SERVE_ARGS, "opencode")
        .map(|port| ManagedChild::OpencodeServe { port })
}

/// Split a `/proc/<pid>/cmdline` body into its arguments. Every argument is
/// NUL-terminated, so the one empty piece at the end is the terminator, not an
/// argument; an empty argument before it stays in place and fails the exact
/// shape match.
#[cfg(unix)]
fn argv_from_cmdline(cmdline: &str) -> Vec<&str> {
    let mut argv: Vec<&str> = cmdline.split('\0').collect();
    if argv.last().is_some_and(|argument| argument.is_empty()) {
        argv.pop();
    }
    argv
}

/// The port of a managed child, when `argv` ends with one of the launch
/// argument lists and the tokens before it name `harness`.
#[cfg(unix)]
fn managed_port(argv: &[&str], launch_args: &[&str], harness: &str) -> Option<u16> {
    let (port, head) = argv.split_last()?;
    let port = port_number(port)?;
    let split = head.len().checked_sub(launch_args.len())?;
    let (program, tail) = head.split_at(split);
    (tail == launch_args && managed_program(program, harness)).then_some(port)
}

/// True when `program` is one of the ways `harness`'s executable appears in a
/// command line: the binary itself (`dsh`, `opencode`, with or without
/// `.exe`), or the JavaScript script an npm/bun bin shim execs in its place.
/// The script path must carry the harness name, so an unrelated script run
/// with the same arguments is not mistaken for a managed child.
#[cfg(unix)]
fn managed_program(program: &[&str], harness: &str) -> bool {
    match program {
        [binary] => program_stem(binary).eq_ignore_ascii_case(harness),
        [runtime, script] => {
            let runtime = program_stem(runtime);
            JS_RUNTIMES
                .iter()
                .any(|known| runtime.eq_ignore_ascii_case(known))
                && script.to_ascii_lowercase().contains(harness)
        }
        _ => false,
    }
}

/// The last component of an argv token, without a `.exe` suffix:
/// `…/bin/opencode.exe` is `opencode`.
#[cfg(unix)]
fn program_stem(program: &str) -> &str {
    let name = program.rsplit('/').next().unwrap_or(program);
    name.strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))
        .unwrap_or(name)
}

/// A port argument exactly as the launch args write it: canonical decimal and
/// nonzero (the app always picks a real free port).
#[cfg(unix)]
fn port_number(token: &str) -> Option<u16> {
    let port: u16 = token.parse().ok()?;
    (port != 0 && port.to_string() == token).then_some(port)
}

/// The managed child one `/proc` snapshot describes when it is stale: an exact
/// managed shape, parented to init (the run that spawned it is gone) and owned
/// by this user. `current_uid` is passed in so the guard is pure and testable
/// without touching a live process. A process reparented to a subreaper
/// instead of init is deliberately skipped: something owns it, and killing
/// under a live parent is the one thing this sweep must never do.
#[cfg(unix)]
fn stale_managed_child(
    cmdline: &str,
    parent: u32,
    owner: u32,
    current_uid: u32,
) -> Option<ManagedChild> {
    if parent != 1 || owner != current_uid {
        return None;
    }
    managed_child_from_cmdline(cmdline)
}

/// The parent pid from `/proc/<pid>/stat`: field 4, read after the final `)`
/// of the comm field (which may itself contain spaces and parentheses).
#[cfg(unix)]
fn parent_pid_from_stat(stat: &str) -> Option<u32> {
    let after_comm = &stat[stat.rfind(')')? + 1..];
    // the fields after the comm are `state`, `ppid`, …
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

/// Stop the managed harness children a previous app run left behind as
/// orphans.
///
/// This runs once in `run()`, before the window opens and before this run
/// starts a server of its own. A managed server is a child of this process:
/// a clean quit stops it, but SIGTERM/SIGKILL leaves it parented to init
/// (PPID 1) while it still holds its port and memory. The sweep only considers
/// processes that are exactly one of this app's launch argv shapes, are
/// orphans and belong to this user; the user's own `dsh web --patch …` and any
/// server of a live instance (its parent is still alive) are never candidates.
/// A failure is logged and startup continues.
#[cfg(unix)]
pub(crate) fn reap_stale_children() {
    // SAFETY: getuid reads the calling process's own real user id; it has no
    // preconditions and cannot fail.
    let current_uid = unsafe { libc::getuid() };
    let entries = match std::fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(err) => {
            // no /proc on this Unix: nothing to sweep
            tracing::debug!("stale child sweep skipped: {err}");
            return;
        }
    };
    let mut stopped = 0;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let Some(child) = stale_child_at(pid, current_uid) else {
            continue;
        };
        if stop_stale_child(pid, child, current_uid) {
            stopped += 1;
        }
    }
    if stopped > 0 {
        tracing::info!("stopped {stopped} stale managed harness child(ren) from a previous run");
    }
}

/// Windows has no `/proc/<pid>/cmdline`, so the sweep is a no-op there.
#[cfg(not(unix))]
pub(crate) fn reap_stale_children() {}

/// Read one process's parent, owner and command line and reduce them to the
/// stale managed child they describe.
#[cfg(unix)]
fn stale_child_at(pid: u32, current_uid: u32) -> Option<ManagedChild> {
    use std::os::unix::fs::MetadataExt as _;

    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let parent = parent_pid_from_stat(&stat)?;
    let owner = std::fs::metadata(format!("/proc/{pid}")).ok()?.uid();
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    stale_managed_child(
        &String::from_utf8_lossy(&cmdline),
        parent,
        owner,
        current_uid,
    )
}

/// Signal one stale child through a pidfd, after re-reading it through the
/// same pid. Returns true when the signal was sent.
#[cfg(target_os = "linux")]
fn stop_stale_child(pid: u32, child: ManagedChild, current_uid: u32) -> bool {
    let handle = match pidfd_open(pid) {
        Ok(handle) => handle,
        Err(err) => {
            // it exited between the two reads, or the kernel has no
            // pidfd_open. Never fall back to a bare kill: a recycled pid
            // could then belong to an unrelated process.
            tracing::debug!("kept stale {} child pid {pid}: {err}", child.harness());
            return false;
        }
    };
    // The scan and this check are two reads, so the pid may have exited and
    // been reused in between. The handle is the proof: it names one process,
    // and a signal through it can never reach a later process that reused the
    // pid (that would be an unrelated kill).
    if stale_child_at(pid, current_uid) != Some(child) {
        return false;
    }
    match pidfd_terminate(&handle) {
        Ok(()) => {
            tracing::info!(
                "stopped stale {} child (pid {pid}, port {})",
                child.harness(),
                child.port()
            );
            true
        }
        Err(err) => {
            tracing::warn!(
                "could not stop stale {} child (pid {pid}): {err}",
                child.harness()
            );
            false
        }
    }
}

/// Non-Linux Unix has no pidfd, so re-read the process immediately before the
/// signal; the window between the check and the kill is as small as it can be.
#[cfg(all(unix, not(target_os = "linux")))]
fn stop_stale_child(pid: u32, child: ManagedChild, current_uid: u32) -> bool {
    if stale_child_at(pid, current_uid) != Some(child) {
        return false;
    }
    // SAFETY: kill sends a signal to the pid this function just checked is an
    // orphan owned by this user and one of its managed shapes; a failure is an
    // errno, not undefined behavior.
    let sent = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if sent != 0 {
        tracing::warn!(
            "could not stop stale {} child (pid {pid}): {}",
            child.harness(),
            std::io::Error::last_os_error()
        );
        return false;
    }
    tracing::info!(
        "stopped stale {} child (pid {pid}, port {})",
        child.harness(),
        child.port()
    );
    true
}

/// A pidfd for `pid` (Linux 5.3+), so signalling cannot be redirected by pid
/// reuse.
#[cfg(target_os = "linux")]
fn pidfd_open(pid: u32) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd as _, OwnedFd};

    // SAFETY: pidfd_open takes a pid and a flags word and returns a new fd or
    // -1 with errno set; both arguments are plain integers.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0u32) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a successful pidfd_open returned a fresh fd owned by this
    // process; OwnedFd closes it exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
}

/// SIGTERM the process a pidfd names (Linux 5.3+).
#[cfg(target_os = "linux")]
fn pidfd_terminate(handle: &std::os::fd::OwnedFd) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    // SAFETY: the pidfd is live for this call. A null siginfo asks the kernel
    // for the signal's standard payload, and flags is 0.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            handle.as_raw_fd(),
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if result < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

async fn is_healthy(client: &reqwest::Client, base_url: &str) -> bool {
    matches!(
        client
            .get(format!("{base_url}/global/health"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await,
        Ok(resp) if resp.status().is_success()
    )
}

/// The registry's per-workspace harness, which this client module serves.
fn workspace_harness_id() -> HarnessId {
    crate::harness::workspace_harness().unwrap_or_else(|| HarnessId::new("opencode"))
}

pub fn find_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

// ---- REST + SSE client ---------------------------------------------------

// Clone shares the connection pool (reqwest::Client is an Arc inside) — the
// command loop clones the workspace's client per request instead of building
// a fresh pool + TLS config per request.
#[derive(Clone)]
pub struct ApiClient {
    /// The `/event` SSE connection: no total timeout (it lives as long as the
    /// session), only a bounded connect phase.
    stream_http: reqwest::Client,
    /// Short REST calls: bounded end to end so a wedged server cannot leave a
    /// tile loading forever (audit B11). The two long POSTs override this with
    /// their own 600 s timeout.
    rest: reqwest::Client,
    pub base_url: String,
}

impl ApiClient {
    pub fn new(base_url: &str) -> Self {
        let stream_http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let rest = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self {
            stream_http,
            rest,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// One low-cost `/global/health` probe, used by the harness adapter's
    /// health poll and its reconnect gate.
    pub async fn healthy(&self) -> bool {
        is_healthy(&self.rest, &self.base_url).await
    }

    pub async fn create_session(&self, title: &str) -> Result<SessionInfo> {
        let resp = self
            .rest
            .post(format!("{}/session", self.base_url))
            .json(&json!({ "title": title }))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        Ok(read_session(&value))
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let resp = self
            .rest
            .get(format!("{}/session", self.base_url))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        Ok(value
            .as_array()
            .map(|arr| arr.iter().map(read_session).collect())
            .unwrap_or_default())
    }

    pub async fn get_messages(&self, session_id: &str) -> Result<Vec<StoredMessage>> {
        let resp = self
            .rest
            .get(format!("{}/session/{}/message", self.base_url, session_id))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        let mut messages = Vec::new();
        if let Some(arr) = value.as_array() {
            for m in arr {
                let info = m.get("info").cloned().unwrap_or(Value::Null);
                let (Some(id), Some(role)) = (
                    info.get("id").and_then(Value::as_str),
                    info.get("role").and_then(Value::as_str),
                ) else {
                    continue;
                };
                let created_ms = info
                    .pointer("/time/created")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let parts = m
                    .get("parts")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                messages.push(StoredMessage {
                    id: id.to_string(),
                    role: role.to_string(),
                    created_ms,
                    info,
                    parts,
                });
            }
        }
        Ok(messages)
    }

    pub async fn rename_session(&self, session_id: &str, title: &str) -> Result<SessionInfo> {
        let resp = self
            .rest
            .patch(format!("{}/session/{}", self.base_url, session_id))
            .json(&json!({ "title": title }))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        Ok(read_session(&value))
    }

    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        let resp = self
            .rest
            .delete(format!("{}/session/{}", self.base_url, session_id))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn abort(&self, session_id: &str) -> Result<()> {
        let resp = self
            .rest
            .post(format!("{}/session/{}/abort", self.base_url, session_id))
            .json(&json!({}))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    /// Fire-and-forget chat send; the answer arrives on the event stream.
    pub async fn send_message(
        &self,
        session_id: &str,
        text: &str,
        model: Option<&str>,
        variant: Option<&str>,
        agent: Option<&str>,
        attachments: &[(String, String, std::sync::Arc<Vec<u8>>)], // (filename, mime, bytes)
    ) -> Result<()> {
        let mut parts = Vec::new();
        if !text.is_empty() {
            parts.push(json!({ "type": "text", "text": text }));
        }
        for (file_name, mime, data) in attachments {
            parts.push(json!({
                "type": "file",
                "mime": mime,
                "filename": file_name,
                "url": format!("data:{mime};base64,{}", base64_encode(data)),
            }));
        }
        let mut body = json!({ "parts": parts });
        if let Some(model) = model {
            let (provider_id, model_id) = match model.split_once('/') {
                Some((p, m)) => (p, m),
                None => (model, ""),
            };
            body["model"] = json!({ "providerID": provider_id, "modelID": model_id });
        }
        if let Some(variant) = variant {
            body["variant"] = json!(variant);
        }
        if let Some(agent) = agent {
            body["agent"] = json!(agent);
        }
        let resp = self
            .rest
            .post(format!("{}/session/{}/message", self.base_url, session_id))
            .json(&body)
            .timeout(Duration::from_secs(600))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    /// Server-side slash command / skill execution (POST /session/{id}/command):
    /// the server expands the template, honors the command's own agent/model
    /// config and starts the turn — the client never sees the expanded text.
    /// `model` is a "provider/model" string on this route, unlike the object
    /// shape the plain message route takes.
    #[expect(
        clippy::too_many_arguments,
        reason = "the parameter list mirrors the server's POST body one-to-one"
    )]
    pub async fn send_command(
        &self,
        session_id: &str,
        name: &str,
        args: &str,
        model: Option<&str>,
        variant: Option<&str>,
        agent: Option<&str>,
        attachments: &[(String, String, std::sync::Arc<Vec<u8>>)], // (filename, mime, bytes)
    ) -> Result<()> {
        let mut body = json!({ "command": name, "arguments": args });
        if let Some(model) = model {
            body["model"] = json!(model);
        }
        if let Some(variant) = variant {
            body["variant"] = json!(variant);
        }
        if let Some(agent) = agent {
            body["agent"] = json!(agent);
        }
        if !attachments.is_empty() {
            let parts: Vec<_> = attachments
                .iter()
                .map(|(file_name, mime, data)| {
                    json!({
                        "type": "file",
                        "mime": mime,
                        "filename": file_name,
                        "url": format!("data:{mime};base64,{}", base64_encode(data)),
                    })
                })
                .collect();
            body["parts"] = json!(parts);
        }
        let resp = self
            .rest
            .post(format!("{}/session/{}/command", self.base_url, session_id))
            .json(&body)
            .timeout(Duration::from_secs(600))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    /// On-demand context compaction (POST /session/{id}/summarize): the
    /// server replaces older history with its summary. The route needs an
    /// explicit provider/model, unlike the plain message route's object shape.
    pub async fn compact_session(&self, session_id: &str, model: &str) -> Result<()> {
        let (provider_id, model_id) = match model.split_once('/') {
            Some((provider, model)) => (provider, model),
            None => (model, ""),
        };
        let resp = self
            .rest
            .post(format!(
                "{}/session/{}/summarize",
                self.base_url, session_id
            ))
            .json(&json!({
                "providerID": provider_id,
                "modelID": model_id,
                "auto": false,
            }))
            .timeout(Duration::from_secs(600))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn respond_permission(
        &self,
        session_id: &str,
        permission_id: &str,
        response: &str,
    ) -> Result<()> {
        let resp = self
            .rest
            .post(format!(
                "{}/session/{}/permissions/{}",
                self.base_url, session_id, permission_id
            ))
            .json(&json!({ "response": response }))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    /// The server's pending question asks. The adapter's tracked set can miss
    /// asks that arrived while its event stream was down, so a pull is
    /// authoritative when it answers.
    pub async fn get_questions(&self) -> Result<Vec<QuestionRequestInfo>> {
        let resp = self
            .rest
            .get(format!("{}/question", self.base_url))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        Ok(value
            .as_array()
            .map(|arr| arr.iter().filter_map(read_question_request).collect())
            .unwrap_or_default())
    }

    pub async fn reply_question(&self, request_id: &str, answers: &[Vec<String>]) -> Result<()> {
        let resp = self
            .rest
            .post(format!("{}/question/{}/reply", self.base_url, request_id))
            .json(&json!({ "answers": answers }))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn reject_question(&self, request_id: &str) -> Result<()> {
        let resp = self
            .rest
            .post(format!("{}/question/{}/reject", self.base_url, request_id))
            .json(&json!({}))
            .send()
            .await?;
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn get_agents(&self) -> Result<Vec<AgentInfo>> {
        let resp = self
            .rest
            .get(format!("{}/agent", self.base_url))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        let mut agents = Vec::new();
        if let Some(arr) = value.as_array() {
            for a in arr {
                let Some(name) = a.get("name").and_then(Value::as_str) else {
                    continue;
                };
                agents.push(AgentInfo {
                    name: name.to_string(),
                    mode: a.get("mode").and_then(Value::as_str).unwrap_or("").into(),
                    hidden: a.get("hidden").and_then(Value::as_bool).unwrap_or(false),
                });
            }
        }
        Ok(agents)
    }

    /// The server's slash commands and skills, in server order.
    pub async fn get_commands(&self) -> Result<Vec<CommandInfo>> {
        let resp = self
            .rest
            .get(format!("{}/command", self.base_url))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        let mut commands = Vec::new();
        if let Some(arr) = value.as_array() {
            for c in arr {
                let Some(name) = c.get("name").and_then(Value::as_str) else {
                    continue;
                };
                commands.push(CommandInfo {
                    name_lower: name.to_lowercase(),
                    name: name.to_string(),
                    description: c
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    source: c
                        .get("source")
                        .and_then(Value::as_str)
                        .unwrap_or("command")
                        .into(),
                });
            }
        }
        Ok(commands)
    }

    /// Filename search over the server's working directory (the `@` mention
    /// source); paths come back as the server reports them, usually relative.
    pub async fn find_files(&self, query: &str) -> Result<Vec<String>> {
        let resp = self
            .rest
            .get(format!("{}/find/file", self.base_url))
            .query(&[("query", query)])
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        Ok(value
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// The server's merged config (global + project + env content). The `mcp`
    /// section lists the workspace's configured MCP servers.
    pub async fn get_config(&self) -> Result<Value> {
        let resp = self
            .rest
            .get(format!("{}/config", self.base_url))
            .send()
            .await?;
        error_for_status(resp)
            .await?
            .json()
            .await
            .map_err(anyhow::Error::from)
    }

    /// Runtime MCP toggles: they change which tools the running server exposes
    /// immediately, touch no config file, and a 404 just means the entry
    /// vanished from config between listing and acting — still a success.
    pub async fn mcp_connect(&self, name: &str) -> Result<()> {
        let resp = self
            .rest
            .post(format!("{}/mcp/{}/connect", self.base_url, name))
            .json(&json!({}))
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn mcp_disconnect(&self, name: &str) -> Result<()> {
        let resp = self
            .rest
            .post(format!("{}/mcp/{}/disconnect", self.base_url, name))
            .json(&json!({}))
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        error_for_status(resp).await.map(|_| ())
    }

    pub async fn get_models(&self) -> Result<Vec<ModelInfo>> {
        let resp = self
            .rest
            .get(format!("{}/config/providers", self.base_url))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        let mut models = Vec::new();
        if let Some(providers) = value.get("providers").and_then(Value::as_array) {
            for provider in providers {
                let Some(pid) = provider.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(models_map) = provider.get("models").and_then(Value::as_object) else {
                    continue;
                };
                for (name, m) in models_map {
                    let mut variants: Vec<String> = m
                        .get("variants")
                        .and_then(Value::as_object)
                        .map(|v| v.keys().cloned().collect())
                        .unwrap_or_default();
                    // picker order = effort tiers low→max, not alphabetical
                    variants.sort_by_key(|v| (crate::store::variant_rank(v), v.to_lowercase()));
                    let context_limit = m
                        .pointer("/limit/context")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    models.push(ModelInfo {
                        key: format!("{pid}/{name}"),
                        harness: workspace_harness_id(),
                        variants,
                        context_limit,
                    });
                }
            }
        }
        Ok(models)
    }

    /// Provider ids and display names from the same catalog `/config/providers`
    /// exposes; the model list only carries the `provider/model` key.
    pub async fn get_providers(&self) -> Result<Vec<(String, String)>> {
        let resp = self
            .rest
            .get(format!("{}/config/providers", self.base_url))
            .send()
            .await?;
        let value: Value = error_for_status(resp).await?.json().await?;
        let mut providers = Vec::new();
        if let Some(entries) = value.get("providers").and_then(Value::as_array) {
            for provider in entries {
                let Some(id) = provider.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let name = provider.get("name").and_then(Value::as_str).unwrap_or(id);
                providers.push((id.to_string(), name.to_string()));
            }
        }
        Ok(providers)
    }

    /// Yields events from the /event SSE stream until it ends. `on_attached`
    /// fires once the server accepted the stream (response status is in) —
    /// the reconnect pump reports Connected from there, not from a mere
    /// attempt.
    pub async fn stream_events(
        &self,
        on_event: impl Fn(Value),
        on_attached: impl Fn(),
        stop: async_channel::Receiver<()>,
    ) -> Result<()> {
        use futures_util::StreamExt as _;
        let resp = self
            .stream_http
            .get(format!("{}/event", self.base_url))
            .send()
            .await?;
        let resp = error_for_status(resp).await?;
        on_attached();
        let mut stream = resp.bytes_stream();
        let mut buffer = Vec::new();
        loop {
            tokio::select! {
                _ = stop.recv() => return Ok(()),
                chunk = stream.next() => {
                    match chunk {
                        Some(Ok(bytes)) => buffer.extend_from_slice(&bytes),
                        Some(Err(err)) => return Err(err.into()),
                        None => return Ok(()),
                    }
                }
            }
            // extract complete lines
            while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buffer.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line[..line.len() - 1]);
                let line = line.trim_end_matches('\r');
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload.is_empty() {
                    continue;
                }
                if let Ok(evt) = serde_json::from_str::<Value>(payload) {
                    on_event(evt);
                }
            }
        }
    }
}

async fn error_for_status(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        let detail = truncate(&detail, 1000);
        tracing::warn!("request failed: {status} — {detail}");
        return Err(anyhow!(
            "{status}{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(" — {detail}")
            }
        ));
    }
    Ok(resp)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        // byte budgets must land on a char boundary or multibyte text panics
        let mut cut = max;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

fn read_session(s: &Value) -> SessionInfo {
    SessionInfo {
        id: s.get("id").and_then(Value::as_str).unwrap_or("").into(),
        title: s.get("title").and_then(Value::as_str).unwrap_or("").into(),
        updated_ms: s
            .pointer("/time/updated")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        created_ms: s
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .unwrap_or(0),
        parent_id: s.get("parentID").and_then(Value::as_str).map(String::from),
    }
}

/// Shared by the /question listing and the question.asked event — same Request shape.
pub fn read_question_request(r: &Value) -> Option<QuestionRequestInfo> {
    let id = r.get("id").and_then(Value::as_str)?;
    let session_id = r.get("sessionID").and_then(Value::as_str)?;
    let questions_value = r.get("questions")?.as_array()?;
    let mut questions = Vec::new();
    for q in questions_value {
        let options = q
            .get("options")
            .and_then(Value::as_array)
            .map(|os| {
                os.iter()
                    .filter_map(|o| {
                        let label = o.get("label").and_then(Value::as_str)?;
                        Some(QuestionOptionInfo {
                            label: label.into(),
                            description: o
                                .get("description")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .into(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        questions.push(QuestionPromptInfo {
            id: q.get("id").and_then(Value::as_str).unwrap_or("").into(),
            question: q
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("")
                .into(),
            header: q.get("header").and_then(Value::as_str).unwrap_or("").into(),
            multiple: q
                .get("multiple")
                .and_then(Value::as_bool)
                .or_else(|| q.get("multiSelect").and_then(Value::as_bool))
                .unwrap_or(false),
            options,
        });
    }
    Some(QuestionRequestInfo {
        id: id.into(),
        session_id: session_id.into(),
        questions,
    })
}

pub(crate) fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// ---- net thread ----------------------------------------------------------

pub enum NetCommand {
    /// Spawn (or attach to) the server for a harness instance and start its
    /// event pump. An app-wide harness is keyed by its id.
    StartServer {
        ws_key: String,
        harness: HarnessId,
        executable: String,
        folder: String,
    },
    /// Stop the workspace's server and event pump.
    StopServer { ws_key: String },
    /// Run a request against the workspace's API; results come back as UiEvents.
    Request { ws_key: String, request: ApiRequest },
    /// Fire-and-forget message send. `command` is `(name, args)` when the text
    /// parsed as a server command/skill — it goes to the /command route
    /// instead of a plain prompt, and the server expands the template.
    SendMessage {
        ws_key: String,
        session_id: String,
        text: String,
        model: Option<String>,
        variant: Option<String>,
        agent: Option<String>,
        attachments: Vec<(String, String, std::sync::Arc<Vec<u8>>)>,
        command: Option<(String, String)>,
    },
    /// Fan a pre-serialized event frame out to every connected door client.
    IpcBroadcast { line: String },
    /// One `harness` door call: the net thread resolves the instance for the
    /// harness's scope and runs the call on a blocking lane. The answer rides
    /// back as [`UiEvent::HarnessCallResult`], so the store never blocks.
    HarnessCall {
        call_id: u64,
        harness: HarnessId,
        executable: String,
        directory: Option<std::path::PathBuf>,
        method: String,
        params: Value,
    },
    /// A chat was deleted: drop any folded follow state for it.
    ForgetSession { session_id: String },
    /// Ask whether a folder is a git repository: the worktree toggle's
    /// visibility, answered once per folder.
    ProbeRepository { folder: String },
    /// Create a draft's worktree. Process work, so it runs on a blocking lane.
    CreateWorktree {
        tile_key: String,
        repo: String,
        branch: String,
    },
    /// Re-attach an existing branch at the same path: the repair for a chat
    /// whose checkout folder disappeared.
    AttachWorktree {
        tile_key: String,
        repo: String,
        folder: String,
        branch: String,
    },
}

pub enum ApiRequest {
    ListSessions,
    /// One workspace folder's sessions on an app-wide instance, filtered by
    /// the session's own cwd. The reply carries the folder as ws_key so the
    /// store applies it like that workspace's own list.
    ListSessionsIn {
        folder: String,
    },
    /// Create a session owned by `folder` on an app-wide instance (its
    /// sessions carry their own cwd). The reply carries the folder as ws_key.
    CreateSessionIn {
        folder: String,
        title: String,
    },
    /// The fork flow on an app-wide instance — an empty session whose composer
    /// is seeded with `context` (there is no parts-append endpoint).
    CreateForkSessionIn {
        folder: String,
        title: String,
        context: String,
    },
    ListModels,
    ListAgents,
    GetMessages {
        session_id: String,
    },
    CreateSession {
        title: String,
    },
    /// Fork: create an empty session for a transcript seed; the context rides
    /// back on the event because it goes into the new tile's composer, never
    /// to the model (the serve API cannot append parts without a turn).
    CreateForkSession {
        title: String,
        context: String,
    },
    RenameSession {
        session_id: String,
        title: String,
    },
    DeleteSession {
        session_id: String,
    },
    Abort {
        session_id: String,
    },
    RespondPermission {
        session_id: String,
        permission_id: String,
        response: String,
    },
    GetQuestions {
        for_session: String,
    },
    /// Answer one pending question/form; the card knows its session.
    ReplyQuestion {
        session_id: String,
        request_id: String,
        field_ids: Vec<String>,
        answers: Vec<Vec<String>>,
    },
    /// Dismiss one pending question/form.
    RejectQuestion {
        session_id: String,
        request_id: String,
    },
    /// The workspace's slash commands and skills (GET /command).
    ListCommands,
    /// The agent-scoped command + skill catalog. Commands are the builtin
    /// actions (`compact`, `plan`, …); skills are the workspace instructions.
    /// Some harnesses need the session id for the catalog's agent scope.
    ListAgentCommands {
        session_id: Option<String>,
    },
    /// Open a session for a draft's slash menu. Unlike
    /// [`ApiRequest::CreateSessionIn`] this carries no prompt: the first real
    /// send reuses the session.
    CreateCommandSession {
        tile_key: String,
        folder: String,
    },
    /// Run one listed command through the harness's command route. Commands
    /// are log-only; they never become a model message.
    ExecuteCommand {
        session_id: String,
        line: String,
    },
    /// Compact one session through the harness's own compaction route. `model`
    /// is the tile's "provider/model"; opencode's endpoint needs both halves.
    CompactSession {
        session_id: String,
        model: String,
    },
    /// Filename search for the `@` mention menu (GET /find/file).
    FindFiles {
        query: String,
    },
    /// Names of the workspace's configured MCP servers (GET /config "mcp" section).
    ListMcpNames,
    /// Runtime MCP toggle for one configured server; instant, no config write.
    McpConnect {
        name: String,
    },
    McpDisconnect {
        name: String,
    },
}

/// Live state of a workspace's event pump. Connected is reported on every
/// successful SSE attach, Reconnecting once per drop of a connected stream
/// (carrying the failing attempt number of [`RECONNECT_MAX_ATTEMPTS`]),
/// Failed when those tries ran out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamStatus {
    Connected,
    Reconnecting { attempt: u32 },
    Failed,
}

/// Everything the UI learns from the network thread arrives as one of these.
pub enum UiEvent {
    /// A folder was probed for its repository: `repository` is the main
    /// checkout when there is one, and `worktrees` its linked worktrees as
    /// (folder, branch-or-label), main excluded.
    RepositoryProbed {
        folder: String,
        repository: Option<String>,
        worktrees: Vec<(String, String)>,
    },
    /// A draft's worktree now exists; `folder` is its working directory.
    WorktreeCreated {
        tile_key: String,
        folder: String,
        branch: String,
    },
    /// A draft's worktree could not be created; the composer keeps the field.
    WorktreeFailed {
        tile_key: String,
        error: String,
    },
    /// A chat's worktree was restored, or the reason it could not be.
    WorktreeAttached {
        tile_key: String,
        folder: String,
        branch: String,
        error: Option<String>,
    },
    ServerStarted {
        ws_key: String,
        result: Result<()>,
    },
    ServerStopped,
    /// the spawned serve process exited on its own (not a user stop)
    ServerCrashed {
        ws_key: String,
    },
    Event {
        ws_key: String,
        event: Value,
    },
    /// One typed harness event from a managed adapter. The store ingests it
    /// through the same typed seam the attached adapter uses; `directory` is
    /// the per-workspace instance's folder, absent for an app-wide harness.
    HarnessEvent {
        harness: HarnessId,
        directory: Option<String>,
        event: crate::harness::HarnessEvent,
    },
    StreamStatus {
        ws_key: String,
        status: StreamStatus,
    },
    /// Latest /global/health answer; None until the first poll lands.
    HealthChanged {
        ws_key: String,
        healthy: Option<bool>,
    },
    SessionsLoaded {
        ws_key: String,
        /// True when the reply came from the shared app-wide server (such a
        /// list must never prune a workspace server's chats, and vice versa);
        /// carried on the event so replies can never be misattributed
        shared: bool,
        result: Result<Vec<SessionInfo>>,
    },
    MessagesLoaded {
        ws_key: String,
        session_id: String,
        result: Result<Vec<StoredMessage>>,
    },
    SessionCreated {
        ws_key: String,
        result: Result<SessionInfo>,
    },
    ForkSessionCreated {
        ws_key: String,
        context: String,
        result: Result<SessionInfo>,
    },
    SessionRenamed {
        session_id: String,
        result: Result<SessionInfo>,
    },
    SessionDeleted {
        session_id: String,
        result: Result<()>,
    },
    Aborted {
        session_id: String,
        result: Result<()>,
    },
    PermissionReplied {
        permission_id: String,
        response: String,
        is_auto: bool,
        result: Result<()>,
    },
    QuestionsPulled {
        for_session: String,
        result: Result<Vec<QuestionRequestInfo>>,
    },
    QuestionReplied {
        request_id: String,
        summary: String,
        result: Result<()>,
    },
    ModelsLoaded {
        harness: HarnessId,
        result: Result<Vec<ModelInfo>>,
    },
    AgentsLoaded {
        result: Result<Vec<AgentInfo>>,
    },
    CommandsLoaded {
        ws_key: String,
        result: Result<Vec<CommandInfo>>,
    },
    /// The lazy session a draft asked for when its slash menu opened.
    CommandSessionCreated {
        tile_key: String,
        ws_key: String,
        result: Result<SessionInfo>,
    },
    /// One command execution result; `Ok` carries the success text (possibly
    /// empty), `Err` the command's own error text.
    CommandExecuted {
        session_id: String,
        result: Result<String>,
    },
    /// opencode: the summarize call settled; the transcript updates from the
    /// event stream, the note only reports the outcome.
    SessionCompacted {
        session_id: String,
        result: Result<()>,
    },
    /// Answer to a FindFiles request; the query rides along so stale answers
    /// can be told apart from the menu's current token.
    FilesFound {
        ws_key: String,
        query: String,
        result: Result<Vec<String>>,
    },
    /// The workspace's configured MCP server names, in stable name order.
    McpNamesLoaded {
        ws_key: String,
        result: Result<Vec<String>>,
    },
    MessageSendFailed {
        session_id: String,
        error: String,
    },
    Log {
        ws_key: String,
        line: String,
    },
    /// A door command relayed from the ipc listener; answered by the store.
    Ipc {
        command: crate::ipc::IpcCommand,
    },
    /// The answer to one [`NetCommand::HarnessCall`]; `call_id` names the door
    /// reply the store parked for it.
    HarnessCallResult {
        call_id: u64,
        result: Result<Value, String>,
    },
    /// One event from warp-flow's door client (`crate::flow`): the board, a
    /// pushed host run, the door's standing, or the answer to this person's
    /// answer / run action.
    Flow {
        event: crate::flow::FlowEvent,
    },
}

// ---- the net thread -------------------------------------------------------
//
// Every managed harness runs through its adapter. The net thread asks the
// registry factory for an instance (through the host), starts it and forwards
// the adapter's typed event stream to the store; requests are dispatched to
// the same instance. The thread itself owns no protocol.

/// One running managed harness: the adapter behind it and the task forwarding
/// its typed events to the store.
struct RunningHarness {
    harness: HarnessId,
    scope: Option<std::path::PathBuf>,
    handle: HarnessHandle,
    pump: tokio::task::JoinHandle<()>,
}

/// Cloneable handle the UI uses to talk to the net thread.
#[derive(Clone)]
pub struct NetSender(pub async_channel::Sender<NetCommand>);

impl NetSender {
    pub fn send(&self, command: NetCommand) {
        if let Err(err) = self.0.send_blocking(command) {
            tracing::error!("net thread gone: {err}");
        }
    }
}

pub fn spawn_net_thread() -> (
    std::thread::JoinHandle<()>,
    NetSender,
    async_channel::Receiver<UiEvent>,
    crate::flow::FlowSender,
) {
    spawn_net_thread_with(crate::harness::HarnessRegistry::builtin())
}

/// The same thread over an explicit registry: the seam a test drives with the
/// fake adapter instead of a real harness.
#[doc(hidden)]
pub fn spawn_net_thread_with(
    registry: crate::harness::HarnessRegistry,
) -> (
    std::thread::JoinHandle<()>,
    NetSender,
    async_channel::Receiver<UiEvent>,
    crate::flow::FlowSender,
) {
    let (command_tx, command_rx) = async_channel::unbounded::<NetCommand>();
    let (event_tx, event_rx) = async_channel::unbounded::<UiEvent>();
    // The Flow client's command lane. The driver itself is spawned inside the
    // runtime below, so warp-flow's door rides the net thread's one runtime and
    // reports through the one event pump; the UI gets the sending end here.
    let (flow_tx, flow_rx) = async_channel::unbounded::<crate::flow::FlowCommand>();
    let handle = std::thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(net_main(registry, command_rx, event_tx, flow_rx));
        })
        .expect("net thread");
    (
        handle,
        NetSender(command_tx),
        event_rx,
        crate::flow::FlowSender(flow_tx),
    )
}

async fn net_main(
    registry: crate::harness::HarnessRegistry,
    command_rx: async_channel::Receiver<NetCommand>,
    event_tx: async_channel::Sender<UiEvent>,
    flow_rx: async_channel::Receiver<crate::flow::FlowCommand>,
) {
    // warp-flow's door client: one task on this runtime, one ask every few
    // seconds, its events forwarded into the app's existing pump as UiEvents.
    let (flow_events_tx, flow_events_rx) = async_channel::unbounded::<crate::flow::FlowEvent>();
    {
        let event_tx = event_tx.clone();
        tokio::spawn(async move {
            while let Ok(event) = flow_events_rx.recv().await {
                if event_tx.send(UiEvent::Flow { event }).await.is_err() {
                    break;
                }
            }
        });
    }
    tokio::spawn(crate::flow::drive(
        crate::flow::endpoint_dir(),
        flow_events_tx,
        flow_rx,
    ));

    // the DevTools door rides this runtime; a bind failure logs and the app
    // keeps working — the door is an integration, not a dependency
    let (door_tx, _) = tokio::sync::broadcast::channel::<String>(64);
    let door = match crate::ipc::start(event_tx.clone(), door_tx.clone()).await {
        Ok(door) => Some(door),
        Err(err) => {
            tracing::warn!("ipc door unavailable: {err:#}");
            None
        }
    };
    net_loop(registry, command_rx, event_tx, &door_tx).await;
    if let Some(door) = &door {
        crate::ipc::unpublish(door);
    }
}

/// The command loop, without the door: the seam the net tests drive directly.
async fn net_loop(
    registry: crate::harness::HarnessRegistry,
    command_rx: async_channel::Receiver<NetCommand>,
    event_tx: async_channel::Sender<UiEvent>,
    door_tx: &tokio::sync::broadcast::Sender<String>,
) {
    // Instances are created through the registry and owned by the host; the
    // map below tracks which net keys have a live adapter and event pump.
    let host = Arc::new(Mutex::new(crate::harness::HarnessHost::new(
        registry.clone(),
    )));
    let instances: Arc<Mutex<HashMap<String, RunningHarness>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // a boot in flight is tracked separately so a duplicate start request does
    // not spawn a second child, and a stop can cancel it
    let starting: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let start_cancelled: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    while let Ok(command) = command_rx.recv().await {
        match command {
            NetCommand::StartServer {
                ws_key,
                harness,
                executable,
                folder,
            } => {
                // A boot can take 25–40 s; awaiting it here would freeze every
                // other command behind it. Spawn it and keep the lane free.
                if instances.lock().expect("instances").contains_key(&ws_key) {
                    continue;
                }
                let claimed = {
                    let mut starting = starting.lock().expect("starting");
                    if starting.contains(&ws_key) {
                        // a boot is already in flight; a fresh start request
                        // clears a stop that raced it, so that boot lands
                        start_cancelled
                            .lock()
                            .expect("start cancellation")
                            .remove(&ws_key);
                        false
                    } else {
                        start_cancelled
                            .lock()
                            .expect("start cancellation")
                            .remove(&ws_key);
                        starting.insert(ws_key.clone())
                    }
                };
                if !claimed {
                    continue;
                }
                let host = Arc::clone(&host);
                let instances = Arc::clone(&instances);
                let starting = Arc::clone(&starting);
                let cancelled = Arc::clone(&start_cancelled);
                let event_tx = event_tx.clone();
                let workspace_scoped = registry.get(&harness).is_some_and(|descriptor| {
                    descriptor.scope == crate::harness::InstanceScope::Workspace
                });
                tokio::spawn(async move {
                    // the descriptor's scope decides whether this is a
                    // per-workspace instance or the one app-wide singleton
                    let directory = if workspace_scoped {
                        Some(std::path::PathBuf::from(&folder))
                    } else {
                        None
                    };
                    let scope = directory.clone();
                    let start_host = Arc::clone(&host);
                    let start_harness = harness.clone();
                    let start_directory = directory.clone();
                    let started = tokio::task::spawn_blocking(move || -> Result<HarnessHandle> {
                        let handle = start_host
                            .lock()
                            .expect("harness host")
                            .ensure_instance(
                                &start_harness,
                                &executable,
                                start_directory.as_deref(),
                            )
                            .map_err(|err| anyhow!("{err}"))?;
                        handle
                            .lock()
                            .expect("harness instance")
                            .start()
                            .map_err(|err| anyhow!("{err}"))?;
                        Ok(handle)
                    })
                    .await;
                    let result = match started {
                        Ok(result) => result,
                        Err(err) => Err(anyhow!("harness start task failed: {err}")),
                    };
                    starting.lock().expect("starting").remove(&ws_key);
                    // a stop that landed while booting wins: tear the child
                    // down and stay silent (StopServer already announced)
                    let cancelled = cancelled
                        .lock()
                        .expect("start cancellation")
                        .remove(&ws_key);
                    match result {
                        Ok(handle) if !cancelled => {
                            let events = handle.lock().expect("harness instance").events();
                            let pump = tokio::spawn(forward_events(
                                events,
                                ws_key.clone(),
                                harness.clone(),
                                directory.clone(),
                                event_tx.clone(),
                            ));
                            instances.lock().expect("instances").insert(
                                ws_key.clone(),
                                RunningHarness {
                                    harness,
                                    scope,
                                    handle,
                                    pump,
                                },
                            );
                            let _ = event_tx
                                .send(UiEvent::ServerStarted {
                                    ws_key,
                                    result: Ok(()),
                                })
                                .await;
                        }
                        Ok(handle) => {
                            // cancelled mid-boot: undo the instance entirely so
                            // its runtime and child cannot linger
                            let removed = host
                                .lock()
                                .expect("harness host")
                                .remove_instance_at(&harness, scope.as_deref());
                            let handle = removed.unwrap_or(handle);
                            let _ = tokio::task::spawn_blocking(move || {
                                let mut instance = handle.lock().expect("harness instance");
                                let _ = instance.stop();
                            })
                            .await;
                        }
                        Err(err) => {
                            // a failed boot leaves no half-started instance
                            let failed = host
                                .lock()
                                .expect("harness host")
                                .remove_instance_at(&harness, scope.as_deref());
                            if let Some(handle) = failed {
                                let _ = tokio::task::spawn_blocking(move || {
                                    let mut instance = handle.lock().expect("harness instance");
                                    let _ = instance.stop();
                                })
                                .await;
                            }
                            let _ = event_tx
                                .send(UiEvent::ServerStarted {
                                    ws_key,
                                    result: Err(err),
                                })
                                .await;
                        }
                    }
                });
            }
            NetCommand::StopServer { ws_key } => {
                // a boot in flight has no map entry yet: remember the stop for
                // the task to honour as soon as its start returns
                start_cancelled
                    .lock()
                    .expect("start cancellation")
                    .insert(ws_key.clone());
                let entry = instances.lock().expect("instances").remove(&ws_key);
                if let Some(entry) = entry {
                    entry.pump.abort();
                    // take the instance out of the host so its runtime and
                    // child are freed instead of lingering per stopped key
                    let removed = host
                        .lock()
                        .expect("harness host")
                        .remove_instance_at(&entry.harness, entry.scope.as_deref());
                    let handle = removed.unwrap_or(entry.handle);
                    // a stop kills a child; never hold the command lane on a
                    // request that still owns the instance
                    drop(tokio::task::spawn_blocking(move || {
                        let mut instance = handle.lock().expect("harness instance");
                        let _ = instance.stop();
                    }));
                }
                let _ = event_tx.send(UiEvent::ServerStopped).await;
            }
            NetCommand::ProbeRepository { folder } => {
                let event_tx = event_tx.clone();
                // a process call, not an async request: keep it off this lane
                tokio::task::spawn_blocking(move || {
                    let path = std::path::Path::new(&folder);
                    let mut repository = None;
                    let mut worktrees = Vec::new();
                    if warpui_kit_worktree::is_repository(path)
                        && let Ok(entries) = warpui_kit_worktree::list(path)
                    {
                        // git lists the main checkout first, whichever folder
                        // the call ran from
                        if let Some(main) = entries.iter().find(|entry| entry.main) {
                            repository = Some(main.path.to_string_lossy().into_owned());
                        }
                        worktrees = entries
                            .into_iter()
                            .filter(|entry| !entry.main)
                            .map(|entry| (entry.path.to_string_lossy().into_owned(), entry.label()))
                            .collect();
                    }
                    let _ = event_tx.send_blocking(UiEvent::RepositoryProbed {
                        folder,
                        repository,
                        worktrees,
                    });
                });
            }
            NetCommand::CreateWorktree {
                tile_key,
                repo,
                branch,
            } => {
                let event_tx = event_tx.clone();
                // `git worktree add` checks a whole branch out: a blocking lane,
                // never the UI thread and never this command loop
                tokio::task::spawn_blocking(move || {
                    let event =
                        match warpui_kit_worktree::create(std::path::Path::new(&repo), &branch) {
                            Ok(created) => UiEvent::WorktreeCreated {
                                tile_key,
                                folder: created.path.to_string_lossy().into_owned(),
                                branch: created.branch.unwrap_or(branch),
                            },
                            Err(err) => UiEvent::WorktreeFailed {
                                tile_key,
                                error: format!("{err:#}"),
                            },
                        };
                    let _ = event_tx.send_blocking(event);
                });
            }
            NetCommand::AttachWorktree {
                tile_key,
                repo,
                folder,
                branch,
            } => {
                let event_tx = event_tx.clone();
                tokio::task::spawn_blocking(move || {
                    let error = warpui_kit_worktree::attach(
                        std::path::Path::new(&repo),
                        &branch,
                        std::path::Path::new(&folder),
                    )
                    .err()
                    .map(|err| format!("{err:#}"));
                    let _ = event_tx.send_blocking(UiEvent::WorktreeAttached {
                        tile_key,
                        folder,
                        branch,
                        error,
                    });
                });
            }
            NetCommand::Request { ws_key, request } => {
                let target = instance_for(&instances, &ws_key);
                let Some((harness, directory, handle)) = target else {
                    continue;
                };
                let app_scope = registry.get(&harness).is_some_and(|descriptor| {
                    descriptor.scope == crate::harness::InstanceScope::App
                });
                let event_tx = event_tx.clone();
                let ws_key = ws_key.clone();
                tokio::spawn(async move {
                    let target_ws = ws_key.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        dispatch_request(
                            &handle,
                            &harness,
                            directory.as_deref(),
                            app_scope,
                            &target_ws,
                            request,
                        )
                    })
                    .await;
                    match result {
                        Ok(Some(event)) => {
                            let _ = event_tx.send(event).await;
                        }
                        Ok(None) => {}
                        Err(err) => {
                            let _ = event_tx
                                .send(UiEvent::Log {
                                    ws_key,
                                    line: format!("harness request failed: {err}"),
                                })
                                .await;
                        }
                    }
                });
            }
            NetCommand::SendMessage {
                ws_key,
                session_id,
                text,
                model,
                variant,
                agent,
                attachments,
                command,
            } => {
                let target = instance_for(&instances, &ws_key);
                let Some((_, _, handle)) = target else {
                    // don't drop silently — the tile would sit "running" forever
                    let _ = event_tx
                        .send(UiEvent::MessageSendFailed {
                            session_id: session_id.clone(),
                            error: "this chat's workspace server is not running".into(),
                        })
                        .await;
                    continue;
                };
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let prompt = crate::harness::Prompt {
                        text,
                        choice: crate::harness::ModelChoice {
                            model,
                            agent,
                            variant,
                        },
                        background: false,
                        attachments: attachments
                            .into_iter()
                            .map(|(filename, mime, bytes)| crate::harness::Attachment {
                                filename,
                                mime,
                                bytes,
                            })
                            .collect(),
                        command: command
                            .map(|(name, text)| crate::harness::CommandRun { name, text }),
                    };
                    let session = crate::harness::SessionId::new(session_id.clone());
                    let result = tokio::task::spawn_blocking(move || {
                        handle
                            .lock()
                            .expect("harness instance")
                            .send_prompt(&session, prompt)
                    })
                    .await;
                    let error = match result {
                        Ok(Ok(())) => None,
                        Ok(Err(err)) => Some(err.to_string()),
                        Err(err) => Some(format!("harness send task failed: {err}")),
                    };
                    if let Some(error) = error {
                        let _ = event_tx
                            .send(UiEvent::MessageSendFailed { session_id, error })
                            .await;
                    }
                });
            }
            NetCommand::ForgetSession { session_id } => {
                let Some(shared) = crate::harness::shared_server_id() else {
                    continue;
                };
                let handle = instances
                    .lock()
                    .expect("instances")
                    .get(shared.as_str())
                    .map(|entry| Arc::clone(&entry.handle));
                let Some(handle) = handle else {
                    continue;
                };
                let session = crate::harness::SessionId::new(session_id);
                drop(tokio::task::spawn_blocking(move || {
                    let _ = handle
                        .lock()
                        .expect("harness instance")
                        .forget_session(&session);
                }));
            }
            NetCommand::HarnessCall {
                call_id,
                harness,
                executable,
                directory,
                method,
                params,
            } => {
                let host = Arc::clone(&host);
                let event_tx = event_tx.clone();
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        harness_call(
                            &host,
                            &harness,
                            &executable,
                            directory.as_deref(),
                            &method,
                            &params,
                        )
                    })
                    .await
                    .map_err(|err| format!("harness call task failed: {err}"))
                    .and_then(|result| result);
                    let _ = event_tx
                        .send(UiEvent::HarnessCallResult { call_id, result })
                        .await;
                });
            }
            NetCommand::IpcBroadcast { line } => {
                let _ = door_tx.send(line);
            }
        }
    }
    // shutdown: stop every spawned adapter and its pump
    let remaining: Vec<RunningHarness> = instances
        .lock()
        .expect("instances")
        .drain()
        .map(|(_, entry)| entry)
        .collect();
    for entry in remaining {
        entry.pump.abort();
        let mut instance = entry.handle.lock().expect("harness instance");
        let _ = instance.stop();
    }
    let _ = host.lock().expect("harness host").shutdown();
}

/// The running harness a net key names, with what a request needs to reach it.
fn instance_for(
    instances: &Arc<Mutex<HashMap<String, RunningHarness>>>,
    ws_key: &str,
) -> Option<(HarnessId, Option<std::path::PathBuf>, HarnessHandle)> {
    instances
        .lock()
        .expect("instances")
        .get(ws_key)
        .map(|entry| {
            (
                entry.harness.clone(),
                entry.scope.clone(),
                Arc::clone(&entry.handle),
            )
        })
}

/// One door harness call against the host's instance for the harness's scope.
///
/// The call is parsed before an instance is touched: an unknown method or a
/// missing param answers `unsupported:`/the param error and never spawns a
/// backend. A managed instance starts on demand — the adapter's own guard
/// makes a second start a no-op, and a workspace harness must carry its
/// folder. The typed answer (or its door-prefixed error) rides back to the
/// store as [`UiEvent::HarnessCallResult`].
fn harness_call(
    host: &Arc<Mutex<crate::harness::HarnessHost>>,
    harness: &HarnessId,
    executable: &str,
    directory: Option<&std::path::Path>,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    let call = crate::harness::call::Call::parse(method, params, directory)?;
    let handle = {
        let mut host = host.lock().expect("harness host");
        let Some(descriptor) = host.registry().get(harness) else {
            return Err(crate::harness::call::unknown(harness.as_str()));
        };
        if descriptor.scope == crate::harness::InstanceScope::Workspace && directory.is_none() {
            return Err(format!(
                "unavailable: harness {harness} needs a workspace directory"
            ));
        }
        host.ensure_instance(harness, executable, directory)
            .map_err(crate::harness::call::error_string)?
    };
    let mut instance = handle.lock().expect("harness instance");
    instance
        .start()
        .map_err(crate::harness::call::error_string)?;
    call.execute(instance.as_mut())
}

/// Forward one adapter's typed stream to the store: connection, health and
/// process-exit facts become their app-level notifications, everything else
/// travels as the typed event the store ingests.
async fn forward_events(
    events: async_channel::Receiver<crate::harness::HarnessEvent>,
    ws_key: String,
    harness: HarnessId,
    directory: Option<std::path::PathBuf>,
    event_tx: async_channel::Sender<UiEvent>,
) {
    while let Ok(event) = events.recv().await {
        // the store's "connected to the event stream" note keys off this
        // app-level frame, not the typed connection fact
        if matches!(&event, crate::harness::HarnessEvent::Connected) {
            let _ = event_tx
                .send(UiEvent::Event {
                    ws_key: ws_key.clone(),
                    event: json!({ "type": "server.connected" }),
                })
                .await;
        }
        let out = match event {
            crate::harness::HarnessEvent::Connected => UiEvent::StreamStatus {
                ws_key: ws_key.clone(),
                status: StreamStatus::Connected,
            },
            crate::harness::HarnessEvent::Reconnecting { attempt } => UiEvent::StreamStatus {
                ws_key: ws_key.clone(),
                status: StreamStatus::Reconnecting { attempt },
            },
            crate::harness::HarnessEvent::Disconnected { .. } => UiEvent::StreamStatus {
                ws_key: ws_key.clone(),
                status: StreamStatus::Failed,
            },
            crate::harness::HarnessEvent::Exited => UiEvent::ServerCrashed {
                ws_key: ws_key.clone(),
            },
            crate::harness::HarnessEvent::Health(health) => UiEvent::HealthChanged {
                ws_key: ws_key.clone(),
                healthy: Some(health.state == crate::harness::HealthState::Ready),
            },
            event => UiEvent::HarnessEvent {
                harness: harness.clone(),
                directory: directory
                    .as_ref()
                    .map(|directory| directory.to_string_lossy().into_owned()),
                event,
            },
        };
        if event_tx.send(out).await.is_err() {
            return;
        }
    }
}

/// One control request, dispatched to the instance the net key names.
///
/// The answers keep the store's existing app-level shape; a request whose
/// result arrives later on the adapter's event stream (a command, say) answers
/// with `None`.
fn dispatch_request(
    handle: &HarnessHandle,
    harness: &HarnessId,
    directory: Option<&std::path::Path>,
    app_scope: bool,
    ws_key: &str,
    request: ApiRequest,
) -> Option<UiEvent> {
    use crate::harness::{ModelChoice, NewSession, SessionId, bridge};

    let mut instance = handle.lock().expect("harness instance");
    match request {
        ApiRequest::ListSessions => Some(UiEvent::SessionsLoaded {
            ws_key: ws_key.to_string(),
            shared: app_scope,
            result: typed(instance.sessions())
                .map(|sessions| sessions.into_iter().map(bridge::session_info).collect()),
        }),
        ApiRequest::ListSessionsIn { folder } => {
            let result = typed(instance.sessions()).map(|sessions| {
                sessions
                    .into_iter()
                    .filter(|summary| {
                        summary.directory.as_ref().is_some_and(|directory| {
                            directory.to_string_lossy().as_ref() == folder.as_str()
                        })
                    })
                    .map(bridge::session_info)
                    .collect()
            });
            Some(UiEvent::SessionsLoaded {
                ws_key: folder,
                shared: true,
                result,
            })
        }
        ApiRequest::CreateSessionIn { folder, title } => {
            let result = typed(instance.create_session(NewSession {
                title: Some(title),
                parent: None,
                directory: Some(std::path::PathBuf::from(&folder)),
                choice: ModelChoice::default(),
            }))
            .map(bridge::session_info);
            Some(UiEvent::SessionCreated {
                ws_key: folder,
                result,
            })
        }
        ApiRequest::CreateForkSessionIn {
            folder,
            title,
            context,
        } => {
            let result = typed(instance.create_session(NewSession {
                title: Some(title),
                parent: None,
                directory: Some(std::path::PathBuf::from(&folder)),
                choice: ModelChoice::default(),
            }))
            .map(bridge::session_info);
            Some(UiEvent::ForkSessionCreated {
                ws_key: folder,
                context,
                result,
            })
        }
        ApiRequest::ListModels => Some(UiEvent::ModelsLoaded {
            harness: harness.clone(),
            result: typed(instance.models()).map(|models| bridge::model_infos(harness, models)),
        }),
        ApiRequest::ListAgents => Some(UiEvent::AgentsLoaded {
            result: typed(instance.agents()).map(bridge::agent_infos),
        }),
        ApiRequest::GetMessages { session_id } => {
            let result = typed(instance.transcript(&SessionId::new(session_id.clone())))
                .map(bridge::stored_messages);
            Some(UiEvent::MessagesLoaded {
                ws_key: ws_key.to_string(),
                session_id,
                result,
            })
        }
        ApiRequest::CreateSession { title } => {
            let result = typed(instance.create_session(NewSession {
                title: Some(title),
                parent: None,
                directory: directory.map(std::path::Path::to_path_buf),
                choice: ModelChoice::default(),
            }))
            .map(bridge::session_info);
            Some(UiEvent::SessionCreated {
                ws_key: ws_key.to_string(),
                result,
            })
        }
        ApiRequest::CreateForkSession { title, context } => {
            let result = typed(instance.create_session(NewSession {
                title: Some(title),
                parent: None,
                directory: directory.map(std::path::Path::to_path_buf),
                choice: ModelChoice::default(),
            }))
            .map(bridge::session_info);
            Some(UiEvent::ForkSessionCreated {
                ws_key: ws_key.to_string(),
                context,
                result,
            })
        }
        ApiRequest::RenameSession { session_id, title } => {
            let result = typed(
                instance.rename_session(&SessionId::new(session_id.clone()), &title),
            )
            .map(|()| SessionInfo {
                id: session_id.clone(),
                title: title.clone(),
                updated_ms: now_ms(),
                created_ms: 0,
                parent_id: None,
            });
            Some(UiEvent::SessionRenamed { session_id, result })
        }
        ApiRequest::DeleteSession { session_id } => Some(UiEvent::SessionDeleted {
            session_id: session_id.clone(),
            result: typed(instance.delete_session(&SessionId::new(session_id))),
        }),
        ApiRequest::Abort { session_id } => Some(UiEvent::Aborted {
            session_id: session_id.clone(),
            result: typed(instance.interrupt(&SessionId::new(session_id))),
        }),
        ApiRequest::RespondPermission {
            session_id,
            permission_id,
            response,
        } => {
            let decision = match response.as_str() {
                "always" => crate::harness::PermissionDecision::Always,
                "reject" => crate::harness::PermissionDecision::Reject,
                _ => crate::harness::PermissionDecision::Once,
            };
            let result = typed(instance.reply_permission(
                &SessionId::new(session_id),
                &permission_id,
                decision,
            ));
            Some(UiEvent::PermissionReplied {
                permission_id,
                response,
                is_auto: false,
                result,
            })
        }
        ApiRequest::GetQuestions { for_session } => Some(UiEvent::QuestionsPulled {
            for_session: for_session.clone(),
            result: typed(instance.forms(&SessionId::new(for_session)))
                .map(bridge::question_requests),
        }),
        ApiRequest::ReplyQuestion {
            session_id,
            request_id,
            field_ids,
            answers,
        } => {
            let summary = answers
                .iter()
                .map(|answer| {
                    if answer.is_empty() {
                        "(empty)".to_string()
                    } else {
                        answer.join(", ")
                    }
                })
                .collect::<Vec<_>>()
                .join(" · ");
            let result = typed(instance.reply_form(
                &SessionId::new(session_id),
                &request_id,
                bridge::form_answer_for_ids(&field_ids, answers),
            ));
            Some(UiEvent::QuestionReplied {
                request_id,
                summary,
                result,
            })
        }
        ApiRequest::RejectQuestion {
            session_id,
            request_id,
        } => {
            let result = typed(instance.reply_form(
                &SessionId::new(session_id),
                &request_id,
                crate::harness::FormAnswer::default(),
            ));
            Some(UiEvent::QuestionReplied {
                request_id,
                summary: "dismissed".into(),
                result,
            })
        }
        ApiRequest::ListCommands => Some(UiEvent::CommandsLoaded {
            ws_key: ws_key.to_string(),
            result: command_catalog(&mut **instance, None),
        }),
        ApiRequest::ListAgentCommands { session_id } => {
            let session = session_id.map(SessionId::new);
            Some(UiEvent::CommandsLoaded {
                ws_key: ws_key.to_string(),
                result: command_catalog(&mut **instance, session.as_ref()),
            })
        }
        ApiRequest::CreateCommandSession { tile_key, folder } => {
            let result = typed(instance.create_session(NewSession {
                title: Some("New chat".into()),
                parent: None,
                directory: Some(std::path::PathBuf::from(&folder)),
                choice: ModelChoice::default(),
            }))
            .map(bridge::session_info);
            Some(UiEvent::CommandSessionCreated {
                tile_key,
                ws_key: folder,
                result,
            })
        }
        ApiRequest::ExecuteCommand { session_id, line } => {
            let result = typed(
                instance.run_command(&SessionId::new(session_id.clone()), command_run(&line)),
            );
            match result {
                // the adapter reports the command's answer on its stream
                Ok(()) => None,
                Err(err) => Some(UiEvent::CommandExecuted {
                    session_id,
                    result: Err(err),
                }),
            }
        }
        ApiRequest::CompactSession { session_id, .. } => Some(UiEvent::SessionCompacted {
            session_id: session_id.clone(),
            result: typed(instance.compact(&SessionId::new(session_id))),
        }),
        ApiRequest::FindFiles { query } => Some(UiEvent::FilesFound {
            ws_key: ws_key.to_string(),
            query: query.clone(),
            result: typed(instance.find_files(&query)),
        }),
        ApiRequest::ListMcpNames => Some(match instance.mcp_servers() {
            Ok(servers) => UiEvent::McpNamesLoaded {
                ws_key: ws_key.to_string(),
                result: Ok(servers.into_iter().map(|server| server.name).collect()),
            },
            Err(err) => UiEvent::Log {
                ws_key: ws_key.to_string(),
                line: format!("mcp: {err}"),
            },
        }),
        ApiRequest::McpConnect { name } => {
            let line = match instance.connect_mcp(&name) {
                Ok(()) => format!("mcp: connect {name}"),
                Err(err) => format!("mcp: connect {name} failed: {err}"),
            };
            Some(UiEvent::Log {
                ws_key: ws_key.to_string(),
                line,
            })
        }
        ApiRequest::McpDisconnect { name } => {
            let line = match instance.disconnect_mcp(&name) {
                Ok(()) => format!("mcp: disconnect {name}"),
                Err(err) => format!("mcp: disconnect {name} failed: {err}"),
            };
            Some(UiEvent::Log {
                ws_key: ws_key.to_string(),
                line,
            })
        }
    }
}

/// An adapter failure as the store's app-level result.
fn typed<T>(result: Result<T, crate::harness::HarnessError>) -> Result<T> {
    result.map_err(|err| anyhow!("{err}"))
}

fn command_catalog(
    instance: &mut dyn crate::harness::Harness,
    session: Option<&crate::harness::SessionId>,
) -> Result<Vec<CommandInfo>> {
    let commands = instance.commands(session).map_err(|err| anyhow!("{err}"))?;
    let skills = instance.skills(session).map_err(|err| anyhow!("{err}"))?;
    Ok(crate::harness::bridge::command_infos(commands, skills))
}

/// The `(name, args)` a slash line names; `line` arrives with its `/`.
fn command_run(line: &str) -> crate::harness::CommandRun {
    let trimmed = line.trim().trim_start_matches('/');
    match trimmed.split_once(char::is_whitespace) {
        Some((name, args)) => crate::harness::CommandRun {
            name: name.to_string(),
            text: args.trim().to_string(),
        },
        None => crate::harness::CommandRun {
            name: trimmed.to_string(),
            text: String::new(),
        },
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::decode_base64;

    #[test]
    fn base64_roundtrips_rfc4648_vectors() {
        for (raw, want) in [
            (&b""[..], ""),
            (b"f".as_slice(), "Zg=="),
            (b"fo".as_slice(), "Zm8="),
            (b"foo".as_slice(), "Zm9v"),
            (b"foob".as_slice(), "Zm9vYg=="),
            (b"fooba".as_slice(), "Zm9vYmE="),
            (b"foobar".as_slice(), "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(raw), want, "encoding {raw:?}");
            assert_eq!(decode_base64(want).as_deref(), Some(raw), "decoding {want}");
        }
        // the attachment decoder tolerates transport newlines
        assert_eq!(
            decode_base64("Zm9v\nYmFy").as_deref(),
            Some(b"foobar".as_slice())
        );
        assert_eq!(decode_base64("not*base64"), None);
    }

    fn fake_handle(id: &str, fake: crate::harness::test_support::FakeHarness) -> HarnessHandle {
        let _ = id;
        Arc::new(Mutex::new(Box::new(fake)))
    }

    fn a_summary(id: &str, title: &str, folder: Option<&str>) -> crate::harness::SessionSummary {
        crate::harness::SessionSummary {
            id: crate::harness::SessionId::new(id),
            title: title.to_string(),
            parent: None,
            directory: folder.map(std::path::PathBuf::from),
            updated_ms: 7,
            created_ms: 3,
        }
    }

    #[test]
    fn a_session_list_request_is_answered_from_the_adapter() {
        use crate::harness::test_support::FakeHarness;

        let handle = fake_handle(
            "opencode",
            FakeHarness::with_sessions("opencode", vec![a_summary("s1", "Chat", Some("/tmp/ws"))]),
        );
        let event = dispatch_request(
            &handle,
            &HarnessId::new("opencode"),
            Some(std::path::Path::new("/tmp/ws")),
            false,
            "/tmp/ws",
            ApiRequest::ListSessions,
        )
        .expect("a sessions answer");
        let UiEvent::SessionsLoaded {
            ws_key,
            shared,
            result,
        } = event
        else {
            panic!("expected a sessions answer");
        };
        assert_eq!(ws_key, "/tmp/ws");
        assert!(!shared);
        let sessions = result.expect("the adapter lists sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s1");
        assert_eq!(sessions[0].updated_ms, 7);
        assert_eq!(sessions[0].created_ms, 3);
    }

    #[test]
    fn a_folder_list_filters_an_app_wide_instance() {
        use crate::harness::test_support::FakeHarness;

        let handle = fake_handle(
            "dsh",
            FakeHarness::with_sessions(
                "dsh",
                vec![
                    a_summary("mine", "Mine", Some("/tmp/ws")),
                    a_summary("other", "Other", Some("/tmp/other")),
                ],
            ),
        );
        let event = dispatch_request(
            &handle,
            &HarnessId::new("dsh"),
            None,
            true,
            "dsh",
            ApiRequest::ListSessionsIn {
                folder: "/tmp/ws".into(),
            },
        )
        .expect("a sessions answer");
        let UiEvent::SessionsLoaded {
            ws_key,
            shared,
            result,
        } = event
        else {
            panic!("expected a sessions answer");
        };
        assert_eq!(ws_key, "/tmp/ws");
        assert!(shared, "an app-wide instance's folder list is shared");
        let sessions = result.expect("the adapter lists sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "mine");
    }

    #[test]
    fn a_create_request_carries_the_folder_and_title() {
        use crate::harness::test_support::FakeHarness;

        let handle = fake_handle(
            "dsh",
            FakeHarness::with_created_session("dsh", a_summary("s1", "New chat", None)),
        );
        let event = dispatch_request(
            &handle,
            &HarnessId::new("dsh"),
            None,
            true,
            "dsh",
            ApiRequest::CreateSessionIn {
                folder: "/tmp/ws".into(),
                title: "New chat".into(),
            },
        )
        .expect("a create answer");
        let UiEvent::SessionCreated { ws_key, result } = event else {
            panic!("expected a create answer");
        };
        assert_eq!(ws_key, "/tmp/ws");
        let info = result.expect("the adapter creates the session");
        assert_eq!(info.id, "s1");
        assert_eq!(info.title, "New chat");
    }

    #[test]
    fn a_command_catalog_merges_commands_and_skills() {
        use crate::harness::test_support::FakeHarness;

        let handle = fake_handle(
            "opencode",
            FakeHarness::with_commands(
                "opencode",
                vec![crate::harness::Command {
                    name: "Plan".into(),
                    description: "draft a plan".into(),
                }],
                vec![crate::harness::Skill {
                    id: "review".into(),
                    name: "Review".into(),
                    description: "review a change".into(),
                }],
            ),
        );
        let event = dispatch_request(
            &handle,
            &HarnessId::new("opencode"),
            Some(std::path::Path::new("/tmp/ws")),
            false,
            "/tmp/ws",
            ApiRequest::ListCommands,
        )
        .expect("a catalog answer");
        let UiEvent::CommandsLoaded { ws_key, result } = event else {
            panic!("expected a catalog answer");
        };
        assert_eq!(ws_key, "/tmp/ws");
        let commands = result.expect("the adapter lists commands");
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].name_lower, "plan");
        assert_eq!(commands[0].source, "command");
        assert_eq!(commands[1].name_lower, "review");
        assert_eq!(commands[1].source, "skill");
    }

    #[test]
    fn an_execute_command_dispatches_and_waits_for_the_event() {
        use crate::harness::test_support::FakeHarness;

        let fake = FakeHarness::new("dsh");
        let recorded = Arc::clone(&fake.commands_run);
        let handle = fake_handle("dsh", fake);
        let event = dispatch_request(
            &handle,
            &HarnessId::new("dsh"),
            None,
            true,
            "dsh",
            ApiRequest::ExecuteCommand {
                session_id: "s1".into(),
                line: "/plan the migration".into(),
            },
        );
        assert!(event.is_none(), "the command answers on the event stream");
        let recorded = recorded.lock().expect("recorded commands").clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].name, "plan");
        assert_eq!(recorded[0].text, "the migration");
    }

    #[test]
    fn the_event_forwarder_maps_facts_and_tags_data_events() {
        use crate::harness::{HarnessEvent, PartPayload, PartSnapshot, SessionId};

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("forwarder runtime");
        runtime.block_on(async {
            let (events_tx, events_rx) = async_channel::unbounded();
            let (event_tx, event_rx) = async_channel::unbounded();
            let task = tokio::spawn(forward_events(
                events_rx,
                "dsh".into(),
                HarnessId::new("dsh"),
                None,
                event_tx,
            ));
            let _ = events_tx.send(HarnessEvent::Connected).await;
            let _ = events_tx.send(HarnessEvent::Exited).await;
            let _ = events_tx
                .send(HarnessEvent::PartSnapshot {
                    part: PartSnapshot {
                        id: "p1".into(),
                        session: SessionId::new("s1"),
                        message_id: "m1".into(),
                        payload: PartPayload::Text("hi".into()),
                    },
                })
                .await;
            let _ = events_tx
                .send(HarnessEvent::Reconnecting { attempt: 2 })
                .await;
            drop(events_tx);
            task.await.expect("the forwarder ends with its stream");

            let mut frames = Vec::new();
            while let Ok(event) = event_rx.try_recv() {
                frames.push(event);
            }
            assert_eq!(frames.len(), 5);
            assert!(matches!(&frames[0], UiEvent::Event { event, .. }
                if event.get("type").and_then(Value::as_str) == Some("server.connected")));
            assert!(matches!(
                &frames[1],
                UiEvent::StreamStatus {
                    status: StreamStatus::Connected,
                    ..
                }
            ));
            assert!(matches!(&frames[2], UiEvent::ServerCrashed { .. }));
            let UiEvent::HarnessEvent {
                harness,
                directory,
                event: HarnessEvent::PartSnapshot { part },
            } = &frames[3]
            else {
                panic!("a data event is forwarded tagged with its harness");
            };
            assert_eq!(harness.as_str(), "dsh");
            assert!(directory.is_none());
            assert_eq!(part.id, "p1");
            assert!(matches!(
                &frames[4],
                UiEvent::StreamStatus {
                    status: StreamStatus::Reconnecting { attempt: 2 },
                    ..
                }
            ));
        });
    }

    fn wait_for(
        events: &async_channel::Receiver<UiEvent>,
        matcher: impl Fn(&UiEvent) -> bool,
    ) -> UiEvent {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match events.try_recv() {
                Ok(event) => {
                    if matcher(&event) {
                        return event;
                    }
                }
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "timed out waiting for a net event"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(async_channel::TryRecvError::Closed) => panic!("the net thread ended"),
            }
        }
    }

    #[test]
    fn the_net_thread_routes_a_harness_through_its_adapter() {
        use crate::harness::test_support::FakeHarness;
        use crate::harness::{
            ConnectionKind, HarnessCapabilities, HarnessConfig, HarnessDescriptor, HarnessEvent,
            HarnessRegistry, InstanceScope,
        };

        const FAKE: &str = "fake";
        fn factory(_config: HarnessConfig) -> Box<dyn crate::harness::Harness> {
            Box::new(
                FakeHarness::with_sessions(
                    FAKE,
                    vec![a_summary("s1", "Fake chat", Some("/tmp/fake-ws"))],
                )
                .scripted(vec![HarnessEvent::SessionCreated {
                    session: crate::harness::SessionId::new("s1"),
                    title: Some("Fake chat".into()),
                    parent: None,
                    agent: None,
                }]),
            )
        }

        let mut registry = HarnessRegistry::new();
        registry
            .register(HarnessDescriptor {
                id: HarnessId::new(FAKE),
                label: "fake",
                default_executable: "fake",
                connection: ConnectionKind::Managed,
                scope: InstanceScope::App,
                capabilities: HarnessCapabilities::NONE,
                launch: None,
                factory: Some(factory),
            })
            .expect("the fake registers");

        let (command_tx, command_rx) = async_channel::unbounded();
        let (event_tx, events) = async_channel::unbounded();
        let (door_tx, _) = tokio::sync::broadcast::channel::<String>(8);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("net runtime");
            runtime.block_on(net_loop(registry, command_rx, event_tx, &door_tx));
        });
        let net = NetSender(command_tx);

        net.send(NetCommand::StartServer {
            ws_key: FAKE.into(),
            harness: HarnessId::new(FAKE),
            executable: "fake".into(),
            folder: ".".into(),
        });
        wait_for(&events, |event| {
            matches!(event, UiEvent::ServerStarted { result: Ok(()), .. })
        });

        net.send(NetCommand::Request {
            ws_key: FAKE.into(),
            request: ApiRequest::ListSessions,
        });
        let answered = wait_for(&events, |event| {
            matches!(event, UiEvent::SessionsLoaded { result: Ok(_), .. })
        });
        let UiEvent::SessionsLoaded { result, .. } = answered else {
            unreachable!()
        };
        assert_eq!(result.expect("the adapter lists sessions").len(), 1);

        net.send(NetCommand::SendMessage {
            ws_key: FAKE.into(),
            session_id: "s1".into(),
            text: "hello".into(),
            model: None,
            variant: None,
            agent: None,
            attachments: Vec::new(),
            command: None,
        });
        let forwarded = wait_for(&events, |event| {
            matches!(
                event,
                UiEvent::HarnessEvent {
                    event: HarnessEvent::SessionCreated { .. },
                    ..
                }
            )
        });
        let UiEvent::HarnessEvent { harness, event, .. } = forwarded else {
            unreachable!()
        };
        assert_eq!(harness.as_str(), FAKE);
        assert!(
            matches!(event, HarnessEvent::SessionCreated { session, .. } if session.as_str() == "s1")
        );

        net.send(NetCommand::StopServer {
            ws_key: FAKE.into(),
        });
        wait_for(&events, |event| matches!(event, UiEvent::ServerStopped));
        drop(net);
        thread.join().expect("the net thread stops");
    }

    #[test]
    fn a_blocking_task_can_drive_an_adapter_runtime() {
        let net = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("net runtime");
        let value = net.block_on(async {
            tokio::task::spawn_blocking(|| {
                let adapter = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("adapter runtime");
                adapter.block_on(async {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    2
                })
            })
            .await
            .expect("blocking task")
        });
        assert_eq!(value, 2);
    }

    /// Every dispatched category reaches the host's adapter and comes back as
    /// the typed JSON result on the call's own id.
    #[test]
    fn the_net_thread_dispatches_the_door_harness_surface() {
        use crate::harness::test_support::SurfaceHarness;
        use crate::harness::{
            ConnectionKind, HarnessCapabilities, HarnessConfig, HarnessDescriptor, HarnessRegistry,
            InstanceScope,
        };

        const FAKE: &str = "fake";
        fn factory(_config: HarnessConfig) -> Box<dyn crate::harness::Harness> {
            Box::new(SurfaceHarness::new(FAKE))
        }

        let mut registry = HarnessRegistry::new();
        registry
            .register(HarnessDescriptor {
                id: HarnessId::new(FAKE),
                label: "fake",
                default_executable: "fake",
                connection: ConnectionKind::Managed,
                scope: InstanceScope::App,
                capabilities: HarnessCapabilities::NONE,
                launch: None,
                factory: Some(factory),
            })
            .expect("the fake registers");

        let (command_tx, command_rx) = async_channel::unbounded();
        let (event_tx, events) = async_channel::unbounded();
        let (door_tx, _) = tokio::sync::broadcast::channel::<String>(8);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("net runtime");
            runtime.block_on(net_loop(registry, command_rx, event_tx, &door_tx));
        });
        let net = NetSender(command_tx);

        let mut call_id = 0;
        let mut call = |method: &str, params: Value| -> Result<Value, String> {
            call_id += 1;
            net.send(NetCommand::HarnessCall {
                call_id,
                harness: HarnessId::new(FAKE),
                executable: "fake".into(),
                directory: Some(std::path::PathBuf::from("/tmp/fake-ws")),
                method: method.into(),
                params,
            });
            let answered = wait_for(
                &events,
                |event| matches!(event, UiEvent::HarnessCallResult { call_id: id, .. } if *id == call_id),
            );
            let UiEvent::HarnessCallResult { result, .. } = answered else {
                unreachable!()
            };
            result
        };

        let catalogs = [
            ("providers", "providers"),
            ("agents", "agents"),
            ("commands", "commands"),
            ("skills", "skills"),
        ];
        for (method, key) in catalogs {
            let result = call(method, json!({"session": "s1"})).expect(method);
            assert!(result.get(key).is_some(), "{method}: {result}");
        }
        let sessions = call("sessions", json!({})).expect("sessions");
        assert_eq!(sessions["sessions"][0]["id"], "session-1");
        let session = call("session", json!({"session": "s1"})).expect("session");
        assert_eq!(session["id"], "s1");
        let transcript = call("transcript", json!({"session": "s1"})).expect("transcript");
        assert_eq!(transcript["messages"][0]["message"]["role"], "assistant");

        let created = call(
            "create",
            json!({"title": "Door chat", "model": "provider-1/model-1"}),
        )
        .expect("create");
        assert_eq!(created["title"], "Door chat");
        for method in ["fork", "rename", "delete"] {
            let params = match method {
                "rename" => json!({"session": "s1", "title": "Renamed"}),
                _ => json!({"session": "s1"}),
            };
            let result = call(method, params).expect(method);
            if method == "fork" {
                assert_eq!(result["id"], "s1");
            } else {
                assert_eq!(result, json!({"ok": true}), "{method}");
            }
        }
        for method in ["interrupt", "compact", "run_command", "activate_skill"] {
            let params = match method {
                "run_command" => json!({"session": "s1", "command": "plan", "text": "x"}),
                "activate_skill" => json!({"session": "s1", "skill": "review"}),
                _ => json!({"session": "s1"}),
            };
            assert_eq!(
                call(method, params).expect(method),
                json!({"ok": true}),
                "{method}"
            );
        }
        let prompt = call(
            "send_prompt",
            json!({"session": "s1", "text": "hello", "model": "provider-1/model-1"}),
        )
        .expect("send_prompt");
        assert_eq!(prompt, json!({"ok": true}));

        let permissions = call("permissions", json!({"session": "s1"})).expect("permissions");
        assert_eq!(permissions["permissions"][0]["action"], "bash");
        assert_eq!(
            call(
                "reply_permission",
                json!({"session": "s1", "request": "permission-1", "decision": "always"}),
            )
            .expect("reply_permission"),
            json!({"ok": true})
        );
        let forms = call("forms", json!({"session": "s1"})).expect("forms");
        assert_eq!(forms["forms"][0]["title"], "Proceed?");
        assert_eq!(
            call(
                "reply_form",
                json!({"session": "s1", "form": "form-1", "values": {"0": "yes"}}),
            )
            .expect("reply_form"),
            json!({"ok": true})
        );
        let diffs = call("diff", json!({"session": "s1"})).expect("diff");
        assert_eq!(diffs["diffs"][0]["path"], "src/main.rs");

        let inbox = call("inbox", json!({"session": "s1"})).expect("inbox");
        assert_eq!(inbox["inbox"][0]["delivery"], "queue");
        assert_eq!(
            call(
                "update_inbox",
                json!({"session": "s1", "item": "item-1", "delivery": "steer"}),
            )
            .expect("update_inbox"),
            json!({"ok": true})
        );
        assert_eq!(
            call("cancel_inbox", json!({"session": "s1", "item": "item-1"})).expect("cancel_inbox"),
            json!({"ok": true})
        );
        for method in ["stage_revert", "commit_revert", "clear_revert"] {
            let params = match method {
                "stage_revert" => json!({"session": "s1", "before": "m1"}),
                _ => json!({"session": "s1"}),
            };
            assert_eq!(
                call(method, params).expect(method),
                json!({"ok": true}),
                "{method}"
            );
        }
        for method in ["run_shell", "background_session"] {
            let params = match method {
                "run_shell" => json!({"session": "s1", "command": "ls"}),
                _ => json!({"session": "s1"}),
            };
            assert_eq!(
                call(method, params).expect(method),
                json!({"ok": true}),
                "{method}"
            );
        }
        let mcp = call("mcp_list", json!({})).expect("mcp_list");
        assert_eq!(mcp["servers"][0]["status"], "connected");
        for (method, params) in [
            ("mcp_add", json!({"name": "docs", "command": "docs-mcp"})),
            ("mcp_connect", json!({"name": "docs"})),
            ("mcp_disconnect", json!({"name": "docs"})),
            ("mcp_remove", json!({"name": "docs"})),
        ] {
            assert_eq!(
                call(method, params).expect(method),
                json!({"ok": true}),
                "{method}"
            );
        }
        assert_eq!(
            call("find_files", json!({"query": "main"})).expect("find_files")["files"][0],
            "src/main.rs"
        );
        assert_eq!(
            call("forget_session", json!({"session": "s1"})).expect("forget_session"),
            json!({"ok": true})
        );

        // an unknown harness is refused before the host creates an instance
        let unknown_call = 9999;
        net.send(NetCommand::HarnessCall {
            call_id: unknown_call,
            harness: HarnessId::new("no-such-harness"),
            executable: "fake".into(),
            directory: None,
            method: "providers".into(),
            params: json!({}),
        });
        let refused = wait_for(
            &events,
            |event| matches!(event, UiEvent::HarnessCallResult { call_id, .. } if *call_id == unknown_call),
        );
        let UiEvent::HarnessCallResult { result, .. } = refused else {
            unreachable!()
        };
        assert_eq!(result.unwrap_err(), "unknown: harness no-such-harness");

        // an unknown method is refused before an adapter is touched
        let error = call("no-such-method", json!({})).unwrap_err();
        assert_eq!(error, "unsupported: harness method no-such-method");

        drop(net);
        thread.join().expect("the net thread stops");
    }

    /// An adapter's typed `Unsupported` keeps the door's prefix.
    #[test]
    fn an_unsupported_adapter_operation_keeps_the_door_prefix() {
        use crate::harness::test_support::FakeHarness;
        use crate::harness::{
            ConnectionKind, HarnessCapabilities, HarnessConfig, HarnessDescriptor, HarnessRegistry,
            InstanceScope,
        };

        const FAKE: &str = "fake";
        fn factory(_config: HarnessConfig) -> Box<dyn crate::harness::Harness> {
            Box::new(FakeHarness::new(FAKE))
        }
        let mut registry = HarnessRegistry::new();
        registry
            .register(HarnessDescriptor {
                id: HarnessId::new(FAKE),
                label: "fake",
                default_executable: "fake",
                connection: ConnectionKind::Managed,
                scope: InstanceScope::App,
                capabilities: HarnessCapabilities::NONE,
                launch: None,
                factory: Some(factory),
            })
            .expect("the fake registers");
        let (command_tx, command_rx) = async_channel::unbounded();
        let (event_tx, events) = async_channel::unbounded();
        let (door_tx, _) = tokio::sync::broadcast::channel::<String>(8);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("net runtime");
            runtime.block_on(net_loop(registry, command_rx, event_tx, &door_tx));
        });
        let net = NetSender(command_tx);
        net.send(NetCommand::HarnessCall {
            call_id: 1,
            harness: HarnessId::new(FAKE),
            executable: "fake".into(),
            directory: None,
            method: "agents".into(),
            params: json!({}),
        });
        let answered = wait_for(&events, |event| {
            matches!(event, UiEvent::HarnessCallResult { call_id: 1, .. })
        });
        let UiEvent::HarnessCallResult { result, .. } = answered else {
            unreachable!()
        };
        assert_eq!(result.unwrap_err(), "unsupported: harness operation agents");
        drop(net);
        thread.join().expect("the net thread stops");
    }

    #[test]
    fn a_slash_line_splits_into_a_command_run() {
        let bare = command_run("/compact");
        assert_eq!(bare.name, "compact");
        assert!(bare.text.is_empty());

        let with_args = command_run("/plan  the migration ");
        assert_eq!(with_args.name, "plan");
        assert_eq!(with_args.text, "the migration");

        let no_slash = command_run("compact");
        assert_eq!(no_slash.name, "compact");
    }

    #[test]
    fn a_typed_failure_keeps_its_message() {
        let err = typed::<()>(Err(crate::harness::HarnessError::Unavailable(
            "opencode is not running".into(),
        )))
        .unwrap_err();
        assert!(err.to_string().contains("opencode is not running"));
    }

    #[cfg(unix)]
    #[test]
    fn a_managed_child_outlives_the_spawn_blocking_worker() {
        fn args(_port: u16) -> Vec<String> {
            vec![
                "-c".into(),
                "echo 'http://127.0.0.1:1/?token=abc'; sleep 5".into(),
            ]
        }
        fn env() -> Vec<(&'static str, &'static str)> {
            Vec::new()
        }
        fn capture(line: &str) -> Option<String> {
            crate::dsh::token_from_line(line).map(str::to_string)
        }
        let spec = crate::harness::LaunchSpec {
            args,
            env,
            readiness: crate::harness::Readiness::LaunchCredential,
            timeout: Duration::from_secs(5),
            attach_healthy: false,
            capture_line: Some(capture),
            timeout_message: "fake child did not print its token",
        };
        // the real start path runs on a spawn_blocking worker, and Linux's
        // parent-death signal is tied to that thread: the child must survive
        // the worker retiring, or every managed server dies ~20 s in
        let process = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(ServerProcess::start(spec, "sh", ".", 0, |_| {}))
        })
        .join()
        .expect("start thread")
        .expect("fake child starts");

        std::thread::sleep(Duration::from_millis(500));
        let alive = process
            .child
            .lock()
            .expect("child lock")
            .as_mut()
            .and_then(|child| child.try_wait().ok())
            .is_some_and(|status| status.is_none());
        let mut process = process;
        process.stop();
        assert!(alive, "the managed child was killed with its spawn thread");
    }

    /// One `/proc/<pid>/cmdline` body: every argument NUL-terminated.
    #[cfg(unix)]
    fn argv_line(argv: &[&str]) -> String {
        let mut line = argv.join("\0");
        line.push('\0');
        line
    }

    #[cfg(unix)]
    #[test]
    fn a_managed_dsh_child_is_recognized_in_every_launch_form() {
        // the configured executable itself, bare or as a path
        for argv in [
            ["dsh", "web", "--no-open", "--host", "127.0.0.1", "--port", "4096"].as_slice(),
            ["/usr/local/bin/dsh", "web", "--no-open", "--host", "127.0.0.1", "--port", "4096"]
                .as_slice(),
            // an npm-installed dsh is a shell shim that execs Node; the kernel
            // records the runtime and the package script, not `dsh`
            [
                "node",
                "/home/u/.local/share/mise/installs/npm-deepseek-ai-dsh/0.2.2/node_modules/@deepseek-ai/dsh/lib/bin.js",
                "web",
                "--no-open",
                "--host",
                "127.0.0.1",
                "--port",
                "4096",
            ]
            .as_slice(),
        ] {
            assert_eq!(
                managed_child_from_cmdline(&argv_line(argv)),
                Some(ManagedChild::DshWeb { port: 4096 }),
                "argv {argv:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_managed_opencode_child_is_recognized_in_every_launch_form() {
        for argv in [
            ["opencode", "serve", "--hostname", "127.0.0.1", "--port", "5123"].as_slice(),
            // the npm package ships its Linux binary with the Windows suffix
            [
                "/home/u/.local/share/mise/installs/opencode/2.3.2/node_modules/@opencode/cli/bin/opencode.exe",
                "serve",
                "--hostname",
                "127.0.0.1",
                "--port",
                "5123",
            ]
            .as_slice(),
        ] {
            assert_eq!(
                managed_child_from_cmdline(&argv_line(argv)),
                Some(ManagedChild::OpencodeServe { port: 5123 }),
                "argv {argv:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unmanaged_command_lines_are_left_alone() {
        let dsh_script = "/home/u/.local/share/mise/installs/npm-deepseek-ai-dsh/0.2.2/node_modules/@deepseek-ai/dsh/lib/bin.js";
        for argv in [
            // the user's own instance, the one shape called out by name
            [
                "dsh",
                "web",
                "--patch",
                "/home/u/.dsh/remote-host.overlay.yml",
                "--no-open",
            ]
            .as_slice(),
            [
                "node",
                dsh_script,
                "web",
                "--patch",
                "/tmp/overlay.yml",
                "--no-open",
            ]
            .as_slice(),
            // a different host, a reordered flag, an extra flag
            [
                "dsh",
                "web",
                "--no-open",
                "--host",
                "0.0.0.0",
                "--port",
                "4096",
            ]
            .as_slice(),
            [
                "dsh",
                "web",
                "--no-open",
                "--port",
                "4096",
                "--host",
                "127.0.0.1",
            ]
            .as_slice(),
            [
                "dsh",
                "web",
                "--no-open",
                "--verbose",
                "--host",
                "127.0.0.1",
                "--port",
                "4096",
            ]
            .as_slice(),
            // opencode's shape with dsh's host flag
            ["opencode", "serve", "--host", "127.0.0.1", "--port", "4096"].as_slice(),
            // the same flags under a program that is not one of the harnesses
            [
                "node",
                "/tmp/other-app/lib/bin.js",
                "web",
                "--no-open",
                "--host",
                "127.0.0.1",
                "--port",
                "4096",
            ]
            .as_slice(),
            [
                "python3",
                "server.py",
                "serve",
                "--hostname",
                "127.0.0.1",
                "--port",
                "4096",
            ]
            .as_slice(),
            // something that merely mentions the shape in a shell command
            [
                "bash",
                "-c",
                "dsh web --no-open --host 127.0.0.1 --port 4096",
            ]
            .as_slice(),
            // not a port the launch args could have written
            [
                "dsh",
                "web",
                "--no-open",
                "--host",
                "127.0.0.1",
                "--port",
                "0",
            ]
            .as_slice(),
            [
                "dsh",
                "web",
                "--no-open",
                "--host",
                "127.0.0.1",
                "--port",
                "+4096",
            ]
            .as_slice(),
            [
                "dsh",
                "web",
                "--no-open",
                "--host",
                "127.0.0.1",
                "--port",
                "04096",
            ]
            .as_slice(),
            [
                "dsh",
                "web",
                "--no-open",
                "--host",
                "127.0.0.1",
                "--port",
                "65536",
            ]
            .as_slice(),
            ["dsh", "web", "--no-open", "--host", "127.0.0.1", "--port"].as_slice(),
            // an empty argument is not normalized away
            [
                "dsh",
                "web",
                "",
                "--no-open",
                "--host",
                "127.0.0.1",
                "--port",
                "4096",
            ]
            .as_slice(),
            // limits
            ["dsh"].as_slice(),
            [""].as_slice(),
        ] {
            assert_eq!(
                managed_child_from_cmdline(&argv_line(argv)),
                None,
                "argv {argv:?}"
            );
        }
        assert_eq!(managed_child_from_cmdline(""), None);
        assert_eq!(managed_child_from_cmdline("\0"), None);
    }

    #[cfg(unix)]
    #[test]
    fn only_an_orphan_of_this_user_is_stale() {
        let opencode = argv_line(&[
            "opencode",
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            "5123",
        ]);
        let expected = Some(ManagedChild::OpencodeServe { port: 5123 });
        // PPID 1 and the same user: the run that spawned it is gone
        assert_eq!(stale_managed_child(&opencode, 1, 1000, 1000), expected);
        // a live parent still owns the child
        assert_eq!(stale_managed_child(&opencode, 4321, 1000, 1000), None);
        // another user's process is not ours to touch
        assert_eq!(stale_managed_child(&opencode, 1, 1001, 1000), None);
        // the user's own patch instance is never a managed shape
        let patch = argv_line(&["dsh", "web", "--patch", "/tmp/overlay.yml", "--no-open"]);
        assert_eq!(stale_managed_child(&patch, 1, 1000, 1000), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_parent_pid_is_the_fourth_stat_field() {
        assert_eq!(
            parent_pid_from_stat("4242 (dsh web) S 1 4242 4242 0 -1"),
            Some(1)
        );
        // comm may contain spaces and parentheses of its own
        assert_eq!(
            parent_pid_from_stat("7 (evil ) name) R 900 7 7 0 -1"),
            Some(900)
        );
        assert_eq!(parent_pid_from_stat("not a stat line"), None);
        assert_eq!(parent_pid_from_stat("7 (dsh) R"), None);
        assert_eq!(parent_pid_from_stat(""), None);
    }
}
