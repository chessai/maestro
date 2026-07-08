//! Integration tests for the daemon socket server (ADR-006, AC4/AC5).
//!
//! These drive the server in-process: they set the XDG env vars to a unique
//! temp dir so `maestro_journal::paths::*` resolve there, start the server on a
//! background thread, and speak the one-line-per-connection JSON protocol.
//!
//! NOTE: `paths::*` read process-global env vars, and these tests mutate them,
//! so the whole socket exercise lives in ONE `#[test]` to avoid cross-test env
//! races. Pure config-resolution unit tests live in `src/resolve.rs`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use maestro_daemon::{Options, Server};
use maestro_journal::proto::{Request, Response, PROTOCOL_VERSION};

/// Create a unique temp directory for this test run.
fn unique_tmp() -> PathBuf {
    let base = std::env::temp_dir();
    let name = format!(
        "maestro-daemon-test-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos() as u64 ^ (Instant::now().elapsed().as_nanos() as u64).rotate_left(17)
    );
    let dir = base.join(name);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Send one request line, read one response line, deserialize.
fn round_trip(socket: &Path, req: &Request) -> Response {
    let mut stream = UnixStream::connect(socket).expect("connect to daemon socket");
    let mut line = serde_json::to_string(req).expect("serialize request");
    line.push('\n');
    stream.write_all(line.as_bytes()).expect("write request");
    stream.flush().expect("flush request");

    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader.read_line(&mut buf).expect("read response");
    serde_json::from_str(buf.trim_end()).expect("deserialize response")
}

/// Send a raw (possibly malformed) line, read the raw response line back.
fn round_trip_raw(socket: &Path, raw: &str) -> String {
    let mut stream = UnixStream::connect(socket).expect("connect");
    stream.write_all(raw.as_bytes()).expect("write");
    stream.flush().expect("flush");
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader.read_line(&mut buf).expect("read");
    buf
}

/// Poll until the socket accepts a connection or the deadline passes.
fn wait_for_socket(socket: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        if Instant::now() >= deadline {
            panic!("daemon socket never became connectable at {}", socket.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn server_serves_hello_doctor_ps_and_survives_malformed() {
    let tmp = unique_tmp();
    // Isolate all path resolution into the temp dir.
    std::env::set_var("XDG_RUNTIME_DIR", &tmp);
    std::env::set_var("XDG_DATA_HOME", &tmp);
    std::env::set_var("XDG_CONFIG_HOME", &tmp);
    std::env::set_var("XDG_STATE_HOME", &tmp);
    // Ensure no ambient profile leaks in.
    std::env::remove_var("MAESTRO_PROFILE");

    // Start the server (opens+migrates the journal, binds the socket). Do NOT
    // detach — the test process must keep its session.
    let server = Server::start(Options {
        profile: None,
        detach: false,
    })
    .expect("server starts");
    let socket = server.socket_path().to_path_buf();
    let shutdown = server.shutdown_handle();

    // Serve on a background thread until we flip the shutdown flag.
    let handle = std::thread::spawn(move || {
        server.serve_until().expect("serve loop");
    });

    wait_for_socket(&socket, Duration::from_secs(5));

    // --- AC4: Hello → protocol_version == PROTOCOL_VERSION ---
    match round_trip(&socket, &Request::Hello) {
        Response::Hello { protocol_version, pid } => {
            assert_eq!(protocol_version, PROTOCOL_VERSION, "handshake version");
            assert_eq!(pid, std::process::id(), "pid is the daemon's own");
        }
        other => panic!("expected Hello response, got {other:?}"),
    }

    // --- AC4: Doctor → non-empty profile, probe.os == "linux" ---
    match round_trip(&socket, &Request::Doctor) {
        Response::Doctor(report) => {
            assert!(!report.profile.is_empty(), "profile name is non-empty");
            // No config file present → implicit "default".
            assert_eq!(report.profile, "default");
            assert_eq!(
                report.probe.get("os").and_then(|v| v.as_str()),
                Some("linux"),
                "probe.os is linux"
            );
            // resolved_profile is the defaults-only merged view (no error).
            assert!(
                report.resolved_profile.get("error").is_none(),
                "no config file must not be an error: {:?}",
                report.resolved_profile
            );
            assert!(
                report.resolved_profile.get("watchdog_minutes").is_some(),
                "resolved profile carries defaults"
            );
        }
        other => panic!("expected Doctor response, got {other:?}"),
    }

    // --- AC4: Ps → empty task list on a fresh DB ---
    match round_trip(&socket, &Request::Ps) {
        Response::Ps { tasks } => {
            assert!(tasks.is_empty(), "fresh DB has no tasks, got {tasks:?}");
        }
        other => panic!("expected Ps response, got {other:?}"),
    }

    // --- AC5: malformed line → Response::Error, server stays up ---
    let raw = round_trip_raw(&socket, "this is not json\n");
    let resp: Response = serde_json::from_str(raw.trim_end()).expect("error response deserializes");
    match resp {
        Response::Error { message } => {
            assert!(message.contains("malformed"), "error message: {message}");
        }
        other => panic!("expected Error response for malformed input, got {other:?}"),
    }

    // Server survives: a subsequent valid Hello on a NEW connection works.
    match round_trip(&socket, &Request::Hello) {
        Response::Hello { protocol_version, .. } => {
            assert_eq!(protocol_version, PROTOCOL_VERSION, "server still up after malformed input");
        }
        other => panic!("expected Hello after malformed input, got {other:?}"),
    }

    // Shut the server down and confirm the socket file is removed.
    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
    assert!(!socket.exists(), "socket file removed on clean shutdown");

    // --- AC6: search-backend resolution from `search.backend` ---------------
    // These reuse the same isolated XDG temp dir (env stays set from above), so
    // they live in this one test to avoid cross-test env races (see module doc).
    // We write a config with two profiles and start a fresh server per profile
    // (a named profile is a per-server `--profile` flag), issue `Search`, and
    // assert the resolved backend. No live web_search call is made.
    let config_dir = tmp.join("maestro");
    std::fs::create_dir_all(&config_dir).expect("create config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        r#"
[defaults]

[profiles.nosearch]
search.backend = "none"

[profiles.searx]
search.backend = "searxng"
"#,
    )
    .expect("write config");

    // `search.backend = "none"` → a loud backend_unavailable, no backend built.
    assert_search_backend_unavailable("nosearch");
    // `search.backend = "searxng"` with no endpoint → backend_unavailable via
    // the SearXNG path (the executor's unset-endpoint error).
    assert_search_backend_unavailable("searx");

    let _ = std::fs::remove_dir_all(&tmp);
}

/// Start a server bound to the isolated temp dir under `--profile <profile>`,
/// register an advisor, issue a `Search`, and assert the reply is a
/// `backend_unavailable` `Response::Error`. Tears the server down before
/// returning so the next profile can rebind the socket.
fn assert_search_backend_unavailable(profile: &str) {
    let server = Server::start(Options {
        profile: Some(profile.to_string()),
        detach: false,
    })
    .expect("server starts");
    let socket = server.socket_path().to_path_buf();
    let shutdown = server.shutdown_handle();
    let handle = std::thread::spawn(move || {
        server.serve_until().expect("serve loop");
    });
    wait_for_socket(&socket, Duration::from_secs(5));

    let advisor_session_id = match round_trip(
        &socket,
        &Request::RegisterAdvisor {
            profile: Some(profile.to_string()),
        },
    ) {
        Response::RegisterAdvisor { advisor_session_id } => advisor_session_id,
        other => panic!("expected RegisterAdvisor, got {other:?}"),
    };

    let resp = round_trip(
        &socket,
        &Request::Search {
            advisor_session_id,
            queries: vec!["rust ownership".to_string()],
        },
    );
    match resp {
        Response::Error { message } => {
            assert!(
                message.contains("backend_unavailable"),
                "profile {profile}: expected backend_unavailable, got: {message}"
            );
        }
        other => panic!("profile {profile}: expected Error, got {other:?}"),
    }

    shutdown.store(true, Ordering::SeqCst);
    handle.join().expect("server thread joins");
}
