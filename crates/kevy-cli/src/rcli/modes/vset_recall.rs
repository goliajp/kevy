//! `--vset-recall <key>`: how often a vector set's approximate search finds
//! what an exact search finds, over queries mixed from random members.

use super::hdr::Histogram;
use super::random::Random;
use crate::rcli::format::Output;
use crate::rcli::send::write_out;
use crate::rcli::session::{Session, eprint_bytes};
use kevy_resp::Reply;

const THRESHOLDS: [f64; 10] = [0.0, 50.0, 60.0, 70.0, 80.0, 85.0, 90.0, 95.0, 99.0, 100.0];

/// Query until Ctrl-C, then report; 1 when the key is not a vector set.
pub(crate) fn run(s: &mut Session, key: &[u8]) -> u8 {
    kevy_sys::install_interrupt(1);
    kevy_sys::note_interrupts();
    let dimension = match s.request(&[b"VDIM", key]) {
        Ok(Reply::Int(d)) if d > 0 => d,
        _ => {
            eprint_bytes(&[b"Error: Cannot get dimension for key ", key, b"\n"]);
            return 1;
        }
    };
    let m = &s.opts.modes;
    let shape = (m.vset_recall_ele, m.vset_recall_count, m.vset_recall_ef);
    write_out(&header(key, dimension, shape));
    let terminal = s.opts.output == Output::Standard;
    let (mut recalls, mut sum, mut queries, mut random) =
        (Histogram::recalls(), 0.0, 0u64, Random::seeded());
    while !kevy_sys::take_noted() {
        match one_query(s, key, shape, &mut random) {
            Ok(Some(recall)) => {
                queries += 1;
                sum += recall;
                recalls.record((recall * 100.0) as u64 + 1);
                let line = format!("Queries: {queries} | Avg recall: {:.2}%", sum / queries as f64);
                write_out(
                    if terminal { format!("\x1b[0G\x1b[2K{line}") } else { format!("{line}\n") }
                        .as_bytes(),
                );
            }
            Ok(None) => {}
            Err(msg) => {
                eprint_bytes(&[b"Error: ", &msg, b"\n"]);
                return 1;
            }
        }
        if s.opts.interval_us > 0 {
            std::thread::sleep(std::time::Duration::from_micros(s.opts.interval_us));
        }
    }
    write_out(&report(&recalls, sum, queries));
    0
}

fn header(key: &[u8], dimension: i64, (mix, count, ef): (i64, i64, i64)) -> Vec<u8> {
    let named = [b"\n# Testing recall for vector set: ".as_slice(), key].concat();
    let rest = format!(
        " (dimension: {dimension})\n# Mixing {mix} random element vectors, top {count} results, EF={ef}\n\n"
    );
    [named, rest.into_bytes()].concat()
}

/// One query's recall in percent; `None` when there was nothing to query.
fn one_query(
    s: &mut Session,
    key: &[u8],
    (mix, count, ef): (i64, i64, i64),
    random: &mut Random,
) -> Result<Option<f64>, Vec<u8>> {
    let failed = |e: crate::rcli::conn::LinkError| e.text().into_bytes();
    let mix_text = mix.to_string();
    let members = match s.request(&[b"VRANDMEMBER", key, mix_text.as_bytes()]).map_err(failed)? {
        Reply::Array(items) => items
            .into_iter()
            .filter_map(|m| if let Reply::Bulk(b) = m { Some(b) } else { None })
            .collect::<Vec<_>>(),
        Reply::Error(msg) => return Err(msg),
        _ => return Ok(None),
    };
    let asks: Vec<Vec<&[u8]>> = members.iter().map(|m| vec![&b"VEMB"[..], key, m]).collect();
    let vectors: Vec<Vec<f64>> =
        s.pipeline(&asks).map_err(failed)?.into_iter().filter_map(numbers).collect();
    let Some(query) = blend(&vectors, random) else { return Ok(None) };
    let blob: Vec<u8> = query.iter().flat_map(|x| (*x as f32).to_le_bytes()).collect();
    let (count, ef) = (count.to_string(), ef.to_string());
    let approximate: Vec<&[u8]> =
        vec![b"VSIM", key, b"FP32", &blob, b"COUNT", count.as_bytes(), b"EF", ef.as_bytes()];
    let exact: Vec<&[u8]> =
        vec![b"VSIM", key, b"FP32", &blob, b"COUNT", count.as_bytes(), b"TRUTH"];
    let mut found = s.pipeline(&[approximate, exact]).map_err(failed)?.into_iter().map(names);
    let (Some(Ok(approximate)), Some(Ok(exact))) = (found.next(), found.next()) else {
        return Err(b"VSIM failed".to_vec());
    };
    if exact.is_empty() {
        return Ok(None);
    }
    let hits = approximate.iter().filter(|a| exact.contains(a)).count();
    Ok(Some(hits as f64 / exact.len() as f64 * 100.0))
}

/// A random convex combination of the members' vectors.
fn blend(vectors: &[Vec<f64>], random: &mut Random) -> Option<Vec<f64>> {
    let dimension = vectors.first()?.len();
    let weights: Vec<f64> = vectors.iter().map(|_| random.unit() + f64::EPSILON).collect();
    let total: f64 = weights.iter().sum();
    Some(
        (0..dimension)
            .map(|d| {
                vectors
                    .iter()
                    .zip(&weights)
                    .map(|(v, w)| v.get(d).copied().unwrap_or(0.0) * w)
                    .sum::<f64>()
                    / total
            })
            .collect(),
    )
}

fn numbers(reply: Reply) -> Option<Vec<f64>> {
    let Reply::Array(items) = reply else { return None };
    items
        .into_iter()
        .map(|i| match i {
            Reply::Double(d) => Some(d),
            Reply::Bulk(b) | Reply::Simple(b) => String::from_utf8_lossy(&b).trim().parse().ok(),
            Reply::Int(n) => Some(n as f64),
            _ => None,
        })
        .collect()
}

fn names(reply: Reply) -> Result<Vec<Vec<u8>>, Vec<u8>> {
    match reply {
        Reply::Array(items) => Ok(items
            .into_iter()
            .filter_map(|i| if let Reply::Bulk(b) = i { Some(b) } else { None })
            .collect()),
        Reply::Error(msg) => Err(msg),
        _ => Ok(Vec::new()),
    }
}

/// The closing report. Distribution figures come from the histogram, whose
/// values are hundredths of a percent plus one.
fn report(recalls: &Histogram, sum: f64, queries: u64) -> Vec<u8> {
    let percent = |v: u64| v.saturating_sub(1) as f64 / 100.0;
    let (low, high) = recalls.bounds();
    let (mean, deviation) = recalls.mean_and_deviation();
    let average = if queries == 0 { 0.0 } else { sum / queries as f64 };
    let mut out = format!(
        "\n\n====================================\n       Recall Test Results\n====================================\n\n\
Total queries:   {queries}\nAverage recall:  {average:.2}%\nMean recall:     {:.2}%\nMedian recall:   {:.2}%\n\
StdDev:          {:.2}%\nMin recall:      {:.2}%\nMax recall:      {:.2}%\n\n\
--- Recall Thresholds ---\nAt least    % of queries\n--------    ------------\n",
        (mean - 1.0).max(0.0) / 100.0,
        percent(recalls.value_at_percentile(50.0)),
        deviation / 100.0,
        percent(low),
        percent(high),
    );
    for t in THRESHOLDS {
        out.push_str(&format!("{t:6.1}%         {:6.2}%\n", share_at_least(recalls, t)));
    }
    out.into_bytes()
}

/// The share of queries with at least `threshold` recall: 100 minus the
/// first percentile, in tenths, whose value reaches it.
fn share_at_least(recalls: &Histogram, threshold: f64) -> f64 {
    if recalls.total() == 0 {
        return 0.0;
    }
    let target = (threshold * 100.0) as u64 + 1;
    (0..=1000)
        .map(|tenths| f64::from(tenths) / 10.0)
        .find(|p| recalls.value_at_percentile(*p) >= target)
        .map_or(0.0, |p| 100.0 - p)
}

#[cfg(test)]
mod tests {
    use super::{Histogram, report};

    #[test]
    fn the_report_reads_the_histogram_as_the_reference_does() {
        let mut h = Histogram::recalls();
        for recall in [100.0f64, 100.0, 80.0] {
            h.record((recall * 100.0) as u64 + 1);
        }
        let text = String::from_utf8_lossy(&report(&h, 280.0, 3)).into_owned();
        for line in [
            "Average recall:  93.33%\n",
            "Mean recall:     93.48%\n",
            "Median recall:   100.46%\n",
            "StdDev:          9.43%\n",
            "Min recall:      79.99%\n",
            "Max recall:      100.46%\n",
            "  80.0%          99.90%\n",
            "  85.0%          50.00%\n",
            "  70.0%         100.00%\n",
        ] {
            assert!(text.contains(line), "no {line:?} in {text}");
        }
        assert!(
            String::from_utf8_lossy(&report(&Histogram::recalls(), 0.0, 0))
                .contains("Total queries:   0\n")
        );
    }
}
