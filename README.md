# reach

A local-first computer-use runtime: headed browsers and virtual displays in disposable sandboxes, a native control plane, Python drivers, and Hermes/Buzz integrations.

## Security model

The host supervisor owns accounts, credentials, lease allocation and approvals. Model-facing callers receive a scoped lease capability, not the supervisor bearer. A display is a routing boundary, **not** an account-isolation boundary: account leases are exclusive across the computer, and code-enabled computers cannot host them.

```text
Operator / authenticated broker
  ├── account policy, vault, cookie jars and approval decisions
  └── native API bound to one computer incarnation
        ├── lease-scoped browser / desktop tools
        ├── native credential injection over stdin
        └── authenticated viewer → private RFB bridge → virtual display
```

### Leases and private state

`reach serve` requires `REACH_AUTH_TOKEN`, including on loopback. Supply it through your secret manager or process environment; do not put it in a URL. The server binds one sandbox at startup.

The supervisor allocates a screen with `POST /agent/screens/{id}/lease`. The request includes an owner and may select a configured account, task ID and attempt ID. The response contains the lease token and handoff generation. Workers send:

- `X-Lease-Token` and `X-Handoff-Gen` on every leased tool call.
- `X-Observation-Gen` from a fresh response's `_meta` before a mutation.

Owner labels are not credentials. Lease callers cannot change account/profile identity, supply arbitrary storage state or select ungranted cookie jars. `exec` and `playwright_eval` require both an explicitly code-enabled computer and an explicitly code-enabled clean lease.

Account policy lives in `$XDG_CONFIG_HOME/reach/config.toml` (default `~/.config/reach/config.toml`). An example policy shape is:

```toml
[accounts.work]
profile = "work"
origins = ["https://example.com"]
jars = ["example.com"]
jars_path = "/absolute/private/cookies.json"
vault_path = "/absolute/private/vault.json"
cards_path = "/absolute/private/cards.json"
```

Paths are operator configuration, never model arguments. Only configure the stores that the account needs. Cookie hydration is explicit; there is no global `/workspace/.reach/state.json` fallback.

### Exact approvals

Native admission gates UI mutation and secret injection; account navigation also requires approval. This is not a model-side keyword classifier.

1. Capture a fresh observation and retain its generation.
2. Submit the intended action. A pending action returns `approval_required` and a digest without executing it.
3. The supervisor inspects `GET /agent/screens/{id}/approval` and approves that digest with `POST` to the same route.
4. Resubmit the exact action under the same live lease and observation.

Approvals bind the action arguments, account grant, task/attempt, computer incarnation and observation/handoff generations. They expire, are consumed once, and do not survive replacement or lease recovery. A returned `approval_required` is not an approval. Releasing the lease invalidates the proposal; a new attempt must observe and propose again.

An origin check is an admission check, not an atomic transaction with a later OS input event. Authorized sites still receive entered credentials and can run their own scripts; do not grant an origin you do not trust.

`browse`, `page_text`, and injection intercept top-level navigation requests, including redirects, while their native helpers run. This is not a persistent browser-network firewall.

### Authenticated viewing and human handoff

Open `reach vnc <sandbox> --api-url http://127.0.0.1:4200` after a worker leases the screen. The host-owned `/viewer/{screen}` page asks for the supervisor bearer and issues a short-lived, HttpOnly, SameSite Strict, screen-scoped cookie. Use HTTPS for remote access so the cookie is Secure.

- Observer sessions cannot send keyboard, pointer or clipboard input through the RFB filter.
- Control sessions require an active human handoff.
- Handback revokes human control. The agent must acknowledge it and capture a fresh observation before acting again.
- Ordinary release cannot eject an active human or an in-flight action. Supervisor force-release is separate and also respects in-flight work.
- Human tokens are not placed in query strings or model responses.

noVNC assets are served by the host, not by sandbox-controlled HTML/JavaScript. Raw websockify connections require an internal bearer; x11vnc listens only on the container loopback interface. Default Docker publications are host-loopback-only and sandboxes have managed isolated bridges. Do not publish CDP or otherwise bypass these boundaries; an operator with Docker/host access remains trusted.

### Native secret injection

The `inject` tool accepts identifiers (`kind`, `domain`, optional `card_id`, and `submit`), not secret values or executable code. The native broker reads the explicitly granted host store and sends values to a fixed browser helper over stdin. It validates the active page, exact origin and effective form/submitter destination, and returns a sanitized receipt.

DOM observations suppress values from native-filled controls, including usernames and card-expiration fields.

Fill-only injection supports form-less controls. Automatic submission requires one common retained form, its exact associated submitter, and an empty or `_self` effective target. Popup, named-frame, and inherited non-self targets require human handling.

A `submit_dispatched` receipt proves dispatch, not business completion. Verify a fresh postcondition separately. Card injection reserves state before dispatch; uncertain outcomes remain locked for reconciliation rather than being retried automatically.

Credentials necessarily enter the authorized website. The broker does not promise that a website cannot echo sensitive data into its content. Treat observations as sensitive runtime data and do not persist raw screenshots or DOM captures.

## Recovery and operator truth

- Computer incarnation, task/attempt and handoff scope bind element references. Old references do not become actionable after restart or handback.
- DOM references and desktop pixel coordinates are different interfaces. References act against the observed browser page; explicit `x`/`y` actions address the desktop.
- `browse` verifies the browser profile, creates an explicit target, navigates once, and checks native focus before reporting success. Starting a process alone is not a navigation receipt.
- Uncertain mutation results stop execution. Reconcile the actual computer state before planning another action; do not replay the failed request blindly.
- Model completion claims are not success. A missing verification marker yields `unverified` or `postcondition_failed`, not a green outcome.
- Buzz projections—including the Hermes groupchat plugin's dedicated takeover and visual-change tools—contain structured metadata, not screenshots or arbitrary model/error text. Task outcome and lease-cleanup outcome are separate.
- Ambiguous task requests need an explicit task contract. Chat text such as “approve” does not grant native authority.

### Routine recording and promotion

Durable traces omit typed values, DOM and frame contents. Typed input uses named runtime parameters. Navigation data beyond a retained origin—including path, query and userinfo—also requires named runtime input rather than silently replaying a shortened URL. Origin checkpoints compare canonical origins exactly.

Recording observes the current page without navigating. Healing is bounded and produces a review candidate; it does not overwrite the routine. Promotion requires the expected routine digest and a complete no-healing replay that satisfies the original checkpoints. Multi-action replacements preserve the original checkpoint on the final replacement action.

Promotion uses a persistent `.routine.lock` shared by the Python and native writers. Replay happens outside the lock; the final digest comparison and atomic replacement happen while it is held. This coordinates repository writers on POSIX hosts; external editors must honor the same lock to avoid lost updates.

### Lifecycle modes

- `--clean` is the default and does not preserve host-backed state.
- `--hydrated` marks one-shot hydration; credentials still come through an authorized account/jar flow, not from the flag itself.
- `--persistent` preserves only explicitly configured host-backed mounts. Profiles/workspaces require deliberate persistence.

Recreation uses a private reset manifest and a fresh computer incarnation. Code permission is not inherited implicitly. Missing manifests fail closed; do not treat an unmanaged legacy container as an authorized recovery source.

## Running and checking

Python 3 is required for the embedded browser-helper regression checks in the Rust suite.

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
uv run --with pytest --with pillow --with numpy pytest tests integrations/hermes/plugins/reach-agent-computer/test_plugin.py integrations/hermes/plugins/buzz-groupchat/test_buzz_plugin.py -q
uv run --with ruff ruff check --select E9,F63,F7,F82 scripts tests integrations/hermes/plugins/reach-agent-computer integrations/hermes/plugins/buzz-groupchat
```

With Docker available and `REACH_AUTH_TOKEN` configured privately:

```bash
reach create --name demo --clean
reach serve --sandbox demo
# Allocate a lease through the authenticated broker before opening the viewer.
reach vnc demo --api-url http://127.0.0.1:4200
```

The standalone driver takes its scoped capability from `REACH_LEASE_TOKEN` and an explicit handoff generation. The Hermes plugin owns authenticated transport through `reach_tool`; do not expose the supervisor bearer as a model argument or bypass that transport with an unscoped MCP call.

MicroVM/Lima scripts remain separate backend utilities. Docker verification does not establish MicroVM boot timing, memory footprint or isolation on another host; validate the actual deployment topology.

## Measurement

```bash
python3 scripts/benchmark_axi.py
python3 scripts/benchmark_axi.py --cases /path/to/local-observations.json
```

This benchmark measures supplied local observations: UTF-8 bytes, local serialization/reference-inspection latency, and expected-reference retention. Token counts are explicitly estimates, not provider measurements. Missing measurements are unknown, not zero. It makes no live-site or model calls and does not establish end-to-end agent latency.

Driver receipts distinguish requested/reported model identifiers from explicit version evidence. An alias alone is not a measured model version. No fixed provider version, throughput, cost or reset-time claim is implied by this repository.

## License

MIT for the project. The bundled noVNC source and license notices are retained under `crates/reach-cli/assets/`; its third-party licenses still apply.
