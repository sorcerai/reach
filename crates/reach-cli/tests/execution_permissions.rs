use axum::{Json, Router, http::Method};
use reach_cli::{
    config::ReachConfig,
    runtime::RuntimeClient,
    tools::{ToolContext, dispatch},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

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
    let mut config = ReachConfig::default();
    config.docker.socket = format!("http://{address}");
    let runtime = RuntimeClient::from_config(&config).unwrap();
    let ctx = ToolContext {
        runtime: &runtime,
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
#[tokio::test]
async fn execution_authorization_and_mutation_use_the_same_resolved_sandbox() {
    let socket = std::path::PathBuf::from(format!(
        "/tmp/reach-exec-{}.sock",
        uuid::Uuid::new_v4().simple()
    ));
    let listener = UnixListener::bind(&socket).unwrap();
    let replacement_executed = Arc::new(AtomicUsize::new(0));
    let observed_replacement = replacement_executed.clone();
    let old_id = "00000000-0000-4000-8000-00000000000a";
    let replacement_id = "00000000-0000-4000-8000-00000000000b";
    let cleanup_socket = socket.clone();
    let server = tokio::spawn(async move {
        let mut list_calls = 0usize;
        loop {
            let mut execution_seen = false;
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.strip_prefix("Content-Length:") {
                    content_length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).await.unwrap();
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let result = match request["method"].as_str().unwrap() {
                "list" => {
                    list_calls += 1;
                    let container_id = if list_calls == 1 {
                        old_id
                    } else {
                        replacement_id
                    };
                    serde_json::json!([{
                        "name": "guestA",
                        "container_id": container_id,
                        "status": "running",
                        "image": "fixture",
                        "ports": {
                            "vnc": null,
                            "novnc": 6080,
                            "health": null,
                            "screens": 1,
                            "extra": []
                        },
                        "created_at": "2026-01-01T00:00:00Z",
                        "allow_exec": list_calls == 1
                    }])
                }
                "exec_input" => {
                    let target = request["params"]["target"].as_str().unwrap();
                    if target == replacement_id {
                        observed_replacement.fetch_add(1, Ordering::SeqCst);
                    }
                    execution_seen = true;
                    serde_json::json!({
                        "exit_code": if target == replacement_id { 1 } else { 0 },
                        "stdout": if target == replacement_id { "" } else { "executed" },
                        "stderr": if target == replacement_id {
                            "replacement execution must not be reached"
                        } else {
                            ""
                        }
                    })
                }
                method => panic!("unexpected broker method {method}"),
            };
            let body = serde_json::to_vec(&serde_json::json!({"result": result})).unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let mut stream = reader.into_inner();
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            if execution_seen {
                break;
            }
        }
        let _ = std::fs::remove_file(cleanup_socket);
    });

    let mut config = ReachConfig::default();
    config.runtime.backend = reach_cli::config::RuntimeBackend::Microvm;
    config.runtime.broker_socket = Some(socket);
    let runtime = RuntimeClient::from_config(&config).unwrap();
    let ctx = ToolContext {
        runtime: &runtime,
        public_host: "127.0.0.1".into(),
        agent: None,
        profile_broker: None,
        cookie_jars: None,
        owner: None,
    };

    let result = dispatch(
        &ctx,
        "exec",
        &serde_json::json!({"command": "true"}),
        "guestA",
    )
    .await;
    assert_eq!(
        replacement_executed.load(Ordering::SeqCst),
        0,
        "the replacement guest must never receive execution"
    );
    assert!(!result.is_error);
    server.await.unwrap();
}
