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
        key: None,
        url: Some("https://example.com".to_string()),
        selector: Some("button#submit".to_string()),
        aria_tag: Some("Submit order".to_string()),
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

    let compiled = compile_trace(&loaded_trace, None).expect("compile trace");
    assert_eq!(compiled.name, "test_roundtrip");
    assert_eq!(compiled.steps.len(), 1);
    assert_eq!(compiled.steps[0].action.kind, "click");
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
