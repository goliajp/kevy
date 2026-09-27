//! The bare-word forms kevy-cli 6.4 shipped (`kevy-cli doctor -p 6004`),
//! kept for the rest of 6.x with one deprecation line each and removed in
//! 7.0 (RFC §13.5). Their private `-h`/`-p` refuse a bad value.
//!
//! `backup` and `restore` are also server commands; only their own flag
//! shapes (`--data-dir`/`--to`, `--from`/`--to`) are the tools, so
//! `kevy-cli restore key 0 payload` stays a RESTORE. `digest <prefix>` has
//! the same shape as `DIGEST <key>` and stays the tool until 7.0 (a listed
//! deviation).

use super::dispatch;
use super::shipped::Shipped;
use crate::link::Link;
use kevy_resp_client::RespClient;
use std::process::ExitCode;

/// Route a bare shipped word; `None` when `args` is not one.
pub(crate) fn route(args: &[String]) -> Option<ExitCode> {
    let tool = Shipped::named(args.first()?.as_bytes())?;
    let rest = &args[1..];
    let flagged = |flags: &[&str]| rest.iter().any(|a| flags.contains(&a.as_str()));
    match tool {
        Shipped::Backup if !flagged(&["--data-dir", "--to"]) => return None,
        Shipped::Restore if !flagged(&["--from", "--to"]) => return None,
        _ => {}
    }
    let name = tool.name();
    eprintln!(
        "kevy-cli: `kevy-cli {name} ...` is deprecated and goes away in 7.0; use `kevy-cli [-h host] [-p port] --kevy {name} ...`"
    );
    Some(match tool {
        Shipped::Diff => diff(rest),
        Shipped::Sql => sql(rest),
        t if !t.needs_server(rest) => dispatch(t, None, rest, &mut no_other),
        t => with_private_connection(name, rest, |link, rest| {
            dispatch(t, Some(link), rest, &mut no_other)
        }),
    })
}

fn no_other(_: &str) -> Result<Box<dyn Link>, String> {
    Err("only diff reaches a second server".into())
}

/// Take `-h host` / `-p port` out of `args`, connect, run `run` on the rest.
pub(crate) fn with_private_connection(
    tool: &str,
    args: &[String],
    run: impl FnOnce(&mut dyn Link, &[String]) -> ExitCode,
) -> ExitCode {
    let (mut host, mut port) = (crate::DEFAULT_HOST.to_string(), crate::DEFAULT_PORT);
    let mut rest = Vec::new();
    let mut scan = super::argscan::Scan::new(args);
    while let Some(word) = scan.next() {
        let taken = match word {
            "-h" => scan.value("-h").map(|h| host = h.to_string()),
            "-p" => scan.value("-p").and_then(|p| {
                p.parse().map(|n| port = n).map_err(|_| format!("-p takes a port, not '{p}'"))
            }),
            other => {
                rest.push(other.to_string());
                Ok(())
            }
        };
        if let Err(msg) = taken {
            eprintln!("kevy-cli {tool}: {msg}");
            return ExitCode::FAILURE;
        }
    }
    connect_then(tool, &host, port, |link| run(link, &rest))
}

fn connect_then(
    tool: &str,
    host: &str,
    port: u16,
    run: impl FnOnce(&mut dyn Link) -> ExitCode,
) -> ExitCode {
    match RespClient::connect(host, port) {
        Ok(mut client) => run(&mut client),
        Err(e) => {
            eprintln!("kevy-cli {tool}: could not connect to {host}:{port}: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `host:port` as a pair.
fn endpoint(text: &str) -> Result<(String, u16), String> {
    let (h, p) = text.rsplit_once(':').ok_or(format!("'{text}' is not host:port"))?;
    Ok((h.to_string(), p.parse().map_err(|_| format!("'{text}' is not host:port"))?))
}

/// 6.4's `diff <hostA:portA> <hostB:portB> <prefix...>`.
fn diff(rest: &[String]) -> ExitCode {
    let Some((a, others)) = rest.split_first() else {
        return dispatch(Shipped::Diff, None, rest, &mut no_other);
    };
    let (host, port) = match endpoint(a) {
        Ok(e) => e,
        Err(msg) => {
            eprintln!("kevy-cli diff: {msg}");
            return ExitCode::FAILURE;
        }
    };
    let mut open = |b: &str| -> Result<Box<dyn Link>, String> {
        let (h, p) = endpoint(b)?;
        RespClient::connect(&h, p)
            .map(|c| Box::new(c) as Box<dyn Link>)
            .map_err(|e| format!("could not connect to {b}: {e}"))
    };
    connect_then("diff", &host, port, |link| dispatch(Shipped::Diff, Some(link), others, &mut open))
}

/// 6.4's `sql compile <file> --apply --url <host:port>`.
fn sql(rest: &[String]) -> ExitCode {
    let at = rest.iter().position(|a| a == "--url");
    let Some(at) = at else { return dispatch(Shipped::Sql, None, rest, &mut no_other) };
    let Some(url) = rest.get(at + 1) else {
        eprintln!("kevy-cli sql: --url needs host:port");
        return ExitCode::FAILURE;
    };
    let (host, port) = match endpoint(url) {
        Ok(e) => e,
        Err(msg) => {
            eprintln!("kevy-cli sql: --url {msg}");
            return ExitCode::FAILURE;
        }
    };
    let args: Vec<String> = rest
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != at && *i != at + 1)
        .map(|(_, a)| a.clone())
        .collect();
    connect_then("sql", &host, port, |link| {
        dispatch(Shipped::Sql, Some(link), &args, &mut no_other)
    })
}

#[cfg(test)]
mod tests {
    use super::route;

    fn words(line: &str) -> Vec<String> {
        line.split(' ').map(String::from).collect()
    }

    #[test]
    fn a_server_command_shaped_backup_or_restore_is_not_the_tool() {
        assert!(route(&words("restore k 0 payload")).is_none());
        assert!(route(&words("backup")).is_none());
        assert!(route(&words("watch k")).is_none());
        assert!(route(&words("tables")).is_none(), "P5 names were never bare words");
    }
}
