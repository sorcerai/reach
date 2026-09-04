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
        }
    }
}

impl std::error::Error for LeaseError {}

#[derive(Debug)]
pub struct AgentState {
    screens: Mutex<Vec<ScreenState>>,
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
            })
            .collect();
        Self {
            screens: Mutex::new(screens),
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
                });
            }
        }
    }

    /// Lease first free screen, or return the screen already owned by `owner`.
    pub fn lease(&self, owner: &str) -> Result<u32, LeaseError> {
        let mut screens = self.screens.lock().unwrap();
        if let Some(s) = screens.iter().find(|s| s.owner.as_deref() == Some(owner)) {
            return Ok(s.id);
        }
        if let Some(s) = screens.iter_mut().find(|s| s.owner.is_none()) {
            s.owner = Some(owner.to_string());
            s.leased_at = Some(chrono::Utc::now().to_rfc3339());
            return Ok(s.id);
        }
        Err(LeaseError::NoFreeScreen)
    }

    /// Lease a specific screen by ID for `owner`. Idempotent if already owned by `owner`.
    pub fn lease_screen(&self, id: u32, owner: &str) -> Result<(), LeaseError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(LeaseError::NotFound(id))?;
        if s.owner.as_deref() == Some(owner) {
            return Ok(());
        }
        if s.owner.is_some() {
            return Err(LeaseError::NotOwner {
                id,
                expected: owner.to_string(),
                actual: s.owner.clone(),
            });
        }
        s.owner = Some(owner.to_string());
        s.leased_at = Some(chrono::Utc::now().to_rfc3339());
        Ok(())
    }

    /// Release screen by ID, verifying owner matches.
    pub fn release(&self, id: u32, owner: &str) -> Result<(), LeaseError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(LeaseError::NotFound(id))?;
        if s.owner.as_deref() != Some(owner) {
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
        Ok(())
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
}
