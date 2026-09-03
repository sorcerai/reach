#!/usr/bin/env bash
# Run INSIDE reach-lab (unlike hermes-setup.sh, which runs on the mini and
# shells in): limactl shell reach-lab bash -lc 'cd ~/src/reach && scripts/lab/install-units.sh'
#
# Installs the reach-serve + hermes-gateway user units and enables them so
# they (a) start now and (b) survive VM restarts, plus linger so they start
# at boot even with no interactive login session.
set -euo pipefail
cd "$(dirname "$0")/../.."

mkdir -p ~/.config/systemd/user
cp integrations/systemd/reach-serve.service ~/.config/systemd/user/
cp integrations/systemd/hermes-gateway.service ~/.config/systemd/user/

systemctl --user daemon-reload
systemctl --user enable --now reach-serve hermes-gateway
loginctl enable-linger "$USER"
