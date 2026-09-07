use super::*;

#[test]
fn observer_filter_drops_fragmented_keyboard_pointer_and_clipboard_messages() {
    let mut filter = RfbClientFilter::observer_or_control(ViewerMode::Observer);
    establish_none_auth(&mut filter);

    let key = [4, 0, 0, 0, 0, 0, 0, 1];
    let pointer = [5, 0, 0, 0, 0, 1];
    let clipboard = [6, 0, 0, 0, 0, 0, 0, 3, b'x', b'y', b'z'];
    for message in [&key[..], &pointer[..], &clipboard[..]] {
        let mut output = Vec::new();
        for chunk in message.chunks(1) {
            output.extend(filter.feed_client(chunk).unwrap());
        }
        assert!(output.is_empty(), "observer forwarded a control message");
    }

    let framebuffer_request = [3, 0, 0, 0, 0, 0, 0, 1, 0, 0];
    assert_eq!(
        filter.feed_client(&framebuffer_request).unwrap(),
        vec![framebuffer_request.to_vec()]
    );
}

#[test]
fn observer_filter_rejects_unknown_message() {
    let mut filter = RfbClientFilter::observer_or_control(ViewerMode::Observer);
    establish_none_auth(&mut filter);
    assert!(filter.feed_client(&[0xff]).is_err());
}

#[test]
fn filter_parses_fragmented_vnc_authentication_and_forces_shared_client() {
    let mut filter = RfbClientFilter::observer_or_control(ViewerMode::Observer);
    for chunk in b"RFB 003.007\n".chunks(2) {
        filter.observe_server(chunk).unwrap();
    }
    filter.observe_server(&[1, 2]).unwrap();
    for chunk in b"RFB 003.007\n".chunks(3) {
        filter.feed_client(chunk).unwrap();
    }
    filter.feed_client(&[2]).unwrap();
    for chunk in [0x55u8; 16].chunks(1) {
        filter.observe_server(chunk).unwrap();
    }
    for chunk in [0xaau8; 16].chunks(2) {
        filter.feed_client(chunk).unwrap();
    }
    filter.observe_server(&[0, 0, 0, 0]).unwrap();
    assert_eq!(filter.feed_client(&[0]).unwrap(), vec![vec![1]]);
}

#[test]
fn viewer_cookie_is_screen_scoped_and_revoked_with_screen() {
    let sessions = ViewerSessions::default();
    let now = Instant::now();
    let record = ViewerSession {
        id: "sid".to_string(),
        screen: 2,
        mode: ViewerMode::Observer,
        sandbox: "sandbox".to_string(),
        incarnation: "container:started".to_string(),
        novnc_port: 6082,
        lease_token: Some("lease".to_string()),
        handoff_gen: 4,
        human_token: None,
        expires_at: now + SESSION_TTL,
        issued_at: now,
    };
    sessions.insert(record);

    assert_eq!(cookie_path(2), "/viewer/2");
    let header = cookie_header(2, "sid", 300, true);
    let cookie = header.to_str().unwrap();
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
    assert!(cookie.contains("; Secure"));
    assert!(cookie.contains("Path=/viewer/2"));
    assert!(sessions.get(2, "sid").is_some());
    assert!(sessions.get(1, "sid").is_none());

    sessions.revoke_screen(2);
    assert!(sessions.get(2, "sid").is_none());
}

fn establish_none_auth(filter: &mut RfbClientFilter) {
    filter.observe_server(b"RFB 003.008\n").unwrap();
    filter.observe_server(&[1, 1]).unwrap();
    filter.feed_client(b"RFB 003.008\n").unwrap();
    filter.feed_client(&[1]).unwrap();
    filter.observe_server(&[0, 0, 0, 0]).unwrap();
    assert_eq!(filter.feed_client(&[0]).unwrap(), vec![vec![1]]);
}
