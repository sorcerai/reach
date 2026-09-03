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

Re-run it any time the source, `config/lima/reach-lab.yaml`, or the sandbox
image changes.

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
and persists it in the guest's `~/.profile` so every future login shell (and
any systemd user unit that sources it) picks it up too.
