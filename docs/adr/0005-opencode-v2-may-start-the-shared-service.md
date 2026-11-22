# opencode v2 may start the shared service

_Status: accepted; supersedes ADR-0004's no-start rule. The no-stop rule remains. Moved from warp-suite, 2026-11-22._

warp-agent attaches to the user's OpenCode v2 shared service, but it may start
that service when it is stopped: when the `opencode-v2` harness is enabled, the
adapter runs `opencode service start`, re-reads the registration file and
attaches. It still never stops the service; stopping is the user's or another
client's decision. Starting is idempotent and the service remains shared, so the
app is not its owner.

Considered options: never starting (strict attachment, but a first run that
requires a terminal command), and a user-initiated start button (no silent
background process, but one extra click on every launch after a reboot).

Consequences: an enabled `opencode-v2` makes warp-agent bring up a shared
background service at launch; a failed start is a normal "service unavailable"
state, not a crash; and disabling the harness disconnects and hides it without
stopping the service.
