use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScreenState {
    pub id: u32,
    pub owner: Option<String>,
    pub takeover_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub takeover_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leased_at: Option<String>,
    #[serde(default)]
    pub busy: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lease_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LeaseResponse {
    #[serde(default = "default_status_ok")]
    pub status: String,
    pub id: u32,
    pub owner: String,
    pub token: String,
}

fn default_status_ok() -> String {
    "ok".to_string()
}

impl LeaseResponse {
    pub fn new(id: u32, owner: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            status: "ok".to_string(),
            id,
            owner: owner.into(),
            token: token.into(),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScreenInfoResponse {
    pub id: u32,
    pub owner: Option<String>,
    pub takeover_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub takeover_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leased_at: Option<String>,
    pub novnc_url: String,
    #[serde(default)]
    pub busy: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum LeaseError {
    NoFreeScreen,
    NotFound(u32),
    NotOwner {
        id: u32,
        expected: String,
        actual: Option<String>,
    },
    InvalidToken {
        id: u32,
    },
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoFreeScreen => write!(f, "no free screens available"),
            Self::NotFound(id) => write!(f, "screen {id} not found"),
            Self::NotOwner {
                id,
                expected,
                actual,
            } => write!(
                f,
                "screen {id} is occupied by {actual:?}, expected {expected}"
            ),
            Self::InvalidToken { id } => {
                write!(f, "invalid or mismatched lease token for screen {id}")
            }
        }
    }
}

impl std::error::Error for LeaseError {}

#[derive(Debug)]
pub struct AgentState {
    screens: Mutex<Vec<ScreenState>>,
    active_tools: Mutex<HashMap<u32, u32>>,
}

/// RAII guard that decrements a screen's active tool count on drop.
pub struct BusyGuard<'a> {
    agent: &'a AgentState,
    screen: u32,
}

impl<'a> Drop for BusyGuard<'a> {
    fn drop(&mut self) {
        self.agent.dec_busy(self.screen);
    }
}

impl AgentState {
    pub fn new(n: u32) -> Self {
        let screens = (0..n)
            .map(|id| ScreenState {
                id,
                owner: None,
                takeover_pending: false,
                takeover_url: None,
                leased_at: None,
                busy: false,
                lease_token: None,
            })
            .collect();
        Self {
            screens: Mutex::new(screens),
            active_tools: Mutex::new(HashMap::new()),
        }
    }

    /// Dynamically expand screens if container has more screens than initially configured.
    pub fn ensure_screens(&self, n: u32) {
        let mut screens = self.screens.lock().unwrap();
        if screens.len() < n as usize {
            for id in (screens.len() as u32)..n {
                screens.push(ScreenState {
                    id,
                    owner: None,
                    takeover_pending: false,
                    takeover_url: None,
                    leased_at: None,
                    busy: false,
                    lease_token: None,
                });
            }
        }
    }

    /// Returns whether any active tool is currently running on `screen`.
    pub fn is_busy(&self, screen: u32) -> bool {
        let tools = self.active_tools.lock().unwrap();
        tools.get(&screen).copied().unwrap_or(0) > 0
    }

    /// Mark `screen` as busy with an RAII guard that resets busy on drop.
    pub fn mark_busy(&self, screen: u32) -> BusyGuard<'_> {
        self.inc_busy(screen);
        BusyGuard {
            agent: self,
            screen,
        }
    }

    /// Increment the active tool counter on `screen` and synchronize `ScreenState::busy`.
    pub fn inc_busy(&self, screen: u32) {
        let is_busy = {
            let mut tools = self.active_tools.lock().unwrap();
            let count = tools.entry(screen).or_insert(0);
            *count += 1;
            *count > 0
        };

        const MAX_SCREENS: u32 = 64;
        if screen >= MAX_SCREENS {
            return;
        }

        let mut screens = self.screens.lock().unwrap();
        if (screen as usize) >= screens.len() {
            for id in (screens.len() as u32)..=screen {
                screens.push(ScreenState {
                    id,
                    owner: None,
                    takeover_pending: false,
                    takeover_url: None,
                    leased_at: None,
                    busy: false,
                    lease_token: None,
                });
            }
        }
        if let Some(s) = screens.iter_mut().find(|s| s.id == screen) {
            s.busy = is_busy;
        }
    }

    /// Decrement the active tool counter on `screen` and synchronize `ScreenState::busy`.
    pub fn dec_busy(&self, screen: u32) {
        let is_busy = {
            let mut tools = self.active_tools.lock().unwrap();
            let count = tools.entry(screen).or_insert(0);
            *count = count.saturating_sub(1);
            *count > 0
        };

        let mut screens = self.screens.lock().unwrap();
        if let Some(s) = screens.iter_mut().find(|s| s.id == screen) {
            s.busy = is_busy;
        }
    }

    /// Explicitly set the busy state on `screen`.
    pub fn set_busy(&self, screen: u32, busy: bool) {
        {
            let mut tools = self.active_tools.lock().unwrap();
            if busy {
                *tools.entry(screen).or_insert(0) += 1;
            } else {
                tools.insert(screen, 0);
            }
        }
        let is_busy = self.is_busy(screen);
        const MAX_SCREENS: u32 = 64;
        if screen >= MAX_SCREENS {
            return;
        }

        let mut screens = self.screens.lock().unwrap();
        if (screen as usize) >= screens.len() {
            for id in (screens.len() as u32)..=screen {
                screens.push(ScreenState {
                    id,
                    owner: None,
                    takeover_pending: false,
                    takeover_url: None,
                    leased_at: None,
                    busy: false,
                    lease_token: None,
                });
            }
        }
        if let Some(s) = screens.iter_mut().find(|s| s.id == screen) {
            s.busy = is_busy;
        }
    }

    /// Lease first free screen, or return the screen already owned by `owner`.
    pub fn lease(&self, owner: &str) -> Result<u32, LeaseError> {
        let mut screens = self.screens.lock().unwrap();
        if let Some(s) = screens
            .iter_mut()
            .find(|s| s.owner.as_deref() == Some(owner))
        {
            if s.lease_token.is_none() {
                s.lease_token = Some(uuid::Uuid::new_v4().to_string());
            }
            return Ok(s.id);
        }
        if let Some(s) = screens.iter_mut().find(|s| s.owner.is_none()) {
            s.owner = Some(owner.to_string());
            s.leased_at = Some(chrono::Utc::now().to_rfc3339());
            s.lease_token = Some(uuid::Uuid::new_v4().to_string());
            return Ok(s.id);
        }
        Err(LeaseError::NoFreeScreen)
    }

    /// Lease a specific screen by ID for `owner`. Idempotent if already owned by `owner`.
    pub fn lease_screen(&self, id: u32, owner: &str) -> Result<LeaseResponse, LeaseError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(LeaseError::NotFound(id))?;
        if s.owner.as_deref() == Some(owner) {
            let token = match &s.lease_token {
                Some(t) => t.clone(),
                None => {
                    let t = uuid::Uuid::new_v4().to_string();
                    s.lease_token = Some(t.clone());
                    t
                }
            };
            return Ok(LeaseResponse::new(id, owner, token));
        }
        if s.owner.is_some() {
            return Err(LeaseError::NotOwner {
                id,
                expected: owner.to_string(),
                actual: s.owner.clone(),
            });
        }
        let token = uuid::Uuid::new_v4().to_string();
        s.owner = Some(owner.to_string());
        s.leased_at = Some(chrono::Utc::now().to_rfc3339());
        s.lease_token = Some(token.clone());
        Ok(LeaseResponse::new(id, owner, token))
    }

    /// Release screen by ID, verifying the lease token (or allowing owner/admin).
    pub fn release_screen(
        &self,
        id: u32,
        owner: &str,
        token: Option<&str>,
    ) -> Result<(), LeaseError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(LeaseError::NotFound(id))?;

        let is_admin = owner == "admin";

        if let Some(tok) = token {
            if s.lease_token.as_deref() != Some(tok) && !is_admin {
                return Err(LeaseError::InvalidToken { id });
            }
        } else if !is_admin && s.owner.as_deref() != Some(owner) {
            return Err(LeaseError::NotOwner {
                id,
                expected: owner.to_string(),
                actual: s.owner.clone(),
            });
        }

        s.owner = None;
        s.leased_at = None;
        s.takeover_pending = false;
        s.takeover_url = None;
        s.lease_token = None;
        Ok(())
    }

    /// Release screen by ID, verifying owner matches.
    pub fn release(&self, id: u32, owner: &str) -> Result<(), LeaseError> {
        self.release_screen(id, owner, None)
    }

    /// Return the active lease token for screen `id` if it is currently leased.
    pub fn lease_token(&self, id: u32) -> Option<String> {
        let screens = self.screens.lock().unwrap();
        screens
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.lease_token.clone())
    }

    /// Check if screen `id` is currently leased.
    pub fn is_leased(&self, id: u32) -> bool {
        let screens = self.screens.lock().unwrap();
        screens
            .iter()
            .find(|s| s.id == id)
            .is_some_and(|s| s.owner.is_some())
    }

    /// Set takeover pending flag and URL for a screen.
    pub fn set_takeover(&self, id: u32, pending: bool, url: Option<String>) {
        let mut screens = self.screens.lock().unwrap();
        if let Some(s) = screens.iter_mut().find(|s| s.id == id) {
            s.takeover_pending = pending;
            s.takeover_url = url;
        }
    }

    /// Return a snapshot of all screen states.
    pub fn snapshot(&self) -> Vec<ScreenState> {
        self.screens.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_is_idempotent_per_owner_and_exhausts() {
        let a = AgentState::new(2);
        assert_eq!(a.lease("piper").unwrap(), 0);
        assert_eq!(a.lease("piper").unwrap(), 0);
        assert_eq!(a.lease("otto").unwrap(), 1);
        assert!(matches!(a.lease("third"), Err(LeaseError::NoFreeScreen)));
        assert!(a.release(1, "piper").is_err());
        a.release(1, "otto").unwrap();
        assert_eq!(a.lease("third").unwrap(), 1);
    }

    #[test]
    fn lease_screen_specific() {
        let a = AgentState::new(2);
        a.lease_screen(1, "otto").unwrap();
        // Idempotent for same owner
        a.lease_screen(1, "otto").unwrap();
        // Fails for different owner
        assert!(a.lease_screen(1, "piper").is_err());
        // Screen 0 is still free
        a.lease_screen(0, "piper").unwrap();
        assert_eq!(a.snapshot().len(), 2);
    }

    #[test]
    fn lease_token_generation_and_validation() {
        let a = AgentState::new(2);
        let res = a.lease_screen(0, "browser-use").unwrap();
        assert!(!res.token.is_empty());
        assert_eq!(res.id, 0);
        assert_eq!(res.owner, "browser-use");
        assert_eq!(a.lease_token(0), Some(res.token.clone()));
        assert!(a.is_leased(0));

        // Idempotent lease returns same token
        let res2 = a.lease_screen(0, "browser-use").unwrap();
        assert_eq!(res2.token, res.token);

        // Rejection on token mismatch
        let err = a
            .release_screen(0, "browser-use", Some("wrong-token"))
            .unwrap_err();
        assert_eq!(err, LeaseError::InvalidToken { id: 0 });
        assert!(a.is_leased(0));

        // Rejection on non-owner without token
        let err2 = a.release_screen(0, "intruder", None).unwrap_err();
        assert!(matches!(err2, LeaseError::NotOwner { .. }));
        assert!(a.is_leased(0));

        // Success with valid token
        a.release_screen(0, "browser-use", Some(&res.token))
            .unwrap();
        assert!(!a.is_leased(0));
        assert_eq!(a.lease_token(0), None);
    }

    #[test]
    fn release_screen_admin_override() {
        let a = AgentState::new(1);
        let _res = a.lease_screen(0, "worker").unwrap();
        assert!(a.is_leased(0));

        // Admin can release even with wrong token or no token
        a.release_screen(0, "admin", Some("wrong-token")).unwrap();
        assert!(!a.is_leased(0));
        assert_eq!(a.lease_token(0), None);

        // Lease again and admin release without token
        let _ = a.lease_screen(0, "worker").unwrap();
        a.release_screen(0, "admin", None).unwrap();
        assert!(!a.is_leased(0));
    }

    #[test]
    fn release_screen_owner_match_without_token() {
        let a = AgentState::new(1);
        let _res = a.lease_screen(0, "otto").unwrap();
        assert!(a.is_leased(0));

        // Owner can release without token
        a.release(0, "otto").unwrap();
        assert!(!a.is_leased(0));
    }

    #[test]
    fn set_takeover_updates_flags() {
        let a = AgentState::new(1);
        a.set_takeover(0, true, Some("http://localhost:6080/vnc.html".into()));
        let snap = a.snapshot();
        assert!(snap[0].takeover_pending);
        assert_eq!(
            snap[0].takeover_url.as_deref(),
            Some("http://localhost:6080/vnc.html")
        );

        a.set_takeover(0, false, None);
        let snap2 = a.snapshot();
        assert!(!snap2[0].takeover_pending);
        assert!(snap2[0].takeover_url.is_none());
    }

    #[test]
    fn ensure_screens_expands() {
        let a = AgentState::new(1);
        assert_eq!(a.snapshot().len(), 1);
        a.ensure_screens(3);
        assert_eq!(a.snapshot().len(), 3);
        assert_eq!(a.snapshot()[2].id, 2);
    }

    #[test]
    fn screen_busy_tracking_with_guard() {
        let a = AgentState::new(2);
        assert!(!a.is_busy(0));
        assert!(!a.is_busy(1));
        assert!(!a.snapshot()[0].busy);

        {
            let _guard = a.mark_busy(0);
            assert!(a.is_busy(0));
            assert!(!a.is_busy(1));
            assert!(a.snapshot()[0].busy);
            assert!(!a.snapshot()[1].busy);
        }

        assert!(!a.is_busy(0));
        assert!(!a.snapshot()[0].busy);
    }

    #[test]
    fn screen_busy_counter_nested() {
        let a = AgentState::new(2);
        a.inc_busy(0);
        a.inc_busy(0);
        assert!(a.is_busy(0));

        a.dec_busy(0);
        assert!(a.is_busy(0));

        a.dec_busy(0);
        assert!(!a.is_busy(0));

        // saturating at 0
        a.dec_busy(0);
        assert!(!a.is_busy(0));
    }

    #[test]
    fn screen_busy_explicit_set() {
        let a = AgentState::new(2);
        a.set_busy(1, true);
        assert!(a.is_busy(1));
        assert!(a.snapshot()[1].busy);

        a.set_busy(1, false);
        assert!(!a.is_busy(1));
        assert!(!a.snapshot()[1].busy);
    }
}
