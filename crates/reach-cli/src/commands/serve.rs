use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Args;
use reach_cli::config::ReachConfig;
use reach_cli::docker::DockerClient;
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
}

struct AppState {
    docker: DockerClient,
    default_sandbox: Option<String>,
    public_host: String,
}

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    let port = args.port;
    let host = args.host.clone();

    let state = Arc::new(AppState {
        docker: DockerClient::new()?,
        default_sandbox: args.sandbox,
        public_host: ReachConfig::load().server.effective_public_host(),
    });

    let app = Router::new()
        .route("/mcp", post(mcp_handler))
        .route("/mcp", get(sse_handler))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    let addr = format!("{host}:{port}");
    println!("reach MCP server listening on {addr}");
    println!("Connect: claude mcp add reach --url http://{addr}/mcp");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn resolve_sandbox(state: &AppState, requested: Option<&str>) -> anyhow::Result<String> {
    if let Some(name) = requested.or(state.default_sandbox.as_deref()) {
        return Ok(name.to_string());
    }
    let sandboxes = state.docker.list().await?;
    sandboxes
        .into_iter()
        .find(|s| matches!(s.status, reach_cli::docker::SandboxStatus::Running))
        .map(|s| s.name)
        .ok_or_else(|| anyhow::anyhow!("no running sandbox found"))
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
