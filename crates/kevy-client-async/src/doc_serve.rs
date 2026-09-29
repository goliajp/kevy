// In-process stand-in for a server, shared by the rustdoc examples through
// `include!`; not part of the crate's module tree.
//
// Each step of `script` takes the next command, on whichever connection it
// arrives, checks it against `want` (the argv joined by single spaces) and
// answers with `reply`, in which `{port}` becomes the listener's port. Every
// connection is served on its own, so a cluster client's seed connection and
// its shard connections share one script.

type Script = &'static [(&'static str, &'static str)];

// Plaintext: returns the address to connect to.
async fn serve(script: Script) -> std::io::Result<std::net::SocketAddr> {
    serve_on(script, None).await
}

// Encrypted client port: every connection runs the Noise responder first.
// Returns the address and the server's public key.
#[allow(dead_code)]
async fn serve_secure(script: Script) -> std::io::Result<(std::net::SocketAddr, [u8; 32])> {
    let secret = [1; 32];
    let public = kevy_noise::Keypair::from_secret(secret).public();
    Ok((serve_on(script, Some(secret)).await?, public))
}

// A public key as the hex a `server_key=` URL parameter takes.
#[allow(dead_code)]
fn hex(key: &[u8; 32]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

async fn serve_on(
    script: Script,
    secret: Option<[u8; 32]>,
) -> std::io::Result<std::net::SocketAddr> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let next_step = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(serve_conn(sock, script, secret, next_step.clone(), addr.port()));
        }
    });
    Ok(addr)
}

async fn serve_conn(
    mut sock: tokio::net::TcpStream,
    script: Script,
    secret: Option<[u8; 32]>,
    next_step: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    port: u16,
) {
    use kevy_noise::{Keypair, Responder, frame};
    use tokio::io::AsyncWriteExt;
    let mut frames = kevy_noise::Frames::default();
    let mut noise = None;
    if let Some(secret) = secret {
        let Some(m1) = next_frame(&mut sock, &mut frames).await else { return };
        let (key, eph) = (Keypair::from_secret(secret), Keypair::from_secret([2; 32]));
        // a client that named another server key is dropped, as a server would
        let Ok((_, r)) = Responder::accept(&key, eph, b"kevy-client\x001", &m1) else { return };
        let (m2, t) = r.finish(b"").unwrap();
        sock.write_all(&frame(&m2).unwrap()).await.unwrap();
        noise = Some(t.split());
    }
    let mut buf = Vec::new();
    loop {
        let Some((argv, used)) = kevy_resp::parse_command(&buf).unwrap() else {
            let more = match &mut noise {
                Some((_, rx)) => {
                    next_frame(&mut sock, &mut frames).await.map(|m| rx.open(&m).unwrap())
                }
                None => next_bytes(&mut sock).await,
            };
            let Some(bytes) = more else { return };
            buf.extend_from_slice(&bytes);
            continue;
        };
        buf.drain(..used);
        let step = next_step.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let Some((want, reply)) = script.get(step) else { return };
        let got: Vec<_> = argv.iter().map(String::from_utf8_lossy).collect();
        assert_eq!(got.join(" "), *want, "the example sent an unexpected command");
        let reply = reply.replace("{port}", &port.to_string()).into_bytes();
        let wire = match &mut noise {
            Some((tx, _)) => frame(&tx.seal(&reply).unwrap()).unwrap(),
            None => reply,
        };
        sock.write_all(&wire).await.unwrap();
    }
}

// The next bytes off the socket; `None` once the client has closed it.
async fn next_bytes(sock: &mut tokio::net::TcpStream) -> Option<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut chunk = [0u8; 4096];
    match sock.read(&mut chunk).await.unwrap_or(0) {
        0 => None,
        n => Some(chunk[..n].to_vec()),
    }
}

// The next whole Noise message; `None` once the client has closed it.
async fn next_frame(
    sock: &mut tokio::net::TcpStream,
    frames: &mut kevy_noise::Frames,
) -> Option<Vec<u8>> {
    loop {
        if let Some(m) = frames.next() {
            return Some(m);
        }
        frames.push(&next_bytes(sock).await?);
    }
}

// `CLUSTER SLOTS` step for a one-node cluster served by the same listener.
#[allow(dead_code)]
const ONE_SHARD: (&str, &str) =
    ("CLUSTER SLOTS", "*1\r\n*3\r\n:0\r\n:16383\r\n*2\r\n$9\r\n127.0.0.1\r\n:{port}\r\n");
