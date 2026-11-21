//! The DevTools door: a token-authenticated NDJSON server on the net
//! thread's runtime. DevTools (or a second launch of this app) opens
//! workspaces, starts chats and runs one-shot prompts through it. The app
//! itself never depends on a client — commands relay to the store through
//! the UiEvent pump and answers ride a oneshot back to the wire.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, oneshot};
use warpui_kit_platform::door::{self, ANSWER_TIMEOUT, MAX_LINE, cmd, token_matches};

use crate::harness::HarnessId;
use crate::oc::UiEvent;
use crate::paths::AppPaths;

pub use warpui_kit_platform::door::PROTOCOL;

/// The door endpoint as currently published; None until an instance binds.
pub fn read_endpoint() -> Option<(u16, String)> {
    door::read_endpoint_in(&AppPaths::config_dir())
}

/// Completes a command's wire response: Ok carries the response payload
/// fields, Err the error string. Dropping it without sending reads as
/// "cancelled" on the client.
pub type Reply = oneshot::Sender<Result<Value, String>>;

/// One connection's filtered harness-event stream.
#[derive(Clone, Debug)]
struct Subscription {
    id: String,
    harness: Option<String>,
    session: Option<String>,
    run: Option<u64>,
}

/// The per-chat model knobs a door create/run carries. Grouped so the store
/// side stays under the argument-count lint and the three fields move
/// together through the seed path.
#[derive(Clone, Debug, Default)]
pub struct DoorModel {
    pub model: Option<String>,
    pub harness: Option<HarnessId>,
    pub variant: Option<String>,
}

/// A door command bound for the store. Ping never gets here — the
/// connection answers it locally.
pub enum IpcCommand {
    OpenWorkspace {
        path: String,
        reply: Reply,
    },
    NewChat {
        workspace: String,
        prefill: String,
        auto_send: bool,
        model: DoorModel,
        reply: Reply,
    },
    /// `run` is the wire request id. The reply fires only when the turn
    /// idles (or dies), not when the prompt is accepted. `background` keeps
    /// the run out of the app's own UI — no tile, no focus, no sidebar row —
    /// for clients that consume the answer themselves (protocol 2).
    RunPrompt {
        run: u64,
        workspace: String,
        prompt: String,
        model: DoorModel,
        background: bool,
        reply: Reply,
    },
    CancelRun {
        run: u64,
        reply: Reply,
    },
    Models {
        /// None answers with every catalog the app knows; a harness starts
        /// that backend when it has no models yet and answers when loaded.
        harness: Option<HarnessId>,
        /// Ask the backend for a fresh list instead of answering from the
        /// in-memory catalog. The settings refresh button sets it.
        refresh: bool,
        reply: Reply,
    },
    /// One app-level harness method through the door's single namespace.
    Harness {
        method: String,
        params: Value,
        reply: Reply,
    },
    Status {
        reply: Reply,
    },
}

/// The bound door; held by the net thread so `unpublish` can tell our files
/// from a newer instance's.
pub struct Door {
    pub port: u16,
    pub token: String,
    dir: PathBuf,
}

/// Binds the door on a loopback port and publishes port+token under
/// ~/.warp/agent. Token is regenerated every launch, so a stale file never
/// grants access.
pub async fn start(
    event_tx: async_channel::Sender<UiEvent>,
    events: broadcast::Sender<String>,
) -> anyhow::Result<Door> {
    start_in(AppPaths::config_dir(), event_tx, events).await
}

/// Test seam for [`start`]: the door publishes into `dir` instead.
pub async fn start_in(
    dir: PathBuf,
    event_tx: async_channel::Sender<UiEvent>,
    events: broadcast::Sender<String>,
) -> anyhow::Result<Door> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let token = door::random_token();
    std::fs::create_dir_all(&dir)?;
    // token first: a reader that sees a port is entitled to its token. Both
    // files are created 0o600 — restricting after a default-mode write leaves
    // a window where the token is world-readable (restrict_perms stays as
    // belt-and-braces and now reports if it cannot confirm).
    let token_path = door::token_file_in(&dir);
    door::write_private(&token_path, &token)?;
    door::restrict_perms(&token_path);
    door::write_private(&door::port_file_in(&dir), &format!("{port}\n"))?;
    door::restrict_perms(&door::port_file_in(&dir));
    tokio::spawn(accept_loop(listener, event_tx, events, token.clone()));
    Ok(Door { port, token, dir })
}

/// Clears the published endpoint when the files still point at this door.
pub fn unpublish(door: &Door) {
    let ours = |file: &Path, value: &str| {
        std::fs::read_to_string(file).is_ok_and(|current| current.trim() == value)
    };
    if ours(&door::port_file_in(&door.dir), &door.port.to_string()) {
        let _ = std::fs::remove_file(door::port_file_in(&door.dir));
    }
    if ours(&door::token_file_in(&door.dir), &door.token) {
        let _ = std::fs::remove_file(door::token_file_in(&door.dir));
    }
}

async fn accept_loop(
    listener: TcpListener,
    event_tx: async_channel::Sender<UiEvent>,
    events: broadcast::Sender<String>,
    token: String,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        tokio::spawn(handle_conn(
            stream,
            event_tx.clone(),
            events.subscribe(),
            token.clone(),
        ));
    }
}

/// Length-checked token compare lives in the protocol crate (the timing
/// guarantee travels with the contract).
async fn handle_conn(
    stream: TcpStream,
    event_tx: async_channel::Sender<UiEvent>,
    mut events: broadcast::Receiver<String>,
    token: String,
) {
    let _ = stream.set_nodelay(true);
    let (read, write) = stream.into_split();
    let (lines_tx, lines_rx) = tokio::sync::mpsc::channel::<String>(64);
    let subscriptions = Arc::new(Mutex::new(Vec::<Subscription>::new()));
    tokio::spawn(write_pump(write, lines_rx));
    let mut reader = BufReader::new(read);

    // auth handshake: one line, then the door stays NDJSON forever
    let Some(line) = read_line(&mut reader).await else {
        return;
    };
    let authed = match serde_json::from_str::<Value>(&line) {
        Ok(value) => value
            .get("auth")
            .and_then(Value::as_str)
            .is_some_and(|auth| token_matches(auth, &token)),
        Err(_) => false,
    };
    if !authed {
        let _ = lines_tx
            .send(json!({"ok": false, "error": "auth"}).to_string())
            .await;
        return;
    }
    let _ = lines_tx.send(json!({"ok": true}).to_string()).await;

    loop {
        let line = tokio::select! {
            biased;
            outgoing = events.recv() => {
                match outgoing {
                    Ok(line) => {
                        match harness_event_frames(&line, &subscriptions) {
                            None => {
                                if lines_tx.send(line).await.is_err() {
                                    return;
                                }
                            }
                            Some(frames) => {
                                for frame in frames {
                                    if lines_tx.send(frame).await.is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let overflow = json!({
                            "event": "subscribe-overflow",
                            "error": "event stream lagged; resubscribe",
                        });
                        if lines_tx.send(overflow.to_string()).await.is_err() {
                            return;
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        let closed = json!({
                            "event": "subscribe-closed",
                            "reason": "app shutting down",
                        });
                        let _ = lines_tx.send(closed.to_string()).await;
                        return;
                    }
                }
            }
            incoming = read_line(&mut reader) => match incoming {
                Some(line) => line,
                None => return,
            },
        };
        if line.trim().is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(err) => {
                let _ = reply(&lines_tx, 0, Err(format!("bad frame: {err}"))).await;
                continue;
            }
        };
        let Some(id) = request.get("id").and_then(Value::as_u64) else {
            let _ = reply(&lines_tx, 0, Err("missing id".into())).await;
            continue;
        };
        // run-prompt's reply can take minutes: every request runs as its own
        // task so the connection keeps reading (cancel is a new frame)
        tokio::spawn(serve_request(
            request,
            id,
            event_tx.clone(),
            lines_tx.clone(),
            subscriptions.clone(),
        ));
    }
}

async fn write_pump(mut write: OwnedWriteHalf, mut lines: tokio::sync::mpsc::Receiver<String>) {
    while let Some(line) = lines.recv().await {
        let broken = write.write_all(line.as_bytes()).await.is_err()
            || write.write_all(b"\n").await.is_err()
            || write.flush().await.is_err();
        if broken {
            return;
        }
    }
}

/// Reads one NDJSON line; None drops the connection (EOF, error, or a line
/// over the cap — a frame that big is a protocol error, not a prompt).
async fn read_line(reader: &mut BufReader<OwnedReadHalf>) -> Option<String> {
    let mut limited = reader.take(MAX_LINE as u64 + 1);
    let mut line = String::new();
    match limited.read_line(&mut line).await {
        Ok(0) => None,
        Ok(_) if line.len() > MAX_LINE => None,
        Ok(_) => Some(line),
        Err(_) => None,
    }
}

fn parse_harness(request: &Value) -> Result<Option<HarnessId>, String> {
    match request.get("harness").and_then(Value::as_str) {
        None => Ok(None),
        Some(id) => HarnessId::from_wire(id)
            .map(Some)
            .ok_or_else(|| format!("unknown harness: {id}")),
    }
}

/// Filters one broadcast line for a connection's harness subscriptions. `None`
/// means the line is not a harness event and goes out unchanged; `Some(frames)`
/// replaces it with one copy per matching subscription (empty = drop it).
fn harness_event_frames(
    line: &str,
    subscriptions: &Mutex<Vec<Subscription>>,
) -> Option<Vec<String>> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("event").and_then(Value::as_str) != Some("harness-event") {
        return None;
    }
    let subscriptions = subscriptions.lock().ok()?;
    let mut frames = Vec::new();
    for subscription in subscriptions.iter() {
        let matches = |expected: Option<&str>, actual: Option<&str>| match expected {
            Some(expected) => actual == Some(expected),
            None => true,
        };
        if !matches(
            subscription.harness.as_deref(),
            value.get("harness").and_then(Value::as_str),
        ) {
            continue;
        }
        if !matches(
            subscription.session.as_deref(),
            value.get("session").and_then(Value::as_str),
        ) {
            continue;
        }
        if let Some(run) = subscription.run
            && value.get("run").and_then(Value::as_u64) != Some(run)
        {
            continue;
        }
        let mut frame = value.clone();
        frame["subscription"] = json!(subscription.id);
        frames.push(frame.to_string());
    }
    Some(frames)
}

async fn serve_request(
    request: Value,
    id: u64,
    event_tx: async_channel::Sender<UiEvent>,
    lines: tokio::sync::mpsc::Sender<String>,
    subscriptions: Arc<Mutex<Vec<Subscription>>>,
) {
    let cmd = request
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let field = |name: &str| {
        request
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let required = |name: &str| -> Option<String> {
        let value = field(name)?;
        (!value.trim().is_empty()).then_some(value)
    };
    let (reply_tx, reply_rx) = oneshot::channel();

    if cmd == cmd::SUBSCRIBE || cmd == cmd::UNSUBSCRIBE {
        let subscription = request
            .get("subscription")
            .and_then(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| value.as_u64().map(|number| number.to_string()))
            })
            .unwrap_or_else(|| id.to_string());
        if cmd == cmd::UNSUBSCRIBE {
            if let Ok(mut subscriptions) = subscriptions.lock() {
                subscriptions.retain(|entry| entry.id != subscription);
            }
            let _ = reply(
                &lines,
                id,
                Ok(json!({"subscription": subscription, "unsubscribed": true})),
            )
            .await;
            return;
        }
        let entry = Subscription {
            id: subscription.clone(),
            harness: field("harness"),
            session: field("session"),
            run: request.get("run").and_then(Value::as_u64),
        };
        if let Ok(mut subscriptions) = subscriptions.lock() {
            subscriptions.retain(|entry| entry.id != subscription);
            subscriptions.push(entry);
        }
        let _ = reply(&lines, id, Ok(json!({"subscription": subscription}))).await;
        return;
    }

    let command = match cmd.as_str() {
        cmd::PING => {
            let _ = reply(
                &lines,
                id,
                Ok(json!({"version": PROTOCOL, "app": "warp-agent"})),
            )
            .await;
            return;
        }
        cmd::OPEN_WORKSPACE => match required("path") {
            Some(path) => Some(IpcCommand::OpenWorkspace {
                path,
                reply: reply_tx,
            }),
            None => {
                let _ = reply(&lines, id, Err("path is required".into())).await;
                return;
            }
        },
        cmd::NEW_CHAT => {
            let workspace = field("workspace").unwrap_or_else(|| "general".into());
            let auto_send = request
                .get("auto_send")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let harness = match parse_harness(&request) {
                Ok(harness) => harness,
                Err(err) => {
                    let _ = reply(&lines, id, Err(err)).await;
                    return;
                }
            };
            Some(IpcCommand::NewChat {
                workspace,
                prefill: field("prefill").unwrap_or_default(),
                auto_send,
                model: DoorModel {
                    model: field("model"),
                    harness,
                    variant: field("variant"),
                },
                reply: reply_tx,
            })
        }
        cmd::RUN_PROMPT => match required("prompt") {
            Some(prompt) => {
                let harness = match parse_harness(&request) {
                    Ok(harness) => harness,
                    Err(err) => {
                        let _ = reply(&lines, id, Err(err)).await;
                        return;
                    }
                };
                Some(IpcCommand::RunPrompt {
                    run: id,
                    workspace: field("workspace").unwrap_or_else(|| "general".into()),
                    prompt,
                    model: DoorModel {
                        model: field("model"),
                        harness,
                        variant: field("variant"),
                    },
                    background: request
                        .get("background")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    reply: reply_tx,
                })
            }
            None => {
                let _ = reply(&lines, id, Err("prompt is required".into())).await;
                return;
            }
        },
        cmd::CANCEL => match request.get("run").and_then(Value::as_u64) {
            Some(run) => Some(IpcCommand::CancelRun {
                run,
                reply: reply_tx,
            }),
            None => {
                let _ = reply(&lines, id, Err("run is required".into())).await;
                return;
            }
        },
        cmd::MODELS => {
            let harness = match parse_harness(&request) {
                Ok(harness) => harness,
                Err(err) => {
                    let _ = reply(&lines, id, Err(err)).await;
                    return;
                }
            };
            Some(IpcCommand::Models {
                harness,
                refresh: request
                    .get("refresh")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                reply: reply_tx,
            })
        }
        cmd::STATUS => Some(IpcCommand::Status { reply: reply_tx }),
        cmd::HARNESS => match field("method") {
            Some(method) if !method.trim().is_empty() => {
                let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
                if !params.is_object() {
                    let _ = reply(&lines, id, Err("params must be an object".into())).await;
                    return;
                }
                Some(IpcCommand::Harness {
                    method,
                    params,
                    reply: reply_tx,
                })
            }
            _ => {
                let _ = reply(&lines, id, Err("method is required".into())).await;
                return;
            }
        },
        other => {
            let _ = reply(&lines, id, Err(format!("unknown command: {other}"))).await;
            return;
        }
    };
    let Some(command) = command else { return };

    if event_tx.send(UiEvent::Ipc { command }).await.is_err() {
        let _ = reply(&lines, id, Err("app is shutting down".into())).await;
        return;
    }
    let answer = if cmd == cmd::RUN_PROMPT {
        match reply_rx.await {
            Ok(result) => result,
            // the store dropped the sender (cancelled run, closing app)
            Err(_) => Err("cancelled".into()),
        }
    } else {
        match tokio::time::timeout(ANSWER_TIMEOUT, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("cancelled".into()),
            Err(_) => Err("timed out".into()),
        }
    };
    let _ = reply(&lines, id, answer).await;
}

async fn reply(
    lines: &tokio::sync::mpsc::Sender<String>,
    id: u64,
    result: Result<Value, String>,
) -> Result<(), tokio::sync::mpsc::error::SendError<String>> {
    // frame building (reserved id/ok fields, error envelope) is the protocol
    // crate's
    lines.send(door::answer_frame(id, result).to_string()).await
}

// ---- second-launch handoff (blocking, pre-runtime) -------------------------

/// When a live instance owns the door, hand it the launch folders (a bare
/// liveness probe when there are none) and report true so the caller can
/// exit. Stale files simply fail to connect.
pub fn try_forward(folders: &[String]) -> bool {
    let Some((port, token)) = read_endpoint() else {
        return false;
    };
    let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let say = |stream: &mut std::net::TcpStream, line: &Value| -> bool {
        use std::io::Write;
        stream.write_all(format!("{line}\n").as_bytes()).is_ok()
    };
    let hear = |stream: &mut std::net::TcpStream| -> Option<Value> {
        use std::io::Read;
        let mut line = String::new();
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(1) if byte[0] == b'\n' => break,
                Ok(1) => {
                    line.push(byte[0] as char);
                    if line.len() > 64 * 1024 {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        serde_json::from_str(&line).ok()
    };
    if !say(&mut stream, &json!({"auth": token})) {
        return false;
    }
    let Some(Some(welcome)) = hear(&mut stream).map(|v| v.get("ok").and_then(Value::as_bool))
    else {
        return false;
    };
    if !welcome {
        return false;
    }
    // ping: the port could belong to some other loopback listener
    if !say(&mut stream, &json!({"id": 1, "cmd": cmd::PING})) {
        return false;
    }
    let Some(Some(true)) = hear(&mut stream).map(|v| v.get("ok").and_then(Value::as_bool)) else {
        return false;
    };
    for (index, folder) in folders.iter().enumerate() {
        let id = index as u64 + 2;
        if !say(
            &mut stream,
            &json!({"id": id, "cmd": "open-workspace", "path": folder}),
        ) {
            return false;
        }
        let Some(Some(true)) = hear(&mut stream).map(|v| v.get("ok").and_then(Value::as_bool))
        else {
            return false;
        };
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A disposable publish dir under the system temp dir.
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("warp-agent-ipc-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A fake store answering the door through the UiEvent channel, like
    /// the real pump does.
    async fn fake_store(event_rx: async_channel::Receiver<UiEvent>) {
        while let Ok(UiEvent::Ipc { command }) = event_rx.recv().await {
            match command {
                IpcCommand::OpenWorkspace { path, reply } => {
                    let _ = reply.send(Ok(json!({"workspace": path})));
                }
                IpcCommand::Status { reply } => {
                    let _ = reply.send(Ok(json!({"workspaces": []})));
                }
                IpcCommand::Models {
                    harness,
                    refresh,
                    reply,
                } => {
                    let _ = reply.send(Ok(json!({
                        "harness": harness.as_ref().map(|harness| harness.as_str().to_string()),
                        "refresh": refresh,
                    })));
                }
                IpcCommand::Harness {
                    method,
                    params,
                    reply,
                } => {
                    if method == "metadata" {
                        let _ = reply.send(Ok(json!({
                            "harnesses": [{
                                "id": "opencode",
                                "label": "opencode",
                                "enabled": true,
                                "available": true,
                                "capabilities": {},
                            }]
                        })));
                    } else if params.get("harness").and_then(Value::as_str)
                        == Some("no-such-harness")
                    {
                        let _ = reply.send(Err("unknown: harness no-such-harness".into()));
                    } else if params.get("harness").and_then(Value::as_str) == Some("off") {
                        let _ = reply.send(Err("disabled: harness off is disabled".into()));
                    } else {
                        let _ = reply.send(Err(format!("unsupported: harness {method}")));
                    }
                }
                _ => {}
            }
        }
    }

    async fn connect(port: u16) -> BufReader<TcpStream> {
        BufReader::new(TcpStream::connect(("127.0.0.1", port)).await.unwrap())
    }

    async fn say(stream: &mut BufReader<TcpStream>, value: Value) {
        use tokio::io::AsyncWriteExt;
        let raw = stream.get_mut();
        raw.write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        raw.flush().await.unwrap();
    }

    async fn hear(stream: &mut BufReader<TcpStream>) -> Value {
        let mut line = String::new();
        stream.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    #[tokio::test]
    async fn door_rejects_bad_tokens_and_answers_roundtrips() {
        let dir = scratch_dir("roundtrip");
        let (event_tx, event_rx) = async_channel::unbounded::<UiEvent>();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let door = start_in(dir.clone(), event_tx, broadcast_tx.clone())
            .await
            .unwrap();
        tokio::spawn(fake_store(event_rx));

        // wrong token: refused and closed
        let mut bad = connect(door.port).await;
        say(&mut bad, json!({"auth": "not-the-token"})).await;
        let verdict = hear(&mut bad).await;
        assert!(!verdict["ok"].as_bool().unwrap());
        drop(bad);

        // right token: welcome, ping (answered locally), open-workspace
        // (answered by the fake store), status, and a broadcast push
        let mut good = connect(door.port).await;
        say(&mut good, json!({"auth": door.token})).await;
        assert!(hear(&mut good).await["ok"].as_bool().unwrap());

        say(&mut good, json!({"id": 1, "cmd": cmd::PING})).await;
        let ping = hear(&mut good).await;
        assert_eq!(ping["id"], 1);
        assert!(ping["ok"].as_bool().unwrap());
        assert_eq!(ping["version"], PROTOCOL);

        say(
            &mut good,
            json!({"id": 2, "cmd": cmd::OPEN_WORKSPACE, "path": "/tmp"}),
        )
        .await;
        let opened = hear(&mut good).await;
        assert_eq!(opened["id"], 2);
        assert!(opened["ok"].as_bool().unwrap());
        assert_eq!(opened["workspace"], "/tmp");

        say(&mut good, json!({"id": 3, "cmd": cmd::STATUS})).await;
        let status = hear(&mut good).await;
        assert_eq!(status["id"], 3);
        assert!(status["ok"].as_bool().unwrap());

        // models carries the requested harness into the store, and an
        // unknown id is refused instead of silently becoming opencode
        say(
            &mut good,
            json!({"id": 4, "cmd": cmd::MODELS, "harness": "dsh"}),
        )
        .await;
        let models = hear(&mut good).await;
        assert_eq!(models["id"], 4);
        assert!(models["ok"].as_bool().unwrap());
        assert_eq!(models["harness"], "dsh");

        // refresh is carried to the store: a client asks the door to re-read
        // the backend instead of answering from its in-memory catalog
        say(
            &mut good,
            json!({"id": 6, "cmd": cmd::MODELS, "harness": "dsh", "refresh": true}),
        )
        .await;
        let refreshed = hear(&mut good).await;
        assert_eq!(refreshed["id"], 6);
        assert!(refreshed["ok"].as_bool().unwrap());
        assert_eq!(refreshed["refresh"], true);

        say(
            &mut good,
            json!({"id": 5, "cmd": cmd::MODELS, "harness": "not-a-harness"}),
        )
        .await;
        let refused = hear(&mut good).await;
        assert_eq!(refused["id"], 5);
        assert!(!refused["ok"].as_bool().unwrap());
        assert!(
            refused["error"]
                .as_str()
                .unwrap_or_default()
                .contains("unknown harness")
        );

        // the store's status push fans out to the connected client
        broadcast_tx
            .send(json!({"event": "status", "data": {"workspaces": []}}).to_string())
            .unwrap();
        let pushed = hear(&mut good).await;
        assert_eq!(pushed["event"], "status");

        drop(good);
        unpublish(&door);
        assert!(!door::port_file_in(&dir).exists());
        assert!(!door::token_file_in(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unpublish_leaves_a_newer_door_alone() {
        let dir = scratch_dir("newer");
        let (event_tx, _event_rx) = async_channel::unbounded::<UiEvent>();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let old = start_in(dir.clone(), event_tx, broadcast_tx).await.unwrap();

        // a newer instance overwrote the files
        std::fs::write(door::port_file_in(&dir), "59999\n").unwrap();
        std::fs::write(door::token_file_in(&dir), "newer-token\n").unwrap();
        unpublish(&old);
        assert_eq!(
            std::fs::read_to_string(door::port_file_in(&dir))
                .unwrap()
                .trim(),
            "59999"
        );
        assert_eq!(
            std::fs::read_to_string(door::token_file_in(&dir))
                .unwrap()
                .trim(),
            "newer-token"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// run-prompt reaches the store with the background flag parsed off the
    /// wire: sent it reads true, absent it stays false (protocol-1 clients).
    #[tokio::test]
    async fn run_prompt_carries_the_background_flag() {
        let dir = scratch_dir("background");
        let (event_tx, event_rx) = async_channel::unbounded::<UiEvent>();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let door = start_in(dir.clone(), event_tx, broadcast_tx).await.unwrap();
        let (flags_tx, mut flags_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok(UiEvent::Ipc { command }) = event_rx.recv().await {
                if let IpcCommand::RunPrompt {
                    background, reply, ..
                } = command
                {
                    // the flag rides to the test; the run stays parked, like
                    // a real turn waiting to idle
                    let _ = flags_tx.send(background);
                    std::mem::forget(reply);
                }
            }
        });

        let mut client = connect(door.port).await;
        say(&mut client, json!({"auth": door.token})).await;
        assert!(hear(&mut client).await["ok"].as_bool().unwrap());

        say(
            &mut client,
            json!({"id": 1, "cmd": cmd::RUN_PROMPT, "prompt": "hi", "background": true}),
        )
        .await;
        assert_eq!(flags_rx.recv().await, Some(true));

        say(
            &mut client,
            json!({"id": 2, "cmd": cmd::RUN_PROMPT, "prompt": "hi"}),
        )
        .await;
        assert_eq!(flags_rx.recv().await, Some(false));

        drop(client);
        unpublish(&door);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn door_harness_namespace_and_subscribe_filter() {
        let dir = scratch_dir("harness-subscribe");
        let (event_tx, event_rx) = async_channel::unbounded::<UiEvent>();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let door = start_in(dir.clone(), event_tx, broadcast_tx.clone())
            .await
            .unwrap();
        tokio::spawn(fake_store(event_rx));

        let mut good = connect(door.port).await;
        say(&mut good, json!({"auth": door.token})).await;
        assert!(hear(&mut good).await["ok"].as_bool().unwrap());

        say(
            &mut good,
            json!({"id": 1, "cmd": cmd::HARNESS, "method": "metadata"}),
        )
        .await;
        let metadata = hear(&mut good).await;
        assert!(metadata["ok"].as_bool().unwrap());
        assert_eq!(metadata["harnesses"][0]["id"], "opencode");

        say(
            &mut good,
            json!({
                "id": 2,
                "cmd": cmd::HARNESS,
                "method": "models",
                "params": {"harness": "off"}
            }),
        )
        .await;
        let rejected = hear(&mut good).await;
        assert!(!rejected["ok"].as_bool().unwrap());
        assert!(rejected["error"].as_str().unwrap().contains("disabled"));

        // an unknown harness id is refused with the door's prefix
        say(
            &mut good,
            json!({
                "id": 5,
                "cmd": cmd::HARNESS,
                "method": "models",
                "params": {"harness": "no-such-harness"}
            }),
        )
        .await;
        let unknown = hear(&mut good).await;
        assert_eq!(unknown["id"], 5);
        assert!(!unknown["ok"].as_bool().unwrap());
        assert!(unknown["error"].as_str().unwrap().starts_with("unknown:"));

        // a method the app does not dispatch answers with its own prefix
        say(
            &mut good,
            json!({"id": 6, "cmd": cmd::HARNESS, "method": "no-such-method"}),
        )
        .await;
        let unsupported = hear(&mut good).await;
        assert_eq!(unsupported["id"], 6);
        assert!(!unsupported["ok"].as_bool().unwrap());
        assert!(
            unsupported["error"]
                .as_str()
                .unwrap()
                .starts_with("unsupported:")
        );

        say(
            &mut good,
            json!({
                "id": 3,
                "cmd": cmd::SUBSCRIBE,
                "subscription": "sub-1",
                "harness": "opencode"
            }),
        )
        .await;
        let subscribed = hear(&mut good).await;
        assert_eq!(subscribed["subscription"], "sub-1");

        broadcast_tx
            .send(
                json!({
                    "event": "harness-event",
                    "harness": "opencode",
                    "session": "sess-1",
                    "type": "session-status",
                    "data": {"state": "busy"}
                })
                .to_string(),
            )
            .unwrap();
        let pushed = hear(&mut good).await;
        assert_eq!(pushed["event"], "harness-event");
        assert_eq!(pushed["subscription"], "sub-1");
        assert_eq!(pushed["session"], "sess-1");

        broadcast_tx
            .send(
                json!({
                    "event": "harness-event",
                    "harness": "dsh",
                    "session": "sess-2",
                    "type": "session-status",
                    "data": {"state": "busy"}
                })
                .to_string(),
            )
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), hear(&mut good))
                .await
                .is_err(),
            "a non-matching harness event is filtered out"
        );

        say(
            &mut good,
            json!({"id": 4, "cmd": cmd::UNSUBSCRIBE, "subscription": "sub-1"}),
        )
        .await;
        let unsubscribed = hear(&mut good).await;
        assert_eq!(unsubscribed["unsubscribed"], true);

        drop(good);
        unpublish(&door);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A method the store dispatches to the net thread answers later: the door
    /// keeps the connection open and sends the parked reply's result.
    #[tokio::test]
    async fn door_harness_dispatch_waits_for_the_store_answer() {
        let dir = scratch_dir("harness-dispatch");
        let (event_tx, event_rx) = async_channel::unbounded::<UiEvent>();
        let (broadcast_tx, _) = broadcast::channel::<String>(16);
        let door = start_in(dir.clone(), event_tx, broadcast_tx).await.unwrap();
        let (deferred_tx, mut deferred_rx) = tokio::sync::mpsc::unbounded_channel::<Reply>();
        tokio::spawn(async move {
            while let Ok(UiEvent::Ipc { command }) = event_rx.recv().await {
                if let IpcCommand::Harness { method, reply, .. } = command {
                    assert_eq!(method, "providers");
                    let _ = deferred_tx.send(reply);
                }
            }
        });

        let mut client = connect(door.port).await;
        say(&mut client, json!({"auth": door.token})).await;
        assert!(hear(&mut client).await["ok"].as_bool().unwrap());

        say(
            &mut client,
            json!({
                "id": 7,
                "cmd": cmd::HARNESS,
                "method": "providers",
                "params": {"harness": "opencode"}
            }),
        )
        .await;
        let reply = deferred_rx.recv().await.expect("the store parks the reply");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), hear(&mut client))
                .await
                .is_err(),
            "the door waits for the parked store reply"
        );
        reply
            .send(Ok(json!({"providers": [{"id": "p1", "name": "Provider"}]})))
            .expect("the connection is still reading");
        let answered = hear(&mut client).await;
        assert_eq!(answered["id"], 7);
        assert!(answered["ok"].as_bool().unwrap());
        assert_eq!(answered["providers"][0]["id"], "p1");

        drop(client);
        unpublish(&door);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
