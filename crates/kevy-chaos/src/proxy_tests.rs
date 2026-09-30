use super::*;
use std::time::Instant;

/// Echo server: writes back whatever it reads, one thread per connection.
fn spawn_echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut buf = [0u8; 1024];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

/// Echo server that additionally pushes a `+` heartbeat every 20 ms,
/// independent of inbound traffic — server->client bytes keep flowing
/// even when client->server is black-holed.
fn spawn_heartbeat_echo_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut hb = stream.try_clone().unwrap();
            thread::spawn(move || {
                while hb.write_all(b"+").is_ok() {
                    thread::sleep(Duration::from_millis(20));
                }
            });
            let mut echo_wr = stream.try_clone().unwrap();
            thread::spawn(move || {
                let mut buf = [0u8; 1024];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if echo_wr.write_all(&buf[..n]).is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    addr
}

/// Collect whatever arrives on `stream` until `dur` elapses.
/// Requires a short read timeout already set on the stream.
fn read_for(stream: &mut TcpStream, dur: Duration) -> Vec<u8> {
    let deadline = Instant::now() + dur;
    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    while Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => {
            }
            Err(_) => break,
        }
    }
    out
}

/// Assert a read result means "connection dead" (EOF or hard error),
/// never a timeout (which would also match a merely-silent link).
fn assert_conn_dead(result: io::Result<usize>) {
    match result {
        Ok(0) => {}
        Err(e) if e.kind() != io::ErrorKind::WouldBlock && e.kind() != io::ErrorKind::TimedOut => {}
        other => panic!("expected dead connection, got {other:?}"),
    }
}

#[test]
fn passthrough_bytes() {
    let server = spawn_echo_server();
    let proxy = ChaosProxy::spawn("127.0.0.1:0", server).unwrap();
    let mut client = TcpStream::connect(proxy.listen_addr()).unwrap();
    client.write_all(b"hello kevy").unwrap();
    let mut buf = [0u8; 10];
    client.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hello kevy");
}

#[test]
fn cut_kills_existing_and_refuses_new() {
    let server = spawn_echo_server();
    let proxy = ChaosProxy::spawn("127.0.0.1:0", server).unwrap();
    let mut old = TcpStream::connect(proxy.listen_addr()).unwrap();
    old.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    old.read_exact(&mut buf).unwrap();

    proxy.cut();

    // Existing connection: killed (EOF or reset), not merely silent.
    old.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut byte = [0u8; 1];
    assert_conn_dead(old.read(&mut byte));

    // New connection: connect may succeed (accept-then-drop) but the
    // socket is dead — first read sees EOF/reset.
    if let Ok(mut fresh) = TcpStream::connect(proxy.listen_addr()) {
        fresh.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert_conn_dead(fresh.read(&mut byte));
    }
}

#[test]
fn heal_restores_service() {
    let server = spawn_echo_server();
    let proxy = ChaosProxy::spawn("127.0.0.1:0", server).unwrap();
    proxy.cut();
    proxy.heal();
    let mut client = TcpStream::connect(proxy.listen_addr()).unwrap();
    client.write_all(b"back").unwrap();
    let mut buf = [0u8; 4];
    client.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"back");
}

#[test]
fn cut_dir_to_upstream_black_holes_one_way() {
    let server = spawn_heartbeat_echo_server();
    let proxy = ChaosProxy::spawn("127.0.0.1:0", server).unwrap();
    let mut client = TcpStream::connect(proxy.listen_addr()).unwrap();
    client.set_read_timeout(Some(Duration::from_millis(50))).unwrap();

    // Sanity pre-cut: echo and heartbeats both flow.
    client.write_all(b"AB").unwrap();
    let got = read_for(&mut client, Duration::from_millis(300));
    assert!(got.contains(&b'A') && got.contains(&b'B'), "echo before cut: {got:?}");
    assert!(got.contains(&b'+'), "heartbeat before cut: {got:?}");

    proxy.cut_dir(Direction::ToUpstream);
    // Drain anything echoed before the cut took effect.
    let _ = read_for(&mut client, Duration::from_millis(100));

    // client->server black-holed: write succeeds, echo never comes back;
    // server->client stays open: heartbeats keep arriving. Asymmetry.
    client.write_all(b"XY").unwrap();
    let got = read_for(&mut client, Duration::from_millis(400));
    assert!(got.contains(&b'+'), "server->client must stay open, got {got:?}");
    assert!(
        !got.contains(&b'X') && !got.contains(&b'Y'),
        "client->server bytes must be black-holed, got {got:?}"
    );

    // heal(): the SAME connection resumes (black hole discards, not closes).
    proxy.heal();
    client.write_all(b"Z").unwrap();
    let got = read_for(&mut client, Duration::from_millis(500));
    assert!(got.contains(&b'Z'), "echo after heal: {got:?}");
}

#[test]
fn delay_slows_round_trip() {
    let server = spawn_echo_server();
    let proxy = ChaosProxy::spawn("127.0.0.1:0", server).unwrap();
    let mut client = TcpStream::connect(proxy.listen_addr()).unwrap();
    let mut buf = [0u8; 1];

    // Baseline sanity without delay.
    client.write_all(b"a").unwrap();
    client.read_exact(&mut buf).unwrap();

    proxy.delay(Duration::from_millis(150));
    let start = Instant::now();
    client.write_all(b"b").unwrap();
    client.read_exact(&mut buf).unwrap();
    let elapsed = start.elapsed();
    // 150 ms injected in each direction => >= ~300 ms round trip.
    assert!(elapsed >= Duration::from_millis(280), "round trip too fast: {elapsed:?}");
}
