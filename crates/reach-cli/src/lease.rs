use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct AccountPolicy {
    pub profile: String,
    pub origins: BTreeSet<String>,
    pub jars: BTreeSet<String>,
    pub jars_path: Option<PathBuf>,
    pub vault_path: Option<PathBuf>,
    pub cards_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LeaseGrant {
    pub account: Option<String>,
    pub profile: String,
    pub origins: BTreeSet<String>,
    pub jars: BTreeSet<String>,
    pub jars_path: Option<PathBuf>,
    pub vault_path: Option<PathBuf>,
    pub task_id: String,
    pub cards_path: Option<PathBuf>,
    pub attempt_id: String,
    pub incarnation: String,
    pub allow_exec: bool,
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
}

pub fn origin(value: &str) -> Result<String, &'static str> {
    let url = url::Url::parse(value).map_err(|_| "invalid URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("only HTTP(S) URLs without embedded credentials are permitted");
    }
    Ok(url.origin().ascii_serialization())
}

impl LeaseGrant {
    pub fn clean() -> Self {
        Self {
            account: None,
            profile: format!("lease-{}", uuid::Uuid::new_v4()),
            jars_path: None,
            vault_path: None,
            cards_path: None,
            origins: BTreeSet::new(),
            jars: BTreeSet::new(),
            task_id: uuid::Uuid::new_v4().to_string(),
            attempt_id: uuid::Uuid::new_v4().to_string(),
            incarnation: String::new(),
            allow_exec: false,
        }
    }

    pub fn for_account(name: &str, policy: &AccountPolicy) -> Result<Self, &'static str> {
        if !valid_name(name) || !valid_name(&policy.profile) || policy.origins.is_empty() {
            return Err("account requires safe identifiers and explicit origins");
        }
        let mut grant = Self::clean();
        grant.account = Some(name.to_owned());
        grant.profile = policy.profile.clone();
        grant.origins = policy
            .origins
            .iter()
            .map(|s| origin(s))
            .collect::<Result<_, _>>()?;
        for domain in &policy.jars {
            if domain.is_empty()
                || domain.contains('/')
                || domain.contains('\\')
                || !domain
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
                || domain.starts_with('.')
                || domain.ends_with('.')
                || domain.contains("..")
            {
                return Err("jar grants require exact canonical hostnames");
            }
            let host = domain.to_ascii_lowercase();
            if !grant.origins.iter().any(|s| {
                url::Url::parse(s)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .as_deref()
                    == Some(host.as_str())
            }) {
                return Err("jar host is outside the account origins");
            }
            grant.jars.insert(host);
        }
        if !grant.jars.is_empty() && policy.jars_path.is_none() {
            return Err("account jars require an explicit private jars_path");
        }
        grant.jars_path = policy.jars_path.clone();
        grant.vault_path = policy.vault_path.clone();
        grant.cards_path = policy.cards_path.clone();
        Ok(grant)
    }

    pub fn permits_origin(&self, value: &str) -> bool {
        origin(value).is_ok_and(|o| self.origins.is_empty() || self.origins.contains(&o))
    }

    pub fn permits_url(&self, value: &str) -> bool {
        self.permits_origin(value)
    }

    pub fn authorize(&self, tool: &str, args: &mut serde_json::Value) -> Result<(), &'static str> {
        let object = args
            .as_object_mut()
            .ok_or("tool arguments must be an object")?;
        if matches!(tool, "exec" | "playwright_eval")
            && (!self.allow_exec || self.account.is_some())
        {
            return Err("this lease does not grant arbitrary code execution");
        }
        if tool == "launch" {
            return Err("leased sessions launch only the scoped browser through browse");
        }
        for key in [
            "storage_state",
            "user_data_dir",
            "account",
            "vault_path",
            "jars_path",
        ] {
            if object.contains_key(key) {
                return Err("model arguments cannot supply private state or authority");
            }
        }
        for key in ["use_profile", "profile"] {
            if let Some(value) = object.get(key)
                && value.as_str() != Some(self.profile.as_str())
            {
                return Err("profile is outside this lease grant");
            }
        }
        if let Some(value) = object.get("url") {
            let value = value.as_str().ok_or("URL must be a string")?;
            if !value.is_empty() && !self.permits_url(value) {
                return Err("URL is outside this lease's allowed origins");
            }
        }
        if let Some(value) = object.get("jars") {
            let requested: Vec<&str> = if let Some(s) = value.as_str() {
                s.split(',').map(str::trim).collect()
            } else if let Some(items) = value.as_array() {
                items
                    .iter()
                    .map(|v| v.as_str().ok_or("jar names must be strings"))
                    .collect::<Result<_, _>>()?
            } else {
                return Err("jars must be a list of granted hostnames");
            };
            if requested.iter().any(|v| !self.jars.contains(*v)) {
                return Err("cookie jar is outside this lease grant");
            }
        }
        if matches!(tool, "browse" | "page_text" | "auth_handoff") {
            object.insert("use_profile".into(), self.profile.clone().into());
            object.remove("profile");
            object.remove("ephemeral");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_grants_reject_wrong_origins_profiles_and_code() {
        let policy = AccountPolicy {
            profile: "work-account".into(),
            origins: BTreeSet::from(["https://example.com".into()]),
            ..Default::default()
        };
        let grant = LeaseGrant::for_account("work", &policy).unwrap();
        for (tool, mut args) in [
            (
                "browse",
                serde_json::json!({"url":"https://example.com.attacker.invalid"}),
            ),
            ("browse", serde_json::json!({"use_profile":"personal"})),
            ("page_text", serde_json::json!({"jars":["other.invalid"]})),
            ("playwright_eval", serde_json::json!({"script":"pass"})),
        ] {
            assert!(grant.authorize(tool, &mut args).is_err());
        }
        let mut allowed = serde_json::json!({"url":"https://example.com/login"});
        grant.authorize("browse", &mut allowed).unwrap();
        assert_eq!(allowed["use_profile"], "work-account");
    }
    #[test]
    fn account_grants_match_only_the_native_current_origin() {
        let policy = AccountPolicy {
            profile: "work-account".into(),
            origins: BTreeSet::from(["https://example.com".into()]),
            ..Default::default()
        };
        let grant = LeaseGrant::for_account("work", &policy).unwrap();

        assert!(grant.permits_origin("https://example.com"));
        assert!(grant.permits_origin("https://example.com/login"));
        assert!(!grant.permits_origin("https://example.com.attacker.invalid"));
        assert!(!grant.permits_origin("about:blank"));
        assert!(!grant.permits_origin(""));
    }
    #[test]
    fn clean_lease_cannot_select_persistent_state_or_local_files() {
        let grant = LeaseGrant::clean();
        assert!(
            grant
                .authorize(
                    "browse",
                    &mut serde_json::json!({"url":"file:///home/sandbox/profile/Cookies"})
                )
                .is_err()
        );
        assert!(
            grant
                .authorize(
                    "page_text",
                    &mut serde_json::json!({"storage_state":"/private/state.json"})
                )
                .is_err()
        );
        assert_ne!(grant.profile, LeaseGrant::clean().profile);
    }
}
