//! `CLIENT LIST`'s filters on a sharded server: every shard renders its
//! own connections, and the filter must reach each of them.

use std::io::Write;

use super::pipeline_order::{Server, read_reply, req};

fn rows(reply: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(reply)
        .lines()
        .filter(|l| l.contains("id="))
        .map(str::to_owned)
        .collect()
}

#[test]
fn list_filters_by_type_and_id_on_every_shard() {
    let srv = Server::start();
    let mut subscribers = Vec::new();
    for i in 0..5 {
        let mut c = srv.connect();
        if i % 2 == 1 {
            c.write_all(&req(&[b"HELLO", b"3"])).unwrap();
            read_reply(&mut c);
        }
        let ch = format!("ch{i}");
        c.write_all(&req(&[b"SUBSCRIBE", ch.as_bytes()])).unwrap();
        read_reply(&mut c);
        subscribers.push(c);
    }
    let _plain: Vec<_> = (0..3).map(|_| srv.connect()).collect();
    let mut c = srv.connect();
    let mut call = |parts: &[&[u8]]| {
        c.write_all(&req(parts)).unwrap();
        read_reply(&mut c)
    };
    let id = String::from_utf8(call(&[b"CLIENT", b"ID"])).unwrap();
    let id = id.trim_start_matches(':').trim_end().to_string();

    let pubsub = rows(&call(&[b"CLIENT", b"LIST", b"TYPE", b"pubsub"]));
    assert_eq!(pubsub.len(), 5, "{pubsub:?}");
    assert!(pubsub.iter().all(|r| r.contains(" flags=P ")), "{pubsub:?}");
    let normal = rows(&call(&[b"CLIENT", b"LIST", b"TYPE", b"Normal"]));
    assert_eq!(normal.len(), 4, "{normal:?}");
    assert!(normal.iter().all(|r| r.contains(" flags=N ")), "{normal:?}");
    assert_eq!(rows(&call(&[b"CLIENT", b"LIST"])).len(), 9);
    let mine = rows(&call(&[b"CLIENT", b"LIST", b"ID", id.as_bytes(), b"0", b"-4"]));
    assert_eq!(mine.len(), 1, "{mine:?}");
    assert!(mine[0].starts_with(&format!("id={id} ")), "{mine:?}");
    assert_eq!(call(&[b"CLIENT", b"LIST", b"TYPE", b"replica"]), b"$0\r\n\r\n");
    assert_eq!(call(&[b"CLIENT", b"LIST", b"TYPE", b"x"]), b"-ERR Unknown client type 'x'\r\n");
    assert_eq!(call(&[b"CLIENT", b"LIST", b"ID", b"1", b"y"]), b"-ERR Invalid client ID\r\n");
}
