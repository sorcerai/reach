use reach_cli::docker::PageTextOutput;
use reach_cli::mcp::{ClickParams, TypeParams};
use reach_cli::refs::{ElementRef, RefScope, global_ref_table, resolve_ref};
use std::collections::HashMap;

#[test]
fn test_element_ref_deserialization_and_target_coords() {
    let json = r#"{
        "ref": "e1",
        "role": "textbox",
        "name": "Username",
        "value": "",
        "selector": "input#user",
        "point": [420, 260],
        "box_bounds": [300, 240, 240, 40],
        "focused": true,
        "disabled": false
    }"#;
    let el: ElementRef = serde_json::from_str(json).unwrap();
    assert_eq!(el.r#ref, "e1");
    assert_eq!(el.role, "textbox");
    assert_eq!(el.name, "Username");
    assert_eq!(el.target_coordinates(), Some((420, 260)));
}

#[test]
fn test_element_ref_target_coords_from_box_bounds_when_point_absent() {
    let el = ElementRef {
        r#ref: "e2".into(),
        role: "button".into(),
        name: "Login".into(),
        value: None,
        selector: None,
        backend_node_id: None,
        point: None,
        box_bounds: Some([100.0, 200.0, 80.0, 30.0]),
        focused: false,
        disabled: false,
    };
    // Center is 100 + 40 = 140, 200 + 15 = 215
    assert_eq!(el.target_coordinates(), Some((140, 215)));
}

#[test]
fn test_page_text_output_roundtrip_with_axtree_and_refs() {
    let mut refs = HashMap::new();
    refs.insert(
        "e1".into(),
        ElementRef {
            r#ref: "e1".into(),
            role: "button".into(),
            name: "Sign In".into(),
            value: None,
            selector: Some("[data-reach-ref=e1]".into()),
            backend_node_id: None,
            point: Some([500.0, 300.0]),
            box_bounds: Some([450.0, 280.0, 100.0, 40.0]),
            focused: false,
            disabled: false,
        },
    );
    let output = PageTextOutput {
        status: "ok".into(),
        page_target_id: None,
        page_loader_id: None,
        text: Some("Sign In to Reach".into()),
        axtree: Some(
            "[heading \"Welcome\"]\n[@e1: button \"Sign In\" x=450 y=280 w=100 h=40]".into(),
        ),
        refs: Some(refs),
        url: Some("https://example.com".into()),
        title: Some("Example Login".into()),
        message: None,
        cookies: vec![],
    };

    let serialized = serde_json::to_string(&output).unwrap();
    let deserialized: PageTextOutput = serde_json::from_str(&serialized).unwrap();
    assert_eq!(deserialized.status, "ok");
    assert!(
        deserialized
            .axtree
            .unwrap()
            .contains("@e1: button \"Sign In\"")
    );
    let read_refs = deserialized.refs.unwrap();
    assert!(read_refs.contains_key("e1"));
    assert_eq!(read_refs["e1"].target_coordinates(), Some((500, 300)));
}

#[test]
fn test_click_params_accepts_ref_or_coords() {
    // Legacy coords call
    let params_coords: ClickParams = serde_json::from_str(r#"{"x": 120, "y": 340}"#).unwrap();
    assert_eq!(params_coords.x, 120);
    assert_eq!(params_coords.y, 340);
    assert!(params_coords.reference.is_none());

    // Ref-based call
    let params_ref: ClickParams = serde_json::from_str(r#"{"ref": "@e5"}"#).unwrap();
    assert_eq!(params_ref.reference, Some("@e5".into()));
}

#[test]
fn test_type_params_accepts_ref_and_clear() {
    let params: TypeParams =
        serde_json::from_str(r#"{"text": "alice@reach.io", "ref": "@e2", "clear": true}"#).unwrap();
    assert_eq!(params.text, "alice@reach.io");
    assert_eq!(params.reference, Some("@e2".into()));
    assert!(params.clear);
}

#[test]
fn test_global_ref_table_is_scoped_and_stale_refs_reject() {
    let scope = RefScope::new(
        "test-ref-actions-container:started",
        0,
        Some("attempt-ref-actions".into()),
        Some(1),
    );
    let mut refs = HashMap::new();
    refs.insert(
        "e4".into(),
        ElementRef {
            r#ref: "e4".into(),
            role: "textbox".into(),
            name: "Search".into(),
            value: None,
            selector: None,
            backend_node_id: None,
            point: Some([600.0, 150.0]),
            box_bounds: Some([500.0, 130.0, 200.0, 40.0]),
            focused: false,
            disabled: false,
        },
    );

    let first = global_ref_table().set_refs(scope.clone(), refs.clone());
    let first_name = first.token_map["e4"].clone();
    let found1 = resolve_ref(&scope, &format!("@{first_name}")).expect("must resolve current ref");
    assert_eq!(found1.name, "Search");
    assert_eq!(found1.target_coordinates(), Some((600, 150)));

    // A second observation gets a fresh number even when the browser helper reused e1/e4.
    let second = global_ref_table().set_refs(scope.clone(), refs);
    let second_name = second.token_map["e4"].clone();
    assert_ne!(first_name, second_name);
    assert!(resolve_ref(&scope, &format!("@{first_name}")).is_none());
    assert!(resolve_ref(&scope, &format!("@{second_name}")).is_some());

    // A new lease/handoff/computer cannot resolve the prior observation.
    let new_lease = RefScope::new(
        "different-container:started",
        0,
        Some("attempt-ref-actions-new".into()),
        Some(2),
    );
    assert!(resolve_ref(&new_lease, &format!("@{second_name}")).is_none());
}

#[test]
fn embedded_helpers_return_structured_errors_for_invalid_input() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    for (script, expected_status) in [
        (reach_cli::docker::PAGE_TEXT_SCRIPT, "error"),
        (reach_cli::docker::PAGE_ACTION_SCRIPT, "error"),
        (
            reach_cli::injection::INJECTION_HELPER_SOURCE,
            "auth_required",
        ),
    ] {
        let mut child = Command::new("python3")
            .args(["-I", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Python 3 is required to verify embedded browser helpers");
        child.stdin.take().unwrap().write_all(b"{").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "helper crashed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let response: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["status"], expected_status);
    }
}
