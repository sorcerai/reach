# reach Agent Computer for hermes — design

Date: 2026-09-02
Status: approved for planning
Scope: reach + a hermes deployment on the Mac mini, later a dedicated box

## 1. Goal

Give hermes a "Grok Bot"-style Agent Computer: a persistent, always-on
desktop sandbox that hermes drives, that the human can watch live and take
over for logins, that keeps browser sessions and files between tasks, that
runs routines unattended, and that several bots can share with one screen
each.

Reference model (xAI Grok Bot, docs.x.ai/grok-bot, Aug 2026):

- One persistent cloud computer per **account**, shared by all of that
  account's bots. Files, cookies, logins, CLI credentials are shared.
- Each bot has its **own screen** on that computer; one computer-use task
  per screen at a time.
- **Agent Computer view**: a live stream of the screen from any chat.
- **Takeover**: on passwords, 2FA, CAPTCHA the bot pauses, the human does
  the step on the live view, then hands control back. Secrets never go
  through chat.
- Only `/workspace` is durable; the rest may be reset to a snapshot.
- Approvals: allow once / deny / always allow, plus review rules.
- Skills (reusable instructions) and routines (schedule or event triggered).
- Bots message each other and hand off work.

## 2. Non-goals

- Reproducing Grok's desktop/iOS apps. Chat surface is the hermes CLI and
  web dashboard for v1; messaging platforms come from hermes, not reach.
- A local vision model. The test profile uses a hosted model.
- Running on the live default hermes (WhatsApp). It is untouched.

## 3. Decisions taken during brainstorming

| Decision | Choice |
|---|---|
| Approach | Hybrid: reach becomes the Agent Computer (C); hermes integrates via a plugin against reach's agent API (B). Phase 1 is deliberately A-shaped (no plugin, skill-driven) inside that design. |
| Phase order | 1 watch + takeover, 2 always-on + routines, 3 multi-bot screens |
| Host for phases 1–2 | Mac mini, new Lima VM `reach-lab`, separate hermes install |
| Chat surface | hermes CLI / web dashboard |
| Model | hosted (OpenRouter, openai-codex, or Z.AI GLM via hermes native providers) |
| Phase 3 hardware | a 32 GB mini-ITX box with a GPU, if phases 1–2 prove the value |

## 4. Topology

### Phase 1–2 (Mac mini)

```
Mac mini (macOS arm64, 16 GiB, Tailscale 100.124.38.17)
 ├ Docker Desktop (existing, ~10 containers)          untouched
 ├ Lima growthcapital-fleet (2 CPU, 3 GiB)            untouched
 └ Lima reach-lab  (Ubuntu 24.04 arm64, vz, 2 vCPU, 4 GiB, 30 GiB)
    ├ Docker CE rootless (Lima `docker` template)
    ├ hermes          HERMES_HOME=~/.hermes  (own install, own model auth)
    ├ reach serve     127.0.0.1:4200   MCP over SSE
    └ container agent-computer  (image reach:arm64)
        Xvfb :99 · openbox · x11vnc :5900 · noVNC :6080 · supervisor :8400
        Chromium (Playwright build) · Playwright · Scrapling
        volumes: /workspace, chrome profile "default"
        memory cap 2.5 GiB, shm 512 MiB
 Lima port forward: guest 6080 -> host 100.124.38.17:6080 (Tailscale IP only)
```

Live view URL the human opens: `http://100.124.38.17:6080/vnc.html?autoconnect=1&resize=remote`.

### Phase 3 (new box)

Same shape, one VM (or bare Linux) with 12–16 GiB for the Agent Computer,
`--screens N`, one hermes profile per bot, hermes Kanban for handoffs.

## 5. Components

### 5.1 reach image

- **Multi-arch.** amd64 keeps Google Chrome. arm64 has no Chrome; use the
  Playwright Chromium already installed by `playwright install chromium`.
  A wrapper `/usr/local/bin/reach-chrome` resolves to whichever exists.
  Chrome policies are copied to both `/etc/opt/chrome/policies/managed`
  and `/etc/chromium/policies/managed`.
- **Screens (phase 3).** `REACH_SCREENS=N` (default 1). Supervisor starts
  N stacks: Xvfb `:99+i`, openbox, x11vnc `5900+i`, noVNC `6080+i`.
  Health API gains `GET /screens` → `[{id, display, vnc_port, novnc_port}]`.

### 5.2 reach CLI

Phase 1:

- `reach create --workspace [PATH]` mounts a host dir at `/workspace`
  (default `~/.local/share/reach/workspaces/<name>`).
- `reach create --memory <bytes|2g>` sets the container memory cap;
  `sandbox.memory` and `sandbox.shm_size` in config. Default shm drops
  to 512 MiB.
- `reach create --restart unless-stopped` (default on for `--workspace`).
- `browse` launches `reach-chrome` with `--user-data-dir` of the persistent
  profile (`use_profile`, default `default`), so GUI browsing, `page_text`,
  and `auth_handoff` share cookies. Today `browse` uses a throwaway profile.
- New config `server.public_host` (and `--public-host`). `auth_handoff`,
  `live_view`, and `connect` build noVNC URLs from it instead of
  `localhost`.
- New MCP tool `live_view {screen?}` → `{novnc_url, screen, display,
  busy: bool}`. Cheap, side-effect free; the skill calls it once per task
  and sends the link.
- `auth_handoff` keeps its contract; its message names the public URL.

Phase 2:

- `reach recreate <name>`: pull/replace the container keeping volumes
  (Grok's "recover"). `reach destroy --keep-volumes` is the same idea.
- `reach serve` runs as a systemd user unit in the VM.

Phase 3 (agent API, the hermes contract):

- `GET  /agent/screens` → `[{id, owner, novnc_url, busy, takeover_pending}]`
- `POST /agent/screens/{id}/lease {owner}` / `DELETE .../lease`
- `POST /agent/screens/{id}/takeover {pending: bool, url?}`
- Every MCP tool accepts `screen` (int, default 0) → sets `DISPLAY`.
- Cookie sharing across screens: one Chromium per screen with its own
  user-data-dir; after a successful `auth_handoff` reach exports Playwright
  `storage_state` to `/workspace/.reach/state.json`, and Chromium contexts
  on other screens import it on launch. Headed `browse` windows get it via
  a fresh profile seeded from the same state. Exact parity with Grok's
  single-profile sharing is not possible with two live Chromium instances;
  this is the documented approximation.

### 5.3 hermes side

Phase 1 (no code in hermes):

```yaml
# Merge into ~/.hermes/config.yaml inside reach-lab.
mcp_servers:
  reach:
    url: http://127.0.0.1:4200/mcp
gateway:
  # Linux/systemd-only: makes `hermes gateway` send sd_notify heartbeats and
  # lets the hermes-gateway.service unit run Type=notify with a matching
  # WatchdogSec, so systemd restarts the process if its event loop stalls.
  systemd_watchdog_seconds: 120
# One browser only: the Agent Computer. Drop hermes's own browser ("browser") +
# host desktop control ("computer_use") toolsets by omitting them here.
# reach's MCP tools need no toolset entry: mcp_servers below enables them directly.
platform_toolsets:
  cli: [web, terminal, file, skills, todo, cronjob, memory, vision, delegation]
terminal:
  backend: local
  cwd: /srv/reach/workspaces/agent-computer
```

Superseded during implementation: hermes has no `mcp` toolset, the coding
toolset is `delegation`, and reach's `/mcp` is used as Streamable HTTP (no
`transport` key). See `integrations/hermes/config.snippet.yaml` for the
shipped config.

Built-in `browser` toolset disabled for this profile so the model has one
browser, the Agent Computer. `computer_use` (cua-driver) disabled.
Terminal backend stays `local` (the VM is the boundary).

Skill `~/.hermes/skills/agent-computer/SKILL.md` — the grokbot loop:

1. When a task needs the browser or desktop, call `live_view` once and
   tell the user the link: "Watch here: <url>".
2. Prefer `page_text` / `scrape` for reading; `screenshot` only to verify
   layout or when text extraction fails.
3. On a login wall, call `auth_handoff` with `use_profile: default` and
   tell the user "Take over at <url>, then say done." Re-call with
   `wait_for_url_contains` or `wait_for_selector` to resume. Never ask for
   a password or code in chat.
4. Durable outputs go under `/workspace/<task>/`. Everything else is
   disposable.
5. Ask before sending, purchasing, deleting, publishing, or changing a
   production system. Report what was done, where it landed.

Phase 2: hermes `gateway` as a systemd user unit (runs cron); routines are
hermes cron jobs with `--skill agent-computer`; results delivered to
`/workspace/reports/` and the chat of origin.

Phase 3: hermes plugin `reach-agent-computer`:

- on session start, leases a screen for the profile (`owner = profile`),
- injects `screen` into every reach tool call,
- surfaces `takeover_pending` as a notice in CLI and dashboard with the
  link,
- releases the lease on session end.
- One hermes profile per bot, each with its own gateway; the default
  profile owns the Kanban dispatcher (hermes rule). Handoffs are Kanban
  tasks.

## 6. Data flow (phase 1, a login-gated task)

```
user ──chat──▶ hermes ──MCP/SSE──▶ reach serve ──docker exec──▶ container
  ◀── "Watch here: http://100.124.38.17:6080/…"  (live_view)
hermes: page_text(url) → login wall detected
hermes: auth_handoff(url, use_profile=default) → status=auth_required
  ◀── "Take over at <url>, say done"
user: logs in on noVNC
hermes: auth_handoff(url, wait_for_url_contains="/dashboard") → authenticated
hermes: page_text / exec … → /workspace/<task>/report.md
  ◀── summary + path
```

## 7. Resource budget

| Item | Mac mini phase 1–2 | New box phase 3 |
|---|---|---|
| VM | 2 vCPU, 4 GiB | 4–6 vCPU, 12–16 GiB |
| container cap | 2.5 GiB, shm 512 MiB | 8 GiB, shm 1 GiB |
| screens | 1 | up to 4 |
| hermes | ~400 MiB | one per bot |

Host headroom on the mini is about 4 GiB today (16 minus Docker Desktop
minus growthcapital-fleet). If the VM swaps, first shrink Docker Desktop.

## 8. Security

- noVNC is bound to the Tailscale IP only via the Lima `hostIP` port
  forward. x11vnc stays `-nopw` in phase 1; phase 2 adds a VNC password
  (`reach create --vnc-password`) once a second person can reach the
  tailnet.
- `reach serve` binds 127.0.0.1 inside the VM. Nothing in the container
  is exposed to the LAN.
- Secrets never traverse chat; `auth_handoff` is the only path.
- The `exec` tool is arbitrary shell inside the container; the container
  is the boundary, not the tool. hermes dangerous-command approval covers
  the VM's local terminal only.
- Grok's caveat holds here: bots sharing an Agent Computer are not a
  security boundary from each other.

## 9. Error handling

- Container unhealthy: `reach serve` returns a tool error naming
  `reach recreate`; the skill tells the user and stops.
- `auth_handoff` timeout: returns `status: timeout`; the skill asks the
  user whether to retry or abandon.
- Chromium OOM inside the cap: supervisor restarts nothing (Chromium is
  not supervised); the next `browse`/`page_text` relaunches it. Documented.
- VM reboot: Docker rootless + `--restart unless-stopped` bring the
  container back; systemd user units bring `reach serve` and hermes
  gateway back; `loginctl enable-linger` keeps user units alive.

## 10. Testing

- Unit: config parsing (`public_host`, `memory`, shm), `--workspace`
  default path, `live_view` schema, `browse` command line uses the profile
  dir, `reach-chrome` resolution logic.
- e2e (`--ignored`, needs Docker): workspace mount visible at `/workspace`,
  `live_view` returns the public host, `browse` then `page_text` share a
  cookie set by a test page, memory cap reflected in `docker inspect`.
- CI: add a `linux/arm64` buildx job that builds the image and runs the
  existing smoke test under QEMU (or a native arm64 runner if available).
- Integration in `reach-lab`: `curl` MCP initialize; hermes one-shot
  `hermes chat -q "read https://example.com with page_text"`; manual
  login-gated task following §6.
- Phase 3: supervisor test that N screens come up and health lists them;
  lease API tests; plugin test that `screen` is injected.

## 11. Open questions (not blocking phase 1)

- Whether hermes's MCP client passes through image content from
  `screenshot` to a vision model on every hosted provider. Verify in
  phase 1 integration.
- VNC password vs. Tailscale ACLs as the phase 2 boundary.
- Whether the new box runs Linux bare metal (simplest) or a VM.
