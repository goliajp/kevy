//! A cluster made of threads: each node is a listener answering from one
//! shared state, enough of the cluster protocol for the cluster manager's
//! tests. The expected outputs in those tests are redis-cli's (checked
//! against the real binary by bench/cligate.py); this only has to make the
//! server side say what a real cluster says.

#![allow(dead_code)] // each test binary uses a different part

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// One node's state.
#[derive(Clone, Default)]
pub struct Node {
    pub id: String,
    pub port: u16,
    /// Index of the master this node replicates.
    pub master: Option<usize>,
    pub slots: Vec<(u16, u16)>,
    /// `(slot, node index)`.
    pub migrating: Vec<(u16, usize)>,
    pub importing: Vec<(u16, usize)>,
    pub keys: i64,
    /// Slots that hold keys here, whatever the owner.
    pub key_slots: Vec<u16>,
    /// Not listening: connections to it are refused.
    pub down: bool,
    /// Extra lines this node appends to its own CLUSTER NODES.
    pub extra_view: Vec<String>,
    /// Alone: its CLUSTER NODES lists only itself until a MEET.
    pub alone: bool,
    pub epoch: u64,
}

/// A reply to override the default one: `(node, argv) -> RESP bytes`.
pub type Hook = Box<dyn Fn(usize, &[Vec<u8>]) -> Option<Vec<u8>> + Send>;

pub struct State {
    pub nodes: Vec<Node>,
    /// Every command each node received, in order.
    pub log: Vec<(usize, Vec<String>)>,
    pub hook: Option<Hook>,
}

pub type Shared = Arc<Mutex<State>>;

/// `masters` masters splitting the slots evenly, then `replicas` replicas
/// of them in turn. Down nodes are marked afterwards by the test.
pub fn cluster(masters: usize, replicas: usize) -> Shared {
    let mut nodes = Vec::new();
    for i in 0..masters {
        let lo = (16384 * i / masters) as u16;
        let hi = (16384 * (i + 1) / masters - 1) as u16;
        nodes.push(Node { slots: vec![(lo, hi)], ..Node::default() });
    }
    for i in 0..replicas {
        nodes.push(Node { master: Some(i % masters.max(1)), ..Node::default() });
    }
    for (i, n) in nodes.iter_mut().enumerate() {
        n.id = format!("{:040x}", 0xa000 + i);
    }
    Arc::new(Mutex::new(State { nodes, log: Vec::new(), hook: None }))
}

/// `count` fresh nodes: alone, no slots, no keys.
pub fn fresh(count: usize) -> Shared {
    let shared = cluster(0, 0);
    shared.lock().unwrap().nodes = (0..count)
        .map(|i| Node { id: format!("{:040x}", 0xa000 + i), alone: true, ..Node::default() })
        .collect();
    shared
}

/// Give every node a port and start the ones not marked down.
pub fn start(shared: &Shared) -> Vec<u16> {
    let count = shared.lock().unwrap().nodes.len();
    let mut ports = Vec::new();
    for i in 0..count {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        ports.push(port);
        let down = {
            let mut st = shared.lock().unwrap();
            st.nodes[i].port = port;
            st.nodes[i].down
        };
        if down {
            continue; // dropped: the port refuses
        }
        let shared = shared.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let shared = shared.clone();
                std::thread::spawn(move || serve(shared, i, conn));
            }
        });
    }
    ports
}

fn serve(shared: Shared, node: usize, mut conn: TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        while let Ok(Some((argv, used))) = kevy_resp::parse_command(&buf) {
            buf.drain(..used);
            let argv: Vec<Vec<u8>> = argv.iter().map(<[u8]>::to_vec).collect();
            let Some(reply) = answer(&shared, node, &argv) else { return };
            if conn.write_all(&reply).is_err() {
                return;
            }
        }
        match conn.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// `None` closes the connection.
fn answer(shared: &Shared, node: usize, argv: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut st = shared.lock().unwrap();
    let words: Vec<String> = argv.iter().map(|a| String::from_utf8_lossy(a).into_owned()).collect();
    st.log.push((node, words.clone()));
    if let Some(reply) = st.hook.as_ref().and_then(|h| h(node, argv)) {
        return (!reply.is_empty()).then_some(reply);
    }
    let upper: Vec<String> = words.iter().map(|w| w.to_ascii_uppercase()).collect();
    let up: Vec<&str> = upper.iter().map(String::as_str).collect();
    Some(match up.as_slice() {
        ["PING"] => b"+PONG\r\n".to_vec(),
        ["DBSIZE"] => format!(":{}\r\n", st.nodes[node].keys).into_bytes(),
        ["CLUSTER", "NODES"] => bulk(&nodes_text(&st, node)),
        ["CLUSTER", "COUNTKEYSINSLOT", slot] => {
            let slot: u16 = slot.parse().unwrap_or(0);
            format!(":{}\r\n", u8::from(st.nodes[node].key_slots.contains(&slot))).into_bytes()
        }
        ["CONFIG", "SET", ..] | ["CONFIG", "REWRITE"] => b"+OK\r\n".to_vec(),
        ["CLUSTER", "INFO"] => {
            let known =
                if st.nodes[node].alone { 1 } else { st.nodes.iter().filter(|n| !n.alone).count() };
            bulk(&format!("cluster_state:ok\r\ncluster_known_nodes:{known}\r\n"))
        }
        ["INFO", "KEYSPACE"] => {
            let keys = st.nodes[node].keys;
            bulk(&if keys > 0 {
                format!("# Keyspace\r\ndb0:keys={keys},expires=0\r\n")
            } else {
                "# Keyspace\r\n".into()
            })
        }
        ["CLUSTER", "ADDSLOTS", slots @ ..] => {
            let mut nums: Vec<u16> = slots.iter().filter_map(|s| s.parse().ok()).collect();
            nums.sort_unstable();
            let n = &mut st.nodes[node];
            for s in nums {
                match n.slots.last_mut() {
                    Some((_, hi)) if *hi + 1 == s => *hi = s,
                    _ => n.slots.push((s, s)),
                }
            }
            b"+OK\r\n".to_vec()
        }
        ["CLUSTER", "SET-CONFIG-EPOCH", e] => {
            st.nodes[node].epoch = e.parse().unwrap_or(0);
            b"+OK\r\n".to_vec()
        }
        ["CLUSTER", "MEET", _, port, ..] => {
            let port: u16 = port.parse().unwrap_or(0);
            let Some(other) = st.nodes.iter().position(|n| n.port == port) else {
                return Some(b"-ERR Invalid node address specified\r\n".to_vec());
            };
            st.nodes[node].alone = false;
            st.nodes[other].alone = false;
            b"+OK\r\n".to_vec()
        }
        ["CLUSTER", "REPLICATE", id] => {
            let id = id.to_ascii_lowercase();
            match st.nodes.iter().position(|n| n.id == id) {
                Some(m) => {
                    st.nodes[node].master = Some(m);
                    b"+OK\r\n".to_vec()
                }
                None => format!("-ERR Unknown node {id}\r\n").into_bytes(),
            }
        }
        _ => format!("-ERR unknown command '{}'\r\n", words[0]).into_bytes(),
    })
}

pub fn bulk(text: &str) -> Vec<u8> {
    format!("${}\r\n{text}\r\n", text.len()).into_bytes()
}

/// CLUSTER NODES as `me` answers it.
pub fn nodes_text(st: &State, me: usize) -> String {
    let mut out = String::new();
    for (i, n) in st.nodes.iter().enumerate() {
        if i != me && (st.nodes[me].alone || n.alone) {
            continue;
        }
        let role = if n.master.is_some() { "slave" } else { "master" };
        let flags = if i == me { format!("myself,{role}") } else { role.to_string() };
        let master = n.master.map_or("-".to_string(), |m| st.nodes[m].id.clone());
        let link = if n.down { "disconnected" } else { "connected" };
        out += &format!(
            "{} 127.0.0.1:{}@{} {flags} {master} 0 0 1 {link}",
            n.id,
            n.port,
            n.port as u32 + 10000
        );
        for &(a, b) in &n.slots {
            out += &if a == b { format!(" {a}") } else { format!(" {a}-{b}") };
        }
        if i == me {
            for &(s, to) in &n.migrating {
                out += &format!(" [{s}->-{}]", st.nodes[to].id);
            }
            for &(s, from) in &n.importing {
                out += &format!(" [{s}-<-{}]", st.nodes[from].id);
            }
        }
        out.push('\n');
    }
    for line in &st.nodes[me].extra_view {
        out += line;
        out.push('\n');
    }
    out
}
