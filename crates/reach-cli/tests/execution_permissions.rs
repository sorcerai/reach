use axum::{Json, Router, http::Method};
use reach_cli::{
    docker::DockerClient,
    tools::{ToolContext, dispatch},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn disabled_execution_never_reaches_docker_exec_for_either_tool() {
    let executions = Arc::new(AtomicUsize::new(0));
    let observed = executions.clone();
    let app = Router::new().fallback(move |method: Method| {
        let observed = observed.clone();
        async move {
            if method != Method::GET { observed.fetch_add(1, Ordering::SeqCst); }
            Json(serde_json::json!([{
                "Id": "fixture-container", "State": "running", "Image": "fixture",
                "Labels": {"reach.name": "fixture", "reach.screens": "1", "reach.allow_exec": "false"}
            }]))
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let docker = DockerClient::new(Some(&format!("http://{address}"))).unwrap();
    let ctx = ToolContext {
        docker: &docker,
        public_host: "127.0.0.1".into(),
        agent: None,
        profile_broker: None,
        cookie_jars: None,
        owner: None,
    };
    for tool in ["exec", "playwright_eval"] {
        let result = dispatch(
            &ctx,
            tool,
            &serde_json::json!({"command": "true", "script": "pass"}),
            "fixture",
        )
        .await;
        assert!(result.is_error);
    }
    server.abort();
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "disabled capability reached Docker execution"
    );
}
