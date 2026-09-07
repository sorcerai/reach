//! Reach Routine: Demonstration Recorder & Routine Compiler.
//!
//! Provides data models, serialization, parameter interpolation, and checkpoint
//! management for automated routines.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
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
/// Canonicalize a retained navigation origin without persisting userinfo or
/// path/query/fragment data.
fn safe_origin(value: &str) -> Option<String> {
    let parsed = url::Url::parse(value).ok()?;
    if !matches!(parsed.scheme(), "http" | "https" | "ws" | "wss") {
        return None;
    }
    let mut host = parsed.host_str()?.to_string();
    if host.contains(':') && !host.starts_with('[') {
        host = format!("[{host}]");
    }
    let mut origin = format!("{}://{host}", parsed.scheme().to_ascii_lowercase());
    if let Some(port) = parsed.port() {
        origin.push_str(&format!(":{port}"));
    }
    Some(origin)
}

fn navigation_needs_runtime_url(value: &str) -> bool {
    let Ok(parsed) = url::Url::parse(value) else {
        return false;
    };
    !parsed.username().is_empty()
        || parsed.password().is_some()
        || (parsed.path() != "" && parsed.path() != "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
}

fn safe_input_name(value: &str) -> Option<String> {
    (!value.is_empty() && value.len() <= 128).then(|| value.to_string())
}

const SAFE_DOM_KEYWORDS: &[&str] = &[
    "success",
    "dashboard",
    "results",
    "welcome",
    "account",
    "profile",
];

fn safe_dom_keyword(value: &Value) -> Option<String> {
    let keyword = value.as_str()?.to_ascii_lowercase();
    SAFE_DOM_KEYWORDS
        .contains(&keyword.as_str())
        .then_some(keyword)
}

fn safe_phash(value: &Value) -> Option<String> {
    let hash = value.as_str()?;
    (hash.len() == 16
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then(|| hash.to_string())
}

fn assigned_input_names(
    trace: &RoutineTrace,
    param_keys: Option<&HashMap<String, String>>,
) -> Vec<Option<String>> {
    let mut used = trace
        .steps
        .iter()
        .filter_map(|step| step.input_name.as_deref().and_then(safe_input_name))
        .collect::<HashSet<_>>();
    if let Some(keys) = param_keys {
        used.extend(keys.values().filter_map(|name| safe_input_name(name)));
    }
    let mut by_text: HashMap<String, String> = HashMap::new();
    let mut by_url: HashMap<String, String> = HashMap::new();
    let mut next_url = 1usize;
    let mut assigned = Vec::with_capacity(trace.steps.len());

    for step in &trace.steps {
        let explicit = step
            .input_name
            .as_deref()
            .and_then(safe_input_name)
            .or_else(|| {
                let value = match step.action_type.as_str() {
                    "type" => step.text.as_ref(),
                    "navigate" => step.url.as_ref(),
                    _ => None,
                }?;
                param_keys?
                    .get(value)
                    .and_then(|name| safe_input_name(name))
            });
        let name = explicit.or_else(|| {
            if step.action_type == "type" && step.text.is_some() {
                if let Some(existing) = step.text.as_ref().and_then(|text| by_text.get(text)) {
                    return Some(existing.clone());
                }
                let mut candidate = infer_param_name(step, assigned.len());
                let mut suffix = 2usize;
                while used.contains(&candidate) {
                    candidate = format!("{}_{}", infer_param_name(step, assigned.len()), suffix);
                    suffix += 1;
                }
                used.insert(candidate.clone());
                if let Some(text) = &step.text {
                    by_text.insert(text.clone(), candidate.clone());
                }
                return Some(candidate);
            }
            if step.action_type == "navigate"
                && step
                    .url
                    .as_deref()
                    .is_some_and(navigation_needs_runtime_url)
            {
                let url = step.url.as_ref()?;
                if let Some(existing) = by_url.get(url) {
                    return Some(existing.clone());
                }
                let candidate = loop {
                    let candidate = format!("url_{next_url}");
                    next_url += 1;
                    if !used.contains(&candidate) {
                        break candidate;
                    }
                };
                used.insert(candidate.clone());
                by_url.insert(url.clone(), candidate.clone());
                return Some(candidate);
            }
            None
        });
        if let Some(name) = &name {
            if step.action_type == "type" {
                if let Some(text) = &step.text {
                    by_text.insert(text.clone(), name.clone());
                }
            } else if step.action_type == "navigate"
                && let Some(url) = &step.url
            {
                by_url.insert(url.clone(), name.clone());
            }
        }
        assigned.push(name);
    }
    assigned
}

fn safe_metadata(value: &serde_json::Value) -> Option<serde_json::Value> {
    const SAFE_KEYS: &[&str] = &[
        "source",
        "dom_keywords",
        "before_frame_hash",
        "after_frame_hash",
        "credential_field",
    ];
    match value {
        Value::Object(items) => {
            let mut output = Map::new();
            for (key, item) in items {
                let lowered = key.to_ascii_lowercase();
                if !SAFE_KEYS.contains(&lowered.as_str()) {
                    continue;
                }
                if lowered == "dom_keywords" {
                    let Some(values) = item.as_array() else {
                        continue;
                    };
                    let keywords = values
                        .iter()
                        .filter_map(safe_dom_keyword)
                        .take(4)
                        .map(Value::String)
                        .collect::<Vec<_>>();
                    if !keywords.is_empty() {
                        output.insert(key.clone(), Value::Array(keywords));
                    }
                } else if lowered == "before_frame_hash" || lowered == "after_frame_hash" {
                    if let Some(hash) = safe_phash(item) {
                        output.insert(key.clone(), Value::String(hash));
                    }
                } else if matches!(item, Value::String(_) | Value::Bool(_) | Value::Number(_)) {
                    output.insert(key.clone(), item.clone());
                }
            }
            (!output.is_empty()).then_some(Value::Object(output))
        }
        _ => None,
    }
}
fn routine_lock(path: &Path) -> Result<std::fs::File> {
    let parent = path
        .parent()
        .context("routine persistence path has no parent directory")?;
    let mut directories = std::fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories
        .create(parent)
        .with_context(|| format!("failed to create routine directory {}", parent.display()))?;

    let lock_path = parent.join(".routine.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&lock_path)
        .with_context(|| format!("failed to open routine lock {}", lock_path.display()))?;
    fs4::FileExt::lock(&file)
        .with_context(|| format!("failed to lock routine {}", lock_path.display()))?;
    Ok(file)
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("routine persistence path has no parent directory")?;
    let mut directories = std::fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directories.mode(0o700);
    }
    directories
        .create(parent)
        .with_context(|| format!("failed to create routine directory {}", parent.display()))?;

    let temporary = parent.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("routine"),
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).with_context(|| {
            format!(
                "failed to create private routine file {}",
                temporary.display()
            )
        })?;
        file.write_all(bytes)
            .context("failed to write private routine file")?;
        file.sync_all()
            .context("failed to sync private routine file")?;
        drop(file);
        std::fs::rename(&temporary, path)
            .with_context(|| format!("failed to atomically install routine {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .with_context(|| format!("failed to protect routine file {}", path.display()))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
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
    #[serde(skip_serializing_if = "Option::is_none", alias = "parameter")]
    pub input_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(skip)]
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
    #[serde(skip)]
    pub aria: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", alias = "parameter")]
    pub input_name: Option<String>,
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
    pub parameters: HashMap<String, Option<String>>,
    #[serde(default)]
    pub steps: Vec<CompiledStep>,
}

// -----------------------------------------------------------------------------
// Serialization and Compilation Helpers
// -----------------------------------------------------------------------------

fn persisted_trace(trace: &RoutineTrace) -> Value {
    let input_names = assigned_input_names(trace, None);
    let steps = trace
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let mut output = Map::new();
            output.insert("step_index".into(), json!(step.step_index));
            output.insert("timestamp".into(), json!(step.timestamp));
            output.insert("action_type".into(), json!(step.action_type));
            if let Some(x) = step.x {
                output.insert("x".into(), json!(x));
            }
            if let Some(y) = step.y {
                output.insert("y".into(), json!(y));
            }
            if let Some(key) = &step.key {
                output.insert("key".into(), json!(key));
            }
            if let Some(origin) = step.url.as_deref().and_then(safe_origin) {
                output.insert("url".into(), json!(origin));
            }
            if let Some(selector) = &step.selector {
                output.insert("selector".into(), json!(selector));
            }
            if let Some(reference) = &step.reference {
                output.insert("ref".into(), json!(reference));
            }
            if let Some(input_name) = input_names[index].clone() {
                output.insert("input_name".into(), json!(input_name));
            }
            let metadata = Value::Object(step.metadata.clone().into_iter().collect());
            if let Some(metadata) = safe_metadata(&metadata) {
                output.insert("metadata".into(), metadata);
            }
            Value::Object(output)
        })
        .collect::<Vec<_>>();
    json!({
        "version": trace.version,
        "name": trace.name,
        "screen": trace.screen,
        "created_at": trace.created_at,
        "steps": steps,
    })
}

fn persisted_checkpoint(checkpoint: &Checkpoint) -> Value {
    let mut output = Map::new();
    output.insert("type".into(), json!(checkpoint.checkpoint_type));
    if checkpoint.checkpoint_type == "url_origin_equals" {
        if let Some(origin) = checkpoint.value.as_deref().and_then(safe_origin) {
            output.insert("value".into(), json!(origin));
        }
    } else if checkpoint.checkpoint_type == "text_contains"
        && let Some(value) = checkpoint.value.as_deref()
        && let Some(keyword) = safe_dom_keyword(&Value::String(value.to_string()))
    {
        output.insert("value".into(), json!(keyword));
    }
    if checkpoint.checkpoint_type == "visual_phash"
        && let Some(hash) = checkpoint
            .expected_hash
            .as_ref()
            .and_then(|hash| safe_phash(&Value::String(hash.clone())))
    {
        output.insert("expected_hash".into(), json!(hash));
    }
    output.insert("threshold".into(), json!(checkpoint.threshold));
    output.insert("description".into(), json!("Verify checkpoint"));
    Value::Object(output)
}

fn persisted_action(action: &CompiledAction) -> Value {
    let mut output = Map::new();
    output.insert("kind".into(), json!(action.kind));
    if let Some(point) = action.point {
        output.insert("point".into(), json!([point.0, point.1]));
    }
    if let Some(point) = action.normalized_point {
        output.insert("normalized_point".into(), json!([point.0, point.1]));
    }
    if let Some(reference) = &action.reference {
        output.insert("ref".into(), json!(reference));
    }
    if let Some(origin) = action.url.as_deref().and_then(safe_origin) {
        output.insert("url".into(), json!(origin));
    }
    if let Some(selector) = &action.selector {
        output.insert("selector".into(), json!(selector));
    }
    if let Some(input_name) = action.input_name.as_deref().and_then(safe_input_name) {
        output.insert("input_name".into(), json!(input_name.clone()));
        if action.kind == "type" {
            output.insert("value".into(), json!(format!("{{{{{input_name}}}}}")));
        }
    }
    if let Some(key) = &action.key {
        output.insert("key".into(), json!(key));
    }
    output.insert("button".into(), json!(action.button));
    Value::Object(output)
}

fn persisted_routine(routine: &CompiledRoutine) -> Value {
    let parameters = routine
        .parameters
        .keys()
        .filter_map(|name| safe_input_name(name).map(|name| (name, Value::Null)))
        .collect::<Map<String, Value>>();
    let steps = routine
        .steps
        .iter()
        .map(|step| {
            json!({
                "step_index": step.step_index,
                "action": persisted_action(&step.action),
                "checkpoints": step.checkpoints.iter().map(persisted_checkpoint).collect::<Vec<_>>(),
            })
        })
        .collect::<Vec<_>>();
    let mut output = Map::new();
    output.insert("version".into(), json!(routine.version));
    output.insert("name".into(), json!(routine.name));
    output.insert("screen".into(), json!(routine.screen));
    output.insert("compiled_at".into(), json!(routine.compiled_at));
    if let Some(healed_at) = &routine.healed_at {
        output.insert("healed_at".into(), json!(healed_at));
    }
    output.insert("parameters".into(), Value::Object(parameters));
    output.insert("steps".into(), Value::Array(steps));
    Value::Object(output)
}

pub fn load_trace(path: &Path) -> Result<RoutineTrace> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read trace file at {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse trace JSON at {}", path.display()))
}

pub fn save_trace(path: &Path, trace: &RoutineTrace) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(&persisted_trace(trace))?;
    let _lock = routine_lock(path)?;
    write_private_atomic(path, &bytes)
}

pub fn load_routine(path: &Path) -> Result<CompiledRoutine> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read routine file at {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse routine JSON at {}", path.display()))
}
pub fn save_routine(path: &Path, routine: &CompiledRoutine) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(&persisted_routine(routine))?;
    let _lock = routine_lock(path)?;
    write_private_atomic(path, &bytes)
}

/// Compile a raw demonstration trace into a normalized, parameterizable routine.
pub fn compile_trace(
    trace: &RoutineTrace,
    param_keys: Option<&HashMap<String, String>>,
) -> Result<CompiledRoutine> {
    let input_names = assigned_input_names(trace, param_keys);
    let parameters = trace
        .steps
        .iter()
        .zip(&input_names)
        .filter(|(step, _)| matches!(step.action_type.as_str(), "type" | "navigate"))
        .filter_map(|(_, name)| name.clone().map(|name| (name, None)))
        .collect();

    let mut compiled_steps = Vec::new();
    for (index, step) in trace.steps.iter().enumerate() {
        let input_name = input_names[index].clone();
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
        let mut description = format!("{} action", step.action_type);
        if let Some(reference) = &step.reference {
            description = format!("{} on ref '{}'", step.action_type, reference);
        } else if let Some(selector) = &step.selector {
            description = format!("{} on '{}'", step.action_type, selector);
        } else if let Some((x, y)) = point {
            description = format!("{} at ({}, {})", step.action_type, x, y);
        }
        let action = CompiledAction {
            kind: step.action_type.clone(),
            point,
            normalized_point,
            reference: step.reference.clone(),
            url: step.url.as_deref().and_then(safe_origin),
            selector: step.selector.clone(),
            aria: step.aria_tag.clone(),
            value: (step.action_type == "type")
                .then(|| input_name.as_ref().map(|name| format!("{{{{{name}}}}}")))
                .flatten(),
            input_name,
            key: step.key.clone(),
            button: "left".to_string(),
            description,
        };

        let mut checkpoints = Vec::new();
        if step.action_type == "navigate"
            && let Some(origin) = step.url.as_deref().and_then(safe_origin)
        {
            checkpoints.push(Checkpoint {
                checkpoint_type: "url_origin_equals".to_string(),
                value: Some(origin),
                expected_hash: None,
                threshold: 0.20,
                frame_path: None,
                description: "Verify origin".to_string(),
            });
        }
        if let Some(keyword) = step
            .metadata
            .get("dom_keywords")
            .and_then(Value::as_array)
            .and_then(|keywords| keywords.iter().find_map(safe_dom_keyword))
        {
            checkpoints.push(Checkpoint {
                checkpoint_type: "text_contains".to_string(),
                value: Some(keyword),
                expected_hash: None,
                threshold: 0.20,
                frame_path: None,
                description: "Verify operational page keyword".to_string(),
            });
        }
        if let Some(hash) = step.metadata.get("after_frame_hash").and_then(safe_phash) {
            checkpoints.push(Checkpoint {
                checkpoint_type: "visual_phash".to_string(),
                value: None,
                expected_hash: Some(hash),
                threshold: 0.20,
                frame_path: None,
                description: "Verify live visual anchor".to_string(),
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
            input_name: None,
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
            input_name: None,
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

        let compiled = compile_trace(
            &trace,
            Some(&HashMap::from([("Tesla".to_string(), "query".to_string())])),
        )
        .unwrap();
        assert_eq!(compiled.parameters.get("query"), Some(&None));
        assert_eq!(
            compiled.steps[1].action.value,
            Some("{{query}}".to_string())
        );
        assert_eq!(compiled.steps[1].action.normalized_point, Some((0.5, 0.5)));
        assert_eq!(compiled.steps[1].action.reference, Some("@e2".to_string()));
        assert_eq!(compiled.steps[1].action.description, "type on ref '@e2'");
    }
}
