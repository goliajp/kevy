//! The encrypted client port. Each connection's Noise handshake and
//! crypto run on threads here, and the plaintext goes to this server's own
//! client port over loopback, so the reactors never see ciphertext and the
//! plaintext path is the same code with or without this port.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use kevy_config::Config;
use kevy_noise::{Frames, Keypair, MAX_MESSAGE, Opener, Responder, Sealer, frame};

/// Must match the clients'.
const PROLOGUE: &[u8] = b"kevy-client\x001";
const TAG: usize = 16;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

struct Front {
    local: Keypair,
    client_keys: Vec<[u8; 32]>,
    upstream: SocketAddr,
    token_hex: String,
}

/// The first encrypted cluster port: `[secure] cluster_port_base`, else
/// `listen_port + 1`.
pub(crate) fn secure_cluster_port_base(cfg: &Config) -> u16 {
    match cfg.secure.cluster_port_base {
        0 => cfg.secure.listen_port.saturating_add(1),
        base => base,
    }
}

/// The first encrypted cluster port told to clients:
/// `announce_cluster_port_base` when set, else the one bound.
pub(crate) fn advertised_secure_cluster_base(cfg: &Config) -> u16 {
    match cfg.secure.announce_cluster_port_base {
        0 => secure_cluster_port_base(cfg),
        base => base,
    }
}

/// Every encrypted port and the plaintext port it relays to: the client
/// port, and in cluster mode one per shard beside each cluster port.
pub(crate) fn port_pairs(cfg: &Config) -> Vec<(u16, u16)> {
    let mut pairs = vec![(cfg.secure.listen_port, cfg.server.port)];
    if cfg.cluster.enabled {
        let (secure, plain) = (secure_cluster_port_base(cfg), crate::cluster_port_base(cfg));
        for i in 0..cfg.server.threads.max(1) as u16 {
            pairs.push((secure.saturating_add(i), plain.saturating_add(i)));
        }
    }
    pairs
}

/// Bind the encrypted client port when `[secure] listen_port` is set (and
/// in cluster mode an encrypted twin of every cluster port), and hand the
/// runtime the token its relayed connections present to name their real
/// clients. A port that cannot be bound stops the server.
pub(crate) fn start<C: kevy_rt::Commands>(
    cfg: &Config,
    key: Option<&Keypair>,
    runtime: kevy_rt::Runtime<C>,
) -> kevy_rt::Runtime<C> {
    let (port, Some(local)) = (cfg.secure.listen_port, key) else { return runtime };
    if port == 0 {
        return runtime;
    }
    let mut token = [0u8; 32];
    if let Err(e) = kevy_sys::fill_random(&mut token) {
        exit_on(port, &e);
    }
    let host = match Ipv4Addr::from(cfg.server.bind) {
        a if a.is_unspecified() => Ipv4Addr::LOCALHOST,
        a => a,
    };
    for (listen, upstream) in port_pairs(cfg) {
        let front = Arc::new(Front {
            local: local.clone(),
            client_keys: cfg.secure.client_keys.clone(),
            upstream: SocketAddr::from((host, upstream)),
            token_hex: kevy_config::key_to_hex(&token),
        });
        let listener = TcpListener::bind((Ipv4Addr::from(cfg.server.bind), listen))
            .unwrap_or_else(|e| exit_on(listen, &e));
        let spawned = std::thread::Builder::new()
            .name("kevy-secure-accept".into())
            .spawn(move || accept_loop(&listener, &front));
        if let Err(e) = spawned {
            exit_on(listen, &e);
        }
    }
    let runtime = runtime.with_peer_token(token);
    if cfg.cluster.enabled {
        eprintln!(
            "kevy: encrypted client port on {port}, encrypted cluster ports from {}",
            secure_cluster_port_base(cfg)
        );
        return runtime.with_secure_cluster_announce(advertised_secure_cluster_base(cfg));
    }
    eprintln!("kevy: encrypted client port on {port}");
    runtime
}

fn exit_on(port: u16, e: &io::Error) -> ! {
    eprintln!("kevy: encrypted client port {port}: {e}");
    std::process::exit(1);
}

fn accept_loop(listener: &TcpListener, front: &Arc<Front>) {
    for conn in listener.incoming() {
        let Ok(client) = conn else { continue };
        let front = Arc::clone(front);
        let _ = std::thread::Builder::new()
            .name("kevy-secure-conn".into())
            .spawn(move || serve_conn(client, &front));
    }
}

/// One client: handshake, open the plaintext side, then relay until either
/// end closes. Any failure just closes the connection; a refused client
/// learns nothing beyond that.
fn serve_conn(client: TcpStream, front: &Front) {
    let Ok((client, sealer, opener, leftover)) = handshake(client, front) else { return };
    let Ok(upstream) = open_upstream(&client, front) else {
        let _ = client.shutdown(Shutdown::Both);
        return;
    };
    let (Ok(c2), Ok(u2)) = (client.try_clone(), upstream.try_clone()) else { return };
    let inbound = std::thread::Builder::new()
        .name("kevy-secure-in".into())
        .spawn(move || client_to_upstream(c2, u2, opener, leftover));
    upstream_to_client(upstream, client, sealer);
    if let Ok(t) = inbound {
        let _ = t.join();
    }
}

type Established = (TcpStream, Sealer, Opener, Frames);

fn handshake(mut client: TcpStream, front: &Front) -> io::Result<Established> {
    client.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    client.set_nodelay(true)?;
    let mut frames = Frames::default();
    let m1 = read_message(&mut client, &mut frames)?;
    let mut secret = [0u8; 32];
    kevy_sys::fill_random(&mut secret)?;
    let (_, resp) = Responder::accept(&front.local, Keypair::from_secret(secret), PROLOGUE, &m1)
        .map_err(bad)?;
    let key = resp.remote_static();
    if !front.client_keys.is_empty()
        && !front.client_keys.iter().any(|k| kevy_crypto::ct_eq(k, &key))
    {
        return Err(bad("client key not listed"));
    }
    let (m2, transport) = resp.finish(b"").map_err(bad)?;
    client.write_all(&frame(&m2).map_err(bad)?)?;
    client.set_read_timeout(None)?;
    let (sealer, opener) = transport.split();
    Ok((client, sealer, opener, frames))
}

/// Connect to the plaintext port and name the real client, so `CLIENT
/// LIST` and `CLIENT KILL ADDR` see it rather than this relay.
fn open_upstream(client: &TcpStream, front: &Front) -> io::Result<TcpStream> {
    let mut up = TcpStream::connect(front.upstream)?;
    up.set_nodelay(true)?;
    if let Ok(SocketAddr::V4(peer)) = client.peer_addr() {
        let (tok, addr) = (front.token_hex.as_bytes(), peer.to_string());
        let parts: [&[u8]; 4] = [b"CLIENT", b"SETPEER", tok, addr.as_bytes()];
        let mut req = format!("*{}\r\n", parts.len()).into_bytes();
        for p in parts {
            req.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
            req.extend_from_slice(p);
            req.extend_from_slice(b"\r\n");
        }
        up.write_all(&req)?;
        let mut reply = [0u8; 5];
        up.read_exact(&mut reply)?;
        if &reply != b"+OK\r\n" {
            return Err(bad("the server refused the relay's peer token"));
        }
    }
    Ok(up)
}

fn client_to_upstream(
    mut client: TcpStream,
    mut up: TcpStream,
    mut rx: Opener,
    mut frames: Frames,
) {
    let mut chunk = vec![0u8; 64 * 1024];
    let relayed = (|| -> io::Result<()> {
        loop {
            while let Some(m) = frames.next() {
                up.write_all(&rx.open(&m).map_err(bad)?)?;
            }
            let n = client.read(&mut chunk)?;
            if n == 0 {
                return Ok(());
            }
            frames.push(&chunk[..n]);
        }
    })();
    drop(relayed);
    let _ = up.shutdown(Shutdown::Both);
    let _ = client.shutdown(Shutdown::Both);
}

fn upstream_to_client(mut up: TcpStream, mut client: TcpStream, mut tx: Sealer) {
    let mut chunk = vec![0u8; MAX_MESSAGE - TAG];
    let relayed = (|| -> io::Result<()> {
        loop {
            let n = up.read(&mut chunk)?;
            if n == 0 {
                return Ok(());
            }
            client.write_all(&frame(&tx.seal(&chunk[..n]).map_err(bad)?).map_err(bad)?)?;
        }
    })();
    drop(relayed);
    let _ = client.shutdown(Shutdown::Both);
    let _ = up.shutdown(Shutdown::Both);
}

fn read_message(sock: &mut TcpStream, frames: &mut Frames) -> io::Result<Vec<u8>> {
    let mut chunk = [0u8; 256];
    loop {
        if let Some(m) = frames.next() {
            return Ok(m);
        }
        let n = sock.read(&mut chunk)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        frames.push(&chunk[..n]);
    }
}

fn bad(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kevy_noise::{Initiator, Transport};

    /// Stands in for the plaintext port: records the SETPEER line, answers
    /// +OK, then echoes everything back.
    fn fake_upstream() -> (SocketAddr, std::sync::mpsc::Receiver<String>) {
        fake_upstream_answering(b"+OK\r\n")
    }

    fn fake_upstream_answering(
        answer: &'static [u8],
    ) -> (SocketAddr, std::sync::mpsc::Receiver<String>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let mut s = s.unwrap();
                let mut head = vec![0u8; 256];
                let n = s.read(&mut head).unwrap();
                tx.send(String::from_utf8_lossy(&head[..n]).into_owned()).unwrap();
                s.write_all(answer).unwrap();
                let mut back = s.try_clone().unwrap();
                std::thread::spawn(move || {
                    let _ = io::copy(&mut s, &mut back);
                });
            }
        });
        (addr, rx)
    }

    fn front(
        client_keys: Vec<[u8; 32]>,
    ) -> (SocketAddr, Keypair, std::sync::mpsc::Receiver<String>) {
        front_over(client_keys, fake_upstream())
    }

    fn front_over(
        client_keys: Vec<[u8; 32]>,
        (upstream, seen): (SocketAddr, std::sync::mpsc::Receiver<String>),
    ) -> (SocketAddr, Keypair, std::sync::mpsc::Receiver<String>) {
        let local = Keypair::from_secret([1; 32]);
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let f = Arc::new(Front {
            local: local.clone(),
            client_keys,
            upstream,
            token_hex: "ab".repeat(32),
        });
        std::thread::spawn(move || accept_loop(&l, &f));
        (addr, local, seen)
    }

    fn connect(
        addr: SocketAddr,
        me: &Keypair,
        server: &Keypair,
    ) -> io::Result<(TcpStream, Transport)> {
        let mut s = TcpStream::connect(addr)?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        let (m1, init) =
            Initiator::start(me, &server.public(), Keypair::from_secret([9; 32]), PROLOGUE, b"")
                .map_err(bad)?;
        s.write_all(&frame(&m1).map_err(bad)?)?;
        let m2 = read_message(&mut s, &mut Frames::default())?;
        let (_, t) = init.finish(&m2).map_err(bad)?;
        Ok((s, t))
    }

    #[test]
    fn a_client_is_relayed_in_both_directions_and_named_upstream() {
        let (addr, server, seen) = front(Vec::new());
        let (mut s, mut t) = connect(addr, &Keypair::from_secret([2; 32]), &server).unwrap();
        let setpeer = seen.recv_timeout(Duration::from_secs(2)).unwrap();
        let local = s.local_addr().unwrap().to_string();
        assert!(setpeer.contains("SETPEER") && setpeer.contains(&"ab".repeat(32)), "{setpeer}");
        assert!(setpeer.contains(&local), "{setpeer} names {local}");
        // larger than one Noise message each way
        let big: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        for chunk in big.chunks(MAX_MESSAGE - TAG) {
            s.write_all(&frame(&t.seal(chunk).unwrap()).unwrap()).unwrap();
        }
        let (mut frames, mut back) = (Frames::default(), Vec::new());
        while back.len() < big.len() {
            back.extend(t.open(&read_message(&mut s, &mut frames).unwrap()).unwrap());
        }
        assert_eq!(back, big);
    }

    #[test]
    fn an_unlisted_client_key_and_a_wrong_server_key_get_no_session() {
        let listed = Keypair::from_secret([2; 32]);
        let (addr, server, seen) = front(vec![listed.public()]);
        assert!(connect(addr, &Keypair::from_secret([3; 32]), &server).is_err());
        assert!(connect(addr, &listed, &Keypair::from_secret([8; 32])).is_err());
        assert!(seen.try_recv().is_err(), "nothing reached the plaintext port");
        assert!(connect(addr, &listed, &server).is_ok());
        assert!(seen.recv_timeout(Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn a_plaintext_client_on_the_encrypted_port_gets_nothing() {
        let (addr, _, seen) = front(Vec::new());
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(7))).unwrap();
        s.write_all(b"*1\r\n$4\r\nPING\r\n").unwrap();
        let mut buf = [0u8; 16];
        assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)));
        assert!(seen.try_recv().is_err());
    }

    #[test]
    fn a_client_is_closed_when_the_server_refuses_the_token_or_is_down() {
        let refusing = fake_upstream_answering(b"-ERR invalid peer token\r\n");
        let (addr, server, _seen) = front_over(Vec::new(), refusing);
        let (mut s, _) = connect(addr, &Keypair::from_secret([2; 32]), &server).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(s.read(&mut buf).unwrap(), 0, "closed, never relayed");

        // nothing listens where the plaintext port should be
        let gone = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = gone.local_addr().unwrap();
        drop(gone);
        let (_tx, rx) = std::sync::mpsc::channel();
        let (addr, server, _) = front_over(Vec::new(), (dead, rx));
        let (mut s, _) = connect(addr, &Keypair::from_secret([2; 32]), &server).unwrap();
        assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)));
    }

    #[test]
    fn encrypted_cluster_ports_follow_their_settings() {
        let mut cfg = Config::default();
        cfg.cluster.enabled = true;
        cfg.server.threads = 3;
        cfg.secure.listen_port = 7000;
        assert_eq!(secure_cluster_port_base(&cfg), 7001, "listen_port + 1 by default");
        assert_eq!(advertised_secure_cluster_base(&cfg), 7001, "advertised as bound");
        cfg.secure.cluster_port_base = 7100;
        cfg.secure.announce_cluster_port_base = 9100;
        assert_eq!(advertised_secure_cluster_base(&cfg), 9100, "a NAT's port is told");
        let plain = crate::cluster_port_base(&cfg);
        assert_eq!(
            port_pairs(&cfg),
            vec![(7000, cfg.server.port), (7100, plain), (7101, plain + 1), (7102, plain + 2)]
        );
        cfg.cluster.enabled = false;
        assert_eq!(port_pairs(&cfg), vec![(7000, cfg.server.port)]);
    }
}
