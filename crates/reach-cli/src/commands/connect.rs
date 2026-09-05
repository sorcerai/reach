use clap::Args;
use reach_cli::config::ReachConfig;
use reach_cli::docker::DockerClient;
use reach_cli::mcp::{
    JsonRpcRequest, JsonRpcResponse, McpInitializeResult, RequestId, tool_definitions,
};
use reach_cli::tools::{ToolContext, dispatch};
use std::io::{BufRead, Write};

#[derive(Args)]
pub struct ConnectArgs {
    /// Sandbox name or container ID
    pub target: String,
}

pub async fn run(args: ConnectArgs) -> anyhow::Result<()> {
    let cfg = ReachConfig::load();
    let docker = DockerClient::new(cfg.docker.socket_path())?;
    let _sandbox = docker.find(&args.target).await?;
    let profile_broker = reach_cli::profile::ProfileBroker::default_broker();
    let cookie_jars = reach_cli::profile::CookieJarService::default_service();
    let ctx = ToolContext {
        docker: &docker,
        public_host: cfg.server.effective_public_host(),
        agent: None,
        profile_broker: Some(&profile_broker),
        cookie_jars: Some(&cookie_jars),
        owner: None,
    };

    tracing::info!(target = args.target, "MCP stdio bridge started");

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = JsonRpcResponse::error(
                    RequestId::Number(0),
                    -32700,
                    format!("parse error: {e}"),
                );
                writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
                continue;
            }
        };

        let response = handle_request(&ctx, &args.target, &request).await;
        writeln!(stdout, "{}", serde_json::to_string(&response)?)?;
        stdout.flush()?;
    }

    Ok(())
}

async fn handle_request(
    ctx: &ToolContext<'_>,
    target: &str,
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
            let tool_name = req
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let arguments = req.params.get("arguments").cloned().unwrap_or_default();
            let result = dispatch(ctx, tool_name, &arguments, target).await;
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
