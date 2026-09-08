#![allow(clippy::collapsible_if)]

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

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
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub human_token: Option<String>,
    #[serde(skip)]
    pub grant: Option<crate::lease::LeaseGrant>,
    #[serde(default)]
    pub observation_gen: u64,
    #[serde(skip)]
    pub approval: Option<crate::approval::PendingApproval>,
    #[serde(skip)]
    pub observation_valid: bool,
}

pub type ScreenInfo = ScreenState;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LeaseResponse {
    #[serde(default = "default_status_ok")]
    pub status: String,
    pub id: u32,
    pub owner: String,
    pub token: String,
    #[serde(default = "default_handoff_gen")]
    pub handoff_gen: u64,
}

fn default_status_ok() -> String {
    "ok".to_string()
}

fn default_handoff_gen() -> u64 {
    1
}

impl LeaseResponse {
    pub fn new(
        id: u32,
        owner: impl Into<String>,
        token: impl Into<String>,
        handoff_gen: u64,
    ) -> Self {
        Self {
            status: "ok".to_string(),
            id,
            owner: owner.into(),
            token: token.into(),
            handoff_gen,
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
    HumanActive {
        id: u32,
    },
    Busy {
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
            Self::HumanActive { id } => write!(f, "human controls screen {id}"),
            Self::Busy { id } => write!(f, "screen {id} has tools in flight"),
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
    Busy {
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
            Self::Busy { id } => {
                write!(f, "screen {id} is busy with in-flight tool execution")
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
    busy_notify: tokio::sync::broadcast::Sender<u32>,
}

/// RAII guard that decrements a screen's active tool count on drop.
pub struct BusyGuard<'a> {
    agent: &'a AgentState,
    screen: u32,
}

impl BusyGuard<'_> {
    pub fn request_takeover(
        &self,
        reason: Option<String>,
        url: Option<String>,
        token: Option<&str>,
    ) -> Result<ScreenState, TakeoverError> {
        self.agent
            .transition_takeover(self.screen, reason, url, token, true)
    }

    pub fn allocate(
        &self,
        owner: &str,
        grant: crate::lease::LeaseGrant,
    ) -> Result<LeaseResponse, LeaseError> {
        self.agent.allocate(self.screen, owner, grant, true, true)
    }
}

impl<'a> Drop for BusyGuard<'a> {
    fn drop(&mut self) {
        self.agent.dec_busy(self.screen);
    }
}

struct ProvisionalLease {
    owner: String,
    token: String,
    handoff_gen: u64,
}

/// Owned admission for a lease provision operation.
///
/// Unlike `BusyGuard`, this guard can outlive the request future. That is
/// required when the runtime may continue a detached reset after cancellation:
/// the screen remains busy until the reset task commits or rolls back the
/// exact provisional token.
pub struct OwnedBusyGuard {
    agent: Arc<AgentState>,
    screen: u32,
    provisional: Option<ProvisionalLease>,
}

impl OwnedBusyGuard {
    pub fn allocate(
        &mut self,
        owner: &str,
        grant: crate::lease::LeaseGrant,
    ) -> Result<LeaseResponse, LeaseError> {
        let lease = self
            .agent
            .allocate(self.screen, owner, grant, true, false)?;
        self.provisional = Some(ProvisionalLease {
            owner: owner.to_owned(),
            token: lease.token.clone(),
            handoff_gen: lease.handoff_gen,
        });
        Ok(lease)
    }

    pub fn commit(&mut self) -> Result<(), LeaseError> {
        let Some(provisional) = self.provisional.as_ref() else {
            return Ok(());
        };
        self.agent.publish_provisional(
            self.screen,
            &provisional.owner,
            &provisional.token,
            provisional.handoff_gen,
        )?;
        self.provisional = None;
        Ok(())
    }

    pub fn rollback(&mut self) {
        let Some(provisional) = self.provisional.as_ref() else {
            return;
        };
        if self
            .agent
            .rollback_provisional(
                self.screen,
                &provisional.owner,
                &provisional.token,
                provisional.handoff_gen,
            )
            .is_ok()
        {
            self.provisional = None;
        }
    }
}

impl Drop for OwnedBusyGuard {
    fn drop(&mut self) {
        self.rollback();
        if self.provisional.is_none() {
            self.agent.dec_busy(self.screen);
        }
    }
}

impl AgentState {
    pub fn subscribe_phase(&self) -> tokio::sync::broadcast::Receiver<u32> {
        self.phase_notify.subscribe()
    }

    pub fn record_observation(&self, id: u32) -> Option<u64> {
        let mut screens = self.screens.lock().unwrap();
        let screen = screens.iter_mut().find(|s| s.id == id)?;
        screen.observation_gen += 1;
        screen.observation_valid = true;
        screen.approval = None;
        Some(screen.observation_gen)
    }

    pub fn invalidate_observation(&self, id: u32) {
        if let Some(screen) = self.screens.lock().unwrap().iter_mut().find(|s| s.id == id) {
            screen.observation_gen += 1;
            screen.observation_valid = false;
            screen.approval = None;
        }
    }

    pub fn authorize_action(
        &self,
        id: u32,
        observation: Option<u64>,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<Option<String>, &'static str> {
        let mut screens = self.screens.lock().unwrap();
        let screen = screens
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or("screen_not_found")?;
        let Some(grant) = &screen.grant else {
            return Ok(None);
        };
        if !crate::approval::requires_approval(tool, args, grant.account.is_some()) {
            return Ok(None);
        }
        if !screen.observation_valid || observation != Some(screen.observation_gen) {
            return Err("fresh_observation_required");
        }
        let request = crate::approval::PendingApproval::new(
            screen.lease_token.as_deref().ok_or("invalid_lease")?,
            grant,
            screen.handoff_gen,
            screen.observation_gen,
            tool,
            args,
        );
        if let Some(pending) = screen
            .approval
            .as_ref()
            .filter(|a| a.current() && a.request_key == request.request_key)
        {
            if !pending.approved {
                return Ok(Some(pending.digest.clone()));
            }
            screen.approval = None;
            screen.observation_valid = false;
            return Ok(None);
        }
        let digest = request.digest.clone();
        screen.approval = Some(request);
        Ok(Some(digest))
    }

    pub fn approve_action(&self, id: u32, digest: &str) -> Result<(), &'static str> {
        let mut screens = self.screens.lock().unwrap();
        let screen = screens
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or("screen_not_found")?;
        if screen.phase != ScreenPhase::AgentActive || screen.busy {
            return Err("screen_not_ready");
        }
        let approval = screen.approval.as_mut().ok_or("no_pending_action")?;
        if !approval.current() || approval.digest != digest {
            return Err("stale_approval");
        }
        approval.approved = true;
        Ok(())
    }

    pub fn new(n: u32) -> Self {
        let (phase_notify, _) = tokio::sync::broadcast::channel(128);
        let (busy_notify, _) = tokio::sync::broadcast::channel(128);
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
                human_token: None,
                grant: None,
                observation_gen: 0,
                approval: None,
                observation_valid: false,
            })
            .collect();
        Self {
            screens: Mutex::new(screens),
            active_tools: Mutex::new(HashMap::new()),
            phase_notify,
            busy_notify,
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
                    human_token: None,
                    grant: None,
                    observation_gen: 0,
                    approval: None,
                    observation_valid: false,
                });
            }
        }
    }

    /// Returns whether any active tool is currently running on `screen`.
    pub fn is_busy(&self, screen: u32) -> bool {
        let tools = self.active_tools.lock().unwrap();
        tools.get(&screen).copied().unwrap_or(0) > 0
    }

    /// Returns the number of active tools running on `screen`.
    pub fn busy_count(&self, screen: u32) -> u32 {
        let tools = self.active_tools.lock().unwrap();
        tools.get(&screen).copied().unwrap_or(0)
    }

    /// Mark `screen` as busy with an RAII guard that resets busy on drop.
    pub fn mark_busy(&self, screen: u32) -> BusyGuard<'_> {
        self.inc_busy(screen);
        BusyGuard {
            agent: self,
            screen,
        }
    }

    /// Bind authorization and generation to an in-flight operation under one lock.
    pub fn begin_tool(
        &self,
        screen: u32,
        token: Option<&str>,
        generation: Option<u64>,
    ) -> Result<BusyGuard<'_>, &'static str> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == screen)
            .ok_or("screen_not_found")?;
        if s.lease_token.as_deref() != token {
            return Err("invalid_lease");
        }
        if Some(s.handoff_gen) != generation {
            return Err("stale_plan");
        }
        if !matches!(s.phase, ScreenPhase::Idle | ScreenPhase::AgentActive) {
            return Err("takeover_active");
        }
        if s.busy {
            return Err("screen_busy");
        }
        *self.active_tools.lock().unwrap().entry(screen).or_insert(0) += 1;
        s.busy = true;
        Ok(BusyGuard {
            agent: self,
            screen,
        })
    }
    /// Bind an owned admission to an in-flight lease provision operation.
    ///
    /// The returned guard is backed by an `Arc`, so a spawned reset task can
    /// retain busy state if the HTTP request is cancelled.
    pub fn begin_tool_owned(
        self: &Arc<Self>,
        screen: u32,
        token: Option<&str>,
        generation: Option<u64>,
    ) -> Result<OwnedBusyGuard, &'static str> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == screen)
            .ok_or("screen_not_found")?;
        if s.lease_token.as_deref() != token {
            return Err("invalid_lease");
        }
        if Some(s.handoff_gen) != generation {
            return Err("stale_plan");
        }
        if !matches!(s.phase, ScreenPhase::Idle | ScreenPhase::AgentActive) {
            return Err("takeover_active");
        }
        if s.busy {
            return Err("screen_busy");
        }
        *self.active_tools.lock().unwrap().entry(screen).or_insert(0) += 1;
        s.busy = true;
        Ok(OwnedBusyGuard {
            agent: Arc::clone(self),
            screen,
            provisional: None,
        })
    }

    /// Increment the active tool counter on `screen` and synchronize `ScreenState::busy`.
    pub fn inc_busy(&self, screen: u32) {
        let mut screens = self.screens.lock().unwrap();
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
                    human_token: None,
                    grant: None,
                    observation_gen: 0,
                    approval: None,
                    observation_valid: false,
                });
            }
        }
        if let Some(s) = screens.iter_mut().find(|s| s.id == screen) {
            s.busy = is_busy;
        }
    }

    /// Decrement the active tool counter on `screen` and synchronize `ScreenState::busy`.
    pub fn dec_busy(&self, screen: u32) {
        let mut screens = self.screens.lock().unwrap();
        let is_busy = {
            let mut tools = self.active_tools.lock().unwrap();
            let count = tools.entry(screen).or_insert(0);
            *count = count.saturating_sub(1);
            *count > 0
        };

        if let Some(s) = screens.iter_mut().find(|s| s.id == screen) {
            s.busy = is_busy;
        }
        drop(screens);
        let _ = self.busy_notify.send(screen);
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
                    human_token: None,
                    grant: None,
                    observation_gen: 0,
                    approval: None,
                    observation_valid: false,
                });
            }
        }
        if let Some(s) = screens.iter_mut().find(|s| s.id == screen) {
            s.busy = is_busy;
        }
        drop(screens);
        let _ = self.busy_notify.send(screen);
    }

    /// Allocate a free screen. Owner labels are diagnostic, not credentials.
    pub fn lease_screen(&self, id: u32, owner: &str) -> Result<LeaseResponse, LeaseError> {
        self.allocate(id, owner, crate::lease::LeaseGrant::clean(), false, true)
    }

    fn allocate(
        &self,
        id: u32,
        owner: &str,
        grant: crate::lease::LeaseGrant,
        owns_permit: bool,
        publish: bool,
    ) -> Result<LeaseResponse, LeaseError> {
        let mut screens = self.screens.lock().unwrap();
        if screens.iter().any(|s| {
            s.id != id
                && s.owner.is_some()
                && (grant.account.is_some()
                    || s.grant.as_ref().is_some_and(|g| g.account.is_some()))
        }) {
            return Err(LeaseError::Busy { id });
        }
        let s = screens
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(LeaseError::NotFound(id))?;
        if s.owner.is_some() {
            return Err(LeaseError::NotOwner {
                id,
                expected: owner.to_string(),
                actual: s.owner.clone(),
            });
        }
        if s.phase != ScreenPhase::Idle
            || (s.busy && !(owns_permit && self.active_tools.lock().unwrap().get(&id) == Some(&1)))
        {
            return Err(LeaseError::Busy { id });
        }
        let token = uuid::Uuid::new_v4().to_string();
        s.owner = Some(owner.to_string());
        s.leased_at = Some(chrono::Utc::now().to_rfc3339());
        s.lease_token = Some(token.clone());
        s.phase = ScreenPhase::AgentActive;
        s.grant = Some(grant);
        s.observation_gen = 0;
        s.observation_valid = false;
        s.approval = None;
        let handoff_gen = s.handoff_gen;
        drop(screens);
        if publish {
            let _ = self.phase_notify.send(id);
        }
        Ok(LeaseResponse::new(id, owner, token, handoff_gen))
    }

    fn publish_provisional(
        &self,
        id: u32,
        owner: &str,
        token: &str,
        handoff_gen: u64,
    ) -> Result<(), LeaseError> {
        let screens = self.screens.lock().unwrap();
        let s = screens
            .iter()
            .find(|s| s.id == id)
            .ok_or(LeaseError::NotFound(id))?;
        if s.owner.as_deref() != Some(owner)
            || s.lease_token.as_deref() != Some(token)
            || s.handoff_gen != handoff_gen
            || !s.busy
        {
            return Err(LeaseError::InvalidToken { id });
        }
        drop(screens);
        let _ = self.phase_notify.send(id);
        Ok(())
    }

    fn rollback_provisional(
        &self,
        id: u32,
        owner: &str,
        token: &str,
        handoff_gen: u64,
    ) -> Result<(), LeaseError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(LeaseError::NotFound(id))?;
        if s.owner.as_deref() != Some(owner)
            || s.lease_token.as_deref() != Some(token)
            || s.handoff_gen != handoff_gen
            || !s.busy
        {
            return Err(LeaseError::InvalidToken { id });
        }
        Self::clear_lease(s);
        drop(screens);
        let _ = self.phase_notify.send(id);
        Ok(())
    }

    /// Release using the capability; only the supervisor may eject a human.
    pub fn release_screen(
        &self,
        id: u32,
        _owner: &str,
        token: Option<&str>,
    ) -> Result<(), LeaseError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(LeaseError::NotFound(id))?;

        if token.is_none() || s.lease_token.as_deref() != token {
            return Err(LeaseError::InvalidToken { id });
        }
        if s.phase == ScreenPhase::HumanActive {
            return Err(LeaseError::HumanActive { id });
        }
        if s.busy {
            return Err(LeaseError::Busy { id });
        }

        Self::clear_lease(s);
        drop(screens);
        let _ = self.phase_notify.send(id);
        Ok(())
    }

    /// Trusted control-plane operation. HTTP callers must prove supervisor authority.
    pub fn force_release_screen(&self, id: u32) -> Result<(), LeaseError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(LeaseError::NotFound(id))?;
        if s.busy {
            return Err(LeaseError::Busy { id });
        }
        Self::clear_lease(s);
        drop(screens);
        let _ = self.phase_notify.send(id);
        Ok(())
    }

    fn clear_lease(s: &mut ScreenState) {
        s.handoff_gen += 1;
        s.owner = None;
        s.leased_at = None;
        s.phase = ScreenPhase::Idle;
        s.takeover_pending = false;
        s.takeover_reason = None;
        s.takeover_url = None;
        s.lease_token = None;
        s.human_token = None;
        s.grant = None;
        s.observation_gen = 0;
        s.observation_valid = false;
        s.approval = None;
    }

    /// Return the active lease token for screen `id` if it is currently leased.
    pub fn lease_token(&self, id: u32) -> Option<String> {
        let screens = self.screens.lock().unwrap();
        screens
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.lease_token.clone())
    }

    pub fn screen_for_lease(&self, token: &str) -> Option<u32> {
        self.screens
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.lease_token.as_deref() == Some(token))
            .map(|s| s.id)
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
        lease_token: Option<&str>,
    ) -> Result<ScreenState, TakeoverError> {
        // Drain / wait up to 2 seconds if tools are in flight
        if self.is_busy(screen_id) {
            let start = std::time::Instant::now();
            let timeout = std::time::Duration::from_secs(2);
            while self.is_busy(screen_id) {
                if start.elapsed() >= timeout {
                    return Err(TakeoverError::Busy { id: screen_id });
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        self.transition_takeover(screen_id, reason, url, lease_token, false)
    }

    fn transition_takeover(
        &self,
        screen_id: u32,
        reason: Option<String>,
        url: Option<String>,
        lease_token: Option<&str>,
        owns_permit: bool,
    ) -> Result<ScreenState, TakeoverError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == screen_id)
            .ok_or(TakeoverError::NotFound(screen_id))?;
        if s.lease_token.as_deref() != lease_token {
            return Err(TakeoverError::InvalidToken { id: screen_id });
        }
        if s.busy && !(owns_permit && self.active_tools.lock().unwrap().get(&screen_id) == Some(&1))
        {
            return Err(TakeoverError::Busy { id: screen_id });
        }

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

        s.human_token = Some(uuid::Uuid::new_v4().to_string());
        s.takeover_url = url;
        s.approval = None;
        s.observation_gen = 0;
        s.observation_valid = false;

        let res = s.clone();
        drop(screens);
        let _ = self.phase_notify.send(screen_id);
        Ok(res)
    }

    /// Moves `HandoffPending` to `HumanActive`.
    pub fn human_connected(
        &self,
        screen_id: u32,
        token: Option<&str>,
    ) -> Result<ScreenState, TakeoverError> {
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
        if token.is_none() || s.human_token.as_deref() != token {
            return Err(TakeoverError::InvalidToken { id: screen_id });
        }

        s.phase = ScreenPhase::HumanActive;
        let res = s.clone();
        drop(screens);
        let _ = self.phase_notify.send(screen_id);
        Ok(res)
    }

    pub fn begin_viewer_input(
        &self,
        screen: u32,
        token: &str,
        generation: u64,
    ) -> Result<BusyGuard<'_>, TakeoverError> {
        let mut screens = self.screens.lock().unwrap();
        let state = screens
            .iter_mut()
            .find(|s| s.id == screen)
            .ok_or(TakeoverError::NotFound(screen))?;
        if state.phase != ScreenPhase::HumanActive
            || state.human_token.as_deref() != Some(token)
            || state.handoff_gen != generation
        {
            return Err(TakeoverError::InvalidToken { id: screen });
        }
        if state.busy {
            return Err(TakeoverError::Busy { id: screen });
        }
        state.busy = true;
        *self.active_tools.lock().unwrap().entry(screen).or_insert(0) += 1;
        Ok(BusyGuard {
            agent: self,
            screen,
        })
    }

    /// Moves `HumanActive` (or `HandoffPending`) to `HumanDone`, increments `handoff_gen`.
    pub fn human_handback(
        &self,
        screen_id: u32,
        token: Option<&str>,
    ) -> Result<ScreenState, TakeoverError> {
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
        if token.is_none() || s.human_token.as_deref() != token {
            return Err(TakeoverError::InvalidToken { id: screen_id });
        }
        if s.busy {
            return Err(TakeoverError::Busy { id: screen_id });
        }

        s.phase = ScreenPhase::HumanDone;
        s.handoff_gen += 1;
        let res = s.clone();
        drop(screens);
        let _ = self.phase_notify.send(screen_id);
        Ok(res)
    }

    /// Moves `HumanDone` to `AgentActive`, increments `handoff_gen`.
    pub fn agent_ack(
        &self,
        screen_id: u32,
        token: Option<&str>,
    ) -> Result<ScreenState, TakeoverError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == screen_id)
            .ok_or(TakeoverError::NotFound(screen_id))?;

        if s.lease_token.as_deref() != token {
            return Err(TakeoverError::InvalidToken { id: screen_id });
        }
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
        s.human_token = None;
        s.approval = None;
        s.observation_gen = 0;
        s.observation_valid = false;
        let res = s.clone();
        drop(screens);
        let _ = self.phase_notify.send(screen_id);
        Ok(res)
    }

    /// Cancels a takeover from `HandoffPending` back to `AgentActive` (or `Idle`), increments `handoff_gen`.
    /// Agents cannot cancel or eject human if already in `HumanActive`.
    pub fn cancel_takeover(
        &self,
        screen_id: u32,
        token: Option<&str>,
    ) -> Result<ScreenState, TakeoverError> {
        let mut screens = self.screens.lock().unwrap();
        let s = screens
            .iter_mut()
            .find(|s| s.id == screen_id)
            .ok_or(TakeoverError::NotFound(screen_id))?;
        if s.lease_token.as_deref() != token {
            return Err(TakeoverError::InvalidToken { id: screen_id });
        }

        if s.phase != ScreenPhase::HandoffPending {
            return Err(TakeoverError::InvalidPhase {
                id: screen_id,
                current: s.phase,
                expected: vec![ScreenPhase::HandoffPending],
            });
        }

        s.phase = if s.owner.is_some() {
            ScreenPhase::AgentActive
        } else {
            ScreenPhase::Idle
        };
        s.handoff_gen += 1;
        s.takeover_pending = false;
        s.takeover_reason = None;
        s.takeover_url = None;
        s.human_token = None;
        s.approval = None;
        s.observation_gen = 0;
        s.observation_valid = false;
        let res = s.clone();
        drop(screens);
        let _ = self.phase_notify.send(screen_id);
        Ok(res)
    }

    /// Set takeover pending flag and URL for a screen.
    pub fn set_takeover(
        &self,
        id: u32,
        pending: bool,
        url: Option<String>,
    ) -> Result<ScreenState, TakeoverError> {
        if pending {
            self.request_takeover(
                id,
                Some("takeover requested".into()),
                url,
                self.lease_token(id).as_deref(),
            )
        } else {
            self.cancel_takeover(id, self.lease_token(id).as_deref())
        }
    }

    /// Return the active human takeover token for screen `id` if present.
    pub fn human_token(&self, screen: u32) -> Option<String> {
        let screens = self.screens.lock().unwrap();
        screens
            .iter()
            .find(|s| s.id == screen)
            .and_then(|s| s.human_token.clone())
    }

    /// Verify whether `token` matches the active human token for screen `id`.
    pub fn verify_human_token(&self, screen: u32, token: &str) -> bool {
        let screens = self.screens.lock().unwrap();
        screens
            .iter()
            .find(|s| s.id == screen)
            .and_then(|s| s.human_token.as_deref())
            == Some(token)
    }

    /// Check whether `token` matches any active human token across all screens.
    pub fn has_human_token(&self, token: &str) -> bool {
        let screens = self.screens.lock().unwrap();
        screens
            .iter()
            .any(|s| s.human_token.as_deref() == Some(token))
    }

    /// Asynchronously wait for busy tool count to drain to 0 on `screen` up to `timeout`.
    pub async fn wait_for_drain(&self, screen: u32, timeout: std::time::Duration) -> bool {
        if !self.is_busy(screen) {
            return true;
        }
        let mut rx = self.busy_notify.subscribe();
        let sleep = tokio::time::sleep(timeout);
        tokio::pin!(sleep);

        loop {
            if !self.is_busy(screen) {
                return true;
            }
            tokio::select! {
                _ = &mut sleep => {
                    return !self.is_busy(screen);
                }
                res = rx.recv() => {
                    match res {
                        Ok(id) if id == screen => {
                            if !self.is_busy(screen) {
                                return true;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            if !self.is_busy(screen) {
                                return true;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return !self.is_busy(screen);
                        }
                        _ => {}
                    }
                }
            }
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
    fn lease_screen_specific() {
        let a = AgentState::new(2);
        a.lease_screen(1, "otto").unwrap();
        assert!(a.lease_screen(1, "otto").is_err());
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

        assert!(a.lease_screen(0, "browser-use").is_err());

        // Rejection on token mismatch
        let err = a
            .release_screen(0, "browser-use", Some("wrong-token"))
            .unwrap_err();
        assert_eq!(err, LeaseError::InvalidToken { id: 0 });
        assert!(a.is_leased(0));

        // Rejection on non-owner without token
        let err2 = a.release_screen(0, "intruder", None).unwrap_err();
        assert!(matches!(err2, LeaseError::InvalidToken { .. }));
        assert!(a.is_leased(0));

        // Success with valid token
        a.release_screen(0, "browser-use", Some(&res.token))
            .unwrap();
        assert!(!a.is_leased(0));
        assert_eq!(a.lease_token(0), None);
    }

    #[test]
    fn supervisor_release_revokes_capabilities_and_generation() {
        let a = AgentState::new(1);
        let lease = a.lease_screen(0, "worker").unwrap();
        a.request_takeover(0, None, None, a.lease_token(0).as_deref())
            .unwrap();
        a.human_connected(0, a.human_token(0).as_deref()).unwrap();
        let generation = a.handoff_gen(0).unwrap();
        a.force_release_screen(0).unwrap();
        assert_eq!(a.lease_token(0), None);
        assert_eq!(a.human_token(0), None);
        assert!(a.handoff_gen(0).unwrap() > generation);
        assert!(a.release_screen(0, "worker", Some(&lease.token)).is_err());
    }

    #[test]
    fn set_takeover_updates_flags() {
        let a = AgentState::new(1);
        let s = a
            .set_takeover(0, true, Some("http://localhost:6080/vnc.html".into()))
            .unwrap();
        let snap = a.snapshot();
        assert!(snap[0].takeover_pending);
        let tok = snap[0].human_token.as_ref().unwrap();
        assert!(!snap[0].takeover_url.as_deref().unwrap().contains(tok));
        assert_eq!(s.human_token.as_ref().unwrap(), tok);

        a.set_takeover(0, false, None).unwrap();
        let snap2 = a.snapshot();
        assert!(!snap2[0].takeover_pending);
        assert!(snap2[0].takeover_url.is_none());
        assert!(snap2[0].human_token.is_none());
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
                a.lease_token(0).as_deref(),
            )
            .unwrap();
        assert_eq!(res.phase, ScreenPhase::HandoffPending);
        assert_eq!(res.handoff_gen, 2);
        assert!(res.takeover_pending);
        assert_eq!(
            res.takeover_reason.as_deref(),
            Some("CAPTCHA challenge detected")
        );
        let human_tok = res.human_token.as_ref().unwrap();
        assert!(!res.takeover_url.as_deref().unwrap().contains(human_tok));
        assert!(a.verify_human_token(0, human_tok));
        assert!(!a.verify_human_token(0, "wrong-token"));

        // Cannot request takeover again when in HandoffPending
        let err = a
            .request_takeover(0, Some("again".into()), None, a.lease_token(0).as_deref())
            .unwrap_err();
        assert!(matches!(err, TakeoverError::InvalidPhase { .. }));

        // 3. Human connected -> moves HandoffPending to HumanActive
        let res = a.human_connected(0, a.human_token(0).as_deref()).unwrap();
        assert_eq!(res.phase, ScreenPhase::HumanActive);
        assert_eq!(res.handoff_gen, 2);

        // Cannot cancel takeover while HumanActive (reach-5zs)
        assert!(matches!(
            a.cancel_takeover(0, a.lease_token(0).as_deref()),
            Err(TakeoverError::InvalidPhase { .. })
        ));
        assert!(matches!(
            a.set_takeover(0, false, None),
            Err(TakeoverError::InvalidPhase { .. })
        ));

        // 4. Human handback -> moves HumanActive to HumanDone, increments gen to 3
        let res = a.human_handback(0, a.human_token(0).as_deref()).unwrap();
        assert_eq!(res.phase, ScreenPhase::HumanDone);
        assert_eq!(res.handoff_gen, 3);

        // Cannot human connected when HumanDone
        assert!(a.human_connected(0, a.human_token(0).as_deref()).is_err());

        // 5. Agent ack -> moves HumanDone to AgentActive, increments gen to 4, clears reason/url/human_token
        let res = a.agent_ack(0, a.lease_token(0).as_deref()).unwrap();
        assert_eq!(res.phase, ScreenPhase::AgentActive);
        assert_eq!(res.handoff_gen, 4);
        assert!(!res.takeover_pending);
        assert_eq!(res.takeover_reason, None);
        assert_eq!(res.takeover_url, None);
        assert_eq!(res.human_token, None);
    }

    #[test]
    fn human_handback_direct_from_handoff_pending() {
        let a = AgentState::new(1);
        let _ = a.lease_screen(0, "agent-bob").unwrap();
        a.request_takeover(
            0,
            Some("Login required".into()),
            None,
            a.lease_token(0).as_deref(),
        )
        .unwrap();
        assert_eq!(a.phase(0), Some(ScreenPhase::HandoffPending));
        assert_eq!(a.handoff_gen(0), Some(2));

        // Skip human_connected and hand back directly
        let res = a.human_handback(0, a.human_token(0).as_deref()).unwrap();
        assert_eq!(res.phase, ScreenPhase::HumanDone);
        assert_eq!(res.handoff_gen, 3);

        let res = a.agent_ack(0, a.lease_token(0).as_deref()).unwrap();
        assert_eq!(res.phase, ScreenPhase::AgentActive);
        assert_eq!(res.handoff_gen, 4);
    }

    #[tokio::test]
    async fn wait_for_phase_immediate_and_wake() {
        use std::sync::Arc;
        use std::time::Duration;

        let a = Arc::new(AgentState::new(1));
        let _ = a.lease_screen(0, "agent-sam").unwrap();
        a.request_takeover(
            0,
            Some("2FA required".into()),
            None,
            a.lease_token(0).as_deref(),
        )
        .unwrap();
        a.human_connected(0, a.human_token(0).as_deref()).unwrap();

        // Spawn a background task that sleeps 50ms and calls human_handback
        let a_clone = Arc::clone(&a);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            a_clone
                .human_handback(0, a_clone.human_token(0).as_deref())
                .unwrap();
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

    #[test]
    fn cancel_takeover_in_handoff_pending_succeeds() {
        let a = AgentState::new(1);
        let _ = a.lease_screen(0, "agent-eva").unwrap();
        a.request_takeover(
            0,
            Some("Solve captcha".into()),
            None,
            a.lease_token(0).as_deref(),
        )
        .unwrap();
        assert_eq!(a.phase(0), Some(ScreenPhase::HandoffPending));
        assert_eq!(a.handoff_gen(0), Some(2));
        assert!(a.human_token(0).is_some());

        // Cancel takeover before human connects
        let res = a.cancel_takeover(0, a.lease_token(0).as_deref()).unwrap();
        assert_eq!(res.phase, ScreenPhase::AgentActive);
        assert_eq!(res.handoff_gen, 3);
        assert!(!res.takeover_pending);
        assert_eq!(res.human_token, None);
        assert_eq!(res.takeover_reason, None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn request_takeover_drains_in_flight_busy_screen() {
        let a = std::sync::Arc::new(AgentState::new(1));
        let _ = a.lease_screen(0, "agent-dave").unwrap();

        // Mark busy
        a.inc_busy(0);
        assert!(a.is_busy(0));

        let a_clone = std::sync::Arc::clone(&a);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            a_clone.dec_busy(0);
        });

        // request_takeover should drain the in-flight tool and succeed
        let res = a
            .request_takeover(
                0,
                Some("drain test".into()),
                None,
                a.lease_token(0).as_deref(),
            )
            .unwrap();
        assert_eq!(res.phase, ScreenPhase::HandoffPending);
        assert!(!a.is_busy(0));
    }

    #[test]
    fn request_takeover_refuses_when_busy_timeout() {
        let a = AgentState::new(1);
        let _ = a.lease_screen(0, "agent-dave").unwrap();
        a.inc_busy(0);

        // Does not clear busy; should fail with TakeoverError::Busy after timeout
        let err = a.request_takeover(
            0,
            Some("drain test".into()),
            None,
            a.lease_token(0).as_deref(),
        );
        assert_eq!(err, Err(TakeoverError::Busy { id: 0 }));
    }

    #[test]
    fn approvals_expire_without_renewal_and_cannot_survive_navigation_or_consumption() {
        let agent = AgentState::new(1);
        agent.lease_screen(0, "worker").unwrap();
        let observation = agent.record_observation(0).unwrap();
        let action = serde_json::json!({"x":10,"y":20});
        let first = agent
            .authorize_action(0, Some(observation), "click", &action)
            .unwrap()
            .unwrap();
        let deadline = agent.screen_info(0).unwrap().approval.unwrap().expires;
        assert_eq!(
            agent
                .authorize_action(0, Some(observation), "click", &action)
                .unwrap(),
            Some(first.clone())
        );
        assert_eq!(
            agent.screen_info(0).unwrap().approval.unwrap().expires,
            deadline
        );
        agent.screens.lock().unwrap()[0]
            .approval
            .as_mut()
            .unwrap()
            .expires = std::time::Instant::now();
        let replacement = agent
            .authorize_action(0, Some(observation), "click", &action)
            .unwrap()
            .unwrap();
        assert_ne!(first, replacement);
        assert!(agent.approve_action(0, &first).is_err());
        agent.approve_action(0, &replacement).unwrap();
        agent.invalidate_observation(0);
        assert!(
            agent
                .authorize_action(0, Some(observation), "click", &action)
                .is_err()
        );
        let fresh = agent.record_observation(0).unwrap();
        let proposal = agent
            .authorize_action(0, Some(fresh), "click", &action)
            .unwrap()
            .unwrap();
        agent.approve_action(0, &proposal).unwrap();
        assert_eq!(
            agent
                .authorize_action(0, Some(fresh), "click", &action)
                .unwrap(),
            None
        );
        assert!(
            agent
                .authorize_action(0, Some(fresh), "click", &action)
                .is_err()
        );
    }

    #[test]
    fn handback_and_release_wait_for_admitted_viewer_input() {
        let agent = AgentState::new(1);
        let lease = agent.lease_screen(0, "worker").unwrap();
        let handoff = agent
            .request_takeover(0, None, None, Some(&lease.token))
            .unwrap();
        let human = handoff.human_token.unwrap();
        agent.human_connected(0, Some(&human)).unwrap();
        let input = agent
            .begin_viewer_input(0, &human, handoff.handoff_gen)
            .unwrap();
        assert!(matches!(
            agent.human_handback(0, Some(&human)),
            Err(TakeoverError::Busy { .. })
        ));
        assert!(matches!(
            agent.force_release_screen(0),
            Err(LeaseError::Busy { .. })
        ));
        drop(input);
        agent.human_handback(0, Some(&human)).unwrap();
        assert!(
            agent
                .begin_viewer_input(0, &human, handoff.handoff_gen)
                .is_err()
        );
    }
}
