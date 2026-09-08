# Lima → Hermes → Nested MicroVM Testing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Establish with live evidence whether this Mac can safely run Hermes inside Lima and computer workloads inside real nested microVMs, then qualify the same architecture independently on ariaserver.

**Architecture:** Physical host → Lima Linux VM → unprivileged Hermes → authenticated, separate lifecycle broker → disposable Linux/KVM microVM. The broker is inside Lima, not inside the Hermes process; Hermes must not receive its administrator credentials, raw hypervisor socket, or unrestricted KVM access. Nested guest RAM is part of the outer VM's allocation, not an additional host allocation.

**Tech Stack:** macOS/Apple Virtualization.framework, Lima 2.2.0 VZ, ARM64 Linux with KVM, existing Reach authority/lifecycle interfaces, Hermes. Firecracker is an optional runtime, not a requirement; the user authorized QEMU/KVM if Firecracker cannot run. Two networkless Firecracker boots now establish bounded local feasibility, not repository integration or production qualification.

**Spec:** User instruction in this conversation: “vm > hermes > microvm”; test locally first, later reproduce on ariaserver. Existing implementation references: `config/lima/reach-lab.yaml`, `scripts/microvm.sh`, `crates/reach-cli/src/commands/create.rs`, `crates/reach-cli/src/lease.rs`, `crates/reach-cli/src/commands/serve.rs`, and the Hermes plugin in `integrations/hermes/plugins/reach-agent-computer/`.

## Global constraints

- Preserve the requested nesting. Sibling VMs and Docker containers are not substitute passing results.
- No real credentials, production accounts, destructive exploit payloads, public listeners, or scanning unrelated network hosts.
- No existing VM deletion, global Docker pruning, host firewall changes, application termination, or remote provisioning without a separately scoped operation.
- Use a uniquely named disposable instance and record every owned process, image, disk, network, key, and temporary path.
- Do not inherit the project's writable host mount or the Lima default mounts template. Disable SSH-agent forwarding, inherited host proxy settings, and automatic service forwarding.
- The provisioning user is not the Hermes runtime identity. Hermes gets no passwordless sudo, sudo group membership, host keys, shared broker UID, or raw hypervisor management socket.
- Record PASS, FAIL, BLOCKED, or NOT RUN per gate. A failed prerequisite blocks dependent claims; it does not turn them into passes or prove the design impossible.
- Use observed memory pressure, swap deltas, and workload behavior for sizing. Swap capacity grows dynamically; “free swap” alone is not a RAM budget.
- No automatic commit, push, merge, deployment, or ariaserver migration under this testing plan.

## Initial evidence — 2026-09-07

- Host: 24 GiB physical RAM; 15 logical CPUs; `kern.hv_support=1`; approximately 146 GiB available disk.
- Current host memory: `kern.memorystatus_vm_pressure_level=2` (warning), roughly 18.2 GiB swap used, approximately 9.36 GiB occupied by the compressor. Do not start the VM while this warning persists.
- Installed Lima 2.2.0 from Homebrew core. `limactl list --json` found no instances in the current user's default Lima home. This does not inventory root, other users, other Lima homes, or remote devices.
- Called Apple's actual `VZGenericPlatformConfiguration.isNestedVirtualizationSupported` through Objective-C runtime: API available, result true. This is a host capability check, NOT a nested Linux/KVM boot result.
- `scripts/microvm.sh` selects OrbStack or Docker. Native `create.rs` uses `DockerClient`. No Firecracker, Cloud Hypervisor, QEMU/KVM or nested-virtualization implementation was found in the searched `crates/`, `config/`, and `scripts/` paths.
- Existing `reach-lab.yaml`: 4 GiB, 2 CPUs, writable `/srv/reach` host-directory mount, no nested virtualization flag. It is a development profile, not this isolation test.
- Correction to previous status: completed Docker/OrbStack security work does not establish support for this nested topology. Backend integration remains an explicit prerequisite.

## Subsequent live evidence — 2026-09-07

- The plugin's HTTP503 classification bug is fixed in both implementation worktrees: only HTTP409 maps to `conflict`; other HTTP errors map to `error`. The new regression failed before the fix; 22 plugin tests and Ruff passed per repository, and 18 real Hermes-registration-to-native-API assertions passed after the fix. Independent review found no issues. These are no-guest integration results, not nested workload evidence.
- Read-only inspection of `ariaserver@100.124.38.17` identified Apple M2, 16 GiB RAM, warning pressure, a running 4 GiB `reach-lab` and stopped 3 GiB `growthcapital-fleet`. Its actual Apple nested-virtualization capability query returned false. The local M5's positive result does not transfer to that server.
- The M5 subsequently passed 61 normal-pressure samples over 300.6 seconds; swap usage decreased by 80 MiB. A private 2 GiB / 2 CPU Lima probe was then permitted under an automatic resource guard.
- The Ubuntu image host failed DNS resolution and direct TLS connection attempts. A read-only copy of ariaserver's original public Ubuntu 26.04 image cache matched Lima 2.2.0's installed pin: `7bcf159e29ad0000bfed9c57875908c39268f5ed1257f4958fa6a9f5f60edd54`. No VM disk or user-state image was copied. The source was `https://cloud-images.ubuntu.com/releases/resolute/release-20260720/ubuntu-26.04-server-cloudimg-arm64.img`; the probe included that template's `internal_netplanOptional: "true"` boot parameter.
- The outer VZ VM booted to Lima readiness with nested virtualization enabled, running Ubuntu `7.0.0-28-generic` aarch64 with 4096-byte pages. `/dev/kvm` existed as `root:kvm` mode660. The provisioning user had no KVM group membership. No virtiofs/9p/SSHFS host mounts were reported. This is not proof of unprivileged KVM access or complete network containment.
- The resource guard observed sustained warning pressure for 30 seconds and stopped only the owned outer VM. At that stop swap usage was below the guard baseline, so the trigger was pressure, not a fixed swap-pool limit. Host pressure returned to normal after stopping. No retry under the unchanged workload is justified.
- The intended dedicated-broker KVM ioctl check could not execute because the VM had already stopped. Firecracker v1.16.1 ARM64 downloaded and matched its published archive SHA256, `8d0e69f6d6f9a1724551f607f18504052c16c1828ee3d4d7b6e6c73380871e0e`, but was not executed. Nested kernel boot, broker integration and all workload/isolation gates remain blocked.
- The observed Ubuntu 7.0 kernel is outside Firecracker v1.16.1's documented tested kernel matrix. A future boot must either explicitly qualify that combination as experimental or use a verified supported kernel; do not infer compatibility from `/dev/kvm` alone.

## Resumed live evidence after memory was freed — 2026-09-07

- The operator reported memory freed. A fresh 300.45-second preflight collected 61 normal-pressure samples; swap decreased 120 MiB. A new private 2 GiB / 2 CPU outer VM booted under the unchanged resource guard.
- Non-root runtime UID999 opened `/dev/kvm`, obtained KVM API12 and successfully issued `KVM_CREATE_VM`. Firecracker v1.16.1 then booted a 512 MiB inner guest twice, with no block devices, NICs, shared filesystems or API listener configured.
- The initial probe configuration omitted the required empty `drives` array and failed before launching a kernel. Adding explicit empty drive/network collections corrected the probe; the failure receipt is retained rather than classified as a runtime incompatibility.
- The inner kernel came from the official, versioned Firecracker CI artifact `firecracker-ci/v1.15/aarch64/vmlinux-6.1.155`. Its MD5 matched the published S3 ETag `897ce0aad7285f83e218bc788c1ab042`; the run pinned SHA256 `e3544b10603acbf3db492cb52e000d22ba202cb4b63b9add027565683e11c591`. This is an HTTPS-source/ETag integrity check, not an independently signed kernel attestation. The ARM64 Image header and published initramfs/console configuration were checked.
- The initramfs used BusyBox and its dynamic loader/libc from the verified Ubuntu image. First serial evidence reported Linux `6.1.155+`, `MemTotal: 495392 kB`, only network device `lo`, computation `sum(1..100)=5050`, and guest-initiated shutdown.
- Outer boot ID: `cc87efc8-3e68-4068-b5ef-480e5bc6b9df`. Inner IDs: `a620583e-bea3-44cb-b390-45db253a890f` and `fb900d45-6421-436a-9ca0-aab90189894c`. Both runs exited0 after guest computation/shutdown. Runtime descriptors included `/dev/kvm`, `anon_inode:kvm-vm` and `anon_inode:kvm-vcpu:0`; UID999 had zero effective capabilities, no-new-privileges and an active seccomp filter.
- An unprivileged stand-in UID997—not an installed/running Hermes agent—was denied KVM and runtime storage access. No host-sharing mounts or SSH agent were visible; the owned host canary was inaccessible through the tested paths and unchanged. This does not prove network egress containment, authenticated broker authorization, or complete cross-process isolation.
- **Resource stability FAIL:** after 234.91 seconds the guard force-stopped the outer VM for warning pressure sustained30 seconds. Swap was24 MiB below the guard baseline, so swap growth did not trigger the stop. Both inner executions had completed before this forced outer teardown; neither makes the overall attempt clean or capacity-qualified.
- First serial output and post-exit PID disappearance were exported. The second successful execution/exit and distinct boot ID were recovered from supervised stdout; the outer shutdown prevented exporting its serial file or an independent post-exit `/proc` check. No retry followed under the unchanged workload.
- Native integration still uses concrete `DockerClient` in `commands/create.rs`, `commands/serve.rs::AppState` and `tools.rs::ToolContext`. Next implementation must replace that runtime dependency at the broker boundary and migrate lifecycle/execution/viewer callers while preserving lease incarnation, revocation, capability checks and reset semantics. No speculative runtime backend was added during this experiment.
- The owned outer instance was deleted after the guard stop. Sanitized evidence is retained at `local://reach-nested-retry-evidence.json`; no existing instance, application or remote VM was modified.

## Files and evidence ownership

- This file owns the test protocol; no runtime source changes are required to publish the plan.
- Create the following proposed YAML only inside a new private temporary run directory as `lima-probe.yaml`; schema-validate it before creating an instance. Never overwrite `reach-lab.yaml` for this probe.
- Store `manifest.json`, `results.json`, serial logs, resource samples, endpoint observations, and sanitized screenshots in that run directory. Manifest fields: timestamp, host architecture/OS, repository commit and dirty state, Lima/runtime versions, resolved configuration hash, image/kernel/rootfs digests, VM identity, allocated resources, and owned-resource inventory.
- Each result records gate ID, expected outcome, observed outcome, status, evidence paths, and cleanup status. Hash downloaded artifacts against official published digests; do not use mutable `latest` artifacts without recording and verifying their resolved identity.

## Gate 1 — Capacity and configuration preflight

- [x] Measure physical RAM, CPU, disk, swap and current pressure; inventory Lima under the current user.
- [x] Install Lima and query actual Apple nested-virtualization support without booting a guest.
- [ ] Resume only after pressure returns to normal for five minutes. Sample `sysctl kern.memorystatus_vm_pressure_level vm.swapusage` and `vm_stat` every five seconds; record baseline deltas, not lifetime swap counters.
- [ ] Start with 2 GiB outer RAM / 2 CPUs and a 512 MiB nested kernel probe. This is only a boot profile, not a browser workload capacity claim.
- [ ] Validate a standalone configuration with no default mounts or Docker base:

```yaml
minimumLimaVersion: 2.2.0
base:
  - template:_images/ubuntu-24.04
vmType: vz
arch: aarch64
nestedVirtualization: true
cpus: 2
memory: 2GiB
disk: 12GiB
mounts: []
containerd:
  system: false
  user: false
ssh:
  loadDotSSHPubKeys: false
  forwardAgent: false
  forwardX11: false
  forwardX11Trusted: false
propagateProxyEnv: false
portForwards:
  - guestPortRange: [1, 65535]
    ignore: true
```

- [x] Run `limactl validate` on the exact YAML above: exit 0, OK. Validation used a private temporary directory, removed afterward; no instance was created.
- [ ] Inspect resolved base images/configuration and pin their verified identities in the manifest before boot; schema acceptance alone does not prove image integrity or isolation.
- [ ] Abort the owned guest if host pressure becomes critical, remains warning for 30 seconds, or swap grows by more than 512 MiB over the pre-boot baseline within five minutes. Record these as conservative test guardrails, not platform capacity limits. Never kill unrelated processes to make the test pass.

**Pass:** Stable normal pressure, verified image/configuration, bounded resource budget and owned-instance inventory. Current result: BLOCKED on memory pressure; host capability query PASS.

## Gate 2 — Outer VM boundary

- [ ] Create/start only the uniquely named instance after Gate 1 passes. Check guest architecture and kernel with `uname -a`; record `/proc/sys/kernel/random/boot_id`.
- [ ] Inventory `findmnt -J`, `/proc/mounts`, `/proc/self/mountinfo`, `ip -j address`, `ip -j route`, and `ss -lntup` inside the guest. Confirm no macOS home/project mounts, SSH agent socket, forwarded Docker socket, or unexpected host listeners.
- [ ] Put a synthetic canary in a run-owned host directory outside guest storage. Attempt access through plausible mount paths from the guest; confirm denial and unchanged host canary hash. Do not expose real files as probes.
- [ ] Start a temporary guest loopback HTTP listener and verify it is not automatically published on the host; separately verify an explicitly requested authenticated loopback tunnel works. Stop only that listener/tunnel.
- [ ] Check routing against an operator-owned host canary service. Lima NAT is not assumed to block guest-to-host traffic. Any reachability contrary to policy is a FAIL requiring an explicit, scoped network policy before proceeding.

**Pass:** Expected mounts/listeners only, host canary inaccessible and unchanged, defined management channel works, forbidden host path blocked with a live positive-control listener.

## Gate 3 — Genuine nested microVM boot

- [x] Inside Lima, inspect `/dev/kvm` ownership and permissions. With the broker runtime identity, open it and issue `KVM_GET_API_VERSION`; require the expected Linux KVM API value 12. A device node alone is insufficient.
- [x] Download a pinned official ARM64 runtime and kernel/userspace artifacts, verify available published digests and record provenance. No emulation fallback, Docker substitution, or host OrbStack invocation. Record unsupported host-kernel combinations as experimental.
- [x] Boot one 512 MiB microVM initially without networking or host-sharing devices. Capture successful serial boot, independent kernel boot ID, runtime PID/UID, KVM file descriptor, and guest `MemTotal`.
- [x] Exercise guest computation and shutdown. Confirm the runtime process exits and no extra VM remains. Record runtime failures before trying an explicitly authorized alternate, such as QEMU/KVM.

**Pass:** A real nested guest with its own kernel executes and exits through hardware KVM. `/dev/kvm`, the Apple support flag and a schema-valid YAML are insufficient. Current bounded boot result: PASS for Firecracker, twice. QEMU/KVM was not needed or tested. Overall resource stability remains FAIL; see the resumed evidence.

## Gate 4 — Repository backend and broker integration

- [ ] Identify the concrete native create/destroy/recreate/exec path used by Hermes. Require it to reach the proven nested backend rather than the current Docker-only create path.
- [ ] Treat missing nested backend support as BLOCKED requiring implementation; do not report the existing container test suite as this gate's evidence. This plan does not prescribe speculative code abstractions before the actual runtime is proven.
- [ ] Run the broker under a separate identity with restricted hypervisor device access and a private management socket. Run Hermes without sudo, broker storage access, KVM access, or ability to signal/debug the broker.
- [ ] Through the real Hermes plugin/native API, request one leased guest, execute one observable operation, destroy it and verify reconciliation. Save actual API outcomes and guest state, not mocked forwarding assertions.
- [ ] Attempt broker socket access, peer process inspection, unauthorized guest creation, arbitrary mount attachment, and cross-lease access as the Hermes UID. All forbidden operations must fail without changing state.

**Pass:** The actual Hermes integration controls the nested guest only through authorized operations; broker authority remains outside Hermes. Current result: BLOCKED pending a real nested backend integration.

## Gate 5 — Authority, replay and confused-deputy tests

- [ ] With fake account A/B and guest A/B, verify no lease, wrong lease, expired lease, revoked lease and wrong account deny mutation. Confirm denied attempts produced no guest-side canary writes.
- [ ] Reuse a request after guest destruction/recreation and after broker restart. Old leases, stale references, prior observation IDs and old viewer credentials must not regain authority.
- [ ] Request host paths, unapproved executable capabilities, protected network destinations and arbitrary hypervisor arguments through valid low-privilege credentials; reject privilege expansion.
- [ ] Exercise approved credential injection with disposable test credentials. Confirm browser operation succeeds while credentials do not appear in response bodies, observation output, logs, routine recordings or exported artifacts.

**Pass:** Positive authorized operations work; each negative case denies before side effects, with scoped sanitized audit evidence.

## Gate 6 — Network containment and browser exfiltration

- [ ] Build an isolated test network with owned canary listeners representing the host, Lima management plane, a second guest, metadata address and an approved application. Do not probe real metadata services or unrelated LAN devices.
- [ ] From both shell and browser, test direct IP, DNS, alternate ports, IPv4/IPv6 where enabled, redirects, WebSockets and DNS-answer changes against those canaries.
- [ ] Confirm default denial of guest-to-host, guest-to-Hermes/broker, guest-to-peer and metadata traffic; allow only explicitly approved workload destinations and broker response channels.
- [ ] Test same-form credential submission, alternate submitters, new-window targets and redirect hops. Keep URL/navigation enforcement and actual network firewall results separate: the existing CDP navigation guard is not an egress firewall.

**Pass:** Allowed destination receives its expected request; denied listeners receive none, backed by listener logs or packet evidence. A connection error against a dead listener is not proof of isolation.

## Gate 7 — Real computer workload and memory sizing

- [ ] After boot/isolation passes and host headroom permits, test 4 GiB outer / 1 GiB inner, then 6 GiB outer / 2 GiB inner only if needed. Recheck host guardrails before each increase. No 8–12 GiB starting allocation.
- [ ] Run real Hermes inside Lima and the browser/GUI workload inside the nested guest. Use a deterministic local fixture first; a real model-driven run requires a separately scoped development credential, never forwarding existing host secret stores.
- [ ] Exercise screenshot, navigation, snapshot/reference targeting, typing, click, authenticated viewer, handoff/resume, and a file download/export. Verify actual fixture state and viewer rendering.
- [ ] Exercise one committed POST followed by a dropped response. Require truthful uncertain outcome and no automatic duplicate mutation.
- [ ] Sample host pressure/swap/CPU and both guest `/proc/meminfo`, PSI, OOM counters, broker/Hermes RSS, boot time and action latency every five seconds. Separate boot success from workload fit.
- [ ] Run 30 minutes and 20 create/use/destroy cycles. Require no OOM, no guardrail violation, no increasing count of orphan processes/mounts/devices/disks, and correct workload results. Record first/last cycle memory separately from filesystem caches.

**Pass:** Correct real workload within observed capacity and repeatable lifecycle behavior. Publish measurements, not a guessed RAM requirement or a universal latency promise.

## Gate 8 — Fault injection and recovery

- [ ] Kill only the run-owned browser, then the nested runtime, then broker, then Hermes in separate cases. Restart the affected layer and observe actual guest reconciliation, revocation and outcome reporting.
- [ ] Disconnect the owned viewer/control connection, freeze only the owned guest, and stop/start only the owned outer Lima VM. Physical-host sleep and power-loss testing need a separate operator-controlled window.
- [ ] Exhaust only a quota-limited guest filesystem and a constrained child-process memory budget; never exhaust host disk/RAM or fork-bomb either VM.
- [ ] Check that side effects committed before connection loss are not replayed, stale authorization is invalidated, partial artifacts are marked incomplete, and recreated clean guests have no prior canary/session state.

**Pass:** Honest failures/uncertainty, bounded resource consumption, no privilege widening, no duplicate effects, and reproducible recovery.

## Gate 9 — Cleanup and reproducibility

- [ ] Export only sanitized evidence and explicitly approved artifacts. Reject path traversal, symlink escape, executable auto-launch, and credential leakage through the export channel.
- [ ] Destroy the nested guests and owned outer VM by recorded identity. Remove only run-owned temporary keys, tunnels, images and disks; preserve the installed Lima package and any unrelated resources.
- [ ] Compare process/listener/VM inventories and disk usage with baseline; confirm host canary unchanged and no residual leased guest or management endpoint.
- [ ] Recreate from the recorded manifest, verified artifacts and configuration; repeat the boot, authority-negative test and one browser operation.

**Pass:** Clean teardown and a second reproducible successful run. Retained evidence explicitly lists anything intentionally left installed or stored.

## Gate 10 — ariaserver qualification, not blind migration

- [ ] First inspect `ariaserver@100.124.38.17` read-only: verified SSH host identity, OS, CPU architecture, RAM/pressure, hypervisor nesting support, Lima version/homes, existing workloads, mounts and network ownership. Do not infer Tailscale, macOS, or VM placement from the address/user name.
- [ ] Distinguish the normal-user, root and Lima instances. Never reuse or overwrite one because its name looks suitable.
- [ ] If the server is Linux or a different architecture, choose and validate its actual outer virtualization backend and matching artifacts. `vmType: vz` is macOS-specific; copying this YAML is not a portable deployment procedure.
- [ ] Provision a fresh destination only after target inspection and scoped approval. Transfer source/configuration and sanitized artifacts, not VM memory snapshots, live lease tokens or host credentials.
- [ ] Repeat Gates 1–9 on the destination. Local success does not transfer its isolation or capacity evidence.

**Pass:** Independently verified VM → Hermes → nested microVM on the destination, with an explicit device-specific resource and networking manifest.

## Acceptance and reporting

Report three separate verdicts: (1) nested virtualization feasibility, (2) repository end-to-end functionality, (3) containment and resource stability. Overall PASS requires all applicable gates and reproducibility; never average away a failed security gate. These tests establish the exercised boundaries, not immunity to hypervisor/kernel exploits or side channels.

Current outcome: bounded nested hardware-KVM boot PASS (two non-root Firecracker executions with independent kernel IDs); repository Hermes/broker/browser integration BLOCKED pending implementation; containment only partially exercised; resource stability FAIL because the guard force-stopped the outer VM after both inner runs. The Ubuntu7.0 host kernel remains outside the pinned runtime's documented tested matrix. AriaServer's M2 reports nested VZ unsupported. No further local runtime attempt without a material capacity change, and no claim of a complete or production-qualified stack.

Sources: https://lima-vm.io/docs/config/vmtype/vz/ and the installed Lima 2.2.0 `limactl info` output. Runtime capability and repository claims above are from local observed checks, not the VZ documentation alone.

Pinned runtime/kernel sources: https://github.com/firecracker-microvm/firecracker/releases/tag/v1.16.1 ; https://raw.githubusercontent.com/firecracker-microvm/firecracker/v1.16.1/docs/kernel-policy.md ; https://s3.amazonaws.com/spec.ccfc.min/?prefix=firecracker-ci/v1.15/aarch64/vmlinux-&list-type=2 .
