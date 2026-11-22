# warp-agent — agent instructions

Read [`../../AGENTS.md`](../../AGENTS.md) first. This file adds only the
app-specific facts. The user-facing description is in
[`README.md`](README.md).

Design plans live in `plans/`: a plan is deleted when its work lands, and the
rules that still matter move into `README.md` or this file.

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
- `src/harness/` — the app-level `Harness` trait, the compile-time registry and
  host, and the opencode v1 / dsh adapters. The old `oc.rs`/`dsh.rs` paths still
  run the app until the contract ticket; the adapters translate each protocol
  into the typed `HarnessEvent` vocabulary.
- `src/ipc.rs` — the warp-agent door: token-authenticated NDJSON on the net
  thread's runtime.
- `src/flow.rs` — warp-flow's door client: two NDJSON connections (verbs,
  subscribe) on the net thread's runtime, typed payloads, `absent` as a
  first-class answer and a slow reconnect. No UI.
- `src/ui/` — the window, sidebar, tile grid, settings, the Flow sheet and
  markdown view.
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
- Both message bubbles take the full `MESSAGE_WIDTH` measure (the prompt is
  `w_full`, not shrink-wrapped), and `render_markdown` asks the `LiveEditor`
  for `unbounded_column()` with `margin_x_em(0.0)`: the bubble's own padding
  is the inset, not the editor's 2em side margin plus centered readable
  column.
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
  text. Tool details and system notes stay on the `TextView` guard with the
  Copy menu; an answer, the user message, reasoning and a question's text use
  the read-only `LiveEditor`. The user message's menu appends Edit in input /
  Re-prompt / Fork to Copy / Select all through `LiveEditor::host_menu`, and
  reasoning is fed with `LiveEditorState::apply_streamed` so a chunk keeps the
  selection.
- Read-only text takes no focus, so neither surface's own Copy action
  dispatches: the window root answers Ctrl+C with the window selection first
  (`TextSelection::selected_text`) and then the document's
  (`warpui_kit_markdown::read_only_selection_text`), and leaves the key alone
  while an `Input` context is focused. The text menus print the `Ctrl C` key
  the host answers — `text_menu::attach(…, Some("Ctrl C".into()))` for the
  guard, `LiveEditor::menu_keys(MenuKeys { copy: Some("Ctrl C".into()),
  select_all: None })` for the read-only editors. A row with no real shortcut
  passes `None` and prints no keycap.
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
- Warp Station sends `new-chat` with a worktree's folder to start a chat
  inside that checkout; the door opens it as a workspace like any other.
- `models` returns the harnesses and each model's variants; `refresh: true`
  re-reads the backend instead of answering from the session catalog.
- The app works fully standalone. A missing client changes nothing.
- Changing a command, a field, the endpoint file names or their location is a
  cross-app contract change: update Warp Station in the same commit.

## The Flow surface (warp-flow's door, as its client)

`src/flow.rs` is the app's client of warp-flow's own loopback door
(`~/.warp/flow/ipc.port` + `ipc.token`, protocol 1); the contract is
[warp-suite's cross-app
contracts](https://github.com/enginkirmaci/warp-suite/blob/main/README.md#cross-app-contracts)
and [ADR-0012](https://github.com/enginkirmaci/warp-suite/blob/main/docs/adr/0012-warp-flow-publishes-a-door-of-its-own.md),
and warp-flow owns it. Two connections: one for `runs`, `run`, `answer`,
`pause`, `resume`, `abandon`; one subscribed connection for `subscribe`'s
pushes. `runs` is pull-only, so it is polled every 3 s; a `{"push": "board"}`
is a re-ask, and a `{"push": "run"}` carries the host run whole. The token is
regenerated every launch, so the endpoint files are re-read on every attempt
and an absent door (no files, refused connection, auth refusal) is a normal,
named state — never a panic, never a blocked UI.

`src/store/flow.rs` holds what the net thread delivered (`Store::flow`), and
`src/ui/flow.rs` draws it as a sheet raised from the rail, `Ctrl+Shift+F`,
`WARP_AGENT_FLOW=1` (the capture hook) or the store's `Refresh`: what waits on
a person, and what moves. Answers and run actions are the only writes, both
through the store's command channel; **starting a run has no verb on purpose**
— it stays warp-flow's own action. Do not add a start verb, do not add a tile
kind, and do not build a second board here: ADR-0012's reason for the door is
that both UIs answer one writer.

## Harnesses

- `oc.rs` owns the `opencode serve` processes: one per workspace, a free port,
  a 25 s health wait, `OPENCODE_CONFIG_CONTENT` with ask-forcing permissions,
  REST calls and the SSE event pump with its reconnect ladder.
- `harness/opencode.rs` wraps that client as the `opencode` adapter: it keeps
  the same readiness, SSE pump and reconnect behaviour and emits the typed
  stream. The host keys one instance per workspace (`InstanceScope::Workspace`).
- `dsh.rs` owns `dsh web`: one shared process for every dsh chat. Unary calls
  are `POST /api/<method>` with the `{type:"client-request", ...}` envelope;
  streams ride one WebSocket at `/api/remote.mux`. The launch token is
  exchanged at `/connect` for a `dsh-auth-*` cookie; the cookie dies with its
  WebSocket, so every reconnect re-exchanges it.
- dsh's title projection has two shapes: `session/list` flattens
  `projections.values.title` to a plain string, while a follow snapshot
  carries the object (`{title, messageSeqs, source}`). `session/title` folds
  into `session.updated` with `info.title`, and `Store::adopt_session_title`
  moves the tile header, sidebar row and workspace section together. Reading
  only the object shape from a list made every dsh chat show "New chat".
- `harness/dsh.rs` wraps the dsh client as the `dsh` adapter: one app-wide
  instance (`InstanceScope::App`), the launch-token/cookie flow, the WebSocket
  pump and the same fold, with typed command output. The old `"dsh"` sentinel
  stays the pump's internal net key and never reaches the host or the door.
- All network work is on a dedicated OS thread with its own tokio runtime.
  Never do file IO, HTTP or process work on the GPUI main thread. The UI talks
  to the net thread through async channels; state arrives already parsed.

## Worktrees

A chat can run in a worktree of its repository instead of the main checkout:
the composer toggle is off on every new chat, appears only where the workspace
is a git repository, and the first send creates the checkout under
`<repo>/.worktrees/<slug>`, installs it as a workspace and creates the session
there. The branch is named from the prompt's first line at that send, never
asked for and never shown in the composer; afterwards the chat is bound to that
checkout, and a taken name gets the next free suffix from the kit.

- `warpui-kit-worktree` owns every rule (`create`, `list`, `remove`, `prune`,
  `attach`, branch naming, the local exclude entry). This app never runs git
  itself.
- The worktree's own workspace row stays out of the sidebar: its chats render
  under the repository's row, one branch group each, and a group's `+` starts
  another chat in that checkout. A repository that is not open yet is installed
  when one of its worktrees has a chat.
- A chat whose checkout folder disappeared refuses to send and offers Restore,
  which re-attaches the same branch at the same path — the one place an
  existing branch is attached.
- A worktree in `ui.json` is an ordinary workspace entry, so the settings
  contract did not change.

## Tests

```sh
cargo test -p warp-agent
cargo build -p warp-agent
```

### Build output

`target/` is a **symlink** to the suite's shared build cache
(`~/.cache/warp-suite/target`), so a standalone `cargo build` here and a build
through `warp-suite/apps/<app>` reuse one dependency tree instead of compiling
the gpui graph twice. Two consequences:

- `[profile.dev] debug = "line-tables-only"` in this app's `Cargo.toml` must
  stay identical to warp-suite's root `[profile.dev]`. Cargo keys a crate's
  fingerprint on the resolved profile, so a different setting makes every
  shared dependency compile a second time. Set `debug = 2` temporarily only
  when a session needs gdb/lldb variable inspection.
- `cargo clean` here removes the **symlink**, not the cache: the next build
  then creates a real `target/` on the slow disk. Re-link with
  `ln -s ~/.cache/warp-suite/target target` (or run
  `warp-suite/scripts/dev/link-apps.sh`, which manages these links).

`.vscode/` carries the F5 setup for this checkout: a `cargo build` task and a
gdb (Linux) / cppvsdbg (Windows) launch config that reads the binary from
`target/debug/`.

`.cargo/config.toml` (gitignored, machine-local) points the warpui-kit crates
at the sibling `warp-suite/kits/` checkout with cargo's `paths` override, so a
build here uses the same kit sources as a build through
`warp-suite/apps/<app>` and the two share the compiled kit artifacts. Kit
breakpoints resolve to `kits/`, not to a `~/.cargo/git/checkouts` copy.
`Cargo.toml` and `Cargo.lock` keep the pinned tag, so CI's `--locked` build is
unaffected; delete the file to build against the tag again.

- `tests/complex_markdown_probe.rs` renders a hostile mixed-markdown reply
  through the same read-only `LiveEditor` the tile uses, so a parser or layout
  panic fails `cargo test`.
- `tests/bubble_layout_probe.rs` opens the real window over a seeded user and
  assistant message and proves both bubbles share one measure and their text
  fills the bubble's padded box (no editor side margin or centered column).
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
- `tests/text_surfaces_probe.rs` selects the reasoning surface, streams a
  chunk and proves the selection and Copy survive, checks the tool details and
  system note Copy menus and the user message's host menu (Edit in input /
  Re-prompt / Fork), and proves a tool title opens no menu.
- `tests/read_only_copy_probe.rs` proves the read-only menus print the
  `menu-key-Ctrl C` keycap (the reasoning `LiveEditor` and the guarded tool
  details), that Ctrl+C copies the window selection and the document
  selection, and that a focused sidebar filter keeps its own Ctrl+C instead of
  copying the read-only text.
- `tests/title_probe.rs` proves the OS title is `<n> running — Warp Agent`
  while a chat runs and the plain app name otherwise.
- `tests/question_text_probe.rs` proves a selection in a question's text
  survives typing in the custom-answer field, the field keeps focus and the
  selection's Copy menu still works.
- `tests/shortcuts_probe.rs` opens the real window's keyboard sheet through
  `WARP_AGENT_SHORTCUTS=1`, `?`, Ctrl+/, the titlebar icon and the scrim, and
  proves the card takes focus, Escape closes it first, every close path
  returns the keys to the window root, and a focused field keeps `?` as text.
  The sheet's rows are pinned by unit tests in `src/shortcuts.rs`.
- `tests/pane_moves_probe.rs` drives the sidebar and three seeded transcripts
  through `warpui_kit_core::pane_nav` and proves the grid order, the row ends
  (no wrapping), a hidden sidebar not being a stop, transcript scrolling
  surviving the shared keys, and the focused composer keeping its own keys.
- `tests/flow_client_probe.rs` drives the real Flow client against the fake
  warp-flow door in `tests/flow_support/` (std threads speaking the published
  contract): absent endpoint, auth refusal, `runs`/`run`, `answer` outcomes,
  action refusals, a `run` push, a `board` notification's re-ask, and a
  reconnect after the door closes and a new launch publishes new files.
- `tests/flow_sheet_probe.rs` opens the real window with the Flow sheet up
  (`WARP_AGENT_FLOW=1`) over that fake door: the waiting card renders the
  question and its options, the list's cursor walks the rows, a click sends
  `answer` and the door's outcome shows, the run leaves the band when the next
  poll says it no longer waits, the moving card offers only the verb its phase
  allows, Escape closes the card, and an absent door is one line whose Retry
  reconnects without a restart.
- The capture hooks are `WARP_AGENT_PAGE`: `settings`, `settings-chat`,
  `settings-permissions`, `settings-server`; `WARP_AGENT_SHORTCUTS=1` opens the
  keyboard sheet on launch; and `WARP_AGENT_FLOW=1` raises the Flow sheet.

## Platform notes

- The packaged Windows exe uses `windows_subsystem = "windows"`; debug builds
  keep the console.
- Managed `serve`/`dsh` children are spawned from transient blocking workers,
  so Linux `PR_SET_PDEATHSIG` must not be used: it fires when the *parent
  thread* exits and killed every child ~20 s after launch. The app owns the
  child through `kill_on_drop`/`StopServer`, and `oc::reap_stale_children()`
  sweeps exact managed shapes orphaned by a SIGKILL/SIGTERM on startup. If you
  add a long-lived child, give it the same normal-quit cleanup and add its
  argv shape to that sweep.
- `composer` key overrides are bound after `gpui_kit::init` (inside the
  bootstrap) with `Some("Input")` and `Some("Composer > Input")` contexts. Do
  not make them global.

## Do not

- Add the real crates.io `gpui`; use the `gpui-pre` remap.
- Move network work onto the UI thread or make the store block on a request.
- Change the door contract without Warp Station in the same commit.
- Add a second markdown renderer; chat answers use `warpui-kit-markdown`.
- Add a per-app build script or installer; the suite's `scripts/` owns those.

## Public history and secrets

- Never commit a token, PAT, password, private IP, Wi-Fi SSID, home path or
  personal email in file content. Use placeholders in docs and examples.
- Pass credentials through GitHub Actions secrets only; never echo or log
  them.
- Before publishing, force-pushing or adding a remote, run this over the
  current tree and every ref:

  ```sh
  git grep -n -i -E '192\.168\.[0-9]+\.[0-9]+|10\.[0-9]+\.[0-9]+\.[0-9]+|/home/[a-z]+|ghp_|gho_|github_pat_|ghs_' -- .
  ```
