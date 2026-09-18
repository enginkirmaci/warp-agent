# warp agent — a chat client over `opencode serve`

Standalone Avalonia concept app — **not** part of the DevTools solution, but built from
the DevTools solution file. Runs on Linux and Windows: on Windows the server spawns
through `cmd /c` (PATH shims) and the Wayland bits are compiled out at runtime. It
manages **workspaces** (any folders you add), spawning one `opencode serve` per
workspace on a free port — several can run at the same time — drives a conversational
chat over their HTTP + SSE APIs, and renders opencode permission asks as inline cards
with **Allow / Always allow / Deny**.

## Run

```sh
dotnet run --project concept/WarpAgent
```

Add a workspace with the **+** in the sidebar, select it, press **Start**. Type a
prompt, press Enter (or Send). Shift+Enter inserts a newline. To see a permission
card, ask for something that triggers a tool, e.g. *"Use your bash tool to actually
execute: echo hello. You must really run it."* — bash/edit/webfetch are forced to
"ask" (below).

## Features

- **Workspaces**: the sidebar lists added folders; each keeps its own server instance,
  its own chat list, and its own last-open chat. Starting/stopping is manual and
  per-workspace (button acts on the selected workspace, or via the workspace context
  menu); switching workspaces never stops a running one, so background chats keep
  generating.
- **Shared tile grid**: open chats of different workspaces sit side by side in the
  grid; selecting a workspace never clears it. Clicking a chat opens it from its own
  workspace's server (auto-started if stopped) next to the existing tiles, and each
  tile sends through its own server. Per-workspace model/agent lists merge, so every
  tile keeps its picker entries.
- **Only chats created from this app appear** under a workspace; chats made elsewhere
  (CLI, other clients) in the same folder are never listed. *Remove from workspace*
  (chat context menu) unlinks a chat without deleting it server-side; *Delete* still
  deletes it for real.
- **Parallel turns**: every chat queues/streams on its own. Switch chats (or
  workspaces) while a turn runs; the running-agents panel shows all of them (with a
  per-chat stop button, workspace label, and a `!` badge when a background chat waits
  for a permission answer). Auto-allow answers background asks too; with auto-allow
  off they are parked and shown when you open that chat.
- Chats per workspace: list (pinned first, then newest), switch (transcript is
  restored from the server), rename, pin, New chat. New sessions are auto-titled
  from the first message. Queued messages belong to their chat and follow it across
  switches.
- Restart persistence: workspaces, their chats, each workspace's last-open chat,
  selected model/variant/agent and settings come from
  `~/.config/opencode-agent/ui.json` (`%APPDATA%\opencode-agent\ui.json` on
  Windows). Servers are not auto-started on launch — press Start. A legacy
  single-folder `ui.json` migrates to one workspace on first run.
- Streaming replies with markdown rendering (headings, bold/italic, inline code,
  fenced code blocks, lists, clickable links), selectable assistant text, timestamps.
- Message queueing: typing Enter while a turn runs queues the message (muted bubble
  with a ✕ to unqueue); it is sent automatically when the session goes idle.
- Attachments: paste images from the clipboard (Ctrl+V in the input or the Paste
  button) or drag files onto the transcript/input (images keep their format, other
  files ride along as data URLs up to 512 KB); preview chips with remove, sent as
  `file` parts; images in the transcript render as bubbles and can be copied back out.
- Model picker with a variant combo (per-model `variants` from
  `/config/providers`, e.g. reasoning effort low/high/max); `(default)` omits
  the field.
- Agent picker (build/plan from `GET /agent`, hidden and subagent modes filtered
  out); `(default build)` omits the field.
- Prompts flyout: server commands from `GET /command` run as turns
  (`POST /session/{id}/command`, the current input becomes `$ARGUMENTS`) plus
  client-saved prompts (stored in `prompts.json`, insert into the input).
- Context usage pill: tokens of the last assistant turn (`info.tokens`) against the
  model's context limit; the Compact button summarizes the conversation
  (`POST /session/{id}/summarize`).
- Changes flyout: files the session touched, with +/− counts and expandable colored
  diffs (from edit-tool `filediff` metadata); permission cards for edits embed the
  same diff preview.
- Undo/Redo of the last turn: revert restores the files it changed
  (`POST /session/{id}/revert` to the last user message) and Redo re-applies
  (`/unrevert`); the transcript reloads after both.
- Share: context menu on a chat copies the public `share.url`
  (`POST /session/{id}/share`) or stops sharing (`DELETE`).
- Stop button aborts the open chat's running turn (`POST /session/{id}/abort`); the
  running-agents panel stops any background chat too.
- Auto-allow toggle (header): permission asks are answered automatically with
  always-allow, including asks from background chats; the card stays marked
  *auto-allowed*. The setting persists across restarts.
- Context menu on any message: Copy; user messages also *Edit in input*.
- Smart autoscroll: follows the stream only while you are at the bottom; a
  "jump to latest" button appears when you scroll up.
- Typing indicator while the turn is running; empty-state hints.

## How it works

- Spawn: `opencode serve --hostname 127.0.0.1 --port {free port}` with `WorkingDirectory`
  set to the workspace folder (or attach if that port already answers a healthy server).
  Permissions are forced to *ask* via the `OPENCODE_CONFIG_CONTENT`
  env var (opencode 1.18.31 has no `--config` flag on serve) so the app always
  exercises the approval flow without touching your global config or the folder.
- Chat: sessions come from `GET /session`, transcripts from `GET /session/{id}/message`;
  sending is `POST /session/{id}/message` (fire-and-forget) and the UI is driven purely
  by the `GET /event` SSE stream (`message.part.updated` carries the full text-so-far).
  Images ride along as `{type:"file", mime, filename, url:"data:image/png;base64,…"}`
  parts; the optional `variant` is a top-level body field next to `model`.
- Permissions: `permission.asked` events become cards; a reply is
  `POST /session/{id}/permissions/{permissionId}` with `{"response":"once"|"always"|"reject"}`
  (v2 asks go to `POST /permission/{requestId}/reply`).
- Rename/delete: `PATCH`/`DELETE /session/{id}`; abort: `POST /session/{id}/abort`.
- Close/Stop kills a spawned server (process tree); an attached server is left running.

Verified live against opencode 1.18.31 on Linux: allow, deny, streaming, model list
(`GET /config/providers`, incl. per-model `limit.context`), session
list/restore/rename/delete/abort, agents (`GET /agent`), commands (`GET /command`),
summarize, revert/unrevert, share, tool-part `filediff` metadata, and orphan-free
shutdown. `GET /session/{id}/diff` returns `[]` on this version — the Changes flyout
is built from transcript tool parts instead.

Note: whether a model actually calls a tool (and thus triggers a card) is model
behavior — instruct it explicitly ("really run … via the tool") if it answers in text.
