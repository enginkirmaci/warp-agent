# Subagent status in warp-agent

**Status: proposal, refreshed against `dc03b1b` (the running panel landed
after the first draft). Decision 3 is settled (B); four decisions still open.**

**Landed:** the depth routing fix. `effective_workspace_path` now walks the
whole subagent lineage through a cycle-capped helper (`lineage_workspace_path`)
instead of one parent hop, so a depth ≥ 2 opencode chat finds its root
workspace and the reply key for its approvals; `session_harness` inherits the
owner's harness through the same walk, so an unmarked dsh subagent's reply
reaches the dsh server instead of opencode; `owner_session_id` shares the depth
cap. Unit tests live in `mod lineage_tests` (`src/store/mod.rs`). Everything
else below is unbuilt.

Reported: warp-agent does not show the status of a subagent *inside* a
subagent. Follow-up: can a nested subagent's approvals be answered? The answer
today is *partly* (see the approvals row below and slice 5). Both questions
come from the same flat, one-level run model, so this plan owns them together.

## Open decisions

1. **Which reading is the bug?** The report fits three cases. This plan fixes
   A and B together because they share the same root cause; C is follow-up.
   - A. Opening a subagent's chat as a tile shows nothing running: it has no
     `Running` chip, no spinner in the sidebar/tile, and is not counted by the
     activity chip or the OS title (`is_busy` only reads `running_by_session`,
     and subagent sessions are never put there).
   - B. A chat's `Tasks & agents` flyout lists only runs whose *direct* parent
     is that chat, so a grandchild run never appears anywhere; and the list is
     not rebuilt when a tile opens, so an already-running child can be missing
     until some event flips.
   - C. Runs that already finished are not kept at all (rows disappear on
     idle), so there is no done/failed status to read afterwards.

   **Recommended:** A + B now, C as a later slice; say if C is the actual
   complaint and the retention design moves into slice 3.

2. **Flyout scope.** Show the chat's **whole live subtree** (pre-order,
   indented) rather than direct children only. A subagent chat's own tile then
   shows its children, and the root chat shows the whole run.
   **Recommended: whole subtree.** The alternative (direct children + a “via
   <parent>” line) keeps the list short but still hides depth ≥ 2 from the
   root chat.

3. **Approval home — decided: B.** The card always lives in the **root chat**
   of the asking session's lineage; a subagent's own chat shows *Needs you* on
   its run row but never renders the card. One place for every approval and no
   duplicated controls; the cost is that answering a nested run means going to
   its root chat, which is accepted. (Rejected: A, the nearest open chat on
   the lineage; C, mirrored cards with one answer resolving the copies.)

4. **Running panel and activity chip.** `dc03b1b` added the panel under the
   title-bar chip; it lists live chats from `running_by_session` and the chip
   and OS title read the same list. **Recommended:** the panel shows each live
   chat followed by its live runs, indented (the same tree as the flyout); a
   chat counts as live when it **or any run beneath it** is working, so the
   chip and OS title keep counting **chats**, never runs. Alternative: keep
   the panel chats-only and add a per-chat “N agents” line.

5. **Subagent sessions in the sidebar.** dsh's `session/list` returns
   subagent-origin rows with the parent's `cwd`, so today they can enter
   `ws.state.sessions` and look like ordinary chats. **Recommended:** hide
   `origin: subagent` rows from the sidebar and reach them only through the
   runs tree. Alternative: keep them and mark them as subagent chats.

## What the app does today, and where it leaks

All of this is in this repository at the `dc03b1b` base revision. The landed
fixes above shift some line numbers; re-read a cited line before cutting a
patch.

| Concern | Today | Leak |
|---|---|---|
| Child → parent map | `subagent_by_session` filled from lists and opencode `session.updated` (`src/store/runs.rs:153`, `src/store/sessions.rs:22`) | A dsh `session/title` fold emits `session.updated` with no parent (`src/dsh.rs:1196`), and the store reads “no parent” as “not a subagent”, so any title update wipes the mapping and the row (`src/store/runs.rs:158`). |
| Live discovery | dsh `$events` folds only `api-session/status` and `api-session/error` (`src/dsh.rs:1594`) | `api-session/added` (full `SessionSummary`: `sessionId`, `parentSessionId`, `origin`, `running`) and `api-session/removed` are dropped. A child created and finished inside one parent turn is never seen. |
| List rows | `SessionInfo` carries id/title/timestamps/parent only (`src/oc.rs:48`); dsh's `list_sessions_filtered` reads `parentSessionId` only (`src/dsh.rs:798`) | The list's `running` and `origin` are ignored, so a running subagent discovered by a list gets a mapping but no row until its next status flip. Every list also re-stamps the label to the literal `subagent`, overwriting the agent name from live events (`src/store/runs.rs:163`). |
| Running set | `set_running` refuses anything that is not an app session and never marks subagents (`src/store/runs.rs:90`) | `ChatTile::is_busy` (`src/store/tile.rs:186`), the sidebar's Running scope and the running panel all miss subagent sessions, so an opened subagent chat looks idle while it streams. |
| Chat projections | `running_jobs`/`activity` read `running_by_session` (`src/store/mod.rs:1501`, `:1530`) | There is no run-level projection at all: the panel and chip cannot show a nested run, and once slice 2 puts subagents in the running set they would be counted as chats unless filtered. |
| Tile flyout | `refresh_tile_agents` filters on the direct parent only (`src/store/runs.rs:251`); called from status changes, never from tile open | Depth ≥ 2 is invisible; a running child is missing until the next event. |
| Opening / stopping / attention | `effective_workspace_path` climbs one level (`src/store/mod.rs:1602`); `open_subagent_session` (`src/store/runs.rs:296`), `mark_attention` (`src/store/runs.rs:72`), `drop_subagent_rows` (`src/store/runs.rs:262`) are all one-level; dsh's Abort is a plain `session/cancel` (`src/dsh.rs:664`) | A depth ≥ 2 row can render but clicking it opens nothing. An ask whose own session has no row falls back only one level, and a workspace removal clears one level. Stop may be rejected on a child session: dsh's own client cancels a subagent through `subagents/interruptByUser(childSessionId)`, not `session/cancel`. |
| Approvals and questions | Cards are parked in the **root** chat (`on_permission_asked` `src/store/permissions.rs:4`, owner at `:89`; `on_question_asked` `:330`, owner at `:339`); the card keeps the **asking** session and the answer routes by it (`respond_permission` `:224`, `submit_question` `:420`, `dismiss_question` `:469`); closing a tile just drops it (`src/store/chat_flow.rs:181`) | dsh answers worked at any depth once the session was known as dsh (the shared server key, `src/store/mod.rs:1625`, and `$events/result` is session-free). Opencode depth ≥ 2 could not answer: `effective_workspace_path` climbed one level, so the card resolved to “server unavailable” (fixed above). A freshly created dsh subagent had no harness mark yet, and `session_harness` defaulted to opencode (`src/store/mod.rs:1616`), so its first ask could be replied to the wrong server (fixed above). A subagent's own chat never shows its own card, and a pending card in a closed chat is lost (dsh has no pull to restore it); both still open. |

dsh facts this plan relies on (checked against the installed 0.2.2-rc.2):

- `SessionSummary` = `{sessionId, updatedAt, running, blank, parentSessionId?,
  origin?: 'subagent', cwd?, projections?}`. Child sessions are minted with a
  bare UUID id; the parent reference is the parent's `session-<uuid>` id.
  **Treat both as opaque strings, never normalize a prefix.**
- `api-session/added(summary)` is an emit on `$events`; `api-session/status`
  is already mapped.
- `session/list` includes subagent-origin rows (the web client indents them),
  and its `projections.subagentCatalog` names a parent's children with labels
  if the list rows ever prove insufficient.
- A subagent activation's owner-only controls,
  `subagents/interruptByUser(childSessionId)` and the prompt/continuation
  methods, would be needed for the full surface. Approvals stay on
  `$events/result`, which needs no session and therefore works at any depth.

## Intended shape

One run record per tracked session, three projections, and one home per ask:

- **State:** `AgentRun { session_id, parent: Option<String>, label, title,
  order, status }`, plus `running_by_session` as the single authority for
  “this session's agent is working”. `order` reuses `running_order`
  (`src/store/mod.rs:1540`) — a subagent run calls `begin_running` when it
  starts, so the tree can sort by start without a second clock. `status` is
  derived: running + a pending ask for it → *Needs you*; running → *Running*;
  otherwise *Idle/Done*.
- **Projection 1 — flyout:** `tile_agents` becomes the chat's whole live
  subtree in pre-order (depth, parent title, status). Built from the parent
  map, never filtered by `== direct parent`, and rebuilt when a tile opens,
  closes or is attached.
- **Projection 2 — running panel:** each live chat followed by its live runs,
  indented, from the same tree (decision 4). The chip and OS title keep
  counting chats.
- **Projection 3 — chats:** the sidebar keeps root chats only; a subagent
  session never becomes a `SessionItem` (decision 5).
- **Asks (decision 3, B):** exactly one card per ask, in the **root chat** of
  the asking session's lineage — the routing the store already uses. A
  subagent's own chat shows its run status (*Needs you*) but renders no card;
  the root chat is where every approval is answered. The card keeps the asking
  session, so the answer goes back through the asking run, and closing the
  root chat with a still-pending card re-parks it instead of dropping it.

Status shows wherever a chat is shown, following the design-system motion
rule: a word everywhere, a spinner only beside it, an accessible name and a
tooltip on the animated element, and the same word under reduced motion.

- Flyout row: indent by depth, status word (Running / Needs you), agent label
  and title, stop button, `aria_label` naming the agent, its status and its
  parent.
- Opened subagent chat: its tile head shows the existing `Running` chip, its
  flyout shows *its* children, and its run row in the root chat's flyout says
  *Needs you* while it waits; the card itself stays in the root chat, and the
  sidebar does not add it as a chat.
- Root chat: the flyout shows the whole run; the summary count is the number
  of live rows; an ask that settled here is answered here and nowhere else.
- Clicking any row opens that session's chat (climbing to the owner workspace
  when the child has no direct workspace entry); Stop aborts that run.

## Slices

Each slice is independently shippable and testable; slices 1 and 2 already fix
A and most of B. Slice 5 is the approval answer.

### 1. Wire model: keep parent, running and origin

- `SessionInfo` gains `running: bool` and `origin: Option<SubagentOrigin>` (or
  `parent_id.is_some()` + origin flag); both harnesses fill them —
  opencode from `parentID` (running comes from events today, so `false` there
  is acceptable if the list does not say), dsh from `parentSessionId`,
  `origin`, `running`.
- `dsh::fold_emit` maps `api-session/added` to an opencode-shaped
  `session.created`/`session.updated` carrying `info.parentID`, `info.title`
  (when present) and the row's `running`; maps `api-session/removed` to a
  “session gone” event that clears rows. dsh's title-only `session.updated`
  must not be able to clear a parent: distinguish *parent unknown*
  (field absent, e.g. a title event) from *explicitly no parent* (a list or
  `added` row). Additive updates only.
- `apply_session_list` seeds the run from the list (`running`, and a row whose
  run is already live) instead of waiting for a flip, and no longer overwrites
  a known agent label with `subagent`.
- Keep `refresh_dsh_lists` as is; with `api-session/added` the tree should no
  longer depend on list timing.

Tests: store unit tests fed synthetic dsh folds — a child adopted from an
`added` frame before any list, a title event that must not drop the parent, a
list that seeds a running child, a nested pair (`S → T`).

### 2. One running set, chat projections stay sane

- Let `set_running` mark any known session (app session or tracked subagent)
  and call `begin_running` for it, so `running_order` covers runs too.
- Keep *row* creation in the subagent path; the running set no longer decides
  whether something is a chat.
- `is_busy`, the tile head and the sidebar spinner then work for an opened
  subagent chat with no extra path.
- `running_jobs`, `running_chats` and `activity` must stay chat-shaped: a chat
  is live when it or any run beneath it is running, and a subagent is never a
  chat row of its own. `running_jobs` / `activity` then feed the panel and the
  title without inflating either.
- `update_sidebar_running` no-ops for sessions that are not sidebar chats.
- `session.idle` still ends the run and drains queues. The idle path may keep
  refreshing the chat list, but discovery must no longer depend on it.

Tests: a probe asserting the tile head shows `Running` for a running subagent
session, the OS title stays at the chat count while a hidden subagent runs,
and the running panel lists the chat once.

### 3. Tree instead of a flat list

- Compute depth and descendants from `subagent_by_session`; break cycles
  defensively (a session naming itself or a loop must not hang).
- `refresh_tile_agents` builds each tile's rows from the subtree of the tile's
  session, sorted pre-order (depth, then `running_order`), and is also called
  when a tile is opened, closed and attached to a session.
- Rows carry `depth`, `parent_title`, `is_subagent` (keep), `status`.
- Cap the visible tree for very wide runs (e.g. first N rows + “+k more”) so a
  big fan-out does not turn the flyout into a scroll wall; the cap is a
  constant beside the flyout.

Tests: pure store test over `S → {T1, T2}`, `T1 → U` asserting pre-order,
depth, titles and that a parent's own row is not duplicated in its child list.

### 4. Any depth is reachable and stoppable

- `effective_workspace_path` climbs the whole lineage — **landed** as
  `lineage_workspace_path` with a depth cap, tests in `mod lineage_tests`.
  This is also what makes an opencode depth ≥ 2 approval answerable.
- `session_harness` inherits the owner's harness when the session has no entry
  of its own — **landed** as `lineage_harness`, tests in `mod lineage_tests`.
  A dsh subagent created mid-turn may ask before any list marks it; without
  this it was treated as opencode, so its chat opened on the wrong server and
  its approval was replied to the wrong one.
- `open_subagent_session` opens any tracked depth; a depth ≥ 2 chat loads
  through the owner's server key (dsh: the shared key, so no workspace entry is
  needed; opencode: the owner's folder).
- `mark_attention` and `drop_subagent_rows` use the owner/descendant walk, so
  an ask from depth 2 lights the run that owns it and a removed workspace
  clears every descendant.
- Stop routes by target: a tracked subagent on dsh goes through
  `subagents/interruptByUser(childSessionId)` (owner-only, which this app is),
  with `session/cancel` as the root-chat path; keep one `ApiRequest::Abort`
  and let `dsh.rs` pick the method from the parent map it already mirrors.
  Opencode keeps its own abort.

Tests: a store test opening a depth-2 session (asserts the request went to the
right net key), one marking attention from a depth-2 ask, and a dsh
`run_request` test proving a subagent abort sends `subagents/interruptByUser`
while a root chat still sends `session/cancel`.

### 5. Approvals and questions at any depth (decision B)

- **Display target:** keep the `owner_session_id` routing in
  `on_permission_asked` and `on_question_asked` — every card lands in the root
  chat's tile, parked there when it is closed. Do not move a card into the
  subagent's chat; its own tile never renders permission or question items.
- **Re-park on close:** `detach_tile` returns still-pending permission and
  question items to the parked maps under their owner (dedupe by id) before
  dropping the tile. Today closing the root chat silently loses the card and
  the run stays blocked.
- **Answer routing:** unchanged in shape (by asking session); the landed
  lineage walk makes opencode depth ≥ 2 answerable, and the landed harness
  inheritance sends an unmarked dsh subagent's reply to the dsh key.
- **Attention:** `mark_attention` uses the asking session, so the run's row
  shows *Needs you* wherever it is listed; `tiles_waiting` still lights the
  rail dot for an ask parked with no chat open. Opening a chat must not clear
  the flag — only answering or re-parking does.
- **Questions pulled from opencode:** keep filtering a pulled batch by the
  owner, and do not re-add an ask that the root tile already displays (id
  check across tiles).

Tests: (a) a depth-2 permission on dsh lands in the root tile and never in the
subagent's; (b) answering it sends one reply with the asking session; (c)
opencode depth-2 permission and question replies reach the owner's workspace
key, and an unmarked dsh subagent's reply reaches the shared dsh key; (d)
closing the root tile with a pending card re-parks it and reopening restores
exactly one; (e) a question asked from depth 2 in opencode is pulled into the
root tile once.

### 6. Flyout UI

- Indent the row by depth (a small fixed step; no new colour or font size —
  read the kit tokens through `theme`), show the status word per decision 1,
  keep the row's open/stop exclusivity and the existing hover/rounded shape.
- `aria_label` names the agent, its status and its parent; the status spinner
  gets its own accessible name and tooltip.
- Section header summary becomes the live-row count (“3 running”, “1 needs
  you” when any row waits).
- The `Tasks & agents` toggle appears when the subtree or the tasks are
  non-empty (unchanged rule, wider source).

Tests: extend the real-window probe style already used in
`tests/pane_moves_probe.rs`/`tests/sidebar_keys_probe.rs` — seed a root tile
with a two-level tree and assert the flyout shows both rows, indented, with the
status words and the right aria labels.

### 7. Running panel

- Feed the panel from the same forest: one row per live chat, then its live
  runs indented, each with the status word; a chat with no run keeps today's
  single row. A chat is listed when it or any run beneath it is live; an open
  subagent chat is represented by its run row, never as a second chat row.
- Keep the chip and OS title on chats (`activity()` unchanged in wording:
  a lone chat names itself, several say “N running”).
- The panel closes on the last chat settling as it does today; a chat whose
  only live work is a subagent keeps it open, labelled *Running* / *Needs you*
  per the tree.

Tests: extend `running_jobs_orders_chats_by_start_and_activity_follows`
(`src/store/mod.rs:3203`) with a nested run: chat first, children after,
`activity()` still “2 running” for two chats, not four.

### 8. Sidebar hygiene (decision 5)

- Do not insert `origin: subagent` sessions into `ws.state.sessions`, and do
  not render them as chats; keep them in the parent map and reachable from the
  tree.
- A subagent session that an older build already adopted is dropped from the
  sidebar on the next list without touching its tile, if open.
- The door's `status` payload is unchanged — subagents are not chats and Warp
  Station must see no new rows.

Tests: a list-apply test asserting a subagent row does not enter
`ws.state.sessions` but does enter the tree.

### 9. Docs and close-out

- One `AGENTS.md` note: the parent map is additive, dsh's
  `api-session/added` is the live source, the tree is per tile and per panel,
  a subagent chat's status comes from the same running set, and an ask always
  stays in the root chat of its lineage (re-parked when that chat closes).
- Delete this plan when the slices land, or fold the surviving rules into the
  app's docs.

## Verification

```sh
cargo test -p warp-agent
cargo build -p warp-agent
cargo fmt --all --check
cargo clippy -p warp-agent --all-targets -- -D warnings
```

Manual check against a real dsh chat: start one and prompt “spawn a subagent
which spawns its own subagent; give both several tool calls that ask for
approval”. Expected:

- The root chat's flyout shows both rows indented with `Running`; the running
  panel shows the chat followed by both runs.
- The middle subagent's own chat shows its child and its own `Running` head.
- Closing and reopening the middle chat keeps the child row.
- The grandchild's permission card appears in the **root** chat, never in the
  grandchild's own chat; the grandchild's run row says *Needs you* and
  approving in the root resumes it.
- A question from the grandchild is answerable the same way.
- Closing the chat that shows a pending card and reopening it brings the same
  card back once.
- Stop on the grandchild aborts only it.

Do not change the door contract in this work: it stays the same commands and
fields, and Warp Station is untouched.

## Risks and out of scope

- `api-session/added` payload shape: confirmed from the installed host's event
  signature (one `SessionSummary` argument) but not yet captured in this app's
  logs. Capture one real frame before slice 1 lands; if the emit arrives with
  the summary nested differently, fold it in `fold_emit` only.
- dsh child ids are bare UUIDs while parent ids are prefixed; mixing the two
  would silently break the map. Never strip or add `session-`.
- Decision B keeps every subagent card in the root chat: looking inside the
  subagent's own chat shows *Needs you* but not the card, so answering means
  going to the root. That is the accepted trade; the run row and the rail dot
  are the pointer.
- Re-parking on close is new state to keep consistent: a re-parked ask must
  not be answerable twice, and the auto-allow countdown must keep running
  while parked (id-keyed today, so it should; test it).
- Opencode parity: opencode already sends `parentID` on session events, so the
  tree should work there unchanged; its question pull returns the whole
  pending list, so the ownership filter must stay strict. Verify one opencode
  run before shipping, and do not add a dsh-only rendering path.
- Subagent cancellation is the least certain endpoint: dsh's own web client
  uses `subagents/interruptByUser` for a child, but `session/cancel` has not
  been tried against a child. Probe both once; keep whichever the host
  accepts, with the other as a fallback only if the failure is a clean
  `not-found`/`unsupported` code.
- Out of scope: the full dsh Tasks page (graph, workflow runs, agent teams),
  the `subagents/*` prompt/continuation methods, persisted run history with
  outcome text, and any change to how a subagent's own transcript renders.
