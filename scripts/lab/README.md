# reach-lab

A Lima VM on the Mac mini (`100.124.38.17`, Tailscale) that runs an always-on
`reach` Agent Computer sandbox for hermes.

## Bring it up

From the mini (repo synced to `~/repos/reach`):

```
scripts/lab/up.sh
```

This is idempotent: it starts the `reach-lab` Lima VM if it isn't running,
builds/installs the `reach` CLI inside the VM from the synced source,
loads the arm64 sandbox image via `make lab-load` (built on the mini's
Docker Desktop, `docker save | limactl shell reach-lab docker load`), writes
`~/.config/reach/config.toml` inside the VM, and creates the `agent-computer`
sandbox if it doesn't already exist.

Re-run it any time `config/lima/reach-lab.yaml` changes. For speed, the two
expensive steps are skipped once done once: `cargo install` is skipped if
`~/.cargo/bin/reach` already exists inside the VM, and `make lab-load` is
skipped if `reach:latest` is already loaded there. If you change the `reach`
CLI source or the sandbox image and need a rebuild, force it first:
`limactl shell reach-lab rm -f ~/.cargo/bin/reach` (CLI) and/or
`limactl shell reach-lab docker rmi reach:latest reach:arm64` (image),
then re-run `up.sh`.

## Bring it down

```
export PATH=/opt/homebrew/bin:$PATH
limactl stop reach-lab
```

To delete the VM entirely (loses the installed `reach` binary and cargo
cache, not the host-mounted workspace/profile data):

```
limactl delete reach-lab
```

## Live view

Once `agent-computer` is running, the noVNC live view is reachable over
Tailscale at:

```
http://100.124.38.17:6080/vnc.html?autoconnect=1&resize=remote
```

The `live_view` MCP tool (via `reach serve`) returns a `novnc_url` pointing
at the same host:port.

## What's mounted where

| Host (mini)                        | Guest (reach-lab)      | Container (agent-computer)   |
|-------------------------------------|-------------------------|-------------------------------|
| `~/.local/share/reach-lab`          | `/srv/reach`             | -                              |
| `/srv/reach/workspaces`             | (same, writable)         | `/srv/reach/workspaces/<name>` mounted as the sandbox's `--workspace` |
| `/srv/reach/profiles`               | (same, writable)         | `<profile>` bind-mounted for `--persist-profile default` |
| `~/repos/reach` (mini, via Lima's default home mount, read-only) | `/Users/ariaserver/repos/reach` | - (source only, rsync'd into `~/src/reach` inside the VM before building) |

`reach` itself (the CLI managing Docker containers) runs inside the Lima
guest, talking to the guest's rootless Docker CE. The `agent-computer`
sandbox container's ports (VNC 5900, noVNC 6080, supervisor 8400) are
published by Docker inside the guest; only noVNC (6080) is forwarded further,
from the guest out to the mini's Tailscale interface, via
`config/lima/reach-lab.yaml`'s `portForwards`.

## Rootless Docker socket

Lima's `template:docker` base installs rootless Docker CE, which listens on
`/run/user/<uid>/docker.sock`, not `/var/run/docker.sock`. Lima gives the
guest user the *host* machine's UID, so this is not always 1000 — on the
mini it's 501. `reach` (via bollard) connects with
`Docker::connect_with_local_defaults()`, which only honors the `DOCKER_HOST`
env var — `sandbox.socket` in `config.toml` is parsed but not currently
wired up (a candidate small fix for a later task). `up.sh` derives the uid
with `id -u`, exports `DOCKER_HOST=unix:///run/user/$(id -u)/docker.sock`,
and persists it two ways: in the guest's `~/.profile` for interactive login
shells, and in `~/.config/environment.d/60-docker-host.conf` for systemd
user units (a later task's `reach serve` unit included), since those don't
source shell rc files.

## hermes

`scripts/lab/hermes-setup.sh` installs hermes inside `reach-lab` (guest-only,
under the guest's own `~/.hermes` — never touches the mini's own `~/.hermes`,
which runs the live WhatsApp hermes on macOS) and merges
`integrations/hermes/config.snippet.yaml` into the guest's
`~/.hermes/config.yaml`, plus copies `integrations/hermes/skills/agent-computer/`
into the guest's `~/.hermes/skills/`. It re-syncs `~/src/reach` on the mini
itself; sync the mini first (`rsync -a --delete --exclude target
--exclude .superpowers --exclude .beads /Users/ahpramesi/repos/reach/
ariaserver@100.124.38.17:repos/reach/`) if you're running it from the Mac
mini's own checkout in `~/repos/reach`.

`reach serve`'s MCP `initialize` response previously serialized snake_case
field names (`protocol_version`, `server_info`), which broke the MCP spec's
required camelCase and failed the handshake for every real MCP client,
hermes's Python SDK included. Fixed in `crates/reach-cli/src/mcp.rs`
(`McpInitializeResult` now serializes `protocolVersion`/`serverInfo`); confirm
your guest has it with `cargo install --path crates/reach-cli --locked
--force` after syncing, then `hermes mcp test reach` should report
`✓ Connected` with 11 tools discovered.

### Next: authenticate hermes

hermes needs an interactive login this script cannot do for you:

```
export PATH=/opt/homebrew/bin:$PATH
limactl shell reach-lab hermes auth      # pick a hosted provider (e.g. openai-codex / zai / openrouter)
limactl shell reach-lab hermes model     # select the model to use
```

Once authenticated, verify with:

```
limactl shell reach-lab bash -lc 'nohup reach serve --sandbox agent-computer >/tmp/reach-serve.log 2>&1 &'
limactl shell reach-lab hermes chat -q "Use page_text to read https://example.com and give me the heading."
limactl shell reach-lab hermes chat -q "Log me into https://github.com/login using the agent computer, then read my profile name."
```

### Create and test a routine

After authentication, create a cron job from the routine template and trigger it:

```
limactl shell reach-lab bash -lc 'cd ~/src/reach && bash integrations/hermes/routines/daily-check.sh'
```

This outputs a job ID. To trigger it manually (for testing, since it's scheduled for 08:00):

```
limactl shell reach-lab hermes cron trigger <job-id>
```

Results land in `/srv/reach/workspaces/agent-computer/reports/` (mounted from `/workspace/reports/` inside the sandbox) and are delivered back to the creating chat via `--deliver origin`.

## Always-on

`reach serve` and `hermes gateway` run as systemd **user** units inside
`reach-lab`, so they survive VM restarts without a login shell and without
the manual `nohup reach serve ...` step above:

```
integrations/systemd/reach-serve.service
integrations/systemd/hermes-gateway.service
```

Install (from the synced repo inside the guest):

```
limactl shell reach-lab bash -lc 'cd ~/src/reach && scripts/lab/install-units.sh'
```

This copies both units into `~/.config/systemd/user/`, runs
`systemctl --user daemon-reload && systemctl --user enable --now reach-serve
hermes-gateway`, and `loginctl enable-linger $USER` so the units start at
boot even before anyone logs in — required because `reach-lab` is a
headless VM.

`reach-serve.service` sets `Environment=DOCKER_HOST=unix:///run/user/%U/docker.sock`
directly (systemd user units don't source `~/.profile` or
`~/.config/environment.d/`, so this mirrors the value `up.sh` exports for
interactive shells — see "Rootless Docker socket" above). It also declares
`After=docker.service` + `Wants=docker.service` (not `After=default.target`
— see the ordering-cycle note below) so it doesn't race rootless dockerd
for `/run/user/%U/docker.sock` on a cold boot; without it, reach-serve fails
once and `Restart=always` recovers it 3s later, which is a strictly worse
"always-on" than starting clean.

`hermes-gateway.service`'s `After=reach-serve.service` is ordering only, not
a dependency (`Wants=`) — a fresh `systemctl --user start` can start
hermes-gateway before reach-serve is ready; `Restart=always` on both units
converges shortly after either fails.

**Important — hermes rewrites its own unit file on every start.** hermes's
`run_gateway()` entrypoint calls `refresh_systemd_unit_if_needed()` on every
`hermes gateway` startup (`hermes_cli/gateway.py`) and silently overwrites
`~/.config/systemd/user/hermes-gateway.service` with its own generated
content whenever ours differs — wiping our checked-in `Description=`,
`After=reach-serve.service`, and `TimeoutStartSec=180` within a second of
the process starting. Confirmed live: the base file's content changes to
hermes's own shape on the very first `hermes gateway` run, every time.

Two consequences:
- `install-units.sh` also installs `integrations/systemd/hermes-gateway.service.d/override.conf`
  as a systemd drop-in: hermes only ever writes the single
  `hermes-gateway.service` file, never a `<unit>.d/` directory, so the
  drop-in's `After=reach-serve.service` and `TimeoutStartSec=180` survive
  every self-heal. `systemctl --user cat hermes-gateway` shows both
  fragments merged.
- The watchdog shape (`Type=notify`, `NotifyAccess=main`, `WatchdogSec=120`)
  in our checked-in `hermes-gateway.service` only matters for the first
  process start after a fresh install; from then on it's **hermes's own**
  regenerated unit supplying those directives, driven entirely by
  `gateway.systemd_watchdog_seconds: 120` in `~/.hermes/config.yaml` (added
  by `integrations/hermes/config.snippet.yaml` / `hermes-setup.sh`) — per
  hermes's docs, a plain `Type=simple` unit (the default when that config
  key is `0`) never receives the `sd_notify` heartbeats and the watchdog
  does nothing. `TimeoutStartSec=180` (from the drop-in) gives hermes room
  to load its provider clients before sending `READY=1` — if
  `systemctl --user status hermes-gateway` sits in `activating` past that,
  hermes never sent `READY=1`; check `journalctl --user -u hermes-gateway`.

**Also important — a real ordering cycle.** `reach-serve.service` originally
had `After=default.target` alongside `WantedBy=default.target`; combined
with `hermes-gateway.service`'s `After=reach-serve.service` (made durable by
the drop-in above), this is a genuine cycle — a `.target` unit implicitly
orders itself `After=` everything it `Wants=`, so
`default.target → hermes-gateway → reach-serve → default.target`. Confirmed
live: `limactl stop reach-lab && limactl start reach-lab` produced `Found
ordering cycle` in the user journal and systemd deleted hermes-gateway's
start job to break it — hermes-gateway never started on that boot.
`reach-serve.service` no longer has `After=default.target`;
`WantedBy=default.target` alone is enough to pull it in at boot.

Check status:

```
limactl shell reach-lab systemctl --user is-active reach-serve hermes-gateway   # => active active
```

### Mac mini autostart

So `reach-lab` itself comes back after the mini reboots (e.g. a power
outage), install the launchd agent on the mini (not inside the VM):

```
cp scripts/lab/launchd/ai.reach.lab.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/ai.reach.lab.plist
launchctl print gui/$(id -u)/ai.reach.lab | head
```

It runs `limactl start reach-lab` at login (`RunAtLoad`); logs go to
`~/Library/Logs/reach-lab.{out,err}.log`. Once the VM is up, the systemd
user units above bring `reach serve`, `hermes gateway`, and (via Docker's
`unless-stopped` restart policy) the `agent-computer` container back on
their own — no further manual steps.
