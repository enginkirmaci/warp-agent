# Warp Agent — a chat client over `opencode serve`

Rust + [GPUI](https://github.com/zed-industries/zed) desktop client (the Avalonia
prototype that preceded it lives in git history). One `opencode serve` per
workspace, a shared tile grid of chats, streaming markdown replies, inline
permission / question cards, and per-chat message queues.

## Run

```sh
cd rust
cargo run            # debug build
cargo run --release  # release build (LTO, stripped)
```

First build fetches GPUI from the zed git repo and takes a while. Needs the
usual Wayland/Vulkan dev packages. State (settings.json, ui.json, logs, the
built-in General workspace) lives in `~/.warp-agent`.

## Desktop entry

```sh
packaging/linux/install.sh
```

Installs `warp-agent.desktop` + hicolor icons so the launcher shows the app;
`Exec` points at `rust/target/debug/warp-agent`.

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

```
rust/            the app
  src/store/     all app logic (single Store entity)
  src/oc.rs      opencode spawn/attach + REST/SSE on a dedicated tokio thread
  src/ui/        GPUI views: window, tile, markdown, diff, input, settings
  src/theme.rs   palette, accent presets, font tokens
  assets/        icons + spark logo (embedded)
packaging/linux/ desktop entry + icon install script
```

See `rust/README.md` for the module map and known gaps. Verified against
opencode 1.18.32.
