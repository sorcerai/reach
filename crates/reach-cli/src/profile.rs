#![allow(clippy::collapsible_if, clippy::result_large_err)]

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
#[allow(unused_imports)]
use fs4::FileExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ═══════════════════════════════════════════════════════════
// Profile Broker & File Locking
// ═══════════════════════════════════════════════════════════

/// Metadata about the task or process holding a profile lock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockHolderInfo {
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub acquired_at: chrono::DateTime<chrono::Utc>,
    pub lease_id: String,
}

impl LockHolderInfo {
    pub fn new(screen: Option<u32>, task: Option<String>, owner: Option<String>) -> Self {
        Self {
            pid: std::process::id(),
            screen,
            task,
            owner,
            acquired_at: chrono::Utc::now(),
            lease_id: uuid::Uuid::new_v4().to_string(),
        }
    }
}

/// Errors when acquiring a profile lock.
#[derive(Debug)]
pub enum ProfileLockError {
    Locked {
        profile: String,
        holder: Option<LockHolderInfo>,
    },
    Timeout {
        profile: String,
        timeout_ms: u64,
        holder: Option<LockHolderInfo>,
    },
    Io {
        profile: String,
        source: std::io::Error,
    },
}

impl fmt::Display for ProfileLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Locked { profile, holder } => match holder {
                Some(h) => write!(
                    f,
                    "profile '{profile}' is locked by pid {} (task: {:?}, screen: {:?})",
                    h.pid, h.task, h.screen
                ),
                None => write!(f, "profile '{profile}' is locked by another process"),
            },
            Self::Timeout {
                profile,
                timeout_ms,
                holder,
            } => match holder {
                Some(h) => write!(
                    f,
                    "timed out after {timeout_ms}ms waiting for profile '{profile}' held by pid {} (task: {:?})",
                    h.pid, h.task
                ),
                None => write!(
                    f,
                    "timed out after {timeout_ms}ms waiting for profile '{profile}'"
                ),
            },
            Self::Io { profile, source } => {
                write!(f, "IO error locking profile '{profile}': {source}")
            }
        }
    }
}

impl std::error::Error for ProfileLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl IntoResponse for ProfileLockError {
    fn into_response(self) -> Response {
        match self {
            Self::Locked { profile, holder } => (
                StatusCode::LOCKED,
                Json(serde_json::json!({
                    "error": "profile_locked",
                    "profile": profile,
                    "holder": holder,
                })),
            )
                .into_response(),
            Self::Timeout {
                profile,
                timeout_ms,
                holder,
            } => (
                StatusCode::LOCKED,
                Json(serde_json::json!({
                    "error": "profile_lock_timeout",
                    "profile": profile,
                    "timeout_ms": timeout_ms,
                    "holder": holder,
                })),
            )
                .into_response(),
            Self::Io { profile, source } => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "profile_lock_io_error",
                    "profile": profile,
                    "message": source.to_string(),
                })),
            )
                .into_response(),
        }
    }
}

/// An active lease on a Chrome profile directory.
///
/// Holds the file lock on `<profile_path>/.reach.lock` and releases it on `Drop`.
pub struct ProfileLease {
    profile: String,
    profile_dir: PathBuf,
    lock_path: PathBuf,
    file: Option<File>,
    holder: LockHolderInfo,
    broker_inner: Option<Arc<ProfileBrokerInner>>,
}

impl fmt::Debug for ProfileLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProfileLease")
            .field("profile", &self.profile)
            .field("profile_dir", &self.profile_dir)
            .field("lock_path", &self.lock_path)
            .field("holder", &self.holder)
            .finish()
    }
}

impl ProfileLease {
    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn profile_dir(&self) -> &Path {
        &self.profile_dir
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub fn holder(&self) -> &LockHolderInfo {
        &self.holder
    }
}

impl Drop for ProfileLease {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            // Truncate lock file before unlocking
            let _ = file.set_len(0);
            let _ = file.unlock();
        }

        if let Some(broker) = &self.broker_inner {
            if let Ok(mut map) = broker.in_process.lock() {
                map.remove(&self.lock_path);
            }
        }
    }
}

/// Normalize a profile name or path into a safe filename for the locks directory.
pub fn normalize_profile_name(profile_name: &str) -> String {
    let sanitized: String = profile_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches(|c| c == '_' || c == '.');
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Check if a process with `pid` is currently alive on the system.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    let res = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if res == 0 {
        true
    } else {
        let err = std::io::Error::last_os_error().raw_os_error();
        err == Some(libc::EPERM)
    }
}

#[cfg(not(unix))]
pub fn is_process_alive(_pid: u32) -> bool {
    true
}

/// Parse a Chrome SingletonLock symlink target (e.g. `<hostname>-<pid>` or `<pid>`) into a PID.
pub fn parse_singleton_lock_pid(target: &str) -> Option<u32> {
    let s = target.trim();
    if let Some((_, pid_str)) = s.rsplit_once('-') {
        if let Ok(pid) = pid_str.parse::<u32>() {
            return Some(pid);
        }
    }
    s.parse::<u32>().ok()
}

/// Inspect Chrome's `SingletonLock` inside `profile_dir`.
///
/// Chrome creates `<profile_dir>/SingletonLock` pointing to `<hostname>-<pid>`.
/// If `SingletonLock` exists and the target PID is currently alive on the system,
/// returns `Ok(Some(holder))` indicating Chrome holds the profile.
/// If the PID is dead (stale lock after crash) or malformed, removes the symlink
/// to allow recovery and returns `Ok(None)`.
pub fn inspect_and_clean_singleton_lock(
    profile_dir: &Path,
) -> Result<Option<LockHolderInfo>, std::io::Error> {
    let singleton_path = profile_dir.join("SingletonLock");
    let meta = match fs::symlink_metadata(&singleton_path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };

    let target_str = if meta.file_type().is_symlink() {
        match fs::read_link(&singleton_path) {
            Ok(target) => target.to_string_lossy().into_owned(),
            Err(e) => {
                tracing::warn!(
                    "Failed to read SingletonLock symlink at {:?}: {e}; removing stale lock",
                    singleton_path
                );
                let _ = fs::remove_file(&singleton_path);
                return Ok(None);
            }
        }
    } else if meta.is_file() {
        match fs::read_to_string(&singleton_path) {
            Ok(content) => content,
            Err(e) => {
                tracing::warn!(
                    "Failed to read SingletonLock file at {:?}: {e}; removing stale lock",
                    singleton_path
                );
                let _ = fs::remove_file(&singleton_path);
                return Ok(None);
            }
        }
    } else {
        let _ = fs::remove_file(&singleton_path);
        return Ok(None);
    };

    let pid_opt = parse_singleton_lock_pid(&target_str);
    match pid_opt {
        Some(pid) if is_process_alive(pid) => Ok(Some(LockHolderInfo {
            pid,
            screen: None,
            task: Some("chrome".to_string()),
            owner: Some("chrome".to_string()),
            acquired_at: chrono::Utc::now(),
            lease_id: format!("chrome-singleton-{pid}"),
        })),
        Some(dead_pid) => {
            tracing::warn!(
                "Detected stale Chrome SingletonLock in {:?} (target '{}', PID {dead_pid} is dead); removing to allow recovery",
                profile_dir,
                target_str
            );
            let _ = fs::remove_file(&singleton_path);
            let _ = fs::remove_file(profile_dir.join("SingletonCookie"));
            let _ = fs::remove_file(profile_dir.join("SingletonSocket"));
            Ok(None)
        }
        None => {
            tracing::warn!(
                "Detected malformed Chrome SingletonLock in {:?} (target '{}'); removing to allow recovery",
                profile_dir,
                target_str
            );
            let _ = fs::remove_file(&singleton_path);
            Ok(None)
        }
    }
}

#[derive(Debug)]
struct ProfileBrokerInner {
    base_dir: PathBuf,
    locks_dir: PathBuf,
    in_process: Mutex<HashMap<PathBuf, LockHolderInfo>>,
}

/// Broker for acquiring exclusive file locks on Chrome `--user-data-dir` profiles.
#[derive(Debug, Clone)]
pub struct ProfileBroker {
    inner: Arc<ProfileBrokerInner>,
}

impl ProfileBroker {
    pub fn new(base_dir: PathBuf) -> Self {
        let locks_dir = base_dir.parent().unwrap_or(&base_dir).join("locks");
        Self::new_with_locks_dir(base_dir, locks_dir)
    }

    pub fn new_with_locks_dir(base_dir: PathBuf, locks_dir: PathBuf) -> Self {
        Self {
            inner: Arc::new(ProfileBrokerInner {
                base_dir,
                locks_dir,
                in_process: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn default_broker() -> Self {
        let base_dir = crate::config::ReachConfig::load()
            .sandbox
            .resolved_profile_dir();
        Self::new(base_dir)
    }

    pub fn base_dir(&self) -> &Path {
        &self.inner.base_dir
    }

    pub fn locks_dir(&self) -> &Path {
        &self.inner.locks_dir
    }

    pub fn profile_dir_for(&self, profile_name: &str) -> PathBuf {
        let p = Path::new(profile_name);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.inner.base_dir.join(profile_name)
        }
    }

    pub fn lock_path_for(&self, profile_name: &str) -> PathBuf {
        let normalized = normalize_profile_name(profile_name);
        self.inner.locks_dir.join(format!("{normalized}.lock"))
    }

    fn ensure_locks_dir(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.inner.locks_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.inner.locks_dir, fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }

    pub fn is_locked(&self, profile_name: &str) -> bool {
        let lock_path = self.lock_path_for(profile_name);
        if let Ok(map) = self.inner.in_process.lock() {
            if map.contains_key(&lock_path) {
                return true;
            }
        }

        let profile_dir = self.profile_dir_for(profile_name);
        if let Ok(Some(_)) = inspect_and_clean_singleton_lock(&profile_dir) {
            return true;
        }

        if !lock_path.exists() {
            return false;
        }

        // Check if existing lock file has a dead holder
        if let Some(holder) = read_holder_from_file(&lock_path) {
            if !is_process_alive(holder.pid) {
                tracing::warn!(
                    "is_locked: found stale lock file {:?} with dead PID {}, removing",
                    lock_path,
                    holder.pid
                );
                let _ = fs::remove_file(&lock_path);
                return false;
            }
        }

        if let Ok(file) = OpenOptions::new().read(true).write(true).open(&lock_path) {
            match file.try_lock() {
                Ok(_) => {
                    let _ = file.unlock();
                    false
                }
                Err(_) => true,
            }
        } else {
            false
        }
    }

    pub fn holder_info(&self, profile_name: &str) -> Option<LockHolderInfo> {
        let lock_path = self.lock_path_for(profile_name);
        if let Ok(map) = self.inner.in_process.lock() {
            if let Some(holder) = map.get(&lock_path) {
                return Some(holder.clone());
            }
        }

        let profile_dir = self.profile_dir_for(profile_name);
        if let Ok(Some(holder)) = inspect_and_clean_singleton_lock(&profile_dir) {
            return Some(holder);
        }

        if let Some(holder) = read_holder_from_file(&lock_path) {
            if is_process_alive(holder.pid) {
                Some(holder)
            } else {
                tracing::warn!(
                    "holder_info: found stale lock file {:?} with dead PID {}, removing",
                    lock_path,
                    holder.pid
                );
                let _ = fs::remove_file(&lock_path);
                None
            }
        } else {
            None
        }
    }

    /// Acquire exclusive lock on `profile_name`. If already locked, waits up to `timeout_ms`.
    pub fn acquire(
        &self,
        profile_name: &str,
        timeout_ms: u64,
    ) -> Result<ProfileLease, ProfileLockError> {
        self.acquire_with_holder(profile_name, timeout_ms, None)
    }

    /// Acquire exclusive lock on `profile_name` with custom holder metadata.
    pub fn acquire_with_holder(
        &self,
        profile_name: &str,
        timeout_ms: u64,
        holder_opt: Option<LockHolderInfo>,
    ) -> Result<ProfileLease, ProfileLockError> {
        let profile_dir = self.profile_dir_for(profile_name);
        let lock_path = self.lock_path_for(profile_name);

        if let Err(e) = self.ensure_locks_dir() {
            return Err(ProfileLockError::Io {
                profile: profile_name.to_string(),
                source: e,
            });
        }

        if let Err(e) = fs::create_dir_all(&profile_dir) {
            return Err(ProfileLockError::Io {
                profile: profile_name.to_string(),
                source: e,
            });
        }

        let holder = holder_opt.unwrap_or_else(|| LockHolderInfo::new(None, None, None));
        let start = Instant::now();
        let timeout = Duration::from_millis(timeout_ms);

        loop {
            // 1. Inspect Chrome's own SingletonLock
            match inspect_and_clean_singleton_lock(&profile_dir) {
                Ok(Some(chrome_holder)) => {
                    if start.elapsed() >= timeout {
                        if timeout_ms == 0 {
                            return Err(ProfileLockError::Locked {
                                profile: profile_name.to_string(),
                                holder: Some(chrome_holder),
                            });
                        } else {
                            return Err(ProfileLockError::Timeout {
                                profile: profile_name.to_string(),
                                timeout_ms,
                                holder: Some(chrome_holder),
                            });
                        }
                    }
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    return Err(ProfileLockError::Io {
                        profile: profile_name.to_string(),
                        source: e,
                    });
                }
            }

            // 2. Check in-process lock tracker
            let in_process_held = {
                let mut map = self.inner.in_process.lock().unwrap();
                if let Some(existing) = map.get(&lock_path) {
                    Some(existing.clone())
                } else {
                    // Pre-reserve entry so competing local threads see it immediately
                    map.insert(lock_path.clone(), holder.clone());
                    None
                }
            };

            if let Some(existing_holder) = in_process_held {
                if start.elapsed() >= timeout {
                    if timeout_ms == 0 {
                        return Err(ProfileLockError::Locked {
                            profile: profile_name.to_string(),
                            holder: Some(existing_holder),
                        });
                    } else {
                        return Err(ProfileLockError::Timeout {
                            profile: profile_name.to_string(),
                            timeout_ms,
                            holder: Some(existing_holder),
                        });
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }

            // 3. Stale-holder check before opening/locking file
            if let Some(existing_holder) = read_holder_from_file(&lock_path) {
                if !is_process_alive(existing_holder.pid) {
                    tracing::warn!(
                        "Lock file {:?} contains dead holder PID {}, removing stale lock file",
                        lock_path,
                        existing_holder.pid
                    );
                    let _ = fs::remove_file(&lock_path);
                }
            }

            // 4. We hold the in-process reservation; now acquire OS file lock (flock/fs4)
            let mut file = match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock_path)
            {
                Ok(f) => f,
                Err(e) => {
                    // Rollback in-process reservation
                    let mut map = self.inner.in_process.lock().unwrap();
                    map.remove(&lock_path);
                    return Err(ProfileLockError::Io {
                        profile: profile_name.to_string(),
                        source: e,
                    });
                }
            };

            match file.try_lock() {
                Ok(_) => {
                    // Successfully locked via OS! Write holder metadata
                    let _ = file.set_len(0);
                    let _ = file.seek(SeekFrom::Start(0));
                    if let Ok(json) = serde_json::to_string(&holder) {
                        let _ = file.write_all(json.as_bytes());
                        let _ = file.flush();
                    }

                    return Ok(ProfileLease {
                        profile: profile_name.to_string(),
                        profile_dir,
                        lock_path,
                        file: Some(file),
                        holder,
                        broker_inner: Some(self.inner.clone()),
                    });
                }
                Err(_) => {
                    // Lock held by external process.
                    // Check if the external process PID is dead.
                    let current_holder = read_holder_from_file(&lock_path);
                    if let Some(ref h) = current_holder {
                        if !is_process_alive(h.pid) {
                            tracing::warn!(
                                "Lock file {:?} is held by dead PID {}, reclaiming stale lock",
                                lock_path,
                                h.pid
                            );
                            drop(file);
                            let _ = fs::remove_file(&lock_path);
                            // Rollback in-process reservation and retry
                            {
                                let mut map = self.inner.in_process.lock().unwrap();
                                map.remove(&lock_path);
                            }
                            std::thread::sleep(Duration::from_millis(20));
                            continue;
                        }
                    }

                    // Rollback in-process reservation
                    {
                        let mut map = self.inner.in_process.lock().unwrap();
                        map.remove(&lock_path);
                    }

                    if start.elapsed() >= timeout {
                        if timeout_ms == 0 {
                            return Err(ProfileLockError::Locked {
                                profile: profile_name.to_string(),
                                holder: current_holder,
                            });
                        } else {
                            return Err(ProfileLockError::Timeout {
                                profile: profile_name.to_string(),
                                timeout_ms,
                                holder: current_holder,
                            });
                        }
                    }

                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}

fn read_holder_from_file(lock_path: &Path) -> Option<LockHolderInfo> {
    if !lock_path.exists() {
        return None;
    }
    let mut file = OpenOptions::new().read(true).open(lock_path).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    if buf.trim().is_empty() {
        return None;
    }
    serde_json::from_str(&buf).ok()
}

// ═══════════════════════════════════════════════════════════
// Domain-Sharded Cookie Jar Service
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct StorageState {
    #[serde(default)]
    pub jar_version: u64,
    #[serde(default)]
    pub cookies: Vec<Cookie>,
    #[serde(default)]
    pub origins: Vec<OriginState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default = "default_cookie_path")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

fn default_cookie_path() -> String {
    "/".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct OriginState {
    pub origin: String,
    #[serde(default, rename = "localStorage")]
    pub local_storage: Vec<LocalStorageItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct LocalStorageItem {
    pub name: String,
    pub value: String,
}

/// Service managing canonical, domain-sharded cookie jars in Playwright `storage_state` format.
#[derive(Debug, Clone)]
pub struct CookieJarService {
    jars_dir: PathBuf,
}

impl CookieJarService {
    pub fn new(jars_dir: PathBuf) -> Self {
        Self { jars_dir }
    }

    pub fn default_service() -> Self {
        Self::new(Self::default_jars_dir())
    }

    pub fn default_jars_dir() -> PathBuf {
        if let Ok(p) = std::env::var("REACH_JARS_PATH") {
            if !p.trim().is_empty() {
                return PathBuf::from(p);
            }
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".reach").join("jars")
    }

    pub fn jars_dir(&self) -> &Path {
        &self.jars_dir
    }

    pub fn sanitize_domain(domain: &str) -> String {
        let trimmed = domain.trim();
        let without_scheme = trimmed
            .strip_prefix("http://")
            .or_else(|| trimmed.strip_prefix("https://"))
            .unwrap_or(trimmed);
        let host = without_scheme.split('/').next().unwrap_or(without_scheme);
        let cleaned = host.trim_start_matches('.');
        if cleaned.is_empty() {
            "default".to_string()
        } else {
            cleaned.to_ascii_lowercase()
        }
    }

    pub fn jar_path(&self, domain: &str) -> PathBuf {
        let clean = Self::sanitize_domain(domain);
        self.jars_dir.join(format!("{clean}.json"))
    }

    pub fn load_jar(&self, domain: &str) -> Option<StorageState> {
        let path = self.jar_path(domain);
        if !path.exists() {
            return None;
        }
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Save state with optimistic concurrency validation.
    ///
    /// If the jar file already exists, `state.jar_version` must equal the on-disk version.
    /// On success, writes the jar with `jar_version = current_version + 1`.
    pub fn save_jar(&self, domain: &str, state: &StorageState) -> anyhow::Result<()> {
        fs::create_dir_all(&self.jars_dir)?;
        let path = self.jar_path(domain);
        let clean = Self::sanitize_domain(domain);
        let lock_path = self.jars_dir.join(format!(".{clean}.lock"));

        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        lock_file.lock()?;

        let result = (|| {
            let current = if path.exists() {
                let content = fs::read_to_string(&path)?;
                Some(serde_json::from_str::<StorageState>(&content)?)
            } else {
                None
            };

            let current_version = current.as_ref().map(|s| s.jar_version).unwrap_or(0);

            if let Some(ref cur) = current {
                if state.jar_version != cur.jar_version {
                    anyhow::bail!(
                        "optimistic concurrency conflict on jar '{}': provided jar_version {} != disk jar_version {}",
                        domain,
                        state.jar_version,
                        cur.jar_version
                    );
                }
            } else if state.jar_version > 1 {
                anyhow::bail!(
                    "optimistic concurrency conflict on jar '{}': expected version {} but jar does not exist on disk",
                    domain,
                    state.jar_version
                );
            }

            let next_version = current_version + 1;
            let mut to_save = state.clone();
            to_save.jar_version = next_version;

            let tmp_path = self
                .jars_dir
                .join(format!(".{clean}.tmp-{}", uuid::Uuid::new_v4()));
            let json_str = serde_json::to_string_pretty(&to_save)?;
            fs::write(&tmp_path, json_str)?;
            fs::rename(&tmp_path, &path)?;
            Ok(())
        })();

        let _ = lock_file.unlock();
        result
    }

    /// Force-save a jar state without requiring version matching, incrementing disk version.
    pub fn force_save_jar(&self, domain: &str, state: &StorageState) -> anyhow::Result<()> {
        fs::create_dir_all(&self.jars_dir)?;
        let path = self.jar_path(domain);
        let clean = Self::sanitize_domain(domain);
        let lock_path = self.jars_dir.join(format!(".{clean}.lock"));

        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        lock_file.lock()?;

        let result = (|| {
            let current_version = if path.exists() {
                let content = fs::read_to_string(&path)?;
                serde_json::from_str::<StorageState>(&content)
                    .map(|s| s.jar_version)
                    .unwrap_or(0)
            } else {
                0
            };

            let mut to_save = state.clone();
            to_save.jar_version = current_version + 1;

            let tmp_path = self
                .jars_dir
                .join(format!(".{clean}.tmp-{}", uuid::Uuid::new_v4()));
            let json_str = serde_json::to_string_pretty(&to_save)?;
            fs::write(&tmp_path, json_str)?;
            fs::rename(&tmp_path, &path)?;
            Ok(())
        })();

        let _ = lock_file.unlock();
        result
    }

    /// Combine multiple domain jars into a single `StorageState`.
    pub fn hydrate_jars(&self, domains: &[String]) -> StorageState {
        let mut combined = StorageState::default();
        for domain in domains {
            if let Some(jar) = self.load_jar(domain) {
                combined.cookies.extend(jar.cookies);
                combined.origins.extend(jar.origins);
            }
        }
        combined
    }

    /// Dump updated cookies back into the matching domain jars.
    pub fn dump_cookies_to_jars(
        &self,
        cookies: &[Cookie],
        declared_jars: &[String],
    ) -> anyhow::Result<()> {
        for domain in declared_jars {
            let clean_domain = Self::sanitize_domain(domain);
            let matching_cookies: Vec<Cookie> = cookies
                .iter()
                .filter(|c| {
                    let c_dom = c.domain.trim_start_matches('.');
                    c_dom.ends_with(&clean_domain) || clean_domain.ends_with(c_dom)
                })
                .cloned()
                .collect();

            if matching_cookies.is_empty() {
                continue;
            }

            for _ in 0..3 {
                let mut jar = self.load_jar(domain).unwrap_or_default();
                for new_c in &matching_cookies {
                    if let Some(existing) = jar.cookies.iter_mut().find(|c| {
                        c.name == new_c.name && c.domain == new_c.domain && c.path == new_c.path
                    }) {
                        *existing = new_c.clone();
                    } else {
                        jar.cookies.push(new_c.clone());
                    }
                }
                if self.save_jar(domain, &jar).is_ok() {
                    break;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_profile_name() {
        assert_eq!(normalize_profile_name("my-profile"), "my-profile");
        assert_eq!(normalize_profile_name("profile_1.2"), "profile_1.2");
        assert_eq!(
            normalize_profile_name("/path/to/profile"),
            "path_to_profile"
        );
        assert_eq!(normalize_profile_name(""), "default");
        assert_eq!(normalize_profile_name("...___..."), "default");
    }

    #[test]
    fn test_parse_singleton_lock_pid() {
        assert_eq!(parse_singleton_lock_pid("myhost-12345"), Some(12345));
        assert_eq!(parse_singleton_lock_pid("host-with-dashes-42"), Some(42));
        assert_eq!(parse_singleton_lock_pid("9999"), Some(9999));
        assert_eq!(parse_singleton_lock_pid("invalid-pid"), None);
        assert_eq!(parse_singleton_lock_pid(""), None);
    }

    #[test]
    fn test_is_process_alive_current_pid() {
        let current_pid = std::process::id();
        assert!(is_process_alive(current_pid));
        assert!(!is_process_alive(0));
        assert!(!is_process_alive(99_999_999));
    }
}
