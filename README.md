# Warp Agent — a chat client over `opencode serve` and `dsh web`

Rust + [GPUI](https://github.com/zed-industries/zed) desktop client (the Avalonia
prototype that preceded it lives in git history). One `opencode serve` per
workspace, a shared tile grid of chats, streaming markdown replies, inline
permission / question cards, and per-chat message queues. Chats can also run
on DeepSeek Harness: the composer's harness pill picks `dsh` per chat, and one
shared `dsh web` process serves every dsh chat (dsh sessions carry their own
working directory).

## Run

```sh
cargo run            # debug build
cargo run --release  # release build (LTO, stripped)
```

First build fetches GPUI from the zed git repo and takes a while. Needs the
usual Wayland/Vulkan dev packages. State (versioned `settings.json` and
`ui.json`, `cache/models.json`, the built-in General workspace and the door
endpoint) lives in `~/.warp/agent`; the rolling logs go to the suite's shared
`~/.warp/logs`.

## DevTools door

At startup the app binds a token-authenticated NDJSON server on
`127.0.0.1` (port + token under `~/.warp/agent/ipc.*`). DevTools drives it:
`open-workspace` (folder args on the command line do the same, forwarded to
a running instance — second launches exit), `new-chat`, `run-prompt`
(one-shot wand runs as real chats, answered at idle; both accept a
`harness`, `model` and `variant`), `models` (the flat key list plus the
harnesses and per-model variants the settings pickers need), `status`
(+ status pushes). The app works fully standalone; the door is only an
integration surface.

## Package

Packaging is driven by the suite, not from this directory:

- Linux: `scripts/build.sh --app warp-agent` stages the binary, the desktop
  entry and hicolor icons; `scripts/deploy.sh --local` installs them.
- Windows (build on a Windows box — GPUI has no Linux→Windows cross build):
  `scripts/build.ps1 -App warp-agent` writes the portable zip and the Inno
  Setup installer. The app icon is embedded into the exe by `build.rs` from
  `packaging/windows/warp-agent.ico`.

This directory only carries the assets: `packaging/linux/{*.desktop,icons}`,
`packaging/windows/{*.ico,*.iss}`.

## Features

- **Workspaces**: the sidebar lists folders; each spawns (or attaches to) its own
  `opencode serve` on a free port — several run at once. Sections start collapsed
  every launch; expanding one auto-connects. A built-in **General** workspace
  leads the list.
- **Shared tile grid**: open chats of different workspaces sit side by side;
  switching workspaces never clears the grid. Opening a chat loads its transcript
  from its own server (auto-started if stopped). Restarts offer a restore banner
  for the chats that were open.
- **Chat list per workspace**: pinned first, then newest; rename, pin, delete,
  remove-from-workspace. Only chats created from the app are listed (client-side
  tracking — the serve bus is shared).
- **Streaming replies** with the own markdown renderer: headings, lists, inline
  code, fenced code with highlighting + copy button, tables, quotes, links.
  Reasoning shows as collapsible *thinking* folds; tool calls as compact rows
  with status and expandable details/diffs.
- **Queued messages**: Enter while a turn runs queues the message (Steer /
  Edit / Remove); it is sent automatically when the session goes idle.
- **Permission cards** with Allow / Always / Deny and an auto-allow countdown
  (quick during working hours, long at lunch/off hours; *allow immediately*
  skips the wait). `question` tool asks render as answer cards with options and
  a custom answer field.
- **Model / variant / agent pickers** fed by every running server; per-tile
  selection; context-usage readout in the tile corner (last-turn tokens vs the
  model's context limit).
- **Prompt history** (Up/Down in the composer), clipboard image paste, and a
  Settings page with the family's four sections (Appearance, Chat defaults,
  Permissions, Server), the shared text size and reduce motion, persisted to
  `~/.warp/agent/settings.json`.
- Stop aborts the running turn (*stopped in Xs*); *done in Xs* closes a normal
  one.

## Layout

| Module | Ported from | Notes |
|---|---|---|
| `src/store/mod.rs` | MainViewModel + ChatTileViewModel + ChatItems | All app logic: workspaces (one `opencode serve` each), the shared tile grid, transcripts, queue w/ Steer/Edit/Remove, permission asks + auto-allow countdowns (lunch/off-hours ranges), `question` tool cards, subagent tracking, prompt history, restore banner |
| `src/oc.rs` | OpenCodeServer + OpenCodeApiClient | Serve spawn/attach, OPENCODE_CONFIG_CONTENT ask-forcing, free-port pick, 25 s health wait; REST + SSE on a dedicated tokio thread talking to the UI via channels |
| `src/theme.rs` | App.xaml resources | Re-exports warpui-kit-core's Copper palette and type ladder; the app keeps no colours or font sizes of its own |
| `src/ui/markdown.rs` | MarkdownViewerKit.Avalonia **(replaced)** | gpui-kit TextView markdown: tree-sitter syntax highlighting for fenced code (12 languages, per-block copy button), selectable text, clickable links |
| `src/ui/diff.rs` | DiffTextBlock | GitHub-style unified diff: line-number gutter, add/del tints, hunk separators |
| gpui-kit `Textarea` | — (was src/ui/input.rs, deleted) | Multi-line composer input: wrapping, auto-grow, Enter/Shift+Enter submit, Up/Down prompt history, paste-image hook |
| `src/ui/tile.rs` | ChatTile.axaml(+cs) | Transcript item cards (user/assistant/reasoning/tool/image/permission/question/note), tasks+agents flyout, model/variant/agent pickers, queue rows, attachments |
| `src/ui/window.rs` | MainWindow.axaml(+cs) | Titlebar (drag/zoom/close/toggle), workspace + chat tree with context menus, drag-resizable sidebar, settings page, restore banner |
| `src/services.rs` | TerminalLauncher + SystemFontScale | `Open in opencode` terminal probe (alacritty→…), gsettings text-scale follower |
| `src/settings.rs`, `src/paths.rs`, `src/logging.rs` | AppSettings/UiState/AppPaths/AppLogging | Versioned snake_case settings/UI state, one-time legacy import; `~/.warp-agent` is adopted on first run |
| `assets/` | — | Icons + spark logo (embedded) |

Verified against opencode 1.18.32.

## Known gaps

- Tooltips render on hover (built on gpui_base's overlay); the "Show tooltips on hover" setting gates them.
- The transcript is not virtualized (the C# one wasn't either).
- IME composition is not wired (dead-key/CJK input in the composer falls back to plain key events).
