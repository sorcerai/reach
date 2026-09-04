use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
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

#[derive(Args)]
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
}

struct AppState {
    docker: DockerClient,
    default_sandbox: Option<String>,
    public_host: String,
    agent: AgentState,
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

    let state = Arc::new(AppState {
        docker,
        default_sandbox: args.sandbox,
        public_host,
        agent,
    });

    println!("Live view host: {}", state.public_host);

    let app = Router::new()
        .route("/mcp", post(mcp_handler))
        .route("/mcp", get(sse_handler))
        .route("/health", get(|| async { "ok" }))
        .route("/agent/screens", get(agent_screens_handler))
        .route("/agent/screens/{id}/lease", post(agent_lease_handler))
        .route("/agent/screens/{id}/lease", delete(agent_release_handler))
        .route("/agent/screens/{id}/takeover", post(agent_takeover_handler))
        .with_state(state);

    let addr = format!("{host}:{port}");
    println!("reach MCP server listening on {addr}");
    println!("Connect: claude mcp add reach --url http://{addr}/mcp");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
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
        Ok(()) => Ok(Json(serde_json::json!({
            "status": "ok",
            "id": id,
            "owner": body.owner,
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
    Json(body): Json<LeaseRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    match state.agent.release(id, &body.owner) {
        Ok(()) => Ok(Json(serde_json::json!({
            "status": "ok",
            "id": id,
            "released": true,
        }))),
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

async fn agent_takeover_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
    Json(body): Json<TakeoverRequest>,
) -> Json<serde_json::Value> {
    state.agent.set_takeover(id, body.pending, body.url);
    Json(serde_json::json!({ "status": "ok", "id": id }))
}

async fn mcp_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<JsonRpcRequest>,
) -> Json<JsonRpcResponse> {
    Json(handle_mcp(&state, &req).await)
}

async fn sse_handler() -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = tokio_stream::once(Ok(Event::default().event("endpoint").data("/mcp")));
    Sse::new(stream)
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
