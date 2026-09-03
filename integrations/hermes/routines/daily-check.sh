#!/usr/bin/env bash
# Example routine: every morning, read a dashboard and file a report under /workspace/reports.
hermes cron create "every day at 08:00" \
  "Use the agent-computer skill. Open https://example.com/dashboard with page_text (use profile default). \
   If a login is required, stop and tell me to take over. Otherwise write a 10-line summary to \
   /workspace/reports/\$(date +%F)-dashboard.md and reply with its path." \
  --skill agent-computer --deliver origin
