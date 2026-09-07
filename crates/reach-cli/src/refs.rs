use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, RwLock};

/// Semantic element reference captured from an Accessibility Tree (AXTree) snapshot.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ElementRef {
    pub r#ref: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub selector: Option<String>,
    #[serde(default)]
    pub backend_node_id: Option<i64>,
    #[serde(default)]
    pub point: Option<[f64; 2]>,
    #[serde(default)]
    pub box_bounds: Option<[f64; 4]>, // [x, y, width, height]
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub disabled: bool,
}

impl ElementRef {
    /// Return the click target coordinates `(x, y)`.
    /// Prefers explicit `point`, then center of `box_bounds`.
    pub fn target_coordinates(&self) -> Option<(i64, i64)> {
        if let Some([px, py]) = self.point {
            return Some((px.round() as i64, py.round() as i64));
        }
        if let Some([bx, by, bw, bh]) = self.box_bounds {
            return Some((
                (bx + bw / 2.0).round() as i64,
                (by + bh / 2.0).round() as i64,
            ));
        }
        None
    }
}

/// Identity of the observation that produced a reference set.
///
/// The container incarnation prevents refs surviving a computer restart. The lease attempt
/// prevents refs crossing account/task leases, and the handoff generation prevents refs crossing
/// human takeovers, releases, and acknowledgements.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct RefScope {
    pub incarnation: String,
    pub screen: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_gen: Option<u64>,
    /// Generation of the successful page observation that produced these refs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_gen: Option<u64>,
}

impl RefScope {
    pub fn new(
        incarnation: impl Into<String>,
        screen: u32,
        attempt_id: Option<String>,
        handoff_gen: Option<u64>,
    ) -> Self {
        Self {
            incarnation: incarnation.into(),
            screen,
            attempt_id: attempt_id.filter(|value| !value.is_empty()),
            handoff_gen,
            observation_gen: None,
        }
    }
    pub fn with_observation_gen(mut self, observation_gen: Option<u64>) -> Self {
        self.observation_gen = observation_gen;
        self
    }

    fn storage_key(&self) -> String {
        let encoded = serde_json::to_vec(self).expect("RefScope serializes");
        let digest = Sha256::digest(encoded);
        digest.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// Result of registering an observation. `token_map` maps the browser-produced refs to the
/// fresh numeric refs returned to the model; `refs` contains the corresponding backend entries.
#[derive(Debug, Clone, Default)]
pub struct SnapshotRefs {
    pub refs: ScreenRefMap,
    pub token_map: HashMap<String, String>,
}

/// Normalize an element reference by trimming whitespace and stripping any leading `@`.
/// E.g. `"@e14"` -> `"e14"`, `"e14"` -> `"e14"`.
pub fn normalize_ref(r: &str) -> &str {
    let trimmed = r.trim();
    trimmed.strip_prefix('@').unwrap_or(trimmed)
}

fn is_numeric_ref(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('e') else {
        return false;
    };
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

pub type ScreenRefMap = HashMap<String, ElementRef>;
pub type TargetScreenKey = RefScope;

#[derive(Debug)]
struct RefTableInner {
    entries: RwLock<HashMap<TargetScreenKey, ScreenRefMap>>,
    page_targets: RwLock<HashMap<TargetScreenKey, PageIdentity>>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PageIdentity {
    pub target_id: String,
    #[serde(default)]
    pub loader_id: Option<String>,
}

/// In-memory table storing active semantic refs per observation identity.
#[derive(Debug, Clone)]
pub struct RefTable {
    inner: Arc<RefTableInner>,
}

impl Default for RefTable {
    fn default() -> Self {
        Self {
            inner: Arc::new(RefTableInner {
                entries: RwLock::new(HashMap::new()),
                page_targets: RwLock::new(HashMap::new()),
            }),
        }
    }
}

impl RefTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace all registered refs for an observation and assign fresh numeric references.
    pub fn set_refs(&self, scope: RefScope, refs: HashMap<String, ElementRef>) -> SnapshotRefs {
        self.clear_screen(&scope.incarnation, scope.screen);
        let mut normalized = HashMap::with_capacity(refs.len());
        let mut token_map = HashMap::with_capacity(refs.len());

        for (source_key, mut element) in refs {
            let source = normalize_ref(&source_key).to_string();
            if !is_numeric_ref(&source) {
                continue;
            }
            let assigned = format!("e{}", uuid::Uuid::new_v4().as_u128());
            element.r#ref = assigned.clone();
            token_map.insert(source, assigned.clone());
            normalized.insert(assigned, element);
        }

        self.inner
            .entries
            .write()
            .expect("reference table lock poisoned")
            .insert(scope.clone(), normalized.clone());
        self.inner
            .page_targets
            .write()
            .expect("reference table lock poisoned")
            .remove(&scope);

        SnapshotRefs {
            refs: normalized,
            token_map,
        }
    }

    /// Register the native page target associated with a snapshot.
    pub fn set_page_identity(
        &self,
        scope: RefScope,
        target_id: Option<String>,
        loader_id: Option<String>,
    ) {
        let mut targets = self
            .inner
            .page_targets
            .write()
            .expect("reference table lock poisoned");
        match (
            target_id.filter(|id| !id.is_empty()),
            loader_id.filter(|id| !id.is_empty()),
        ) {
            (Some(target_id), loader_id) => {
                targets.insert(
                    scope,
                    PageIdentity {
                        target_id,
                        loader_id,
                    },
                );
            }
            _ => {
                targets.remove(&scope);
            }
        }
    }

    pub fn set_page_target(&self, scope: RefScope, target_id: Option<String>) {
        self.set_page_identity(scope, target_id, None);
    }

    pub fn page_identity(&self, scope: &RefScope) -> Option<PageIdentity> {
        self.inner
            .page_targets
            .read()
            .expect("reference table lock poisoned")
            .get(scope)
            .cloned()
    }

    pub fn page_target(&self, scope: &RefScope) -> Option<String> {
        self.page_identity(scope).map(|identity| identity.target_id)
    }

    /// Retrieve an `ElementRef` by numeric ref for a given observation.
    pub fn get_ref(&self, scope: &RefScope, ref_name: &str) -> Option<ElementRef> {
        let clean = normalize_ref(ref_name);
        if !is_numeric_ref(clean) {
            return None;
        }
        self.inner
            .entries
            .read()
            .expect("reference table lock poisoned")
            .get(scope)
            .and_then(|screen_refs| screen_refs.get(clean).cloned())
    }

    /// List all registered `ElementRef` items for a given observation.
    pub fn list_refs(&self, scope: &RefScope) -> Vec<ElementRef> {
        self.inner
            .entries
            .read()
            .expect("reference table lock poisoned")
            .get(scope)
            .map(|screen_refs| screen_refs.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Clear all registered refs for a given observation.
    pub fn clear(&self, scope: &RefScope) {
        self.inner
            .entries
            .write()
            .expect("reference table lock poisoned")
            .remove(scope);
        self.inner
            .page_targets
            .write()
            .expect("reference table lock poisoned")
            .remove(scope);
    }
    /// Clear all refs and native identities for one screen, regardless of observation generation.
    pub fn clear_screen(&self, incarnation: &str, screen: u32) {
        let scopes: Vec<_> = self
            .inner
            .entries
            .read()
            .expect("reference table lock poisoned")
            .keys()
            .filter(|scope| scope.incarnation == incarnation && scope.screen == screen)
            .cloned()
            .collect();
        for scope in scopes {
            self.clear(&scope);
        }
    }
}

static GLOBAL_REF_TABLE: LazyLock<RefTable> = LazyLock::new(RefTable::new);

/// Access the global shared reference table.
pub fn global_ref_table() -> &'static RefTable {
    &GLOBAL_REF_TABLE
}

/// Rewrite every exact `@e<number>` token in an AXTree using the fresh numeric refs.
/// Non-reference text and unknown/non-token-looking strings remain byte-for-byte unchanged.
pub fn rewrite_axtree(axtree: &str, token_map: &HashMap<String, String>) -> String {
    let mut output = String::with_capacity(axtree.len());
    let mut cursor = 0;
    while cursor < axtree.len() {
        let remaining = &axtree[cursor..];
        let Some(relative_start) = remaining.find("@e") else {
            output.push_str(remaining);
            break;
        };
        let start = cursor + relative_start;
        output.push_str(&axtree[cursor..start]);

        let after_prefix = start + 2;
        let digits_end = after_prefix
            + axtree[after_prefix..]
                .bytes()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
        if digits_end == after_prefix {
            output.push_str("@e");
            cursor = after_prefix;
            continue;
        }

        let is_token_boundary = axtree[digits_end..]
            .chars()
            .next()
            .map(|ch| !(ch.is_ascii_alphanumeric() || ch == '_'))
            .unwrap_or(true);
        let source = &axtree[start + 1..digits_end];
        if is_token_boundary {
            if let Some(replacement) = token_map.get(source) {
                output.push('@');
                output.push_str(replacement);
            } else {
                output.push_str(&axtree[start..digits_end]);
            }
        } else {
            output.push_str(&axtree[start..digits_end]);
        }
        cursor = digits_end;
    }
    output
}

fn ref_storage_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("REACH_REFS_DIR") {
        return Some(PathBuf::from(path));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".reach").join("refs"))
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredElementRef {
    pub r#ref: String,
    pub point: Option<[f64; 2]>,
    pub box_bounds: Option<[f64; 4]>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredSnapshot {
    scope: RefScope,
    refs: HashMap<String, StoredElementRef>,
}

/// Persist only the opaque ref identity and geometry. Text, selectors, values, and state flags
/// never enter the durable cache. The scope digest also avoids unsafe filename interpolation.
pub fn save_refs_to_disk(scope: &RefScope, refs: &HashMap<String, ElementRef>) {
    let Some(dir) = ref_storage_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let file_path = dir.join(format!("scope_{}.json", scope.storage_key()));
    let stored_refs = refs
        .iter()
        .filter(|(key, _)| is_numeric_ref(key))
        .map(|(key, element)| {
            (
                key.clone(),
                StoredElementRef {
                    // The map key is the only accepted opaque identity; do not persist an
                    // untrusted ElementRef.r#ref field.
                    r#ref: key.clone(),
                    point: element.point,
                    box_bounds: element.box_bounds,
                },
            )
        })
        .collect();
    let snapshot = StoredSnapshot {
        scope: scope.clone(),
        refs: stored_refs,
    };
    if let Ok(data) = serde_json::to_string(&snapshot) {
        let _ = std::fs::write(file_path, data);
    }
}

/// Load a scoped snapshot from disk, returning only geometry and opaque identity.
pub fn load_refs_from_disk(scope: &RefScope) -> Option<HashMap<String, ElementRef>> {
    let dir = ref_storage_dir()?;
    let file_path = dir.join(format!("scope_{}.json", scope.storage_key()));
    let data = std::fs::read_to_string(file_path).ok()?;
    let snapshot: StoredSnapshot = serde_json::from_str(&data).ok()?;
    if snapshot.scope != *scope {
        return None;
    }
    Some(
        snapshot
            .refs
            .into_iter()
            .map(|(key, stored)| {
                (
                    key,
                    ElementRef {
                        r#ref: stored.r#ref,
                        role: String::new(),
                        name: String::new(),
                        value: None,
                        selector: None,
                        backend_node_id: None,
                        point: stored.point,
                        box_bounds: stored.box_bounds,
                        focused: false,
                        disabled: false,
                    },
                )
            })
            .collect(),
    )
}

/// Resolve only refs from the currently registered observation. Persisted snapshots are never
/// hydrated implicitly, so a prior observation cannot become actionable accidentally.
pub fn resolve_ref(scope: &RefScope, ref_name: &str) -> Option<ElementRef> {
    global_ref_table().get_ref(scope, ref_name)
}

/// Resolve the native target identity captured for a snapshot.
pub fn resolve_page_target(scope: &RefScope) -> Option<String> {
    global_ref_table().page_target(scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(attempt_id: &str, handoff_gen: u64) -> RefScope {
        RefScope::new(
            "container:started",
            0,
            Some(attempt_id.into()),
            Some(handoff_gen),
        )
    }

    fn element(ref_name: &str, name: &str) -> ElementRef {
        ElementRef {
            r#ref: ref_name.into(),
            role: "textbox".into(),
            name: name.into(),
            value: Some("secret-value".into()),
            selector: Some("input[type=email]".into()),
            backend_node_id: None,
            point: Some([200.0, 300.0]),
            box_bounds: Some([150.0, 280.0, 100.0, 40.0]),
            focused: true,
            disabled: false,
        }
    }

    #[test]
    fn test_normalize_ref() {
        assert_eq!(normalize_ref("@e1"), "e1");
        assert_eq!(normalize_ref("e1"), "e1");
        assert_eq!(normalize_ref("  @e42  "), "e42");
        assert_eq!(normalize_ref("e99"), "e99");
    }

    #[test]
    fn test_target_coordinates() {
        let el1 = ElementRef {
            r#ref: "e1".into(),
            role: "button".into(),
            name: "Submit".into(),
            value: None,
            selector: None,
            backend_node_id: None,
            point: Some([320.0, 180.0]),
            box_bounds: Some([300.0, 160.0, 40.0, 40.0]),
            focused: false,
            disabled: false,
        };
        assert_eq!(el1.target_coordinates(), Some((320, 180)));

        let el2 = ElementRef {
            r#ref: "e2".into(),
            role: "link".into(),
            name: "Help".into(),
            value: None,
            selector: None,
            backend_node_id: None,
            point: None,
            box_bounds: Some([100.0, 200.0, 60.0, 20.0]),
            focused: false,
            disabled: false,
        };
        assert_eq!(el2.target_coordinates(), Some((130, 210)));

        let el3 = ElementRef {
            r#ref: "e3".into(),
            role: "text".into(),
            name: "Label".into(),
            value: None,
            selector: None,
            backend_node_id: None,
            point: None,
            box_bounds: None,
            focused: false,
            disabled: false,
        };
        assert_eq!(el3.target_coordinates(), None);
    }

    #[test]
    fn test_element_ref_deserialization_with_negative_zero_float() {
        let json = r#"{
            "ref": "e1",
            "role": "link",
            "name": "Wikipedia",
            "point": [-0.0, 10.5],
            "box_bounds": [-0.0, 0.0, 50.0, 50.0]
        }"#;
        let el: ElementRef = serde_json::from_str(json).unwrap();
        assert_eq!(el.target_coordinates(), Some((0, 11)));
    }

    #[test]
    fn snapshots_get_fresh_numeric_refs_and_stale_scopes_do_not_resolve() {
        let table = RefTable::new();
        let first_scope = scope("attempt-a", 1).with_observation_gen(Some(1));
        let second_scope = scope("attempt-a", 1).with_observation_gen(Some(2));
        let first = table.set_refs(
            first_scope.clone(),
            HashMap::from([("e1".into(), element("e1", "first"))]),
        );
        let second = table.set_refs(
            second_scope.clone(),
            HashMap::from([("e1".into(), element("e1", "second"))]),
        );
        let first_id = &first.token_map["e1"];
        let second_id = &second.token_map["e1"];
        assert_ne!(first_id, second_id);
        assert!(is_numeric_ref(first_id) && is_numeric_ref(second_id));
        assert!(table.get_ref(&first_scope, first_id).is_none());
        assert_eq!(
            table.get_ref(&second_scope, second_id).unwrap().name,
            "second"
        );

        let restarted = scope("attempt-a", 2);
        assert!(table.get_ref(&restarted, second_id).is_none());
        let new_computer = RefScope::new("different:started", 0, Some("attempt-a".into()), Some(1));
        assert!(table.get_ref(&new_computer, second_id).is_none());
    }

    #[test]
    fn axtree_tokens_are_rewritten_exactly() {
        let map = HashMap::from([(String::from("e1"), String::from("e42"))]);
        assert_eq!(
            rewrite_axtree("[@e1: button \"Save\"] text @e10 @e1foo", &map),
            "[@e42: button \"Save\"] text @e10 @e1foo"
        );
    }
}
