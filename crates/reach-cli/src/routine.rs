//! Reach Routine: Demonstration Recorder & Routine Compiler.
//!
//! Provides data models, serialization, parameter interpolation, and checkpoint
//! management for automated routines.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Default location for routines (~/.reach/routines).
pub fn default_routines_dir() -> PathBuf {
    if let Some(home) = dirs_home() {
        home.join(".reach").join("routines")
    } else {
        PathBuf::from(".reach").join("routines")
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Resolve directory path for a named routine.
pub fn routine_dir(name: &str, base: Option<&Path>) -> PathBuf {
    let base_path = match base {
        Some(b) => b.to_path_buf(),
        None => default_routines_dir(),
    };
    base_path.join(name)
}

/// Path to trace.json for a routine.
pub fn trace_path(name: &str, base: Option<&Path>) -> PathBuf {
    routine_dir(name, base).join("trace.json")
}

/// Path to routine.json for a routine.
pub fn routine_path(name: &str, base: Option<&Path>) -> PathBuf {
    routine_dir(name, base).join("routine.json")
}

/// Path to frames directory for a routine.
pub fn frames_dir(name: &str, base: Option<&Path>) -> PathBuf {
    routine_dir(name, base).join("frames")
}

/// Interpolate template placeholders like `{{var}}` or `{var}` with provided parameters.
pub fn render_template(template: &str, params: &HashMap<String, String>) -> String {
    let mut result = template.to_string();
    for (k, v) in params {
        let pattern_double = format!("{{{{{k}}}}}");
        let pattern_single = format!("{{{k}}}");
        result = result.replace(&pattern_double, v);
        result = result.replace(&pattern_single, v);
    }
    result
}

// -----------------------------------------------------------------------------
// Trace Models
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraceStep {
    pub step_index: usize,
    pub timestamp: String,
    pub action_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aria_tag: Option<String>,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_frame: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_frame: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dom_snapshot: Option<String>,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RoutineTrace {
    #[serde(default = "default_version")]
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub screen: u32,
    pub created_at: String,
    #[serde(default)]
    pub steps: Vec<TraceStep>,
}

fn default_version() -> u32 {
    1
}

// -----------------------------------------------------------------------------
// Compiled Routine Models
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Checkpoint {
    #[serde(rename = "type")]
    pub checkpoint_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_hash: Option<String>,
    #[serde(default = "default_threshold")]
    pub threshold: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_path: Option<String>,
    #[serde(default)]
    pub description: String,
}

fn default_threshold() -> f64 {
    0.20
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompiledAction {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<(i32, i32)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normalized_point: Option<(f64, f64)>,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aria: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default = "default_button")]
    pub button: String,
    #[serde(default)]
    pub description: String,
}

fn default_button() -> String {
    "left".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompiledStep {
    pub step_index: usize,
    pub action: CompiledAction,
    #[serde(default)]
    pub checkpoints: Vec<Checkpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompiledRoutine {
    #[serde(default = "default_version")]
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub screen: u32,
    pub compiled_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healed_at: Option<String>,
    #[serde(default)]
    pub parameters: HashMap<String, String>,
    #[serde(default)]
    pub steps: Vec<CompiledStep>,
}

// -----------------------------------------------------------------------------
// Serialization and Compilation Helpers
// -----------------------------------------------------------------------------

pub fn load_trace(path: &Path) -> Result<RoutineTrace> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read trace file at {}", path.display()))?;
    let trace: RoutineTrace = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse trace JSON at {}", path.display()))?;
    Ok(trace)
}

pub fn save_trace(path: &Path, trace: &RoutineTrace) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(trace)?;
    std::fs::write(path, json)?;
    Ok(())
}

pub fn load_routine(path: &Path) -> Result<CompiledRoutine> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read routine file at {}", path.display()))?;
    let routine: CompiledRoutine = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse routine JSON at {}", path.display()))?;
    Ok(routine)
}

pub fn save_routine(path: &Path, routine: &CompiledRoutine) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(routine)?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Compile a raw demonstration trace into a normalized, parameterizable routine.
pub fn compile_trace(
    trace: &RoutineTrace,
    param_keys: Option<&HashMap<String, String>>,
) -> Result<CompiledRoutine> {
    let mut parameters = HashMap::new();
    let mut compiled_steps = Vec::new();

    // Map of text value to variable name
    let mut val_to_var: HashMap<String, String> = HashMap::new();
    if let Some(keys) = param_keys {
        for (k, v) in keys {
            val_to_var.insert(k.clone(), v.clone());
        }
    }

    // Step 1: Detect parameters
    for step in &trace.steps {
        if step.action_type == "type"
            && let Some(txt) = &step.text
        {
            if !val_to_var.contains_key(txt) {
                let var_name = infer_param_name(step, parameters.len());
                val_to_var.insert(txt.clone(), var_name.clone());
            }
            let var_name = &val_to_var[txt];
            parameters.insert(var_name.clone(), txt.clone());
        }
    }

    // Step 2: Compile steps
    for step in &trace.steps {
        let mut value = step.text.clone();
        if step.action_type == "type"
            && let Some(txt) = &step.text
            && let Some(var_name) = val_to_var.get(txt)
        {
            value = Some(format!("{{{{{var_name}}}}}"));
        }

        let point = match (step.x, step.y) {
            (Some(x), Some(y)) => Some((x, y)),
            _ => None,
        };

        let normalized_point = point.map(|(x, y)| {
            (
                (x as f64 / 1280.0 * 10000.0).round() / 10000.0,
                (y as f64 / 720.0 * 10000.0).round() / 10000.0,
            )
        });

        let mut desc = format!("{} action", step.action_type);
        if let Some(r) = &step.reference {
            desc = format!("{} on ref '{}'", step.action_type, r);
        } else if let Some(sel) = &step.selector {
            desc = format!("{} on '{}'", step.action_type, sel);
        } else if let Some((x, y)) = point {
            desc = format!("{} at ({}, {})", step.action_type, x, y);
        }

        let action = CompiledAction {
            kind: step.action_type.clone(),
            point,
            normalized_point,
            reference: step.reference.clone(),
            url: step.url.clone(),
            selector: step.selector.clone(),
            aria: step.aria_tag.clone(),
            value,
            key: step.key.clone(),
            button: "left".to_string(),
            description: desc,
        };

        let mut checkpoints = Vec::new();
        if step.action_type == "navigate"
            && let Some(url) = &step.url
        {
            let domain = url
                .split("://")
                .last()
                .unwrap_or(url)
                .split('/')
                .next()
                .unwrap_or(url);
            checkpoints.push(Checkpoint {
                checkpoint_type: "url_contains".to_string(),
                value: Some(domain.to_string()),
                expected_hash: None,
                threshold: 0.20,
                frame_path: None,
                description: format!("Verify URL contains '{}'", domain),
            });
        }

        if let Some(frame) = &step.after_frame {
            checkpoints.push(Checkpoint {
                checkpoint_type: "visual_phash".to_string(),
                value: None,
                expected_hash: None,
                threshold: 0.20,
                frame_path: Some(frame.clone()),
                description: format!("Visual anchor verification for step {}", step.step_index),
            });
        }

        compiled_steps.push(CompiledStep {
            step_index: step.step_index,
            action,
            checkpoints,
        });
    }

    Ok(CompiledRoutine {
        version: 1,
        name: trace.name.clone(),
        screen: trace.screen,
        compiled_at: chrono::Utc::now().to_rfc3339(),
        healed_at: None,
        parameters,
        steps: compiled_steps,
    })
}

fn infer_param_name(step: &TraceStep, idx: usize) -> String {
    let context = format!(
        "{} {}",
        step.selector.as_deref().unwrap_or(""),
        step.aria_tag.as_deref().unwrap_or("")
    )
    .to_lowercase();

    if context.contains("search") || context.contains("query") || context.contains('q') {
        if idx == 0 {
            "query".to_string()
        } else {
            format!("query_{}", idx + 1)
        }
    } else if context.contains("email") {
        "email".to_string()
    } else if context.contains("user") {
        "username".to_string()
    } else if context.contains("company") {
        "company".to_string()
    } else {
        format!("param_{}", idx + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_template_interpolation() {
        let mut params = HashMap::new();
        params.insert("query".to_string(), "Tesla".to_string());
        params.insert("count".to_string(), "5".to_string());

        let t1 = "Search for {{query}} with count {count}";
        let rendered = render_template(t1, &params);
        assert_eq!(rendered, "Search for Tesla with count 5");
    }

    #[test]
    fn test_compile_trace_parameterization() {
        let step1 = TraceStep {
            step_index: 1,
            timestamp: "2026-09-05T00:00:00Z".to_string(),
            action_type: "navigate".to_string(),
            x: None,
            y: None,
            text: None,
            key: None,
            url: Some("https://www.google.com".to_string()),
            selector: None,
            aria_tag: None,
            reference: None,
            before_frame: None,
            after_frame: Some("frames/step_001_after.png".to_string()),
            dom_snapshot: None,
            metadata: HashMap::new(),
        };

        let step2 = TraceStep {
            step_index: 2,
            timestamp: "2026-09-05T00:00:05Z".to_string(),
            action_type: "type".to_string(),
            x: Some(640),
            y: Some(360),
            text: Some("Tesla".to_string()),
            key: None,
            url: Some("https://www.google.com".to_string()),
            selector: Some("input[name='q']".to_string()),
            aria_tag: Some("Search query".to_string()),
            reference: Some("@e2".to_string()),
            before_frame: None,
            after_frame: None,
            dom_snapshot: None,
            metadata: HashMap::new(),
        };

        let trace = RoutineTrace {
            version: 1,
            name: "test_search".to_string(),
            screen: 0,
            created_at: "2026-09-05T00:00:00Z".to_string(),
            steps: vec![step1, step2],
        };

        let compiled = compile_trace(&trace, None).unwrap();
        assert_eq!(compiled.parameters.get("query"), Some(&"Tesla".to_string()));
        assert_eq!(
            compiled.steps[1].action.value,
            Some("{{query}}".to_string())
        );
        assert_eq!(compiled.steps[1].action.normalized_point, Some((0.5, 0.5)));
        assert_eq!(compiled.steps[1].action.reference, Some("@e2".to_string()));
        assert_eq!(compiled.steps[1].action.description, "type on ref '@e2'");
    }
}
