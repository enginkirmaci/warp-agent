# Harness support is compiled in

_Status: accepted. Moved from warp-suite, 2026-11-22._

warp-agent reaches coding-agent runtimes through adapters that are compiled into
the binary and listed in one registry, not through a runtime plugin directory.
Each adapter implements the `Harness` interface for one protocol; adding one is
a Rust module plus a registry entry and a settings row, shipped with an app
release. The deciding factor is that every adapter already drives a separate
process over loopback HTTP/WebSocket, so a dynamic plugin system would only add
the ability to install adapter *code* — and with it a stable ABI, version
negotiation, sandboxing and failure isolation that no requirement asks for.

Considered options: runtime plugins (dylib or wasm) for third-party harnesses —
the most literal reading of "plugin", but Rust has no stable ABI and the
versioning and sandboxing work is a separate project; and a process-level
harness protocol every runtime would implement — the cleanest long-term shape,
but `opencode` and `dsh` would both need a bridge before the first new harness
runs.

Consequences: a new harness or protocol ships with an app release, so adding one
is a code change rather than a user action; disabling a harness hides it from
the picker and the door and stops its process while its code stays compiled in;
and the registry, not a settings file, is the single list of what the app can
run.
