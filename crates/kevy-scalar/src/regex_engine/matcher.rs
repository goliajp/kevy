//! (vendored regex engine — see the parent module.)
#![allow(clippy::all, clippy::pedantic)]
use super::*;

// LOC-WAIVER: vendored spg ERE engine core (byte-identical fork); splitting upstream's tested matcher/parser injects bugs without readability gain.
pub(crate) fn re_match_at(
    node: &ReNode,
    s: &[char],
    pos: usize,
    depth: u32,
    steps: &mut u64,
) -> Result<Option<usize>, ReErr> {
    // abort before the recursive descent can
    // overflow the Rust call stack on an adversarial pattern.
    if depth > MATCH_DEPTH_LIMIT {
        return Err(ReErr::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    // total-work (time) bound: this counter is
    // monotonic across every backtracking branch and start position, so
    // a catastrophic backtracker (shallow depth, exponential paths)
    // fails fast instead of hanging.
    *steps += 1;
    if *steps > MATCH_STEP_LIMIT {
        return Err(ReErr::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    let d = depth + 1;
    match node {
        ReNode::Literal(c) => Ok(if s.get(pos).copied() == Some(*c) {
            Some(pos + 1)
        } else {
            None
        }),
        // PG's ARE is non-newline-sensitive by default,
        // so `.` matches ANY character including `\n` (unlike Perl, where a
        // separate `s`/DOTALL flag is needed). SPG previously excluded `\n`,
        // diverging from PG on multi-line input.
        ReNode::AnyChar => Ok(if pos < s.len() { Some(pos + 1) } else { None }),
        ReNode::Class { members, negated } => match s.get(pos) {
            Some(&c) => {
                let hit = members.iter().any(|m| class_matches(m, c));
                Ok(if hit ^ negated { Some(pos + 1) } else { None })
            }
            None => Ok(None),
        },
        ReNode::Start => Ok(if pos == 0 { Some(pos) } else { None }),
        ReNode::End => Ok(if pos == s.len() { Some(pos) } else { None }),
        ReNode::WordBoundary(kind) => {
            // Zero-width: assert on the flanking chars, consume nothing.
            let before = pos > 0 && is_word_char(s[pos - 1]);
            let after = pos < s.len() && is_word_char(s[pos]);
            let ok = match kind {
                WordBoundaryKind::Boundary => before != after,
                WordBoundaryKind::NonBoundary => before == after,
                WordBoundaryKind::BegWord => !before && after,
                WordBoundaryKind::EndWord => before && !after,
            };
            Ok(if ok { Some(pos) } else { None })
        }
        // Concat delegates to the
        // backtracking sequence matcher so quantifiers can shrink
        // when the tail fails ('bar.*que' now matches 'barbeque';
        // the old stop-gap was greedy-without-backtracking).
        ReNode::Concat(items) => re_match_seq(items, s, pos, d, steps),
        ReNode::Alt(branches) => {
            for b in branches {
                if let Some(p) = re_match_at(b, s, pos, d, steps)? {
                    return Ok(Some(p));
                }
            }
            Ok(None)
        }
        ReNode::Quant {
            inner,
            min,
            max,
            greedy,
        } => {
            // Standalone quantifier (no tail). Greedy → the LONGEST
            // match (match as many reps as fit). Lazy → the FEWEST
            // (match exactly `min` reps, then stop). Tail interaction is
            // handled by re_match_seq; here there is nothing to satisfy
            // beyond the quantifier, so both directions collapse to a
            // single answer.
            let mut count = 0usize;
            let mut p = pos;
            loop {
                // Lazy: once the minimum is reached, take no more reps.
                if !*greedy && count >= *min {
                    break;
                }
                if let Some(cap) = max {
                    if count >= *cap {
                        break;
                    }
                }
                match re_match_at(inner, s, p, d, steps)? {
                    Some(np) if np > p => {
                        p = np;
                        count += 1;
                    }
                    _ => break,
                }
            }
            if count < *min {
                return Ok(None);
            }
            Ok(Some(p))
        }
        ReNode::Lookahead { negative, inner } => {
            // Zero-width: try `inner` at the current position; succeed (consuming
            // nothing) per the positive/negative sense, else fail.
            let hit = re_match_at(inner, s, pos, d, steps)?.is_some();
            Ok(if hit != *negative { Some(pos) } else { None })
        }
        // Stage 1: a capturing group matches transparently
        // (capture recording is threaded in a later stage).
        ReNode::Group { inner, .. } => re_match_at(inner, s, pos, d, steps),
        // A backref never reaches the capture-free path (re_find routes any
        // backref pattern to the caps matcher); defensively fail to match.
        ReNode::Backref { .. } => Ok(None),
    }
}

/// backtracking sequence matcher.
/// Matches `items` in order starting at `pos`; greedy quantifiers
/// try their longest expansion first and shrink until the rest of
/// the sequence matches. Alternations retry the tail per branch.
// LOC-WAIVER: vendored spg ERE engine core (byte-identical fork); splitting upstream's tested matcher/parser injects bugs without readability gain.
pub(crate) fn re_match_seq(
    items: &[ReNode],
    s: &[char],
    pos: usize,
    depth: u32,
    steps: &mut u64,
) -> Result<Option<usize>, ReErr> {
    // same stack-overflow guard as re_match_at.
    if depth > MATCH_DEPTH_LIMIT {
        return Err(ReErr::TypeMismatch {
            detail: "invalid regular expression: regular expression is too complex".into(),
        });
    }
    // total-work (time) bound; see re_match_at.
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
        ReNode::Quant {
            inner,
            min,
            max,
            greedy,
        } => {
            // Enumerate every reachable end position (0, 1, 2, ...
            // repetitions). The reachable set is identical for greedy
            // and lazy; only the ORDER in which we try the tail against
            // those ends differs — greedy tries longest-first (max reps,
            // give back), lazy tries shortest-first (min reps, take
            // more only when the tail fails). Both honor the same
            // `[min, max]` bound and the same step/depth guards.
            let mut ends = vec![pos];
            let mut p = pos;
            let mut count = 0usize;
            loop {
                if let Some(cap) = max {
                    if count >= *cap {
                        break;
                    }
                }
                match re_match_at(inner, s, p, d, steps)? {
                    Some(np) if np > p => {
                        p = np;
                        count += 1;
                        ends.push(p);
                    }
                    _ => break,
                }
            }
            // Try the tail at each reachable rep count. Greedy walks
            // high→low (longest first, give back); lazy walks low→high
            // (shortest first, take more). A single loop keeps this
            // recursive frame small — the P0 `MATCH_DEPTH_LIMIT` no-
            // overflow proof (`redos_deep_match_returns_err_not_overflow`)
            // is calibrated against this frame size.
            let n = ends.len(); // entries for reps = 0 ..= count
            for i in 0..n {
                let reps = if *greedy { n - 1 - i } else { i };
                if reps < *min {
                    // Greedy descends past min → done; lazy ascends past
                    // the below-min reps → skip and keep climbing.
                    if *greedy {
                        break;
                    }
                    continue;
                }
                if let Some(e) = re_match_seq(rest, s, ends[reps], d, steps)? {
                    return Ok(Some(e));
                }
            }
            Ok(None)
        }
        ReNode::Alt(branches) => {
            for b in branches {
                // Each branch may itself contain quantifiers —
                // match it standalone, then retry the tail.
                if let Some(p) = re_match_at(b, s, pos, d, steps)? {
                    if let Some(e) = re_match_seq(rest, s, p, d, steps)? {
                        return Ok(Some(e));
                    }
                }
            }
            Ok(None)
        }
        ReNode::Concat(nested) => {
            // Flatten: nested ++ rest, preserving backtracking
            // across the boundary.
            let mut combined: Vec<ReNode> =
                Vec::with_capacity(nested.len() + rest.len());
            combined.extend(nested.iter().cloned());
            combined.extend(rest.iter().cloned());
            re_match_seq(&combined, s, pos, d, steps)
        }
        other => match re_match_at(other, s, pos, d, steps)? {
            Some(p) => re_match_seq(rest, s, p, d, steps),
            None => Ok(None),
        },
    }
}

/// does the pattern contain a backreference? Such a
/// pattern must run on the capture-aware matcher (the capture-free hot path has
/// no `Caps` to consult).
pub(crate) fn has_backref(node: &ReNode) -> bool {
    match node {
        ReNode::Backref { .. } => true,
        ReNode::Group { inner, .. }
        | ReNode::Quant { inner, .. }
        | ReNode::Lookahead { inner, .. } => has_backref(inner),
        ReNode::Concat(items) | ReNode::Alt(items) => items.iter().any(has_backref),
        _ => false,
    }
}

/// Find the first match of `node` in `s`, starting at or after
/// `from`. Returns the (start, end) char positions of the match.
pub(crate) fn re_find(node: &ReNode, s: &[char], from: usize) -> Result<Option<(usize, usize)>, ReErr> {
    // A backref pattern has no meaning on the capture-free path — route it to
    // the caps matcher and discard the captures.
    if has_backref(node) {
        return Ok(re_find_caps(node, s, from, max_group(node))?.map(|(span, _caps)| span));
    }
    // one monotonic step budget shared across
    // every start position of this find, so total backtracking WORK
    // (time), not just recursion depth, is bounded.
    let mut steps: u64 = 0;
    let mut start = from;
    loop {
        if let Some(end) = re_match_at(node, s, start, 0, &mut steps)? {
            return Ok(Some((start, end)));
        }
        if start >= s.len() {
            return Ok(None);
        }
        start += 1;
    }
}

/// Highest capturing-group index inside `node` (0 = no capturing groups).
pub(crate) fn max_group(node: &ReNode) -> usize {
    match node {
        ReNode::Group { idx, inner } => (*idx).max(max_group(inner)),
        ReNode::Concat(items) | ReNode::Alt(items) => {
            items.iter().map(max_group).max().unwrap_or(0)
        }
        ReNode::Quant { inner, .. } | ReNode::Lookahead { inner, .. } => max_group(inner),
        ReNode::Backref { idx, .. } => *idx,
        _ => 0,
    }
}

// ── capture-aware matcher ──────────────────────────────
//
// A PARALLEL copy of the matcher above, threaded with a capture buffer, used
// ONLY by the group consumers (regexp_replace `\N`, regexp_matches,
// substring(from pattern)). The hot LIKE / `~` path keeps calling the
// capture-free matcher unchanged — so its ReDoS `MATCH_DEPTH_LIMIT`
// no-overflow calibration is untouched. This variant carries two extra
// pointers plus a per-backtrack journal mark, so it runs under its own,
// lower depth bound.
