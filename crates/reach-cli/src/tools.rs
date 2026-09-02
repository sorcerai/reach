//! Shared MCP tool dispatcher used by both `serve` (SSE/HTTP) and `connect` (stdio).

use crate::docker::{
    AuthHandoffOptions, DockerClient, PageTextOptions, ProfileMount, Sandbox, novnc_url,
};
use crate::mcp::ToolResponse;

pub struct ToolContext<'a> {
    pub docker: &'a DockerClient,
    pub public_host: String,
}

/// Resolve the noVNC URL for a sandbox using the configured public host.
pub fn novnc_url_for(ctx: &ToolContext<'_>, sandbox: &Sandbox) -> String {
    let port = sandbox.ports.novnc.unwrap_or(6080);
    novnc_url(&ctx.public_host, port)
}

/// Validate the `screen` argument for `live_view`. Only screen 0 exists
/// today (single-display sandbox); anything else is a caller error.
pub fn requested_screen(args: &serde_json::Value) -> Result<u32, String> {
    match args.get("screen").and_then(|v| v.as_u64()) {
        None | Some(0) => Ok(0),
        Some(n) => Err(format!(
            "screen {n} is not available: this sandbox has a single screen (0)"
        )),
    }
}

pub fn browse_command(url: &str, profile_dir: &str) -> String {
    format!(
        "mkdir -p '{p}' && reach-chrome --no-sandbox --disable-gpu --no-first-run \
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
    match tool {
        "screenshot" => match ctx.docker.screenshot(target).await {
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
                &format!("xdotool mousemove {x} {y} click {btn}"),
            )
            .await
        }
        "type" => {
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            sh(
                ctx,
                target,
                &format!("xdotool type -- '{}'", text.replace('\'', "'\\''")),
            )
            .await
        }
        "key" => {
            let combo = args
                .get("combo")
                .and_then(|v| v.as_str())
                .unwrap_or("Return");
            sh(ctx, target, &format!("xdotool key {combo}")).await
        }
        "browse" => {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("about:blank");
            let profile = args
                .get("use_profile")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            sh(
                ctx,
                target,
                &browse_command(url, &ProfileMount::container_path_for(profile)),
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
            py(ctx, target, script).await
        }
        "exec" => {
            let cmd = args
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("echo");
            sh(ctx, target, cmd).await
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
            };

            // Resolve the noVNC URL up-front so we can include it in the
            // response no matter which branch the helper takes.
            let vnc = match ctx.docker.find(target).await {
                Ok(sandbox) => novnc_url_for(ctx, &sandbox),
                Err(_) => novnc_url(&ctx.public_host, 6080),
            };

            match ctx.docker.auth_handoff(target, &opts).await {
                Ok(out) => {
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
        "live_view" => {
            if let Err(e) = requested_screen(args) {
                return ToolResponse::error(e);
            }
            match ctx.docker.find(target).await {
                Ok(sb) => {
                    let busy = ctx
                        .docker
                        .exec(target, &["xdotool".into(), "getactivewindow".into()])
                        .await
                        .map(|o| o.exit_code == 0)
                        .unwrap_or(false);
                    ToolResponse::text(
                        serde_json::json!({
                            "novnc_url": novnc_url_for(ctx, &sb),
                            "screen": 0,
                            "display": ":99",
                            "busy": busy,
                        })
                        .to_string(),
                    )
                }
                Err(e) => ToolResponse::error(e.to_string()),
            }
        }
        _ => ToolResponse::error(format!("unknown tool: {tool}")),
    }
}

async fn sh(ctx: &ToolContext<'_>, target: &str, cmd: &str) -> ToolResponse {
    match ctx
        .docker
        .exec(target, &["bash".into(), "-c".into(), cmd.into()])
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

async fn py(ctx: &ToolContext<'_>, target: &str, script: &str) -> ToolResponse {
    match ctx
        .docker
        .exec(target, &["python3".into(), "-c".into(), script.into()])
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
    fn requested_screen_rejects_nonzero() {
        assert_eq!(requested_screen(&serde_json::json!({})), Ok(0));
        assert_eq!(requested_screen(&serde_json::json!({"screen": 0})), Ok(0));
        assert!(requested_screen(&serde_json::json!({"screen": 1})).is_err());
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
