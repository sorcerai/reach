# Agent Computer

reach as a persistent, watchable desktop for an AI teammate (hermes).

| Piece | Where |
|---|---|
| Live view | `http://<public_host>:<novnc_port>/vnc.html?autoconnect=1&resize=remote` (`live_view` tool) |
| Takeover | `auth_handoff` returns `auth_required` + the live view URL; the human logs in there |
| Durable files | `/workspace` (`reach create --workspace`) |
| Browser sessions | profile `default` shared by `browse`, `page_text`, `auth_handoff` |
| Memory cap | `reach create --memory 2.5g` |
| Always-on | restart policy `unless-stopped` (disable with `--no-restart`) |

Set `server.public_host` in `~/.config/reach/config.toml` (or `reach serve --public-host`) to the
address humans reach the host on, e.g. a Tailscale IP.

See `integrations/hermes/` for the hermes side.

## Browser profile notes

Chromium flushes its cookie database on a timer of roughly 30 seconds and drops session cookies if the browser is killed. A headed `browse` window and a Playwright call cannot open the same profile at the same time (Chromium's SingletonLock). When a login must persist across restarts, use `auth_handoff` (Playwright, clean close) rather than a raw `browse` window to authenticate, and pair it with a shared persistent profile via `use_profile`.
