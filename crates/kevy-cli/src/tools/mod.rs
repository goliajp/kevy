//! The tools kevy-cli shipped before `--kevy`: one dispatch, reached from
//! `--kevy <tool>` on the session's connection and, for the rest of 6.x,
//! from the bare word with its own `-h`/`-p` (RFC §13).

pub(crate) mod argscan;
pub(crate) mod bare;
mod data;
mod prefix;
pub(crate) mod session;
pub(crate) mod shipped;
mod sql;
mod sql_probe;

use crate::link::Link;
use prefix::Opener;
use shipped::Shipped;
use std::process::ExitCode;

/// Run `tool` with `args`; `link` is the server when there is one, `open`
/// reaches `diff`'s second server.
pub(crate) fn dispatch(
    tool: Shipped,
    link: Option<&mut dyn Link>,
    args: &[String],
    open: Opener<'_>,
) -> ExitCode {
    match (tool, link) {
        (Shipped::Backup, _) => data::backup(args),
        (Shipped::Restore, _) => data::restore(args),
        (Shipped::Sql, link) => sql::run(args, link),
        (_, None) => {
            eprintln!("kevy-cli {}: needs a server", tool.name());
            ExitCode::FAILURE
        }
        (Shipped::Export, Some(link)) => data::export(link, args),
        (Shipped::Import, Some(link)) => data::import(link, args),
        (Shipped::Doctor, Some(link)) => crate::doctor::run_on(link, args),
        (Shipped::Shadow, Some(link)) => crate::shadow::run_on(link, args),
        (Shipped::Lint, Some(link)) => crate::lint::run_on(link, args),
        (Shipped::BackfillKeys, Some(link)) => crate::backfill_keys::run_on(link, args),
        (
            Shipped::CopyPrefix
            | Shipped::DeletePrefix
            | Shipped::Digest
            | Shipped::Inspect
            | Shipped::Diff,
            Some(link),
        ) => prefix::run(tool, link, args, open),
    }
}
