use reach_cli::agent::{AgentState, ScreenPhase, TakeoverError, WaitError};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn test_screen_phase_lifecycle_and_generation_counter() {
    let agent = AgentState::new(3);

    // Initial state: Idle, gen 1
    for i in 0..3 {
        let info = agent.screen_info(i).expect("screen should exist");
        assert_eq!(info.phase, ScreenPhase::Idle);
        assert_eq!(info.handoff_gen, 1);
        assert!(!info.takeover_pending);
        assert_eq!(info.takeover_reason, None);
        assert_eq!(info.takeover_url, None);
    }

    // Lease screen 1 -> AgentActive, gen 1
    let lease = agent
        .lease_screen(1, "worker-1")
        .expect("lease should succeed");
    assert_eq!(lease.id, 1);
    assert_eq!(agent.phase(1), Some(ScreenPhase::AgentActive));
    assert_eq!(agent.handoff_gen(1), Some(1));

    // Request takeover -> HandoffPending, gen 2
    let s = agent
        .request_takeover(
            1,
            Some("Solve Cloudflare Turnstile".into()),
            Some("http://localhost:6081/vnc.html".into()),
        )
        .expect("takeover request should succeed");
    assert_eq!(s.phase, ScreenPhase::HandoffPending);
    assert_eq!(s.handoff_gen, 2);
    assert!(s.takeover_pending);
    assert_eq!(
        s.takeover_reason.as_deref(),
        Some("Solve Cloudflare Turnstile")
    );
    assert!(
        s.takeover_url
            .as_deref()
            .unwrap()
            .starts_with("http://localhost:6081/vnc.html")
    );

    // Human connects -> HumanActive, gen 2
    let s = agent
        .human_connected(1)
        .expect("human_connected should succeed");
    assert_eq!(s.phase, ScreenPhase::HumanActive);
    assert_eq!(s.handoff_gen, 2);

    // Human hands back -> HumanDone, gen 3
    let s = agent
        .human_handback(1)
        .expect("human_handback should succeed");
    assert_eq!(s.phase, ScreenPhase::HumanDone);
    assert_eq!(s.handoff_gen, 3);

    // Agent acks -> AgentActive, gen 4, takeover cleared
    let s = agent.agent_ack(1).expect("agent_ack should succeed");
    assert_eq!(s.phase, ScreenPhase::AgentActive);
    assert_eq!(s.handoff_gen, 4);
    assert!(!s.takeover_pending);
    assert_eq!(s.takeover_reason, None);
    assert_eq!(s.takeover_url, None);

    // Second takeover cycle increments generation further
    let s = agent
        .request_takeover(1, Some("Solve 2FA".into()), None)
        .expect("second takeover request should succeed");
    assert_eq!(s.phase, ScreenPhase::HandoffPending);
    assert_eq!(s.handoff_gen, 5);

    let s = agent
        .human_handback(1)
        .expect("direct handback should succeed");
    assert_eq!(s.phase, ScreenPhase::HumanDone);
    assert_eq!(s.handoff_gen, 6);

    let s = agent.agent_ack(1).expect("agent_ack should succeed");
    assert_eq!(s.phase, ScreenPhase::AgentActive);
    assert_eq!(s.handoff_gen, 7);
}

#[test]
fn test_invalid_phase_transitions_rejected() {
    let agent = AgentState::new(1);
    let _ = agent.lease_screen(0, "bot").unwrap();

    // Cannot human_connected from AgentActive
    assert!(matches!(
        agent.human_connected(0),
        Err(TakeoverError::InvalidPhase { .. })
    ));

    // Cannot human_handback from AgentActive
    assert!(matches!(
        agent.human_handback(0),
        Err(TakeoverError::InvalidPhase { .. })
    ));

    // Cannot agent_ack from AgentActive
    assert!(matches!(
        agent.agent_ack(0),
        Err(TakeoverError::InvalidPhase { .. })
    ));

    // Move to HandoffPending
    agent
        .request_takeover(0, Some("reason".into()), None)
        .unwrap();

    // Cannot request takeover again when already HandoffPending
    assert!(matches!(
        agent.request_takeover(0, Some("another".into()), None),
        Err(TakeoverError::InvalidPhase { .. })
    ));

    // Move to HumanActive
    agent.human_connected(0).unwrap();

    // Cannot request takeover from HumanActive
    assert!(matches!(
        agent.request_takeover(0, None, None),
        Err(TakeoverError::InvalidPhase { .. })
    ));

    // Move to HumanDone
    agent.human_handback(0).unwrap();

    // Cannot human_connected from HumanDone
    assert!(matches!(
        agent.human_connected(0),
        Err(TakeoverError::InvalidPhase { .. })
    ));
}

#[tokio::test]
async fn test_long_poll_wait_for_phase() {
    let agent = Arc::new(AgentState::new(1));
    let _ = agent.lease_screen(0, "bot").unwrap();
    agent
        .request_takeover(0, Some("captcha".into()), None)
        .unwrap();
    agent.human_connected(0).unwrap();

    // Concurrent task: waits for HumanDone
    let agent_clone = Arc::clone(&agent);
    let waiter = tokio::spawn(async move {
        agent_clone
            .wait_for_phase(0, ScreenPhase::HumanDone, Duration::from_secs(3))
            .await
    });

    // Simulate human taking action and handing back after 50ms
    tokio::time::sleep(Duration::from_millis(50)).await;
    agent.human_handback(0).unwrap();

    let result = waiter
        .await
        .expect("task join failed")
        .expect("wait failed");
    assert_eq!(result.phase, ScreenPhase::HumanDone);
    assert_eq!(result.handoff_gen, 3);

    // Timeout test
    let timeout_err = agent
        .wait_for_phase(0, ScreenPhase::Idle, Duration::from_millis(20))
        .await
        .expect_err("should have timed out");
    assert!(matches!(timeout_err, WaitError::Timeout { .. }));
}

#[test]
fn test_agent_cannot_eject_human_in_human_active() {
    let agent = AgentState::new(1);
    let _ = agent.lease_screen(0, "bot").unwrap();

    // 1. In HandoffPending, agent CAN cancel takeover
    agent
        .request_takeover(0, Some("need auth".into()), None)
        .unwrap();
    assert_eq!(agent.phase(0), Some(ScreenPhase::HandoffPending));
    let initial_gen = agent.handoff_gen(0).unwrap();

    let cancelled = agent.set_takeover(0, false, None).unwrap();
    assert_eq!(cancelled.phase, ScreenPhase::AgentActive);
    assert!(!cancelled.takeover_pending);
    assert_eq!(cancelled.handoff_gen, initial_gen + 1);

    // 2. Request takeover again and transition to HumanActive
    agent
        .request_takeover(0, Some("need auth".into()), None)
        .unwrap();
    agent.human_connected(0).unwrap();
    assert_eq!(agent.phase(0), Some(ScreenPhase::HumanActive));

    // 3. In HumanActive, agent CANNOT cancel takeover or force-eject human
    let eject_err = agent
        .set_takeover(0, false, None)
        .expect_err("agent must not be able to eject human in HumanActive");
    assert!(matches!(eject_err, TakeoverError::InvalidPhase { .. }));

    // Cannot agent_ack either
    let ack_err = agent.agent_ack(0).expect_err("cannot ack in HumanActive");
    assert!(matches!(ack_err, TakeoverError::InvalidPhase { .. }));

    // Only human_handback transitions out of HumanActive
    let handed_back = agent.human_handback(0).unwrap();
    assert_eq!(handed_back.phase, ScreenPhase::HumanDone);
}

#[test]
fn test_human_token_minting_and_verification() {
    let agent = AgentState::new(1);
    let _ = agent.lease_screen(0, "bot").unwrap();

    let base_url = "http://127.0.0.1:6080/vnc.html?autoconnect=1";
    let state = agent
        .request_takeover(0, Some("login".into()), Some(base_url.into()))
        .unwrap();

    let token = state
        .human_token
        .expect("request_takeover must mint human_token");
    assert!(!token.is_empty());

    let takeover_url = state.takeover_url.expect("takeover_url should be set");
    assert!(
        takeover_url.contains(&format!("token={}", token)),
        "takeover_url must include minted token"
    );

    // Verification succeeds with correct token
    assert!(agent.verify_human_token(0, &token));
    assert!(agent.has_human_token(&token));

    // Verification fails with bogus token
    assert!(!agent.verify_human_token(0, "bogus-token"));
    assert!(!agent.has_human_token("bogus-token"));

    // Complete handoff cycle
    agent.human_connected(0).unwrap();
    agent.human_handback(0).unwrap();
    agent.agent_ack(0).unwrap();

    // After ack, human token is cleared
    assert_eq!(agent.human_token(0), None);
    assert!(!agent.has_human_token(&token));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_takeover_busy_drain_and_timeout() {
    let agent = Arc::new(AgentState::new(1));
    let _ = agent.lease_screen(0, "bot").unwrap();

    // Mark screen busy (in-flight tool)
    agent.inc_busy(0);
    assert_eq!(agent.busy_count(0), 1);

    // Drain finishes in background after 50ms
    let agent_clone = Arc::clone(&agent);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        agent_clone.dec_busy(0);
    });

    let drained = agent.wait_for_drain(0, Duration::from_millis(500)).await;
    assert!(drained, "wait_for_drain should succeed once busy reaches 0");

    // Mark screen busy again and test drain timeout
    agent.inc_busy(0);
    let timed_out = !agent.wait_for_drain(0, Duration::from_millis(50)).await;
    assert!(
        timed_out,
        "wait_for_drain should time out if busy remains > 0"
    );
    agent.dec_busy(0);
}
