#!/usr/bin/env bash
# Install hermes inside reach-lab with its own HERMES_HOME and wire it to reach.
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH=/opt/homebrew/bin:$PATH
limactl shell reach-lab bash -lc '
  set -eux
  export PATH="$HOME/.local/bin:$PATH"
  command -v hermes >/dev/null || curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash
  mkdir -p ~/.hermes/skills/agent-computer
  cp ~/src/reach/integrations/hermes/skills/agent-computer/SKILL.md ~/.hermes/skills/agent-computer/
  # Prefer system python3 with PyYAML; fall back to hermes own venv python, which ships PyYAML.
  PY=python3
  python3 -c "import yaml" >/dev/null 2>&1 || PY="$HOME/.hermes/hermes-agent/.venv/bin/python"
  "$PY" - <<"EOF"
import yaml, pathlib

def deep_merge(base, snip):
    """Merge snip into base in place: nested maps merge key by key,
    scalars/lists from snip win over base."""
    for key, value in snip.items():
        if isinstance(value, dict) and isinstance(base.get(key), dict):
            deep_merge(base[key], value)
        else:
            base[key] = value
    return base

cfg = pathlib.Path.home()/".hermes/config.yaml"
base = yaml.safe_load(cfg.read_text()) if cfg.exists() else {}
base = base or {}
snip = yaml.safe_load(open(pathlib.Path.home()/"src/reach/integrations/hermes/config.snippet.yaml"))
deep_merge(base, snip)
cfg.write_text(yaml.safe_dump(base, sort_keys=False))
EOF
  echo "Now run: limactl shell reach-lab hermes auth   # pick a hosted provider"
'
