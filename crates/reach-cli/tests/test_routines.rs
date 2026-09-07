use reach_cli::routine::{
    Checkpoint, RoutineTrace, TraceStep, compile_trace, default_routines_dir, frames_dir,
    load_routine, load_trace, render_template, routine_dir, routine_path, save_routine, save_trace,
    trace_path,
};
use std::collections::HashMap;

#[test]
fn test_routine_path_resolution() {
    let def_dir = default_routines_dir();
    assert!(def_dir.ends_with(".reach/routines"));

    let r_dir = routine_dir("test_routine", None);
    assert!(r_dir.ends_with(".reach/routines/test_routine"));

    let t_path = trace_path("test_routine", None);
    assert!(t_path.ends_with(".reach/routines/test_routine/trace.json"));

    let p_path = routine_path("test_routine", None);
    assert!(p_path.ends_with(".reach/routines/test_routine/routine.json"));

    let f_dir = frames_dir("test_routine", None);
    assert!(f_dir.ends_with(".reach/routines/test_routine/frames"));
}

#[test]
fn test_render_template() {
    let mut params = HashMap::new();
    params.insert("query".to_string(), "Tesla".to_string());
    params.insert("city".to_string(), "Austin".to_string());

    let template = "Search {{query}} in {city}";
    assert_eq!(render_template(template, &params), "Search Tesla in Austin");
}

#[test]
fn test_trace_and_routine_serde_roundtrip() {
    let tmp = std::env::temp_dir().join(format!("reach-routine-test-{}", uuid::Uuid::new_v4()));
    let _ = std::fs::create_dir_all(&tmp);

    let step = TraceStep {
        step_index: 1,
        timestamp: "2026-09-05T00:00:00Z".to_string(),
        action_type: "click".to_string(),
        x: Some(500),
        y: Some(300),
        text: None,
        input_name: None,
        key: None,
        url: Some("https://example.com".to_string()),
        selector: Some("button#submit".to_string()),
        aria_tag: Some("Submit order".to_string()),
        reference: Some("@e1".to_string()),
        before_frame: Some("frames/step_001_before.png".to_string()),
        after_frame: Some("frames/step_001_after.png".to_string()),
        dom_snapshot: Some("<div>Submit</div>".to_string()),
        metadata: HashMap::new(),
    };

    let trace = RoutineTrace {
        version: 1,
        name: "test_roundtrip".to_string(),
        screen: 0,
        created_at: "2026-09-05T00:00:00Z".to_string(),
        steps: vec![step],
    };

    let trace_file = tmp.join("trace.json");
    save_trace(&trace_file, &trace).expect("save trace");
    let loaded_trace = load_trace(&trace_file).expect("load trace");
    assert_eq!(loaded_trace.name, "test_roundtrip");
    assert_eq!(loaded_trace.steps.len(), 1);
    assert_eq!(loaded_trace.steps[0].x, Some(500));
    assert_eq!(loaded_trace.steps[0].reference, Some("@e1".to_string()));

    let compiled = compile_trace(&loaded_trace, None).expect("compile trace");
    assert_eq!(compiled.name, "test_roundtrip");
    assert_eq!(compiled.steps.len(), 1);
    assert_eq!(compiled.steps[0].action.kind, "click");
    assert_eq!(compiled.steps[0].action.reference, Some("@e1".to_string()));
    assert_eq!(compiled.steps[0].action.description, "click on ref '@e1'");
    assert_eq!(
        compiled.steps[0].action.selector,
        Some("button#submit".to_string())
    );

    let routine_file = tmp.join("routine.json");
    save_routine(&routine_file, &compiled).expect("save routine");
    let loaded_routine = load_routine(&routine_file).expect("load routine");
    assert_eq!(loaded_routine.version, 1);
    assert_eq!(loaded_routine.steps[0].action.kind, "click");

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn test_checkpoint_types_and_defaults() {
    let cp = Checkpoint {
        checkpoint_type: "visual_phash".to_string(),
        value: None,
        expected_hash: Some("a1b2c3d4e5f60718".to_string()),
        threshold: 0.20,
        frame_path: Some("frames/step_001_after.png".to_string()),
        description: "Visual verification".to_string(),
    };

    let json = serde_json::to_string(&cp).unwrap();
    assert!(json.contains("\"type\":\"visual_phash\""));
    assert!(json.contains("\"expected_hash\":\"a1b2c3d4e5f60718\""));

    let deserialized: Checkpoint = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.checkpoint_type, "visual_phash");
    assert_eq!(deserialized.threshold, 0.20);
}

#[test]
fn test_native_persistence_redacts_values_and_keeps_required_inputs() {
    let tmp = std::env::temp_dir().join(format!("reach-routine-privacy-{}", uuid::Uuid::new_v4()));
    let _ = std::fs::create_dir_all(&tmp);
    let canary = "typed-secret-canary";
    let navigation = "https://user:password@example.com/private/path?token=url-secret#fragment";
    let trace = RoutineTrace {
        version: 1,
        name: "privacy".to_string(),
        screen: 2,
        created_at: "2026-09-05T00:00:00Z".to_string(),
        steps: vec![
            TraceStep {
                step_index: 1,
                timestamp: "2026-09-05T00:00:00Z".to_string(),
                action_type: "navigate".to_string(),
                x: None,
                y: None,
                text: None,
                input_name: None,
                key: None,
                url: Some(navigation.to_string()),
                selector: None,
                aria_tag: None,
                reference: None,
                before_frame: Some(canary.to_string()),
                after_frame: Some(canary.to_string()),
                dom_snapshot: Some(canary.to_string()),
                metadata: HashMap::from([
                    ("arbitrary_text".to_string(), serde_json::json!(canary)),
                    ("dom_keywords".to_string(), serde_json::json!([canary])),
                    ("after_frame_hash".to_string(), serde_json::json!(canary)),
                ]),
            },
            TraceStep {
                step_index: 2,
                timestamp: "2026-09-05T00:00:01Z".to_string(),
                action_type: "type".to_string(),
                x: Some(10),
                y: Some(20),
                text: Some(canary.to_string()),
                input_name: Some("query".to_string()),
                key: None,
                url: Some(navigation.to_string()),
                selector: Some("input[name=q]".to_string()),
                aria_tag: Some("Search".to_string()),
                reference: Some("@query".to_string()),
                before_frame: None,
                after_frame: None,
                dom_snapshot: None,
                metadata: HashMap::new(),
            },
        ],
    };

    let direct = compile_trace(&trace, None).expect("compile in-memory trace");
    assert_eq!(direct.steps[0].checkpoints.len(), 1);
    let direct_json = serde_json::to_string(&direct).expect("serialize in-memory routine");
    assert!(!direct_json.contains(canary));
    assert!(!direct_json.contains("url-secret"));
    assert!(!direct_json.contains("/private/path"));
    let trace_file = tmp.join("trace.json");
    save_trace(&trace_file, &trace).expect("save private trace");
    let trace_json = std::fs::read_to_string(&trace_file).expect("read trace");
    let trace_doc: serde_json::Value = serde_json::from_str(&trace_json).expect("parse trace");
    assert!(!trace_json.contains(canary));
    assert!(!trace_json.contains("url-secret"));
    assert!(!trace_json.contains("/private/path"));
    assert_eq!(
        trace_doc["steps"][0]["input_name"],
        serde_json::json!("url_1")
    );
    assert_eq!(
        trace_doc["steps"][0]["url"],
        serde_json::json!("https://example.com")
    );

    let loaded = load_trace(&trace_file).expect("load redacted trace");
    let compiled = compile_trace(&loaded, None).expect("compile redacted trace");
    assert_eq!(
        compiled.steps[0].checkpoints[0].checkpoint_type,
        "url_origin_equals"
    );
    assert_eq!(
        compiled.steps[0].checkpoints[0].value.as_deref(),
        Some("https://example.com")
    );
    assert_eq!(
        compiled.steps[0].action.input_name.as_deref(),
        Some("url_1")
    );
    assert_eq!(compiled.steps[1].action.value.as_deref(), Some("{{query}}"));

    let routine_file = tmp.join("routine.json");
    save_routine(&routine_file, &compiled).expect("save private routine");
    let routine_json = std::fs::read_to_string(&routine_file).expect("read routine");
    let routine_doc: serde_json::Value =
        serde_json::from_str(&routine_json).expect("parse routine");
    assert!(!routine_json.contains(canary));
    assert!(!routine_json.contains("url-secret"));
    assert!(!routine_json.contains("/private/path"));
    assert!(routine_doc["parameters"]["url_1"].is_null());
    assert!(routine_doc["parameters"]["query"].is_null());
    assert_eq!(
        routine_doc["steps"][0]["checkpoints"][0]["type"],
        serde_json::json!("url_origin_equals")
    );

    let _ = std::fs::remove_dir_all(&tmp);
}
