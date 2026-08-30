//! (vendored regex engine — see the parent module.)
#![allow(clippy::all, clippy::pedantic)]
use super::*;

pub(crate) fn cap_set(caps: &mut Caps, journal: &mut CapJournal, idx: usize, val: (usize, usize)) {
    if idx < caps.len() {
        journal.push((idx, caps[idx]));
        caps[idx] = Some(val);
    }
}

pub(crate) fn cap_undo(caps: &mut Caps, journal: &mut CapJournal, mark: usize) {
    while journal.len() > mark {
        let (idx, old) = journal.pop().unwrap();
        caps[idx] = old;
    }
}

// LOC-WAIVER: vendored spg ERE engine core (byte-identical fork); splitting upstream's tested matcher/parser injects bugs without readability gain.
pub(crate) fn re_match_at_caps(
    node: &ReNode,
    s: &[char],
    pos: usize,
    depth: u32,
    steps: &mut u64,
    caps: &mut Caps,
    journal: &mut CapJournal,
) -> Result<Option<usize>, ReErr> {
    if depth > CAP_MATCH_DEPTH_LIMIT {
        return Err(ReErr::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    *steps += 1;
    if *steps > MATCH_STEP_LIMIT {
        return Err(ReErr::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    let d = depth + 1;
    match node {
        // Non-recursive leaves are identical to the capture-free matcher.
        ReNode::Literal(c) => Ok((s.get(pos).copied() == Some(*c)).then_some(pos + 1)),
        ReNode::AnyChar => Ok((pos < s.len()).then_some(pos + 1)),
        ReNode::Class { members, negated } => match s.get(pos) {
            Some(&c) => {
                let hit = members.iter().any(|m| class_matches(m, c));
                Ok((hit ^ negated).then_some(pos + 1))
            }
            None => Ok(None),
        },
        ReNode::Start => Ok((pos == 0).then_some(pos)),
        ReNode::End => Ok((pos == s.len()).then_some(pos)),
        ReNode::WordBoundary(kind) => {
            let before = pos > 0 && is_word_char(s[pos - 1]);
            let after = pos < s.len() && is_word_char(s[pos]);
            let ok = match kind {
                WordBoundaryKind::Boundary => before != after,
                WordBoundaryKind::NonBoundary => before == after,
                WordBoundaryKind::BegWord => !before && after,
                WordBoundaryKind::EndWord => before && !after,
            };
            Ok(ok.then_some(pos))
        }
        ReNode::Concat(items) => re_match_seq_caps(items, s, pos, d, steps, caps, journal),
        ReNode::Alt(branches) => {
            for b in branches {
                let mark = journal.len();
                if let Some(p) = re_match_at_caps(b, s, pos, d, steps, caps, journal)? {
                    return Ok(Some(p));
                }
                cap_undo(caps, journal, mark);
            }
            Ok(None)
        }
        ReNode::Quant { inner, min, max, greedy } => {
            // Standalone quantifier (no tail): greedy = longest, lazy = fewest.
            // Captures accumulate across reps (PG: `(a)*` keeps the LAST rep);
            // a rep that fails past the minimum leaves the earlier caps intact.
            let mut count = 0usize;
            let mut p = pos;
            loop {
                if !*greedy && count >= *min {
                    break;
                }
                if let Some(cap) = max {
                    if count >= *cap {
                        break;
                    }
                }
                let mark = journal.len();
                match re_match_at_caps(inner, s, p, d, steps, caps, journal)? {
                    Some(np) if np > p => {
                        p = np;
                        count += 1;
                    }
                    _ => {
                        cap_undo(caps, journal, mark);
                        break;
                    }
                }
            }
            if count < *min {
                return Ok(None);
            }
            Ok(Some(p))
        }
        ReNode::Lookahead { negative, inner } => {
            // Zero-width: probe `inner`, then discard any captures it made
            // (they must not leak out of the assertion) and consume nothing.
            let mark = journal.len();
            let hit = re_match_at_caps(inner, s, pos, d, steps, caps, journal)?.is_some();
            cap_undo(caps, journal, mark);
            Ok((hit != *negative).then_some(pos))
        }
        ReNode::Group { idx, inner } => {
            let start = pos;
            match re_match_at_caps(inner, s, pos, d, steps, caps, journal)? {
                Some(end) => {
                    cap_set(caps, journal, *idx, (start, end));
                    Ok(Some(end))
                }
                None => Ok(None),
            }
        }
        // match the previously-captured group text at
        // `pos`. A group that did not participate matches the empty string.
        ReNode::Backref { idx, ci } => match caps.get(*idx).copied().flatten() {
            Some((cs, ce)) => {
                let need_len = ce - cs;
                let end = pos + need_len;
                if end <= s.len()
                    && (0..need_len).all(|k| {
                        let (a, b) = (s[pos + k], s[cs + k]);
                        if *ci { a.eq_ignore_ascii_case(&b) } else { a == b }
                    })
                {
                    Ok(Some(end))
                } else {
                    Ok(None)
                }
            }
            None => Ok(Some(pos)),
        },
    }
}

// LOC-WAIVER: vendored spg ERE engine core (byte-identical fork); splitting upstream's tested matcher/parser injects bugs without readability gain.
pub(crate) fn re_match_seq_caps(
    items: &[ReNode],
    s: &[char],
    pos: usize,
    depth: u32,
    steps: &mut u64,
    caps: &mut Caps,
    journal: &mut CapJournal,
) -> Result<Option<usize>, ReErr> {
    if depth > CAP_MATCH_DEPTH_LIMIT {
        return Err(ReErr::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    *steps += 1;
    if *steps > MATCH_STEP_LIMIT {
        return Err(ReErr::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    let d = depth + 1;
    let Some((first, rest)) = items.split_first() else {
        return Ok(Some(pos));
    };
    match first {
        // a captured *quantified* group (`(a*)`) must be
        // a backtrack point when a following backref constrains it: enumerate the
        // inner quant's reachable ends, record caps[idx] at each rep count, and
        // try the tail (so `^(a*)\1$` on `aaaa` gives back to group = `aa`).
        ReNode::Group { idx, inner } if matches!(**inner, ReNode::Quant { .. }) => {
            let ReNode::Quant { inner: qinner, min, max, greedy } = &**inner else {
                unreachable!()
            };
            let mut ends = vec![pos];
            let mut marks = vec![journal.len()];
            let mut p = pos;
            let mut count = 0usize;
            loop {
                if let Some(cap) = max {
                    if count >= *cap {
                        break;
                    }
                }
                let mark = journal.len();
                match re_match_at_caps(qinner, s, p, d, steps, caps, journal)? {
                    Some(np) if np > p => {
                        p = np;
                        count += 1;
                        ends.push(p);
                        marks.push(mark);
                    }
                    _ => {
                        cap_undo(caps, journal, mark);
                        break;
                    }
                }
            }
            let n = ends.len();
            for i in 0..n {
                let reps = if *greedy { n - 1 - i } else { i };
                if reps < *min {
                    if *greedy {
                        break;
                    }
                    continue;
                }
                // keep the caps of the first `reps`
                // repetitions: roll back to the state AFTER rep `reps`
                // finished (marks[k] is the mark BEFORE rep k+1 starts, so
                // that state is marks[reps + 1]; at the full count nothing
                // rolls back). `cap_undo(marks[reps])` dropped rep `reps`'s
                // own captures — the off-by-one that made `(o)(o)?` report
                // a participating group as NULL.
                if reps + 1 < n {
                    cap_undo(caps, journal, marks[reps + 1]);
                }
                cap_set(caps, journal, *idx, (pos, ends[reps]));
                let tail_mark = journal.len();
                if let Some(e) = re_match_seq_caps(rest, s, ends[reps], d, steps, caps, journal)? {
                    return Ok(Some(e));
                }
                cap_undo(caps, journal, tail_mark);
            }
            Ok(None)
        }
        ReNode::Quant { inner, min, max, greedy } => {
            // Enumerate reachable ends, recording a journal MARK before each
            // rep so trying the tail at `k` reps can undo the captures made by
            // the reps beyond `k` (otherwise a backtrack leaves stale caps).
            let mut ends = vec![pos];
            let mut marks = vec![journal.len()];
            let mut p = pos;
            let mut count = 0usize;
            loop {
                if let Some(cap) = max {
                    if count >= *cap {
                        break;
                    }
                }
                let mark = journal.len();
                match re_match_at_caps(inner, s, p, d, steps, caps, journal)? {
                    Some(np) if np > p => {
                        p = np;
                        count += 1;
                        ends.push(p);
                        marks.push(mark);
                    }
                    _ => {
                        cap_undo(caps, journal, mark);
                        break;
                    }
                }
            }
            let n = ends.len();
            for i in 0..n {
                let reps = if *greedy { n - 1 - i } else { i };
                if reps < *min {
                    if *greedy {
                        break;
                    }
                    continue;
                }
                // Roll captures back to exactly `reps` repetitions — the
                // state AFTER rep `reps` finished (see the Group arm above;
                // marks[reps] would also drop rep `reps`'s own captures).
                if reps + 1 < n {
                    cap_undo(caps, journal, marks[reps + 1]);
                }
                let tail_mark = journal.len();
                if let Some(e) = re_match_seq_caps(rest, s, ends[reps], d, steps, caps, journal)? {
                    return Ok(Some(e));
                }
                cap_undo(caps, journal, tail_mark);
            }
            Ok(None)
        }
        ReNode::Alt(branches) => {
            for b in branches {
                let mark = journal.len();
                if let Some(p) = re_match_at_caps(b, s, pos, d, steps, caps, journal)? {
                    if let Some(e) = re_match_seq_caps(rest, s, p, d, steps, caps, journal)? {
                        return Ok(Some(e));
                    }
                }
                cap_undo(caps, journal, mark);
            }
            Ok(None)
        }
        ReNode::Concat(nested) => {
            let mut combined: Vec<ReNode> = Vec::with_capacity(nested.len() + rest.len());
            combined.extend(nested.iter().cloned());
            combined.extend(rest.iter().cloned());
            re_match_seq_caps(&combined, s, pos, d, steps, caps, journal)
        }
        other => {
            let mark = journal.len();
            match re_match_at_caps(other, s, pos, d, steps, caps, journal)? {
                Some(p) => {
                    if let Some(e) = re_match_seq_caps(rest, s, p, d, steps, caps, journal)? {
                        return Ok(Some(e));
                    }
                    cap_undo(caps, journal, mark);
                    Ok(None)
                }
                None => Ok(None),
            }
        }
    }
}

/// Find the first match of `node` at or after `from`, returning the whole-match
/// span plus each capturing group's span (index 1..=`ngroups`; `None` where a
/// group did not participate). `ngroups` is the highest group index in `node`.
pub(crate) fn re_find_caps(
    node: &ReNode,
    s: &[char],
    from: usize,
    ngroups: usize,
) -> Result<Option<MatchWithCaps>, ReErr> {
    let mut steps: u64 = 0;
    let mut start = from;
    loop {
        let mut caps: Caps = vec![None; ngroups + 1];
        let mut journal: CapJournal = Vec::new();
        if let Some(end) = re_match_at_caps(node, s, start, 0, &mut steps, &mut caps, &mut journal)?
        {
            return Ok(Some(((start, end), caps)));
        }
        if start >= s.len() {
            return Ok(None);
        }
        start += 1;
    }
}

pub(crate) fn expand_replacement(
    repl: &str,
    chars: &[char],
    whole: (usize, usize),
    caps: &Caps,
    out: &mut String,
) {
    let rep: Vec<char> = repl.chars().collect();
    let mut i = 0;
    while i < rep.len() {
        if rep[i] == '\\' && i + 1 < rep.len() {
            let c = rep[i + 1];
            if let Some(d) = c.to_digit(10) {
                let g = d as usize;
                if g == 0 {
                    out.extend(chars[whole.0..whole.1].iter());
                } else if let Some(Some((a, b))) = caps.get(g) {
                    out.extend(chars[*a..*b].iter());
                }
                // A `\N` for a non-participating / out-of-range group expands
                // to nothing, matching PG.
            } else if c == '&' {
                out.extend(chars[whole.0..whole.1].iter());
            } else {
                out.push(c);
            }
            i += 2;
        } else {
            out.push(rep[i]);
            i += 1;
        }
    }
}
