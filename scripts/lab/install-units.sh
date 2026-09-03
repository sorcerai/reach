#!/usr/bin/env bash
# Run INSIDE reach-lab (unlike hermes-setup.sh, which runs on the mini and
# shells in): limactl shell reach-lab bash -lc 'cd ~/src/reach && scripts/lab/install-units.sh'
#
# Installs the reach-serve + hermes-gateway user units and enables them so
# they (a) start now and (b) survive VM restarts, plus linger so they start
# at boot even with no interactive login session.
set -euo pipefail
cd "$(dirname "$0")/../.."

mkdir -p ~/.config/systemd/user ~/.config/systemd/user/hermes-gateway.service.d
cp integrations/systemd/reach-serve.service ~/.config/systemd/user/
cp integrations/systemd/hermes-gateway.service ~/.config/systemd/user/
# Drop-in for settings hermes's own self-heal (run_gateway() calls
# refresh_systemd_unit_if_needed() on every start) would otherwise wipe from
# the base unit above — see the comment in the drop-in file itself.
cp integrations/systemd/hermes-gateway.service.d/override.conf \
  ~/.config/systemd/user/hermes-gateway.service.d/

systemctl --user daemon-reload
systemctl --user enable --now reach-serve hermes-gateway
loginctl enable-linger "$USER"
