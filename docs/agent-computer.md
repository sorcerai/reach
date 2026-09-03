# Agent Computer

reach as a persistent, watchable desktop for an AI teammate (hermes).

| Piece | Where |
|---|---|
| Live view | `http://<public_host>:6080/vnc.html?autoconnect=1&resize=remote` (port 6080 by default; `live_view` tool) |
| Takeover | `auth_handoff` returns `auth_required` + the live view URL; the human logs in there |
| Durable files | `/workspace` (`reach create --workspace`); host default: `~/.local/share/reach/workspaces/<name>` |
| Browser sessions | profile `default` shared by `browse`, `page_text`, `auth_handoff` |
| Memory cap | `reach create --memory 2.5g` |
| Always-on | restart policy `unless-stopped` (disable with `--no-restart`) |

Set `server.public_host` in `~/.config/reach/config.toml` (or `reach serve --public-host`) to the
address humans reach the host on, e.g. a Tailscale IP.

See `integrations/hermes/` for the hermes side.

## Configuration

Create `~/.config/reach/config.toml` to tune reach's behavior. All keys are optional:

```toml
[server]
public_host = "100.124.38.17"    # Host address reachable by humans; default localhost

[sandbox]
memory = 2684354560              # Hard memory cap in bytes (example: 2.5 GiB); default unlimited
workspace_dir = "/srv/reach/workspaces"  # Host directory for --workspace mounts; default ~/.local/share/reach/workspaces
shm_size = 536870912             # Shared memory size in bytes; default 512 MiB (536870912)
profile_dir = "/home/user/.reach/profiles"  # Host directory for persistent Chrome profiles; default ~/.local/share/reach/profiles
```

## Browser profile notes

Chromium flushes its cookie database on a timer of roughly 30 seconds and drops session cookies if the browser is killed. A headed `browse` window and a Playwright call cannot open the same profile at the same time (Chromium's SingletonLock). When a login must persist across restarts, use `auth_handoff` (Playwright, clean close) rather than a raw `browse` window to authenticate, and pair it with a shared persistent profile via `use_profile`.
