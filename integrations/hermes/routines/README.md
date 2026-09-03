# Hermes Routines for Agent Computer

This directory contains cron job templates for automated tasks using the Agent Computer skill.

## daily-check.sh

A template routine that demonstrates how to set up an automated check with `hermes cron create`.

### How it works

`daily-check.sh` contains a `hermes cron create` command that:
1. Takes a schedule (e.g., "every day at 08:00")
2. Passes a prompt describing the task (read a dashboard, login if needed, file a report)
3. Uses `--skill agent-computer` to run against the Agent Computer
4. Uses `--deliver origin` to send the result back to the creating chat

### Reports

Routine results are delivered back to the chat via `--deliver origin`. They also land as durable files under `/workspace/reports/` in the sandbox, which is mounted to `/srv/reach/workspaces/agent-computer/reports/` on the host machine. Follow the naming convention:

```
/workspace/reports/<YYYY-MM-DD>-<slug>.md
```

### Timeout behavior

Routine runs must never wait on a takeover longer than 60 seconds. If `auth_handoff` needs the human to intervene (login, 2FA, CAPTCHA, etc.), the routine should:
1. Report `auth_required` immediately with the live view link
2. Exit gracefully without hanging

The timeout is a circuit breaker: if a routine can't proceed without human intervention within 60 seconds, it should acknowledge that and exit, allowing the next scheduled run to try again.

### Usage

1. After authenticating hermes (see `scripts/lab/README.md`), SSH into the guest or use `limactl shell`:

```bash
limactl shell reach-lab bash -lc 'cd ~/src/reach && bash integrations/hermes/routines/daily-check.sh'
```

2. The script creates the cron job and returns a job ID. Check status with:

```bash
limactl shell reach-lab hermes cron list
```

3. To trigger the job manually (for testing):

```bash
limactl shell reach-lab hermes cron trigger <job-id>
```

4. Results appear in `/srv/reach/workspaces/agent-computer/reports/` on the host and are delivered back to the chat.
