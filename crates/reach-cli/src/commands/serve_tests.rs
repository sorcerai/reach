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
        default_sandbox: Some("fixture".into()),
        public_host: "127.0.0.1".into(),
        bind_host: "127.0.0.1".into(),
        auth_token: auth_token.map(String::from),
        agent,
        profile_broker,
        cookie_jars,
        accounts: Default::default(),
        viewer_sessions: Default::default(),
        raw_viewer_token: "test-only-backend-token".into(),
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
    assert!(state.agent.lease_screen(0, "user-a").is_err());
    state.agent.cancel_takeover(0, None).unwrap();

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
        .request_takeover(
            0,
            Some("captcha".into()),
            None,
            state.agent.lease_token(0).as_deref(),
        )
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
        .request_takeover(
            0,
            Some("captcha".into()),
            None,
            state.agent.lease_token(0).as_deref(),
        )
        .unwrap();
    state
        .agent
        .human_connected(0, state.agent.human_token(0).as_deref())
        .unwrap();
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

    // Observations must be fenced to the same handoff generation as actions.
    let req = Request::builder()
        .method("POST")
        .uri("/tools/screenshot")
        .header("Host", "127.0.0.1:4200")
        .header("Content-Type", "application/json")
        .header("X-Lease-Token", &lease.token)
        .body(Body::from(r#"{"screen": 0}"#))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
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

#[tokio::test]
async fn test_lease_screen_is_creation_only_for_same_owner_label() {
    let state = test_state(None);
    let app = build_app(state.clone());

    let lease = state.agent.lease_screen(0, "worker-a").unwrap();

    // Re-leasing with the same owner label is creation-only: it must fail and
    // must never echo back the active lease token.
    let req = Request::builder()
        .method("POST")
        .uri("/agent/screens/0/lease")
        .header("Host", "127.0.0.1:4200")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"owner": "worker-a"}"#))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert!(body_json.get("error").is_some());
    assert!(body_json.get("token").is_none());

    // A different label on an occupied screen is rejected the same way.
    let req = Request::builder()
        .method("POST")
        .uri("/agent/screens/0/lease")
        .header("Host", "127.0.0.1:4200")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"owner": "worker-b"}"#))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);

    // The original capability is unchanged.
    assert_eq!(
        state.agent.lease_token(0).as_deref(),
        Some(lease.token.as_str())
    );
}

#[tokio::test]
async fn test_owner_label_cannot_release_during_human_active() {
    let state = test_state(None);
    let app = build_app(state.clone());

    let lease = state.agent.lease_screen(0, "worker").unwrap();
    state
        .agent
        .request_takeover(
            0,
            Some("login".into()),
            None,
            state.agent.lease_token(0).as_deref(),
        )
        .unwrap();
    state
        .agent
        .human_connected(0, state.agent.human_token(0).as_deref())
        .unwrap();

    // Owner labels are diagnostic, never authority: an "admin" label without
    // a capability token cannot release the screen.
    let req = Request::builder()
        .method("DELETE")
        .uri("/agent/screens/0/lease")
        .header("Host", "127.0.0.1:4200")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"owner": "admin"}"#))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Even the worker's valid capability cannot eject an active human.
    let req = Request::builder()
        .method("DELETE")
        .uri("/agent/screens/0/lease")
        .header("Host", "127.0.0.1:4200")
        .header("Content-Type", "application/json")
        .header("X-Lease-Token", &lease.token)
        .body(Body::from(r#"{"owner": "worker"}"#))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);

    // Human stays in control and the lease is untouched.
    assert_eq!(
        state.agent.phase(0),
        Some(reach_cli::agent::ScreenPhase::HumanActive)
    );
    assert_eq!(
        state.agent.lease_token(0).as_deref(),
        Some(lease.token.as_str())
    );
    assert!(state.agent.human_token(0).is_some());
}

#[tokio::test]
async fn test_worker_lease_token_scoped_to_its_own_screen() {
    let state = test_state(Some("server-secret"));
    let app = build_app(state.clone());

    let lease = state.agent.lease_screen(0, "worker").unwrap();

    // Own screen read route is accepted with the worker credential alone.
    let req = Request::builder()
        .uri("/agent/screens/0")
        .header("Host", "127.0.0.1:4200")
        .header("X-Lease-Token", &lease.token)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // The same credential is rejected for a different screen.
    let req = Request::builder()
        .uri("/agent/screens/1")
        .header("Host", "127.0.0.1:4200")
        .header("X-Lease-Token", &lease.token)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(
        body_json.get("error").unwrap(),
        "invalid or out-of-scope lease capability"
    );

    // Lease allocation is not covered by the worker credential.
    let req = Request::builder()
        .method("POST")
        .uri("/agent/screens/0/lease")
        .header("Host", "127.0.0.1:4200")
        .header("Content-Type", "application/json")
        .header("X-Lease-Token", &lease.token)
        .body(Body::from(r#"{"owner": "worker"}"#))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Human-only takeover routes are not covered either.
    let req = Request::builder()
        .method("POST")
        .uri("/agent/screens/0/connected")
        .header("Host", "127.0.0.1:4200")
        .header("X-Lease-Token", &lease.token)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // Tool calls whose arguments target another screen are rejected before
    // the Docker call.
    let req = Request::builder()
        .method("POST")
        .uri("/tools/click")
        .header("Host", "127.0.0.1:4200")
        .header("Content-Type", "application/json")
        .header("X-Lease-Token", &lease.token)
        .header("X-Handoff-Gen", "1")
        .body(Body::from(r#"{"screen": 1, "x": 10, "y": 10}"#))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(
        body_json.get("error").unwrap(),
        "lease capability does not authorize this screen"
    );
}

#[tokio::test]
async fn test_worker_lease_token_cannot_force_release() {
    let state = test_state(Some("server-secret"));
    let app = build_app(state.clone());

    let lease = state.agent.lease_screen(0, "worker").unwrap();

    // A valid worker capability is rejected on the supervisor-only route,
    // even for its own screen.
    let req = Request::builder()
        .method("POST")
        .uri("/agent/screens/0/force-release")
        .header("Host", "127.0.0.1:4200")
        .header("X-Lease-Token", &lease.token)
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(
        body_json.get("error").unwrap(),
        "invalid or out-of-scope lease capability"
    );
    assert_eq!(
        state.agent.lease_token(0).as_deref(),
        Some(lease.token.as_str())
    );

    // Without any credentials the request is unauthorized outright.
    let req = Request::builder()
        .method("POST")
        .uri("/agent/screens/0/force-release")
        .header("Host", "127.0.0.1:4200")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        state.agent.lease_token(0).as_deref(),
        Some(lease.token.as_str())
    );
}

#[tokio::test]
async fn test_supervisor_force_release_invalidates_lease_and_human_token() {
    let state = test_state(Some("server-secret"));
    let app = build_app(state.clone());

    state.agent.lease_screen(0, "worker").unwrap();
    let takeover = state
        .agent
        .request_takeover(
            0,
            Some("captcha".into()),
            None,
            state.agent.lease_token(0).as_deref(),
        )
        .unwrap();
    state
        .agent
        .human_connected(0, state.agent.human_token(0).as_deref())
        .unwrap();
    let human_token = takeover.human_token.expect("token should be minted");
    let gen_before = state.agent.handoff_gen(0).unwrap();

    // Supervisor force-release succeeds with the configured bearer.
    let req = Request::builder()
        .method("POST")
        .uri("/agent/screens/0/force-release")
        .header("Host", "127.0.0.1:4200")
        .header("Authorization", "Bearer server-secret")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body_json.get("status").unwrap(), "ok");
    assert_eq!(body_json.get("released").unwrap(), true);

    // Lease capability and human token are both invalidated.
    assert!(state.agent.lease_token(0).is_none());
    assert!(!state.agent.is_leased(0));
    assert!(!state.agent.verify_human_token(0, &human_token));
    assert_eq!(
        state.agent.phase(0),
        Some(reach_cli::agent::ScreenPhase::Idle)
    );
    assert!(state.agent.handoff_gen(0).unwrap() > gen_before);

    // The stale human token no longer grants any access.
    let req = Request::builder()
        .uri(format!("/agent/screens?token={}", human_token))
        .header("Host", "127.0.0.1:4200")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_force_release_unavailable_without_configured_bearer() {
    let state = test_state(None);
    let app = build_app(state.clone());

    let lease = state.agent.lease_screen(0, "worker").unwrap();

    // Local unauthenticated mode cannot use force-release.
    let req = Request::builder()
        .method("POST")
        .uri("/agent/screens/0/force-release")
        .header("Host", "127.0.0.1:4200")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(
        body_json.get("error").unwrap(),
        "configured supervisor bearer required"
    );

    // An arbitrary bearer cannot substitute: there is no configured
    // supervisor bearer to match against.
    let req = Request::builder()
        .method("POST")
        .uri("/agent/screens/0/force-release")
        .header("Host", "127.0.0.1:4200")
        .header("Authorization", "Bearer anything")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        state.agent.lease_token(0).as_deref(),
        Some(lease.token.as_str())
    );
}

#[tokio::test]
async fn test_arbitrary_code_tools_reject_stale_generation_before_docker() {
    let state = test_state(None);
    let app = build_app(state.clone());

    let lease = state.agent.lease_screen(0, "worker").unwrap();

    // exec with a stale X-Handoff-Gen -> 409 stale_plan. The rejection comes
    // from screen validation, which runs before any Docker dispatch.
    let req = Request::builder()
        .method("POST")
        .uri("/tools/exec")
        .header("Host", "127.0.0.1:4200")
        .header("Content-Type", "application/json")
        .header("X-Lease-Token", &lease.token)
        .header("X-Handoff-Gen", "99")
        .body(Body::from(r#"{"screen": 0, "command": "echo hi"}"#))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body_json.get("error").unwrap(), "stale_plan");

    // exec without any X-Handoff-Gen -> 409 missing_handoff_gen.
    let req = Request::builder()
        .method("POST")
        .uri("/tools/exec")
        .header("Host", "127.0.0.1:4200")
        .header("Content-Type", "application/json")
        .header("X-Lease-Token", &lease.token)
        .body(Body::from(r#"{"screen": 0, "command": "echo hi"}"#))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body_json.get("error").unwrap(), "missing_handoff_gen");

    // playwright_eval with a stale generation -> 409 stale_plan.
    let req = Request::builder()
        .method("POST")
        .uri("/tools/playwright_eval")
        .header("Host", "127.0.0.1:4200")
        .header("Content-Type", "application/json")
        .header("X-Lease-Token", &lease.token)
        .header("X-Handoff-Gen", "42")
        .body(Body::from(r#"{"screen": 0, "script": "1+1"}"#))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body_json.get("error").unwrap(), "stale_plan");

    // playwright_eval without a generation -> 409 missing_handoff_gen.
    let req = Request::builder()
        .method("POST")
        .uri("/tools/playwright_eval")
        .header("Host", "127.0.0.1:4200")
        .header("Content-Type", "application/json")
        .header("X-Lease-Token", &lease.token)
        .body(Body::from(r#"{"screen": 0, "script": "1+1"}"#))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body_json.get("error").unwrap(), "missing_handoff_gen");

    // The MCP route enforces the same current-generation check for exec.
    let mcp_call = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "exec",
            "arguments": {
                "screen": 0,
                "command": "echo hi"
            }
        }
    });
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("Host", "127.0.0.1:4200")
        .header("Content-Type", "application/json")
        .header("X-Lease-Token", &lease.token)
        .header("X-Handoff-Gen", "99")
        .body(Body::from(serde_json::to_vec(&mcp_call).unwrap()))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::CONFLICT);
    let body_bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(body_json.get("error").unwrap(), "stale_plan");
}
