# Agent Computer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn reach into a Grok Bot–style Agent Computer for hermes: a persistent desktop sandbox with live view, login takeover, shared browser sessions, routines, and (phase 3) one screen per bot.

**Architecture:** reach stays the sandbox and gains an "agent" surface (public URLs, workspace volume, `live_view`, later a screen-lease API). hermes integrates via `mcp_servers` (SSE) plus one skill in phase 1, and a small plugin against the lease API in phase 3. Phases 1–2 run in a Lima VM `reach-lab` on the Mac mini; phase 3 targets a 32 GB box.

**Tech Stack:** Rust 2024 (clap 4, bollard 0.18, axum 0.8, tokio), Docker CE rootless in Lima 2.2 (vz, arm64), Playwright Chromium, hermes-agent 0.20.x (Python), systemd user units.

**Spec:** `docs/superpowers/specs/2026-09-02-agent-computer-design.md`

## Global Constraints

- Rust 2024 edition; `cargo fmt --all` and `cargo clippy --workspace -- -D warnings` must pass before every commit.
- `rustfmt.toml` max_width = 100; `clippy.toml` too-many-arguments-threshold = 8.
- Conventional commit messages (`feat:`, `fix:`, `chore:`, `test:`, `docs:`, `refactor:`).
- Container-side Python helpers are embedded as `pub const` strings in `crates/reach-cli/src/docker.rs`.
- The image must build for `linux/amd64` (Google Chrome) and `linux/arm64` (Playwright Chromium).
- Mac mini budget: VM 2 vCPU / 4 GiB; container memory cap 2.5 GiB; shm 512 MiB.
- noVNC is exposed on the Tailscale IP `100.124.38.17` only.
- The live default hermes on the mini (`/Users/ariaserver/.hermes`, WhatsApp) is never modified.
- Do not commit or push unless asked; report `git status` at handoff (beads conservative profile).

Beads: every task below has a matching `bd` issue (see `bd list`). Claim with `bd update <id> --claim`, close with `bd close <id> --reason=...` when its commit lands.

---

## File structure

| Path | Responsibility |
|---|---|
| `crates/reach-cli/src/config.rs` | `server.public_host`, `sandbox.memory`, shm default, `default_workspace_dir()` |
| `crates/reach-cli/src/docker.rs` | `SandboxConfig` gains `memory`, `workspace`, `restart`; `build_mounts()`; `Labels::WORKSPACE`; `recreate()` |
| `crates/reach-cli/src/tools.rs` (new) | Single MCP tool dispatcher shared by `serve` and `connect` (`ToolContext`, `dispatch`) |
| `crates/reach-cli/src/mcp.rs` | `live_view` tool definition, `screen` param (phase 3) |
| `crates/reach-cli/src/commands/create.rs` | `--workspace`, `--memory`, `--no-restart`, `--screens` |
| `crates/reach-cli/src/commands/recreate.rs` (new) | Replace container, keep volumes |
| `crates/reach-cli/src/commands/serve.rs` | uses `tools::dispatch`; `--public-host`; agent API routes (phase 3) |
| `crates/reach-cli/src/commands/connect.rs` | uses `tools::dispatch` |
| `crates/reach-cli/src/agent.rs` (new, phase 3) | Screen lease/takeover state + axum routes |
| `crates/reach-supervisor/src/processes.rs` | multi-screen process table (phase 3), VNC password |
| `crates/reach-supervisor/src/health.rs` | `/screens` (phase 3) |
| `Dockerfile`, `scripts/reach-chrome` | multi-arch browser wrapper |
| `config/lima/reach-lab.yaml` | Lima VM definition |
| `scripts/lab/*.sh` | VM bring-up, image load, hermes setup |
| `integrations/hermes/config.snippet.yaml` | hermes `mcp_servers` + toolsets |
| `integrations/hermes/skills/agent-computer/SKILL.md` | the grokbot loop |
| `integrations/hermes/plugins/reach-agent-computer/` | phase 3 plugin |
| `integrations/systemd/*.service` | phase 2 user units |
| `docs/mcp-tools.md`, `docs/agent-computer.md`, `README.md`, `CLAUDE.md` | docs |

---

# Phase 1 — watch + takeover on the Mac mini

### Task 1: Config keys — `public_host`, `memory`, smaller shm, workspace dir

**Files:**
- Modify: `crates/reach-cli/src/config.rs`
- Test: `crates/reach-cli/tests/config_types.rs`

**Interfaces:**
- Produces: `ServerConfig.public_host: Option<String>`, `SandboxDefaults.memory: Option<u64>`, `SandboxDefaults.workspace_dir: Option<PathBuf>`, `SandboxDefaults::resolved_workspace_dir(&self) -> PathBuf`, `pub fn default_workspace_dir() -> PathBuf`, `ServerConfig::effective_public_host(&self) -> String` (falls back to `"localhost"`).

- [ ] **Step 1: Write the failing tests** (append to `crates/reach-cli/tests/config_types.rs`)

```rust
#[test]
fn default_shm_is_512mb() {
    let config = ReachConfig::default();
    assert_eq!(config.sandbox.shm_size, 512 * 1024 * 1024);
}

#[test]
fn public_host_defaults_to_localhost() {
    let config = ReachConfig::default();
    assert_eq!(config.server.public_host, None);
    assert_eq!(config.server.effective_public_host(), "localhost");
}

#[test]
fn public_host_and_memory_parse_from_toml() {
    let config: ReachConfig = toml::from_str(
        r#"
        [server]
        public_host = "100.124.38.17"
        [sandbox]
        memory = 2684354560
        workspace_dir = "/srv/reach/workspaces"
        "#,
    )
    .unwrap();
    assert_eq!(config.server.effective_public_host(), "100.124.38.17");
    assert_eq!(config.sandbox.memory, Some(2_684_354_560));
    assert_eq!(
        config.sandbox.resolved_workspace_dir(),
        std::path::PathBuf::from("/srv/reach/workspaces")
    );
}

#[test]
fn workspace_dir_falls_back_to_default() {
    let d = SandboxDefaults::default().resolved_workspace_dir();
    assert!(d.ends_with("reach/workspaces"));
}
```

Delete the old `default_shm_is_2gb` test in the same file (it now contradicts the spec).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p reach-cli --test config_types`
Expected: compile error, `no field public_host` / `no method resolved_workspace_dir`.

- [ ] **Step 3: Implement**

In `crates/reach-cli/src/config.rs`:

```rust
pub struct SandboxDefaults {
    // ...existing fields...
    /// Hard memory limit for the container in bytes (`None` = unlimited).
    #[serde(default)]
    pub memory: Option<u64>,
    /// Root directory for `/workspace` bind mounts on the host.
    #[serde(default)]
    pub workspace_dir: Option<PathBuf>,
}

impl SandboxDefaults {
    pub fn resolved_workspace_dir(&self) -> PathBuf {
        self.workspace_dir.clone().unwrap_or_else(default_workspace_dir)
    }
}

/// Platform default for `/workspace` bind mounts: `~/.local/share/reach/workspaces`.
pub fn default_workspace_dir() -> PathBuf {
    data_home().join("reach").join("workspaces")
}

fn data_home() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".local").join("share")
        })
}

pub struct ServerConfig {
    pub port: u16,
    pub host: String,
    /// Hostname or IP that humans use to open noVNC (e.g. a Tailscale IP).
    /// `None` = "localhost".
    #[serde(default)]
    pub public_host: Option<String>,
}

impl ServerConfig {
    pub fn effective_public_host(&self) -> String {
        self.public_host.clone().unwrap_or_else(|| "localhost".into())
    }
}
```

Refactor `default_profile_dir()` to use `data_home()` too. Change the `shm_size` default to `512 * 1024 * 1024` in `impl Default for SandboxDefaults`; set `memory: None, workspace_dir: None`; set `public_host: None` in `impl Default for ServerConfig`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p reach-cli --test config_types && cargo test -p reach-cli`
Expected: all pass (the docker_types test that asserts 2 GiB shm on `SandboxConfig::default()` may fail — update it to 512 MiB in Task 2).

- [ ] **Step 5: Commit**

```bash
git add crates/reach-cli/src/config.rs crates/reach-cli/tests/config_types.rs
git commit -m "feat(config): add server.public_host, sandbox.memory, workspace_dir; shm default 512MiB"
```

---

### Task 2: `SandboxConfig` — memory cap, `/workspace` mount, restart policy

**Files:**
- Modify: `crates/reach-cli/src/docker.rs:18-26` (struct), `:100-125` (Default), `:224-340` (`create`)
- Test: `crates/reach-cli/tests/docker_types.rs`

**Interfaces:**
- Produces: `SandboxConfig { memory: Option<u64>, workspace: Option<PathBuf>, restart_unless_stopped: bool, .. }`, `pub fn build_mounts(config: &SandboxConfig) -> Vec<Mount>`, `Labels::WORKSPACE = "reach.workspace"`, `WORKSPACE_CONTAINER_PATH = "/workspace"`.

- [ ] **Step 1: Write the failing tests** (append to `crates/reach-cli/tests/docker_types.rs`)

```rust
use reach_cli::docker::*;
use std::path::PathBuf;

#[test]
fn default_sandbox_config_has_no_workspace_and_512mb_shm() {
    let c = SandboxConfig::default();
    assert_eq!(c.shm_size, 512 * 1024 * 1024);
    assert!(c.workspace.is_none());
    assert_eq!(c.memory, None);
    assert!(c.restart_unless_stopped);
}

#[test]
fn build_mounts_includes_workspace_and_profile() {
    let c = SandboxConfig {
        workspace: Some(PathBuf::from("/tmp/ws")),
        profile: Some(ProfileMount {
            name: "default".into(),
            host_path: PathBuf::from("/tmp/prof"),
            container_path: ProfileMount::container_path_for("default"),
        }),
        ..SandboxConfig::default()
    };
    let mounts = build_mounts(&c);
    let targets: Vec<String> = mounts.iter().map(|m| m.target.clone().unwrap()).collect();
    assert!(targets.contains(&WORKSPACE_CONTAINER_PATH.to_string()));
    assert!(targets.contains(&"/home/sandbox/.config/google-chrome-profiles/default".to_string()));
}

#[test]
fn labels_include_workspace_when_set() {
    let c = SandboxConfig {
        workspace: Some(PathBuf::from("/tmp/ws")),
        ..SandboxConfig::default()
    };
    let l = Labels::for_sandbox(&c);
    assert_eq!(l.get(Labels::WORKSPACE).map(String::as_str), Some("/tmp/ws"));
}
```

If `docker_types.rs` already asserts `shm_size == 2 GiB`, change that assertion to 512 MiB.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p reach-cli --test docker_types`
Expected: compile errors on missing fields / `build_mounts`.

- [ ] **Step 3: Implement**

In `docker.rs`:

```rust
pub const WORKSPACE_CONTAINER_PATH: &str = "/workspace";

pub struct SandboxConfig {
    pub name: String,
    pub image: String,
    pub resolution: Resolution,
    pub shm_size: u64,
    pub ports: SandboxPorts,
    pub profile: Option<ProfileMount>,
    /// Host directory bind-mounted at `/workspace` (durable files).
    pub workspace: Option<PathBuf>,
    /// Hard memory cap in bytes.
    pub memory: Option<u64>,
    /// Docker restart policy `unless-stopped` (always-on Agent Computer).
    pub restart_unless_stopped: bool,
}
// Default: shm_size: 512 * 1024 * 1024, workspace: None, memory: None, restart_unless_stopped: true

impl Labels {
    pub const WORKSPACE: &str = "reach.workspace";
    // in for_sandbox(): if let Some(ws) = &config.workspace { m.insert(WORKSPACE, ws.to_string_lossy().into_owned()); }
}

fn bind(source: &std::path::Path, target: &str) -> Mount {
    Mount {
        target: Some(target.to_string()),
        source: Some(source.to_string_lossy().into_owned()),
        typ: Some(MountTypeEnum::BIND),
        read_only: Some(false),
        ..Default::default()
    }
}

/// Bind mounts for a sandbox: persistent Chrome profile and `/workspace`.
pub fn build_mounts(config: &SandboxConfig) -> Vec<Mount> {
    let mut v = Vec::new();
    if let Some(p) = &config.profile {
        v.push(bind(&p.host_path, &p.container_path));
    }
    if let Some(ws) = &config.workspace {
        v.push(bind(ws, WORKSPACE_CONTAINER_PATH));
    }
    v
}
```

In `create()`: replace the inline `mounts` block with

```rust
for dir in config.profile.iter().map(|p| &p.host_path).chain(config.workspace.iter()) {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create host dir {}", dir.display()))?;
}
let mounts = build_mounts(&config);
let host_config = HostConfig {
    port_bindings: Some(port_bindings),
    shm_size: Some(config.shm_size as i64),
    memory: config.memory.map(|m| m as i64),
    restart_policy: config.restart_unless_stopped.then(|| bollard::models::RestartPolicy {
        name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
        maximum_retry_count: None,
    }),
    mounts: if mounts.is_empty() { None } else { Some(mounts) },
    ..Default::default()
};
```

Also add `WORKSPACE_CONTAINER_PATH` to `env` as `REACH_WORKSPACE=/workspace` when set (the skill tells the model this path; the env makes `exec` scripts self-describing).

- [ ] **Step 4: Run tests, fmt, clippy**

Run: `cargo test -p reach-cli && cargo fmt --all && cargo clippy --workspace -- -D warnings`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/reach-cli/src/docker.rs crates/reach-cli/tests/docker_types.rs
git commit -m "feat(docker): workspace mount, memory cap, unless-stopped restart policy"
```

---

### Task 3: `reach create --workspace / --memory / --no-restart`

**Files:**
- Modify: `crates/reach-cli/src/commands/create.rs`
- Test: `crates/reach-cli/src/commands/create.rs` (unit tests for `parse_memory`)

**Interfaces:**
- Consumes: Task 1 config, Task 2 `SandboxConfig` fields.
- Produces: `pub fn parse_memory(s: &str) -> Result<u64, String>` accepting `2g`, `512m`, `2684354560`.

- [ ] **Step 1: Write failing tests** (bottom of `create.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::parse_memory;

    #[test]
    fn parses_suffixes() {
        assert_eq!(parse_memory("512m").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory("2g").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory("2.5G").unwrap(), 2_684_354_560);
        assert_eq!(parse_memory("1024").unwrap(), 1024);
        assert!(parse_memory("lots").is_err());
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p reach-cli parses_suffixes`
Expected: `cannot find function parse_memory`.

- [ ] **Step 3: Implement**

```rust
/// Parse `512m`, `2g`, `2.5G`, or raw bytes.
pub fn parse_memory(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('k' | 'K') => (&s[..s.len() - 1], 1024f64),
        Some('m' | 'M') => (&s[..s.len() - 1], 1024f64 * 1024.0),
        Some('g' | 'G') => (&s[..s.len() - 1], 1024f64 * 1024.0 * 1024.0),
        _ => (s, 1.0),
    };
    let n: f64 = num.parse().map_err(|_| format!("invalid memory size {s:?}"))?;
    Ok((n * mult) as u64)
}
```

Add to `CreateArgs`:

```rust
/// Mount a host directory at /workspace (durable files). Without a value,
/// uses `<workspace_dir>/<name>` (default ~/.local/share/reach/workspaces/<name>).
#[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "")]
pub workspace: Option<String>,

/// Hard memory cap, e.g. `2.5g` (default: sandbox.memory from config, else unlimited)
#[arg(long, value_parser = parse_memory)]
pub memory: Option<u64>,

/// Do not set the `unless-stopped` restart policy
#[arg(long)]
pub no_restart: bool,
```

In `run()`:

```rust
let workspace = args.workspace.as_ref().map(|w| {
    if w.is_empty() {
        cfg.sandbox.resolved_workspace_dir().join(&args.name)
    } else {
        std::path::PathBuf::from(w)
    }
});
let config = SandboxConfig {
    // ...existing...
    workspace: workspace.clone(),
    memory: args.memory.or(cfg.sandbox.memory),
    restart_unless_stopped: !args.no_restart,
};
```

Print a `Workspace` line after the `Profile` line when set (same style as Profile).

- [ ] **Step 4: Verify**

Run: `cargo test -p reach-cli && cargo build -p reach-cli && ./target/debug/reach create --help | grep -E "workspace|memory|no-restart"`
Expected: tests pass; three flags listed.

- [ ] **Step 5: Commit**

```bash
git add crates/reach-cli/src/commands/create.rs
git commit -m "feat(create): --workspace, --memory, --no-restart"
```

---

### Task 4: Multi-arch image — `reach-chrome` wrapper, arm64 build target

**Files:**
- Create: `scripts/reach-chrome`
- Modify: `Dockerfile`, `Makefile`, `.github/workflows/ci.yml`
- Test: `crates/reach-cli/tests/e2e_container.rs` (new test `t90_reach_chrome_resolves`)

**Interfaces:**
- Produces: `/usr/local/bin/reach-chrome` inside the image (execs Google Chrome on amd64, Playwright Chromium on arm64); `PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright`; `make image-arm64`, `make lab-load`.

- [ ] **Step 1: Write the failing e2e test** (append to `e2e_container.rs`)

```rust
#[test]
#[ignore]
fn t90_reach_chrome_resolves() {
    ensure_container();
    let out = sh_ok("reach-chrome --version");
    assert!(
        out.contains("Chrom"),
        "reach-chrome should report a Chrome/Chromium version, got: {out}"
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `make image && cargo test -p reach-cli --test e2e_container t90 -- --ignored`
Expected: FAIL, `reach-chrome: command not found`.

- [ ] **Step 3: Add the wrapper**

`scripts/reach-chrome`:

```bash
#!/usr/bin/env bash
# Resolve a headed Chromium-family browser for this architecture.
# amd64: Google Chrome (deb). arm64: Playwright's Chromium (no Chrome deb exists).
set -euo pipefail
for c in /usr/bin/google-chrome-stable \
         "${PLAYWRIGHT_BROWSERS_PATH:-/opt/ms-playwright}"/chromium-*/chrome-linux*/chrome; do
  if [ -x "$c" ]; then exec "$c" "$@"; fi
done
echo "reach-chrome: no browser found" >&2
exit 127
```

- [ ] **Step 4: Make the Dockerfile arch-aware**

Replace Layer 2 with:

```dockerfile
# Layer 2: Google Chrome (amd64 only — there is no arm64 Linux Chrome build).
ARG TARGETARCH
RUN if [ "$TARGETARCH" = "amd64" ]; then \
      curl -fsSL https://dl.google.com/linux/linux_signing_key.pub \
        | gpg --dearmor -o /usr/share/keyrings/google-chrome.gpg \
      && echo "deb [arch=amd64 signed-by=/usr/share/keyrings/google-chrome.gpg] http://dl.google.com/linux/chrome/deb/ stable main" \
        > /etc/apt/sources.list.d/google-chrome.list \
      && apt-get update && apt-get install -y --no-install-recommends google-chrome-stable \
      && rm -rf /var/lib/apt/lists/*; \
    fi
```

Before Layer 4 add `ENV PLAYWRIGHT_BROWSERS_PATH=/opt/ms-playwright`, and in Layer 4 append `&& playwright install-deps chromium` before `playwright install chromium` (arm64 needs the shared libs). Remove the trailing `|| true` so a broken install fails the build. After Layer 6 add:

```dockerfile
COPY scripts/reach-chrome /usr/local/bin/reach-chrome
RUN chmod +x /usr/local/bin/reach-chrome \
    && mkdir -p /etc/chromium/policies/managed \
    && chmod -R a+rX /opt/ms-playwright
COPY config/chrome-policies.json /etc/chromium/policies/managed/reach.json
```

Keep the existing `COPY config/chrome-policies.json /etc/opt/chrome/policies/managed/reach.json`.

- [ ] **Step 5: Makefile targets**

```make
image-arm64:
	docker buildx build --platform linux/arm64 -t reach:arm64 --load .

# Load the arm64 image into the reach-lab Lima VM's rootless Docker.
lab-load: image-arm64
	docker save reach:arm64 | limactl shell reach-lab docker load
	limactl shell reach-lab docker tag reach:arm64 reach:latest
```

- [ ] **Step 6: CI arm64 build job** (add to `ci.yml` after `docker-build`)

```yaml
  docker-build-arm64:
    name: Docker Build (arm64)
    runs-on: ubuntu-latest
    needs: [unit-tests]
    steps:
      - uses: actions/checkout@v4
      - uses: docker/setup-qemu-action@v3
      - uses: docker/setup-buildx-action@v3
      - name: Build arm64 image (no load, build-only)
        uses: docker/build-push-action@v6
        with:
          context: .
          platforms: linux/arm64
          tags: reach:ci-arm64
          cache-from: type=gha,scope=arm64
          cache-to: type=gha,mode=max,scope=arm64
```

- [ ] **Step 7: Verify both arches**

Run: `make image && cargo test -p reach-cli --test e2e_container -- --ignored --test-threads=1` (all e2e still pass on amd64/your Mac's Docker; on an arm64 Mac `make image` already builds arm64 and `t90` must print `Chromium`).
Expected: `t90` passes; existing tests unchanged.

- [ ] **Step 8: Commit**

```bash
git add scripts/reach-chrome Dockerfile Makefile .github/workflows/ci.yml crates/reach-cli/tests/e2e_container.rs
git commit -m "feat(image): multi-arch browser via reach-chrome wrapper; arm64 build targets"
```

---

### Task 5: Extract one tool dispatcher (`tools.rs`) used by `serve` and `connect`

`connect.rs` and `serve.rs` each carry a full copy of the tool match. Later tasks add `live_view`, public URLs, and `screen`; doing it twice is how the two drift. Extract first.

**Files:**
- Create: `crates/reach-cli/src/tools.rs`
- Modify: `crates/reach-cli/src/lib.rs`, `crates/reach-cli/src/commands/serve.rs`, `crates/reach-cli/src/commands/connect.rs`
- Test: `crates/reach-cli/src/tools.rs` unit tests (pure helpers)

**Interfaces:**
- Produces:

```rust
pub struct ToolContext<'a> { pub docker: &'a DockerClient, pub public_host: String }
pub async fn dispatch(ctx: &ToolContext<'_>, tool: &str, args: &serde_json::Value, target: &str) -> ToolResponse;
pub fn browse_command(url: &str, profile_dir: &str) -> String;
pub fn novnc_url_for(ctx: &ToolContext<'_>, sandbox: &crate::docker::Sandbox) -> String;
```

- [ ] **Step 1: Write failing unit tests** (in `tools.rs` `#[cfg(test)]`)

```rust
#[test]
fn browse_command_uses_wrapper_and_profile() {
    let cmd = browse_command("https://ex.com/a?b='c'", "/home/sandbox/.config/google-chrome-profiles/default");
    assert!(cmd.starts_with("reach-chrome "));
    assert!(cmd.contains("--user-data-dir=/home/sandbox/.config/google-chrome-profiles/default"));
    assert!(cmd.contains("%27c%27"));
    assert!(cmd.ends_with(" &"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p reach-cli browse_command_uses_wrapper`
Expected: module not found.

- [ ] **Step 3: Implement `tools.rs`**

Move the body of `serve.rs::dispatch` (and its `sh`/`py` helpers) into `tools.rs`, parameterised by `ToolContext`. `browse` becomes:

```rust
pub fn browse_command(url: &str, profile_dir: &str) -> String {
    format!(
        "mkdir -p '{p}' && reach-chrome --no-sandbox --disable-gpu --no-first-run \
         --user-data-dir='{p}' '{u}' >/dev/null 2>&1 &",
        p = profile_dir,
        u = url.replace('\'', "%27")
    )
}
// in dispatch:
"browse" => {
    let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("about:blank");
    let profile = args.get("use_profile").and_then(|v| v.as_str()).unwrap_or("default");
    sh(ctx, target, &browse_command(url, &ProfileMount::container_path_for(profile))).await
}
```

`auth_handoff` computes its URL with `novnc_url_for(ctx, &sandbox)` = `novnc_url(&ctx.public_host, port)`. Add `pub mod tools;` to `lib.rs`. Replace `serve.rs::dispatch` and `connect.rs::dispatch_tool` with calls to `reach_cli::tools::dispatch`; `AppState` gains `public_host: String`; `connect` builds `ToolContext { docker: &docker, public_host: ReachConfig::load().server.effective_public_host() }`. Delete the duplicated helpers from both command files.

- [ ] **Step 4: Verify**

Run: `cargo test -p reach-cli && cargo clippy --workspace -- -D warnings && cargo test -p reach-cli --test e2e_container -- --ignored --test-threads=1`
Expected: pass; `connect.rs` and `serve.rs` no longer contain a `"screenshot" =>` arm.

- [ ] **Step 5: Commit**

```bash
git add crates/reach-cli/src/tools.rs crates/reach-cli/src/lib.rs crates/reach-cli/src/commands/serve.rs crates/reach-cli/src/commands/connect.rs
git commit -m "refactor(tools): single MCP dispatcher shared by serve and connect; browse uses persistent profile"
```

---

### Task 6: `live_view` tool and `--public-host`

**Files:**
- Modify: `crates/reach-cli/src/mcp.rs` (tool def), `crates/reach-cli/src/tools.rs` (dispatch arm), `crates/reach-cli/src/commands/serve.rs` (`--public-host` flag)
- Test: `crates/reach-cli/tests/mcp_types.rs`, `crates/reach-cli/tests/e2e_container.rs`

**Interfaces:**
- Produces: MCP tool `live_view { sandbox?, screen? }` → text JSON `{ "novnc_url": String, "screen": 0, "display": ":99", "busy": bool }`. `busy` = `xdotool getactivewindow` succeeds (a window is focused). `ServeArgs.public_host: Option<String>` overrides config.

- [ ] **Step 1: Failing tests**

`mcp_types.rs`:

```rust
#[test]
fn live_view_tool_is_registered_and_has_no_required_fields() {
    let tool = tool_definitions().into_iter().find(|t| t.name == "live_view").unwrap();
    assert!(tool.input_schema.get("required").is_none());
}
```

Update `all_tools_are_registered` to expect 11 tools including `live_view`.

`e2e_container.rs` (this test drives `reach serve` against the shared container; add helper `mcp_call(port, name, args)` using `curl -s -X POST http://127.0.0.1:PORT/mcp -d '{jsonrpc...}'`):

```rust
#[test]
#[ignore]
fn t91_live_view_uses_public_host() {
    ensure_container();
    let serve = std::process::Command::new(env!("CARGO_BIN_EXE_reach"))
        .args(["serve", "--port", "14200", "--sandbox", CONTAINER, "--public-host", "100.64.0.9"])
        .spawn()
        .unwrap();
    sleep_ms(800);
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"live_view","arguments":{}}}"#;
    let out = std::process::Command::new("curl")
        .args(["-s", "-X", "POST", "-H", "content-type: application/json", "-d", body, "http://127.0.0.1:14200/mcp"])
        .output().unwrap();
    let _ = std::process::Command::new("kill").arg(serve.id().to_string()).status();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("http://100.64.0.9:16080/vnc.html"), "got {text}");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p reach-cli --test mcp_types live_view`
Expected: `unwrap` on `None`.

- [ ] **Step 3: Implement**

`mcp.rs` — append to `tool_definitions()`:

```rust
ToolDefinition {
    name: "live_view".into(),
    description: "Return the noVNC URL where a human can watch (and take over) this sandbox's \
                  screen. Call once per task and share the URL with the user.".into(),
    input_schema: serde_json::json!({
        "type": "object",
        "properties": {
            "sandbox": { "type": "string" },
            "screen": { "type": "integer", "default": 0 }
        }
    }),
},
```

`tools.rs`:

```rust
"live_view" => match ctx.docker.find(target).await {
    Ok(sb) => {
        let busy = ctx.docker.exec(target, &["xdotool".into(), "getactivewindow".into()]).await
            .map(|o| o.exit_code == 0).unwrap_or(false);
        ToolResponse::text(serde_json::json!({
            "novnc_url": novnc_url_for(ctx, &sb),
            "screen": 0, "display": ":99", "busy": busy,
        }).to_string())
    }
    Err(e) => ToolResponse::error(e.to_string()),
},
```

`serve.rs`: `#[arg(long)] pub public_host: Option<String>`; `public_host: args.public_host.unwrap_or_else(|| ReachConfig::load().server.effective_public_host())`; print `Live view host: {public_host}` at startup.

- [ ] **Step 4: Verify**

Run: `cargo test -p reach-cli && cargo test -p reach-cli --test e2e_container t91 -- --ignored`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/reach-cli/src/mcp.rs crates/reach-cli/src/tools.rs crates/reach-cli/src/commands/serve.rs crates/reach-cli/tests/mcp_types.rs crates/reach-cli/tests/e2e_container.rs
git commit -m "feat(mcp): live_view tool; --public-host for human-facing noVNC URLs"
```

---

### Task 7: e2e — workspace mount and shared cookies between `browse` and `page_text`

**Files:**
- Test: `crates/reach-cli/tests/e2e_container.rs`
- Modify: `crates/reach-cli/src/docker.rs` (`PAGE_TEXT_SCRIPT` default `user_data_dir`)

**Interfaces:**
- Consumes: Task 2 `WORKSPACE_CONTAINER_PATH`, Task 5 `browse_command`.
- Produces: `page_text` and `auth_handoff` default `user_data_dir` = `ProfileMount::container_path_for("default")` (was `_reach_default`), so all three browser paths share one profile without `use_profile`.

- [ ] **Step 1: Failing tests**

Change `ensure_container()` to add `"-v", "/tmp/reach-e2e-ws:/workspace"` and `"--memory=2560m"` to `docker run`. Append:

```rust
#[test]
#[ignore]
fn t92_workspace_is_mounted() {
    ensure_container();
    sh_ok("echo hello > /workspace/probe.txt");
    assert_eq!(std::fs::read_to_string("/tmp/reach-e2e-ws/probe.txt").unwrap().trim(), "hello");
}

#[test]
#[ignore]
fn t93_browse_and_page_text_share_profile() {
    ensure_container();
    // Serve a page that sets a cookie, then echoes it.
    sh_ok(r#"cat > /tmp/cookie.py <<'EOF'
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        c = self.headers.get('Cookie', '')
        self.send_response(200)
        self.send_header('Set-Cookie', 'reach=yes; Path=/')
        self.send_header('Content-Type', 'text/html')
        self.end_headers()
        self.wfile.write(('<body>cookie=[%s]</body>' % c).encode())
HTTPServer(('127.0.0.1', 8765), H).serve_forever()
EOF
nohup python3 /tmp/cookie.py >/dev/null 2>&1 &"#);
    sleep_ms(500);
    let profile = "/home/sandbox/.config/google-chrome-profiles/default";
    // Headed browse sets the cookie, then we close the browser so the profile unlocks.
    sh_ok(&reach_cli::tools::browse_command("http://127.0.0.1:8765/", profile));
    sleep_ms(4000);
    sh_ok("pkill -f user-data-dir= || true");
    sleep_ms(1000);
    let out = sh_ok(&format!(
        "REACH_PAGE_TEXT_PAYLOAD='{{\"url\":\"http://127.0.0.1:8765/\",\"user_data_dir\":\"{profile}\",\"timeout_ms\":15000}}' \
         python3 -c \"$(cat <<'EOF'\n{}\nEOF\n)\"",
        reach_cli::docker::PAGE_TEXT_SCRIPT
    ));
    assert!(out.contains("reach=yes"), "cookie not shared: {out}");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p reach-cli --test e2e_container t9 -- --ignored --test-threads=1`
Expected: `t92` fails (no mount yet in `ensure_container`) then passes after the edit; `t93` fails because the headed browser used a throwaway profile before Task 5 — with Task 5 applied it should pass; if it fails on the `_reach_default` mismatch, Step 3 fixes it.

- [ ] **Step 3: Align default profile names**

In `docker.rs`, `PAGE_TEXT_SCRIPT` and `AUTH_HANDOFF_SCRIPT`: replace `"/home/sandbox/.config/google-chrome-profiles/_reach_default"` with `"/home/sandbox/.config/google-chrome-profiles/default"`. In `tools.rs`, when `use_profile` is absent, pass `Some(ProfileMount::container_path_for("default"))` for `page_text`/`auth_handoff` too.

- [ ] **Step 4: Verify**

Run: `cargo test -p reach-cli --test e2e_container -- --ignored --test-threads=1`
Expected: all e2e pass.

- [ ] **Step 5: Commit**

```bash
git add crates/reach-cli/tests/e2e_container.rs crates/reach-cli/src/docker.rs crates/reach-cli/src/tools.rs
git commit -m "test(e2e): workspace mount and shared browser profile across browse/page_text"
```

---

### Task 8: Docs for phase 1 reach changes

**Files:**
- Modify: `docs/mcp-tools.md` (add `live_view`, `use_profile` on `browse`), `README.md` (11 tools, new flags), `CLAUDE.md` (tools.rs pointer, workspace/memory flags)
- Create: `docs/agent-computer.md` (overview: what it is, ports, URLs, phase status)

- [ ] **Step 1: Write docs** — `docs/agent-computer.md`:

```markdown
# Agent Computer

reach as a persistent, watchable desktop for an AI teammate (hermes).

| Piece | Where |
|---|---|
| Live view | `http://<public_host>:<novnc_port>/vnc.html?autoconnect=1&resize=remote` (`live_view` tool) |
| Takeover | `auth_handoff` returns `auth_required` + the live view URL; the human logs in there |
| Durable files | `/workspace` (`reach create --workspace`) |
| Browser sessions | profile `default` shared by `browse`, `page_text`, `auth_handoff` |
| Memory cap | `reach create --memory 2.5g` |
| Always-on | restart policy `unless-stopped` (disable with `--no-restart`) |

Set `server.public_host` in `~/.config/reach/config.toml` (or `reach serve --public-host`) to the
address humans reach the host on, e.g. a Tailscale IP.

See `integrations/hermes/` for the hermes side.
```

- [ ] **Step 2: Update the tool table in `README.md` and `docs/mcp-tools.md`; add to `CLAUDE.md` under "Working on This Repo": `10. All MCP tools are dispatched from crates/reach-cli/src/tools.rs; serve and connect are transports only.`

- [ ] **Step 3: Commit**

```bash
git add docs README.md CLAUDE.md
git commit -m "docs: agent computer overview, live_view, workspace/memory flags"
```

---

### Task 9: Lima VM `reach-lab` definition and bring-up script

**Files:**
- Create: `config/lima/reach-lab.yaml`, `scripts/lab/up.sh`, `scripts/lab/README.md`

**Interfaces:**
- Produces: a running Lima instance `reach-lab` with rootless Docker CE, cargo, and the reach CLI built; guest 6080 forwarded to host `100.124.38.17:6080`.

- [ ] **Step 1: Write `config/lima/reach-lab.yaml`**

```yaml
# reach Agent Computer lab VM. Start: limactl start --name=reach-lab config/lima/reach-lab.yaml
minimumLimaVersion: 2.0.0
base:
  - template:docker          # rootless Docker CE, Ubuntu LTS
vmType: vz
arch: aarch64
cpus: 2
memory: 4GiB
disk: 30GiB
mounts:
  - location: "~/.local/share/reach-lab"   # host dir for /workspace + profiles
    writable: true
    mountPoint: /srv/reach
provision:
  - mode: system
    script: |
      #!/bin/bash
      set -eux
      apt-get update
      apt-get install -y build-essential pkg-config libssl-dev curl git xdotool
      loginctl enable-linger "$(id -un 1000)" || true
  - mode: user
    script: |
      #!/bin/bash
      set -eux
      command -v cargo >/dev/null || curl -sSf https://sh.rustup.rs | sh -s -- -y
portForwards:
  - guestPort: 6080
    hostIP: "100.124.38.17"     # Tailscale only
    hostPort: 6080
  - guestPortRange: [1, 65535]
    ignore: true                # nothing else leaves the VM
```

- [ ] **Step 2: Write `scripts/lab/up.sh`**

```bash
#!/usr/bin/env bash
# Bring up reach-lab on the Mac mini, install reach CLI + image, start the Agent Computer.
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH=/opt/homebrew/bin:$PATH
limactl list -q | grep -qx reach-lab || limactl start --name=reach-lab --tty=false config/lima/reach-lab.yaml
limactl shell reach-lab bash -lc 'mkdir -p ~/src && rsync -a --delete --exclude target /Users/'"$USER"'/repos/reach/ ~/src/reach/ && cd ~/src/reach && cargo install --path crates/reach-cli --locked'
make lab-load
limactl shell reach-lab bash -lc '
  mkdir -p ~/.config/reach /srv/reach/workspaces /srv/reach/profiles
  cat > ~/.config/reach/config.toml <<EOF
[server]
public_host = "100.124.38.17"
[sandbox]
memory = 2684354560
workspace_dir = "/srv/reach/workspaces"
profile_dir = "/srv/reach/profiles"
EOF
  reach list | grep -q agent-computer || reach create --name agent-computer --workspace --persist-profile default --memory 2.5g
'
echo "Live view: http://100.124.38.17:6080/vnc.html?autoconnect=1&resize=remote"
```

Note for the mini: the repo must be present at `/Users/ariaserver/repos/reach` (clone it there; `rsync` from the host mount is the simplest copy since Lima mounts `~`).

- [ ] **Step 3: Verify on the mini**

Run (from your Mac): `ssh ariaserver@100.124.38.17 'cd repos/reach && scripts/lab/up.sh'`
Then: `curl -sf http://100.124.38.17:6080/ | head -c 100` → noVNC HTML. From inside: `limactl shell reach-lab reach list` → `agent-computer  running`.
Expected: both succeed; `free -m` inside the VM shows > 800 MiB available after Chromium opens one page.

- [ ] **Step 4: Commit**

```bash
git add config/lima/reach-lab.yaml scripts/lab
git commit -m "chore(lab): Lima reach-lab VM definition and bring-up script"
```

---

### Task 10: hermes in the lab — config snippet, toolsets, agent-computer skill

**Files:**
- Create: `integrations/hermes/config.snippet.yaml`, `integrations/hermes/skills/agent-computer/SKILL.md`, `scripts/lab/hermes-setup.sh`

**Interfaces:**
- Consumes: `reach serve` on `127.0.0.1:4200` (Task 6), tools `live_view`, `page_text`, `auth_handoff`, `browse`, `screenshot`, `exec`.

- [ ] **Step 1: Config snippet** — `integrations/hermes/config.snippet.yaml`

```yaml
# Merge into ~/.hermes/config.yaml inside reach-lab.
mcp_servers:
  reach:
    url: http://127.0.0.1:4200/mcp
    transport: sse
# One browser only: the Agent Computer. Drop hermes's own browser + host desktop control.
platform_toolsets:
  cli: [web, terminal, file, skills, todo, cronjob, memory, vision, delegate, mcp]
terminal:
  backend: local
  cwd: /srv/reach/workspaces/agent-computer
```

- [ ] **Step 2: Skill** — `integrations/hermes/skills/agent-computer/SKILL.md`

```markdown
---
name: agent-computer
description: Use the reach Agent Computer (MCP server "reach") for anything that needs a browser or a desktop — reading web pages, logging into sites, filling forms, downloading files. Always share the live view link and hand logins to the human.
---

# Agent Computer

You have a persistent Linux desktop with Chromium. The human can watch it live and take over.

## Loop
1. **Announce the screen.** At the start of any browser/desktop task call `live_view` once and tell the user: `Watch here: <novnc_url>`.
2. **Read with text first.** Use `page_text` (JS-rendered) or `scrape` (static). Use `screenshot` only to confirm layout or when text extraction fails. Use `browse` to open a page for the human to see.
3. **Logins are the human's job.** If a page shows a login, 2FA, CAPTCHA, or "verify it's you":
   - call `auth_handoff` with the URL (profile `default`),
   - tell the user: `Take over at <vnc_url>, log in, then tell me "done"`,
   - when they say done, call `auth_handoff` again with `wait_for_url_contains` or `wait_for_selector` describing the logged-in page, then continue.
   - Never ask for a password or code in chat. Never type one.
4. **Durable output goes to /workspace.** Write results under `/workspace/<task-slug>/` (via `exec` or the terminal; `/srv/reach/workspaces/agent-computer` is the same directory on this VM). Everything else is disposable.
5. **Ask before acting outward.** Sending, purchasing, deleting, publishing, or changing a production system needs an explicit yes from the user first.
6. **Report.** End with what was done, what landed where, and the live view link if the screen is left in a state worth looking at.

## Notes
- Sessions persist: once logged in, you usually stay logged in across tasks.
- If a tool returns "no running sandbox" or "unhealthy", stop and tell the user to run `reach recreate agent-computer`.
```

- [ ] **Step 3: Setup script** — `scripts/lab/hermes-setup.sh`

```bash
#!/usr/bin/env bash
# Install hermes inside reach-lab with its own HERMES_HOME and wire it to reach.
set -euo pipefail
cd "$(dirname "$0")/../.."
export PATH=/opt/homebrew/bin:$PATH
limactl shell reach-lab bash -lc '
  set -eux
  command -v hermes >/dev/null || curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash
  mkdir -p ~/.hermes/skills/agent-computer
  cp ~/src/reach/integrations/hermes/skills/agent-computer/SKILL.md ~/.hermes/skills/agent-computer/
  python3 - <<"EOF"
import yaml, pathlib
cfg = pathlib.Path.home()/".hermes/config.yaml"
base = yaml.safe_load(cfg.read_text()) if cfg.exists() else {}
snip = yaml.safe_load(open(pathlib.Path.home()/"src/reach/integrations/hermes/config.snippet.yaml"))
base.update(snip)
cfg.write_text(yaml.safe_dump(base, sort_keys=False))
EOF
  echo "Now run: limactl shell reach-lab hermes auth   # pick openai-codex / zai / openrouter"
'
```

- [ ] **Step 4: Verify integration in the VM**

1. `limactl shell reach-lab hermes auth` (interactive, pick the hosted provider) and `hermes model` to select it.
2. Start `reach serve` in a tmux: `limactl shell reach-lab bash -lc 'reach serve --sandbox agent-computer'`.
3. `limactl shell reach-lab hermes chat -q "Use page_text to read https://example.com and give me the heading."`
   Expected: response contains "Example Domain".
4. `limactl shell reach-lab hermes chat -q "Log me into https://github.com/login using the agent computer, then read my profile name."`
   Expected: reply includes `Watch here: http://100.124.38.17:6080/...` and `Take over at ...`; you log in on the noVNC page; say "done"; hermes reads the page.

Record outcomes (including whether `screenshot` images reach the model) in the beads issue.

- [ ] **Step 5: Commit**

```bash
git add integrations/hermes scripts/lab/hermes-setup.sh
git commit -m "feat(hermes): config snippet and agent-computer skill for the reach lab"
```

---

# Phase 2 — always-on and routines

### Task 11: systemd user units for `reach serve` and `hermes gateway`

**Files:**
- Create: `integrations/systemd/reach-serve.service`, `integrations/systemd/hermes-gateway.service`, `scripts/lab/install-units.sh`

- [ ] **Step 1: Units**

`reach-serve.service`:

```ini
[Unit]
Description=reach MCP server (Agent Computer)
After=default.target

[Service]
ExecStart=%h/.cargo/bin/reach serve --sandbox agent-computer
Restart=always
RestartSec=3
Environment=DOCKER_HOST=unix:///run/user/%U/docker.sock

[Install]
WantedBy=default.target
```

`hermes-gateway.service`:

```ini
[Unit]
Description=hermes gateway (cron + platforms)
After=reach-serve.service

[Service]
ExecStart=%h/.local/bin/hermes gateway
Restart=always
RestartSec=5
WatchdogSec=120

[Install]
WantedBy=default.target
```

`install-units.sh` copies both into `~/.config/systemd/user/`, runs `systemctl --user daemon-reload && systemctl --user enable --now reach-serve hermes-gateway`, and `loginctl enable-linger $USER`. Add `gateway: { systemd_watchdog_seconds: 120 }` to the hermes config snippet.

- [ ] **Step 2: Verify** — `limactl stop reach-lab && limactl start reach-lab`; then `limactl shell reach-lab systemctl --user is-active reach-serve hermes-gateway` → `active active`; `reach list` shows `agent-computer running`; `curl -sf http://100.124.38.17:6080/` works.

- [ ] **Step 3: Mini autostart** — add `scripts/lab/launchd/ai.reach.lab.plist` that runs `/opt/homebrew/bin/limactl start reach-lab` at login, with `RunAtLoad`. Install with `launchctl load ~/Library/LaunchAgents/ai.reach.lab.plist`.

- [ ] **Step 4: Commit**

```bash
git add integrations/systemd scripts/lab
git commit -m "chore(lab): systemd user units and launchd autostart for the always-on lab"
```

---

### Task 12: `reach recreate` (Grok's "recover": new container, same volumes)

**Files:**
- Create: `crates/reach-cli/src/commands/recreate.rs`
- Modify: `crates/reach-cli/src/commands/mod.rs`, `crates/reach-cli/src/docker.rs` (`inspect_config`)
- Test: `crates/reach-cli/tests/e2e_container.rs`

**Interfaces:**
- Produces: `DockerClient::inspect_config(&self, target) -> Result<SandboxConfig>` (rebuilds a `SandboxConfig` from labels + inspect: name, image, resolution label, ports, `reach.profile` → profile mount, `reach.workspace` → workspace, memory from HostConfig), `DockerClient::recreate(&self, target, image: Option<String>) -> Result<Sandbox>` = inspect → destroy → create.

- [ ] **Step 1: Failing e2e test**

```rust
#[test]
#[ignore]
fn t94_recreate_keeps_workspace() {
    ensure_container();
    sh_ok("echo keep > /workspace/keep.txt");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_reach"))
        .args(["recreate", CONTAINER]).output().unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(wait_for_health(30));
    assert_eq!(sh_ok("cat /workspace/keep.txt"), "keep");
}
```

(`ensure_container` must now start the e2e container via `reach create --name reach-e2e --workspace /tmp/reach-e2e-ws --vnc-port 15900 --novnc-port 16080 --health-port 18400` so labels exist; keep the `docker run` fallback out.)

- [ ] **Step 2: Run to verify failure** — `unknown subcommand recreate`.

- [ ] **Step 3: Implement** — `recreate.rs`:

```rust
#[derive(Args)]
pub struct RecreateArgs {
    pub target: String,
    /// Replace the image (e.g. after `make lab-load`)
    #[arg(long)]
    pub image: Option<String>,
}

pub async fn run(args: RecreateArgs) -> anyhow::Result<()> {
    let docker = DockerClient::new()?;
    let sb = docker.recreate(&args.target, args.image).await?;
    docker.wait_healthy(&sb.name, std::time::Duration::from_secs(45)).await?;
    println!("recreated {} ({})", sb.name, &sb.container_id[..12]);
    Ok(())
}
```

`docker.rs::inspect_config` reads `inspect_container`, then labels (`Labels::RESOLUTION`, `PROFILE`, `WORKSPACE`), `host_config.memory`, and `host_config.port_bindings` for 5900/6080/8400 host ports. Add `Labels::PROFILE_HOST = "reach.profile_host"` written at create so the profile host path survives (today only the name is stored). Add the `Recreate` variant to `commands/mod.rs`.

- [ ] **Step 4: Verify** — `cargo test -p reach-cli --test e2e_container t94 -- --ignored`; clippy clean.

- [ ] **Step 5: Commit** — `git commit -m "feat(cli): reach recreate keeps workspace and profile volumes"`.

---

### Task 13: VNC password

**Files:**
- Modify: `crates/reach-supervisor/src/processes.rs` (x11vnc args), `crates/reach-cli/src/docker.rs` (`vnc_password: Option<String>` → env `VNC_PASSWORD`), `crates/reach-cli/src/commands/create.rs` (`--vnc-password`)
- Test: `crates/reach-supervisor/src/processes.rs` unit test on the process table

- [ ] **Step 1: Failing test** (supervisor)

```rust
#[test]
fn x11vnc_uses_password_when_env_set() {
    let sup = Supervisor::new(99, 1280, 720).with_vnc_password(Some("s3cret".into()));
    let x = sup.process_table().into_iter().find(|p| p.name == "x11vnc").unwrap();
    assert!(x.args.windows(2).any(|w| w == ["-passwd", "s3cret"]));
    assert!(!x.args.iter().any(|a| a == "-nopw"));
}
```

- [ ] **Step 2: Implement** — `Supervisor` gains `vnc_password: Option<String>` read from `VNC_PASSWORD` in `main.rs`; process table emits `-passwd <pw>` instead of `-nopw` when set. `SandboxConfig.vnc_password` → container env. `create --vnc-password <PW>` (also `sandbox.vnc_password` in config). The noVNC page prompts for it.

- [ ] **Step 3: Verify** — `cargo test --workspace`; e2e with `--vnc-password x`: `curl` to noVNC still 200; the `live_view` output unchanged.

- [ ] **Step 4: Commit** — `git commit -m "feat(vnc): optional VNC password via --vnc-password / VNC_PASSWORD"`.

---

### Task 14: Routines — cron job template and report convention

**Files:**
- Create: `integrations/hermes/routines/README.md`, `integrations/hermes/routines/daily-check.sh`
- Modify: `integrations/hermes/skills/agent-computer/SKILL.md` (reports section)

- [ ] **Step 1: Write** — `daily-check.sh`:

```bash
#!/usr/bin/env bash
# Example routine: every morning, read a dashboard and file a report under /workspace/reports.
hermes cron create "every day at 08:00" \
  "Use the agent-computer skill. Open https://example.com/dashboard with page_text (use profile default). \
   If a login is required, stop and tell me to take over. Otherwise write a 10-line summary to \
   /workspace/reports/\$(date +%F)-dashboard.md and reply with its path." \
  --skill agent-computer --deliver origin
```

Skill addendum: "Routine runs must never wait on a takeover longer than 60 s; report `auth_required` and exit."

- [ ] **Step 2: Verify** — create the job in the VM, `hermes cron trigger <id>`, confirm a file lands in `/srv/reach/workspaces/agent-computer/reports/`.

- [ ] **Step 3: Commit** — `git commit -m "docs(hermes): routine template for the agent computer"`.

---

# Phase 3 — multi-bot screens and the hermes plugin (new box)

### Task 15: Supervisor multi-screen process table and `/screens`

**Files:**
- Modify: `crates/reach-supervisor/src/processes.rs`, `crates/reach-supervisor/src/health.rs`, `crates/reach-supervisor/src/main.rs`
- Test: `crates/reach-supervisor/src/processes.rs` unit tests

**Interfaces:**
- Produces: env `REACH_SCREENS` (default 1); process names `xvfb-<i>`, `openbox-<i>`, `x11vnc-<i>`, `novnc-<i>`; displays `:99+i`; ports `5900+i`, `6080+i`; `GET /screens` → `[{ "id": i, "display": ":99", "vnc_port": 5900, "novnc_port": 6080 }]`.

- [ ] **Step 1: Failing test**

```rust
#[test]
fn two_screens_produce_two_stacks_on_distinct_ports() {
    let sup = Supervisor::new(99, 1280, 720).with_screens(2);
    let t = sup.process_table();
    assert_eq!(t.len(), 8);
    let vnc: Vec<&ProcessSpec> = t.iter().filter(|p| p.name.starts_with("x11vnc")).collect();
    assert!(vnc[0].args.contains(&"5900".to_string()));
    assert!(vnc[1].args.contains(&"5901".to_string()));
    assert!(t.iter().any(|p| p.name == "xvfb-1" && p.args.contains(&":100".to_string())));
}
```

`ProcessSpec.name` becomes `String` (was `&'static str`); `depends_on: Vec<String>`.

- [ ] **Step 2: Implement** — loop `for i in 0..self.screens` building the four specs with `display = self.display + i`, ports offset by `i`, names suffixed `-{i}`. `main.rs` reads `REACH_SCREENS`. `health.rs` adds `/screens` from `sup.screens()`; `HealthResponse.display` stays screen 0.

- [ ] **Step 3: Verify** — `cargo test -p reach-supervisor`; `docker run -e REACH_SCREENS=2 -p 6080:6080 -p 6081:6081 reach` → both noVNC pages render.

- [ ] **Step 4: Commit** — `git commit -m "feat(supervisor): REACH_SCREENS multi-display stacks and /screens"`.

---

### Task 16: `reach create --screens N`, `screen` param on every tool

**Files:**
- Modify: `crates/reach-cli/src/docker.rs` (`SandboxConfig.screens`, port bindings `5900+i`/`6080+i` → `ports.vnc+i`/`ports.novnc+i`, env `REACH_SCREENS`, `screenshot(target, display)`), `crates/reach-cli/src/tools.rs` (`screen` → `DISPLAY=:99+screen` prefix on `sh`/`py`; `live_view` uses `novnc + screen`), `crates/reach-cli/src/mcp.rs` (add `"screen": {"type":"integer","default":0}` to every tool schema)
- Test: `mcp_types.rs` (`every_tool_accepts_screen`), `docker_types.rs` (port map for 2 screens)

- [ ] **Step 1: Failing tests**

```rust
#[test]
fn every_tool_accepts_screen() {
    for t in tool_definitions() {
        assert!(t.input_schema["properties"].get("screen").is_some(), "{} lacks screen", t.name);
    }
}
```

- [ ] **Step 2: Implement** — `pub fn display_for(screen: u32) -> String { format!(":{}", 99 + screen) }` in `tools.rs`; `sh()` becomes `sh(ctx, target, screen, cmd)` and prefixes `DISPLAY={} `; `screenshot` takes a display. `SandboxPortMapping` gains `screens: u32`.

- [ ] **Step 3: Verify** — unit + e2e (`REACH_SCREENS=2` container, `screenshot {screen:1}` returns a PNG).

- [ ] **Step 4: Commit** — `git commit -m "feat: per-screen tool routing and --screens"`.

---

### Task 17: Agent API — screen leases and takeover flags

**Files:**
- Create: `crates/reach-cli/src/agent.rs`
- Modify: `crates/reach-cli/src/commands/serve.rs` (mount routes), `crates/reach-cli/src/tools.rs` (`auth_handoff` sets `takeover_pending=true` for its screen; a successful re-call clears it)
- Test: `crates/reach-cli/src/agent.rs` unit tests (pure state machine), e2e via curl

**Interfaces:**

```rust
pub struct ScreenState { pub id: u32, pub owner: Option<String>, pub takeover_pending: bool, pub takeover_url: Option<String>, pub leased_at: Option<String> }
pub struct AgentState { screens: Mutex<Vec<ScreenState>> }
impl AgentState {
    pub fn new(n: u32) -> Self;
    pub fn lease(&self, owner: &str) -> Result<u32, LeaseError>;        // first free screen, or the one already owned by `owner`
    pub fn release(&self, id: u32, owner: &str) -> Result<(), LeaseError>;
    pub fn set_takeover(&self, id: u32, pending: bool, url: Option<String>);
    pub fn snapshot(&self) -> Vec<ScreenState>;
}
```

Routes: `GET /agent/screens`, `POST /agent/screens/{id}/lease {owner}`, `DELETE /agent/screens/{id}/lease {owner}`, `POST /agent/screens/{id}/takeover {pending, url?}`; `GET /agent/screens` items include `novnc_url`.

- [ ] **Step 1: Failing tests**

```rust
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
```

- [ ] **Step 2: Implement** the state machine and axum handlers; `AppState` gains `agent: AgentState` sized from the sandbox's `/screens`.

- [ ] **Step 3: Verify** — unit tests; `curl -X POST .../agent/screens/0/lease -d '{"owner":"piper"}'` then `GET` shows owner.

- [ ] **Step 4: Commit** — `git commit -m "feat(agent): screen lease and takeover API"`.

---

### Task 18: Cookie sharing across screens via Playwright `storage_state`

**Files:**
- Modify: `crates/reach-cli/src/docker.rs` (`AUTH_HANDOFF_SCRIPT`, `PAGE_TEXT_SCRIPT`), `crates/reach-cli/src/tools.rs` (`browse` seeds a per-screen profile)
- Test: e2e `t95_cookie_set_on_screen0_visible_on_screen1`

- [ ] **Step 1: Implement** — on `status: authenticated`, `AUTH_HANDOFF_SCRIPT` runs `ctx.storage_state(path="/workspace/.reach/state.json")`. `PAGE_TEXT_SCRIPT` and `AUTH_HANDOFF_SCRIPT`, when launching a context for a profile that has no `Default/Cookies` yet, pass `storage_state="/workspace/.reach/state.json"` if the file exists. `browse` on screen `i>0` uses profile `default-screen{i}` and, if that profile dir is empty, first runs a tiny Playwright snippet that creates it from the state file. Per-screen profile naming: `ProfileMount::container_path_for(&format!("default-screen{i}"))`.

- [ ] **Step 2: e2e** — set a cookie via `auth_handoff` on screen 0 against the local cookie server from Task 7 with `wait_for_url_contains: "/"`; then `page_text` on screen 1 shows `reach=yes`.

- [ ] **Step 3: Commit** — `git commit -m "feat(browser): share login state across screens via storage_state"`.

---

### Task 19: hermes plugin `reach-agent-computer`

**Files:**
- Create: `integrations/hermes/plugins/reach-agent-computer/plugin.yaml`, `.../__init__.py`, `.../test_plugin.py`

**Interfaces:**
- Consumes: Task 17 HTTP API at `REACH_AGENT_URL` (default `http://127.0.0.1:4200`), hermes hooks `on_session_start(session_id, model, platform, **kw)`, `pre_tool_call(tool_name, args, task_id, **kw)` (return `{"modify": {...}}` to merge args), `post_tool_call(tool_name, args, result, **kw)`, `on_session_finalize(session_id, **kw)`; `ctx.inject_message(text, role="user")`.

- [ ] **Step 1: Failing test** (`test_plugin.py`, plain `pytest`, HTTP mocked with a tiny `http.server` thread)

```python
def test_pre_tool_call_injects_leased_screen(plugin_with_fake_api):
    p = plugin_with_fake_api            # fixture: register(ctx) against a fake API that leases screen 1
    p.hooks["on_session_start"](session_id="s1", model="m", platform="cli")
    out = p.hooks["pre_tool_call"](tool_name="reach_page_text", args={"url": "https://x"}, task_id="t")
    assert out == {"modify": {"screen": 1}}
    assert p.hooks["pre_tool_call"](tool_name="terminal", args={}, task_id="t") is None
```

- [ ] **Step 2: Implement** `__init__.py`

```python
"""hermes plugin: lease one reach screen per profile, route reach tools to it, surface takeovers."""
import json, os, urllib.request

API = os.environ.get("REACH_AGENT_URL", "http://127.0.0.1:4200")
OWNER = os.environ.get("HERMES_PROFILE", "default")
_state = {"screen": None}

def _post(path, body):
    req = urllib.request.Request(f"{API}{path}", data=json.dumps(body).encode(),
                                 headers={"content-type": "application/json"}, method="POST")
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.loads(r.read() or b"{}")

def register(ctx):
    def on_start(**kw):
        try:
            screens = json.loads(urllib.request.urlopen(f"{API}/agent/screens", timeout=10).read())
            free = next((s for s in screens if s["owner"] in (None, OWNER)), None)
            if free is None:
                ctx.inject_message("No free Agent Computer screen; desktop tools are unavailable this session.", role="user")
                return
            _post(f"/agent/screens/{free['id']}/lease", {"owner": OWNER})
            _state["screen"] = free["id"]
            ctx.inject_message(f"Agent Computer screen {free['id']} leased. Live view: {free['novnc_url']}", role="user")
        except Exception as e:  # never break the session over the lab API
            ctx.inject_message(f"Agent Computer unavailable: {e}", role="user")

    def pre_tool(tool_name, args, **kw):
        if tool_name.startswith("reach_") and _state["screen"] is not None and "screen" not in args:
            return {"modify": {"screen": _state["screen"]}}
        return None

    def post_tool(tool_name, args, result, **kw):
        if tool_name == "reach_auth_handoff" and '"auth_required"' in str(result):
            try:
                url = json.loads(result).get("vnc_url")
            except Exception:
                url = None
            _post(f"/agent/screens/{_state['screen']}/takeover", {"pending": True, "url": url})

    def on_finalize(**kw):
        if _state["screen"] is not None:
            try:
                urllib.request.urlopen(urllib.request.Request(
                    f"{API}/agent/screens/{_state['screen']}/lease",
                    data=json.dumps({"owner": OWNER}).encode(), method="DELETE",
                    headers={"content-type": "application/json"}), timeout=10)
            except Exception:
                pass

    ctx.register_hook("on_session_start", on_start)
    ctx.register_hook("pre_tool_call", pre_tool)
    ctx.register_hook("post_tool_call", post_tool)
    ctx.register_hook("on_session_finalize", on_finalize)
```

`plugin.yaml`: `name: reach-agent-computer`, `version: "0.1"`, `description: Lease a reach Agent Computer screen per hermes profile`. Enable via `plugins.enabled: [reach-agent-computer]`. Confirm the MCP tool name prefix (`reach_`) by listing tools with `hermes tools` in the lab; adjust the prefix constant if hermes names them differently.

- [ ] **Step 3: Verify** — `pytest integrations/hermes/plugins/reach-agent-computer`; in the lab with two profiles (`hermes profile create piper --clone`), run two `hermes -p <name> chat -q "call live_view"` concurrently and confirm they report screens 0 and 1.

- [ ] **Step 4: Commit** — `git commit -m "feat(hermes): reach-agent-computer plugin (screen lease, routing, takeover notice)"`.

---

### Task 20: Multi-bot handoff via hermes Kanban (docs + verified walkthrough)

**Files:**
- Create: `docs/multi-bot.md`

- [ ] **Step 1: Write** the walkthrough: create profiles `piper` (research) and `otto` (ops) with `--clone`; default profile owns `kanban.dispatch_in_gateway: true`, the others `false`; a board `agent-computer`; a task chain "piper collects → otto files"; both bots run with the plugin; expected: two screens busy in `GET /agent/screens`, handoff visible in the Kanban log.
- [ ] **Step 2: Verify** on the new box; record measured RAM per screen in the doc.
- [ ] **Step 3: Commit** — `git commit -m "docs: multi-bot handoff walkthrough"`.

---

## Self-review

- **Spec coverage.** §5.1 image → T4, T15. §5.2 CLI phase 1 → T1–T3, T5–T7; phase 2 → T11–T13; phase 3 API → T16–T18. §5.3 hermes → T10, T14, T19. §6 data flow exercised by T10 step 4. §7 budget → T9 yaml + T2 cap. §8 security → T9 `hostIP`, T13. §9 errors → skill text in T10, T12 recreate. §10 testing → each task. §11 open questions → T10 step 4 records the image-passthrough answer.
- **Placeholders.** None; every code step has content. T13/T15/T16 give signatures rather than full diffs where the change is mechanical.
- **Type consistency.** `ToolContext`/`dispatch` (T5) used by T6, T7, T16, T17. `SandboxConfig.workspace/memory/restart_unless_stopped` (T2) used by T3, T12. `AgentState` API (T17) consumed by T19's HTTP calls with the same paths. `display_for(screen)` (T16) matches supervisor `:99+i` (T15).
