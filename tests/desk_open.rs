//! Desk requests exercise the actual CLI, socket, and terminal loop.
mod support;
use margin::desk::{self, Request, Server};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};
use support::{pty::Pty, repo::TestRepo};
const WAIT: Duration = Duration::from_secs(10);
fn command(repo: &TestRepo, socket: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_margin"))
        .args(args)
        .current_dir(repo.path())
        .envs(repo.env().vars())
        .env("MARGIN_DESK_SOCKET", socket)
        .output()
        .unwrap()
}
fn request(pty: &mut Pty, repo: &TestRepo, socket: &Path, args: &[&str]) -> std::process::Output {
    std::thread::scope(|scope| {
        let child = scope.spawn(|| command(repo, socket, args));
        while !child.is_finished() {
            pty.pump(Duration::from_millis(20));
        }
        child.join().unwrap()
    })
}
fn viewer(repo: &TestRepo, socket: &Path) -> Pty {
    let mut vars: Vec<_> = repo
        .env()
        .vars()
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    vars.push(("MARGIN_DESK_SOCKET".into(), socket.display().to_string()));
    Pty::spawn(
        Path::new(env!("CARGO_BIN_EXE_margin")),
        &["tui", "--reduced-updates"],
        &vars,
        repo.path(),
        110,
        32,
    )
    .unwrap()
}
#[test]
fn cli_changes_existing_viewer_after_draw_and_preserves_repository() {
    let repo = TestRepo::init();
    repo.write_file("content/a.md", b"# Alpha\nfirst document\n");
    repo.write_file(
        "content/space λ.md",
        b"# Beta\nsecond document\nthird passage\n",
    );
    repo.write_file("outside.md", b"# Outside\n");
    repo.write_file("content/no.txt", b"unsupported\n");
    repo.commit_all("import");
    let dir = desk::directory().unwrap();
    let socket = dir.join("open.sock");
    let init = command(&repo, &socket, &["init", "--scope", "content/", "--json"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stdout)
    );
    let before = repo.git_ok(["show-ref"]);
    let status = repo.git_ok(["status", "--porcelain=v1"]);
    let mut pty = viewer(&repo, &socket);
    // Send immediately: the client must bridge viewer startup.
    let result = request(
        &mut pty,
        &repo,
        &socket,
        &["open", "content/space λ.md", "--line", "3", "--json"],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    assert!(pty.wait_for("third passage", WAIT));
    for path in [
        "content/missing.md",
        "content/no.txt",
        "outside.md",
        "../escape.md",
    ] {
        let result = request(&mut pty, &repo, &socket, &["open", path, "--json"]);
        assert!(!result.status.success(), "{path}");
    }
    std::os::unix::fs::symlink(
        repo.path().join("outside.md"),
        repo.path().join("content/link.md"),
    )
    .unwrap();
    assert!(
        !request(
            &mut pty,
            &repo,
            &socket,
            &["open", "content/link.md", "--json"]
        )
        .status
        .success()
    );
    std::fs::remove_file(repo.path().join("content/link.md")).unwrap();
    pty.send(b"/");
    pty.pump(Duration::from_millis(200));
    let result = request(
        &mut pty,
        &repo,
        &socket,
        &["open", "content/a.md", "--json"],
    );
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("busy"));
    pty.send(b"\x1b");
    pty.pump(Duration::from_millis(100));
    assert!(
        request(
            &mut pty,
            &repo,
            &socket,
            &["open", "content/a.md", "--json"]
        )
        .status
        .success()
    );
    assert!(pty.wait_for("first document", WAIT));
    assert_eq!(repo.git_ok(["show-ref"]), before);
    assert_eq!(repo.git_ok(["status", "--porcelain=v1"]), status);
    pty.send(b"q");
    assert!(pty.wait_exit(WAIT).unwrap().success());
    assert!(!socket.exists());
    assert!(!dir.exists());
    let start = Instant::now();
    assert!(
        !command(
            &repo,
            &socket,
            &["--timeout-ms", "100", "open", "content/a.md", "--json"]
        )
        .status
        .success()
    );
    assert!(start.elapsed() < Duration::from_secs(2));
}
#[test]
fn transport_bounds_invalid_busy_unresponsive_and_expired_requests() {
    let dir = desk::directory().unwrap();
    let socket = dir.join("open.sock");
    let server = Server::bind(socket.clone()).unwrap();
    for bytes in [
        b"not json".to_vec(),
        vec![b'x'; 9000],
        br#"{"path":"a.md","line":null,"extra":true}"#.to_vec(),
    ] {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        stream.write_all(&bytes).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert!(reply.contains("invalid"), "{reply}");
    }
    server.set_busy(true);
    let start = Instant::now();
    let result = desk::open_at(
        &socket,
        Request {
            path: "a.md".into(),
            line: None,
        },
        WAIT,
    )
    .unwrap_err();
    assert!(result.message.contains("busy"), "{}", result.message);
    assert!(start.elapsed() < Duration::from_secs(1));
    server.set_busy(false);
    let result = desk::open_at(
        &socket,
        Request {
            path: "a.md".into(),
            line: None,
        },
        WAIT,
    )
    .unwrap_err();
    assert!(
        result.message.contains("unresponsive"),
        "{}",
        result.message
    );
    assert!(server.requests.recv_timeout(WAIT).unwrap().deadline <= Instant::now());
    drop(server);
    assert!(!dir.exists());
}
#[test]
fn links_carry_the_desk_and_round_trip_literal_paths() {
    let request = Request {
        path: "content/space λ;$(no).md".into(),
        line: Some(42),
    };
    let first = desk::link(Path::new("/tmp/desk-one/open.sock"), &request).unwrap();
    let second = desk::link(Path::new("/tmp/desk-two/open.sock"), &request).unwrap();
    assert_ne!(first, second);
    let (socket, decoded) = desk::parse_link(&first).unwrap();
    assert_eq!(socket, Path::new("/tmp/desk-one/open.sock"));
    assert_eq!(decoded.path, request.path);
    assert_eq!(decoded.line, Some(42));
    for url in [
        "margin://open/zz/61/1",
        "https://example.org",
        "margin://open/2f/ff/1",
        "margin://open/2f/61/1/extra",
    ] {
        assert!(desk::parse_link(url).is_err(), "{url}");
    }
}
#[test]
fn two_viewers_and_link_adapter_target_only_the_named_desk() {
    let repo = TestRepo::init();
    repo.write_file("a.md", b"# FIRST\nOriginal text\n");
    repo.write_file("b.md", b"# SECOND\nZZZZZZZZZZZZZZZZZZ\n");
    repo.commit_all("import");
    let one = desk::directory().unwrap().join("open.sock");
    let two = desk::directory().unwrap().join("open.sock");
    let mut p1 = viewer(&repo, &one);
    let mut p2 = viewer(&repo, &two);
    assert!(p1.wait_for("Original text", WAIT));
    assert!(p2.wait_for("Original text", WAIT));
    let url = desk::link(
        &one,
        &Request {
            path: "b.md".into(),
            line: None,
        },
    )
    .unwrap();
    assert!(
        request(&mut p1, &repo, &two, &["open", "--url", &url, "--json"])
            .status
            .success()
    );
    assert!(p1.wait_for("ZZZZZZZZZZZZZZZZZZ", WAIT));
    p2.pump(Duration::from_millis(200));
    assert!(!p2.shows("ZZZZZZZZZZZZZZZZZZ"));
    p1.send(b"q");
    p2.send(b"q");
    assert!(p1.wait_exit(WAIT).unwrap().success());
    assert!(p2.wait_exit(WAIT).unwrap().success());
    assert!(
        !command(
            &repo,
            &two,
            &["--timeout-ms", "100", "open", "--url", &url, "--json"]
        )
        .status
        .success()
    );
}

#[test]
fn external_editor_handoff_keeps_transport_responsive() {
    use std::os::unix::fs::PermissionsExt;
    let repo = TestRepo::init();
    repo.write_file("a.md", b"# First\nOriginal text\n");
    repo.commit_all("import");
    let editor = repo.path().join("editor.sh");
    std::fs::write(&editor, "#!/bin/sh\necho EDITOR-HANDOFF\nread answer\n").unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let socket = desk::directory().unwrap().join("open.sock");
    let argv = serde_json::to_string(&[editor.to_str().unwrap()]).unwrap();
    assert!(
        command(
            &repo,
            &socket,
            &[
                "init",
                "--scope",
                "a.md",
                "--editor-argv-json",
                &argv,
                "--json"
            ]
        )
        .status
        .success()
    );
    let mut pty = viewer(&repo, &socket);
    assert!(pty.wait_for("Original text", WAIT));
    pty.send(b"E");
    assert!(pty.wait_for("EDITOR-HANDOFF", WAIT));
    let started = Instant::now();
    let result = command(&repo, &socket, &["open", "a.md", "--json"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("busy"));
    assert!(started.elapsed() < Duration::from_secs(1));
    pty.send(b"done\n");
    assert!(pty.wait_for("editor returned", WAIT));
    assert!(
        request(&mut pty, &repo, &socket, &["open", "a.md", "--json"])
            .status
            .success()
    );
    pty.send(b"q");
    assert!(pty.wait_exit(WAIT).unwrap().success());
}

#[test]
fn listener_rejects_public_directories_and_incomplete_requests_are_bounded() {
    use std::os::unix::fs::PermissionsExt;
    let dir = desk::directory().unwrap();
    let socket = dir.join("open.sock");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Server::bind(socket.clone()).is_err());
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = Server::bind(socket.clone()).unwrap();
    assert!(
        Server::bind(socket.clone()).is_err(),
        "never unlink an active socket"
    );
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    stream.write_all(b"{\"path\":").unwrap();
    let start = Instant::now();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    assert!(reply.contains("invalid"));
    assert!(start.elapsed() < Duration::from_secs(2));
    drop(server);
}

#[test]
fn cli_link_output_is_explicit_and_missing_viewers_are_clear() {
    let repo = TestRepo::init();
    let socket = desk::directory().unwrap().join("open.sock");
    let output = command(
        &repo,
        &socket,
        &["open", "space λ.md", "--line", "42", "--link", "--json"],
    );
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"]["displayed"], false);
    let (_, request) = desk::parse_link(value["data"]["url"].as_str().unwrap()).unwrap();
    assert_eq!(request.path, "space λ.md");
    assert_eq!(request.line, Some(42));
    let output = command(&repo, &socket, &["open", "space λ.md", "--link"]);
    assert!(output.stdout.starts_with(b"\x1b]8;;margin://open/"));
    assert!(
        !command(
            &repo,
            &socket,
            &["open", "space λ.md", "--line", "0", "--json"]
        )
        .status
        .success()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_margin"))
        .args(["open", "a.md", "--json"])
        .current_dir(repo.path())
        .envs(repo.env().vars())
        .env_remove("MARGIN_DESK_SOCKET")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("margin desk -- AGENT"));
    std::fs::remove_dir(socket.parent().unwrap()).unwrap();
}

#[test]
fn malformed_and_negative_acknowledgments_cannot_claim_success() {
    use std::os::unix::net::UnixListener;
    for bytes in [
        b"{}".as_slice(),
        br#"{"displayed":false,"error":null}"#,
        br#"{"displayed":true,"error":null,"extra":true}"#,
    ] {
        let directory = desk::directory().unwrap();
        let socket = directory.join("open.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                stream.read_to_end(&mut request).unwrap();
                stream.write_all(bytes).unwrap();
            });
            assert!(
                desk::open_at(
                    &socket,
                    Request {
                        path: "a.md".into(),
                        line: None
                    },
                    WAIT
                )
                .is_err()
            );
        });
        drop(listener);
        std::fs::remove_file(socket).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
