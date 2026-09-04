#!/usr/bin/env bash
# Bring up reach-lab on the Mac mini, install reach CLI + image, start the Agent Computer.
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH=/opt/homebrew/bin:$PATH
if limactl list reach-lab --format '{{.Status}}' 2>/dev/null | grep -qx Running; then
  :
elif limactl list -q | grep -qx reach-lab; then
  limactl start reach-lab
else
  limactl start --name=reach-lab --tty=false config/lima/reach-lab.yaml
fi
limactl shell reach-lab bash -lc 'mkdir -p ~/src && rsync -a --delete --exclude target /Users/'"$USER"'/repos/reach/ ~/src/reach/ && cd ~/src/reach && if [ ! -x ~/.cargo/bin/reach ]; then cargo install --path crates/reach-cli --locked; fi'
limactl shell reach-lab docker image inspect reach:latest >/dev/null 2>&1 || make lab-load
limactl shell reach-lab bash -lc '
  set -e
  # rootless Docker CE listens on the per-user socket (uid-scoped), not
  # /var/run/docker.sock. Reach reads docker.socket from config.toml.
  # The uid must be derived, not hard-coded: Lima gives the guest user
  # the *host* UID, which varies per machine.
  mkdir -p ~/.config/environment.d
  echo "DOCKER_HOST=unix:///run/user/$(id -u)/docker.sock" > ~/.config/environment.d/60-docker-host.conf
  export DOCKER_HOST="unix:///run/user/$(id -u)/docker.sock"
  mkdir -p ~/.config/reach /srv/reach/workspaces /srv/reach/profiles
  cat > ~/.config/reach/config.toml <<EOF
[server]
public_host = "100.124.38.17"
[sandbox]
memory = 2684354560
workspace_dir = "/srv/reach/workspaces"
profile_dir = "/srv/reach/profiles"
[docker]
socket = "unix:///run/user/$(id -u)/docker.sock"
EOF
  reach list | grep -q agent-computer || reach create --name agent-computer --workspace --persist-profile default --memory 2.5g
'
echo "Live view: http://100.124.38.17:6080/vnc.html?autoconnect=1&resize=remote"
