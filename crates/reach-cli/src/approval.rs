use sha2::{Digest, Sha256};
use std::fmt::Write;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingApproval {
    pub digest: String,
    pub request_key: String,
    pub tool: String,
    pub arguments: serde_json::Value,
    pub approved: bool,
    pub expires: Instant,
}

impl PendingApproval {
    pub fn new(
        lease_token: &str,
        grant: &crate::lease::LeaseGrant,
        handoff: u64,
        observation: u64,
        tool: &str,
        args: &serde_json::Value,
    ) -> Self {
        let encoded = serde_json::to_vec(&serde_json::json!({
            "lease": lease_token, "task": grant.task_id, "attempt": grant.attempt_id,
            "incarnation": grant.incarnation, "handoff": handoff, "observation": observation,
            "grant": grant,
            "tool": tool, "arguments": args,
        }))
        .expect("JSON values serialize");
        let request_key = hex_digest(&encoded);
        let mut proposal = encoded;
        proposal.extend_from_slice(uuid::Uuid::new_v4().as_bytes());
        let digest = hex_digest(&proposal);
        Self {
            digest,
            request_key,
            tool: tool.into(),
            arguments: args.clone(),
            approved: false,
            expires: Instant::now() + Duration::from_secs(300),
        }
    }
    pub fn current(&self) -> bool {
        Instant::now() < self.expires
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut digest = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut digest, "{byte:02x}").expect("string writing cannot fail");
    }
    digest
}

pub fn requires_approval(tool: &str, args: &serde_json::Value, account: bool) -> bool {
    matches!(
        tool,
        "click" | "type" | "key" | "exec" | "playwright_eval" | "inject"
    ) || (account
        && args
            .get("url")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()))
}
