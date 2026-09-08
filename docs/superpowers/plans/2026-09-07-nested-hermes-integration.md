# Nested Hermes Integration Implementation Plan

**Goal:** Exercise real Hermes session/plugin calls through the authenticated Reach API against an actual nested KVM guest, including guest execution, browser interaction, screenshot, recreation, stale-authority rejection and destruction.

**Architecture:** Keep the existing public lease/tool/viewer authority layer. Replace its concrete Docker dependency with explicit runtime selection. A separate non-root broker owns Firecracker, guest disks and a private Unix management socket; Hermes has neither that socket nor KVM access. Guest commands and authenticated viewer traffic cross vsock, not a host filesystem mount or public management port.

**Tech stack:** Rust Reach CLI/API, Python standard-library broker and vsock guest agent, Lima VZ ARM64, Firecracker1.16.1, Linux6.1.155 inner kernel, existing reach-supervisor/browser tooling and actual Hermes plugin registration/session hooks.

**Approved specification:** User approved the design in this conversation and instructed execution. Firecracker is optional, but it already passed two hardware-KVM boots. QEMU/KVM remains an authorized alternative if needed. Warning memory pressure is recorded without stopping; critical pressure and swap growth exceeding512MiB within five minutes remain abort conditions. No claim of production capacity, external egress qualification or a model-provider roundtrip without exercising it.

## Constraints and ownership

No commits, pushes, application termination, existing VM modification, remote provisioning, real account credentials or ambient Docker/OrbStack use. Work in the existing security/phase1-authority worktree. Beads in the canonical agent-computer checkout remains the task ledger; this file is the implementation contract, not a parallel progress tracker.

Native worker owns Rust crate changes. Broker worker owns scripts/reach_microvm_broker.py, scripts/reach_microvm_guest.py and tests/test_microvm_broker.py. Main owns guest-image provisioning, live tests, integration, review, documentation and mirroring. Those writes do not overlap. Concurrent workers skip builds/tests/formatters/linters; Main runs verification and resolves interface issues. Split change phases into at most five files and validate coherent boundaries; never count expected intermediate migration errors as a passing final build.

## Shared runtime contract

Configuration:

```toml
[runtime]
backend = "microvm"
broker_socket = "/run/reach-microvm/broker.sock"
```

Default Docker configuration continues to work. An explicit microvm selection must never contact Docker or fall back after a config/connection error. Configuration parsing must not silently discard an invalid backend block.

`RuntimeClient` supplies the production lifecycle/execution operations. Existing DockerClient becomes Docker-specific only. Reuse the existing screenshot, reset, browser and sensitive-stdin helpers rather than fork their behavior. All production AppState, ToolContext and native CLI callers must use the selected runtime. Keep the existing serialized Sandbox.container_id field as its opaque runtime identity; do not invent a Docker ID for a microVM.

Broker transport: HTTP POST `/v1/rpc` on an owner-only Unix socket, directory0700/socket0600, Linux peer-UID check. No TCP management endpoint. Request:

```json
{"method":"exec_input","params":{"target":"exact-opaque-uuid","command":["python3","-c","print(1+1)"],"input_base64":""}}
```

Success is HTTP200 with `{"result": value}`. Failure is `{"error":{"code":"stable_code","message":"safe description"}}`, using400 for invalid input,404 missing,409 conflict and503 unavailable. Limit frames/bodies to8MiB. Never automatically retry execution or mutation after an uncertain transport outcome.

Operations and values:

| Method | Parameters | Result |
|---|---|---|
| list | empty object | array of existing Sandbox wire shape |
| create | config: existing SandboxConfig wire shape | Sandbox |
| destroy | target: exact name or full UUID | null |
| inspect_config | target | SandboxConfig |
| incarnation | target | runtime incarnation string |
| exec_input | target, command array, input_base64 | existing ExecOutput wire shape |

Native execution resolves a name to the full immutable runtime UUID before RPC, preventing replacement under the same name from receiving an old operation. Existing lease/incarnation/ref/revocation checks remain load-bearing.

## Phase A: native runtime connection and clean caller migration

Files: crates/reach-cli/src/config.rs, lib.rs, docker.rs and topical runtime modules; affected commands, tools.rs, injection.rs and tests identified by complete DockerClient/config-load searches.

Implement explicit runtime selection, private Unix HTTP client, complete method dispatch, bounded response/error handling and the common browser/helper reuse. Extract oversized Docker source into topical modules; do not retain duplicate browser security logic or compatibility aliases. Migrate every affected production caller and contract-breaking test.

Regression already observed against the current binary: `/tmp/reach-runtime-routing-kXBMRC/routing_smoke.py` configures microvm with an absent broker and a private sentinel Docker socket. Current `reach list` contacts `/containers/json` and exits0; the assertion forbidding Docker access fails. After implementation it must fail startup without touching Docker. Add a permanent behavioral test for this boundary and malformed-backend configuration; no source-text assertions.

Validation: focused runtime/config tests, full Rust crate tests, strict Clippy and binary build. Configuration failure must be observable rather than a default backend.

## Phase B: actual broker and guest transport

Files: scripts/reach_microvm_broker.py, scripts/reach_microvm_guest.py, tests/test_microvm_broker.py.

Broker CLI consumes a private JSON config with socket, state_dir, firecracker, images registry, memory_mib, max_memory_mib, max_guests and exec_timeout_seconds. Each registered image defines kernel/rootfs paths and SHA256 values. Requests select only registered image names, never host paths. Validate digests, input shapes and limits before allocation. Reject host binds, extra raw port publication and unsupported persistence/restart settings explicitly before side effects. No fake fallback.

Lifecycle allocates a new UUID, private copied rootfs, private Firecracker configuration/log/vsock paths and bounded RAM. Spawn the real runtime under non-root KVM authority. Reconcile actual process/guest readiness. Serialize conflicting lifecycle actions, prevent stale UUID redirection, and clean partial creates. Shutdown/crash handling must not leave guests orphaned or kill reused/unowned PIDs.

The guest service listens on AF_VSOCK1024, accepts only host CID2 and uses four-byte big-endian length-prefixed JSON. Broker connects Firecracker's Unix vsock endpoint using `CONNECT 1024\n` and requires its OK response. Guest operations are health, configure, exec and connect. Exec runs commands inside the guest with sensitive input on stdin; enforce time/output limits and kill its process group on timeout without replay. Configure starts the existing reach-supervisor with validated display/screens settings. Command execution uses the sandbox guest identity, not the outer broker identity.

Connect permits only guest-loopback noVNC ports6080 through6080+screens-1. Broker publishes those on outer loopback, retaining the existing ViewerTokenAuth protection. Never publish raw VNC, CDP, supervisor health/control or arbitrary extra ports to Hermes. Guest health checks are performed through guest execution. No NIC is needed for the first controlled end-to-end browser fixture.

Tests defend invalid/unregistered images and host paths before side effects, size/target limits, peer access and stale incarnation behavior. These tests do not stand in for the real boot and browser run.

## Phase C: real guest image and isolated environment

Main builds a real ARM64 ext4 guest containing existing reach-supervisor, reach-chrome, viewer auth module, policies, Xvfb, x11vnc, websockify, browser, Python/Playwright and the vsock service. Reuse the existing Dockerfile's workload requirements without using an ambient host Docker daemon. Guest image provisioning and source transfer use the owned Lima instance and private SSH, not host mounts.

Use a new private Lima home, pinned verified image/kernel/runtime artifacts and a fresh resource baseline. Record warning pressure; stop only under the approved critical/swap-growth conditions. Start with the plan's bounded browser profile, not an unbounded VM. Broker/native service and Hermes use separate identities. Assert Hermes cannot open KVM, broker socket or broker storage. Use synthetic local fixture data only.

## Phase D: real end-to-end acceptance

Start the actual selected native backend and authenticated Reach API. Load the real Hermes plugin in the guest-side Hermes installation/session context, not a reimplemented HTTP client or mocked registry. Capture the trusted session hook/lease flow and issue real tools:

1. Lease a real guest and observe its kernel identity/marker through authorized exec.
2. Browse the guest-local fixture, inspect its real DOM, perform an action and verify fixture state.
3. Capture an actual screenshot and inspect its pixels; test viewer authentication and deny raw access.
4. Destroy/recreate cleanly with a new incarnation. Prove old lease/ref authorization cannot mutate the replacement; acquire fresh authority and perform a positive operation.
5. Release/destroy and confirm owned process/disk/socket cleanup.

No claim of full Hermes model-provider execution if only the actual registered tool/session path was exercised. No claim of network egress containment beyond the tested networkless topology. Missing implementation must be fixed, not reclassified as a boot-only completion.

## Phase E: acceptance and delivery

Review native correctness and broker security independently. Run the applicable Rust tests/Clippy/build and Python tests/Ruff once on settled candidates. Repeat the real integration after corrections. Mirror only verified changes into reach-security-phase1, preserving unrelated work. Update the existing test protocol and operator docs with exact commands, outcomes and remaining constraints. Preserve sanitized evidence, remove owned VM/images/keys/scripts, keep the Beads issue open until the requested end-to-end criteria pass, and do not commit or push automatically.

## Preflight review

Native and broker share only the locked RPC/schema above and have disjoint file ownership. Guest-image work consumes the guest configure/exec/connect protocol and existing supervisor tooling; Main verifies the exact launch contract before building. Potential public-port bypass is resolved by forwarding only authenticated noVNC, never raw VNC/CDP/health. Unsupported bind/persistence requests are explicit errors rather than silently weakened isolation. The baseline forbidden-Docker-routing regression is already red. Execution is approved; no additional execution-mode confirmation is needed.
