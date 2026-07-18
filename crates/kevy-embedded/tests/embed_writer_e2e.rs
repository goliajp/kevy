//! E2e: an embed-as-writer's replication source
//! listener serves real `kevy_replicate::ReplicaClient` subscribers,
//! and every commit on the embed shows up on the wire in offset
//! order.

#![cfg(not(target_arch = "wasm32"))]

use std::time::{Duration, Instant};

use kevy_embedded::{Config, Store};
use kevy_replicate::replica::{ReplicaClient, ReplicaEvent};

/// Open an embed writer on an OS-assigned ephemeral port and read the
/// real address back — race-free, unlike the old bind-probe-release
/// `free_port()` (whose window let a parallel test steal the port:
/// AddrInUse flakes under covgate's instrumented, slowed runs).
fn open_writer(cfg: Config) -> (Store, String) {
    let store = Store::open(cfg.with_embed_writer("127.0.0.1:0")).unwrap();
    let addr = store.writer_addr().expect("writer listener bound").to_string();
    (store, addr)
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
    let (writer, addr) = open_writer(Config::default());

    // Apply two writes BEFORE the subscriber connects. Snapshot
    // semantics (snapshot.md): offset 0 against non-empty history =
    // SNAPSHOT ship (keyspace + as-of offset), then live frames —
    // NOT a frame replay from 0.
    writer.set(b"k1", b"v1").unwrap();
    writer.set(b"k2", b"v2").unwrap();

    // Subscribe at offset 0 → snapshot path.
    let mut client = ReplicaClient::connect(addr.as_str(), "test-sub-1", 0)
        .expect("ReplicaClient should connect to the embed writer's listener");

    let (payload, ack) = drain_snapshot(&mut client);
    assert_eq!(ack, 2, "as-of offset covers both pre-connect writes");
    assert!(!payload.is_empty(), "snapshot carries the keyspace");

    // A live write after the ship flows as a frame at the ack offset.
    writer.set(b"live", b"yes").unwrap();
    let frame_c = next_frame(&mut client, Duration::from_secs(2));
    assert_eq!(frame_c.offset, 2);
    assert_eq!(argv_to_vecvec(&frame_c.argv), vec![b"SET".to_vec(), b"live".to_vec(), b"yes".to_vec()]);

    drop(client);
    drop(writer);
}

#[test]
fn embed_writer_serves_multiple_subscribers_independently() {
    let (writer, addr) = open_writer(Config::default());
    writer.set(b"shared", b"v").unwrap();

    // Offset 0 vs history → each subscriber gets its own
    // snapshot ship, then live frames.
    let mut a = ReplicaClient::connect(addr.as_str(), "sub-a", 0).unwrap();
    let mut b = ReplicaClient::connect(addr.as_str(), "sub-b", 0).unwrap();
    let (_, ack_a) = drain_snapshot(&mut a);
    let (_, ack_b) = drain_snapshot(&mut b);
    assert_eq!((ack_a, ack_b), (1, 1));

    writer.set(b"shared2", b"w").unwrap();
    let fa = next_frame(&mut a, Duration::from_secs(2));
    let fb = next_frame(&mut b, Duration::from_secs(2));
    // Both subscribers see the same live frame.
    assert_eq!(fa.offset, 1);
    assert_eq!(fb.offset, 1);
    assert_eq!(argv_to_vecvec(&fa.argv), argv_to_vecvec(&fb.argv));

    drop(a);
    drop(b);
    drop(writer);
}

#[test]
fn two_embed_writers_distinct_scopes_both_visible_to_subscribers() {
    // E2e — two embed-as-writer stores own disjoint key
    // prefixes; two subscribers (one per writer) each see their
    // own writer's keyspace. Validates that two replication
    // source listeners in one process don't interfere with each
    // other (separate `ReplicaSource` instances + separate
    // accept loops).
    let (writer_a, addr_a) = open_writer(Config::default());
    let (writer_b, addr_b) = open_writer(Config::default());

    // Pre-fill disjoint scopes.
    writer_a.set(b"app:billing:1", b"a-bill-1").unwrap();
    writer_a.set(b"app:billing:2", b"a-bill-2").unwrap();
    writer_b.set(b"app:auth:1", b"b-auth-1").unwrap();

    // Each subscriber connects to ONE writer.
    let mut sub_a = ReplicaClient::connect(addr_a.as_str(), "sub-of-a", 0).unwrap();
    let mut sub_b = ReplicaClient::connect(addr_b.as_str(), "sub-of-b", 0).unwrap();

    // Each subscriber receives its own writer's snapshot
    // (pre-fill rides the ship, not frames).
    let (_, ack_a) = drain_snapshot(&mut sub_a);
    let (_, ack_b) = drain_snapshot(&mut sub_b);
    assert_eq!((ack_a, ack_b), (2, 1), "as-of offsets per writer");

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
    let (writer, _addr) = open_writer(Config::default());
    assert!(!writer.is_replica());
    writer.set(b"k", b"v").unwrap();
    assert_eq!(writer.get(b"k").unwrap().as_deref(), Some(b"v".as_slice()));
    // Wait briefly to let the writer's accept thread bind so drop
    // is clean.
    assert!(wait_for(Duration::from_millis(500), || true));
}

/// Consume one snapshot ship: returns (payload, ack_offset).
fn drain_snapshot(client: &mut ReplicaClient) -> (Vec<u8>, u64) {
    let mut payload = Vec::new();
    loop {
        match client.next_event() {
            Some(Ok(ReplicaEvent::SnapshotBegin)) => {}
            Some(Ok(ReplicaEvent::SnapshotChunk(bytes))) => payload.extend_from_slice(&bytes),
            Some(Ok(ReplicaEvent::SnapshotEnd { ack_offset })) => return (payload, ack_offset),
            other => panic!("expected snapshot event, got {other:?}"),
        }
    }
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

/// The T8 offset-aliasing fence: a subscriber resuming with a cursor
/// from a PREVIOUS writer boot must get a snapshot ship, never
/// frame continuity — the restarted writer's in-memory backlog
/// restarted offsets at 0, so "offset 3" now names different data.
/// Pre-fence, a new history that had grown past the old cursor
/// served frames from it silently (missing everything the new boot
/// wrote before that offset).
#[test]
fn writer_restart_generation_fence_ships_instead_of_aliasing() {
    // Boot A: three writes → next_offset 3.
    let (writer_a, addr_a) = open_writer(Config::default());
    for i in 0..3u8 {
        writer_a.set(format!("a:{i}").as_bytes(), b"old").unwrap();
    }
    let mut sub = ReplicaClient::connect(addr_a.as_str(), "sub-restart", 0).unwrap();
    let gen_a = sub.primary_gen_at_handshake();
    assert_ne!(gen_a, 0, "writer must advertise a real generation");
    let (_payload, ack_a) = drain_snapshot(&mut sub);
    assert_eq!(ack_a, 3, "boot A as-of offset");
    drop(sub);
    drop(writer_a);

    // Boot B ("restarted writer"): a fresh source, offsets restart at
    // 0, and its history RACES PAST the old cursor (6 > 3) — the
    // exact shape where pre-fence code would serve aliased frames
    // 3..6 and silently skip b:0..b:2.
    let (writer_b, addr_b) = open_writer(Config::default());
    for i in 0..6u8 {
        writer_b.set(format!("b:{i}").as_bytes(), b"new").unwrap();
    }

    // Resume claim from boot A's history: (gen_a, offset 3).
    let mut sub = ReplicaClient::connect_at(
        addr_b.as_str(),
        "sub-restart",
        gen_a,
        3,
        Duration::from_secs(5),
    )
    .unwrap();
    let gen_b = sub.primary_gen_at_handshake();
    assert_ne!(gen_b, gen_a, "each boot mints its own generation");
    // The fence must answer with a FULL snapshot of boot B's
    // keyspace — drain_snapshot panics if a live frame arrives
    // instead (the aliasing failure mode).
    let (payload, ack_b) = drain_snapshot(&mut sub);
    assert_eq!(ack_b, 6, "snapshot covers all of boot B's history");
    let text = String::from_utf8_lossy(&payload).into_owned();
    for i in 0..6u8 {
        assert!(
            text.contains(&format!("b:{i}")),
            "snapshot must carry b:{i} (got no aliased frame gap)"
        );
    }
}
