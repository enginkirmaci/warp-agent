# opencode v2 attaches to the shared service

_Status: accepted. Moved from warp-suite, 2026-11-22._

warp-agent attaches to the user's OpenCode v2 shared background service instead
of spawning its own `opencode serve`. V2's default is one service per user
account that owns sessions, configuration, integrations and tool execution, and
the TUI, web and desktop clients all connect to it; a private server would fork
the session store and duplicate configuration. The adapter accepts the service
URL and authenticates with Basic auth (username `opencode`), and never starts or
stops the service.

Considered options: a managed private `opencode serve` (isolation and the hard
"disable stops the process" rule, but a second session store and two sets of
config), and supporting both modes (flexibility, at the cost of two lifecycle
paths and a doubled test matrix).

Consequences: disabling `opencode-v2` disconnects and hides it, it does not stop
the service — the one exception to the hard-disable rule; an absent or
unreachable service is a normal state the UI must show, not a crash; sessions
and configuration are shared with the user's other OpenCode clients; and
`opencode` v1 and `dsh` stay managed processes.
