#![allow(clippy::collapsible_if, clippy::needless_return)]

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use clap::Args;
use reach_cli::agent::{AgentState, ScreenInfoResponse};
use reach_cli::config::ReachConfig;
use reach_cli::mcp::{
    JsonRpcRequest, JsonRpcResponse, McpInitializeResult, ToolResponse, tool_definitions,
};
use reach_cli::runtime::RuntimeClient;
use reach_cli::tools::{ToolContext, dispatch};
use std::convert::Infallible;
use std::sync::Arc;

#[derive(Args, Clone, Debug)]
pub struct ServeArgs {
    /// Port for the MCP SSE server
    #[arg(long, default_value = "4200")]
    pub port: u16,

    /// Bind address
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Target sandbox (default: first running)
    #[arg(long)]
    pub sandbox: Option<String>,

    /// Host/IP humans should use to reach noVNC (overrides config)
    #[arg(long)]
    pub public_host: Option<String>,

    /// Bearer authentication token for API routes (or set REACH_AUTH_TOKEN env var)
    #[arg(long)]
    pub auth_token: Option<String>,
}
pub struct AppState {
    pub runtime: Arc<RuntimeClient>,
    pub default_sandbox: Option<String>,
    pub public_host: String,
    pub bind_host: String,
    pub auth_token: Option<String>,
    pub agent: Arc<AgentState>,
    pub profile_broker: Arc<reach_cli::profile::ProfileBroker>,
    pub cookie_jars: Arc<reach_cli::profile::CookieJarService>,
    pub accounts: std::collections::BTreeMap<String, reach_cli::lease::AccountPolicy>,
    pub viewer_sessions: super::viewer::ViewerSessions,
    pub raw_viewer_token: String,
    pub viewer_token_target: tokio::sync::Mutex<Option<String>>,
}

impl AppState {
    pub async fn ensure_viewer_token(&self, target: &str) -> anyhow::Result<()> {
        let mut provisioned = self.viewer_token_target.lock().await;
        if provisioned.as_deref() == Some(target) {
            return Ok(());
        }
        let helper = "import os,sys\nos.makedirs('/run/reach',mode=0o700,exist_ok=True)\nfd=os.open('/run/reach/viewer-token',os.O_WRONLY|os.O_CREAT|os.O_TRUNC|os.O_NOFOLLOW,0o600)\nos.fchmod(fd,0o600)\nwith os.fdopen(fd,'wb') as f: f.write(sys.stdin.buffer.read())";
        let receipt = self
            .runtime
            .exec_input(
                target,
                &["python3".into(), "-c".into(), helper.into()],
                self.raw_viewer_token.as_bytes(),
            )
            .await?;
        if receipt.exit_code != 0 {
            anyhow::bail!("failed to initialize private viewer transport");
        }
        *provisioned = Some(target.to_owned());
        Ok(())
    }

    pub fn is_allowed_host(&self, host_header: &str) -> bool {
        let host_candidate = extract_host(host_header);
        if host_candidate.is_empty() {
            return false;
        }

        let candidate_lower = host_candidate.to_ascii_lowercase();
        if candidate_lower == "localhost"
            || candidate_lower == "127.0.0.1"
            || candidate_lower == "::1"
        {
            return true;
        }

        let bind = extract_host(&self.bind_host).to_ascii_lowercase();
        if !bind.is_empty() && bind != "0.0.0.0" && bind != "::" && candidate_lower == bind {
            return true;
        }

        let pub_host = normalize_host_target(&self.public_host).to_ascii_lowercase();
        !pub_host.is_empty() && candidate_lower == pub_host
    }
}
pub fn extract_host(host_header: &str) -> &str {
    let host = host_header.trim();
    if host.starts_with('[') {
        if let Some(end) = host.find(']') {
            &host[1..end]
        } else {
            host
        }
    } else {
        host.split(':').next().unwrap_or(host)
    }
}

pub fn normalize_host_target(target: &str) -> &str {
    let without_scheme = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("https://"))
        .unwrap_or(target);
    extract_host(without_scheme)
}
async fn computer_handler(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({"sandbox":state.default_sandbox}))
}

pub fn build_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/mcp", post(mcp_handler))
        .route("/mcp", get(sse_handler))
        .route("/sse", get(sse_handler))
        .route("/health", get(|| async { "ok" }))
        .route("/agent/computer", get(computer_handler))
        .route("/agent/screens", get(agent_screens_handler))
        .route("/agent/screens/{id}", get(agent_screen_get_handler))
        .route("/agent/screens/{id}/lease", post(agent_lease_handler))
        .route("/agent/screens/{id}/lease", delete(agent_release_handler))
        .route(
            "/agent/screens/{id}/force-release",
            post(agent_force_release_handler),
        )
        .route("/agent/screens/{id}/takeover", post(agent_takeover_handler))
        .route("/agent/screens/{id}/handback", post(agent_handback_handler))
        .route("/agent/screens/{id}/ack", post(agent_ack_handler))
        .route("/agent/screens/{id}/wait", get(agent_wait_handler))
        .route(
            "/agent/screens/{id}/approval",
            get(super::security::pending).post(super::security::approve),
        )
        .merge(super::viewer::router())
        .route(
            "/agent/screens/{id}/connected",
            post(agent_connected_handler),
        )
        .route("/tools", get(tools_list_handler).post(tools_post_handler))
        .route("/tools/{tool}", post(tool_call_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            host_validation_middleware,
        ))
        .with_state(state)
}

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    let port = args.port;
    let host = args.host.clone();

    let cfg = ReachConfig::load()?;
    let public_host = args
        .public_host
        .unwrap_or_else(|| cfg.server.effective_public_host());

    let runtime = Arc::new(RuntimeClient::from_config(&cfg)?);
    let default_sandbox = match resolve_sandbox_name(&runtime, args.sandbox.as_deref()).await {
        Ok(name) => Some(name),
        Err(error)
            if matches!(
                &cfg.runtime.backend,
                reach_cli::config::RuntimeBackend::Docker
            ) =>
        {
            tracing::warn!(error = %error, "no default sandbox available");
            None
        }
        Err(error) => return Err(error),
    };
    let screens_count = match default_sandbox.as_deref() {
        Some(name) => match runtime.find(name).await {
            Ok(sb) => sb.ports.screens.max(1),
            Err(_) => 1,
        },
        None => 1,
    };
    let agent = Arc::new(AgentState::new(screens_count));
    let auth_token = args
        .auth_token
        .or_else(|| std::env::var("REACH_AUTH_TOKEN").ok())
        .filter(|t| !t.trim().is_empty());
    if auth_token.is_none() {
        anyhow::bail!("serving requires a configured supervisor authentication token");
    }

    let raw_viewer_token = uuid::Uuid::new_v4().to_string();
    let profile_broker = Arc::new(reach_cli::profile::ProfileBroker::default_broker()?);
    let cookie_jars = Arc::new(reach_cli::profile::CookieJarService::default_service());
    let state = Arc::new(AppState {
        runtime,
        default_sandbox,
        public_host,
        bind_host: host.clone(),
        auth_token,
        agent,
        profile_broker,
        cookie_jars,
        accounts: cfg.accounts,
        viewer_sessions: super::viewer::ViewerSessions::default(),
        raw_viewer_token,
        viewer_token_target: tokio::sync::Mutex::new(None),
    });

    println!("Live view host: {}", state.public_host);

    let app = build_app(state);

    let addr = format!("{host}:{port}");
    println!("reach MCP server listening on {addr}");
    println!("Connect: claude mcp add reach --url http://{addr}/mcp");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn host_validation_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let host_header = match req.headers().get(axum::http::header::HOST) {
        Some(h) => match h.to_str() {
            Ok(s) => s,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "invalid Host header encoding" })),
                )
                    .into_response();
            }
        },
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "missing Host header" })),
            )
                .into_response();
        }
    };

    if !state.is_allowed_host(host_header) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "rejected Host header to protect against DNS rebinding"
            })),
        )
            .into_response();
    }

    next.run(req).await
}

fn extract_query_token(query: &str) -> Option<&str> {
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if let (Some("token"), Some(val)) = (parts.next(), parts.next()) {
            return Some(val);
        }
    }
    None
}

fn parse_profile_lock_error(resp: &ToolResponse) -> Option<serde_json::Value> {
    if !resp.is_error {
        return None;
    }
    let text = match resp.content.first()? {
        reach_cli::mcp::ContentBlock::Text { text } => text,
        _ => return None,
    };
    let val = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let err_kind = val.get("error")?.as_str()?;
    if matches!(
        err_kind,
        "profile_locked" | "profile_lock_timeout" | "profile_lock_io_error"
    ) {
        Some(val)
    } else {
        None
    }
}

fn parse_screen_id_from_path(path: &str) -> Option<u32> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() >= 3 && parts[0] == "agent" && parts[1] == "screens" {
        parts[2].parse::<u32>().ok()
    } else {
        None
    }
}

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(token) = req.headers().get("x-lease-token") {
        let screen = token
            .to_str()
            .ok()
            .and_then(|t| state.agent.screen_for_lease(t));
        let path = req.uri().path();
        let allowed = screen.is_some_and(|id| {
            let prefix = format!("/agent/screens/{id}");
            path == "/tools"
                || path.starts_with("/tools/")
                || path == "/mcp"
                || path == "/sse"
                || path == "/agent/screens"
                || path == prefix
                || path == format!("{prefix}/takeover")
                || path == format!("{prefix}/ack")
                || path == format!("{prefix}/wait")
                || (path == format!("{prefix}/lease") && req.method() == axum::http::Method::DELETE)
        });
        if !allowed {
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "invalid or out-of-scope lease capability"
                })),
            )
                .into_response();
        }
        return next.run(req).await;
    }
    if let Some(expected_token) = &state.auth_token {
        let path = req.uri().path();
        if path.starts_with("/agent")
            || path.starts_with("/tools")
            || path.starts_with("/mcp")
            || path.starts_with("/sse")
        {
            let auth_header = req.headers().get(axum::http::header::AUTHORIZATION);
            let bearer_token = auth_header
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.strip_prefix("Bearer "))
                .map(|t| t.trim());

            let query_token = req
                .uri()
                .query()
                .and_then(extract_query_token)
                .map(|t| t.trim());

            let is_authorized = bearer_token == Some(expected_token.as_str())
                || ((path.starts_with("/sse") || path.starts_with("/mcp"))
                    && query_token == Some(expected_token.as_str()));

            if is_authorized {
                return next.run(req).await;
            }

            // Human token bypass for takeover/banner endpoints:
            let human_token_header = req
                .headers()
                .get("x-human-token")
                .and_then(|h| h.to_str().ok())
                .map(|t| t.trim());
            let human_token_candidate = query_token.or(human_token_header);

            if path == "/agent/screens" {
                if let Some(tok) = human_token_candidate {
                    if state.agent.has_human_token(tok) {
                        return next.run(req).await;
                    }
                }
            }

            if let Some(sid) = parse_screen_id_from_path(path) {
                if path.ends_with("/connected") || path.ends_with("/handback") {
                    if let Some(tok) = human_token_candidate {
                        if state.agent.verify_human_token(sid, tok) {
                            return next.run(req).await;
                        }
                    }
                    return (
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({
                            "error": "forbidden: invalid or missing human token"
                        })),
                    )
                        .into_response();
                }
            }

            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "unauthorized: valid Bearer token required"
                })),
            )
                .into_response();
        }
    }

    next.run(req).await
}

async fn resolve_sandbox_name(
    runtime: &RuntimeClient,
    requested: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(name) = requested {
        return Ok(name.to_string());
    }
    let sandboxes = runtime.list().await?;
    sandboxes
        .into_iter()
        .find(|s| matches!(s.status, reach_cli::docker::SandboxStatus::Running))
        .map(|s| s.name)
        .ok_or_else(|| anyhow::anyhow!("no running sandbox found"))
}

async fn resolve_sandbox(state: &AppState, requested: Option<&str>) -> anyhow::Result<String> {
    resolve_sandbox_name(
        &state.runtime,
        requested.or(state.default_sandbox.as_deref()),
    )
    .await
}

async fn agent_screens_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<HumanTokenQuery>,
) -> Json<Vec<ScreenInfoResponse>> {
    let sb = match resolve_sandbox(&state, None).await {
        Ok(name) => state.runtime.find(&name).await.ok(),
        Err(_) => None,
    };

    if let Some(ref sb) = sb {
        state.agent.ensure_screens(sb.ports.screens);
    }

    let snapshot = state.agent.snapshot();
    let res = snapshot
        .into_iter()
        .filter(|s| {
            let lease = headers.get("x-lease-token").and_then(|h| h.to_str().ok());
            let human = extract_human_token(&headers, Some(&query));
            lease.is_none_or(|token| s.lease_token.as_deref() == Some(token))
                && human
                    .as_deref()
                    .is_none_or(|token| s.human_token.as_deref() == Some(token))
        })
        .map(|s| ScreenInfoResponse {
            id: s.id,
            owner: s.owner,
            phase: s.phase,
            handoff_gen: s.handoff_gen,
            takeover_pending: s.takeover_pending,
            takeover_reason: s.takeover_reason,
            takeover_url: s.takeover_url,
            leased_at: s.leased_at,
            novnc_url: format!("/viewer/{}", s.id),
            busy: s.busy,
        })
        .collect();

    Json(res)
}

async fn agent_screen_get_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
    headers: HeaderMap,
) -> Result<Json<ScreenInfoResponse>, (StatusCode, Json<serde_json::Value>)> {
    let sb = match resolve_sandbox(&state, None).await {
        Ok(name) => state.runtime.find(&name).await.ok(),
        Err(_) => None,
    };

    if let Some(ref sb) = sb {
        state.agent.ensure_screens(sb.ports.screens);
    }

    if let Some(s) = state.agent.screen_info(id) {
        if let Some(token) = headers.get("x-lease-token").and_then(|v| v.to_str().ok()) {
            if s.lease_token.as_deref() != Some(token) {
                return Err((
                    StatusCode::FORBIDDEN,
                    Json(serde_json::json!({"error": "invalid_lease"})),
                ));
            }
        }
        Ok(Json(ScreenInfoResponse {
            id: s.id,
            owner: s.owner,
            phase: s.phase,
            handoff_gen: s.handoff_gen,
            takeover_pending: s.takeover_pending,
            takeover_reason: s.takeover_reason,
            takeover_url: s.takeover_url,
            leased_at: s.leased_at,
            novnc_url: format!("/viewer/{}", s.id),
            busy: s.busy,
        }))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("screen {id} not found") })),
        ))
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct LeaseRequest {
    pub owner: String,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub attempt_id: Option<String>,
    #[serde(default)]
    pub allow_exec: bool,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct ReleaseRequest {
    #[serde(default = "default_release_owner")]
    pub owner: String,
    #[serde(default)]
    pub token: Option<String>,
}

fn default_release_owner() -> String {
    "default".to_string()
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct TakeoverRequest {
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub pending: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct WaitQuery {
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub timeout: Option<u64>,
}

fn validate_tool_screen(
    state: &AppState,
    screen: u32,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if let Some(token) = headers.get("x-lease-token") {
        if state.default_sandbox.is_none() {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "No sandbox was bound at startup; restart with --sandbox"
                })),
            ));
        }
        if token
            .to_str()
            .ok()
            .and_then(|t| state.agent.screen_for_lease(t))
            != Some(screen)
        {
            return Err((
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "lease capability does not authorize this screen"
                })),
            ));
        }
    }
    let is_leased = state.agent.is_leased(screen);
    if let Some(active_token) = state.agent.lease_token(screen) {
        let provided = headers.get("x-lease-token").and_then(|v| v.to_str().ok());
        if provided != Some(&active_token) {
            return Err((
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "forbidden: invalid or missing X-Lease-Token for leased screen"
                })),
            ));
        }
    }

    if let Some(info) = state.agent.screen_info(screen) {
        let gen_hdr = headers.get("x-handoff-gen").and_then(|v| v.to_str().ok());
        if let Some(raw_gen) = gen_hdr {
            if let Ok(expected_gen) = raw_gen.trim().parse::<u64>() {
                if expected_gen != info.handoff_gen {
                    return Err((
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({
                            "error": "stale_plan",
                            "expected_gen": info.handoff_gen,
                            "provided_gen": expected_gen,
                        })),
                    ));
                }
            } else {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "invalid X-Handoff-Gen header"
                    })),
                ));
            }
        } else if is_leased {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "missing_handoff_gen",
                    "screen": screen,
                    "expected_gen": info.handoff_gen,
                })),
            ));
        }

        if info.phase != reach_cli::agent::ScreenPhase::AgentActive
            && info.phase != reach_cli::agent::ScreenPhase::Idle
        {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "takeover_active",
                    "phase": info.phase,
                    "handoff_gen": info.handoff_gen,
                })),
            ));
        }
    }

    Ok(())
}

async fn agent_force_release_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "));
    if state
        .auth_token
        .as_deref()
        .is_none_or(|expected| bearer != Some(expected))
    {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "configured supervisor bearer required"
            })),
        ));
    }
    state.agent.force_release_screen(id).map_err(|e| {
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;
    Ok(Json(
        serde_json::json!({"status": "ok", "id": id, "released": true}),
    ))
}

async fn agent_lease_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
    Json(body): Json<LeaseRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let mut grant = if let Some(account) = body.account.as_deref() {
        let policy = state
            .accounts
            .get(account)
            .ok_or_else(|| super::security::reject(StatusCode::FORBIDDEN, "account_not_granted"))?;
        reach_cli::lease::LeaseGrant::for_account(account, policy)
            .map_err(|e| super::security::reject(StatusCode::BAD_REQUEST, e))?
    } else {
        reach_cli::lease::LeaseGrant::clean()
    };
    for (input, output) in [
        (&body.task_id, &mut grant.task_id),
        (&body.attempt_id, &mut grant.attempt_id),
    ] {
        if let Some(value) = input {
            if !reach_cli::lease::valid_name(value) {
                return Err(super::security::reject(
                    StatusCode::BAD_REQUEST,
                    "invalid task or attempt identifier",
                ));
            }
            *output = value.clone();
        }
    }

    // Reserve the screen before any runtime call. Duplicate or conflicting
    // admissions therefore fail closed without resetting the guest.
    let mut permit = state
        .agent
        .begin_tool_owned(id, None, state.agent.handoff_gen(id))
        .map_err(|e| super::security::reject(StatusCode::CONFLICT, e))?;

    let target = state.default_sandbox.as_deref().ok_or_else(|| {
        super::security::reject(StatusCode::SERVICE_UNAVAILABLE, "no_bound_computer")
    })?;
    let sandbox = state.runtime.find(target).await.map_err(|_| {
        super::security::reject(StatusCode::SERVICE_UNAVAILABLE, "computer_unavailable")
    })?;
    let target_id = sandbox.container_id.clone();
    grant.incarnation = state.runtime.incarnation(&target_id).await.map_err(|_| {
        super::security::reject(StatusCode::SERVICE_UNAVAILABLE, "computer_unavailable")
    })?;
    if grant.account.is_some() && sandbox.allow_exec {
        return Err(super::security::reject(
            StatusCode::FORBIDDEN,
            "account_requires_non_code_computer",
        ));
    }
    if body.allow_exec && (!sandbox.allow_exec || grant.account.is_some()) {
        return Err(super::security::reject(
            StatusCode::FORBIDDEN,
            "execution_not_granted",
        ));
    }
    grant.allow_exec = body.allow_exec;

    // Keep admission until reset completes. A canceled request drops the
    // task's owned receipt, rolling back the unpublished grant only after
    // the guest operation has finished.
    let lease = permit.allocate(&body.owner, grant).map_err(|e| {
        (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;
    let runtime = Arc::clone(&state.runtime);
    let reset_task = tokio::spawn(async move {
        runtime.reset_screen(&target_id, id).await.map_err(|_| ())?;
        Ok::<_, ()>((lease, permit))
    });
    match reset_task.await {
        Ok(Ok((lease, mut permit))) => {
            permit.commit().map_err(|_| {
                super::security::reject(StatusCode::SERVICE_UNAVAILABLE, "screen_cleanup_failed")
            })?;
            Ok(Json(serde_json::json!({
                "status": "ok",
                "id": id,
                "owner": body.owner,
                "token": lease.token,
                "handoff_gen": lease.handoff_gen,
            })))
        }
        Ok(Err(())) | Err(_) => Err(super::security::reject(
            StatusCode::SERVICE_UNAVAILABLE,
            "screen_cleanup_failed",
        )),
    }
}

async fn agent_release_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
    headers: HeaderMap,
    body: Option<Json<ReleaseRequest>>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let req = body.map(|b| b.0).unwrap_or_default();
    let owner = if req.owner.is_empty() {
        "default"
    } else {
        &req.owner
    };
    let token_header = headers.get("x-lease-token").and_then(|v| v.to_str().ok());
    let token = token_header.or(req.token.as_deref());

    match state.agent.release_screen(id, owner, token) {
        Ok(()) => Ok(Json(serde_json::json!({
            "status": "ok",
            "id": id,
            "released": true,
        }))),
        Err(e) => {
            let status = match e {
                reach_cli::agent::LeaseError::InvalidToken { .. } => StatusCode::FORBIDDEN,
                reach_cli::agent::LeaseError::NotOwner { .. } => StatusCode::FORBIDDEN,
                reach_cli::agent::LeaseError::NotFound(_) => StatusCode::NOT_FOUND,
                reach_cli::agent::LeaseError::HumanActive { .. }
                | reach_cli::agent::LeaseError::Busy { .. } => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            Err((status, Json(serde_json::json!({ "error": e.to_string() }))))
        }
    }
}

#[derive(Debug, Default, serde::Deserialize)]
pub struct HumanTokenQuery {
    pub token: Option<String>,
}

fn extract_human_token(headers: &HeaderMap, query: Option<&HumanTokenQuery>) -> Option<String> {
    headers
        .get("x-human-token")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .or_else(|| query.and_then(|q| q.token.clone()))
}

fn verify_caller_human_token(
    state: &AppState,
    id: u32,
    headers: &HeaderMap,
    query: Option<&HumanTokenQuery>,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    if let Some(expected_token) = &state.auth_token {
        let auth_header = headers.get(axum::http::header::AUTHORIZATION);
        let bearer = auth_header
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(|t| t.trim());
        if bearer == Some(expected_token.as_str()) {
            return Ok(state.agent.human_token(id).unwrap_or_default());
        }
    }

    let token = extract_human_token(headers, query);
    if let Some(expected) = state.agent.human_token(id) {
        if token.as_deref() == Some(expected.as_str()) {
            return Ok(expected);
        }
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "forbidden: invalid or missing human token"
            })),
        ));
    } else {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "forbidden: no active human takeover session"
            })),
        ));
    }
}

async fn agent_takeover_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
    headers: HeaderMap,
    Json(body): Json<TakeoverRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if let Some(active_token) = state.agent.lease_token(id) {
        let provided = headers.get("x-lease-token").and_then(|v| v.to_str().ok());
        if provided != Some(&active_token) {
            return Err((
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "forbidden: invalid or missing X-Lease-Token for leased screen"
                })),
            ));
        }
    }

    if body.pending == Some(false) {
        match state.agent.cancel_takeover(
            id,
            headers.get("x-lease-token").and_then(|v| v.to_str().ok()),
        ) {
            Ok(screen) => Ok(Json(serde_json::json!({
                "status": "ok",
                "id": id,
                "phase": screen.phase,
                "handoff_gen": screen.handoff_gen,
            }))),
            Err(e) => {
                let status = match e {
                    reach_cli::agent::TakeoverError::NotFound(_) => StatusCode::NOT_FOUND,
                    reach_cli::agent::TakeoverError::InvalidPhase { .. } => StatusCode::CONFLICT,
                    _ => StatusCode::BAD_REQUEST,
                };
                let err_code = match e {
                    reach_cli::agent::TakeoverError::InvalidPhase { .. } => "cannot_eject_human",
                    _ => "cancel_takeover_failed",
                };
                Err((
                    status,
                    Json(serde_json::json!({ "error": err_code, "message": e.to_string() })),
                ))
            }
        }
    } else {
        // Drain / wait for any in-flight busy tools on screen
        if state.agent.is_busy(id) {
            let drained = state
                .agent
                .wait_for_drain(id, std::time::Duration::from_secs(3))
                .await;
            if !drained {
                return Err((
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "screen_busy",
                        "id": id,
                        "message": format!("screen {id} has tools in flight")
                    })),
                ));
            }
        }

        match state.agent.request_takeover(
            id,
            body.reason,
            Some(format!("/viewer/{id}")),
            headers.get("x-lease-token").and_then(|v| v.to_str().ok()),
        ) {
            Ok(screen) => Ok(Json(serde_json::json!({
                "status": "ok",
                "id": id,
                "phase": screen.phase,
                "handoff_gen": screen.handoff_gen,
                "takeover_url": screen.takeover_url,
            }))),
            Err(e) => {
                let status = match e {
                    reach_cli::agent::TakeoverError::NotFound(_) => StatusCode::NOT_FOUND,
                    reach_cli::agent::TakeoverError::InvalidPhase { .. } => StatusCode::CONFLICT,
                    reach_cli::agent::TakeoverError::Busy { .. } => StatusCode::CONFLICT,
                    _ => StatusCode::BAD_REQUEST,
                };
                Err((status, Json(serde_json::json!({ "error": e.to_string() }))))
            }
        }
    }
}

async fn agent_handback_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<HumanTokenQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let token = verify_caller_human_token(&state, id, &headers, Some(&query))?;

    match state.agent.human_handback(id, Some(&token)) {
        Ok(screen) => Ok(Json(serde_json::json!({
            "status": "ok",
            "id": id,
            "phase": screen.phase,
            "handoff_gen": screen.handoff_gen,
        }))),
        Err(e) => {
            let status = match e {
                reach_cli::agent::TakeoverError::NotFound(_) => StatusCode::NOT_FOUND,
                reach_cli::agent::TakeoverError::InvalidPhase { .. } => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            Err((status, Json(serde_json::json!({ "error": e.to_string() }))))
        }
    }
}

async fn agent_ack_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if let Some(active_token) = state.agent.lease_token(id) {
        let provided = headers.get("x-lease-token").and_then(|v| v.to_str().ok());
        if provided != Some(&active_token) {
            return Err((
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "forbidden: invalid or missing X-Lease-Token for leased screen"
                })),
            ));
        }
    }

    match state.agent.agent_ack(
        id,
        headers.get("x-lease-token").and_then(|v| v.to_str().ok()),
    ) {
        Ok(screen) => Ok(Json(serde_json::json!({
            "status": "ok",
            "id": id,
            "phase": screen.phase,
            "handoff_gen": screen.handoff_gen,
        }))),
        Err(e) => {
            let status = match e {
                reach_cli::agent::TakeoverError::NotFound(_) => StatusCode::NOT_FOUND,
                reach_cli::agent::TakeoverError::InvalidPhase { .. } => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            Err((status, Json(serde_json::json!({ "error": e.to_string() }))))
        }
    }
}

async fn agent_connected_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
    headers: HeaderMap,
    axum::extract::Query(query): axum::extract::Query<HumanTokenQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let token = verify_caller_human_token(&state, id, &headers, Some(&query))?;

    match state.agent.human_connected(id, Some(&token)) {
        Ok(screen) => Ok(Json(serde_json::json!({
            "status": "ok",
            "id": id,
            "phase": screen.phase,
            "handoff_gen": screen.handoff_gen,
        }))),
        Err(e) => {
            let status = match e {
                reach_cli::agent::TakeoverError::NotFound(_) => StatusCode::NOT_FOUND,
                reach_cli::agent::TakeoverError::InvalidPhase { .. } => StatusCode::CONFLICT,
                _ => StatusCode::BAD_REQUEST,
            };
            Err((status, Json(serde_json::json!({ "error": e.to_string() }))))
        }
    }
}

async fn agent_wait_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
    axum::extract::Query(query): axum::extract::Query<WaitQuery>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let target_phase = match query.phase.as_deref() {
        Some(s) => match s.parse::<reach_cli::agent::ScreenPhase>() {
            Ok(p) => p,
            Err(e) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": e })),
                ));
            }
        },
        None => reach_cli::agent::ScreenPhase::HumanDone,
    };

    let timeout_secs = query.timeout.unwrap_or(600);
    let timeout = std::time::Duration::from_secs(timeout_secs);

    let outcome = state.agent.wait_for_phase(id, target_phase, timeout).await;
    if let Some(token) = headers.get("x-lease-token").and_then(|v| v.to_str().ok()) {
        if state.agent.screen_for_lease(token) != Some(id) {
            return Err((
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({"error": "invalid_lease"})),
            ));
        }
    }
    match outcome {
        Ok(screen) => Ok(Json(serde_json::json!({
            "status": "ok",
            "id": id,
            "phase": screen.phase,
            "handoff_gen": screen.handoff_gen,
        }))),
        Err(reach_cli::agent::WaitError::NotFound(sid)) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("screen {sid} not found") })),
        )),
        Err(reach_cli::agent::WaitError::Timeout {
            id: sid,
            phase,
            handoff_gen,
        }) => Ok(Json(serde_json::json!({
            "status": "timeout",
            "id": sid,
            "phase": phase,
            "handoff_gen": handoff_gen,
        }))),
    }
}

async fn mcp_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> Result<Json<JsonRpcResponse>, (StatusCode, Json<serde_json::Value>)> {
    if req.method == "tools/call" {
        let args = req
            .params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let screen = reach_cli::tools::screen_for(&args);
        validate_tool_screen(&state, screen, &headers)?;
    }

    Ok(Json(handle_mcp(&state, &headers, &req).await))
}

async fn sse_handler() -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = tokio_stream::once(Ok(Event::default().event("endpoint").data("/mcp")));
    Sse::new(stream)
}

async fn tools_list_handler() -> Json<serde_json::Value> {
    let tools = tool_definitions();

    Json(serde_json::json!({ "tools": tools }))
}
fn finish_handoff(
    permit: &reach_cli::agent::BusyGuard<'_>,
    headers: &HeaderMap,
    tool: &str,
    args: &serde_json::Value,
    mut result: ToolResponse,
) -> ToolResponse {
    if tool != "auth_handoff" || result.is_error {
        return result;
    }
    if let Some(reach_cli::mcp::ContentBlock::Text { text }) = result.content.first_mut() {
        if let Ok(mut body) = serde_json::from_str::<serde_json::Value>(text) {
            if body.get("status").and_then(|v| v.as_str()) == Some("auth_required") {
                let url = body
                    .get("vnc_url")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let token = headers.get("x-lease-token").and_then(|v| v.to_str().ok());
                let reason = args
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                match permit.request_takeover(reason, url, token) {
                    Ok(handoff) => {
                        body["vnc_url"] = serde_json::json!(handoff.takeover_url);
                        body["handoff_gen"] = serde_json::json!(handoff.handoff_gen);
                        body["phase"] = serde_json::json!(handoff.phase);
                        *text = body.to_string();
                    }
                    Err(error) => return ToolResponse::error(error.to_string()),
                }
            }
        }
    }
    result
}

async fn tool_call_handler(
    State(state): State<Arc<AppState>>,
    Path(tool): Path<String>,
    headers: HeaderMap,
    Json(mut args): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let screen = reach_cli::tools::screen_for(&args);
    validate_tool_screen(&state, screen, &headers)?;

    let initial_gen = headers
        .get("x-handoff-gen")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .or_else(|| state.agent.handoff_gen(screen));
    let _permit = state
        .agent
        .begin_tool(
            screen,
            headers.get("x-lease-token").and_then(|v| v.to_str().ok()),
            initial_gen,
        )
        .map_err(|error| {
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error": error})),
            )
        })?;

    let sandbox_arg = args.get("sandbox").and_then(|v| v.as_str());
    if headers.contains_key("x-lease-token")
        && sandbox_arg.is_some()
        && sandbox_arg != state.default_sandbox.as_deref()
    {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "sandbox_out_of_scope"})),
        ));
    }
    let target_result = match resolve_sandbox(&state, sandbox_arg).await {
        Ok(name) => state.runtime.find(&name).await,
        Err(error) => Err(error),
    };
    let target = match target_result.as_ref() {
        Ok(sandbox) => sandbox.container_id.as_str(),
        Err(_) => {
            return Err(super::security::reject(
                StatusCode::SERVICE_UNAVAILABLE,
                "computer_unavailable",
            ));
        }
    };
    let grant =
        super::security::prepare(&state, &headers, screen, target, &tool, &mut args).await?;
    let scoped_cookies = super::security::cookies(grant.as_ref());

    let owner = headers
        .get("x-owner")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let ctx = ToolContext {
        runtime: state.runtime.as_ref(),
        public_host: state.public_host.clone(),
        agent: Some(state.agent.as_ref()),
        profile_broker: Some(&state.profile_broker),
        cookie_jars: if grant.is_some() {
            scoped_cookies.as_ref()
        } else {
            Some(&state.cookie_jars)
        },
        owner,
    };
    let result = if tool == "inject" {
        let mut request_args = args.clone();
        request_args.as_object_mut().map(|a| a.remove("screen"));
        match serde_json::from_value::<reach_cli::injection::InjectionRequest>(request_args) {
            Ok(request) => match reach_cli::injection::inject(
                state.runtime.as_ref(),
                target,
                screen,
                grant
                    .as_ref()
                    .expect("injection admission requires a grant"),
                &request,
            )
            .await
            {
                Ok(receipt) => ToolResponse::text(serde_json::to_string(&receipt).unwrap()),
                Err(_) => ToolResponse::error(
                    "{\"status\":\"uncertain\",\"error\":\"injection_failed_requires_reconciliation\"}",
                ),
            },
            Err(_) => {
                return Err(super::security::reject(
                    StatusCode::BAD_REQUEST,
                    "invalid injection request",
                ));
            }
        }
    } else {
        dispatch(&ctx, &tool, &args, target).await
    };

    // Check if phase transitioned to HumanActive or handoff generation changed during execution (reach-pkm)
    if let Some(post_info) = state.agent.screen_info(screen) {
        if post_info.phase == reach_cli::agent::ScreenPhase::HumanActive
            || (post_info.phase != reach_cli::agent::ScreenPhase::AgentActive
                && post_info.phase != reach_cli::agent::ScreenPhase::Idle)
            || (initial_gen.is_some() && Some(post_info.handoff_gen) != initial_gen)
        {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "executed_during_takeover",
                    "phase": post_info.phase,
                    "handoff_gen": post_info.handoff_gen,
                })),
            ));
        }
    }

    if let Some(val) = parse_profile_lock_error(&result) {
        match val.get("error").and_then(|v| v.as_str()) {
            Some("profile_locked") | Some("profile_lock_timeout") => {
                return Err((StatusCode::LOCKED, Json(val)));
            }
            Some("profile_lock_io_error") => {
                return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(val)));
            }
            _ => {}
        }
    }

    let result = finish_handoff(&_permit, &headers, &tool, &args, result);
    let mut value = serde_json::to_value(result).unwrap_or_default();
    if let Some(snapshot) = state.agent.screen_info(screen) {
        value["_meta"] = serde_json::json!({"observation_gen":snapshot.observation_gen,
            "task_id":grant.as_ref().map(|g| &g.task_id), "attempt_id":grant.as_ref().map(|g| &g.attempt_id),
            "incarnation":grant.as_ref().map(|g| &g.incarnation)});
    }
    Ok(Json(value))
}

async fn tools_post_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let tool = body
        .get("name")
        .or_else(|| body.get("tool"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let args = body
        .get("arguments")
        .or_else(|| body.get("args"))
        .cloned()
        .unwrap_or(serde_json::json!({}));

    tool_call_handler(State(state), Path(tool), headers, Json(args)).await
}

async fn handle_mcp(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    req: &JsonRpcRequest,
) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => {
            let init = McpInitializeResult::default();
            JsonRpcResponse::success(req.id.clone(), serde_json::to_value(init).unwrap())
        }
        "tools/list" => {
            let tools = tool_definitions();
            JsonRpcResponse::success(req.id.clone(), serde_json::json!({ "tools": tools }))
        }
        "tools/call" => {
            let tool = req
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args = req
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            match tool_call_handler(
                State(state.clone()),
                Path(tool.to_owned()),
                headers.clone(),
                Json(args),
            )
            .await
            {
                Ok(Json(result)) => JsonRpcResponse::success(req.id.clone(), result),
                Err((status, Json(mut error))) => {
                    error["http_status"] = serde_json::json!(status.as_u16());
                    JsonRpcResponse::success(
                        req.id.clone(),
                        serde_json::to_value(ToolResponse::error(error.to_string())).unwrap(),
                    )
                }
            }
        }
        "notifications/initialized" | "ping" => {
            JsonRpcResponse::success(req.id.clone(), serde_json::json!({}))
        }
        _ => JsonRpcResponse::error(
            req.id.clone(),
            -32601,
            format!("unknown method: {}", req.method),
        ),
    }
}

#[cfg(test)]
#[path = "serve_tests.rs"]
mod tests;
