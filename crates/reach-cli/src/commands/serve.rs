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
use reach_cli::docker::{DockerClient, novnc_url};
use reach_cli::mcp::{
    JsonRpcRequest, JsonRpcResponse, McpInitializeResult, ToolResponse, tool_definitions,
};
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
    pub docker: DockerClient,
    pub default_sandbox: Option<String>,
    pub public_host: String,
    pub bind_host: String,
    pub auth_token: Option<String>,
    pub agent: AgentState,
    pub profile_broker: Arc<reach_cli::profile::ProfileBroker>,
    pub cookie_jars: Arc<reach_cli::profile::CookieJarService>,
}

impl AppState {
    pub fn is_allowed_host(&self, host_header: &str) -> bool {
        let host_candidate = extract_host(host_header);
        if host_candidate.is_empty() {
            return false;
        }

        let candidate_lower = host_candidate.to_ascii_lowercase();

        // 1. Always allow loopback
        if candidate_lower == "localhost"
            || candidate_lower == "127.0.0.1"
            || candidate_lower == "::1"
        {
            return true;
        }

        // 2. Allow configured bind host
        let bind = extract_host(&self.bind_host).to_ascii_lowercase();
        if !bind.is_empty() && bind != "0.0.0.0" && bind != "::" && candidate_lower == bind {
            return true;
        }

        // 3. Allow configured public host
        let pub_host = normalize_host_target(&self.public_host).to_ascii_lowercase();
        if !pub_host.is_empty() && candidate_lower == pub_host {
            return true;
        }

        false
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

pub fn build_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/mcp", post(mcp_handler))
        .route("/mcp", get(sse_handler))
        .route("/sse", get(sse_handler))
        .route("/health", get(|| async { "ok" }))
        .route("/agent/screens", get(agent_screens_handler))
        .route("/agent/screens/{id}", get(agent_screen_get_handler))
        .route("/agent/screens/{id}/lease", post(agent_lease_handler))
        .route("/agent/screens/{id}/lease", delete(agent_release_handler))
        .route("/agent/screens/{id}/takeover", post(agent_takeover_handler))
        .route("/agent/screens/{id}/handback", post(agent_handback_handler))
        .route("/agent/screens/{id}/ack", post(agent_ack_handler))
        .route("/agent/screens/{id}/wait", get(agent_wait_handler))
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

    let cfg = ReachConfig::load();
    let public_host = args
        .public_host
        .unwrap_or_else(|| cfg.server.effective_public_host());

    let docker = DockerClient::new(cfg.docker.socket_path())?;
    let screens_count = match resolve_sandbox_name(&docker, args.sandbox.as_deref()).await {
        Ok(name) => match docker.find(&name).await {
            Ok(sb) => sb.ports.screens.max(1),
            Err(_) => 1,
        },
        Err(_) => 1,
    };
    let agent = AgentState::new(screens_count);
    let auth_token = args
        .auth_token
        .or_else(|| std::env::var("REACH_AUTH_TOKEN").ok())
        .filter(|t| !t.trim().is_empty());

    let profile_broker = Arc::new(reach_cli::profile::ProfileBroker::default_broker());
    let cookie_jars = Arc::new(reach_cli::profile::CookieJarService::default_service());
    let state = Arc::new(AppState {
        docker,
        default_sandbox: args.sandbox,
        public_host,
        bind_host: host.clone(),
        auth_token,
        agent,
        profile_broker,
        cookie_jars,
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
    docker: &DockerClient,
    requested: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(name) = requested {
        return Ok(name.to_string());
    }
    let sandboxes = docker.list().await?;
    sandboxes
        .into_iter()
        .find(|s| matches!(s.status, reach_cli::docker::SandboxStatus::Running))
        .map(|s| s.name)
        .ok_or_else(|| anyhow::anyhow!("no running sandbox found"))
}

async fn resolve_sandbox(state: &AppState, requested: Option<&str>) -> anyhow::Result<String> {
    resolve_sandbox_name(
        &state.docker,
        requested.or(state.default_sandbox.as_deref()),
    )
    .await
}

async fn agent_screens_handler(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<ScreenInfoResponse>> {
    let sb = match resolve_sandbox(&state, None).await {
        Ok(name) => state.docker.find(&name).await.ok(),
        Err(_) => None,
    };

    if let Some(ref sb) = sb {
        state.agent.ensure_screens(sb.ports.screens);
    }

    let novnc_base = sb.as_ref().and_then(|s| s.ports.novnc).unwrap_or(6080);

    let snapshot = state.agent.snapshot();
    let res = snapshot
        .into_iter()
        .map(|s| ScreenInfoResponse {
            id: s.id,
            owner: s.owner,
            phase: s.phase,
            handoff_gen: s.handoff_gen,
            takeover_pending: s.takeover_pending,
            takeover_reason: s.takeover_reason,
            takeover_url: s.takeover_url,
            leased_at: s.leased_at,
            novnc_url: novnc_url(&state.public_host, novnc_base + s.id as u16),
            busy: s.busy,
        })
        .collect();

    Json(res)
}

async fn agent_screen_get_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
) -> Result<Json<ScreenInfoResponse>, (StatusCode, Json<serde_json::Value>)> {
    let sb = match resolve_sandbox(&state, None).await {
        Ok(name) => state.docker.find(&name).await.ok(),
        Err(_) => None,
    };

    if let Some(ref sb) = sb {
        state.agent.ensure_screens(sb.ports.screens);
    }

    let novnc_base = sb.as_ref().and_then(|s| s.ports.novnc).unwrap_or(6080);

    if let Some(s) = state.agent.screen_info(id) {
        Ok(Json(ScreenInfoResponse {
            id: s.id,
            owner: s.owner,
            phase: s.phase,
            handoff_gen: s.handoff_gen,
            takeover_pending: s.takeover_pending,
            takeover_reason: s.takeover_reason,
            takeover_url: s.takeover_url,
            leased_at: s.leased_at,
            novnc_url: novnc_url(&state.public_host, novnc_base + s.id as u16),
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
    pub url: Option<String>,
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

fn is_mutating_tool(tool: &str) -> bool {
    matches!(tool, "click" | "type" | "key" | "browse")
}

fn validate_tool_screen(
    state: &AppState,
    screen: u32,
    headers: &HeaderMap,
    tool: Option<&str>,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
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
        } else if is_leased && tool.is_some_and(is_mutating_tool) {
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

async fn agent_lease_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
    Json(body): Json<LeaseRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    match state.agent.lease_screen(id, &body.owner) {
        Ok(lease) => Ok(Json(serde_json::json!({
            "status": "ok",
            "id": id,
            "owner": body.owner,
            "token": lease.token,
            "handoff_gen": lease.handoff_gen,
        }))),
        Err(e) => Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
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
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    if let Some(expected_token) = &state.auth_token {
        let auth_header = headers.get(axum::http::header::AUTHORIZATION);
        let bearer = auth_header
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(|t| t.trim());
        if bearer == Some(expected_token.as_str()) {
            return Ok(());
        }
    }

    let token = extract_human_token(headers, query);
    if let Some(expected) = state.agent.human_token(id) {
        if token.as_deref() == Some(expected.as_str()) {
            return Ok(());
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
        match state.agent.cancel_takeover(id) {
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

        match state.agent.request_takeover(id, body.reason, body.url) {
            Ok(screen) => Ok(Json(serde_json::json!({
                "status": "ok",
                "id": id,
                "phase": screen.phase,
                "handoff_gen": screen.handoff_gen,
                "human_token": screen.human_token,
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
    verify_caller_human_token(&state, id, &headers, Some(&query))?;

    match state.agent.human_handback(id) {
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

    match state.agent.agent_ack(id) {
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
    verify_caller_human_token(&state, id, &headers, Some(&query))?;

    match state.agent.human_connected(id) {
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

    match state.agent.wait_for_phase(id, target_phase, timeout).await {
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
        let tool_name = req.params.get("name").and_then(|v| v.as_str());
        let args = req.params.get("arguments").cloned().unwrap_or_default();
        let screen = reach_cli::tools::screen_for(&args);
        validate_tool_screen(&state, screen, &headers, tool_name)?;
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

async fn tool_call_handler(
    State(state): State<Arc<AppState>>,
    Path(tool): Path<String>,
    headers: HeaderMap,
    Json(args): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let screen = reach_cli::tools::screen_for(&args);
    validate_tool_screen(&state, screen, &headers, Some(&tool))?;

    let initial_gen = state.agent.handoff_gen(screen);

    let sandbox_arg = args.get("sandbox").and_then(|v| v.as_str());
    let target_result = resolve_sandbox(&state, sandbox_arg).await;
    let target = target_result.as_deref().unwrap_or("");

    let owner = headers
        .get("x-owner")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let ctx = ToolContext {
        docker: &state.docker,
        public_host: state.public_host.clone(),
        agent: Some(&state.agent),
        profile_broker: Some(&state.profile_broker),
        cookie_jars: Some(&state.cookie_jars),
        owner,
    };
    let result = dispatch(&ctx, &tool, &args, target).await;

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

    if let Err(e) = target_result {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ));
    }

    Ok(Json(serde_json::to_value(result).unwrap_or_default()))
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
    state: &AppState,
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
            let args = req.params.get("arguments").cloned().unwrap_or_default();
            let screen = reach_cli::tools::screen_for(&args);
            let initial_gen = state.agent.handoff_gen(screen);
            let sandbox_arg = args.get("sandbox").and_then(|v| v.as_str());

            let target_result = resolve_sandbox(state, sandbox_arg).await;
            let target = target_result.as_deref().unwrap_or("");

            let owner = headers
                .get("x-owner")
                .and_then(|v| v.to_str().ok())
                .map(String::from);

            let ctx = ToolContext {
                docker: &state.docker,
                public_host: state.public_host.clone(),
                agent: Some(&state.agent),
                profile_broker: Some(&state.profile_broker),
                cookie_jars: Some(&state.cookie_jars),
                owner,
            };
            let result = dispatch(&ctx, tool, &args, target).await;

            // Check if phase transitioned to HumanActive or handoff generation changed during execution (reach-pkm)
            if let Some(post_info) = state.agent.screen_info(screen) {
                if post_info.phase == reach_cli::agent::ScreenPhase::HumanActive
                    || (post_info.phase != reach_cli::agent::ScreenPhase::AgentActive
                        && post_info.phase != reach_cli::agent::ScreenPhase::Idle)
                    || (initial_gen.is_some() && Some(post_info.handoff_gen) != initial_gen)
                {
                    let err_val = serde_json::json!({
                        "error": "executed_during_takeover",
                        "phase": post_info.phase,
                        "handoff_gen": post_info.handoff_gen,
                    });
                    return JsonRpcResponse::success(
                        req.id.clone(),
                        serde_json::to_value(ToolResponse::error(err_val.to_string())).unwrap(),
                    );
                }
            }

            if parse_profile_lock_error(&result).is_some() {
                return JsonRpcResponse::success(
                    req.id.clone(),
                    serde_json::to_value(result).unwrap(),
                );
            }

            if let Err(e) = target_result {
                return JsonRpcResponse::success(
                    req.id.clone(),
                    serde_json::to_value(ToolResponse::error(e.to_string())).unwrap(),
                );
            }

            JsonRpcResponse::success(req.id.clone(), serde_json::to_value(result).unwrap())
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
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state(auth_token: Option<&str>) -> Arc<AppState> {
        let docker = DockerClient::new(None).unwrap();
        let agent = AgentState::new(2);
        let profile_broker = Arc::new(reach_cli::profile::ProfileBroker::new(
            std::path::PathBuf::from("/tmp/reach-test-profiles"),
        ));
        let cookie_jars = Arc::new(reach_cli::profile::CookieJarService::new(
            std::path::PathBuf::from("/tmp/reach-test-jars"),
        ));
        Arc::new(AppState {
            docker,
            default_sandbox: None,
            public_host: "127.0.0.1".into(),
            bind_host: "127.0.0.1".into(),
            auth_token: auth_token.map(String::from),
            agent,
            profile_broker,
            cookie_jars,
        })
    }

    #[test]
    fn test_extract_host_and_normalize() {
        assert_eq!(extract_host("localhost:4200"), "localhost");
        assert_eq!(extract_host("127.0.0.1:4200"), "127.0.0.1");
        assert_eq!(extract_host("[::1]:4200"), "::1");
        assert_eq!(extract_host("myhost.com"), "myhost.com");
        assert_eq!(normalize_host_target("http://127.0.0.1:6080"), "127.0.0.1");
        assert_eq!(
            normalize_host_target("https://reach.local:4200"),
            "reach.local"
        );
        assert_eq!(normalize_host_target("reach.internal"), "reach.internal");
    }

    #[test]
    fn test_is_allowed_host() {
        let state = test_state(None);
        assert!(state.is_allowed_host("localhost:4200"));
        assert!(state.is_allowed_host("127.0.0.1:4200"));
        assert!(state.is_allowed_host("[::1]:4200"));
        assert!(!state.is_allowed_host("evil.com:4200"));
        assert!(!state.is_allowed_host("attacker.org"));
        assert!(!state.is_allowed_host(""));
    }

    #[tokio::test]
    async fn test_host_header_middleware() {
        let state = test_state(None);
        let app = build_app(state);

        // Valid localhost
        let req = Request::builder()
            .uri("/health")
            .header("Host", "localhost:4200")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Valid 127.0.0.1
        let req = Request::builder()
            .uri("/health")
            .header("Host", "127.0.0.1:4200")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Missing host header -> 400 Bad Request
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // Untrusted / DNS rebinding host header -> 400 Bad Request
        let req = Request::builder()
            .uri("/health")
            .header("Host", "evil.com")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_bearer_auth_middleware() {
        let state = test_state(Some("secret-pass"));
        let app = build_app(state);

        // Health endpoint is public and does not require auth
        let req = Request::builder()
            .uri("/health")
            .header("Host", "127.0.0.1:4200")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // /agent/screens requires auth: missing header -> 401
        let req = Request::builder()
            .uri("/agent/screens")
            .header("Host", "127.0.0.1:4200")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // /agent/screens: wrong bearer token -> 401
        let req = Request::builder()
            .uri("/agent/screens")
            .header("Host", "127.0.0.1:4200")
            .header("Authorization", "Bearer wrong-pass")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // /agent/screens: valid bearer token -> 200
        let req = Request::builder()
            .uri("/agent/screens")
            .header("Host", "127.0.0.1:4200")
            .header("Authorization", "Bearer secret-pass")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // /tools requires auth: missing header -> 401
        let req = Request::builder()
            .uri("/tools")
            .header("Host", "127.0.0.1:4200")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // /tools: valid bearer token -> 200
        let req = Request::builder()
            .uri("/tools")
            .header("Host", "127.0.0.1:4200")
            .header("Authorization", "Bearer secret-pass")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // /mcp requires auth: missing header -> 401
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // /mcp: wrong bearer token -> 401
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "127.0.0.1:4200")
            .header("Authorization", "Bearer wrong-pass")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // /mcp: valid bearer token -> 200
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "127.0.0.1:4200")
            .header("Authorization", "Bearer secret-pass")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // /sse requires auth: missing header/query -> 401
        let req = Request::builder()
            .uri("/sse")
            .header("Host", "127.0.0.1:4200")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // /sse: wrong bearer token -> 401
        let req = Request::builder()
            .uri("/sse")
            .header("Host", "127.0.0.1:4200")
            .header("Authorization", "Bearer wrong-pass")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // /sse: valid bearer token -> 200
        let req = Request::builder()
            .uri("/sse")
            .header("Host", "127.0.0.1:4200")
            .header("Authorization", "Bearer secret-pass")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // /sse: wrong ?token= query param -> 401
        let req = Request::builder()
            .uri("/sse?token=wrong-pass")
            .header("Host", "127.0.0.1:4200")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // /sse: valid ?token= query param -> 200
        let req = Request::builder()
            .uri("/sse?token=secret-pass")
            .header("Host", "127.0.0.1:4200")
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_screen_takeover_lease_token_enforcement() {
        let state = test_state(None);
        let app = build_app(state.clone());

        // Screen 0 unleased: takeover succeeds without X-Lease-Token
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/takeover")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"pending": true}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Lease screen 0
        let lease = state.agent.lease_screen(0, "user-a").unwrap();

        // Screen 0 leased: takeover without X-Lease-Token -> 403 Forbidden
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/takeover")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"pending": true}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Takeover with wrong X-Lease-Token -> 403 Forbidden
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/takeover")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", "wrong-token")
            .body(Body::from(r#"{"pending": true}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Takeover with valid X-Lease-Token -> 200 OK
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/takeover")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .body(Body::from(r#"{"pending": true}"#))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_mcp_synthetic_tools_lease_token_enforcement() {
        let state = test_state(None);
        let app = build_app(state.clone());

        // Lease screen 0
        let lease = state.agent.lease_screen(0, "worker").unwrap();

        let tool_call_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "click",
                "arguments": {
                    "screen": 0,
                    "x": 100,
                    "y": 100
                }
            }
        });

        // Request without X-Lease-Token -> 403 Forbidden
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&tool_call_body).unwrap()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Request with wrong X-Lease-Token -> 403 Forbidden
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", "incorrect-token")
            .body(Body::from(serde_json::to_vec(&tool_call_body).unwrap()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Request with valid X-Lease-Token and X-Handoff-Gen -> passes lease validation (status 200)
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .header("X-Handoff-Gen", "1")
            .body(Body::from(serde_json::to_vec(&tool_call_body).unwrap()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // Request targeting unleased screen 1 without X-Lease-Token -> passes lease check (status 200)
        let unleased_call_body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "click",
                "arguments": {
                    "screen": 1,
                    "x": 100,
                    "y": 100
                }
            }
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&unleased_call_body).unwrap()))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_http_tools_lease_token_enforcement() {
        let state = test_state(None);
        let app = build_app(state.clone());

        // Lease screen 0
        let lease = state.agent.lease_screen(0, "worker").unwrap();

        // Calling /tools/click for leased screen 0 without token -> 403 Forbidden
        let req = Request::builder()
            .method("POST")
            .uri("/tools/click")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"screen": 0, "x": 50, "y": 50}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Calling /tools/click with wrong token -> 403 Forbidden
        let req = Request::builder()
            .method("POST")
            .uri("/tools/click")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", "wrong-lease-token")
            .body(Body::from(r#"{"screen": 0, "x": 50, "y": 50}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Release screen 0 with wrong token -> 403 Forbidden
        let req = Request::builder()
            .method("DELETE")
            .uri("/agent/screens/0/lease")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", "bad-token")
            .body(Body::from(r#"{"owner": "worker"}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Release screen 0 with correct token -> 200 OK
        let req = Request::builder()
            .method("DELETE")
            .uri("/agent/screens/0/lease")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .body(Body::from(r#"{"owner": "worker"}"#))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_takeover_state_machine_http_endpoints_and_gating() {
        let state = test_state(None);
        let app = build_app(state.clone());

        // 1. Lease screen 0 for an agent
        let lease = state.agent.lease_screen(0, "agent-eva").unwrap();
        assert_eq!(state.agent.handoff_gen(0), Some(1));
        assert_eq!(
            state.agent.phase(0),
            Some(reach_cli::agent::ScreenPhase::AgentActive)
        );

        // Tool execution with correct X-Handoff-Gen = 1 passes phase validation
        let req = Request::builder()
            .method("POST")
            .uri("/tools/click")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .header("X-Handoff-Gen", "1")
            .body(Body::from(r#"{"screen": 0, "x": 10, "y": 10}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        // Since docker client in test_state is a dummy, dispatch may fail with internal server error
        // or 500 when resolving sandbox, but it MUST NOT be 409!
        assert_ne!(res.status(), StatusCode::CONFLICT);

        // Tool execution with stale X-Handoff-Gen = 99 -> 409 Conflict {"error": "stale_plan"}
        let req = Request::builder()
            .method("POST")
            .uri("/tools/click")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .header("X-Handoff-Gen", "99")
            .body(Body::from(r#"{"screen": 0, "x": 10, "y": 10}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body_json.get("error").unwrap(), "stale_plan");

        // 2. POST /agent/screens/0/takeover with reason and url -> moves to HandoffPending, gen increments to 2
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/takeover")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .body(Body::from(
                r#"{"reason": "CAPTCHA challenge", "url": "http://127.0.0.1:6080/vnc.html"}"#,
            ))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            state.agent.phase(0),
            Some(reach_cli::agent::ScreenPhase::HandoffPending)
        );
        assert_eq!(state.agent.handoff_gen(0), Some(2));

        // 3. Tool execution while takeover is active -> 409 Conflict {"error": "takeover_active", ...}
        let req = Request::builder()
            .method("POST")
            .uri("/tools/click")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .header("X-Handoff-Gen", "2")
            .body(Body::from(r#"{"screen": 0, "x": 10, "y": 10}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body_json.get("error").unwrap(), "takeover_active");
        assert_eq!(body_json.get("phase").unwrap(), "HandoffPending");
        assert_eq!(body_json.get("handoff_gen").unwrap(), 2);

        // Also test MCP endpoint tool call gets 409 Conflict
        let mcp_call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "click",
                "arguments": { "screen": 0, "x": 10, "y": 10 }
            }
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .body(Body::from(serde_json::to_vec(&mcp_call).unwrap()))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);

        // 4. POST /agent/screens/0/handback (e.g. human clicks banner) -> moves to HumanDone, gen increments to 3
        let human_token = state.agent.human_token(0).unwrap();
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/handback")
            .header("Host", "127.0.0.1:4200")
            .header("X-Human-Token", &human_token)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            state.agent.phase(0),
            Some(reach_cli::agent::ScreenPhase::HumanDone)
        );
        assert_eq!(state.agent.handoff_gen(0), Some(3));

        // 5. GET /agent/screens/0/wait?phase=HumanDone returns immediately since already HumanDone
        let req = Request::builder()
            .method("GET")
            .uri("/agent/screens/0/wait?phase=HumanDone&timeout=5")
            .header("Host", "127.0.0.1:4200")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body_json.get("status").unwrap(), "ok");
        assert_eq!(body_json.get("phase").unwrap(), "HumanDone");
        assert_eq!(body_json.get("handoff_gen").unwrap(), 3);

        // 6. POST /agent/screens/0/ack -> moves HumanDone to AgentActive, gen increments to 4
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/ack")
            .header("Host", "127.0.0.1:4200")
            .header("X-Lease-Token", &lease.token)
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            state.agent.phase(0),
            Some(reach_cli::agent::ScreenPhase::AgentActive)
        );
        assert_eq!(state.agent.handoff_gen(0), Some(4));

        // 7. After ack, tool execution with gen 4 is no longer rejected with 409
        let req = Request::builder()
            .method("POST")
            .uri("/tools/click")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .header("X-Handoff-Gen", "4")
            .body(Body::from(r#"{"screen": 0, "x": 10, "y": 10}"#))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_ne!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn test_tool_call_locked_profile_returns_http_423() {
        let state = test_state(None);
        let app = build_app(state.clone());

        // Acquire lock on profile "test-work" via state.profile_broker
        let _lease = state
            .profile_broker
            .acquire("test-work", 0)
            .expect("should lock");

        // Try calling /tools/browse with use_profile "test-work"
        let req = Request::builder()
            .method("POST")
            .uri("/tools/browse")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .body(Body::from(
                r#"{"url": "https://github.com", "use_profile": "test-work"}"#,
            ))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::LOCKED);
        assert_eq!(res.status().as_u16(), 423);

        let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body_json.get("error").unwrap(), "profile_locked");
        assert_eq!(body_json.get("profile").unwrap(), "test-work");
    }

    #[tokio::test]
    async fn test_mcp_tool_call_locked_profile_returns_error() {
        let state = test_state(None);
        let app = build_app(state.clone());

        // Acquire lock on profile "test-work-mcp" via state.profile_broker
        let _lease = state
            .profile_broker
            .acquire("test-work-mcp", 0)
            .expect("should lock");

        let mcp_call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "browse",
                "arguments": {
                    "url": "https://github.com",
                    "use_profile": "test-work-mcp"
                }
            }
        });

        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .body(Body::from(serde_json::to_vec(&mcp_call).unwrap()))
            .unwrap();

        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        let result = body_json.get("result").expect("expected result field");
        let is_err = result
            .get("is_error")
            .or_else(|| result.get("isError"))
            .and_then(|v| v.as_bool());
        assert_eq!(is_err, Some(true));

        let content = result
            .get("content")
            .and_then(|v| v.as_array())
            .expect("expected content array");
        let text = content[0]
            .get("text")
            .and_then(|v| v.as_str())
            .expect("expected text content");

        let err_obj: serde_json::Value = serde_json::from_str(text).expect("text should be json");
        assert_eq!(err_obj.get("error").unwrap(), "profile_locked");
        assert_eq!(err_obj.get("profile").unwrap(), "test-work-mcp");
    }

    #[tokio::test]
    async fn test_auth_bypass_with_human_token() {
        // Setup server with REACH_AUTH_TOKEN
        let state = test_state(Some("bearer-secret"));
        let app = build_app(state.clone());

        // Lease and request takeover directly on agent
        let _ = state.agent.lease_screen(0, "bot").unwrap();
        let takeover_state = state
            .agent
            .request_takeover(0, Some("captcha".into()), None)
            .unwrap();
        let token = takeover_state.human_token.expect("token should be minted");

        // 1. /agent/screens/0/connected without token or bearer -> 403 Forbidden
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/connected")
            .header("Host", "127.0.0.1:4200")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // 2. /agent/screens/0/connected with invalid human token -> 403 Forbidden
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/connected?token=bogus-token")
            .header("Host", "127.0.0.1:4200")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // 3. /agent/screens/0/connected with valid human token in query param -> 200 OK
        let req = Request::builder()
            .method("POST")
            .uri(format!("/agent/screens/0/connected?token={}", token))
            .header("Host", "127.0.0.1:4200")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            state.agent.phase(0),
            Some(reach_cli::agent::ScreenPhase::HumanActive)
        );

        // 4. /agent/screens with valid token query param -> 200 OK without Bearer
        let req = Request::builder()
            .method("GET")
            .uri(format!("/agent/screens?token={}", token))
            .header("Host", "127.0.0.1:4200")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // 5. /agent/screens/0/handback with valid X-Human-Token header -> 200 OK
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/handback")
            .header("Host", "127.0.0.1:4200")
            .header("X-Human-Token", &token)
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            state.agent.phase(0),
            Some(reach_cli::agent::ScreenPhase::HumanDone)
        );
    }

    #[tokio::test]
    async fn test_cannot_eject_human_via_takeover_pending_false() {
        let state = test_state(None);
        let app = build_app(state.clone());

        let lease = state.agent.lease_screen(0, "bot").unwrap();
        state
            .agent
            .request_takeover(0, Some("captcha".into()), None)
            .unwrap();
        state.agent.human_connected(0).unwrap();
        assert_eq!(
            state.agent.phase(0),
            Some(reach_cli::agent::ScreenPhase::HumanActive)
        );

        // Agent tries to eject human by setting pending=false -> 409 Conflict
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/takeover")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .body(Body::from(r#"{"pending": false}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body_json.get("error").unwrap(), "cannot_eject_human");

        // Agent tries to ack while HumanActive -> 409 Conflict
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/ack")
            .header("Host", "127.0.0.1:4200")
            .header("X-Lease-Token", &lease.token)
            .body(Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn test_mutating_tools_require_handoff_gen() {
        let state = test_state(None);
        let app = build_app(state.clone());

        let lease = state.agent.lease_screen(0, "bot").unwrap();

        // Mutating tool /tools/click without X-Handoff-Gen on leased screen -> 409 missing_handoff_gen
        let req = Request::builder()
            .method("POST")
            .uri("/tools/click")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .body(Body::from(r#"{"screen": 0, "x": 10, "y": 10}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body_json.get("error").unwrap(), "missing_handoff_gen");

        // Mutating tool with wrong generation -> 409 stale_plan
        let req = Request::builder()
            .method("POST")
            .uri("/tools/click")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .header("X-Handoff-Gen", "42")
            .body(Body::from(r#"{"screen": 0, "x": 10, "y": 10}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body_json.get("error").unwrap(), "stale_plan");

        // Mutating tool with matching generation -> passes handoff check (status != 409)
        let req = Request::builder()
            .method("POST")
            .uri("/tools/click")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .header("X-Handoff-Gen", "1")
            .body(Body::from(r#"{"screen": 0, "x": 10, "y": 10}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_ne!(res.status(), StatusCode::CONFLICT);

        // Non-mutating tool /tools/screenshot without X-Handoff-Gen -> does not return 409
        let req = Request::builder()
            .method("POST")
            .uri("/tools/screenshot")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .body(Body::from(r#"{"screen": 0}"#))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_ne!(res.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn test_takeover_busy_drain_and_timeout() {
        let state = test_state(None);
        let app = build_app(state.clone());

        let lease = state.agent.lease_screen(0, "bot").unwrap();

        // Mark screen busy
        state.agent.inc_busy(0);

        // POST /agent/screens/0/takeover should timeout and return 409 screen_busy
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/takeover")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .body(Body::from(r#"{"reason": "auth"}"#))
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
        let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body_json.get("error").unwrap(), "screen_busy");

        // Clean up busy
        state.agent.dec_busy(0);

        // Now takeover request succeeds
        let req = Request::builder()
            .method("POST")
            .uri("/agent/screens/0/takeover")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
            .body(Body::from(r#"{"reason": "auth"}"#))
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
