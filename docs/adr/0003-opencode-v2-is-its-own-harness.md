# opencode v2 is its own harness

_Status: accepted. Moved from warp-suite, 2026-11-22._

OpenCode v2 is a separate harness (`opencode-v2`), not a protocol version the
`opencode` entry negotiates. V2's server is a different API — `/api/*`, a new
event vocabulary, mandatory Basic auth, and one server for many locations — and
a v1 client gets 404s rather than a degraded answer; the two releases also share
the `opencode` command and are not installed side by side by default. The
registry therefore lists `opencode` (v1) and `opencode-v2` with independent
enable and connection settings, and coexistence is a manual setup: the v1
binary on its own port beside the v2 service.

Considered options: one `opencode` harness that probes `/api/info` against
`/global/health` and speaks whichever answers (fewer rows, but a single entry
cannot hold two servers, two credentials, or run both at once), and v2 replacing
v1 outright (cleanest, but no transition and no fallback).

Consequences: `opencode-v2` joins the settings and door vocabulary, so the
settings schema's "`HarnessKind` stays `opencode`/`dsh`" line moves; v1 can be
disabled and later removed once v2 is the daily driver; and a harness is a
connection, not just an executable, because v2 may be a service the app attaches
to rather than one it spawns.
