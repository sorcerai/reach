# Nested Hermes Verification — 2026-09-08

## Scope

The production registered-tool/session path passed against a real networkless
Firecracker/KVM guest, including an approved browser mutation and authority safety
across same-name replacement. This is infrastructure verification, not a
model-provider-driven job, deployment, production-capacity benchmark, or full
Grok Bot parity.

The execution contract is the
[production acceptance plan](superpowers/plans/2026-09-08-production-browser-and-workflow-acceptance.md).
The task ledger is `agent-computer-91a` in the canonical `agent-computer` checkout.
No commit, push, or deployment was performed by this verification run.

## Candidate and topology

| Artifact | SHA256 |
|---|---|
| Production Linux ARM64 Reach executable | `1fc2e74b8d9bf7e9c384c0ef5fe17789a506e922903717a4b4c11c516f58ebe8` |
| Browser guest rootfs | `5dabc171d1a92828d87f780565519109435bbb5d8b847cc02a929b141a30b958` |
| Inner Linux kernel | `e3544b10603acbf3db492cb52e000d22ba202cb4b63b9add027565683e11c591` |
| Installed broker source | `726ddb52261d8ace524ca8331cb586e4f885f8de6765db2e6307eceecd8ba7c1` |
| Installed guest-agent source | `19adb0acf3a8ea56be01794a288601d6423d7868a6aba2264576195f43f29f23` |

The older `d0dfbea3…` executable in run-6/run-7 planning records is a historical
baseline, not this candidate. The delivery manifest identifies 65 source inputs.
The original 57-input build manifest is preserved separately; a native read-only
image inspection verified all eight builder-supplied guest files against both the
installed Linux source and canonical source, closing its guest-agent omission.
No code or image change was needed for that provenance correction.

The owned Lima VZ outer VM used ARM64, nested virtualization, two vCPUs, and 1.5 GiB
RAM. The broker admitted one 1 GiB guest. Host mounts and automatic port forwarding
were disabled. The guest had no NIC. Its observed kernel was
`Linux 6.1.155+ aarch64 GNU/Linux`; the browser reported `Chrome/151.0.7922.34`.
Hermes UID 994 and broker/API UID 997 were separate identities.

## Changes verified

- Explicit runtime selection and complete native caller migration; invalid MicroVM
  configuration or broker failure does not silently contact Docker.
- An owner-only Unix broker, registered image digests, private guest storage, vsock
  transport, immutable runtime identities, bounded process lifecycle, and cleanup.
- Existing browser, DOM, screenshot, approval, and viewer contracts reused through
  the real Hermes plugin registration and session hooks.
- A bounded 60-second cold browser readiness budget. CDP metadata had become
  available before browser protocol commands were ready; the earlier five-second
  readiness budget failed the real cold path.
- Native DOM action completion bounded independently from the locator timeout.
  A whole-operation timeout explicitly reports an uncertain outcome requiring
  reconciliation, rather than inviting an automatic mutation replay.

## Live results

| Check | Observed result |
|---|---|
| Fresh production browser | No Chrome process before browse; real Hermes `browse` and `page_text` succeeded |
| Approved DOM action | `click` succeeded; DOM and the actual screenshot showed “Confirmed inside the guest” |
| Direct isolation | Hermes denied KVM, broker storage, golden image, broker socket, raw VNC, raw CDP, and supervisor-health access |
| Viewer access | Authenticated observer received real RFB data; unauthenticated viewer and cross-origin requests were rejected |
| Same-name replacement | Old approved lease and DOM action returned HTTP 409 `stale_computer_incarnation` before old-session finalization |
| Independent stale-reference check | An old A reference was rejected under fresh B authority; a newly observed B reference succeeded |
| Viewer replacement fence | Previously working cookie rejected with `viewer scope revoked` at age 25.70 seconds, inside its 300-second lifetime |
| No stale mutation | Replacement marker was initially absent and the forbidden stale-write marker remained absent |
| Fresh replacement authority | Explicit fresh lease, approved execution, expected marker, and authenticated RFB access succeeded |
| Old capability after renewal | Rejected with HTTP 403 `invalid or out-of-scope lease capability` |

The successful browser sequence used incarnation
`fa29649f-b3f0-4ca7-8f1b-0e9f67bc1cc2`. The freshness-bound replacement proof used
`d68d8167-b70b-45e7-a14e-b93e7bad1a49` →
`77ee7594-690c-433b-ba50-8a08dabc3807` with the same executable. Linux API PID 14127
and process start-time counter 622448 remained unchanged across that transition.
The macOS Lima launcher PID is a different PID namespace, not another API identity.

“No stale mutation” refers to the observed marker and fixture contracts, not an
exhaustive comparison of every filesystem block.

## Verification commands and results

On the settled canonical source:

```bash
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build -p reach-cli
uv run --with ruff ruff check --select E9,F63,F7,F82 scripts tests integrations/hermes/plugins/reach-agent-computer integrations/hermes/plugins/buzz-groupchat
uv run --with pytest --with pillow --with numpy pytest tests integrations/hermes/plugins/reach-agent-computer/test_plugin.py integrations/hermes/plugins/buzz-groupchat/test_buzz_plugin.py -q
```

- Rust: **321 passed, 46 ignored**. Formatting, strict Clippy, and build passed.
- Host Python: **208 passed, 2 skipped, 43 subtests passed**. Twelve existing Pillow
  `Image.getdata()` deprecation warnings were reported, not suppressed.
- Linux Python: **80 passed, 25 subtests passed**, including the real Linux lifecycle
  checks. The isolated runner was
  `/var/lib/reach-build/integration-test-venv/bin/python`, not system Python or the
  production Hermes venv. Ruff was absent in that Linux venv; the matching source
  passed the documented host Ruff command instead.
- Independent final native-contract and acceptance-evidence reviews reported no
  blocking findings. The mirror's separate commands, results, and before/after
  source-identity check are retained in `mirror-verification.json`.

These suites overlap; their counts must not be added as unique coverage. Ignored
or skipped tests are not passing live scenarios.

## Guards, cleanup, and qualified observations

A fresh five-minute baseline preceded runtime startup. Warning pressure level 2
was recorded; critical pressure level 4 or rolling five-minute swap growth above
512 MiB would stop the owned VM. Run 8 finished normally without relaxing these
thresholds. The later provenance-only inspection used another fresh guarded
baseline and started only the outer VM: no guest, broker, API, Hermes session, or
browser was started. It inspected the same rootfs read-only and then shut down.

Owned guest storage was empty, Firecracker processes and broker TCP forwarders
were absent, and the API port was closed. The broker was stopped through an
ownership-checked pidfd and its Unix socket was removed. The private Lima instance,
outer disk, downloaded VM inputs, and temporary SSH keypair were removed. The
sanitized receipts and exact accepted Linux executable were preserved.

An earlier production action rejection was reconciled as unchanged. It did not
reproduce in the exact diagnostic, and later production actions passed. Its cause
is **not claimed fixed**; the action helper's 3000 ms CDP connect timeout was not
widened speculatively. A first replacement viewer cookie had already expired and
was excluded from incarnation-fence evidence; the fresh-cookie transition above
replaced that invalid check. An offline filesystem parser also failed to decode the
root directory; that was not treated as evidence of missing image files. Native
read-only inspection subsequently verified all eight supplied files and the same
rootfs digest.

## Evidence and remaining boundary

The private local evidence bundle is:

`~/.local/share/reach/acceptance/2026-09-08-nested-run8-4kd_ynhj/`

It contains the delivery source manifest, exact Linux executable, source bundle,
image/broker provenance, actual screenshot, approvals and tool/session results,
viewer and isolation receipts, guard records, reviews, cleanup receipt, mirror
preservation decisions, and verification results. `checkpoint.json` records the
final delivery state. The bundle is not a published release or committed artifact.

The model-driven build-and-revise extension benchmark is tracked separately as
`agent-computer-df2`, blocked on authorization.
It requires explicit model-provider/credential and any necessary egress
authorization. No model-produced extension, follow-up revision, external-account
workflow, arbitrary egress containment, or broad product parity was demonstrated.
