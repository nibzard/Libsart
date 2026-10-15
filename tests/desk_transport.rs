//! Exercise the public CLI against real private viewer transports.
use margin::desk::{Reply, Server, directory};
use std::{
    io::{Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    process::Command,
    thread,
    time::{Duration, Instant},
};

#[test]
fn cli_waits_for_ack_and_desks_are_isolated() {
    let a = directory().unwrap().join("viewer.sock");
    let b = directory().unwrap().join("viewer.sock");
    let first = Server::bind(a.clone()).unwrap();
    let second = Server::bind(b.clone()).unwrap();
    let child = thread::spawn(move || {
        Command::new(env!("CARGO_BIN_EXE_margin"))
            .args(["open", "notes/résumé 文.md", "--line", "42", "--json"])
            .env("MARGIN_DESK_SOCKET", a)
            .output()
            .unwrap()
    });
    let pending = first.requests.recv_timeout(Duration::from_secs(3)).unwrap();
    assert_eq!(pending.request.path, "notes/résumé 文.md");
    assert_eq!(pending.request.line, Some(42));
    assert!(!child.is_finished());
    assert!(second.requests.try_recv().is_err());
    pending.reply.send(Reply { error: None }).unwrap();
    let output = child.join().unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["effects"]["working_tree"], "unchanged");
    second.set_busy(true);
    let start = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_margin"))
        .args(["open", "a.md", "--json"])
        .env("MARGIN_DESK_SOCKET", &b)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("BUSY"));
    assert!(start.elapsed() < Duration::from_secs(3));
    drop(second);
    assert!(!b.exists());
}

#[test]
fn malformed_messages_do_not_break_the_listener_and_unresponsive_is_bounded() {
    let path = directory().unwrap().join("viewer.sock");
    let server = Server::bind(path.clone()).unwrap();
    for bytes in [b"invalid".to_vec(), vec![b'x'; 8193]] {
        let mut socket = UnixStream::connect(&path).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        socket.write_all(&bytes).unwrap();
        let _ = socket.shutdown(Shutdown::Write);
        let mut reply = String::new();
        socket.read_to_string(&mut reply).unwrap();
        assert!(reply.contains("invalid or oversized"));
    }
    let start = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_margin"))
        .args(["open", "a.md", "--json"])
        .env("MARGIN_DESK_SOCKET", &path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(start.elapsed() < Duration::from_secs(4));
    let pending = server.requests.try_recv().unwrap();
    assert!(Instant::now() >= pending.deadline);
}

#[test]
fn startup_races_retry_and_closed_viewers_have_a_bounded_error() {
    let path = directory().unwrap().join("viewer.sock");
    let address = path.clone();
    let child = thread::spawn(move || {
        Command::new(env!("CARGO_BIN_EXE_margin"))
            .args(["open", "a.md", "--json"])
            .env("MARGIN_DESK_SOCKET", address)
            .output()
            .unwrap()
    });
    thread::sleep(Duration::from_millis(100));
    let server = Server::bind(path.clone()).unwrap();
    let pending = server
        .requests
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    pending.reply.send(Reply { error: None }).unwrap();
    assert!(child.join().unwrap().status.success());
    drop(server);
    let start = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_margin"))
        .args(["open", "a.md", "--timeout-ms", "50", "--json"])
        .env("MARGIN_DESK_SOCKET", path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("closed or unavailable"));
    assert!(start.elapsed() < Duration::from_secs(1));
}
