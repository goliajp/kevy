//! Phase 3 / T3.10 e2e: an embed-as-writer's replication source
//! listener serves real `kevy_replicate::ReplicaClient` subscribers,
//! and every commit on the embed shows up on the wire in offset
//! order.

#![cfg(not(target_arch = "wasm32"))]

use std::net::TcpListener;
use std::time::{Duration, Instant};

use kevy_embedded::{Config, Store};
use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};

/// Reserve one free port + return its address. Single-port flavour
/// of the existing test harness — the embed writer doesn't need a
/// block of consecutive ports the way the server replication tests
/// do.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn wait_for<F: FnMut() -> bool>(timeout: Duration, mut predicate: F) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn embed_writer_streams_committed_argvs_to_replica_client() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = Config::default().with_embed_writer(&addr);
    let writer = Store::open(cfg).unwrap();

    // Apply two writes BEFORE the subscriber connects so the
    // backlog has frames waiting at offset 0.
    writer.set(b"k1", b"v1").unwrap();
    writer.set(b"k2", b"v2").unwrap();

    // Subscribe at offset 0.
    let mut client = ReplicaClient::connect(addr.as_str(), "test-sub-1", 0)
        .expect("ReplicaClient should connect to the embed writer's listener");

    // Read two frames; embed wrote both via `set` so the wire shape
    // is `*3\r\n$3\r\nSET\r\n$2\r\nk1\r\n$2\r\nv1\r\n` for each.
    let frame_a = next_frame(&mut client, Duration::from_secs(2));
    let frame_b = next_frame(&mut client, Duration::from_secs(2));

    assert_eq!(frame_a.offset, 0);
    assert_eq!(frame_b.offset, 1);
    assert_eq!(argv_to_vecvec(&frame_a.argv), vec![b"SET".to_vec(), b"k1".to_vec(), b"v1".to_vec()]);
    assert_eq!(argv_to_vecvec(&frame_b.argv), vec![b"SET".to_vec(), b"k2".to_vec(), b"v2".to_vec()]);

    // A live write after the subscriber is caught up also flows
    // through.
    writer.set(b"live", b"yes").unwrap();
    let frame_c = next_frame(&mut client, Duration::from_secs(2));
    assert_eq!(frame_c.offset, 2);
    assert_eq!(argv_to_vecvec(&frame_c.argv), vec![b"SET".to_vec(), b"live".to_vec(), b"yes".to_vec()]);

    drop(client);
    drop(writer);
}

#[test]
fn embed_writer_serves_multiple_subscribers_independently() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let cfg = Config::default().with_embed_writer(&addr);
    let writer = Store::open(cfg).unwrap();
    writer.set(b"shared", b"v").unwrap();

    let mut a = ReplicaClient::connect(addr.as_str(), "sub-a", 0).unwrap();
    let mut b = ReplicaClient::connect(addr.as_str(), "sub-b", 0).unwrap();

    let fa = next_frame(&mut a, Duration::from_secs(2));
    let fb = next_frame(&mut b, Duration::from_secs(2));
    // Both subscribers see the same offset-0 frame.
    assert_eq!(fa.offset, 0);
    assert_eq!(fb.offset, 0);
    assert_eq!(argv_to_vecvec(&fa.argv), argv_to_vecvec(&fb.argv));

    drop(a);
    drop(b);
    drop(writer);
}

#[test]
fn two_embed_writers_distinct_scopes_both_visible_to_subscribers() {
    // T3.16 e2e — two embed-as-writer stores own disjoint key
    // prefixes; two subscribers (one per writer) each see their
    // own writer's keyspace. Validates that two replication
    // source listeners in one process don't interfere with each
    // other (separate `ReplicaSource` instances + separate
    // accept loops).
    let port_a = free_port();
    let port_b = free_port();
    let writer_a = Store::open(Config::default().with_embed_writer(format!("127.0.0.1:{port_a}"))).unwrap();
    let writer_b = Store::open(Config::default().with_embed_writer(format!("127.0.0.1:{port_b}"))).unwrap();

    // Pre-fill disjoint scopes.
    writer_a.set(b"app:billing:1", b"a-bill-1").unwrap();
    writer_a.set(b"app:billing:2", b"a-bill-2").unwrap();
    writer_b.set(b"app:auth:1", b"b-auth-1").unwrap();

    // Each subscriber connects to ONE writer.
    let mut sub_a = ReplicaClient::connect(
        format!("127.0.0.1:{port_a}").as_str(),
        "sub-of-a",
        0,
    ).unwrap();
    let mut sub_b = ReplicaClient::connect(
        format!("127.0.0.1:{port_b}").as_str(),
        "sub-of-b",
        0,
    ).unwrap();

    // sub_a sees A's two frames in order; sub_b sees B's one.
    let a_f1 = next_frame(&mut sub_a, Duration::from_secs(2));
    let a_f2 = next_frame(&mut sub_a, Duration::from_secs(2));
    let b_f1 = next_frame(&mut sub_b, Duration::from_secs(2));

    assert_eq!(a_f1.offset, 0);
    assert_eq!(a_f2.offset, 1);
    assert_eq!(b_f1.offset, 0);

    let a_argv1 = argv_to_vecvec(&a_f1.argv);
    let a_argv2 = argv_to_vecvec(&a_f2.argv);
    let b_argv1 = argv_to_vecvec(&b_f1.argv);

    assert_eq!(a_argv1[1], b"app:billing:1");
    assert_eq!(a_argv2[1], b"app:billing:2");
    assert_eq!(b_argv1[1], b"app:auth:1");

    // Live writes flow independently.
    writer_a.set(b"app:billing:3", b"a-live").unwrap();
    writer_b.set(b"app:auth:2", b"b-live").unwrap();

    let a_live = next_frame(&mut sub_a, Duration::from_secs(2));
    let b_live = next_frame(&mut sub_b, Duration::from_secs(2));
    assert_eq!(argv_to_vecvec(&a_live.argv)[1], b"app:billing:3");
    assert_eq!(argv_to_vecvec(&b_live.argv)[1], b"app:auth:2");

    drop(sub_a);
    drop(sub_b);
    drop(writer_a);
    drop(writer_b);
}

#[test]
fn embed_writer_local_writes_are_not_readonly() {
    // Sanity: the writer is NOT in replica mode, so local writes
    // succeed (READONLY enforcement is Phase-2 / open_replica only).
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let writer = Store::open(Config::default().with_embed_writer(&addr)).unwrap();
    assert!(!writer.is_replica());
    writer.set(b"k", b"v").unwrap();
    assert_eq!(writer.get(b"k").unwrap().as_deref(), Some(b"v".as_slice()));
    // Wait briefly to let the writer's accept thread bind so drop
    // is clean.
    assert!(wait_for(Duration::from_millis(500), || true));
}

fn next_frame(
    client: &mut ReplicaClient,
    timeout: Duration,
) -> kevy_replicate::replica::DecodedFrame {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match client.next_event() {
            Some(Ok(ReplicaEvent::Frame(f))) => return f,
            Some(Ok(_)) => continue,
            Some(Err(e)) => panic!("ReplicaClient error: {e}"),
            None => panic!("ReplicaClient EOF before next frame"),
        }
    }
    panic!("timed out waiting for next frame");
}

fn argv_to_vecvec(argv: &kevy_persist::Argv) -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    for i in 0..argv.len() {
        if let Some(part) = argv.get(i) {
            v.push(part.to_vec());
        }
    }
    v
}
