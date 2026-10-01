//! The `--keystats` report, as text for a pipe or a screen for a terminal.

use super::human::bytes;
use super::keystats::{NAME_BUCKETS, Stats};
use super::progress::bar_line;
use super::server::{Align, pad};
use super::sizes::Measure;

/// The whole report for a pipe, printed once at the end.
pub(crate) fn text(st: &Stats) -> Vec<u8> {
    let mut out = format!("{:6.2}% keys scanned\n", percent(st)).into_bytes();
    for line in body(st) {
        out.extend_from_slice(&line);
        out.push(b'\n');
    }
    out.extend_from_slice(&interrupted(st));
    out
}

/// The report as a terminal draws it, each line cleared first. Until the
/// `last` drawing it returns to its top, to be drawn over.
pub(crate) fn screen(st: &Stats, last: bool) -> Vec<u8> {
    let mut out = bar_line(percent(st));
    let lines = body(st);
    for line in &lines {
        out.extend_from_slice(b"\x1b[2K\r");
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    if last {
        out.extend_from_slice(&interrupted(st));
    } else {
        out.extend_from_slice(format!("\x1b[{}A\r", lines.len() + 1).as_bytes());
    }
    out
}

fn percent(st: &Stats) -> f64 {
    if st.total == 0 { 0.0 } else { 100.0 * st.sampled as f64 / st.total as f64 }
}

fn interrupted(st: &Stats) -> Vec<u8> {
    match st.resume_at {
        Some(cursor) => format!(
            "\nScan interrupted:\nUse 'kevy-cli --keystats --cursor {cursor}' to restart from the last cursor.\n"
        )
        .into_bytes(),
        None => Vec::new(),
    }
}

fn body(st: &Stats) -> Vec<Vec<u8>> {
    let mut lines = vec![
        format!("Keys sampled: {}", st.sampled).into_bytes(),
        format!("Keys size:    {}", bytes(st.memory)).into_bytes(),
        Vec::new(),
    ];
    lines.extend(tops(st));
    lines.push(Vec::new());
    lines.extend(size_distribution(st));
    lines.push(Vec::new());
    lines.extend(name_lengths(st));
    lines.push(Vec::new());
    lines.extend(type_table(st));
    lines
}

/// The biggest keys, then the biggest key of each type by size and by length.
fn tops(st: &Stats) -> Vec<Vec<u8>> {
    let mut lines = vec![format!("--- Top {} key sizes ---", st.top_wanted).into_bytes()];
    for (rank, (size, kind, key)) in st.top.iter().enumerate() {
        let name = pad(&st.kinds.kinds[*kind].name, 10, Align::Left);
        lines.push(
            [format!("{:>3} {:>8} ", rank + 1, bytes(*size)).as_bytes(), &name, b" ", key].concat(),
        );
    }
    lines.push(Vec::new());
    lines.push(b"--- Top size per type ---".to_vec());
    for (kind, t) in present(st) {
        if let Some((size, key)) = &t.biggest_memory {
            lines.push(
                [
                    pad(&kind.name, 10, Align::Left).as_slice(),
                    b" ",
                    key.as_slice(),
                    b" is ",
                    bytes(*size).as_bytes(),
                ]
                .concat(),
            );
        }
    }
    lines.push(Vec::new());
    lines.push(b"--- Top length and cardinality per type ---".to_vec());
    for (kind, t) in present(st) {
        if let Some((length, key)) = &t.biggest_length {
            let amount = if kind.name == b"string" {
                bytes(*length)
            } else {
                format!("{length} {}", kind.unit(Measure::Length))
            };
            lines.push(
                [
                    pad(&kind.name, 10, Align::Left).as_slice(),
                    b" ",
                    key.as_slice(),
                    b" has ",
                    amount.as_bytes(),
                ]
                .concat(),
            );
        }
    }
    lines
}

/// Types that have keys, with their figures, in the tally's order.
fn present(st: &Stats) -> impl Iterator<Item = (&super::sizes::Kind, &super::keystats::TypeStats)> {
    st.kinds.kinds.iter().zip(&st.per_kind).filter(|(_, t)| t.keys > 0)
}

fn size_distribution(st: &Stats) -> Vec<Vec<u8>> {
    if st.sizes.total() == 0 {
        return vec![b"No key size samples collected".to_vec()];
    }
    let mut lines = vec![
        b"Key size Percentile Total keys".to_vec(),
        b"-------- ---------- -----------".to_vec(),
    ];
    for row in st.sizes.percentile_rows() {
        let pct = 100.0 * row.cumulative as f64 / st.sizes.total() as f64;
        lines
            .push(format!("{:>8} {pct:9.4}% {:11}", bytes(row.value), row.cumulative).into_bytes());
    }
    let (mean, deviation) = st.sizes.mean_and_deviation();
    lines.push(
        format!(
            "Note: 0.01% size precision, Mean: {}, StdDeviation: {}",
            // Whole bytes, truncated.
            bytes(mean as u64),
            bytes(deviation as u64)
        )
        .into_bytes(),
    );
    lines
}

fn name_lengths(st: &Stats) -> Vec<Vec<u8>> {
    let mut lines = vec![
        b"Key name length Percentile Total keys".to_vec(),
        b"--------------- ---------- -----------".to_vec(),
    ];
    let mut cumulative = 0;
    for (bound, count) in NAME_BUCKETS.iter().zip(st.names) {
        if count == 0 {
            continue;
        }
        cumulative += count;
        let label = bytes((*bound).min(st.longest_name));
        let pct = 100.0 * cumulative as f64 / st.sampled as f64;
        lines.push(format!("{label:>15} {pct:9.4}% {cumulative:11}").into_bytes());
    }
    let avg = st.name_bytes.checked_div(st.sampled).map_or_else(|| "0".to_string(), bytes);
    lines.push(format!("Total key length is {} ({avg} avg)", bytes(st.name_bytes)).into_bytes());
    lines
}

fn type_table(st: &Stats) -> Vec<Vec<u8>> {
    let mut lines = vec![
        b"Type        Total keys  Keys % Tot size Avg size  Total length/card Avg ln/card".to_vec(),
        b"--------- ------------ ------- -------- -------- ------------------ -----------".to_vec(),
    ];
    for (kind, t) in present(st) {
        let share = 100.0 * t.keys as f64 / st.sampled as f64;
        let average = t.length as f64 / t.keys as f64;
        let (length, avg) = match (kind.has_length(), kind.name.as_slice()) {
            // Nothing to measure a length with: a dash in each column,
            // written with the trailing space redis-cli gives it.
            (false, _) => ("- ".to_string(), "- ".to_string()),
            (true, b"string") => (bytes(t.length), bytes(average.round() as u64)),
            (true, _) => {
                (format!("{} {}", t.length, kind.unit(Measure::Length)), format!("{average:.2}"))
            }
        };
        let figures = format!(
            " {:11} {share:6.2}% {:>8} {:>8} {length:>18} {avg:>11}",
            t.keys,
            bytes(t.memory),
            bytes(t.memory / t.keys)
        );
        lines.push([pad(&kind.name, 10, Align::Left), figures.into_bytes()].concat());
    }
    lines
}
