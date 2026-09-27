//! The options every relational tool shares — output shape and timing —
//! pulled out of its arguments before the tool reads the rest.

use super::render::{Format, Style};
use crate::rcli::format::Output;
use crate::rcli::session::eprint_bytes;

/// Shared options, and the arguments left for the tool.
pub(crate) struct Common {
    pub(crate) style: Style,
    pub(crate) timing: bool,
    pub(crate) args: Vec<Vec<u8>>,
}

/// Read `--format`, `--no-header`, `--null`, `--expanded` and `--timing`
/// from anywhere in `args`; the default format follows redis-cli's output
/// mode (a table on a terminal, TSV when piped, `--csv` / `--json` as given).
pub(crate) fn parse(args: &[Vec<u8>], output: Output) -> Option<Common> {
    let format = match output {
        Output::Standard => Format::Table,
        Output::Raw => Format::Tsv,
        Output::Csv => Format::Csv,
        Output::Json | Output::QuotedJson => Format::Json,
    };
    let mut c = Common {
        style: Style { format, header: true, null: Vec::new(), expanded: false },
        timing: false,
        args: Vec::new(),
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_slice() {
            b"--format" => {
                c.style.format = format_named(args.get(i + 1)?)?;
                i += 1;
            }
            b"--null" => {
                c.style.null = args.get(i + 1).cloned().or_else(|| missing(b"--null"))?;
                i += 1;
            }
            b"--no-header" => c.style.header = false,
            b"--expanded" => c.style.expanded = true,
            b"--timing" => c.timing = true,
            other => c.args.push(other.to_vec()),
        }
        i += 1;
    }
    Some(c)
}

fn format_named(name: &[u8]) -> Option<Format> {
    match name {
        b"table" => Some(Format::Table),
        b"tsv" => Some(Format::Tsv),
        b"csv" => Some(Format::Csv),
        b"json" => Some(Format::Json),
        _ => {
            eprint_bytes(&[
                b"kevy-cli: --format must be table, tsv, csv or json, not '",
                name,
                b"'\n",
            ]);
            None
        }
    }
}

fn missing<T>(flag: &[u8]) -> Option<T> {
    eprint_bytes(&[b"kevy-cli: ", flag, b" needs a value\n"]);
    None
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::rcli::format::Output;
    use crate::rcli::rds::render::Format;

    fn args(a: &[&str]) -> Vec<Vec<u8>> {
        a.iter().map(|s| s.as_bytes().to_vec()).collect()
    }

    #[test]
    fn shared_options_come_out_of_the_arguments() {
        let c = parse(
            &args(&["users", "--format", "csv", "--null", "NULL", "--no-header", "x*", "--timing"]),
            Output::Standard,
        )
        .unwrap();
        assert_eq!(
            (c.style.format, c.style.header, c.style.null.as_slice(), c.timing),
            (Format::Csv, false, &b"NULL"[..], true)
        );
        assert_eq!(c.args, args(&["users", "x*"]));
        assert_eq!(parse(&[], Output::Raw).unwrap().style.format, Format::Tsv);
        assert_eq!(parse(&[], Output::Json).unwrap().style.format, Format::Json);
        assert!(parse(&args(&["--format", "xml"]), Output::Raw).is_none());
        assert!(parse(&args(&["--null"]), Output::Raw).is_none());
    }
}
