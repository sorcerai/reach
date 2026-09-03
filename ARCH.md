# ARCH.md — Construction blueprint

> **For builder agents.** Short by design: negatives (forbidden edges/patterns)
> don't rot; positive specs do. Inject into agent context at session start. A
> change that breaks one of these is DRIFT, not a fix — stop and update this
> file (with a reason + beads issue) first.

## What this is (2-line positive anchor)

An AI-drivable containerized desktop sandbox: a Rust host CLI (`reach-cli`) drives
Docker containers running a full X11 desktop, and exposes them to agents as MCP
tools over SSE — no container-side logic runs on the host process itself.

## Negative invariants (forbidden — breaking one is drift, not a fix)

1. `reach-cli` never runs sandbox-side application logic in-process; it only
   reaches into a container via `docker exec` or the Docker API (bollard).
2. Every MCP tool dispatches through `crates/reach-cli/src/tools.rs` — no tool
   handler lives inline in `mcp.rs` or `main.rs`.
3. `reach serve` and `reach connect` are transports only (SSE / stdio bridge);
   they must not contain tool business logic.
4. Container-side Python helpers are embedded as `pub const` strings in
   `docker.rs` (see `PAGE_TEXT_SCRIPT`, `AUTH_HANDOFF_SCRIPT`) so the binary
   stays self-contained — no loose `.py` files shipped separately at runtime.
5. Secrets (the VNC password) never appear in Docker labels, `reach list`
   output, or log lines; they reach the container only via env var.
6. noVNC (port 6080) and VNC (port 5900) are only reachable through a host
   port forward the operator explicitly requested (`--novnc-port`, compose,
   or `--extra-port`) — reach never binds them to `0.0.0.0` implicitly.
7. `reach-supervisor` is a leaf: nothing in `reach-cli` links against it or
   assumes its internals; the two communicate only via the health/metrics API
   on port 8400.
8. Restart policy (`unless-stopped`) is only applied to sandboxes created with
   `--workspace` (something durable to survive a restart into) — never to a
   purely ephemeral container.

## When this file is wrong

If a task genuinely requires breaking an invariant, update THIS file first (with
a beads issue + reason), then change the code. A silent violation is the exact
late-caught drift this file exists to prevent.
