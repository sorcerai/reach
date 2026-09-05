# Browser Use Integration for Agent Computer 🌐🤖

This integration connects the popular [`browser-use`](https://github.com/browser-use/browser-use) web automation library directly to **Agent Computer's** multi-screen virtual workstations via remote Chrome DevTools Protocol (CDP).

---

## Why Use Browser Use with Agent Computer?

1. **True Visual Isolation**: Run automated browser tasks on dedicated virtual displays (`DISPLAY=:99+i`) without hijacking your primary macOS mouse, keyboard, or focus.
2. **Live noVNC Observation**: Watch the agent browse, type, and click live in your browser at `http://localhost:6080/vnc.html`.
3. **Out-of-Band Secret Brokering**: Securely pass credentials and 2FA TOTP tokens from the host-side vault without writing secrets to the container disk.
4. **Instant Hypervisor Lifecycle**: If the browser state gets dirty or enters a captcha loop, nuke and reset the environment in <250ms via `scripts/microvm.sh`.

---

## Architecture

```
                               ┌─────────────────────────────┐
                               │   Browser Use Agent (Python)│
                               │   • DOM tree indexing       │
                               │   • Set-of-Marks prompting  │
                               └──────────────┬──────────────┘
                                              │ Remote CDP (port 9222+i)
                                              ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                          Agent Computer Workstation                         │
│                                                                             │
│  [Screen 0] DISPLAY=:99      Chrome Headed (CDP :9222)   noVNC :6080        │
│  [Screen 1] DISPLAY=:100     Chrome Headed (CDP :9223)   noVNC :6081        │
│  [Screen 2] DISPLAY=:101     Chrome Headed (CDP :9224)   noVNC :6082        │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## Installation

```bash
# In your virtual environment or project root:
uv pip install browser-use
# or
pip install browser-use
```

---

## Python API Usage

```python
from integrations.browser_use import AgentComputerBrowserAdapter
from browser_use import Agent

# Lease screen 0 and attach via CDP
with AgentComputerBrowserAdapter(screen_id=0) as adapter:
    print(f"Watch live at: {adapter.novnc_url}")
    
    # Create browser connected to Agent Computer CDP
    browser = adapter.create_browser()
    
    # Run task
    agent = Agent(
        task="Find the latest release notes for Rust 1.85",
        browser=browser,
    )
    await agent.run()
```

---

## CLI Usage

```bash
python3 scripts/reach_browser_use.py \
  --task "Search Hacker News for AI Agent benchmarks" \
  --screen 0
```
