# Warp Agent — a chat client over `opencode serve`

Rust + [GPUI](https://github.com/zed-industries/zed) desktop client (the Avalonia
prototype that preceded it lives in git history). One `opencode serve` per
workspace, a shared tile grid of chats, streaming markdown replies, inline
permission / question cards, and per-chat message queues.

## Run

```sh
cargo run            # debug build
cargo run --release  # release build (LTO, stripped)
```

First build fetches GPUI from the zed git repo and takes a while. Needs the
usual Wayland/Vulkan dev packages. State (settings.json, ui.json, logs, the
built-in General workspace) lives in `~/.warp-agent`.

## Package

- Linux: `packaging/linux/install.sh` installs the desktop entry + hicolor
  icons so the launcher shows the app; `Exec` points at
  `target/debug/warp-agent`.
- Windows (build on a Windows box — GPUI has no Linux→Windows cross build):
  `packaging/windows/build.ps1` produces a portable zip under
  `packaging/windows/out`; `-Installer` also compiles the Inno Setup setup.exe.
  The app icon is embedded into the exe by `build.rs` from
  `packaging/windows/warp-agent.ico`.

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
  settings page (accent presets, text size, auto-allow, executable) persisted to
  `~/.warp-agent/settings.json`.
- Stop aborts the running turn (*stopped in Xs*); *done in Xs* closes a normal
  one.

## Layout

| Module | Ported from | Notes |
|---|---|---|
| `src/store/mod.rs` | MainViewModel + ChatTileViewModel + ChatItems | All app logic: workspaces (one `opencode serve` each), the shared tile grid, transcripts, queue w/ Steer/Edit/Remove, permission asks + auto-allow countdowns (lunch/off-hours ranges), `question` tool cards, subagent tracking, prompt history, restore banner |
| `src/oc.rs` | OpenCodeServer + OpenCodeApiClient | Serve spawn/attach, OPENCODE_CONFIG_CONTENT ask-forcing, free-port pick, 25 s health wait; REST + SSE on a dedicated tokio thread talking to the UI via channels |
| `src/theme.rs` | App.xaml resources | The whole palette, accent presets (green/amber/salmon/4 grays), font/icon size tokens with compact/normal/large × system factor |
| `src/ui/markdown.rs` | MarkdownViewerKit.Avalonia **(replaced)** | Own renderer: headings, bold/italic, inline code, fenced code with syntax highlighting + copy button, lists, links (clickable), tables, quotes, rules |
| `src/ui/highlight.rs` | MarkdownTextBlock highlighter | Keyword/string/comment/number passes per language family |
| `src/ui/diff.rs` | DiffTextBlock | GitHub-style unified diff: line-number gutter, add/del tints, hunk separators |
| `src/ui/input.rs` | — (gpui has no editor) | Multi-line composer input: wrapping, caret, click-to-position, Up/Down history hooks, Enter/Shift+Enter, Ctrl+V image paste hook |
| `src/ui/tile.rs` | ChatTile.axaml(+cs) | Transcript item cards (user/assistant/reasoning/tool/image/permission/question/note), tasks+agents flyout, model/variant/agent pickers, queue rows, attachments |
| `src/ui/window.rs` | MainWindow.axaml(+cs) | Titlebar (drag/zoom/close/toggle), workspace + chat tree with context menus, drag-resizable sidebar, settings page, restore banner, error overlay |
| `src/services.rs` | TerminalLauncher + SystemFontScale | `Open in opencode` terminal probe (alacritty→…), gsettings text-scale follower |
| `src/settings.rs`, `src/paths.rs`, `src/logging.rs` | AppSettings/UiState/AppPaths/AppLogging | Same files, same defaults, legacy migration |
| `assets/` | — | Icons + spark logo (embedded) |

Verified against opencode 1.18.32.

## Known gaps

- Hover tooltips are not rendered (the ShowToolTips setting is stored; balloons are a follow-up).
- The transcript is not virtualized (the C# one wasn't either).
- The composer input has no text selection yet (no select-all/copy inside the field; message copy via right-click menus works).
- IME composition is not wired (dead-key/CJK input in the composer falls back to plain key events).
