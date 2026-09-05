// Reach noVNC floating overlay banner
// Displays "Agent is waiting: <reason>" with "[ Hand Back to Agent ]" button.
(function() {
  if (document.getElementById("reach-takeover-banner")) {
    return;
  }

  const style = document.createElement("style");
  style.id = "reach-banner-style";
  style.textContent = `
    #reach-takeover-banner {
      position: fixed;
      top: 0;
      left: 0;
      right: 0;
      z-index: 999999;
      background: rgba(24, 24, 37, 0.95);
      backdrop-filter: blur(8px);
      color: #cdd6f4;
      border-bottom: 2px solid #fab387;
      padding: 10px 24px;
      display: flex;
      justify-content: space-between;
      align-items: center;
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
      box-shadow: 0 4px 20px rgba(0, 0, 0, 0.5);
    }
    .reach-banner-content {
      display: flex;
      align-items: center;
      gap: 12px;
      font-size: 14px;
      font-weight: 500;
    }
    .reach-pulse-dot {
      width: 10px;
      height: 10px;
      border-radius: 50%;
      background: #fab387;
      box-shadow: 0 0 10px #fab387;
      display: inline-block;
      animation: reach-pulse 1.5s infinite;
    }
    @keyframes reach-pulse {
      0% { transform: scale(0.95); opacity: 0.8; }
      50% { transform: scale(1.2); opacity: 1; }
      100% { transform: scale(0.95); opacity: 0.8; }
    }
    #reach-takeover-reason {
      font-weight: 700;
      color: #fab387;
    }
    .reach-handback-btn {
      background: #89b4fa;
      color: #11111b;
      border: none;
      outline: none;
      font-weight: 600;
      font-size: 13px;
      padding: 8px 18px;
      border-radius: 8px;
      cursor: pointer;
      transition: all 0.2s ease;
      box-shadow: 0 2px 8px rgba(137, 180, 250, 0.3);
    }
    .reach-handback-btn:hover:not(:disabled) {
      background: #b4befe;
      transform: translateY(-1px);
      box-shadow: 0 4px 12px rgba(137, 180, 250, 0.4);
    }
    .reach-handback-btn:disabled {
      opacity: 0.7;
      cursor: not-allowed;
    }
    .reach-handback-btn.success {
      background: #a6e3a1;
      color: #11111b;
    }
  `;
  document.head.appendChild(style);

  const banner = document.createElement("div");
  banner.id = "reach-takeover-banner";
  banner.innerHTML = `
    <div class="reach-banner-content">
      <span class="reach-pulse-dot"></span>
      <span class="reach-banner-msg">Agent is waiting: <span id="reach-takeover-reason">takeover requested</span></span>
    </div>
    <button id="reach-handback-btn" class="reach-handback-btn" type="button">Hand Back to Agent</button>
  `;

  document.body.appendChild(banner);

  const params = new URLSearchParams(window.location.search);
  const token = params.get("token") || "";
  let screenId = 0;
  if (params.has("screen")) {
    screenId = parseInt(params.get("screen"), 10) || 0;
  } else {
    const port = parseInt(window.location.port, 10);
    if (port >= 6080 && port < 6100) {
      screenId = port - 6080;
    }
  }

  const apiBase = params.get("api")
    ? params.get("api").replace(/\/$/, "")
    : `${window.location.protocol}//${window.location.hostname}:4200`;

  const reasonEl = document.getElementById("reach-takeover-reason");
  const btn = document.getElementById("reach-handback-btn");

  if (params.get("reason")) {
    reasonEl.textContent = params.get("reason");
    banner.style.display = "flex";
  }

  let notifiedConnected = false;

  async function checkTakeoverStatus() {
    try {
      const url = token
        ? `${apiBase}/agent/screens?token=${encodeURIComponent(token)}`
        : `${apiBase}/agent/screens`;
      const headers = token ? { "X-Human-Token": token } : {};
      const res = await fetch(url, { cache: "no-store", headers });
      if (!res.ok) return;
      const screens = await res.json();
      const current = screens.find(s => s.id === screenId);
      if (!current) return;

      if (current.phase === "HandoffPending" || current.phase === "HumanActive") {
        banner.style.display = "flex";
        if (current.takeover_reason) {
          reasonEl.textContent = current.takeover_reason;
        }
        if (current.phase === "HandoffPending" && !notifiedConnected) {
          notifiedConnected = true;
          const connUrl = token
            ? `${apiBase}/agent/screens/${screenId}/connected?token=${encodeURIComponent(token)}`
            : `${apiBase}/agent/screens/${screenId}/connected`;
          fetch(connUrl, {
            method: "POST",
            headers: token ? { "X-Human-Token": token } : {},
          }).catch(() => {});
        }
      } else {
        banner.style.display = "none";
        notifiedConnected = false;
      }
    } catch (_) {
      // Backend not reached or offline, keep current banner status
    }
  }

  setInterval(checkTakeoverStatus, 1500);
  checkTakeoverStatus();

  btn.addEventListener("click", async () => {
    btn.disabled = true;
    btn.textContent = "Handing back...";
    try {
      const hbUrl = token
        ? `${apiBase}/agent/screens/${screenId}/handback?token=${encodeURIComponent(token)}`
        : `${apiBase}/agent/screens/${screenId}/handback`;
      const hbHeaders = { "Content-Type": "application/json" };
      if (token) {
        hbHeaders["X-Human-Token"] = token;
      }
      const res = await fetch(hbUrl, {
        method: "POST",
        headers: hbHeaders,
      });
      if (res.ok) {
        btn.textContent = "Handed back \u2713";
        btn.classList.add("success");
        reasonEl.textContent = "Control returned to agent";
        setTimeout(() => {
          banner.style.display = "none";
          btn.disabled = false;
          btn.classList.remove("success");
          btn.textContent = "Hand Back to Agent";
        }, 1500);
      } else {
        throw new Error(`HTTP ${res.status}`);
      }
    } catch (e) {
      btn.disabled = false;
      btn.textContent = "Hand Back to Agent";
      alert("Failed to hand back to agent: " + e.message);
    }
  });
})();
