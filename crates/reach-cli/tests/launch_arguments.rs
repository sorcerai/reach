#![cfg(unix)]
use reach_cli::tools::browse_command_input;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::{
    fs,
    net::TcpListener,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn browser_profile_stays_literal_and_url_never_enters_process_arguments() {
    let dir = std::env::temp_dir().join(format!("reach-launch-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let launcher = dir.join("reach-chrome");
    fs::write(
        &launcher,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$REACH_TEST_ARGS\"\n",
    )
    .unwrap();
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700)).unwrap();
    let profile = dir.join("profile' ; touch PWNED ; #");
    let profile = profile.to_str().unwrap();
    let url = "https://example.invalid/a'b?x=$(touch URL_PWNED)&u=雪";
    let capture = dir.join("args");
    let unavailable_cdp = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = unavailable_cdp.local_addr().unwrap().port();
    let (command, payload) = browse_command_input(url, profile, None, Some(port), ":99", None);

    // Request data is carried by stdin, never interpolated into the fixed
    // helper command or exposed as a process argument.
    assert!(
        command
            .iter()
            .all(|arg| !arg.contains(profile) && !arg.contains(url)),
        "private browser payload leaked into command argv"
    );
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .current_dir(&dir)
        .env(
            "PATH",
            format!("{}:{}", dir.display(), std::env::var("PATH").unwrap()),
        )
        .env("REACH_TEST_ARGS", &capture)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .expect("browser helper stdin unavailable")
        .write_all(&payload)
        .unwrap();
    let status = child.wait().unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while !capture.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !dir.join("PWNED").exists(),
        "profile escaped into shell code"
    );
    assert!(
        !dir.join("URL_PWNED").exists(),
        "URL escaped into shell code"
    );
    assert!(
        !status.success(),
        "launching a process without a live browser must not report successful navigation"
    );
    let args = fs::read_to_string(&capture).expect("browser was launched");
    assert!(
        args.lines()
            .any(|arg| arg == format!("--user-data-dir={profile}"))
    );
    assert!(
        !args.contains(url),
        "navigation URL leaked into browser argv"
    );
    fs::remove_dir_all(dir).unwrap();
}
