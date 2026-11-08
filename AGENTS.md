# warp-agent — agent instructions

Read [`../../AGENTS.md`](../../AGENTS.md) first. This file adds only the
app-specific facts. The user-facing description is in
[`README.md`](README.md).

## Shape

warp-agent is a single binary crate (`warp-agent`) with **no
`lib.rs` boundary**. That is deliberate today, and it has one consequence: a
probe cannot build the app's views. The one integration probe
(`tests/complex_markdown_probe.rs`) targets the shared markdown kit instead.
Prefer adding a library boundary before adding a view-level probe to this app.

## Where things live

- `src/main.rs` — startup order and the composer key bindings.
- `src/store/` — the app state: workspaces, the shared tile grid, transcripts,
  queues, permissions, runs, sessions, sidebar, streaming and parsing.
- `src/oc.rs` — `opencode serve` lifecycle plus its REST/SSE client on the net
  thread.
- `src/dsh.rs` — DeepSeek Harness (`dsh web`) client and the event translation
  into opencode-shaped events.
- `src/ipc.rs` — the warp-agent door: token-authenticated NDJSON on the net
  thread's runtime.
- `src/ui/` — the window, sidebar, tile grid, settings and markdown view.
- `src/settings.rs`, `paths.rs`, `logging.rs`, `services.rs`, `theme.rs`.

Settings and UI state are versioned snake_case documents; the contract is
`../../docs/agents/settings-schema.md`. The model cache lives in
`~/.warp/agent/cache/models.json` and is disposable.

## Startup order

1. `paths::AppPaths::migrate_legacy_layout()` — absorb `~/.warp-agent` before
   logging chooses its file.
2. `logging::initialize()`; keep the guard.
3. `services::install_panic_hook()`.
4. `ipc::try_forward(&folders)` — a live instance takes the launch folders (or
   a bare liveness probe) and this process exits. A second launch against a
   live instance does not open a second window.
5. `Store::new`, `warpui_kit_core::bootstrap::init` (the retheme closure
   re-derives `store.fonts` and `store.accent`), then the composer key
   bindings, the launch folders, the net-event pump and the countdown loop.

## The door (owned here)

The wire contract lives in `warpui_kit_platform::door`; warp-agent is the
server and Warp Station is the client. Endpoint files are
`~/.warp/agent/ipc.port` and `ipc.token`; auth is one token line, then one
`{"id", "cmd"}` request and one answer.

- Commands: `ping`, `open-workspace`, `new-chat`, `run-prompt`, `cancel`,
  `models`, `status` (plus pushed `status` events).
- `new-chat`/`run-prompt` take a folder or `general` for the built-in Tasks
  workspace, and `harness`/`model`/`variant`.
- `models` returns the harnesses and each model's variants; `refresh: true`
  re-reads the backend instead of answering from the session catalog.
- The app works fully standalone. A missing client changes nothing.
- Changing a command, a field, the endpoint file names or their location is a
  cross-app contract change: update Warp Station in the same commit.

## Harnesses

- `oc.rs` owns the `opencode serve` processes: one per workspace, a free port,
  a 25 s health wait, `OPENCODE_CONFIG_CONTENT` with ask-forcing permissions,
  REST calls and the SSE event pump with its reconnect ladder.
- `dsh.rs` owns `dsh web`: one shared process for every dsh chat. Unary calls
  are `POST /api/<method>` with the `{type:"client-request", ...}` envelope;
  streams ride one WebSocket at `/api/remote.mux`. The launch token is
  exchanged at `/connect` for a `dsh-auth-*` cookie; the cookie dies with its
  WebSocket, so every reconnect re-exchanges it.
- All network work is on a dedicated OS thread with its own tokio runtime.
  Never do file IO, HTTP or process work on the GPUI main thread. The UI talks
  to the net thread through async channels; state arrives already parsed.

## Tests

```sh
cargo test -p warp-agent
cargo build -p warp-agent
```

- `tests/complex_markdown_probe.rs` renders a hostile mixed-markdown reply
  through the same read-only `LiveEditor` the tile uses, so a parser or layout
  panic fails `cargo test`.
- There are no view probes because there is no lib target. Add a lib boundary
  first if a change needs one; see
  [`../../docs/agents/gpui-app-recipe.md`](../../docs/agents/gpui-app-recipe.md).
- The capture hook is `WARP_AGENT_PAGE`: `settings`, `settings-chat`,
  `settings-permissions`, `settings-server`.

## Platform notes

- The packaged Windows exe uses `windows_subsystem = "windows"`; debug builds
  keep the console.
- On Linux, `libc` sets `PR_SET_PDEATHSIG` for the spawned `serve`/`dsh`
  children. If you add a long-lived child, give it the same treatment.
- `composer` key overrides are bound after `gpui_kit::init` (inside the
  bootstrap) with `Some("Input")` and `Some("Composer > Input")` contexts. Do
  not make them global.

## Do not

- Add the real crates.io `gpui`; use the `gpui-pre` remap.
- Move network work onto the UI thread or make the store block on a request.
- Change the door contract without Warp Station in the same commit.
- Add a second markdown renderer; chat answers use `warpui-kit-markdown`.
- Add a per-app build script or installer; the suite's `scripts/` owns those.
