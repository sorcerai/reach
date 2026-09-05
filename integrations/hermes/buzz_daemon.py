"""Hermes integration module for Reach Buzz Agent Daemon."""

from __future__ import annotations

import sys
from pathlib import Path

REPO_ROOT = Path(__file__).parent.parent.parent.resolve()
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.buzz_daemon import (
    BuzzDaemon,
    ParsedTask,
    ReachApiClient,
    buzz_get_messages,
    buzz_list_channels,
    buzz_post_visual_diff,
    buzz_send_message,
    buzz_send_takeover_alert,
    main,
    parse_task_message,
    run_buzz_cli,
)

__all__ = [
    "BuzzDaemon",
    "ParsedTask",
    "ReachApiClient",
    "parse_task_message",
    "buzz_send_message",
    "buzz_send_takeover_alert",
    "buzz_post_visual_diff",
    "buzz_get_messages",
    "buzz_list_channels",
    "run_buzz_cli",
    "main",
]

if __name__ == "__main__":
    main()
