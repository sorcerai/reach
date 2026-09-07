use reach_cli::agent::{AgentState, ScreenPhase};

#[test]
fn owner_labels_cannot_recover_or_release_another_capability() {
    let agent = AgentState::new(1);
    let lease = agent.lease_screen(0, "worker").unwrap();
    assert!(agent.lease_screen(0, "worker").is_err());
    assert!(agent.release_screen(0, "worker", None).is_err());
    assert!(
        agent
            .release_screen(0, "admin", Some("wrong-token"))
            .is_err()
    );
    assert_eq!(agent.lease_token(0).as_deref(), Some(lease.token.as_str()));
    agent
        .release_screen(0, "worker", Some(&lease.token))
        .unwrap();
    assert!(!agent.is_leased(0));
}

#[test]
fn ordinary_release_cannot_eject_an_active_human() {
    let agent = AgentState::new(1);
    let lease = agent.lease_screen(0, "worker").unwrap();
    agent
        .request_takeover(
            0,
            Some("login".into()),
            None,
            agent.lease_token(0).as_deref(),
        )
        .unwrap();
    agent
        .human_connected(0, agent.human_token(0).as_deref())
        .unwrap();
    assert!(agent.release_screen(0, "admin", None).is_err());
    assert!(
        agent
            .release_screen(0, "worker", Some(&lease.token))
            .is_err()
    );
    assert_eq!(agent.phase(0), Some(ScreenPhase::HumanActive));
    assert!(agent.human_token(0).is_some());
}

#[test]
fn admission_cannot_be_reallocated_or_overtake_handoff() {
    let agent = AgentState::new(1);
    let local = agent.begin_tool(0, None, agent.handoff_gen(0)).unwrap();
    assert!(agent.lease_screen(0, "worker").is_err());
    drop(local);
    let lease = agent.lease_screen(0, "worker").unwrap();
    let generation = agent.handoff_gen(0);
    let permit = agent.begin_tool(0, Some(&lease.token), generation).unwrap();
    assert!(agent.begin_tool(0, Some(&lease.token), generation).is_err());
    permit
        .request_takeover(Some("login".into()), None, Some(&lease.token))
        .unwrap();
    drop(permit);
    assert_eq!(agent.phase(0), Some(ScreenPhase::HandoffPending));
    assert!(agent.begin_tool(0, Some(&lease.token), generation).is_err());
}
