//! `--kevy <shipped tool>`: the tool on the session's connection, so it
//! authenticates, speaks unix sockets and URIs, and selects a database
//! exactly as a command does (RFC §13.2 I4).

use super::dispatch;
use super::shipped::Shipped;
use crate::link::Link;
use crate::rcli::opts::Opts;
use crate::rcli::session::{Connect, Session, eprint_bytes};
use std::process::ExitCode;

/// Run `tool` with `argv`; the exit code.
pub(crate) fn run(s: &mut Session, tool: Shipped, argv: &[Vec<u8>]) -> u8 {
    let args: Option<Vec<String>> =
        argv.iter().map(|a| String::from_utf8(a.clone()).ok()).collect();
    let Some(args) = args else {
        eprint_bytes(&[b"kevy-cli ", tool.name().as_bytes(), b": arguments must be UTF-8\n"]);
        return 1;
    };
    let code = if tool.needs_server(&args) {
        if s.conn.is_none() && !s.connect(Connect::Report) {
            return 1;
        }
        let opts = s.opts.clone();
        let mut open = |endpoint: &str| other(&opts, endpoint);
        let link = s.conn.as_mut().map(|c| c as &mut dyn Link);
        dispatch(tool, link, &args, &mut open)
    } else {
        dispatch(tool, None, &args, &mut |_: &str| Err("no second server here".into()))
    };
    u8::from(code != ExitCode::SUCCESS)
}

/// A second connection with the session's options, to `host:port` or a
/// `redis://`/`valkey://` URI (credentials included).
fn other(opts: &Opts, endpoint: &str) -> Result<Box<dyn Link>, String> {
    let mut o = opts.clone();
    o.socket = None;
    if endpoint.contains("://") {
        if crate::rcli::uri::apply_uri(&mut o, endpoint.as_bytes()).is_some() {
            return Err(format!("'{endpoint}' is not a URI kevy-cli reads"));
        }
    } else {
        let (h, p) = endpoint.rsplit_once(':').ok_or(format!("'{endpoint}' is not host:port"))?;
        let port = p.parse::<u16>().map_err(|_| format!("'{endpoint}' is not host:port"))?;
        o.host = h.as_bytes().to_vec();
        o.port = i32::from(port);
    }
    let mut second = Session::new(o);
    if !second.connect(Connect::Report) {
        return Err(format!("could not connect to {endpoint}"));
    }
    second
        .conn
        .take()
        .map(|c| Box::new(c) as Box<dyn Link>)
        .ok_or(format!("could not connect to {endpoint}"))
}
