//! The REPL prompt: where the session is connected and what state it is in.

use super::session::Session;

/// Longest prompt, in bytes.
const MAX_PROMPT: usize = 127;

/// `host:port[db](TX)(subscribed mode)> `, `kevy <socket>> …`, or
/// `not connected> `.
pub(crate) fn prompt(s: &Session) -> Vec<u8> {
    if s.conn.is_none() {
        return b"not connected> ".to_vec();
    }
    let mut p = match &s.opts.socket {
        // DEV-011: redis-cli names itself here ("redis <path>").
        Some(path) => [b"kevy ".as_slice(), path].concat(),
        None if s.opts.host.contains(&b':') => [b"[".as_slice(), &s.opts.host, b"]"].concat(),
        None => s.opts.host.clone(),
    };
    if s.opts.socket.is_none() {
        p.extend_from_slice(format!(":{}", s.opts.port).as_bytes());
    }
    if s.dbnum != 0 {
        p.extend_from_slice(format!("[{}]", s.dbnum).as_bytes());
    }
    if s.in_multi {
        p.extend_from_slice(b"(TX)");
    }
    if s.pubsub_mode {
        p.extend_from_slice(b"(subscribed mode)");
    }
    p.truncate(MAX_PROMPT - 2);
    p.extend_from_slice(b"> ");
    p
}

#[cfg(test)]
mod tests {
    use super::prompt;
    use crate::rcli::conn::Conn;
    use crate::rcli::opts::Opts;
    use crate::rcli::session::Session;

    /// A session connected to a listener nobody serves: enough for a prompt.
    fn session(opts: Opts, dir: &std::path::Path) -> (Session, std::os::unix::net::UnixListener) {
        let path = dir.join("p.sock");
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind a test socket");
        let mut s = Session::new(opts);
        s.conn = Some(Conn::unix(path.as_os_str().as_encoded_bytes()).expect("connect to it"));
        (s, listener)
    }

    #[test]
    fn the_prompt_names_where_and_what_state() {
        let dir = std::env::temp_dir().join(format!("kevy-cli-prompt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let text = |s: &Session| String::from_utf8_lossy(&prompt(s)).into_owned();

        let mut opts = Opts::defaults(false);
        opts.port = 7000;
        assert_eq!(text(&Session::new(opts.clone())), "not connected> ");
        let (mut s, _l) = session(opts.clone(), &dir);
        assert_eq!(text(&s), "127.0.0.1:7000> ");
        s.dbnum = 3;
        s.in_multi = true;
        s.pubsub_mode = true;
        assert_eq!(text(&s), "127.0.0.1:7000[3](TX)(subscribed mode)> ");

        opts.host = b"::1".to_vec();
        let (s, _l) = session(opts.clone(), &dir);
        assert_eq!(text(&s), "[::1]:7000> ", "an IPv6 host is bracketed");

        opts.socket = Some(b"/run/kevy.sock".to_vec());
        let (s, _l) = session(opts.clone(), &dir);
        assert_eq!(text(&s), "kevy /run/kevy.sock> ", "DEV-011");

        opts.socket = Some(vec![b'x'; 300]);
        let (s, _l) = session(opts, &dir);
        let long = prompt(&s);
        assert_eq!(long.len(), 127, "cut to fit, keeping the `> `");
        assert!(long.ends_with(b"> "));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
