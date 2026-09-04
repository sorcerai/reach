use anyhow::{Context, Result};
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use std::path::Path;
use std::time::Duration;
use tokio::process::{Child, Command};
use tokio::time;

// ═══════════════════════════════════════════════════════════
// Process specification — what to run and how to manage it
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct ProcessSpec {
    pub name: String,
    pub command: &'static str,
    pub args: Vec<String>,
    pub env: Vec<(&'static str, String)>,
    pub restart: RestartPolicy,
    pub depends_on: Vec<String>,
    pub ready_check: ReadyCheck,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScreenInfo {
    pub id: u32,
    pub display: String,
    pub vnc_port: u16,
    pub novnc_port: u16,
}

#[derive(Debug, Clone)]
pub enum RestartPolicy {
    Always {
        max_restarts: u32,
        backoff: Duration,
    },
}

#[derive(Debug, Clone)]
pub enum ReadyCheck {
    /// Process is ready when file exists (e.g., X11 socket)
    FileExists(String),
    /// Process is ready when TCP port accepts connections
    TcpPort(u16),
    /// Process is ready immediately after spawn
    Immediate,
}

// ═══════════════════════════════════════════════════════════
// Process state — runtime status of a managed process
// ═══════════════════════════════════════════════════════════

#[derive(Debug)]
pub struct ManagedProcess {
    pub spec: ProcessSpec,
    pub state: ProcessState,
}

#[derive(Debug)]
pub enum ProcessState {
    Stopped,
    Starting,
    Running {
        child: Child,
        pid: u32,
        started_at: std::time::Instant,
        restart_count: u32,
    },
    Failed {
        exit_code: Option<i32>,
        restart_count: u32,
        last_error: String,
    },
}

impl ProcessState {
    pub fn is_running(&self) -> bool {
        matches!(self, ProcessState::Running { .. })
    }

    pub fn pid(&self) -> Option<u32> {
        match self {
            ProcessState::Running { pid, .. } => Some(*pid),
            _ => None,
        }
    }

    pub fn uptime(&self) -> Option<Duration> {
        match self {
            ProcessState::Running { started_at, .. } => Some(started_at.elapsed()),
            _ => None,
        }
    }

    pub fn restart_count(&self) -> u32 {
        match self {
            ProcessState::Running { restart_count, .. } => *restart_count,
            ProcessState::Failed { restart_count, .. } => *restart_count,
            _ => 0,
        }
    }

    pub fn exit_code(&self) -> Option<i32> {
        match self {
            ProcessState::Failed { exit_code, .. } => *exit_code,
            _ => None,
        }
    }

    pub fn last_error(&self) -> Option<&str> {
        match self {
            ProcessState::Failed { last_error, .. } => Some(last_error),
            _ => None,
        }
    }
}

// ═══════════════════════════════════════════════════════════
// Health — reported per-process and aggregate
// ═══════════════════════════════════════════════════════════

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProcessHealth {
    pub name: String,
    pub status: ProcessStatus,
    pub pid: Option<u32>,
    pub uptime_secs: Option<f64>,
    pub restart_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessStatus {
    Running,
    Starting,
    Stopped,
    Failed,
}

// ═══════════════════════════════════════════════════════════
// Supervisor — owns and manages all processes
// ═══════════════════════════════════════════════════════════

pub struct Supervisor {
    processes: Vec<ManagedProcess>,
    display: u32,
    width: u32,
    height: u32,
    screens: u32,
    vnc_password: Option<String>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new(99, 1280, 720)
    }
}

impl Supervisor {
    pub fn new(display: u32, width: u32, height: u32) -> Self {
        Self {
            processes: Vec::new(),
            display,
            width,
            height,
            screens: 1,
            vnc_password: None,
        }
    }

    pub fn with_screens(mut self, screens: u32) -> Self {
        self.screens = screens.max(1);
        self
    }

    pub fn display(&self) -> u32 {
        self.display
    }

    pub fn screens(&self) -> Vec<ScreenInfo> {
        (0..self.screens)
            .map(|i| ScreenInfo {
                id: i,
                display: format!(":{}", self.display + i),
                vnc_port: 5900 + i as u16,
                novnc_port: 6080 + i as u16,
            })
            .collect()
    }

    /// Set (or clear) the VNC password. When set, `x11vnc` runs with
    /// `-passwd <pw>` instead of `-nopw`.
    ///
    /// Note: `x11vnc` is passed the password on its command line, so in
    /// principle another process could catch it via `execve` tracing or a
    /// race right at spawn time — `x11vnc` itself scrubs `-passwd <pw>`
    /// from its own argv immediately after startup (confirmed: it does not
    /// appear in `ps`/`/proc/<pid>/cmdline` once running), so this is not
    /// the plaintext-in-`ps`-forever exposure it would be for most CLI
    /// tools. Acceptable for phase 2 — the container itself is the trust
    /// boundary — but not a substitute for `-rfbauth` if that boundary
    /// ever moves.
    pub fn with_vnc_password(mut self, vnc_password: Option<String>) -> Self {
        self.vnc_password = vnc_password;
        self
    }

    pub fn from_env() -> Self {
        let display = std::env::var("DISPLAY_NUM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(99);
        let width = std::env::var("WIDTH")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1280);
        let height = std::env::var("HEIGHT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(720);
        let screens = std::env::var("REACH_SCREENS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        let vnc_password = std::env::var("VNC_PASSWORD").ok().filter(|s| !s.is_empty());

        Self::new(display, width, height)
            .with_screens(screens)
            .with_vnc_password(vnc_password)
    }

    /// Build the process table — defines what runs and in what order.
    pub(crate) fn process_table(&self) -> Vec<ProcessSpec> {
        let resolution = format!("{}x{}x24", self.width, self.height);
        let mut specs = Vec::with_capacity((self.screens * 4) as usize);

        for i in 0..self.screens {
            let display_num = self.display + i;
            let display = format!(":{display_num}");
            let vnc_port = 5900 + i as u16;
            let novnc_port = 6080 + i as u16;

            specs.push(ProcessSpec {
                name: format!("xvfb-{i}"),
                command: "Xvfb",
                args: vec![
                    display.clone(),
                    "-screen".into(),
                    "0".into(),
                    resolution.clone(),
                    "-nolisten".into(),
                    "tcp".into(),
                ],
                env: vec![],
                restart: RestartPolicy::Always {
                    max_restarts: 5,
                    backoff: Duration::from_secs(1),
                },
                depends_on: vec![],
                ready_check: ReadyCheck::FileExists(format!("/tmp/.X11-unix/X{display_num}")),
            });

            specs.push(ProcessSpec {
                name: format!("openbox-{i}"),
                command: "openbox",
                args: vec!["--sm-disable".into()],
                env: vec![("DISPLAY", display.clone())],
                restart: RestartPolicy::Always {
                    max_restarts: 5,
                    backoff: Duration::from_secs(1),
                },
                depends_on: vec![format!("xvfb-{i}")],
                ready_check: ReadyCheck::Immediate,
            });

            specs.push(ProcessSpec {
                name: format!("x11vnc-{i}"),
                command: "x11vnc",
                args: {
                    let mut args = vec!["-display".to_string(), display.clone(), "-forever".into()];
                    match &self.vnc_password {
                        Some(pw) => args.extend(["-passwd".into(), pw.clone()]),
                        None => args.push("-nopw".into()),
                    }
                    args.extend(["-rfbport".into(), vnc_port.to_string(), "-shared".into()]);
                    args
                },
                env: vec![],
                restart: RestartPolicy::Always {
                    max_restarts: 10,
                    backoff: Duration::from_millis(500),
                },
                depends_on: vec![format!("xvfb-{i}")],
                ready_check: ReadyCheck::TcpPort(vnc_port),
            });

            specs.push(ProcessSpec {
                name: format!("novnc-{i}"),
                command: "websockify",
                args: vec![
                    "--web".into(),
                    "/opt/noVNC".into(),
                    novnc_port.to_string(),
                    format!("localhost:{vnc_port}"),
                ],
                env: vec![],
                restart: RestartPolicy::Always {
                    max_restarts: 5,
                    backoff: Duration::from_secs(1),
                },
                depends_on: vec![format!("x11vnc-{i}")],
                ready_check: ReadyCheck::TcpPort(novnc_port),
            });
        }

        specs
    }

    /// Start all processes in dependency order.
    pub async fn start_all(&mut self) -> Result<()> {
        let specs = self.process_table();
        for spec in specs {
            self.spawn_process(spec).await?;
        }
        tracing::info!("all processes started");
        Ok(())
    }

    async fn spawn_process(&mut self, spec: ProcessSpec) -> Result<()> {
        tracing::info!(name = spec.name, cmd = spec.command, "starting process");

        let mut cmd = Command::new(spec.command);
        cmd.args(&spec.args);
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        // Push as Starting before spawn
        let idx = self.processes.len();
        self.processes.push(ManagedProcess {
            spec: spec.clone(),
            state: ProcessState::Starting,
        });

        let child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn {}", spec.name))?;

        let pid = child.id().unwrap_or(0);

        // Wait for ready check
        self.wait_ready(&spec.ready_check).await?;

        tracing::info!(name = %spec.name, pid, "process ready");

        if spec.name.starts_with("xvfb-") {
            let display = spec.args.first().cloned().unwrap_or_else(|| ":99".into());
            set_root_background(&display).await;
        }

        // Transition to Running
        self.processes[idx] = ManagedProcess {
            spec,
            state: ProcessState::Running {
                child,
                pid,
                started_at: std::time::Instant::now(),
                restart_count: 0,
            },
        };

        Ok(())
    }

    async fn wait_ready(&self, check: &ReadyCheck) -> Result<()> {
        match check {
            ReadyCheck::Immediate => Ok(()),
            ReadyCheck::FileExists(path) => {
                for _ in 0..50 {
                    if Path::new(path).exists() {
                        return Ok(());
                    }
                    time::sleep(Duration::from_millis(100)).await;
                }
                anyhow::bail!("timeout waiting for {path}")
            }
            ReadyCheck::TcpPort(port) => {
                let addr = format!("127.0.0.1:{port}");
                for _ in 0..50 {
                    if tokio::net::TcpStream::connect(&addr).await.is_ok() {
                        return Ok(());
                    }
                    time::sleep(Duration::from_millis(100)).await;
                }
                anyhow::bail!("timeout waiting for port {port}")
            }
        }
    }

    /// Graceful shutdown — SIGTERM each child, wait, then SIGKILL stragglers.
    pub async fn stop_all(&mut self) -> Result<()> {
        for proc in self.processes.iter_mut().rev() {
            if let ProcessState::Running { child, pid, .. } = &mut proc.state {
                tracing::info!(name = %proc.spec.name, pid, "sending SIGTERM");
                let nix_pid = Pid::from_raw(*pid as i32);
                let _ = nix::sys::signal::kill(nix_pid, Signal::SIGTERM);

                match time::timeout(Duration::from_secs(5), child.wait()).await {
                    Ok(Ok(status)) => {
                        tracing::info!(
                            name = %proc.spec.name,
                            code = ?status.code(),
                            "exited cleanly"
                        );
                    }
                    _ => {
                        tracing::warn!(name = %proc.spec.name, "SIGKILL after timeout");
                        let _ = child.kill().await;
                    }
                }
            }
            proc.state = ProcessState::Stopped;
        }
        Ok(())
    }

    /// Check all processes and restart any that have exited unexpectedly.
    /// Returns the number of processes restarted.
    pub async fn check_and_restart(&mut self) -> Result<usize> {
        let mut restarted = 0;

        // Snapshot which processes are running before mutable iteration
        let running_names: Vec<String> = self
            .processes
            .iter()
            .filter(|p| p.state.is_running())
            .map(|p| p.spec.name.clone())
            .collect();

        for proc in &mut self.processes {
            if let ProcessState::Running {
                child,
                restart_count,
                ..
            } = &mut proc.state
            {
                // Non-blocking check if process exited
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let code = status.code();
                        let count = *restart_count;

                        tracing::warn!(
                            name = %proc.spec.name,
                            exit_code = ?code,
                            restart_count = count,
                            "process exited unexpectedly"
                        );

                        // Check restart policy
                        match &proc.spec.restart {
                            RestartPolicy::Always {
                                max_restarts,
                                backoff,
                            } => {
                                if count >= *max_restarts {
                                    tracing::error!(
                                        name = %proc.spec.name,
                                        "max restarts ({max_restarts}) exceeded"
                                    );
                                    proc.state = ProcessState::Failed {
                                        exit_code: code,
                                        restart_count: count,
                                        last_error: format!(
                                            "max restarts exceeded (exit code: {:?})",
                                            code
                                        ),
                                    };
                                    continue;
                                }

                                // Backoff before restart
                                time::sleep(*backoff).await;

                                // Check dependencies are still running
                                let deps_ok = proc
                                    .spec
                                    .depends_on
                                    .iter()
                                    .all(|dep| running_names.contains(dep));

                                if !deps_ok {
                                    tracing::warn!(
                                        name = %proc.spec.name,
                                        "dependency not running, marking failed"
                                    );
                                    proc.state = ProcessState::Failed {
                                        exit_code: code,
                                        restart_count: count,
                                        last_error: "dependency not running".into(),
                                    };
                                    continue;
                                }

                                // Restart
                                tracing::info!(
                                    name = %proc.spec.name,
                                    attempt = count + 1,
                                    "restarting process"
                                );

                                let mut cmd = Command::new(proc.spec.command);
                                cmd.args(&proc.spec.args);
                                for (k, v) in &proc.spec.env {
                                    cmd.env(k, v);
                                }

                                match cmd.spawn() {
                                    Ok(new_child) => {
                                        let pid = new_child.id().unwrap_or(0);
                                        proc.state = ProcessState::Running {
                                            child: new_child,
                                            pid,
                                            started_at: std::time::Instant::now(),
                                            restart_count: count + 1,
                                        };
                                        if proc.spec.name.starts_with("xvfb-") {
                                            let display = proc
                                                .spec
                                                .args
                                                .first()
                                                .cloned()
                                                .unwrap_or_else(|| ":99".into());
                                            set_root_background(&display).await;
                                        }
                                        restarted += 1;
                                    }
                                    Err(e) => {
                                        proc.state = ProcessState::Failed {
                                            exit_code: code,
                                            restart_count: count,
                                            last_error: e.to_string(),
                                        };
                                    }
                                }
                            }
                        }
                    }
                    Ok(None) => {} // still running
                    Err(e) => {
                        tracing::error!(name = %proc.spec.name, error = %e, "failed to check process");
                    }
                }
            }
        }

        Ok(restarted)
    }

    /// Collect health status for all managed processes.
    pub fn health(&self) -> Vec<ProcessHealth> {
        self.processes
            .iter()
            .map(|p| ProcessHealth {
                name: p.spec.name.clone(),
                status: match &p.state {
                    ProcessState::Running { .. } => ProcessStatus::Running,
                    ProcessState::Starting => ProcessStatus::Starting,
                    ProcessState::Stopped => ProcessStatus::Stopped,
                    ProcessState::Failed { .. } => ProcessStatus::Failed,
                },
                pid: p.state.pid(),
                uptime_secs: p.state.uptime().map(|d| d.as_secs_f64()),
                restart_count: p.state.restart_count(),
                exit_code: p.state.exit_code(),
                last_error: p.state.last_error().map(String::from),
            })
            .collect()
    }

    /// Check if all processes are running.
    pub fn all_healthy(&self) -> bool {
        self.processes.iter().all(|p| p.state.is_running())
    }
}

/// Set root window background color so noVNC never shows pitch black.
pub async fn set_root_background(disp: &str) {
    let color = std::env::var("REACH_BG_COLOR").unwrap_or_else(|_| "#1e1e2e".to_string());

    // 1. Try xsetroot
    if let Ok(mut child) = Command::new("xsetroot")
        .args(["-display", disp, "-solid", &color])
        .spawn()
        && let Ok(status) = child.wait().await
        && status.success()
    {
        tracing::info!(disp, %color, "root window background set via xsetroot");
        return;
    }

    // 2. Try reach-wallpaper script
    if let Ok(mut child) = Command::new("reach-wallpaper")
        .args([disp])
        .env("REACH_BG_COLOR", &color)
        .spawn()
        && let Ok(status) = child.wait().await
        && status.success()
    {
        tracing::info!(disp, %color, "root window background set via reach-wallpaper");
        return;
    }

    // 3. Fallback to python ctypes using libX11.so.6
    let py_script = r#"
import ctypes, sys
class XColor(ctypes.Structure):
    _fields_ = [("pixel", ctypes.c_ulong), ("red", ctypes.c_ushort), ("green", ctypes.c_ushort), ("blue", ctypes.c_ushort), ("flags", ctypes.c_byte), ("pad", ctypes.c_byte)]
try:
    x11 = ctypes.cdll.LoadLibrary("libX11.so.6")
    x11.XOpenDisplay.restype = ctypes.c_void_p
    x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
    x11.XDefaultScreen.restype = ctypes.c_int
    x11.XDefaultScreen.argtypes = [ctypes.c_void_p]
    x11.XDefaultColormap.restype = ctypes.c_ulong
    x11.XDefaultColormap.argtypes = [ctypes.c_void_p, ctypes.c_int]
    x11.XDefaultRootWindow.restype = ctypes.c_ulong
    x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
    x11.XAllocNamedColor.restype = ctypes.c_int
    x11.XAllocNamedColor.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_char_p, ctypes.POINTER(XColor), ctypes.POINTER(XColor)]
    x11.XSetWindowBackground.restype = ctypes.c_int
    x11.XSetWindowBackground.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong]
    x11.XClearWindow.restype = ctypes.c_int
    x11.XClearWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    x11.XFlush.restype = ctypes.c_int
    x11.XFlush.argtypes = [ctypes.c_void_p]
    x11.XCloseDisplay.restype = ctypes.c_int
    x11.XCloseDisplay.argtypes = [ctypes.c_void_p]

    display = sys.argv[1].encode()
    color = sys.argv[2].encode()
    dpy = x11.XOpenDisplay(display)
    if dpy:
        screen = x11.XDefaultScreen(dpy)
        cmap = x11.XDefaultColormap(dpy, screen)
        root = x11.XDefaultRootWindow(dpy)
        exact, closest = XColor(), XColor()
        if x11.XAllocNamedColor(dpy, cmap, color, ctypes.byref(exact), ctypes.byref(closest)):
            x11.XSetWindowBackground(dpy, root, exact.pixel)
            x11.XClearWindow(dpy, root)
            x11.XFlush(dpy)
            x11.XCloseDisplay(dpy)
except Exception:
    pass
"#;
    let _ = Command::new("python3")
        .args(["-c", py_script, disp, &color])
        .status()
        .await;
}

/// Remove stale X11 lock files that prevent Xvfb from starting.
pub fn clean_x11_locks() -> Result<()> {
    for display in 99..116 {
        let lock = format!("/tmp/.X{display}-lock");
        let path = Path::new(&lock);
        if path.exists() {
            std::fs::remove_file(path)?;
            tracing::info!("removed stale X11 lock: {lock}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11vnc_uses_password_when_env_set() {
        let sup = Supervisor::new(99, 1280, 720).with_vnc_password(Some("s3cret".into()));
        let x = sup
            .process_table()
            .into_iter()
            .find(|p| p.name == "x11vnc-0")
            .unwrap();
        assert!(x.args.windows(2).any(|w| w == ["-passwd", "s3cret"]));
        assert!(!x.args.iter().any(|a| a == "-nopw"));
    }

    #[test]
    fn x11vnc_uses_nopw_when_unset() {
        let sup = Supervisor::new(99, 1280, 720);
        let x = sup
            .process_table()
            .into_iter()
            .find(|p| p.name == "x11vnc-0")
            .unwrap();
        assert!(x.args.iter().any(|a| a == "-nopw"));
        assert!(!x.args.iter().any(|a| a == "-passwd"));
    }

    #[test]
    fn two_screens_produce_two_stacks_on_distinct_ports() {
        let sup = Supervisor::new(99, 1280, 720).with_screens(2);
        let t = sup.process_table();
        assert_eq!(t.len(), 8);
        let vnc: Vec<&ProcessSpec> = t.iter().filter(|p| p.name.starts_with("x11vnc")).collect();
        assert!(vnc[0].args.contains(&"5900".to_string()));
        assert!(vnc[1].args.contains(&"5901".to_string()));
        assert!(
            t.iter()
                .any(|p| p.name == "xvfb-1" && p.args.contains(&":100".to_string()))
        );
    }
}
