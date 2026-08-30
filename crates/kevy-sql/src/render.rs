//! The readable script artifact: commands as pasteable lines, query
//! cards as documented comment blocks with a machine-readable
//! `#@card` / `#@param` / `#@argv` section (argv tab-separated), then
//! the notes. Output is deterministic (declaration order throughout).

use crate::Compilation;

/// Shell-quote one argv token for a pasteable command line: safe
/// tokens pass through, everything else single-quotes (with the
/// standard `'\''` escape). Empty strings render as `''`.
fn shq(s: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_.:@%+=/,-".contains(c);
    if !s.is_empty() && s.chars().all(safe) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Card-template quoting: keep `$N` slots bare so the template reads
/// as a template; quote only what would split.
fn cardq(s: &str) -> String {
    if s.starts_with('$') && s.len() > 1 && s[1..].chars().all(|c| c.is_ascii_digit()) {
        return s.to_string();
    }
    shq(s)
}

pub(crate) fn render(c: &Compilation) -> String {
    let mut out = String::new();
    out.push_str("# compiled by kevy-sql ");
    out.push_str(env!("CARGO_PKG_VERSION"));
    out.push_str(
        " \u{2014} declaration-time only (Law 3): these commands run ONCE, like a migration.\n",
    );
    out.push_str("# Ad-hoc runtime SQL stays refused by the engine; runtime reads use the query cards below.\n\n");
    for cmd in &c.commands {
        let line: Vec<String> = cmd.iter().map(|a| shq(a)).collect();
        out.push_str(&line.join(" "));
        out.push('\n');
    }
    for card in &c.query_cards {
        out.push('\n');
        out.push_str(&format!("# ---- query card: {} ----\n", card.name));
        out.push_str("# runtime template \u{2014} substitute the $N slots and send as-is:\n");
        for p in &card.params {
            out.push_str(&format!("#   ${} = {} ({})\n", p.n, p.column, p.ty.tag()));
        }
        let line: Vec<String> = card.argv.iter().map(|a| cardq(a)).collect();
        out.push_str(&format!("#   {}\n", line.join(" ")));
        out.push_str(&format!("#@card {}\n", card.name));
        for p in &card.params {
            out.push_str(&format!("#@param {} {} {}\n", p.n, p.column, p.ty.tag()));
        }
        out.push_str("#@argv ");
        out.push_str(&card.argv.join("\t"));
        out.push('\n');
    }
    if !c.notes.is_empty() {
        out.push_str("\n# notes:\n");
        for n in &c.notes {
            out.push_str(&format!("#   - {n}\n"));
        }
    }
    out
}
