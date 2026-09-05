#![allow(clippy::collapsible_if)]

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ScreenPhase {
    Idle,
    AgentActive,
    HandoffPending,
    HumanActive,
    HumanDone,
}

impl fmt::Display for ScreenPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl std::str::FromStr for ScreenPhase {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Idle" | "idle" => Ok(ScreenPhase::Idle),
            "AgentActive" | "agent_active" | "agentActive" => Ok(ScreenPhase::AgentActive),
            "HandoffPending" | "handoff_pending" | "handoffPending" => {
                Ok(ScreenPhase::HandoffPending)
            }
            "HumanActive" | "human_active" | "humanActive" => Ok(ScreenPhase::HumanActive),
            "HumanDone" | "human_done" | "humanDone" => Ok(ScreenPhase::HumanDone),
            _ => Err(format!("unknown screen phase: {s}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScreenState {
    pub id: u32,
    pub owner: Option<String>,
    pub phase: ScreenPhase,
    pub handoff_gen: u64,
    #[serde(default)]
    pub takeover_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub takeover_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub takeover_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leased_at: Option<String>,
    #[serde(default)]
    pub busy: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub lease_token: Option<String>,
}

pub type ScreenInfo = ScreenState;

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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ScreenInfoResponse {
    pub id: u32,
    pub owner: Option<String>,
    pub phase: ScreenPhase,
    pub handoff_gen: u64,
    pub takeover_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub takeover_reason: Option<String>,
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

#[derive(Debug, PartialEq, Eq)]
pub enum TakeoverError {
    NotFound(u32),
    InvalidPhase {
        id: u32,
        current: ScreenPhase,
        expected: Vec<ScreenPhase>,
    },
    NotOwner {
        id: u32,
        expected: String,
        actual: Option<String>,
    },
    InvalidToken {
        id: u32,
    },
}

impl fmt::Display for TakeoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "screen {id} not found"),
            Self::InvalidPhase {
                id,
                current,
                expected,
            } => {
                write!(
                    f,
                    "screen {id} in invalid phase {current:?}, expected one of {expected:?}"
                )
            }
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

impl std::error::Error for TakeoverError {}

#[derive(Debug, PartialEq, Eq)]
pub enum WaitError {
    NotFound(u32),
    Timeout {
        id: u32,
        phase: ScreenPhase,
        handoff_gen: u64,
    },
}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "screen {id} not found"),
            Self::Timeout {
                id,
                phase,
                handoff_gen,
            } => {
                write!(
                    f,
                    "timeout waiting for screen {id} (phase: {phase:?}, gen: {handoff_gen})"
                )
            }
        }
    }
}

impl std::error::Error for WaitError {}

#[derive(Debug)]
pub struct AgentState {
    screens: Mutex<Vec<ScreenState>>,
    active_tools: Mutex<HashMap<u32, u32>>,
    phase_notify: tokio::sync::broadcast::Sender<u32>,
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
        let (phase_notify, _) = tokio::sync::broadcast::channel(128);
        let screens = (0..n)
            .map(|id| ScreenState {
                id,
                owner: None,
                phase: ScreenPhase::Idle,
                handoff_gen: 1,
                takeover_pending: false,
                takeover_reason: None,
                takeover_url: None,
                leased_at: None,
                busy: false,
                lease_token: None,
            })
            .collect();
        Self {
            screens: Mutex::new(screens),
            active_tools: Mutex::new(HashMap::new()),
            phase_notify,
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
                    phase: ScreenPhase::Idle,
                    handoff_gen: 1,
                    takeover_pending: false,
                    takeover_reason: None,
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
                    phase: ScreenPhase::Idle,
                    handoff_gen: 1,
                    takeover_pending: false,
                    takeover_reason: None,
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
                    phase: ScreenPhase::Idle,
                    handoff_gen: 1,
                    takeover_pending: false,
                    takeover_reason: None,
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
            s.phase = ScreenPhase::AgentActive;
            let screen_id = s.id;
            drop(screens);
            let _ = self.phase_notify.send(screen_id);
            return Ok(screen_id);
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
        s.phase = ScreenPhase::AgentActive;
        drop(screens);
        let _ = self.phase_notify.send(id);
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
        s.phase = ScreenPhase::Idle;
        s.takeover_pending = false;
        s.takeover_reason = None;
        s.takeover_url = None;
        s.lease_token = None;
        drop(screens);
        let _ = self.phase_notify.send(id);
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

    /// Moves `AgentActive` (or `Idle`) to `HandoffPending`, increments `handoff_gen`.
    pub fn request_takeover(
        &self,
        screen_id: u32,
        reason: Option<String>,
        url: Option<String>,
    ) -> Result<ScreenState, TakeoverError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == screen_id)
            .ok_or(TakeoverError::NotFound(screen_id))?;

        if s.phase != ScreenPhase::AgentActive && s.phase != ScreenPhase::Idle {
            return Err(TakeoverError::InvalidPhase {
                id: screen_id,
                current: s.phase,
                expected: vec![ScreenPhase::AgentActive, ScreenPhase::Idle],
            });
        }

        s.phase = ScreenPhase::HandoffPending;
        s.handoff_gen += 1;
        s.takeover_pending = true;
        s.takeover_reason = reason;
        s.takeover_url = url;
        let res = s.clone();
        drop(screens);
        let _ = self.phase_notify.send(screen_id);
        Ok(res)
    }

    /// Moves `HandoffPending` to `HumanActive`.
    pub fn human_connected(&self, screen_id: u32) -> Result<ScreenState, TakeoverError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == screen_id)
            .ok_or(TakeoverError::NotFound(screen_id))?;

        if s.phase != ScreenPhase::HandoffPending {
            return Err(TakeoverError::InvalidPhase {
                id: screen_id,
                current: s.phase,
                expected: vec![ScreenPhase::HandoffPending],
            });
        }

        s.phase = ScreenPhase::HumanActive;
        let res = s.clone();
        drop(screens);
        let _ = self.phase_notify.send(screen_id);
        Ok(res)
    }

    /// Moves `HumanActive` (or `HandoffPending`) to `HumanDone`, increments `handoff_gen`.
    pub fn human_handback(&self, screen_id: u32) -> Result<ScreenState, TakeoverError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == screen_id)
            .ok_or(TakeoverError::NotFound(screen_id))?;

        if s.phase != ScreenPhase::HumanActive && s.phase != ScreenPhase::HandoffPending {
            return Err(TakeoverError::InvalidPhase {
                id: screen_id,
                current: s.phase,
                expected: vec![ScreenPhase::HumanActive, ScreenPhase::HandoffPending],
            });
        }

        s.phase = ScreenPhase::HumanDone;
        s.handoff_gen += 1;
        let res = s.clone();
        drop(screens);
        let _ = self.phase_notify.send(screen_id);
        Ok(res)
    }

    /// Moves `HumanDone` to `AgentActive`, increments `handoff_gen`.
    pub fn agent_ack(&self, screen_id: u32) -> Result<ScreenState, TakeoverError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == screen_id)
            .ok_or(TakeoverError::NotFound(screen_id))?;

        if s.phase != ScreenPhase::HumanDone {
            return Err(TakeoverError::InvalidPhase {
                id: screen_id,
                current: s.phase,
                expected: vec![ScreenPhase::HumanDone],
            });
        }

        s.phase = ScreenPhase::AgentActive;
        s.handoff_gen += 1;
        s.takeover_pending = false;
        s.takeover_reason = None;
        s.takeover_url = None;
        let res = s.clone();
        drop(screens);
        let _ = self.phase_notify.send(screen_id);
        Ok(res)
    }

    /// Set takeover pending flag and URL for a screen.
    pub fn set_takeover(&self, id: u32, pending: bool, url: Option<String>) {
        if pending {
            let _ = self.request_takeover(id, Some("takeover requested".into()), url);
        } else {
            let _ = self.human_handback(id);
            let _ = self.agent_ack(id);
        }
    }

    /// Return screen info by ID if exists.
    pub fn screen_info(&self, screen: u32) -> Option<ScreenState> {
        let screens = self.screens.lock().unwrap();
        screens.iter().find(|s| s.id == screen).cloned()
    }

    /// Return phase for screen by ID.
    pub fn phase(&self, screen: u32) -> Option<ScreenPhase> {
        let screens = self.screens.lock().unwrap();
        screens.iter().find(|s| s.id == screen).map(|s| s.phase)
    }

    /// Return handoff generation counter for screen by ID.
    pub fn handoff_gen(&self, screen: u32) -> Option<u64> {
        let screens = self.screens.lock().unwrap();
        screens
            .iter()
            .find(|s| s.id == screen)
            .map(|s| s.handoff_gen)
    }

    /// Wait for a screen to transition to `target_phase` or timeout.
    pub async fn wait_for_phase(
        &self,
        screen_id: u32,
        target_phase: ScreenPhase,
        timeout: std::time::Duration,
    ) -> Result<ScreenState, WaitError> {
        let mut rx = self.phase_notify.subscribe();
        if let Some(s) = self.screen_info(screen_id) {
            if s.phase == target_phase {
                return Ok(s);
            }
        } else {
            return Err(WaitError::NotFound(screen_id));
        }

        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);

        loop {
            tokio::select! {
                _ = &mut sleep => {
                    if let Some(s) = self.screen_info(screen_id) {
                        return Err(WaitError::Timeout {
                            id: screen_id,
                            phase: s.phase,
                            handoff_gen: s.handoff_gen,
                        });
                    } else {
                        return Err(WaitError::NotFound(screen_id));
                    }
                }
                res = rx.recv() => {
                    match res {
                        Ok(notified_id) => {
                            if notified_id == screen_id {
                                if let Some(s) = self.screen_info(screen_id) {
                                    if s.phase == target_phase {
                                        return Ok(s);
                                    }
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if let Some(s) = self.screen_info(screen_id) {
                                if s.phase == target_phase {
                                    return Ok(s);
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            if let Some(s) = self.screen_info(screen_id) {
                                return Err(WaitError::Timeout {
                                    id: screen_id,
                                    phase: s.phase,
                                    handoff_gen: s.handoff_gen,
                                });
                            } else {
                                return Err(WaitError::NotFound(screen_id));
                            }
                        }
                    }
                }
            }
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

    #[test]
    fn screen_phase_state_machine_and_handoff_gen() {
        let a = AgentState::new(1);
        let screen = a.screen_info(0).unwrap();
        assert_eq!(screen.phase, ScreenPhase::Idle);
        assert_eq!(screen.handoff_gen, 1);
        assert!(!screen.takeover_pending);
        assert_eq!(screen.takeover_reason, None);
        assert_eq!(screen.takeover_url, None);

        // 1. Lease screen -> moves Idle to AgentActive
        let _ = a.lease_screen(0, "agent-alice").unwrap();
        assert_eq!(a.phase(0), Some(ScreenPhase::AgentActive));
        assert_eq!(a.handoff_gen(0), Some(1));

        // 2. Request takeover -> moves AgentActive to HandoffPending, increments gen to 2
        let res = a
            .request_takeover(
                0,
                Some("CAPTCHA challenge detected".into()),
                Some("http://127.0.0.1:6080/vnc.html".into()),
            )
            .unwrap();
        assert_eq!(res.phase, ScreenPhase::HandoffPending);
        assert_eq!(res.handoff_gen, 2);
        assert!(res.takeover_pending);
        assert_eq!(
            res.takeover_reason.as_deref(),
            Some("CAPTCHA challenge detected")
        );
        assert_eq!(
            res.takeover_url.as_deref(),
            Some("http://127.0.0.1:6080/vnc.html")
        );

        // Cannot request takeover again when in HandoffPending
        let err = a
            .request_takeover(0, Some("again".into()), None)
            .unwrap_err();
        assert!(matches!(err, TakeoverError::InvalidPhase { .. }));

        // 3. Human connected -> moves HandoffPending to HumanActive
        let res = a.human_connected(0).unwrap();
        assert_eq!(res.phase, ScreenPhase::HumanActive);
        assert_eq!(res.handoff_gen, 2);

        // 4. Human handback -> moves HumanActive to HumanDone, increments gen to 3
        let res = a.human_handback(0).unwrap();
        assert_eq!(res.phase, ScreenPhase::HumanDone);
        assert_eq!(res.handoff_gen, 3);

        // Cannot human connected when HumanDone
        assert!(a.human_connected(0).is_err());

        // 5. Agent ack -> moves HumanDone to AgentActive, increments gen to 4, clears reason/url
        let res = a.agent_ack(0).unwrap();
        assert_eq!(res.phase, ScreenPhase::AgentActive);
        assert_eq!(res.handoff_gen, 4);
        assert!(!res.takeover_pending);
        assert_eq!(res.takeover_reason, None);
        assert_eq!(res.takeover_url, None);
    }

    #[test]
    fn human_handback_direct_from_handoff_pending() {
        let a = AgentState::new(1);
        let _ = a.lease_screen(0, "agent-bob").unwrap();
        a.request_takeover(0, Some("Login required".into()), None)
            .unwrap();
        assert_eq!(a.phase(0), Some(ScreenPhase::HandoffPending));
        assert_eq!(a.handoff_gen(0), Some(2));

        // Skip human_connected and hand back directly
        let res = a.human_handback(0).unwrap();
        assert_eq!(res.phase, ScreenPhase::HumanDone);
        assert_eq!(res.handoff_gen, 3);

        let res = a.agent_ack(0).unwrap();
        assert_eq!(res.phase, ScreenPhase::AgentActive);
        assert_eq!(res.handoff_gen, 4);
    }

    #[tokio::test]
    async fn wait_for_phase_immediate_and_wake() {
        use std::sync::Arc;
        use std::time::Duration;

        let a = Arc::new(AgentState::new(1));
        let _ = a.lease_screen(0, "agent-sam").unwrap();
        a.request_takeover(0, Some("2FA required".into()), None)
            .unwrap();
        a.human_connected(0).unwrap();

        // Spawn a background task that sleeps 50ms and calls human_handback
        let a_clone = Arc::clone(&a);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            a_clone.human_handback(0).unwrap();
        });

        // wait_for_phase should wake up when HumanDone is reached
        let res = a
            .wait_for_phase(0, ScreenPhase::HumanDone, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(res.phase, ScreenPhase::HumanDone);
        assert_eq!(res.handoff_gen, 3);

        // Calling wait_for_phase immediately for an already matching phase returns immediately
        let res_imm = a
            .wait_for_phase(0, ScreenPhase::HumanDone, Duration::from_millis(500))
            .await
            .unwrap();
        assert_eq!(res_imm.phase, ScreenPhase::HumanDone);

        // Timeout case
        let err = a
            .wait_for_phase(0, ScreenPhase::Idle, Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(matches!(err, WaitError::Timeout { .. }));
    }
}
