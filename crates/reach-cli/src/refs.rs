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
    pub point: Option<[i64; 2]>,
    #[serde(default)]
    pub box_bounds: Option<[i64; 4]>, // [x, y, width, height]
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
            return Some((px, py));
        }
        if let Some([bx, by, bw, bh]) = self.box_bounds {
            return Some((bx + bw / 2, by + bh / 2));
        }
        None
    }
}

/// Normalize an element reference by trimming whitespace and stripping any leading `@`.
/// E.g. `"@e14"` -> `"e14"`, `"e14"` -> `"e14"`.
pub fn normalize_ref(r: &str) -> &str {
    r.trim().strip_prefix('@').unwrap_or(r.trim())
}

pub type ScreenRefMap = HashMap<String, ElementRef>;
pub type TargetScreenKey = (String, u32);

/// In-memory table storing active semantic refs per target sandbox and screen.
#[derive(Debug, Default, Clone)]
pub struct RefTable {
    entries: Arc<RwLock<HashMap<TargetScreenKey, ScreenRefMap>>>,
}

impl RefTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace all registered refs for a given `(target, screen)` pair.
    pub fn set_refs(&self, target: &str, screen: u32, refs: HashMap<String, ElementRef>) {
        let mut map = self.entries.write().unwrap();
        let normalized = refs
            .into_iter()
            .map(|(k, v)| (normalize_ref(&k).to_string(), v))
            .collect();
        map.insert((target.to_string(), screen), normalized);
    }

    /// Retrieve an `ElementRef` by name for a given screen.
    pub fn get_ref(&self, target: &str, screen: u32, ref_name: &str) -> Option<ElementRef> {
        let clean = normalize_ref(ref_name);
        let map = self.entries.read().unwrap();
        map.get(&(target.to_string(), screen))
            .and_then(|screen_refs| screen_refs.get(clean).cloned())
    }

    /// List all registered `ElementRef` items for a given screen.
    pub fn list_refs(&self, target: &str, screen: u32) -> Vec<ElementRef> {
        let map = self.entries.read().unwrap();
        map.get(&(target.to_string(), screen))
            .map(|screen_refs| screen_refs.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Clear all registered refs for a given screen (e.g. after top-level navigation).
    pub fn clear(&self, target: &str, screen: u32) {
        let mut map = self.entries.write().unwrap();
        map.remove(&(target.to_string(), screen));
    }
}

static GLOBAL_REF_TABLE: LazyLock<RefTable> = LazyLock::new(RefTable::new);

/// Access the global shared reference table.
pub fn global_ref_table() -> &'static RefTable {
    &GLOBAL_REF_TABLE
}

fn ref_storage_dir() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("REACH_REFS_DIR") {
        return Some(PathBuf::from(path));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".reach").join("refs"))
}

/// Persist refs to disk at `~/.reach/refs/{target}_screen_{screen}.json`.
pub fn save_refs_to_disk(target: &str, screen: u32, refs: &HashMap<String, ElementRef>) {
    if let Some(dir) = ref_storage_dir() {
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join(format!("{target}_screen_{screen}.json"));
        if let Ok(data) = serde_json::to_string_pretty(refs) {
            let _ = std::fs::write(file_path, data);
        }
    }
}

/// Load refs from disk if present.
pub fn load_refs_from_disk(target: &str, screen: u32) -> Option<HashMap<String, ElementRef>> {
    let dir = ref_storage_dir()?;
    let file_path = dir.join(format!("{target}_screen_{screen}.json"));
    let data = std::fs::read_to_string(file_path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Resolve an element reference by name, checking in-memory `GLOBAL_REF_TABLE`
/// first and falling back to persisted disk cache if available.
pub fn resolve_ref(target: &str, screen: u32, ref_name: &str) -> Option<ElementRef> {
    if let Some(el) = global_ref_table().get_ref(target, screen, ref_name) {
        return Some(el);
    }
    // Attempt hydration from disk cache (for cross-process CLI calls)
    if let Some(disk_refs) = load_refs_from_disk(target, screen) {
        global_ref_table().set_refs(target, screen, disk_refs);
        return global_ref_table().get_ref(target, screen, ref_name);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
            point: Some([320, 180]),
            box_bounds: Some([300, 160, 40, 40]),
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
            point: None,
            box_bounds: Some([100, 200, 60, 20]),
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
            point: None,
            box_bounds: None,
            focused: false,
            disabled: false,
        };
        assert_eq!(el3.target_coordinates(), None);
    }

    #[test]
    fn test_ref_table_store_and_lookup() {
        let table = RefTable::new();
        let mut refs = HashMap::new();
        refs.insert(
            "@e1".into(),
            ElementRef {
                r#ref: "e1".into(),
                role: "textbox".into(),
                name: "Email".into(),
                value: Some("alice@example.com".into()),
                selector: Some("input[type=email]".into()),
                point: Some([200, 300]),
                box_bounds: Some([150, 280, 100, 40]),
                focused: true,
                disabled: false,
            },
        );

        table.set_refs("default-box", 0, refs);

        // Can look up with or without leading @
        let found1 = table.get_ref("default-box", 0, "@e1");
        assert!(found1.is_some());
        assert_eq!(found1.unwrap().name, "Email");

        let found2 = table.get_ref("default-box", 0, "e1");
        assert!(found2.is_some());
        assert_eq!(found2.unwrap().value, Some("alice@example.com".into()));

        // Nonexistent ref returns None
        assert!(table.get_ref("default-box", 0, "@e2").is_none());
        assert!(table.get_ref("other-box", 0, "@e1").is_none());
    }
}
