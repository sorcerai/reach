# Production Browser and Useful-Job Acceptance Implementation Plan

**Goal:** Finish production nested-Hermes acceptance, then demonstrate one useful build-and-revise workflow without mistaking diagnostic success for production parity.

**Architecture:** Preserve the current Hermes session/plugin → authenticated Reach API → private broker → real nested KVM guest path. Diagnose the existing production browser helper before changing behavior. Reuse current authority, observation, approval and viewer contracts; no alternate browser implementation or silent Docker fallback.

**Tech Stack:** Rust Reach, Python Hermes integration/broker/guest agent, Lima VZ ARM64, Firecracker/KVM, Chromium/Playwright, existing authenticated viewer.

**Spec:** `2026-09-07-nested-hermes-integration.md` in this directory, plus the operator's request to identify gaps and formulate the next plan. This document is an execution contract, not a second task tracker. Beads issue `agent-computer-91a` in `/Users/ahpramesi/repos/agent-computer` owns existing integration acceptance. The useful-job demonstration is a subsequent scope, not an extra condition silently added to that issue.

## Constraints

Planning does not authorize a VM restart. Resume only after explicit resource recovery and a fresh five-minute guarded baseline. Record warning pressure level 2; abort at critical level >=4 or rolling five-minute swap growth >512 MiB. Do not reset the baseline to evade an abort. Do not terminate unrelated applications, change existing VMs, provision remote hosts, expose management ports, use real credentials, purchase services, commit, push or deploy.

Keep exact production invocation, sensitive stdin handling, profile/focus fences, immutable incarnation binding and no-replay behavior. Tests and diagnostic binaries do not substitute for final-candidate live acceptance. Build before guest startup, use one Linux build job and disabled dev debug symbols as in the last successful ARM64 build. Preserve interrupted-run evidence.

## Evidence baseline and scope separation

Run 6 checkpoint: `/var/folders/bd/y_5sw19d1wx7h5s0_5qfnkkc0000gn/T/reach-nested-integration-6-3hncxypl/checkpoint.json`.
Run 7 checkpoint: `/var/folders/bd/y_5sw19d1wx7h5s0_5qfnkkc0000gn/T/reach-nested-integration-7-_40z5man/checkpoint.json`.

Production binary SHA256: `d0dfbea3f6f8cfe8b054fd3c03edfec311deb5e74872bace49c1a87b5c3e64d7`. Separate diagnostic SHA256: `a86331d85a2ac260a023f90e177a5182bab4c906e652d8fa63a3a80e1ffd43f4`; built but not exercised against a guest in run 7. Earlier manually supplied diagnostic payload succeeded; final production browse did not. Production exec and screenshot passed. Native gate recorded 319 passed and 46 ignored; Linux Python gate recorded 80 passed plus 25 subtests. The debug-symbol build was OOM-killed and rejected; the reduced-debug build passed.

`/Users/ahpramesi/repos/hermes-agent/docs/HANDOFF.md` describes earlier AXI/tiered-routing work and an OrbStack reach-lab deployment. Its 100%-deployed/pushed language and benchmarks are not evidence for this separate Lima/security-worktree candidate. Do not infer that earlier deployment failed, or that this candidate is deployed, from those different-scope records. Current runtime or performance claims require matching runtime/source receipts.

## Gate 1 — Safe, reproducible diagnostic environment

Consume the run 7 checkpoint and existing launcher/guard assets. Before any restart, obtain the required resource recovery and baseline. Re-stage the existing diagnostic launcher because its guest-side `/tmp` location does not survive reboot. Verify ownership before reconciling any prior process/storage; do not unlink locks to bypass an owner. Bring up the private broker and prove real RPC readiness before creating one guest. Use the original identities and private credential launcher, never print tokens.

Output: timestamped guard record, source/binary/image digests, selected runtime, broker readiness, guest UUID/incarnation, and exact launcher identity. Stop immediately under the unchanged guard; report a blocked attempt, not browser failure or host incapacity.

## Gate 2 — Diagnose and repair the actual production browser path

Primary source surface: `crates/reach-cli/src/tools.rs`; follow the actual helper/runtime bindings with LSP references before editing. Broker/transport changes, only if the trace proves they own the defect: `scripts/reach_microvm_broker.py` and `scripts/reach_microvm_guest.py`. Use existing relevant Rust tests and `tests/test_microvm_broker.py`; do not preselect an implementation based on the prior timeout hypothesis.

Run the real Hermes registered browse call through the separate exact-invocation diagnostic binary. Capture sanitized exception type and helper frame/stage; do not change payload generation, grant selection, focus fences or stdin to make the probe succeed. The earlier attempted site-packages trace installation lacked permissions; use the separately built diagnostic route rather than widening Hermes write authority.

Once the failing boundary is known, construct a minimal behavior reproduction that fails on the unfixed candidate. Keep one or two regression tests only where they defend the identified bug. Apply the smallest source fix; no guessed timeout expansion, tab deletion, fallback browser or replay of uncertain actions. Rebuild the final production binary and repeat the original call without diagnostic-only behavior.

Output: exact root cause, failing and passing reproduction receipts, final source/binary hashes, successful cold production browse followed by readable DOM. A successful diagnostic run alone does not pass this gate.

## Gate 3 — Complete real production operation acceptance

Use real Hermes PluginContext, ToolRegistry and session hooks against a fresh production guest. Lease it; execute a guest kernel/marker observation; navigate the local fixture; obtain DOM references; explicitly approve one mutation; verify resulting fixture state; capture and visually inspect an actual screenshot. Verify authenticated viewer access and unauthenticated/cross-origin rejection. Recheck the existing seven direct-isolation boundaries under the same candidate. Record approvals and results without secrets.

Output: one candidate-bound successful operation sequence, observed state changes and visual evidence. Label registered-tool/session execution accurately; this is not yet a model-provider-driven job.

## Gate 4 — Prove authority safety across replacement

Keep the Reach API process and old lease alive while replacing a guest under the same name. Record distinct immutable incarnations. Attempt old lease, old DOM reference and old viewer access against the replacement; all must be rejected with no replacement-state mutation. Acquire fresh authority and demonstrate a positive operation on the replacement. Preserve no-replay handling when a mutation outcome is uncertain.

Output: old-versus-new identity records, negative authorization results, unchanged replacement marker after denied mutations, and a successful fresh-authority action. Unit tests are supporting evidence, not substitutes for this live transition.

## Gate 5 — Accept and deliver the infrastructure candidate

After live acceptance, run settled-source Rust tests, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` and build, plus affected Python checks using the established project runners and Ruff configuration. Hermes checkout tests must use `scripts/run_tests.sh`. Report ignored/skipped tests separately. If source changes after the live run, invalidate affected receipts and rerun the relevant scenario.

Perform independent correctness/security review before acceptance. Then update operator documentation and evidence with exact commands, source identity, outcomes and remaining limits. Confirm cleanup of owned guests/processes/disks/sockets/temporary keys without deleting unrelated state or preserved evidence. Mirror only verified scoped changes into `/Users/ahpramesi/repos/reach-security-phase1`, preserving unrelated work and comparing mirrored source identities. Close `agent-computer-91a` only when its original acceptance criteria pass. Commit, push and deployment remain separate authorization decisions.

Output: reviewed final candidate, reproducible receipts, confirmed owned-resource cleanup and verified-source mirror. This completes infrastructure acceptance, not full Grok Bot parity.

## Gate 6 — Demonstrate one useful build-and-revise job

After infrastructure acceptance, track this as a separate Beads task. Proposed bounded benchmark: ask the actual Hermes model loop to create a Chromium extension that blocks one selected synthetic site during a configured interval, allows it outside that interval, and provides an explicit user-controlled disable action. Use a disposable browser profile, local synthetic sites and a disposable workspace; no real browsing history, paid development service, Stripe account or native Mac app. This is a browser-extension benchmark, not OS-level application blocking parity.

Before execution, establish authorized model-provider access and any required network/egress scope. Existing networkless-guest evidence does not authorize adding unrestricted egress. Reuse existing execution and artifact transfer mechanisms; inspect them rather than inventing mounts or privileged installation paths.

The model must produce the source and loadable artifact, install it in the disposable browser, exercise both allowed and blocked states, and report real results. Ask for a second blocked synthetic site while preserving the first site's behavior. Exercise both sites and disable/re-enable after the revision. Capture the model/tool transcript, artifact hashes, actual browser observations and any human handoffs. Inject no fake success or manually completed artifact into the model's result.

Output: an installed, behaviorally verified artifact and successful follow-up revision from a real model-driven job. If authorization, egress, installation or artifact delivery is unavailable, name that exact gap and do not count this gate as passed. Do not extend the gate to payments, publishing or broad product parity.

## Ordering and delegation

Gates 1–4 are sequential because they depend on one guarded runtime and the exact final production candidate. Do not split diagnosis and fix into competing speculative implementations. Once a trace identifies genuinely disjoint native and broker changes, assign non-overlapping owners; workers skip concurrent validation and the integration owner runs final gates. Documentation/evidence reconciliation can occur without starting the VM. No new feature work is needed to unblock the browser unless the trace proves it.

## Definition of success

Infrastructure accepted means the production path and replacement-safety scenario pass on the same identified candidate, with review and delivery evidence. Useful-job parity demonstrated means one actual model-driven artifact workflow and revision passes afterward. Neither means full Grok Bot parity, proven production capacity, current historical benchmark numbers, or runtime deployment.
