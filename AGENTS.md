# warp-agent — agent instructions

Read [`../../AGENTS.md`](../../AGENTS.md) first. This file adds only the
app-specific facts. The user-facing description is in
[`README.md`](README.md).

## Shape

warp-agent is a library (`warp_agent`) plus a thin binary. `src/lib.rs` owns
the module list and `run()`; `src/main.rs` only carries the Windows console
attribute and calls `warp_agent::run()`. The boundary exists so probes can
open the real `WarpWindow`: `tests/sidebar_keys_probe.rs` drives its sidebar
with a scratch store, and `tests/transcript_keys_probe.rs` focuses the real
transcript and scrolls it. The kit-level probes still cover the shared pieces:
`tests/complex_markdown_probe.rs` runs a hostile reply through the read-only
`LiveEditor`, `tests/transcript_list_probe.rs` drives the transcript's
`list`/`ListState` wiring, and `tests/selection_probe.rs` drags a selection
through a bold run.

## Where things live

- `src/lib.rs` — startup order (`run()`), module list and the composer key
  bindings.
- `src/main.rs` — the Windows console attribute and the `run()` call.
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

## Transcript

The chat transcript is a virtualized GPUI `list` (`ListState`, top-aligned
with `FollowMode::Tail`), not a flex column: only the rows its viewport plus
overdraw covers are built and laid out, so a long chat costs a screenful per
frame instead of every message's markdown editor. Do not go back to rendering
the whole transcript per repaint. Top alignment (not `ListAlignment::Bottom`)
keeps a chat shorter than the tile at the top, the way the flex column looked;
the tail follow still opens a longer chat at its newest message.

- A row is `Row::Item { index, id }` or a folded `Row::ToolGroup` of two or
  more consecutive tool calls. Rows must **not** hold `Arc<ChatItem>`: a row
  lives across frames, and a held Arc turns the store's `Arc::make_mut` on
  every streamed text snapshot into a full copy. `render_row` reads the item
  from the store by index and checks the id.
- `TileView::sync_rows` splices only the range the rows moved; an append keeps
  every measured height above it. `rows_match` compares ids without
  allocating, so a scroll repaint reuses the rows instead of rebuilding them.
- The list measures each visible row per layout; a text-size change
  invalidates all cached heights (`last_font_body` → `remeasure`).
- A message's editor is an entity, not a view: its `notify` never invalidates
  the cached tile. `md_editor` observes each editor and repaints the tile when
  its **selection** or content height moves. Without the selection half, a
  drag's highlight only appeared when some store event happened to repaint the
  tile — a streaming chat looked live at random, an idle one did not. Do not
  narrow that observer back to the height alone.
- Follow state: send and the jump-to-latest button re-arm
  `FollowMode::Tail`; opening a fold pauses it (`pause_following_tail`).
  `TileView::following` mirrors the list for the button, refreshed from the
  list's scroll handler.
- A tool's change reaches the tile through `store::parsing`: opencode ships
  `metadata.filediff.patch` and dsh ships `meta.diffs` (before/after text per
  hunk). `patch_from_hunks` turns dsh's pairs into the same patch text, so one
  kit diff draws both harnesses. Do not add a second diff renderer for dsh.
- The sidebar's flattened rows own a cursor separate from the open chat:
  arrows move it, `Enter` runs the row's click. The transcript takes focus on
  click and its arrows scroll it. A tool title is a label and stays plain
  text; its details, a reasoning block, a question and a note stay markdown
  with the Copy menu.
- The OS title mirrors the titlebar chip through
  `warpui_kit_core::activity::activity_title`: `2 running — Warp Agent`, or
  the plain app name when idle.

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
- `tests/transcript_list_probe.rs` proves the transcript list builds only a
  viewport of rows (not the whole chat), opens at the newest message and stays
  bounded when scrolled to the top.
- `tests/selection_probe.rs` proves a drag over a styled run selects before
  the button comes up and repaints the cached host, and that bold and plain
  text are selected alike.
- `tests/sidebar_keys_probe.rs` opens the real `WarpWindow` over a scratch
  HOME, focuses the sidebar, walks its flattened rows with the arrow keys and
  proves a focused filter field keeps them and Enter focuses the chat's tile.
  It uses `Store::for_probe`, whose net channel has no receiver, plus
  `Store::seed_probe_tile` to seed a tile and its session mapping.
- `tests/transcript_keys_probe.rs` clicks the transcript in the real window
  and proves Down/Up/PageDown/Home/End move its `ListState` offset while the
  sidebar cursor stays put.
- `tests/text_surfaces_probe.rs` drag-selects and right-clicks the reasoning
  and tool-details surfaces through their Copy menu, checks the user message's
  item menu, and proves a tool title opens no menu.
- `tests/title_probe.rs` proves the OS title is `<n> running — Warp Agent`
  while a chat runs and the plain app name otherwise.
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
