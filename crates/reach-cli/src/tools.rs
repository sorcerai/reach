//! Shared MCP tool dispatcher used by both `serve` (SSE/HTTP) and `connect` (stdio).

use crate::docker::{
    AuthHandoffOptions, DockerClient, PageTextOptions, ProfileMount, Sandbox, novnc_url,
};
use crate::mcp::ToolResponse;

pub struct ToolContext<'a> {
    pub docker: &'a DockerClient,
    pub public_host: String,
    pub agent: Option<&'a crate::agent::AgentState>,
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

pub fn browse_command(url: &str, profile_dir: &str) -> String {
    format!(
        "mkdir -p '{p}' && \
         python3 -c \"import os, json, time; \
           p = '{p}'; \
           sf = '/workspace/.reach/state.json'; \
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

pub async fn dispatch(
    ctx: &ToolContext<'_>,
    tool: &str,
    args: &serde_json::Value,
    target: &str,
) -> ToolResponse {
    let screen = screen_for(args);
    let display = display_for(screen);

    match tool {
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
            let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
            let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
            let btn = match args.get("button").and_then(|v| v.as_str()) {
                Some("right") => "3",
                Some("middle") => "2",
                _ => "1",
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
            sh(
                ctx,
                target,
                screen,
                &format!("xdotool type -- '{}'", text.replace('\'', "'\\''")),
            )
            .await
        }
        "key" => {
            let combo = args
                .get("combo")
                .and_then(|v| v.as_str())
                .unwrap_or("Return");
            sh(ctx, target, screen, &format!("xdotool key {combo}")).await
        }
        "browse" => {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("about:blank");
            let profile = match args.get("use_profile").and_then(|v| v.as_str()) {
                Some(p) => p.to_string(),
                None => {
                    if screen > 0 {
                        format!("default-screen{screen}")
                    } else {
                        "default".to_string()
                    }
                }
            };
            sh(
                ctx,
                target,
                screen,
                &browse_command(url, &ProfileMount::container_path_for(&profile)),
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
            let f = if stealth {
                "StealthyFetcher"
            } else {
                "Fetcher"
            };
            py(
                ctx,
                target,
                screen,
                &format!(
                    "from scrapling import {f}; r = {f}().get('{url}'); \
                     elems = r.css('{sel}'); \
                     import json; print(json.dumps([{{'content': e.text, 'tag': e.tag}} for e in elems]))"
                ),
            )
            .await
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
            sh(ctx, target, screen, cmd).await
        }
        "page_text" => {
            let url = match args.get("url").and_then(|v| v.as_str()) {
                Some(u) if !u.is_empty() => u.to_string(),
                _ => return ToolResponse::error("page_text: missing required `url`"),
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
                timeout_ms: args
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30_000),
                user_data_dir: Some(ProfileMount::container_path_for(
                    args.get("use_profile")
                        .and_then(|v| v.as_str())
                        .unwrap_or("default"),
                )),
                display: Some(display.clone()),
            };
            match ctx.docker.page_text(target, &opts).await {
                Ok(out) => match serde_json::to_string_pretty(&out) {
                    Ok(s) => ToolResponse::text(s),
                    Err(e) => ToolResponse::error(e.to_string()),
                },
                Err(e) => ToolResponse::error(e.to_string()),
            }
        }
        "auth_handoff" => {
            let url = match args.get("url").and_then(|v| v.as_str()) {
                Some(u) if !u.is_empty() => u.to_string(),
                _ => return ToolResponse::error("auth_handoff: missing required `url`"),
            };
            let opts = AuthHandoffOptions {
                url,
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
                user_data_dir: Some(ProfileMount::container_path_for(
                    args.get("use_profile")
                        .and_then(|v| v.as_str())
                        .unwrap_or("default"),
                )),
                display: Some(display.clone()),
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
                            agent.set_takeover(screen, true, Some(vnc.clone()));
                        } else if out.status == "authenticated" {
                            agent.set_takeover(screen, false, None);
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
                let busy = ctx
                    .docker
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
                    .unwrap_or(false);
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
    }
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
}
