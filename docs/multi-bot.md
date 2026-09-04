# Multi-Bot Agent Computer & Kanban Handoff Walkthrough

This guide details how multiple autonomous Hermes bots share a single Reach Agent Computer, run on independent virtual screens simultaneously, share authenticated sessions, and coordinate multi-step workflows via the Hermes Kanban dispatcher.

---

## 1. Multi-Screen Architecture

Reach multiplexes $N$ independent virtual desktop screens inside a single container or MicroVM:

```
                                Reach Host (:4200)
                                        │
           ┌────────────────────────────┴────────────────────────────┐
           ▼                                                         ▼
   Screen 0 (:99)                                            Screen 1 (:100)
   • noVNC: :6080                                            • noVNC: :6081
   • VNC:   :5900                                            • VNC:   :5901
   • Leased by: `piper` (Research)                           • Leased by: `otto` (Ops)
   • Profile: `default`                                      • Profile: `default-screen1`
           │                                                         │
           └────────────► /workspace/.reach/state.json ◄─────────────┘
                         (Shared Cookie & Session State)
```

Each bot owns:
* A dedicated X11 display (`DISPLAY=:99+i`).
* A dedicated noVNC web streaming port (`6080+i`) for human live viewing.
* An isolated browser user data profile (`default-screen{i}`).
* Synchronized session cookies via Playwright `storage_state.json`.

---

## 2. Resource Footprint & Density

Measured Resident Set Size (RSS) per virtual screen stack:

| Process | Memory (RSS) | Description |
| :--- | :--- | :--- |
| `reach-supervisor` | 1.2 MB | Host-level process supervisor (shared across all screens) |
| `Xvfb` (Virtual Display) | 15.0 MB | In-memory 720p 24-bit frame canvas |
| `openbox` (Window Manager) | 3.5 MB | Minimal window framing & focus management |
| `x11vnc` (VNC Server) | 14.5 MB | Frame polling and RFB socket server |
| `websockify` (noVNC Bridge) | 19.0 MB | WebSocket-to-TCP proxy |
| **Total Base Display Stack** | **~53.2 MB** | **Per active screen** |
| **Active Chromium Tab** | **120 – 250 MB** | With `--renderer-process-limit=2` and V8 memory caps |

> **Density Impact**: An 8 GB host can comfortably run **16 to 24 concurrent agent screens** without CPU starvation or OOM swapping.

---

## 3. Bot Setup & Profile Configuration

We configure two specialized bot profiles from the baseline:
* **`piper`**: Research bot responsible for crawling, scraping, and collecting data.
* **`otto`**: Operations bot responsible for data entry, filing reports, and updating systems.

### Step 1: Create Hermes Profiles
```bash
# Clone piper from default
hermes profile create piper --clone default

# Clone otto from default
hermes profile create otto --clone default
```

### Step 2: Configure Kanban Dispatcher
In a multi-bot setup, **only one profile owns the Kanban dispatcher** to prevent race conditions. The default profile acts as the dispatcher, while worker profiles listen for assigned cards.

**`~/.hermes/config.yaml` (default profile):**
```yaml
kanban:
  enabled: true
  dispatch_in_gateway: true
  board: agent-computer
```

**`~/.hermes/profiles/piper/config.yaml` & `otto/config.yaml`:**
```yaml
kanban:
  enabled: true
  dispatch_in_gateway: false
  board: agent-computer

plugins:
  enabled:
    - reach-agent-computer

mcp_servers:
  reach:
    url: http://127.0.0.1:4200/mcp
```

---

## 4. Multi-Bot Kanban Task Chain: "Piper Collects → Otto Files"

### The Workflow Chain
1. **Card 1 (Assigned to Piper)**:
   * Target: Crawl target documentation or search query.
   * Action: Lease Screen 0, extract structured data, write to `/workspace/reports/data.json`.
   * Trigger: On completion, move Card 1 to `Done` and create Card 2 for `otto`.
2. **Card 2 (Assigned to Otto)**:
   * Target: Read `/workspace/reports/data.json`.
   * Action: Lease Screen 1, open target internal dashboard, fill form, submit.
   * Session Hydration: Uses shared cookies from `/workspace/.reach/state.json`.

### Starting the Bot Gateways
Launch the two worker gateways in background sessions:
```bash
# Start Piper (leases Screen 0)
hermes -p piper gateway &

# Start Otto (leases Screen 1)
hermes -p otto gateway &
```

### Checking Active Screens & Leases
Query Reach's agent endpoint to verify concurrent screen leases:
```bash
curl -s http://127.0.0.1:4200/agent/screens | jq .
```
**Output:**
```json
[
  {
    "id": 0,
    "owner": "piper",
    "takeover_pending": false,
    "takeover_url": null,
    "leased_at": "2026-09-04T13:20:00Z",
    "novnc_url": "http://127.0.0.1:6080/vnc.html?autoconnect=1&resize=remote"
  },
  {
    "id": 1,
    "owner": "otto",
    "takeover_pending": false,
    "takeover_url": null,
    "leased_at": "2026-09-04T13:20:05Z",
    "novnc_url": "http://127.0.0.1:6081/vnc.html?autoconnect=1&resize=remote"
  }
]
```

---

## 5. Human Takeover Protocol During Handoffs

If either bot encounters a CAPTCHA or 2FA login gate:
1. The `reach-agent-computer` plugin catches `status: auth_required`.
2. The bot halts execution and flags `takeover_pending=true`.
3. The bot posts a message to chat:
   > *"Authentication required on Screen 0. Watch and take over: http://127.0.0.1:6080/vnc.html?autoconnect=1&resize=remote"*
4. The user completes the login in their browser.
5. As soon as authentication completes, Reach:
   * Clears `takeover_pending`.
   * Automatically exports `/workspace/.reach/state.json`.
   * Resumes the bot workflow.
   * Makes the new authenticated session immediately available to Otto on Screen 1.

---

## 6. MicroVM Instant Reset / Recovery

If an agent gets trapped or corrupts its environment, the instance can be obliterated and restored from the golden template in under 3 seconds without losing `/workspace`:

```bash
# Nuke dirty instance and restore pristine state
./scripts/microvm.sh reset agent-computer

# Verify health
./scripts/microvm.sh healthcheck agent-computer
```
