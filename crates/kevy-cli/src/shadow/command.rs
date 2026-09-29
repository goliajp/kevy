//! The command-line face of `shadow`: two quoted commands in, an exit
//! code a cutover script can gate on out.

use std::process::ExitCode;

use super::{Shape, print_report, run};

/// Everything `shadow` takes from the command line.
struct ShadowArgs {
    old: Option<String>,
    new: Option<String>,
    old_shape: Shape,
    new_shape: Shape,
    samples: u64,
}

fn parse_shadow_flags(args: &[String]) -> Result<ShadowArgs, String> {
    // A kevy paged reply is recognised from its shape. The ambiguity
    // that needs declaring is member/score pairs versus a plain list,
    // and only on the old side in practice.
    let mut a = ShadowArgs {
        old: None,
        new: None,
        old_shape: Shape::Flat,
        new_shape: Shape::Paged,
        samples: 1,
    };
    let mut scan = crate::tools::argscan::Scan::new(args);
    while let Some(word) = scan.next() {
        match word {
            "--old" => a.old = Some(scan.value("--old")?.to_string()),
            "--new" => a.new = Some(scan.value("--new")?.to_string()),
            "--old-pairs" => a.old_shape = Shape::Pairs,
            "--new-flat" => a.new_shape = Shape::Flat,
            "--samples" => a.samples = scan.number("--samples")?,
            other => return Err(crate::tools::argscan::unexpected(other)),
        }
    }
    Ok(a)
}

/// `shadow --old "<cmd>" --new "<cmd>" [--old-pairs] [--new-flat]
/// [--samples n]` on `client`.
///
/// Both sides are whole commands, quoted, because the old path is
/// whatever the application already runs — a ZRANGE, an LRANGE, a
/// SMEMBERS — and the new one is an `IDX.QUERY`. Nothing here knows
/// which; it compares the two orders of row keys they produce. Exits
/// non-zero on any divergence, so a cutover script can gate on it
/// without parsing the text.
pub(crate) fn run_on(client: &mut dyn crate::link::Link, args: &[String]) -> ExitCode {
    let parsed = parse_shadow_flags(args);
    let Ok(ShadowArgs { old: Some(old), new: Some(new), old_shape, new_shape, samples }) = parsed
    else {
        if let Err(msg) = parsed {
            eprintln!("kevy-cli shadow: {msg}");
        }
        eprintln!(
            "usage: kevy-cli --kevy shadow --old \"<command>\" --new \"<command>\" \
             [--old-pairs] [--new-flat] [--samples n]"
        );
        return ExitCode::FAILURE;
    };
    let split =
        |s: &str| -> Vec<Vec<u8>> { s.split_whitespace().map(|t| t.as_bytes().to_vec()).collect() };
    match run(client, &split(&old), &split(&new), old_shape, new_shape, samples) {
        Ok(report) => {
            print_report(&report);
            // A divergence is a finding, not a crash.
            if report.diverged > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
        }
        Err(e) => {
            eprintln!("kevy-cli shadow: {e}");
            ExitCode::FAILURE
        }
    }
}
