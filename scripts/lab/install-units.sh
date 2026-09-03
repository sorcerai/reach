#!/usr/bin/env bash
# Install the reach-serve + hermes-gateway user units inside reach-lab and
# enable them so they (a) start now and (b) survive VM restarts, plus linger
# so they start at boot even with no interactive login session.
set -euo pipefail
cd "$(dirname "$0")/../.."

mkdir -p ~/.config/systemd/user
cp integrations/systemd/reach-serve.service ~/.config/systemd/user/
cp integrations/systemd/hermes-gateway.service ~/.config/systemd/user/

systemctl --user daemon-reload
systemctl --user enable --now reach-serve hermes-gateway
loginctl enable-linger "$USER"
