//! The prefix tools: `copy-prefix`, `delete-prefix`, `digest`, `inspect`
//! on one server, and `diff`, which compares a prefix on two.

use super::argscan::{Scan, unexpected};
use super::shipped::Shipped;
use crate::bulk::DeleteMode;
use crate::link::Link;
use std::io;
use std::process::ExitCode;

/// Opens the second server `diff` compares against.
pub(crate) type Opener<'a> = &'a mut dyn FnMut(&str) -> Result<Box<dyn Link>, String>;

/// What the prefix tools take besides the connection.
struct Words {
    rate: u64,
    mode: DeleteMode,
    positional: Vec<String>,
}

fn words(tool: Shipped, args: &[String]) -> Result<Words, String> {
    let mut w = Words { rate: 0, mode: DeleteMode::Unlink, positional: Vec::new() };
    let mut scan = Scan::new(args);
    while let Some(word) = scan.next() {
        match word {
            "--rate" if matches!(tool, Shipped::CopyPrefix | Shipped::DeletePrefix) => {
                w.rate = scan.number("--rate")?
            }
            "--dry-run" if tool == Shipped::DeletePrefix => w.mode = DeleteMode::DryRun,
            p if !p.starts_with('-') => w.positional.push(p.to_string()),
            other => return Err(unexpected(other)),
        }
    }
    Ok(w)
}

fn usage(tool: Shipped) -> &'static str {
    match tool {
        Shipped::CopyPrefix => "[--rate n] <src-prefix> <dst-prefix>",
        Shipped::DeletePrefix => "[--rate n] [--dry-run] <prefix>",
        Shipped::Diff => "<other host:port | redis://...> <prefix>...",
        _ => "<prefix>",
    }
}

fn fail(tool: Shipped, msg: &str) -> ExitCode {
    let name = tool.name();
    eprintln!("kevy-cli {name}: {msg}");
    eprintln!("usage: kevy-cli --kevy {name} {}", usage(tool));
    ExitCode::FAILURE
}

/// Run one prefix tool on `link`; `open` reaches `diff`'s second server.
pub(crate) fn run(
    tool: Shipped,
    link: &mut dyn Link,
    args: &[String],
    open: Opener<'_>,
) -> ExitCode {
    let w = match words(tool, args) {
        Ok(w) => w,
        Err(msg) => return fail(tool, &msg),
    };
    let done: io::Result<bool> = match (tool, w.positional.as_slice()) {
        (Shipped::CopyPrefix, [src, dst]) => {
            crate::bulk::run_copy_prefix(link, src.as_bytes(), dst.as_bytes(), w.rate).map(|e| {
                super::data::report_skipped("copy-prefix", &e.skipped);
                println!("copied {} keys", e.keys);
                true
            })
        }
        (Shipped::DeletePrefix, [p]) => {
            crate::bulk::run_delete_prefix(link, p.as_bytes(), w.rate, w.mode).map(|n| {
                let verb = if w.mode == DeleteMode::DryRun { "would delete " } else { "deleted " };
                println!("{verb}{n} keys");
                true
            })
        }
        (Shipped::Digest, [p]) => crate::bulk::run_digest(link, p.as_bytes())
            .map(|(n, d)| println!("{n} keys {d}"))
            .map(|()| true),
        (Shipped::Inspect, [p]) => {
            crate::bulk::run_inspect(link, p.as_bytes(), io::stdout()).map(|()| true)
        }
        (Shipped::Diff, [other, prefixes @ ..]) if !prefixes.is_empty() => {
            let mut b = match open(other) {
                Ok(b) => b,
                Err(msg) => return fail(tool, &msg),
            };
            let prefixes: Vec<Vec<u8>> = prefixes.iter().map(|p| p.as_bytes().to_vec()).collect();
            crate::bulk::run_diff(link, b.as_mut(), &prefixes, io::stdout())
                .map(|bad| bad.is_empty())
        }
        _ => return fail(tool, "wrong arguments"),
    };
    exit_code(tool, done)
}

/// A tool's verdict (`Ok(false)`: it ran and found a difference) as its exit code.
fn exit_code(tool: Shipped, done: io::Result<bool>) -> ExitCode {
    match done {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("kevy-cli {}: {e}", tool.name());
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_run_succeeds_and_a_difference_or_an_io_error_fails() {
        assert_eq!(exit_code(Shipped::Digest, Ok(true)), ExitCode::SUCCESS);
        assert_eq!(exit_code(Shipped::Diff, Ok(false)), ExitCode::FAILURE);
        let reset = io::Error::other("connection reset");
        assert_eq!(exit_code(Shipped::Digest, Err(reset)), ExitCode::FAILURE);
    }
}
