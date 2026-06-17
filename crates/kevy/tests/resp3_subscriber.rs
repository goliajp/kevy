//! `Subscriber::hello3` round-trip — a kevy-client Subscriber opts into
//! RESP3 against a real kevy server, then receives subscribe/message
//! frames as RESP3 push frames (`>N\r\n…`). The Reply parser auto-
//! decodes RESP3 prefixes (P1); `classify` accepts both `Reply::Array`
//! and `Reply::Push` (P5). End-to-end: user code doesn't change shape
//! to opt into RESP3, just adds `sub.hello3()?` before `subscribe`.

use kevy_client::{PubsubEvent, Subscriber};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static START_GATE: Mutex<()> = Mutex::new(());

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct Server {
    port: u16,
    dir: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let _gate = START_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let port = free_port();
        let dir = std::env::temp_dir().join(format!(
            "kevy-client-resp3-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let dir_thread = dir.clone();
        let handle = std::thread::spawn(move || {
            let rt = kevy_rt::Runtime::new([127, 0, 0, 1], port, 1, kevy::KevyCommands)
                .with_data_dir(dir_thread);
            rt.run(stop_thread).unwrap();
        });
        for _ in 0..200 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Self { port, dir, stop, handle: Some(handle) }
    }

    fn url(&self) -> String {
        format!("kevy://127.0.0.1:{}", self.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn hello3_then_subscribe_recv_push_frames() {
    let srv = Server::start();

    // V3-aware subscriber: connect, HELLO 3, then SUBSCRIBE.
    let mut sub = Subscriber::connect(&srv.url()).unwrap();
    sub.hello3().unwrap();
    sub.subscribe(&[b"news"]).unwrap();
    sub.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // The SUBSCRIBE ack on a V3 conn arrives as a push frame; classify
    // accepts Push transparently and returns the same Subscribe variant.
    let ack = sub.recv().unwrap();
    assert!(
        matches!(ack, PubsubEvent::Subscribe { count: 1, .. }),
        "expected Subscribe ack via push frame, got {ack:?}"
    );

    // Publisher (separate conn, regular RESP2 — proves V3 + V2 mix
    // works server-side, P4).
    let mut pubconn = kevy_client::Connection::open(&srv.url()).unwrap();
    let n = pubconn.publish(b"news", b"hello").unwrap();
    assert_eq!(n, 1);

    let msg = sub.recv().unwrap();
    assert_eq!(
        msg,
        PubsubEvent::Message {
            channel: b"news".to_vec(),
            payload: b"hello".to_vec(),
        },
        "expected Message via push frame, got the wrong variant"
    );
}

#[test]
fn hello3_then_psubscribe_recv_pmessage_push() {
    let srv = Server::start();

    let mut sub = Subscriber::connect(&srv.url()).unwrap();
    sub.hello3().unwrap();
    sub.psubscribe(&[b"news.*"]).unwrap();
    sub.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let ack = sub.recv().unwrap();
    assert!(matches!(ack, PubsubEvent::Psubscribe { count: 1, .. }));

    let mut pubconn = kevy_client::Connection::open(&srv.url()).unwrap();
    let _ = pubconn.publish(b"news.tech", b"hi").unwrap();

    let pmsg = sub.recv().unwrap();
    assert_eq!(
        pmsg,
        PubsubEvent::Pmessage {
            pattern: b"news.*".to_vec(),
            channel: b"news.tech".to_vec(),
            payload: b"hi".to_vec(),
        }
    );
}

#[test]
fn hello3_on_embedded_subscriber_returns_unsupported() {
    let mut sub = Subscriber::open("mem://hello3-embed", &[b"chan"]).unwrap();
    let err = sub.hello3().unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::Unsupported);
}
