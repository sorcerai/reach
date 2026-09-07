use super::serve::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use reach_cli::{
    docker::{ExecOutput, screen_cdp_port},
    lease::LeaseGrant,
    profile::CookieJarService,
};
use serde_json::{Value, json};
use std::sync::Arc;

const CURRENT_BROWSER_ORIGIN_SCRIPT: &str = concat!(
    include_str!("../../assets/browser_page.py"),
    r#"
import json
import sys

def reject():
    print(json.dumps({"status": "error"}))
    raise SystemExit(0)

try:
    payload = json.load(sys.stdin)
    cdp_port = payload.get("cdp_port")
    if isinstance(cdp_port, bool):
        reject()
    cdp_port = int(cdp_port)
    if not 1 <= cdp_port <= 65535:
        reject()
    from playwright.sync_api import sync_playwright
except Exception:
    reject()

try:
    with sync_playwright() as playwright:
        browser = playwright.chromium.connect_over_cdp(
            f"http://127.0.0.1:{cdp_port}", timeout=3000
        )
        page = current_page(browser)
        if not page.url or page.url == "about:blank":
            reject()
        current_origin = page.evaluate("() => window.location.origin")
        if (
            not isinstance(current_origin, str)
            or not current_origin
            or current_origin == "null"
        ):
            reject()
        print(json.dumps({"status": "ok", "origin": current_origin}))
except SystemExit:
    raise
except Exception:
    reject()
"#
);

fn account_ui_mutation(tool: &str, args: &Value) -> bool {
    matches!(tool, "click" | "type" | "key")
        || (tool == "page_text"
            && args
                .get("url")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty))
}

fn parse_current_browser_origin(output: &ExecOutput) -> Option<String> {
    output.stdout.lines().rev().find_map(|line| {
        let value: Value = serde_json::from_str(line.trim()).ok()?;
        (value.get("status").and_then(Value::as_str) == Some("ok"))
            .then(|| value.get("origin").and_then(Value::as_str))
            .flatten()
            .filter(|origin| !origin.is_empty())
            .map(str::to_owned)
    })
}

async fn current_browser_origin(
    state: &AppState,
    target: &str,
    screen: u32,
) -> Result<String, Rejection> {
    let cdp_port = screen_cdp_port(screen).map_err(|_| {
        reject(
            StatusCode::SERVICE_UNAVAILABLE,
            "browser_origin_unavailable",
        )
    })?;
    let payload = serde_json::to_vec(&json!({"cdp_port": cdp_port})).map_err(|_| {
        reject(
            StatusCode::SERVICE_UNAVAILABLE,
            "browser_origin_unavailable",
        )
    })?;
    let output = state
        .docker
        .exec_input(
            target,
            &[
                "timeout".into(),
                "--kill-after=1".into(),
                "5".into(),
                "python3".into(),
                "-c".into(),
                CURRENT_BROWSER_ORIGIN_SCRIPT.into(),
            ],
            &payload,
        )
        .await
        .map_err(|_| {
            reject(
                StatusCode::SERVICE_UNAVAILABLE,
                "browser_origin_unavailable",
            )
        })?;
    if output.exit_code != 0 {
        return Err(reject(
            StatusCode::SERVICE_UNAVAILABLE,
            "browser_origin_unavailable",
        ));
    }
    parse_current_browser_origin(&output).ok_or_else(|| {
        reject(
            StatusCode::SERVICE_UNAVAILABLE,
            "browser_origin_unavailable",
        )
    })
}

async fn require_account_browser_origin(
    state: &AppState,
    target: &str,
    screen: u32,
    grant: &LeaseGrant,
    tool: &str,
    args: &Value,
) -> Result<(), Rejection> {
    if grant.account.is_none() || !account_ui_mutation(tool, args) {
        return Ok(());
    }
    if grant.origins.is_empty() {
        return Err(reject(
            StatusCode::FORBIDDEN,
            "account lease has no allowed origins",
        ));
    }
    let current = current_browser_origin(state, target, screen).await?;
    if !grant.permits_origin(&current) {
        return Err(reject(
            StatusCode::FORBIDDEN,
            "browser origin is outside this lease's allowed origins",
        ));
    }
    Ok(())
}

pub type Rejection = (StatusCode, Json<Value>);
pub fn reject(status: StatusCode, error: impl ToString) -> Rejection {
    (status, Json(json!({"error": error.to_string()})))
}

pub fn supervisor(state: &AppState, headers: &HeaderMap) -> Result<(), Rejection> {
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    if state
        .auth_token
        .as_deref()
        .is_none_or(|expected| bearer != Some(expected))
    {
        return Err(reject(
            StatusCode::FORBIDDEN,
            "configured supervisor bearer required",
        ));
    }
    Ok(())
}

pub async fn approve(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Json<Value>, Rejection> {
    supervisor(&state, &headers)?;
    let digest = body
        .get("digest")
        .and_then(Value::as_str)
        .ok_or_else(|| reject(StatusCode::BAD_REQUEST, "digest required"))?;
    state
        .agent
        .approve_action(id, digest)
        .map_err(|e| reject(StatusCode::CONFLICT, e))?;
    Ok(Json(json!({"status":"approved", "digest":digest})))
}
pub async fn pending(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
    headers: HeaderMap,
) -> Result<Json<Value>, Rejection> {
    supervisor(&state, &headers)?;
    let screen = state
        .agent
        .screen_info(id)
        .ok_or_else(|| reject(StatusCode::NOT_FOUND, "screen_not_found"))?;
    let approval = screen
        .approval
        .filter(|a| a.current())
        .ok_or_else(|| reject(StatusCode::NOT_FOUND, "no_pending_action"))?;
    Ok(Json(
        json!({"digest":approval.digest,"tool":approval.tool,"arguments":approval.arguments,"observation_gen":screen.observation_gen}),
    ))
}

pub async fn prepare(
    state: &AppState,
    headers: &HeaderMap,
    screen: u32,
    target: &str,
    tool: &str,
    args: &mut Value,
) -> Result<Option<LeaseGrant>, Rejection> {
    let grant = state.agent.screen_info(screen).and_then(|s| s.grant);
    if let Some(grant) = &grant {
        grant
            .authorize(tool, args)
            .map_err(|e| reject(StatusCode::FORBIDDEN, e))?;
        let incarnation = state
            .docker
            .incarnation(target)
            .await
            .map_err(|_| reject(StatusCode::SERVICE_UNAVAILABLE, "computer_unavailable"))?;
        if incarnation != grant.incarnation {
            return Err(reject(StatusCode::CONFLICT, "stale_computer_incarnation"));
        }
        require_account_browser_origin(state, target, screen, grant, tool, args).await?;
        let observation = headers
            .get("x-observation-gen")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok());
        if let Some(digest) = state
            .agent
            .authorize_action(screen, observation, tool, args)
            .map_err(|e| reject(StatusCode::CONFLICT, e))?
        {
            return Err((
                StatusCode::PRECONDITION_REQUIRED,
                Json(json!({"error":"approval_required","digest":digest})),
            ));
        }
    } else if tool == "inject" {
        return Err(reject(
            StatusCode::FORBIDDEN,
            "secret injection requires an account lease",
        ));
    }
    if (!matches!(tool, "screenshot" | "page_text" | "click" | "type" | "key"))
        || args
            .get("url")
            .and_then(Value::as_str)
            .is_some_and(|url| !url.is_empty())
    {
        if let Ok(incarnation) = state.docker.incarnation(target).await {
            reach_cli::refs::global_ref_table().clear_screen(&incarnation, screen);
        }
        state.agent.invalidate_observation(screen);
    }
    Ok(grant)
}

pub fn cookies(grant: Option<&LeaseGrant>) -> Option<CookieJarService> {
    grant
        .and_then(|g| g.jars_path.clone())
        .map(CookieJarService::new)
}
