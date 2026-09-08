#![cfg(unix)]

use reach_cli::config::{ReachConfig, RuntimeBackend};
use reach_cli::docker::PageActionOptions;
use reach_cli::runtime::RuntimeClient;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::time::Duration;

#[tokio::test]
async fn completed_action_is_not_abandoned_by_its_locator_timeout() {
    let root = std::path::Path::new("/tmp").join(format!("reach-action-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let socket = root.join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let committed = root.join("committed");
    let server_commit = committed.clone();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut length = None;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let mut body = vec![0; length.unwrap()];
        reader.read_exact(&mut body).unwrap();
        drop(reader);
        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(request["method"], "exec_input");
        std::fs::write(server_commit, "clicked once").unwrap();
        // Process startup/transport is not part of the locator's interaction timeout.
        std::thread::sleep(Duration::from_secs(5));
        let body = serde_json::json!({"result": {
            "exit_code": 0, "stdout": "{\"status\":\"ok\",\"action\":\"click\"}\n", "stderr": ""
        }})
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        // The unfixed client disconnects after four seconds; retain the committed outcome.
        let _ = stream.write_all(response.as_bytes());
    });
    let mut config = ReachConfig::default();
    config.runtime.backend = RuntimeBackend::Microvm;
    config.runtime.broker_socket = Some(socket);
    let runtime = RuntimeClient::from_config(&config).unwrap();
    let result = runtime
        .page_action(
            &uuid::Uuid::new_v4().to_string(),
            &PageActionOptions {
                target_id: "target".into(),
                loader_id: "loader".into(),
                selector: "#confirm".into(),
                backend_node_id: 1,
                action: "click".into(),
                button: "left".into(),
                text: String::new(),
                clear: false,
                submit: false,
                timeout_ms: 1_000,
                user_data_dir: "/profile".into(),
                display: ":99".into(),
                screen: 0,
            },
        )
        .await;
    server.join().unwrap();
    let actual_state = std::fs::read_to_string(&committed).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(actual_state, "clicked once");
    let outcome: serde_json::Value = serde_json::from_str(
        &result
            .expect("a completed broker action must return its outcome, not a premature timeout"),
    )
    .unwrap();
    assert_eq!(outcome["status"], "ok");
}
