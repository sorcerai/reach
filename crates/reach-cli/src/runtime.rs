use crate::config::{ReachConfig, RuntimeBackend};
use crate::docker::{
    self, AuthHandoffOptions, ExecOutput, PageActionOptions, PageTextOptions, Sandbox,
    SandboxConfig,
};
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::Engine;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const MAX_RPC_BODY: usize = 8 * 1024 * 1024;
const MAX_RPC_HEADERS: usize = 64 * 1024;
#[cfg(not(test))]
const RPC_DEADLINE: Duration = Duration::from_secs(130);
#[cfg(test)]
const RPC_DEADLINE: Duration = Duration::from_secs(2);

#[derive(serde::Deserialize)]
struct SupervisorHealth<'a> {
    #[serde(borrow)]
    status: Cow<'a, str>,
}

pub enum RuntimeClient {
    Docker(docker::DockerClient),
    MicroVm(MicroVmClient),
}

impl RuntimeClient {
    pub fn from_config(config: &ReachConfig) -> Result<Self> {
        match &config.runtime.backend {
            RuntimeBackend::Docker => Ok(Self::Docker(docker::DockerClient::new(
                config.docker.socket_path(),
            )?)),
            RuntimeBackend::Microvm => {
                let socket = config
                    .runtime
                    .broker_socket
                    .as_deref()
                    .filter(|path| !path.as_os_str().is_empty())
                    .context("runtime.backend=microvm requires runtime.broker_socket")?;
                Ok(Self::MicroVm(MicroVmClient::new(socket)?))
            }
        }
    }

    pub async fn create(&self, config: SandboxConfig) -> Result<Sandbox> {
        RuntimeOps::create(self, config).await
    }
    pub async fn destroy(&self, target: &str) -> Result<()> {
        RuntimeOps::destroy(self, target).await
    }
    pub async fn list(&self) -> Result<Vec<Sandbox>> {
        RuntimeOps::list(self).await
    }
    pub async fn find(&self, target: &str) -> Result<Sandbox> {
        RuntimeOps::find(self, target).await
    }
    pub async fn exec(&self, target: &str, command: &[String]) -> Result<ExecOutput> {
        RuntimeOps::exec(self, target, command).await
    }
    pub async fn exec_input(
        &self,
        target: &str,
        command: &[String],
        input: &[u8],
    ) -> Result<ExecOutput> {
        RuntimeOps::exec_input(self, target, command, input).await
    }
    pub async fn screenshot(&self, target: &str, display: &str) -> Result<Vec<u8>> {
        RuntimeOps::screenshot(self, target, display).await
    }
    pub async fn incarnation(&self, target: &str) -> Result<String> {
        RuntimeOps::incarnation(self, target).await
    }
    pub async fn reset_screen(&self, target: &str, screen: u32) -> Result<()> {
        RuntimeOps::reset_screen(self, target, screen).await
    }
    pub async fn page_text(
        &self,
        target: &str,
        opts: &PageTextOptions,
    ) -> Result<docker::PageTextOutput> {
        RuntimeOps::page_text(self, target, opts).await
    }
    pub async fn page_action(&self, target: &str, opts: &PageActionOptions) -> Result<String> {
        RuntimeOps::page_action(self, target, opts).await
    }
    pub async fn auth_handoff(
        &self,
        target: &str,
        opts: &AuthHandoffOptions,
    ) -> Result<docker::AuthHandoffOutput> {
        RuntimeOps::auth_handoff(self, target, opts).await
    }
    pub async fn inspect_config(&self, target: &str) -> Result<SandboxConfig> {
        RuntimeOps::inspect_config(self, target).await
    }
    pub async fn recreate(&self, target: &str, image: Option<String>) -> Result<Sandbox> {
        RuntimeOps::recreate(self, target, image).await
    }
    pub async fn wait_healthy(&self, target: &str, timeout: Duration) -> Result<()> {
        RuntimeOps::wait_healthy(self, target, timeout).await
    }
}

/// The selected runtime's lifecycle and execution boundary. Methods with a
/// default implementation are shared security-sensitive helpers, so Docker and
/// microVM cannot drift in screenshot, reset, or browser behavior.
#[async_trait]
pub trait RuntimeOps {
    async fn create(&self, config: SandboxConfig) -> Result<Sandbox>;
    async fn destroy(&self, target: &str) -> Result<()>;
    async fn list(&self) -> Result<Vec<Sandbox>>;
    async fn find(&self, target: &str) -> Result<Sandbox>;
    async fn exec(&self, target: &str, command: &[String]) -> Result<ExecOutput>;
    async fn exec_input(
        &self,
        target: &str,
        command: &[String],
        input: &[u8],
    ) -> Result<ExecOutput>;
    async fn incarnation(&self, target: &str) -> Result<String>;
    async fn inspect_config(&self, target: &str) -> Result<SandboxConfig>;

    async fn screenshot(&self, target: &str, display: &str) -> Result<Vec<u8>> {
        let shot_id = uuid::Uuid::new_v4().simple();
        let disp_clean = display.replace(':', "_");
        let shot_file = format!("/tmp/_reach_shot_{disp_clean}_{shot_id}.png");
        let out = self
            .exec(
                target,
                &[
                    "bash".into(),
                    "-c".into(),
                    format!(
                        "DISPLAY={display} scrot -z '{shot_file}' && base64 -w 0 '{shot_file}' && rm -f '{shot_file}'"
                    ),
                ],
            )
            .await?;
        if out.exit_code != 0 {
            bail!("screenshot failed: {}", out.stderr);
        }
        let clean: String = out.stdout.chars().filter(|c| !c.is_whitespace()).collect();
        base64::engine::general_purpose::STANDARD
            .decode(&clean)
            .context("failed to decode screenshot base64")
    }

    async fn reset_screen(&self, target: &str, screen: u32) -> Result<()> {
        let _ = docker::screen_cdp_port(screen)?;
        let output = self
            .exec(
                target,
                &[
                    "python3".into(),
                    "-c".into(),
                    docker::RESET_SCREEN_SCRIPT.into(),
                    screen.to_string(),
                ],
            )
            .await?;
        if output.exit_code != 0 {
            bail!(
                "screen reset failed closed for screen {screen}: {}",
                output.stderr.trim()
            );
        }
        Ok(())
    }

    async fn page_text(
        &self,
        target: &str,
        opts: &PageTextOptions,
    ) -> Result<docker::PageTextOutput> {
        let screen_num = opts
            .display
            .as_deref()
            .and_then(|d| d.strip_prefix(':'))
            .and_then(|n| n.parse().ok())
            .map(|d: u32| if d >= 99 { d - 99 } else { d })
            .unwrap_or(0);
        let payload = serde_json::json!({
            "url": opts.url,
            "wait_for": opts.wait_for,
            "selector": opts.selector,
            "format": opts.format,
            "timeout_ms": opts.timeout_ms,
            "user_data_dir": opts.user_data_dir,
            "hydrated_cookies": opts.hydrated_cookies,
            "display": opts.display,
            "screen": screen_num,
            "allowed_origins": opts.allowed_origins,
        });
        let payload =
            serde_json::to_vec(&payload).context("failed to serialize page_text payload")?;
        let out = self
            .exec_input(
                target,
                &docker::browser_helper_command(docker::PAGE_TEXT_SCRIPT),
                &payload,
            )
            .await?;
        docker::parse_page_text_exec_output(&out)
    }

    async fn page_action(&self, target: &str, opts: &PageActionOptions) -> Result<String> {
        let payload = serde_json::json!({
            "target_id": opts.target_id,
            "loader_id": opts.loader_id,
            "selector": opts.selector,
            "backend_node_id": opts.backend_node_id,
            "action": opts.action,
            "button": opts.button,
            "text": opts.text,
            "clear": opts.clear,
            "submit": opts.submit,
            "timeout_ms": opts.timeout_ms,
            "user_data_dir": opts.user_data_dir,
            "display": opts.display,
            "screen": opts.screen,
        });
        let payload =
            serde_json::to_vec(&payload).context("failed to serialize page action payload")?;
        // The locator timeout excludes helper startup and transport; bound the whole operation separately.
        let out = tokio::time::timeout(
            RPC_DEADLINE,
            self.exec_input(
                target,
                &docker::browser_helper_command(docker::PAGE_ACTION_SCRIPT),
                &payload,
            ),
        )
        .await
        .context("native page action timed out; outcome is uncertain and must be reconciled before retry")??;
        if out.exit_code != 0 {
            bail!("native page action helper failed");
        }
        let line = docker::last_json_line(&out.stdout)
            .context("native page action returned no outcome")?;
        let result: Value =
            serde_json::from_str(&line).context("native page action returned malformed output")?;
        if result.get("status").and_then(|v| v.as_str()) != Some("ok") {
            bail!("native page action rejected");
        }
        Ok(line)
    }

    async fn auth_handoff(
        &self,
        target: &str,
        opts: &AuthHandoffOptions,
    ) -> Result<docker::AuthHandoffOutput> {
        let payload = serde_json::json!({
            "url": opts.url,
            "wait_for_selector": opts.wait_for_selector,
            "wait_for_url_contains": opts.wait_for_url_contains,
            "timeout_seconds": opts.timeout_seconds,
            "user_data_dir": opts.user_data_dir,
            "storage_state": opts.storage_state,
            "reason": opts.reason,
            "display": opts.display,
            "headless": false,
        });
        let payload =
            serde_json::to_vec(&payload).context("failed to serialize auth_handoff payload")?;
        let out = self
            .exec_input(
                target,
                &docker::browser_helper_command(docker::AUTH_HANDOFF_SCRIPT),
                &payload,
            )
            .await?;
        docker::parse_auth_handoff_exec_output(&out)
    }

    async fn recreate(&self, target: &str, image: Option<String>) -> Result<Sandbox> {
        let sandbox = self.find(target).await?;
        let mut config = self.inspect_config(&sandbox.container_id).await?;
        if let Some(image) = image {
            config.image = image;
        }
        let name = config.name.clone();
        self.destroy(&sandbox.container_id).await?;
        self.create(config).await.with_context(|| {
            format!(
                "destroyed sandbox '{name}' but failed to recreate it; its workspace and \
                 profile directories are intact — rerun `reach recreate` with a valid --image, \
                 or `reach create --name {name} ...`"
            )
        })
    }

    async fn wait_healthy(&self, target: &str, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        let command = [
            "curl",
            "-sf",
            "--max-time",
            "2",
            "http://127.0.0.1:8400/health",
        ]
        .map(str::to_owned);
        loop {
            if tokio::time::Instant::now() > deadline {
                bail!("timeout waiting for sandbox '{}' to become healthy", target);
            }
            let out = tokio::time::timeout_at(deadline, self.exec(target, &command))
                .await
                .with_context(|| {
                    format!("timeout waiting for sandbox '{target}' to become healthy")
                })?;
            if let Ok(result) = out
                && result.exit_code == 0
                && let Ok(health) = serde_json::from_str::<SupervisorHealth<'_>>(&result.stdout)
                && health.status == "healthy"
            {
                return Ok(());
            }
            tokio::time::sleep_until(
                (tokio::time::Instant::now() + Duration::from_millis(500)).min(deadline),
            )
            .await;
        }
    }
}

#[async_trait]
impl RuntimeOps for RuntimeClient {
    async fn create(&self, config: SandboxConfig) -> Result<Sandbox> {
        match self {
            Self::Docker(client) => RuntimeOps::create(client, config).await,
            Self::MicroVm(client) => RuntimeOps::create(client, config).await,
        }
    }
    async fn destroy(&self, target: &str) -> Result<()> {
        match self {
            Self::Docker(client) => RuntimeOps::destroy(client, target).await,
            Self::MicroVm(client) => RuntimeOps::destroy(client, target).await,
        }
    }
    async fn list(&self) -> Result<Vec<Sandbox>> {
        match self {
            Self::Docker(client) => RuntimeOps::list(client).await,
            Self::MicroVm(client) => RuntimeOps::list(client).await,
        }
    }
    async fn find(&self, target: &str) -> Result<Sandbox> {
        match self {
            Self::Docker(client) => RuntimeOps::find(client, target).await,
            Self::MicroVm(client) => RuntimeOps::find(client, target).await,
        }
    }
    async fn exec(&self, target: &str, command: &[String]) -> Result<ExecOutput> {
        match self {
            Self::Docker(client) => RuntimeOps::exec(client, target, command).await,
            Self::MicroVm(client) => RuntimeOps::exec(client, target, command).await,
        }
    }
    async fn exec_input(
        &self,
        target: &str,
        command: &[String],
        input: &[u8],
    ) -> Result<ExecOutput> {
        match self {
            Self::Docker(client) => RuntimeOps::exec_input(client, target, command, input).await,
            Self::MicroVm(client) => RuntimeOps::exec_input(client, target, command, input).await,
        }
    }
    async fn incarnation(&self, target: &str) -> Result<String> {
        match self {
            Self::Docker(client) => RuntimeOps::incarnation(client, target).await,
            Self::MicroVm(client) => RuntimeOps::incarnation(client, target).await,
        }
    }
    async fn inspect_config(&self, target: &str) -> Result<SandboxConfig> {
        match self {
            Self::Docker(client) => RuntimeOps::inspect_config(client, target).await,
            Self::MicroVm(client) => RuntimeOps::inspect_config(client, target).await,
        }
    }
}

#[async_trait]
impl RuntimeOps for docker::DockerClient {
    async fn create(&self, config: SandboxConfig) -> Result<Sandbox> {
        docker::DockerClient::create(self, config).await
    }
    async fn destroy(&self, target: &str) -> Result<()> {
        docker::DockerClient::destroy(self, target).await
    }
    async fn list(&self) -> Result<Vec<Sandbox>> {
        docker::DockerClient::list(self).await
    }
    async fn find(&self, target: &str) -> Result<Sandbox> {
        docker::DockerClient::find(self, target).await
    }
    async fn exec(&self, target: &str, command: &[String]) -> Result<ExecOutput> {
        docker::DockerClient::exec(self, target, command).await
    }
    async fn exec_input(
        &self,
        target: &str,
        command: &[String],
        input: &[u8],
    ) -> Result<ExecOutput> {
        docker::DockerClient::exec_input(self, target, command, input).await
    }
    async fn incarnation(&self, target: &str) -> Result<String> {
        docker::DockerClient::incarnation(self, target).await
    }
    async fn inspect_config(&self, target: &str) -> Result<SandboxConfig> {
        docker::DockerClient::inspect_config(self, target).await
    }
}

pub struct MicroVmClient {
    socket: PathBuf,
}

impl MicroVmClient {
    pub fn new(socket: &Path) -> Result<Self> {
        if !socket.is_absolute() {
            bail!("microVM broker socket must be an absolute path");
        }
        Ok(Self {
            socket: socket.to_path_buf(),
        })
    }
    fn is_canonical_uuid(value: &str) -> bool {
        let bytes = value.as_bytes();
        if bytes.len() != 36 {
            return false;
        }
        for (index, byte) in bytes.iter().copied().enumerate() {
            if matches!(index, 8 | 13 | 18 | 23) {
                if byte != b'-' {
                    return false;
                }
            } else if !(byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')) {
                return false;
            }
        }
        true
    }

    async fn rpc<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let body = serde_json::to_vec(&serde_json::json!({"method": method, "params": params}))
            .context("failed to encode microVM broker request")?;
        if body.len() > MAX_RPC_BODY {
            bail!("microVM broker request exceeds 8 MiB limit");
        }
        let deadline = tokio::time::Instant::now() + RPC_DEADLINE;
        let mut stream = tokio::time::timeout_at(deadline, UnixStream::connect(&self.socket))
            .await
            .map_err(|error| Self::rpc_deadline_error(method, error))?
            .context("microVM broker unavailable")?;
        let header = format!(
            "POST /v1/rpc HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        tokio::time::timeout_at(deadline, stream.write_all(header.as_bytes()))
            .await
            .map_err(|error| Self::rpc_deadline_error(method, error))?
            .context("failed to send broker request")?;
        tokio::time::timeout_at(deadline, stream.write_all(&body))
            .await
            .map_err(|error| Self::rpc_deadline_error(method, error))?
            .context("failed to send broker request body")?;
        tokio::time::timeout_at(deadline, stream.flush())
            .await
            .map_err(|error| Self::rpc_deadline_error(method, error))?
            .context("failed to flush broker request")?;

        let mut reader = BufReader::new(stream);
        let mut header_bytes = Vec::with_capacity(1024);
        loop {
            let remaining = (MAX_RPC_HEADERS - header_bytes.len() + 1) as u64;
            let mut limited = (&mut reader).take(remaining);
            let count =
                tokio::time::timeout_at(deadline, limited.read_until(b'\n', &mut header_bytes))
                    .await
                    .map_err(|error| Self::rpc_deadline_error(method, error))?
                    .context("failed to read broker response headers")?;
            if count == 0 {
                bail!("microVM broker returned truncated HTTP headers");
            }
            if header_bytes.len() > MAX_RPC_HEADERS {
                bail!("microVM broker response headers exceed limit");
            }
            if header_bytes.ends_with(b"\r\n\r\n") {
                break;
            }
        }

        let header_text = std::str::from_utf8(&header_bytes)
            .context("microVM broker returned non-UTF8 headers")?;
        let mut lines = header_text.split("\r\n");
        let status_line = lines
            .next()
            .context("microVM broker returned no HTTP status line")?;
        let mut status_parts = status_line.splitn(3, ' ');
        let version = status_parts.next().unwrap_or_default();
        let status = status_parts
            .next()
            .filter(|code| code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|code| code.parse::<u16>().ok())
            .context("microVM broker returned malformed HTTP status")?;
        if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
            bail!("microVM broker returned unsupported HTTP protocol");
        }
        if !matches!(status, 200 | 400 | 404 | 409 | 503) {
            bail!("microVM broker returned unsupported HTTP status");
        }

        let mut content_length = None;
        for line in lines {
            if line.is_empty() {
                break;
            }
            let (name, value) = line
                .split_once(':')
                .context("microVM broker returned malformed HTTP header")?;
            if name.eq_ignore_ascii_case("transfer-encoding") {
                bail!("microVM broker response uses unsupported transfer encoding");
            }
            if name.eq_ignore_ascii_case("content-length") {
                if content_length.is_some() {
                    bail!("microVM broker response has conflicting Content-Length headers");
                }
                let parsed = value
                    .trim()
                    .parse::<usize>()
                    .context("microVM broker response has invalid Content-Length")?;
                if parsed > MAX_RPC_BODY {
                    bail!("microVM broker response exceeds 8 MiB limit");
                }
                content_length = Some(parsed);
            }
        }
        let body_len = content_length.context("microVM broker response missing Content-Length")?;
        let mut response_body = vec![0u8; body_len];
        tokio::time::timeout_at(deadline, reader.read_exact(&mut response_body))
            .await
            .map_err(|error| Self::rpc_deadline_error(method, error))?
            .context("microVM broker returned truncated response body")?;
        if !reader.buffer().is_empty() {
            bail!("microVM broker returned trailing response data");
        }

        let value: Value = serde_json::from_slice(&response_body)
            .context("microVM broker returned malformed JSON response")?;
        if status != 200 {
            let error = value
                .get("error")
                .and_then(Value::as_object)
                .context("microVM broker returned malformed error envelope")?;
            let code = error
                .get("code")
                .and_then(Value::as_str)
                .filter(|code| !code.is_empty() && code.len() <= 64)
                .context("microVM broker error missing safe code")?;
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .filter(|message| {
                    !message.is_empty()
                        && message.len() <= 512
                        && !message.chars().any(|ch| ch == '\r' || ch == '\n')
                })
                .context("microVM broker error missing safe message")?;
            bail!("microVM broker {code}: {message}");
        }
        let result = value
            .get("result")
            .cloned()
            .context("microVM broker response missing result")?;
        serde_json::from_value(result).context("microVM broker result had invalid shape")
    }

    fn rpc_deadline_error(method: &str, error: tokio::time::error::Elapsed) -> anyhow::Error {
        let context = if matches!(method, "create" | "destroy" | "exec" | "exec_input") {
            format!("microVM broker operation timed out; {method} outcome is uncertain")
        } else {
            format!("microVM broker operation timed out: {method}")
        };
        anyhow::Error::new(error).context(context)
    }

    async fn resolved_id(&self, target: &str) -> Result<String> {
        if Self::is_canonical_uuid(target) {
            Ok(target.to_owned())
        } else {
            Ok(self.find(target).await?.container_id)
        }
    }
}

#[async_trait]
impl RuntimeOps for MicroVmClient {
    async fn create(&self, config: SandboxConfig) -> Result<Sandbox> {
        docker::validate_sandbox_config(&config)?;
        self.rpc("create", serde_json::json!({"config": config}))
            .await
    }
    async fn destroy(&self, target: &str) -> Result<()> {
        let target_id = self.resolved_id(target).await?;
        self.rpc("destroy", serde_json::json!({"target": target_id}))
            .await
    }
    async fn list(&self) -> Result<Vec<Sandbox>> {
        self.rpc("list", serde_json::json!({})).await
    }
    async fn find(&self, target: &str) -> Result<Sandbox> {
        let by_id = Self::is_canonical_uuid(target);
        self.list()
            .await?
            .into_iter()
            .find(|sandbox| {
                if by_id {
                    sandbox.container_id == target
                } else {
                    sandbox.name == target
                }
            })
            .ok_or_else(|| anyhow::anyhow!("sandbox '{}' not found", target))
    }
    async fn exec(&self, target: &str, command: &[String]) -> Result<ExecOutput> {
        let target_id = self.resolved_id(target).await?;
        self.exec_input_resolved(&target_id, command, &[]).await
    }
    async fn exec_input(
        &self,
        target: &str,
        command: &[String],
        input: &[u8],
    ) -> Result<ExecOutput> {
        let target_id = self.resolved_id(target).await?;
        self.exec_input_resolved(&target_id, command, input).await
    }
    async fn incarnation(&self, target: &str) -> Result<String> {
        let target_id = self.resolved_id(target).await?;
        self.rpc("incarnation", serde_json::json!({"target": target_id}))
            .await
    }
    async fn inspect_config(&self, target: &str) -> Result<SandboxConfig> {
        let target_id = self.resolved_id(target).await?;
        self.rpc("inspect_config", serde_json::json!({"target": target_id}))
            .await
    }
}

impl MicroVmClient {
    async fn exec_input_resolved(
        &self,
        target: &str,
        command: &[String],
        input: &[u8],
    ) -> Result<ExecOutput> {
        let input_base64 = base64::engine::general_purpose::STANDARD.encode(input);
        self.rpc(
            "exec_input",
            serde_json::json!({"target": target, "command": command, "input_base64": input_base64}),
        )
        .await
    }
}
#[cfg(test)]
mod tests {
    use super::RuntimeClient;
    use crate::config::{ReachConfig, RuntimeBackend};
    use std::future::Future;
    use std::path::PathBuf;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    fn test_socket(label: &str) -> PathBuf {
        PathBuf::from("/tmp").join(format!(
            "reach-runtime-{label}-{}.sock",
            uuid::Uuid::new_v4()
        ))
    }

    async fn read_request(stream: &mut UnixStream) -> serde_json::Value {
        let mut reader = BufReader::new(stream);
        let mut length = None;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let mut body = vec![0; length.unwrap()];
        reader.read_exact(&mut body).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn response_server(
        path: PathBuf,
        response: String,
        keep_open: bool,
    ) -> impl Future<Output = ()> {
        let listener = UnixListener::bind(&path).unwrap();
        async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await;
            stream.write_all(response.as_bytes()).await.unwrap();
            if keep_open {
                std::future::pending::<()>().await;
            }
            std::fs::remove_file(path).unwrap();
        }
    }

    fn http_response(status: &str, body: &str, extra_headers: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra_headers}\r\n{}",
            body.len(),
            body
        )
    }

    #[tokio::test]
    async fn explicit_microvm_uses_only_the_broker_socket() {
        let path = test_socket("selection");
        let response = http_response("200 OK", r#"{"result":[]}"#, "Connection: close\r\n");
        let server = tokio::spawn(response_server(path.clone(), response, false));
        tokio::task::yield_now().await;
        let mut config = ReachConfig::default();
        config.runtime.backend = RuntimeBackend::Microvm;
        config.runtime.broker_socket = Some(path);
        let runtime = RuntimeClient::from_config(&config).unwrap();
        assert!(runtime.list().await.unwrap().is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn full_uuid_targets_are_id_only_not_names() {
        let path = test_socket("uuid");
        let old_id = uuid::Uuid::new_v4();
        let replacement_id = uuid::Uuid::new_v4();
        let body = format!(
            r#"{{"result":[{{"name":"{old_id}","container_id":"{replacement_id}","status":"running","image":"fixture","ports":{{"vnc":null,"novnc":6080,"health":null,"screens":1,"extra":[]}},"created_at":"2026-01-01T00:00:00Z","allow_exec":false}}]}}"#
        );
        let response = http_response("200 OK", &body, "Connection: close\r\n");
        let server = tokio::spawn(response_server(path.clone(), response, false));
        tokio::task::yield_now().await;
        let mut config = ReachConfig::default();
        config.runtime.backend = RuntimeBackend::Microvm;
        config.runtime.broker_socket = Some(path);
        let runtime = RuntimeClient::from_config(&config).unwrap();
        assert!(runtime.find(&old_id.to_string()).await.is_err());
        server.await.unwrap();
    }
    #[tokio::test]
    async fn destroy_uses_the_resolved_uuid_not_the_logical_name() {
        let path = test_socket("destroy");
        let name = "fixture";
        let container_id = uuid::Uuid::new_v4();
        let list_body = format!(
            r#"{{"result":[{{"name":"{name}","container_id":"{container_id}","status":"running","image":"fixture","ports":{{"vnc":null,"novnc":6080,"health":null,"screens":1,"extra":[]}},"created_at":"2026-01-01T00:00:00Z","allow_exec":false}}]}}"#
        );
        let list_response = http_response("200 OK", &list_body, "Connection: close\r\n");
        let destroy_response =
            http_response("200 OK", r#"{"result":null}"#, "Connection: close\r\n");
        let server_path = path.clone();
        let listener = UnixListener::bind(&server_path).unwrap();
        let server = tokio::spawn(async move {
            let (mut list_stream, _) = listener.accept().await.unwrap();
            read_request(&mut list_stream).await;
            list_stream
                .write_all(list_response.as_bytes())
                .await
                .unwrap();
            let (mut destroy_stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut destroy_stream).await;
            assert_eq!(request["params"]["target"], container_id.to_string());
            destroy_stream
                .write_all(destroy_response.as_bytes())
                .await
                .unwrap();
            let _ = std::fs::remove_file(server_path);
        });
        tokio::task::yield_now().await;
        let mut config = ReachConfig::default();
        config.runtime.backend = RuntimeBackend::Microvm;
        config.runtime.broker_socket = Some(path);
        let runtime = RuntimeClient::from_config(&config).unwrap();
        runtime.destroy(name).await.unwrap();
        server.await.unwrap();
    }
    #[tokio::test]
    async fn noncanonical_uuid_shaped_name_resolves_before_mutation() {
        let path = test_socket("noncanonical-name");
        let logical_name = "24289793-E334-48A7-ba61-5d9c9f041aad";
        let old_id = "deadbeef-abcd-4abc-8def-0123456789ab";
        let replacement_id = "feedface-cafe-49ab-9abc-fedcba987654";
        let list_body = format!(
            r#"{{"result":[{{"name":"{logical_name}","container_id":"{old_id}","status":"running","image":"fixture","ports":{{"vnc":null,"novnc":6080,"health":null,"screens":1,"extra":[]}},"created_at":"2026-01-01T00:00:00Z","allow_exec":false}}]}}"#
        );
        let list_response = http_response("200 OK", &list_body, "Connection: close\r\n");
        let destroy_response =
            http_response("200 OK", r#"{"result":null}"#, "Connection: close\r\n");
        let server_path = path.clone();
        let listener = UnixListener::bind(&server_path).unwrap();
        let server = tokio::spawn(async move {
            let (old_removed, replacement_survives) = loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                match request["method"].as_str() {
                    Some("list") => {
                        stream.write_all(list_response.as_bytes()).await.unwrap();
                    }
                    Some("destroy") => {
                        let target = request["params"]["target"].as_str().unwrap();
                        let victim = if target == logical_name {
                            replacement_id
                        } else {
                            target
                        };
                        let old_removed = victim == old_id;
                        let replacement_survives = victim != replacement_id;
                        stream.write_all(destroy_response.as_bytes()).await.unwrap();
                        break (old_removed, replacement_survives);
                    }
                    _ => panic!("unexpected broker request"),
                }
            };
            let _ = std::fs::remove_file(server_path);
            (old_removed, replacement_survives)
        });
        tokio::task::yield_now().await;
        let mut config = ReachConfig::default();
        config.runtime.backend = RuntimeBackend::Microvm;
        config.runtime.broker_socket = Some(path);
        let runtime = RuntimeClient::from_config(&config).unwrap();
        runtime.destroy(logical_name).await.unwrap();
        let (old_removed, replacement_survives) = server.await.unwrap();
        assert!(old_removed, "destroy must target the captured immutable ID");
        assert!(
            replacement_survives,
            "destroy must not resolve a rebound logical name"
        );
    }

    #[tokio::test]
    async fn content_length_response_does_not_require_eof() {
        let path = test_socket("framing");
        let response = http_response("200 OK", r#"{"result":[]}"#, "Connection: keep-alive\r\n");
        let server = tokio::spawn(response_server(path.clone(), response, true));
        tokio::task::yield_now().await;
        let mut config = ReachConfig::default();
        config.runtime.backend = RuntimeBackend::Microvm;
        config.runtime.broker_socket = Some(path.clone());
        let runtime = RuntimeClient::from_config(&config).unwrap();
        let result = runtime.list().await;
        server.abort();
        let _ = server.await;
        std::fs::remove_file(path).unwrap();
        assert!(result.unwrap().is_empty());
    }
    #[tokio::test]
    async fn stalled_action_outcome_requires_reconciliation() {
        let path = test_socket("action-uncertain");
        let listener = UnixListener::bind(&path).unwrap();
        let (committed, mut observed_commit) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut lookup, _) = listener.accept().await.unwrap();
            assert_eq!(read_request(&mut lookup).await["method"], "list");
            let container_id = uuid::Uuid::new_v4();
            let body = format!(
                r#"{{"result":[{{"name":"fixture","container_id":"{container_id}","status":"running","image":"fixture","ports":{{"vnc":null,"novnc":6080,"health":null,"screens":1,"extra":[]}},"created_at":"2026-01-01T00:00:00Z","allow_exec":true}}]}}"#
            );
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            lookup
                .write_all(http_response("200 OK", &body, "Connection: close\r\n").as_bytes())
                .await
                .unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert_eq!(request["method"], "exec_input");
            committed.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let mut config = ReachConfig::default();
        config.runtime.backend = RuntimeBackend::Microvm;
        config.runtime.broker_socket = Some(path.clone());
        let runtime = RuntimeClient::from_config(&config).unwrap();
        let options = crate::docker::PageActionOptions {
            target_id: "target".into(),
            loader_id: "loader".into(),
            selector: "#confirm".into(),
            backend_node_id: 1,
            action: "click".into(),
            button: "left".into(),
            text: String::new(),
            clear: false,
            submit: false,
            timeout_ms: 1_000,
            user_data_dir: "/profile".into(),
            display: ":99".into(),
            screen: 0,
        };
        let result = runtime.page_action("fixture", &options).await;
        server.abort();
        let _ = server.await;
        std::fs::remove_file(path).unwrap();
        assert!(
            observed_commit.try_recv().is_ok(),
            "action must have reached the broker"
        );
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("outcome is uncertain"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn stalled_broker_response_is_bounded_by_operation_deadline() {
        let path = test_socket("deadline");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let mut config = ReachConfig::default();
        config.runtime.backend = RuntimeBackend::Microvm;
        config.runtime.broker_socket = Some(path.clone());
        let runtime = RuntimeClient::from_config(&config).unwrap();
        let result = tokio::time::timeout(
            super::RPC_DEADLINE + std::time::Duration::from_secs(2),
            runtime.list(),
        )
        .await;
        server.abort();
        let _ = server.await;
        std::fs::remove_file(path).unwrap();
        let error = result
            .expect("RPC must enforce its own deadline")
            .unwrap_err();
        assert!(
            error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
        );
    }

    #[tokio::test]
    async fn supervisor_http_success_does_not_mean_healthy() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::Duration;

        let path = test_socket("health");
        let listener = UnixListener::bind(&path).unwrap();
        let healthy = Arc::new(AtomicBool::new(false));
        let server_health = healthy.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_request(&mut stream).await;
                let status = if server_health.load(Ordering::Acquire) {
                    "healthy"
                } else {
                    "degraded"
                };
                let body = serde_json::json!({"result": {
                    "exit_code": 0,
                    "stdout": serde_json::json!({"status": status}).to_string(),
                    "stderr": ""
                }})
                .to_string();
                let response = http_response("200 OK", &body, "Connection: close\r\n");
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let mut config = ReachConfig::default();
        config.runtime.backend = RuntimeBackend::Microvm;
        config.runtime.broker_socket = Some(path.clone());
        let runtime = RuntimeClient::from_config(&config).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let readiness = runtime.wait_healthy(&id, Duration::from_secs(8));
        tokio::pin!(readiness);
        let stayed_pending = tokio::time::timeout(Duration::from_secs(2), &mut readiness)
            .await
            .is_err();
        healthy.store(true, Ordering::Release);
        let became_healthy = if stayed_pending {
            matches!(
                tokio::time::timeout(Duration::from_secs(4), &mut readiness).await,
                Ok(Ok(()))
            )
        } else {
            false
        };
        server.abort();
        let _ = server.await;
        std::fs::remove_file(path).unwrap();
        assert!(
            stayed_pending,
            "degraded supervisor must not satisfy readiness"
        );
        assert!(became_healthy, "healthy supervisor must satisfy readiness");
    }

    #[tokio::test]
    async fn truncated_content_length_is_rejected() {
        let path = test_socket("truncated");
        let response =
            "HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\n{}".to_string();
        let server = tokio::spawn(response_server(path.clone(), response, false));
        tokio::task::yield_now().await;
        let mut config = ReachConfig::default();
        config.runtime.backend = RuntimeBackend::Microvm;
        config.runtime.broker_socket = Some(path);
        let runtime = RuntimeClient::from_config(&config).unwrap();
        assert!(runtime.list().await.is_err());
        server.await.unwrap();
    }
}
