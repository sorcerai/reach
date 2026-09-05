use reach_cli::docker::PageTextOutput;
use reach_cli::mcp::{ClickParams, TypeParams};
use reach_cli::refs::{ElementRef, global_ref_table, resolve_ref};
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
        point: None,
        box_bounds: Some([100, 200, 80, 30]),
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
            point: Some([500, 300]),
            box_bounds: Some([450, 280, 100, 40]),
            focused: false,
            disabled: false,
        },
    );
    let output = PageTextOutput {
        status: "ok".into(),
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
fn test_global_ref_table_and_resolve_ref_fallback() {
    let target = "test-sandbox-ref-actions";
    let screen = 0;
    let mut refs = HashMap::new();
    refs.insert(
        "e4".into(),
        ElementRef {
            r#ref: "e4".into(),
            role: "textbox".into(),
            name: "Search".into(),
            value: None,
            selector: None,
            point: Some([600, 150]),
            box_bounds: Some([500, 130, 200, 40]),
            focused: false,
            disabled: false,
        },
    );

    global_ref_table().set_refs(target, screen, refs.clone());

    // Look up with @e4 and e4
    let found1 = resolve_ref(target, screen, "@e4").expect("must resolve @e4");
    assert_eq!(found1.name, "Search");
    assert_eq!(found1.target_coordinates(), Some((600, 150)));

    let found2 = resolve_ref(target, screen, "e4").expect("must resolve e4");
    assert_eq!(found2.name, "Search");

    // Unknown ref
    assert!(resolve_ref(target, screen, "@e999").is_none());
}
