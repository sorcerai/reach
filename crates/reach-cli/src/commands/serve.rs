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
        .route("/health", get(|| async { "ok" }))
        .route("/agent/screens", get(agent_screens_handler))
        .route("/agent/screens/{id}/lease", post(agent_lease_handler))
        .route("/agent/screens/{id}/lease", delete(agent_release_handler))
        .route("/agent/screens/{id}/takeover", post(agent_takeover_handler))
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

    let state = Arc::new(AppState {
        docker,
        default_sandbox: args.sandbox,
        public_host,
        bind_host: host.clone(),
        auth_token,
        agent,
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

async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(expected_token) = &state.auth_token {
        let path = req.uri().path();
        if path.starts_with("/agent") || path.starts_with("/tools") {
            let auth_header = req.headers().get(axum::http::header::AUTHORIZATION);
            let token = auth_header
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.strip_prefix("Bearer "))
                .map(|t| t.trim());

            if token != Some(expected_token.as_str()) {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({
                        "error": "unauthorized: valid Bearer token required"
                    })),
                )
                    .into_response();
            }
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
            takeover_pending: s.takeover_pending,
            takeover_url: s.takeover_url,
            leased_at: s.leased_at,
            novnc_url: novnc_url(&state.public_host, novnc_base + s.id as u16),
            busy: s.busy,
        })
        .collect();

    Json(res)
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

#[derive(Debug, serde::Deserialize)]
pub struct TakeoverRequest {
    pub pending: bool,
    #[serde(default)]
    pub url: Option<String>,
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

    state.agent.set_takeover(id, body.pending, body.url);
    Ok(Json(serde_json::json!({ "status": "ok", "id": id })))
}

async fn mcp_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> Result<Json<JsonRpcResponse>, (StatusCode, Json<serde_json::Value>)> {
    if req.method == "tools/call" {
        let args = req.params.get("arguments").cloned().unwrap_or_default();
        let screen = reach_cli::tools::screen_for(&args);
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
    }

    Ok(Json(handle_mcp(&state, &req).await))
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

    let sandbox_arg = args.get("sandbox").and_then(|v| v.as_str());
    let target = match resolve_sandbox(&state, sandbox_arg).await {
        Ok(t) => t,
        Err(e) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            ));
        }
    };

    let ctx = ToolContext {
        docker: &state.docker,
        public_host: state.public_host.clone(),
        agent: Some(&state.agent),
    };
    let result = dispatch(&ctx, &tool, &args, &target).await;
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

async fn handle_mcp(state: &AppState, req: &JsonRpcRequest) -> JsonRpcResponse {
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
            let sandbox_arg = args.get("sandbox").and_then(|v| v.as_str());

            let target = match resolve_sandbox(state, sandbox_arg).await {
                Ok(t) => t,
                Err(e) => {
                    return JsonRpcResponse::success(
                        req.id.clone(),
                        serde_json::to_value(ToolResponse::error(e.to_string())).unwrap(),
                    );
                }
            };

            let ctx = ToolContext {
                docker: &state.docker,
                public_host: state.public_host.clone(),
                agent: Some(&state.agent),
            };
            let result = dispatch(&ctx, tool, &args, &target).await;
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
        Arc::new(AppState {
            docker,
            default_sandbox: None,
            public_host: "127.0.0.1".into(),
            bind_host: "127.0.0.1".into(),
            auth_token: auth_token.map(String::from),
            agent,
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

        // Request with valid X-Lease-Token -> passes lease validation (status 200)
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("Host", "127.0.0.1:4200")
            .header("Content-Type", "application/json")
            .header("X-Lease-Token", &lease.token)
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
}
