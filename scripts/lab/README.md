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

Known issue found while wiring this up: `reach serve`'s MCP `initialize`
response uses snake_case field names (`protocol_version`, `server_info`)
where the MCP spec (and every real MCP client, hermes's Python SDK included)
requires camelCase (`protocolVersion`, `serverInfo`). This is a bug in
`crates/reach-cli/src/mcp.rs`'s `McpInitializeResult` (missing `#[serde(rename
= "...")]` on those two fields), not something this script works around.
Until it's fixed, `hermes mcp test reach` (and any real MCP-SDK client) fails
initialize with a Pydantic validation error, even though the raw JSON-RPC
`tools/list` call over `curl` works fine.

### Next: authenticate hermes

hermes needs an interactive login this script cannot do for you:

```
export PATH=/opt/homebrew/bin:$PATH
limactl shell reach-lab hermes auth      # pick a hosted provider (e.g. openai-codex / zai / openrouter)
limactl shell reach-lab hermes model     # select the model to use
```

Once fixed and authenticated, verify with:

```
limactl shell reach-lab bash -lc 'nohup reach serve --sandbox agent-computer >/tmp/reach-serve.log 2>&1 &'
limactl shell reach-lab hermes chat -q "Use page_text to read https://example.com and give me the heading."
limactl shell reach-lab hermes chat -q "Log me into https://github.com/login using the agent computer, then read my profile name."
```
