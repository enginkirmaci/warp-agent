# Warp Agent

A desktop chat client for coding agents. One `opencode serve` per workspace, a
shared tile grid of chats, streaming markdown replies, inline permission and
question cards, and per-chat message queues. Chats can also run on DeepSeek
Harness, and each chat can run in its own git worktree.

Part of the [warp suite](https://github.com/enginkirmaci/warp-suite). Rust and
GPUI.

## Install

The normal way is the suite bundle, which installs every warp app at once:

- **Linux** — download `warp-suite-<version>-linux.tar.gz` from the
  [suite release](https://github.com/enginkirmaci/warp-suite/releases), extract
  it, and run `./install.sh`. Binaries go to `~/.local/bin`, desktop entries
  and icons to `~/.local/share`; everything is per-user, no root needed.
- **Windows** — download `warp-suite-<version>-windows-x64.zip`, extract it,
  and run `install.cmd`. It installs under
  `%LOCALAPPDATA%\Programs\warp-suite` with a Start Menu folder. The same ZIP
  is portable: run the executables under `apps\` without installing.

Warp Agent also ships on its own: the Linux tarball and the Windows portable
zip and installer are on
[this repository's releases](https://github.com/enginkirmaci/warp-agent/releases).

## Run

```sh
cargo run            # debug build
cargo run --release  # release build (LTO, stripped)
```

The first build fetches GPUI from the zed git repository and takes a while. It
needs the usual Wayland/Vulkan development packages.

Warp Agent needs a harness to talk to: `opencode` on your `PATH`, or `dsh` for
DeepSeek Harness chats. The app works fully standalone; its door is only an
integration surface for the other warp apps.

## What it does

- **Workspaces**: the sidebar lists folders, and each spawns or attaches to its
  own `opencode serve` on a free port — several run at once. A built-in
  **General** workspace leads the list.
- **A shared tile grid**: chats from different workspaces sit side by side, and
  switching workspaces never clears the grid.
- **Streaming replies** through the family's markdown renderer: headings,
  lists, fenced code with highlighting and a copy button, tables, quotes and
  links. Reasoning shows as collapsible *thinking* folds; tool calls as compact
  rows with expandable details and diffs.
- **Queued messages**: Enter while a turn runs queues the message (Steer / Edit
  / Remove), and it is sent when the session goes idle.
- **Permission cards** with Allow / Always / Deny and an auto-allow countdown
  that is quick during working hours and long at lunch and off hours.
- **Worktree chats**: the composer's worktree toggle creates
  `<repo>/.worktrees/<branch>` on the first send and runs the chat there, so
  the main checkout stays where its owner left it.
- **Model, variant and agent pickers** fed by every running server, with a
  context-usage readout in the tile corner.

## Where it keeps its files

| Path | What |
| --- | --- |
| `~/.warp/agent/settings.json` | the versioned settings |
| `~/.warp/agent/ui.json` | the window and layout state |
| `~/.warp/agent/cache/models.json` | the model catalog cache |
| `~/.warp/agent/ipc.port`, `ipc.token` | the loopback door endpoint |
| `~/.warp/logs/` | the suite's shared rolling logs |

The door is a token-authenticated NDJSON server on `127.0.0.1` that the other
warp apps drive: `open-workspace`, `new-chat`, `run-prompt`, `models` and
`status`. Changing its commands, file names or location is a cross-app
contract change.

## Siblings

| App | What it adds |
| --- | --- |
| [warp-station](https://github.com/enginkirmaci/warp-station) | the repository workbench that drives this app's door |
| [warp-flow](https://github.com/enginkirmaci/warp-flow) | the workflow engine, whose steps are chats here |
| [warp-notes](https://github.com/enginkirmaci/warp-notes) | per-repo markdown notes |
| [warp-pomodoro](https://github.com/enginkirmaci/warp-pomodoro) | the focus timer |
| [warp-snapit](https://github.com/enginkirmaci/warp-snapit) | the Windows zone snapper |

## Build and package

```sh
cargo build --release
```

The `release` workflow packages this repository alone: on Linux it stages the
binary, the desktop entry and the hicolor icons into a tarball; on Windows it
writes the portable zip and the Inno Setup installer. The assets stay here
under `packaging/linux/` and `packaging/windows/`.

## License

GPL-3.0. See [LICENSE](LICENSE).
