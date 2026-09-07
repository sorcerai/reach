use anyhow::{Context, Result, bail};
use reach_secrets::{
    agent_card::AgentCardEngine,
    vault::{self, Vault},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{docker::DockerClient, lease::LeaseGrant};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InjectionRequest {
    pub kind: String,
    pub domain: String,
    #[serde(default)]
    pub card_id: Option<String>,
    #[serde(default)]
    pub submit: bool,
}

/// Control-plane receipt. Secret values and helper output are deliberately not retained.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InjectionReceipt {
    pub status: String,
    pub outcome: String,
    pub submitted: bool,
}

#[derive(Debug, Deserialize)]
struct HelperOutcome {
    status: String,
}

/// Fixed browser helper. It receives the JSON payload on stdin only.
pub const INJECTION_HELPER_SOURCE: &str = concat!(
    include_str!("../assets/browser_page.py"),
    r#"
import json
import sys

def emit(status, outcome, submitted=False):
    print(json.dumps({"status": status, "outcome": outcome, "submitted": bool(submitted)}))

def stop(status, outcome, submitted=False):
    emit(status, outcome, submitted)
    raise SystemExit

mutation_started = False
try:
    payload = json.load(sys.stdin)
    kind = payload.get("kind")
    domain = payload.get("domain")
    allowed = set(payload.get("allowed_origins") or [])
    screen = int(payload.get("screen") or 0)
    submit = bool(payload.get("submit"))
    if kind not in ("vault", "card") or not domain or not allowed:
        stop("auth_required", "invalid_request")
    try:
        from playwright.sync_api import sync_playwright
    except Exception:
        stop("auth_required", "playwright_unavailable")

    with sync_playwright() as playwright:
        browser = playwright.chromium.connect_over_cdp(
            "http://127.0.0.1:%d" % (9222 + screen), timeout=3000
        )
        try:
            page = current_page(browser)
        except RuntimeError:
            stop("auth_required", "no_active_page")

        def verify_page():
            origin = page.evaluate("location.origin")
            if origin not in allowed:
                stop("rejected", "origin_mismatch")
            for frame in page.frames:
                frame_origin = frame.evaluate("location.origin") if frame.url else ""
                if frame_origin and frame_origin != "null" and frame_origin != origin:
                    stop("rejected", "cross_origin_frame")
                document = frame.evaluate("""() => {
                    const base = new URL(document.baseURI);
                    const forms = Array.from(document.forms).map(form => {
                        const raw = form.getAttribute("action");
                        const action = new URL(
                            raw === null || raw === "" ? document.baseURI : raw,
                            document.baseURI
                        );
                        return action.origin;
                    });
                    return {base_origin: base.origin, form_origins: forms};
                }""") if frame.url else {"base_origin": "", "form_origins": []}
                base_origin = document["base_origin"]
                if base_origin != origin:
                    stop("rejected", "cross_origin_form_action")
                if any(action != origin for action in document["form_origins"]):
                    stop("rejected", "cross_origin_form_action")

        def metadata(element):
            return {key: (element.get_attribute(key) or "").lower() for key in
                    ("type", "autocomplete", "name", "id", "aria-label", "placeholder")}

        def choose(field):
            best, best_score = None, 0
            candidates = page.locator("input:not([type='hidden']), textarea")
            for index in range(candidates.count()):
                element = candidates.nth(index)
                if not element.is_visible() or element.is_disabled() or not element.is_editable():
                    continue
                info = metadata(element)
                haystack = " ".join(info.values())
                score = 0
                if field == "username":
                    score += 100 if info["autocomplete"] in ("username", "email") else 0
                    score += 40 if info["type"] == "email" else 0
                    score += 20 if any(word in haystack for word in ("user", "email", "login")) else 0
                elif field == "password":
                    score += 100 if info["type"] == "password" or info["autocomplete"] == "current-password" else 0
                    score += 20 if "pass" in haystack else 0
                elif field == "otp":
                    score += 100 if info["autocomplete"] in ("one-time-code", "otp") else 0
                    score += 30 if any(word in haystack for word in ("otp", "2fa", "two-factor", "verification", "code")) else 0
                elif field == "card_number":
                    score += 100 if info["autocomplete"] == "cc-number" else 0
                    score += 30 if any(word in haystack for word in ("card", "pan", "cc-number")) else 0
                elif field == "exp":
                    score += 100 if info["autocomplete"] == "cc-exp" else 0
                    score += 30 if info["autocomplete"] not in ("cc-exp-month", "cc-exp-year") and any(word in haystack for word in ("exp", "expiry", "expiration")) else 0
                elif field == "exp_month":
                    score += 100 if info["autocomplete"] == "cc-exp-month" else 0
                    score += 30 if any(word in haystack for word in ("exp-month", "expiry-month", "expiration-month")) else 0
                elif field == "exp_year":
                    score += 100 if info["autocomplete"] == "cc-exp-year" else 0
                    score += 30 if any(word in haystack for word in ("exp-year", "expiry-year", "expiration-year")) else 0
                elif field == "cvv":
                    score += 100 if info["autocomplete"] in ("cc-csc", "cc-cvv") else 0
                    score += 30 if any(word in haystack for word in ("cvv", "cvc", "csc", "security")) else 0
                if score > best_score:
                    best, best_score = element, score
            return best

        field_errors = {
            "username": "username_field_not_supported",
            "password": "password_field_not_supported",
            "otp": "otp_field_not_supported",
            "card_number": "card_number_field_not_supported",
            "exp": "expiration_field_not_supported",
            "exp_month": "expiration_fields_not_supported",
            "exp_year": "expiration_fields_not_supported",
            "cvv": "cvv_field_not_supported",
        }

        def retain_fields(specs):
            retained, form = [], None
            for field, selected in specs:
                element = selected if selected is not None else choose(field)
                if element is None:
                    stop("auth_required", field_errors[field])
                handle = element.element_handle()
                if handle is None:
                    stop("auth_required", field_errors[field])
                if submit:
                    if not handle.evaluate("element => !!element.form"):
                        stop("auth_required", field_errors[field])
                    candidate_form = handle.evaluate_handle("element => element.form")
                    if form is None:
                        form = candidate_form
                    elif not handle.evaluate("(element, expected) => element.form === expected", form):
                        stop("auth_required", "ambiguous_field_form")
                retained.append((field, handle))
            if submit and form is None:
                stop("auth_required", "ambiguous_field_form")
            return retained, form

        def submitter_path(submitter):
            return submitter.evaluate("""submitter => {
                const form = submitter.form;
                if (!form) return null;
                const base = new URL(document.baseURI);
                const formAction = new URL(
                    (() => {
                        const raw = form.getAttribute("action");
                        return raw === null || raw === "" ? document.baseURI : raw;
                    })(),
                    document.baseURI
                );
                const submitterAction = new URL(
                    (() => {
                        const raw = submitter.getAttribute("formaction");
                        if (raw === null) return formAction.href;
                        return raw === "" ? document.baseURI : raw;
                    })(),
                    document.baseURI
                );
                const target = submitter.getAttribute("formtarget")
                    ?? form.getAttribute("target")
                    ?? document.querySelector("base[target]")?.getAttribute("target")
                    ?? "";
                return {
                    base_origin: base.origin,
                    form_origin: formAction.origin,
                    submitter_origin: submitterAction.origin,
                    target: target.toLowerCase()
                };
            }""")

        def validate_submitter(submitter, form):
            origin = page.evaluate("location.origin")
            if origin not in allowed:
                stop("rejected", "origin_mismatch")
            if not submitter.evaluate("(element, expected) => element.form === expected", form):
                stop("auth_required", "submit_control_not_supported")
            if not submitter.is_visible() or submitter.is_disabled():
                stop("auth_required", "submit_control_not_supported")
            path = submitter_path(submitter)
            if path is None:
                stop("auth_required", "submit_control_not_supported")
            if any(path[key] != origin for key in
                   ("base_origin", "form_origin", "submitter_origin")):
                stop("rejected", "cross_origin_form_action")
            if path["target"] not in ("", "_self"):
                stop("rejected", "cross_context_form_target")

        def find_submitter(form):
            candidates = page.locator("button, input")
            for index in range(candidates.count()):
                element = candidates.nth(index)
                tag = element.evaluate("element => element.tagName.toLowerCase()")
                control_type = (element.get_attribute("type") or "").lower()
                if tag == "button":
                    # HTML defaults an omitted or empty button type to submit.
                    if control_type not in ("", "submit"):
                        continue
                elif tag == "input":
                    if control_type not in ("submit", "image"):
                        continue
                else:
                    continue
                if not element.is_visible() or element.is_disabled():
                    continue
                handle = element.element_handle()
                if handle is not None and handle.evaluate(
                    "(element, expected) => element.form === expected", form
                ):
                    return handle
            return None

        verify_page()
        if kind == "vault":
            specs = [("username", None), ("password", None)]
            if payload.get("otp") is not None:
                specs.append(("otp", None))
        else:
            expiration = choose("exp")
            if expiration is not None:
                specs = [("card_number", None), ("exp", expiration), ("cvv", None)]
            else:
                specs = [
                    ("card_number", None),
                    ("exp_month", None),
                    ("exp_year", None),
                    ("cvv", None),
                ]
        retained, form = retain_fields(specs)

        submitter = None
        if submit:
            submitter = find_submitter(form)
            if submitter is None:
                stop("auth_required", "submit_control_not_supported")
            # Validate the exact retained submitter and its effective destination
            # before any secret is written to the page.
            validate_submitter(submitter, form)

        navigation_guard = NavigationGuard(page, allowed)
        mutation_started = True
        try:
            for field, handle in retained:
                handle.evaluate("element => element.setAttribute('data-reach-sensitive', '')")
                handle.fill(payload.get(field, ""))

            navigation_guard.check()
            if not submit:
                stop("filled", "fields_filled")
            verify_page()
            # Revalidate the same retained form and submitter after filling. Never
            # resolve a new locator here: a DOM replacement must fail closed.
            validate_submitter(submitter, form)
            submitter.click()
            navigation_guard.check()
            stop("submitted", "submit_dispatched", True)
        finally:
            navigation_guard.close()
except SystemExit:
    pass
except Exception:
    # Never expose exception text: browser errors can contain page data.
    emit("uncertain" if mutation_started else "auth_required", "helper_error")
"#,
);

fn exact_origins(grant: &LeaseGrant, domain: &str) -> Result<Vec<String>> {
    let host = vault::normalize_domain(domain);
    if host.is_empty() {
        bail!("injection domain must not be empty");
    }
    let origins = grant
        .origins
        .iter()
        .filter_map(|candidate| {
            let parsed = url::Url::parse(candidate).ok()?;
            // Canonicalize only for matching the vault domain. Keep the
            // original exact origin (including a leading www.) in the
            // allow-list used by the browser helper.
            let candidate_host = vault::normalize_domain(parsed.host_str()?);
            (candidate_host == host).then(|| parsed.origin().ascii_serialization())
        })
        .collect::<Vec<_>>();
    if origins.is_empty() {
        bail!("domain is outside this lease's exact allowed origins");
    }
    Ok(origins)
}

fn make_payload(
    request: &InjectionRequest,
    screen: u32,
    secret: serde_json::Value,
    origins: &[String],
) -> Result<Vec<u8>> {
    let mut payload = json!({
        "kind": request.kind,
        "domain": vault::normalize_domain(&request.domain),
        "screen": screen,
        "submit": request.submit,
        "allowed_origins": origins,
    });
    let object = payload
        .as_object_mut()
        .context("injection payload is not an object")?;
    let secrets = secret
        .as_object()
        .context("injection secret payload is not an object")?;
    object.extend(
        secrets
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    Ok(serde_json::to_vec(&payload)?)
}

async fn run_helper(
    docker: &DockerClient,
    target: &str,
    payload: Vec<u8>,
) -> Result<HelperOutcome> {
    let command = vec![
        "python3".into(),
        "-c".into(),
        INJECTION_HELPER_SOURCE.into(),
    ];
    let output = docker.exec_input(target, &command, &payload).await?;
    if output.exit_code != 0 {
        bail!("injection helper exited with status {}", output.exit_code);
    }
    let line = output
        .stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with('{') && line.ends_with('}'))
        .context("injection helper returned no control outcome")?;
    let outcome: HelperOutcome = serde_json::from_str(line)
        .context("injection helper returned malformed control outcome")?;
    if outcome.status == "uncertain" {
        bail!("injection outcome requires reconciliation");
    }
    if !matches!(
        outcome.status.as_str(),
        "filled" | "submitted" | "auth_required" | "rejected"
    ) {
        bail!("injection helper returned unknown control outcome");
    }
    Ok(outcome)
}

pub async fn inject(
    docker: &DockerClient,
    target: &str,
    screen: u32,
    grant: &LeaseGrant,
    request: &InjectionRequest,
) -> Result<InjectionReceipt> {
    if grant.account.as_deref().is_none_or(str::is_empty) {
        bail!("secret injection requires an explicit leased account");
    }
    let domain = vault::normalize_domain(&request.domain);
    let origins = exact_origins(grant, &domain)?;
    let (secret, mut card_engine, card_id) = match request.kind.as_str() {
        "vault" => {
            if request.card_id.is_some() {
                bail!("card_id is only valid for card injection");
            }
            let path: PathBuf = grant
                .vault_path
                .clone()
                .context("vault injection requires an explicit vault path")?;
            if path.as_os_str().is_empty() {
                bail!("vault injection requires a non-empty vault path");
            }
            let data = Vault::new(path)
                .load_data()
                .context("failed to load native vault")?;
            let credential = data
                .credentials
                .get(&domain)
                .cloned()
                .context("no credentials found for requested domain")?;
            let otp = credential
                .totp_secret
                .as_deref()
                .map(|secret| {
                    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                    vault::generate_totp_from_secret(secret, now).map_err(anyhow::Error::from)
                })
                .transpose()?;
            (
                json!({"username": credential.username, "password": credential.password, "otp": otp}),
                None,
                None,
            )
        }
        "card" => {
            let requested_id = request
                .card_id
                .as_deref()
                .context("card injection requires card_id")?;
            let path = grant
                .cards_path
                .clone()
                .context("card injection requires an explicit cards path")?;
            if path.as_os_str().is_empty() {
                bail!("card injection requires a non-empty cards path");
            }
            let mut engine = AgentCardEngine::new(Some(path));
            let card = engine.get_card(requested_id)?;
            if vault::normalize_domain(&card.merchant) != domain {
                bail!("card merchant does not match requested domain");
            }
            // Persist reservation before sending any card bytes to the container.
            let reserved =
                engine.reserve_for_injection(requested_id, Some(&format!("https://{domain}")))?;
            (
                json!({
                    "card_number": reserved.card_number,
                    "exp": format!("{}/{}", reserved.exp_month, reserved.exp_year),
                    "exp_month": reserved.exp_month,
                    "exp_year": reserved.exp_year,
                    "cvv": reserved.cvv,
                }),
                Some(engine),
                Some(requested_id.to_owned()),
            )
        }
        _ => bail!("injection kind must be exactly 'vault' or 'card'"),
    };
    let payload = make_payload(request, screen, secret, &origins)?;
    let helper = run_helper(docker, target, payload).await?;
    if let (Some(mut engine), Some(card_id)) = (card_engine.take(), card_id.as_deref()) {
        engine.finalize_injection(card_id)?;
    }
    let (outcome, submitted) = match helper.status.as_str() {
        "filled" => ("fields_filled", false),
        "submitted" => ("submit_dispatched", true),
        "auth_required" => ("auth_required", false),
        "rejected" => ("rejected", false),
        _ => unreachable!("run_helper validates status"),
    };
    Ok(InjectionReceipt {
        status: helper.status,
        outcome: outcome.into(),
        submitted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_does_not_retain_secret_data() {
        let receipt = InjectionReceipt {
            status: "submitted".into(),
            outcome: "submit_dispatched".into(),
            submitted: true,
        };
        let encoded = serde_json::to_string(&receipt).unwrap();
        for secret in [
            "alice",
            "s3cret-password",
            "123456",
            "4111222233334444",
            "123",
        ] {
            assert!(!encoded.contains(secret));
        }
    }

    #[test]
    fn exact_origin_rejects_other_hosts_and_subdomains() {
        let mut grant = LeaseGrant::clean();
        grant.origins.insert("https://shop.example".into());
        grant.origins.insert("https://www.example.com".into());
        assert_eq!(
            exact_origins(&grant, "www.example.com").unwrap(),
            ["https://www.example.com"]
        );
        assert_eq!(
            exact_origins(&grant, "shop.example").unwrap(),
            ["https://shop.example"]
        );
        assert!(exact_origins(&grant, "evil.example").is_err());
        assert!(exact_origins(&grant, "sub.shop.example").is_err());
    }

    #[test]
    fn request_defaults_submit_and_rejects_unknown_fields() {
        let request: InjectionRequest =
            serde_json::from_str(r#"{"kind":"vault","domain":"example.com"}"#).unwrap();
        assert!(!request.submit);
        assert!(
            serde_json::from_str::<InjectionRequest>(
                r#"{"kind":"vault","domain":"example.com","unexpected":true}"#
            )
            .is_err()
        );
    }
}
