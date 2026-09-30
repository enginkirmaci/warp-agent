# WarpAgent — Rust + GPUI

A full port of the Avalonia WarpAgent client to Rust and [GPUI](https://github.com/zed-industries/zed) (Zed's UI framework). Same feature surface, same `~/.warp-agent` state files (settings.json / ui.json are read and written interchangeably with the C# build), same opencode REST + SSE surface (verified against opencode 1.18.31).

## Run

```sh
cd rust
cargo run --release
```

First build fetches GPUI from the zed git repo and compiles for a few minutes. Needs the usual Wayland/Vulkan dev packages (already present on omarchy).

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

## Known gaps vs the C# build

- Hover tooltips are not rendered (the ShowToolTips setting is stored; balloons are a follow-up).
- The composer input has no text selection yet (no select-all/copy inside the field; message copy via right-click menus works).
- IME composition is not wired (dead-key/CJK input in the composer falls back to plain key events).
- Transcript is not virtualized (the C# one wasn't either).
