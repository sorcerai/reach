---
name: agent-computer
description: Use the reach Agent Computer (MCP server "reach") for anything that needs a browser or a desktop — reading web pages, logging into sites, filling forms, downloading files. Always share the live view link and hand logins to the human.
---

# Agent Computer

You have a persistent Linux desktop with Chromium. The human can watch it live and take over.

## Loop
1. **Announce the screen.** At the start of any browser/desktop task call `live_view` once and tell the user: `Watch here: <novnc_url>`.
2. **Read with text first.** Use `page_text` (JS-rendered) or `scrape` (static). Use `screenshot` only to confirm layout or when text extraction fails. Use `browse` to open a page for the human to see.
3. **Logins are the human's job.** If a page shows a login, 2FA, CAPTCHA, or "verify it's you":
   - call `auth_handoff` with the URL (profile `default`),
   - tell the user: `Take over at <vnc_url>, log in, then tell me "done"`,
   - when they say done, call `auth_handoff` again with `wait_for_url_contains` or `wait_for_selector` describing the logged-in page, then continue.
   - Never ask for a password or code in chat. Never type one.
4. **Durable output goes to /workspace.** Write results under `/workspace/<task-slug>/` (via `exec` or the terminal; `/srv/reach/workspaces/agent-computer` is the same directory on this VM). Everything else is disposable.
5. **Ask before acting outward.** Sending, purchasing, deleting, publishing, or changing a production system needs an explicit yes from the user first.
6. **Report.** End with what was done, what landed where, and the live view link if the screen is left in a state worth looking at.

## Notes
- Sessions persist: once logged in, you usually stay logged in across tasks.
- If a tool returns "no running sandbox" or "unhealthy", stop and tell the user to run `reach recreate agent-computer`.
- Logins must be done through `auth_handoff` (Playwright, clean close) — a login in a raw `browse` window may not persist because Chromium flushes cookies on a ~30 s timer and drops session cookies when killed.
- Routine runs must never wait on a takeover longer than 60 s; report `auth_required` with the live view link and exit.

## Reports

Durable outputs for routines go under `/workspace/reports/<YYYY-MM-DD>-<slug>.md`.
