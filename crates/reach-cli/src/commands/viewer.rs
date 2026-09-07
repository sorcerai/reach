#![allow(clippy::collapsible_if)]

use super::serve::AppState;
use axum::body::Bytes;
use axum::extract::ws::{Message as BrowserMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, HOST, ORIGIN, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_tungstenite::tungstenite::Message as UpstreamMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use url::Url;

pub const SESSION_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_SESSIONS: usize = 256;
const MAX_HTTP_BODY: usize = 8 * 1024 * 1024;
const MAX_RFB_BUFFER: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewerMode {
    Observer,
    Control,
}

#[derive(Debug, Clone)]
struct ViewerSession {
    id: String,
    screen: u32,
    mode: ViewerMode,
    sandbox: String,
    incarnation: String,
    novnc_port: u16,
    lease_token: Option<String>,
    handoff_gen: u64,
    human_token: Option<String>,
    expires_at: Instant,
    issued_at: Instant,
}

#[derive(Debug, Default)]
struct SessionStore {
    sessions: HashMap<String, ViewerSession>,
}

#[derive(Debug, Clone, Default)]
pub struct ViewerSessions {
    inner: Arc<Mutex<SessionStore>>,
}

impl ViewerSessions {
    fn prune(&self, now: Instant) {
        let mut store = self.inner.lock().expect("viewer session mutex poisoned");
        store.sessions.retain(|_, session| session.expires_at > now);
    }

    fn insert(&self, session: ViewerSession) {
        let now = Instant::now();
        let mut store = self.inner.lock().expect("viewer session mutex poisoned");
        store
            .sessions
            .retain(|_, existing| existing.expires_at > now);
        while store.sessions.len() >= MAX_SESSIONS {
            let oldest = store
                .sessions
                .iter()
                .min_by_key(|(_, existing)| existing.issued_at)
                .map(|(id, _)| id.clone());
            if let Some(id) = oldest {
                store.sessions.remove(&id);
            } else {
                break;
            }
        }
        store.sessions.insert(session.id.clone(), session);
    }

    fn get(&self, screen: u32, id: &str) -> Option<ViewerSession> {
        self.prune(Instant::now());
        let store = self.inner.lock().expect("viewer session mutex poisoned");
        store
            .sessions
            .get(id)
            .filter(|session| session.screen == screen)
            .cloned()
    }

    fn remove(&self, id: &str) {
        self.inner
            .lock()
            .expect("viewer session mutex poisoned")
            .sessions
            .remove(id);
    }

    fn revoke_screen(&self, screen: u32) {
        self.inner
            .lock()
            .expect("viewer session mutex poisoned")
            .sessions
            .retain(|_, session| session.screen != screen);
    }
}

#[derive(Debug, Deserialize, Default)]
struct SessionRequest {
    #[serde(default)]
    mode: Option<ViewerMode>,
}

#[derive(Debug, Clone)]
struct ProxyTarget {
    sandbox: String,
    incarnation: String,
    novnc_port: u16,
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/viewer/{id}", get(index_handler))
        .route(
            "/viewer/{id}/session",
            post(session_handler).get(websocket_handler),
        )
        .route("/viewer/{id}/handback", post(handback_handler))
        .route("/viewer/{id}/{*path}", get(asset_handler))
}

async fn index_handler(Path(screen): Path<u32>) -> Html<String> {
    Html(viewer_shell(screen))
}

async fn session_handler(
    State(state): State<Arc<AppState>>,
    Path(screen): Path<u32>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ViewerError> {
    let request = if body.is_empty() {
        SessionRequest::default()
    } else {
        serde_json::from_slice::<SessionRequest>(&body)
            .map_err(|_| error(StatusCode::BAD_REQUEST, "invalid viewer session request"))?
    };
    require_same_origin(&state, &headers)?;
    reject_query_token(&uri)?;
    require_supervisor_bearer(&state, &headers)?;

    let info = state
        .agent
        .screen_info(screen)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "screen not found"))?;
    let lease_token = info
        .lease_token
        .clone()
        .ok_or_else(|| error(StatusCode::CONFLICT, "screen is not leased"))?;
    let mode = request.mode.unwrap_or(ViewerMode::Observer);
    if mode == ViewerMode::Control
        && !matches!(
            info.phase,
            reach_cli::agent::ScreenPhase::HandoffPending
                | reach_cli::agent::ScreenPhase::HumanActive
        )
    {
        return Err(error(
            StatusCode::CONFLICT,
            "control sessions require HandoffPending or HumanActive",
        ));
    }

    let target = resolve_target(&state, screen, None).await?;
    let session_id = uuid::Uuid::new_v4().simple().to_string();
    let now = Instant::now();
    state.viewer_sessions.insert(ViewerSession {
        id: session_id.clone(),
        screen,
        mode,
        sandbox: target.sandbox,
        incarnation: target.incarnation,
        novnc_port: target.novnc_port,
        lease_token: Some(lease_token),
        handoff_gen: info.handoff_gen,
        human_token: info.human_token,
        expires_at: now + SESSION_TTL,
        issued_at: now,
    });

    let mut response = Json(serde_json::json!({
        "status": "ok",
        "screen": screen,
        "mode": mode,
        "expires_in": SESSION_TTL.as_secs(),
    }))
    .into_response();
    response.headers_mut().insert(
        SET_COOKIE,
        cookie_header(
            screen,
            &session_id,
            SESSION_TTL.as_secs(),
            state.public_host.starts_with("https://"),
        ),
    );
    Ok(response)
}

async fn handback_handler(
    State(state): State<Arc<AppState>>,
    Path(screen): Path<u32>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, ViewerError> {
    require_same_origin(&state, &headers)?;
    reject_query_token(&uri)?;
    let id = cookie_session_id(screen, &headers)
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "viewer session required"))?;
    let session = current_session(&state, screen, &id).await?;
    if session.mode != ViewerMode::Control {
        return Err(error(
            StatusCode::FORBIDDEN,
            "observer sessions cannot hand back control",
        ));
    }
    let token = session.human_token.as_deref().ok_or_else(|| {
        error(
            StatusCode::UNAUTHORIZED,
            "control handoff is no longer valid",
        )
    })?;
    state
        .agent
        .human_handback(screen, Some(token))
        .map_err(|_| error(StatusCode::CONFLICT, "human handback is no longer valid"))?;
    state.viewer_sessions.revoke_screen(screen);

    let mut response =
        Json(serde_json::json!({ "status": "ok", "screen": screen })).into_response();
    response
        .headers_mut()
        .insert(SET_COOKIE, expired_cookie_header(screen));
    Ok(response)
}

async fn websocket_handler(
    State(state): State<Arc<AppState>>,
    Path(screen): Path<u32>,
    uri: Uri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ViewerError> {
    require_same_origin(&state, &headers)?;
    reject_query_token(&uri)?;
    let id = cookie_session_id(screen, &headers)
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "viewer session required"))?;
    let session = current_session(&state, screen, &id).await?;
    let target = resolve_target(&state, screen, Some(&session)).await?;

    let state_for_task = state.clone();
    Ok(ws.on_upgrade(move |socket| proxy_socket(socket, state_for_task, session, id, target)))
}

async fn asset_handler(
    State(state): State<Arc<AppState>>,
    Path((screen, path)): Path<(u32, String)>,
    uri: Uri,
    headers: HeaderMap,
) -> Result<Response, ViewerError> {
    reject_query_token(&uri)?;
    let id = cookie_session_id(screen, &headers)
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "viewer session required"))?;
    current_session(&state, screen, &id).await?;
    let (content_type, body): (&str, &'static [u8]) = match path.as_str() {
        "vnc.html" => (
            "text/html; charset=utf-8",
            include_bytes!("../../assets/viewer.html"),
        ),
        "novnc.js" => (
            "text/javascript; charset=utf-8",
            include_bytes!("../../assets/novnc.js"),
        ),
        "NOVNC-LICENSE.txt" => (
            "text/plain; charset=utf-8",
            include_bytes!("../../assets/NOVNC-LICENSE.txt"),
        ),
        "novnc-source.tgz" => (
            "application/gzip",
            include_bytes!("../../assets/novnc-source.tgz"),
        ),
        _ => return Err(error(StatusCode::NOT_FOUND, "viewer asset not found")),
    };
    Ok((
        [
            (CONTENT_TYPE, content_type),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response())
}

async fn current_session(
    state: &Arc<AppState>,
    screen: u32,
    id: &str,
) -> Result<ViewerSession, ViewerError> {
    let session = state
        .viewer_sessions
        .get(screen, id)
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "viewer session expired"))?;
    let info = state
        .agent
        .screen_info(screen)
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "viewer scope revoked"))?;
    if info.lease_token != session.lease_token
        || info.handoff_gen != session.handoff_gen
        || info.human_token != session.human_token
    {
        state.viewer_sessions.remove(id);
        return Err(error(StatusCode::UNAUTHORIZED, "viewer scope revoked"));
    }
    Ok(session)
}

async fn resolve_target(
    state: &Arc<AppState>,
    screen: u32,
    pinned: Option<&ViewerSession>,
) -> Result<ProxyTarget, ViewerError> {
    let sandbox = state
        .default_sandbox
        .clone()
        .ok_or_else(|| error(StatusCode::SERVICE_UNAVAILABLE, "no sandbox is pinned"))?;
    let info = state
        .docker
        .find(&sandbox)
        .await
        .map_err(|_| error(StatusCode::BAD_GATEWAY, "pinned sandbox is unavailable"))?;
    if info.status != reach_cli::docker::SandboxStatus::Running {
        return Err(error(
            StatusCode::BAD_GATEWAY,
            "pinned sandbox is not running",
        ));
    }
    if screen >= info.ports.screens {
        return Err(error(
            StatusCode::NOT_FOUND,
            "screen is not present in sandbox",
        ));
    }
    let base = info
        .ports
        .novnc
        .ok_or_else(|| error(StatusCode::BAD_GATEWAY, "sandbox noVNC port is not mapped"))?;
    let port = base
        .checked_add(screen as u16)
        .ok_or_else(|| error(StatusCode::BAD_GATEWAY, "invalid sandbox noVNC port"))?;
    let incarnation = state
        .docker
        .incarnation(&sandbox)
        .await
        .map_err(|_| error(StatusCode::BAD_GATEWAY, "sandbox incarnation unavailable"))?;
    if let Some(pinned) = pinned {
        if pinned.sandbox != sandbox
            || pinned.incarnation != incarnation
            || pinned.novnc_port != port
        {
            state.viewer_sessions.revoke_screen(screen);
            return Err(error(StatusCode::UNAUTHORIZED, "viewer scope revoked"));
        }
    }
    Ok(ProxyTarget {
        sandbox,
        incarnation,
        novnc_port: port,
    })
}

async fn proxy_socket(
    mut browser: WebSocket,
    state: Arc<AppState>,
    session: ViewerSession,
    session_id: String,
    target: ProxyTarget,
) {
    if session.mode == ViewerMode::Control
        && state.agent.phase(session.screen) == Some(reach_cli::agent::ScreenPhase::HandoffPending)
    {
        let Some(token) = session.human_token.as_deref() else {
            let _ = browser.send(BrowserMessage::Close(None)).await;
            return;
        };
        if state
            .agent
            .human_connected(session.screen, Some(token))
            .is_err()
        {
            let _ = browser.send(BrowserMessage::Close(None)).await;
            return;
        }
    }

    let upstream_url = match Url::parse(&format!("ws://127.0.0.1:{}/websockify", target.novnc_port))
    {
        Ok(url) => url,
        Err(_) => {
            let _ = browser.send(BrowserMessage::Close(None)).await;
            return;
        }
    };
    let Ok(mut request) = upstream_url.as_str().into_client_request() else {
        return;
    };
    let Ok(authorization) = HeaderValue::from_str(&format!("Bearer {}", state.raw_viewer_token))
    else {
        return;
    };
    request.headers_mut().insert(AUTHORIZATION, authorization);
    let (mut upstream, _) = match tokio_tungstenite::connect_async(request).await {
        Ok(connection) => connection,
        Err(_) => {
            let _ = browser.send(BrowserMessage::Close(None)).await;
            return;
        }
    };

    let mut rfb = RfbClientFilter::observer_or_control(session.mode);
    let mut phase_rx = state.agent.subscribe_phase();
    let mut scope_tick = tokio::time::interval(Duration::from_secs(1));
    scope_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = scope_tick.tick() => {
                if current_session(&state, session.screen, &session_id).await.is_err()
                    || resolve_target(&state, session.screen, Some(&session)).await.is_err()
                {
                    state.viewer_sessions.remove(&session_id);
                    let _ = browser.send(BrowserMessage::Close(None)).await;
                    let _ = upstream.close(None).await;
                    return;
                }
            }
            phase = phase_rx.recv() => {
                if matches!(phase, Ok(id) if id == session.screen)
                    && current_session(&state, session.screen, &session_id).await.is_err()
                {
                    state.viewer_sessions.revoke_screen(session.screen);
                    let _ = browser.send(BrowserMessage::Close(None)).await;
                    let _ = upstream.close(None).await;
                    return;
                }
            }
            incoming = browser.recv() => {
                let Some(Ok(message)) = incoming else { break; };
                match message {
                    BrowserMessage::Binary(bytes) => {
                        if bytes.len() > MAX_HTTP_BODY {
                            break;
                        }
                        let outputs = match rfb.feed_client(&bytes) {
                            Ok(outputs) => outputs,
                            Err(_) => break,
                        };
                        for output in outputs {
                            if current_session(&state, session.screen, &session_id).await.is_err() { return; }
                            let _input = if session.mode == ViewerMode::Control {
                                match state.agent.begin_viewer_input(session.screen, session.human_token.as_deref().unwrap_or(""), session.handoff_gen) {
                                    Ok(permit) => Some(permit),
                                    Err(_) => return,
                                }
                            } else { None };
                            if upstream.send(UpstreamMessage::Binary(output.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                    BrowserMessage::Ping(bytes) => {
                        if upstream.send(UpstreamMessage::Ping(bytes)).await.is_err() { break; }
                    }
                    BrowserMessage::Pong(bytes) => {
                        if upstream.send(UpstreamMessage::Pong(bytes)).await.is_err() { break; }
                    }
                    BrowserMessage::Close(_) => break,
                    BrowserMessage::Text(_) => break,
                }
            }
            incoming = upstream.next() => {
                let Some(Ok(message)) = incoming else { break; };
                match message {
                    UpstreamMessage::Binary(bytes) => {
                        if bytes.len() > MAX_HTTP_BODY || rfb.observe_server(&bytes).is_err() {
                            break;
                        }
                        if browser.send(BrowserMessage::Binary(bytes)).await.is_err() { break; }
                    }
                    UpstreamMessage::Ping(bytes) => {
                        if browser.send(BrowserMessage::Ping(bytes)).await.is_err() { break; }
                    }
                    UpstreamMessage::Pong(bytes) => {
                        if browser.send(BrowserMessage::Pong(bytes)).await.is_err() { break; }
                    }
                    UpstreamMessage::Close(_) => break,
                    UpstreamMessage::Text(_) => break,
                    UpstreamMessage::Frame(_) => break,
                }
            }
        }
    }
    // Keep the short-lived cookie session available for reconnect and handback.
    let _ = upstream.close(None).await;
    let _ = browser.send(BrowserMessage::Close(None)).await;
}

fn require_supervisor_bearer(
    state: &Arc<AppState>,
    headers: &HeaderMap,
) -> Result<(), ViewerError> {
    let expected = state
        .auth_token
        .as_deref()
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "supervisor bearer is not configured",
            )
        })?;
    let actual = headers
        .get(AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    if actual != Some(expected) {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            "valid supervisor bearer required",
        ));
    }
    Ok(())
}

fn require_same_origin(state: &Arc<AppState>, headers: &HeaderMap) -> Result<(), ViewerError> {
    let origin = headers
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| error(StatusCode::FORBIDDEN, "same-origin Origin is required"))?;
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| error(StatusCode::FORBIDDEN, "same-origin Host is required"))?;
    let origin_url =
        Url::parse(origin).map_err(|_| error(StatusCode::FORBIDDEN, "invalid Origin"))?;
    if !matches!(origin_url.scheme(), "http" | "https") {
        return Err(error(StatusCode::FORBIDDEN, "invalid Origin scheme"));
    }
    let origin_host = origin_url
        .host_str()
        .ok_or_else(|| error(StatusCode::FORBIDDEN, "invalid Origin host"))?;
    let scheme = if state.public_host.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    let request_url = Url::parse(&format!("{scheme}://{host}"))
        .map_err(|_| error(StatusCode::FORBIDDEN, "invalid Host"))?;
    let request_host = request_url
        .host_str()
        .ok_or_else(|| error(StatusCode::FORBIDDEN, "invalid Host"))?;
    let same_port = origin_url.port_or_known_default() == request_url.port_or_known_default();
    if origin_url.scheme() != scheme
        || !same_port
        || !origin_host.eq_ignore_ascii_case(request_host)
        || !state.is_allowed_host(host)
    {
        return Err(error(
            StatusCode::FORBIDDEN,
            "cross-origin request rejected",
        ));
    }
    Ok(())
}

fn reject_query_token(uri: &Uri) -> Result<(), ViewerError> {
    if uri.query().is_some_and(|query| {
        let lower = query.to_ascii_lowercase();
        lower.contains("token")
            || query
                .split('&')
                .any(|pair| pair.split('=').next().is_some_and(|key| key.contains('%')))
    }) {
        return Err(error(
            StatusCode::FORBIDDEN,
            "query capabilities are not accepted",
        ));
    }
    Ok(())
}

fn cookie_name(screen: u32) -> String {
    format!("reach_viewer_{screen}")
}

fn cookie_path(screen: u32) -> String {
    format!("/viewer/{screen}")
}

fn cookie_session_id(screen: u32, headers: &HeaderMap) -> Option<String> {
    let name = cookie_name(screen);
    headers
        .get(COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|header| {
            header.split(';').find_map(|pair| {
                let (key, value) = pair.trim().split_once('=')?;
                (key == name && !value.is_empty()).then(|| value.to_string())
            })
        })
}

fn cookie_header(screen: u32, id: &str, max_age: u64, secure: bool) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{}={}; Path={}; Max-Age={}; HttpOnly; SameSite=Strict{}",
        cookie_name(screen),
        id,
        cookie_path(screen),
        max_age,
        if secure { "; Secure" } else { "" }
    ))
    .expect("viewer cookie header is valid")
}

fn expired_cookie_header(screen: u32) -> HeaderValue {
    cookie_header(screen, "", 0, false)
}

#[derive(Debug)]
struct ViewerError {
    status: StatusCode,
    message: &'static str,
}

impl IntoResponse for ViewerError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

fn error(status: StatusCode, message: &'static str) -> ViewerError {
    ViewerError { status, message }
}

fn viewer_shell(screen: u32) -> String {
    let path = format!("/viewer/{screen}");
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Reach live viewer</title>
<style>
:root{{color-scheme:dark;font:16px system-ui,sans-serif;background:#111827;color:#f9fafb}}
body{{margin:0;min-height:100vh;display:grid;place-items:center}}main{{width:min(58rem,94vw);display:grid;gap:1rem}}form{{display:flex;flex-wrap:wrap;gap:.6rem}}input,button{{font:inherit;padding:.65rem .8rem;border:1px solid #4b5563;border-radius:.4rem;background:#1f2937;color:inherit}}button{{cursor:pointer;background:#2563eb;border-color:#2563eb}}#status{{min-height:1.5rem;color:#fbbf24}}iframe{{display:none;width:100%;height:min(72vh,48rem);border:1px solid #374151;border-radius:.5rem;background:#000}}
</style></head>
<body><main>
<h1>Reach live viewer</h1><p>Enter the configured supervisor bearer to start a screen-scoped session.</p>
<form id="login"><label for="bearer">Supervisor bearer</label><input id="bearer" type="password" autocomplete="off" required><label for="mode">Mode</label><select id="mode"><option value="observer">Observer (read-only)</option><option value="control">Control (handoff only)</option></select><button type="submit">Open viewer</button></form>
<div id="status" role="status"></div><button id="handback" hidden type="button">Hand back to agent</button><iframe id="screen" title="Reach live screen"></iframe>
<script>
const form=document.getElementById('login'), status=document.getElementById('status'), frame=document.getElementById('screen'), handback=document.getElementById('handback');
form.addEventListener('submit',async(event)=>{{event.preventDefault();status.textContent='Opening…';
 const bearer=document.getElementById('bearer').value, mode=document.getElementById('mode').value;
 try{{const response=await fetch('{path}/session',{{method:'POST',headers:{{'Authorization':'Bearer '+bearer,'Content-Type':'application/json'}},body:JSON.stringify({{mode}})}});
  if(!response.ok)throw new Error('Access denied'); document.getElementById('bearer').value='';form.style.display='none';status.textContent='Connected';
  handback.hidden=mode!=='control';frame.src='{path}/vnc.html?mode='+encodeURIComponent(mode);frame.style.display='block';
 }}catch(error){{status.textContent=error.message;}}
}});
handback.addEventListener('click',async()=>{{const response=await fetch('{path}/handback',{{method:'POST'}});if(response.ok){{status.textContent='Handed back';handback.hidden=true;}}else status.textContent='Handback rejected';}});
</script></main></body></html>"##
    )
}

#[derive(Debug)]
pub enum RfbError {
    Protocol(&'static str),
    UnknownClientMessage(u8),
    FrameTooLarge,
}

impl std::fmt::Display for RfbError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Protocol(message) => formatter.write_str(message),
            Self::UnknownClientMessage(kind) => write!(formatter, "unknown RFB message: {kind}"),
            Self::FrameTooLarge => formatter.write_str("RFB frame too large"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientStage {
    Protocol,
    SecuritySelection,
    VncResponse,
    SecurityResult,
    ClientInit,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerStage {
    Version,
    SecurityTypes,
    ClientSelection,
    VncChallenge,
    VncResponse,
    SecurityResult,
    ClientInit,
    Active,
}

#[derive(Debug)]
pub struct RfbClientFilter {
    observer: bool,
    client_stage: ClientStage,
    server_stage: ServerStage,
    client_buffer: Vec<u8>,
    server_buffer: Vec<u8>,
    auth_types: Vec<u8>,
}

impl RfbClientFilter {
    fn observer_or_control(mode: ViewerMode) -> Self {
        Self {
            observer: mode == ViewerMode::Observer,
            client_stage: ClientStage::Protocol,
            server_stage: ServerStage::Version,
            client_buffer: Vec::new(),
            server_buffer: Vec::new(),
            auth_types: Vec::new(),
        }
    }

    pub fn observe_server(&mut self, bytes: &[u8]) -> Result<(), RfbError> {
        if matches!(self.server_stage, ServerStage::Active) {
            self.server_buffer.clear();
            return Ok(());
        }
        self.server_buffer.extend_from_slice(bytes);
        if self.server_buffer.len() > MAX_RFB_BUFFER {
            return Err(RfbError::FrameTooLarge);
        }
        self.drain_server()
    }

    pub fn feed_client(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>, RfbError> {
        self.client_buffer.extend_from_slice(bytes);
        if self.client_buffer.len() > MAX_RFB_BUFFER {
            return Err(RfbError::FrameTooLarge);
        }
        let mut output = Vec::new();
        loop {
            match self.client_stage {
                ClientStage::Protocol => {
                    let Some(message) = self.take_client(12) else {
                        break;
                    };
                    if !supported_version(&message) {
                        return Err(RfbError::Protocol("RFB 3.7/3.8 is required"));
                    }
                    self.client_stage = ClientStage::SecuritySelection;
                    output.push(message);
                }
                ClientStage::SecuritySelection => {
                    let Some(message) = self.take_client(1) else {
                        break;
                    };
                    let auth = message[0];
                    if !self.auth_types.contains(&auth) || !matches!(auth, 1 | 2) {
                        return Err(RfbError::Protocol(
                            "only None and VNC authentication are supported",
                        ));
                    }
                    if auth == 1 {
                        self.client_stage = ClientStage::SecurityResult;
                        self.server_stage = ServerStage::SecurityResult;
                    } else {
                        self.client_stage = ClientStage::VncResponse;
                        self.server_stage = ServerStage::VncChallenge;
                    }
                    self.drain_server()?;
                    output.push(message);
                }
                ClientStage::VncResponse => {
                    let Some(message) = self.take_client(16) else {
                        break;
                    };
                    self.client_stage = ClientStage::SecurityResult;
                    self.server_stage = ServerStage::SecurityResult;
                    output.push(message);
                }
                ClientStage::SecurityResult => {
                    if !self.client_buffer.is_empty() {
                        return Err(RfbError::Protocol(
                            "client data arrived before SecurityResult",
                        ));
                    }
                    break;
                }
                ClientStage::ClientInit => {
                    let Some(mut message) = self.take_client(1) else {
                        break;
                    };
                    // Always request a shared desktop so an observer cannot disconnect
                    // the agent's existing RFB client.
                    message[0] = 1;
                    self.client_stage = ClientStage::Active;
                    self.server_stage = ServerStage::Active;
                    output.push(message);
                }
                ClientStage::Active => {
                    let Some((message, forward)) = self.take_active()? else {
                        break;
                    };
                    if forward {
                        output.push(message);
                    }
                }
            }
        }
        Ok(output)
    }

    fn drain_server(&mut self) -> Result<(), RfbError> {
        loop {
            match self.server_stage {
                ServerStage::Version => {
                    let Some(message) = self.take_server(12) else {
                        break;
                    };
                    if !supported_version(&message) {
                        return Err(RfbError::Protocol("RFB 3.7/3.8 is required"));
                    }
                    self.server_stage = ServerStage::SecurityTypes;
                }
                ServerStage::SecurityTypes => {
                    let Some(count) = self.server_buffer.first().copied() else {
                        break;
                    };
                    if count == 0 {
                        return Err(RfbError::Protocol(
                            "RFB authentication failure is unsupported",
                        ));
                    }
                    let total = 1 + count as usize;
                    if self.server_buffer.len() < total {
                        break;
                    }
                    let message = self.take_server(total).expect("length checked");
                    self.auth_types = message[1..]
                        .iter()
                        .copied()
                        .filter(|auth| matches!(auth, 1 | 2))
                        .collect();
                    if self.auth_types.is_empty() {
                        return Err(RfbError::Protocol(
                            "server offered no supported authentication",
                        ));
                    }
                    self.server_stage = ServerStage::ClientSelection;
                }
                ServerStage::ClientSelection => break,
                ServerStage::VncChallenge => {
                    let Some(_) = self.take_server(16) else {
                        break;
                    };
                    self.server_stage = ServerStage::VncResponse;
                }
                ServerStage::VncResponse => break,
                ServerStage::SecurityResult => {
                    let Some(result) = self.take_server(4) else {
                        break;
                    };
                    if result.iter().any(|byte| *byte != 0) {
                        return Err(RfbError::Protocol("RFB authentication failed"));
                    }
                    self.client_stage = ClientStage::ClientInit;
                    self.server_stage = ServerStage::ClientInit;
                }
                ServerStage::ClientInit => break,
                ServerStage::Active => break,
            }
        }
        Ok(())
    }

    fn take_client(&mut self, len: usize) -> Option<Vec<u8>> {
        if self.client_buffer.len() < len {
            return None;
        }
        Some(self.client_buffer.drain(..len).collect())
    }

    fn take_server(&mut self, len: usize) -> Option<Vec<u8>> {
        if self.server_buffer.len() < len {
            return None;
        }
        Some(self.server_buffer.drain(..len).collect())
    }

    fn take_active(&mut self) -> Result<Option<(Vec<u8>, bool)>, RfbError> {
        let Some(kind) = self.client_buffer.first().copied() else {
            return Ok(None);
        };
        let (len, forward) = match kind {
            0 => (20, true),
            2 => {
                if self.client_buffer.len() < 4 {
                    return Ok(None);
                }
                let count =
                    u16::from_be_bytes([self.client_buffer[2], self.client_buffer[3]]) as usize;
                let len = 4usize
                    .checked_add(count.checked_mul(4).ok_or(RfbError::FrameTooLarge)?)
                    .ok_or(RfbError::FrameTooLarge)?;
                if len > MAX_RFB_BUFFER {
                    return Err(RfbError::FrameTooLarge);
                }
                if self.client_buffer.len() < len {
                    return Ok(None);
                }
                (len, true)
            }
            3 => (10, true),
            4 => (8, !self.observer),
            5 => (6, !self.observer),
            6 => {
                if self.client_buffer.len() < 8 {
                    return Ok(None);
                }
                let length = u32::from_be_bytes([
                    self.client_buffer[4],
                    self.client_buffer[5],
                    self.client_buffer[6],
                    self.client_buffer[7],
                ]) as usize;
                let len = 8usize.checked_add(length).ok_or(RfbError::FrameTooLarge)?;
                if len > MAX_RFB_BUFFER {
                    return Err(RfbError::FrameTooLarge);
                }
                (len, !self.observer)
            }
            other => return Err(RfbError::UnknownClientMessage(other)),
        };
        if self.client_buffer.len() < len {
            return Ok(None);
        }
        let mut message = self.take_client(len).expect("length checked");
        if kind == 2 {
            let mut end = 4;
            for start in (4..message.len()).step_by(4) {
                let encoding = i32::from_be_bytes(
                    message[start..start + 4]
                        .try_into()
                        .expect("encoding width"),
                );
                if supported_encoding(encoding) {
                    message.copy_within(start..start + 4, end);
                    end += 4;
                }
            }
            message[2..4].copy_from_slice(&(((end - 4) / 4) as u16).to_be_bytes());
            message.truncate(end);
        }
        Ok(Some((message, forward)))
    }
}

fn supported_version(version: &[u8]) -> bool {
    version == b"RFB 003.007\n" || version == b"RFB 003.008\n"
}

fn supported_encoding(encoding: i32) -> bool {
    // Do not negotiate extensions that add unframed client input messages.
    matches!(
        encoding,
        0 | 1
            | 2
            | 5
            | 6
            | 7
            | 16
            | 21
            | 50
            | -260
            | -223
            | -224
            | -239
            | -261
            | -307
            | 0x574d_5664
    ) || (-256..=-247).contains(&encoding)
        || (-32..=-23).contains(&encoding)
}
#[cfg(test)]
#[path = "viewer_tests.rs"]
mod viewer_tests;
