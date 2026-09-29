// The server the rustdoc examples talk to: an in-memory embedded store
// answering RESP on a loopback port, so every example runs against the real
// verb table instead of needing a kevy server of its own. Included as an
// expression; it evaluates to the port.
{
    fn serve(mut conn: std::net::TcpStream, store: &kevy_embedded::Store) {
        use std::io::{Read, Write};
        let (mut buf, mut chunk, mut out) = (Vec::new(), [0u8; 4096], Vec::new());
        while let Ok(n @ 1..) = conn.read(&mut chunk) {
            buf.extend_from_slice(&chunk[..n]);
            out.clear();
            while let Ok(Some((cmd, used))) = kevy_resp::parse_command(&buf) {
                let argv: Vec<Vec<u8>> = cmd.iter().map(<[u8]>::to_vec).collect();
                store.dispatch_argv(&argv, &mut out);
                buf.drain(..used);
            }
            if conn.write_all(&out).is_err() {
                return;
            }
        }
    }
    let store = kevy_embedded::Store::open(kevy_embedded::Config::default())?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let store = store.clone();
            std::thread::spawn(move || serve(conn, &store));
        }
    });
    port
}
