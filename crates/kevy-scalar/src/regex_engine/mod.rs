//! POSIX-ERE regex engine — forked from the golia sibling project spg
//! (`spg-engine`'s `eval/regexp.rs`, a no_std hand-written matcher), the
//! engine CORE only: the `ReNode` AST, the parser, and the backtracking
//! matcher with capture groups. spg's function wrappers (which spoke
//! its `Value` type) are NOT ported — kevy's live in `super::regexp`,
//! returning `Scalar::Text`. The only adaptation to the vendored code
//! is the error type (`EvalError::TypeMismatch{detail}` → this module's
//! `ReErr`) and the std/alloc split; the matcher is byte-identical.
//!
//! kevy is zero-dependency, so this is why the engine is vendored
//! rather than pulled from crates.io. Validation: the pg_regress
//! `33_regexp_family` probes run it end to end through funcgate, plus
//! the unit tests below.
#![allow(clippy::all, clippy::pedantic)]

mod caps;
mod classes;
mod matcher;
mod parse;
mod parse_class;

// Cross-submodule visibility: every helper is `pub(crate)`; these
// globs let each submodule's `use super::*` reach its siblings, and
// the wrapper module (`super::regexp`) reach the entry points.
#[allow(unused_imports)]
pub(crate) use caps::*;
#[allow(unused_imports)]
pub(crate) use classes::*;
#[allow(unused_imports)]
pub(crate) use matcher::*;
#[allow(unused_imports)]
pub(crate) use parse::*;
#[allow(unused_imports)]
pub(crate) use parse_class::*;

/// The engine's internal error (a parse failure), mapped to
/// `ScalarError` at the wrapper boundary in `super::regexp`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) enum ReErr {
    /// A malformed pattern — carries PG's own phrasing.
    TypeMismatch { detail: String },
}

/// pattern trips this cleanly rather than overflowing. 100 nested
/// groups is still far beyond any real pattern.
pub(crate) const PARSE_DEPTH_LIMIT: u32 = 100;

/// PG's `DUPMAX` — the largest `{m,n}` repetition count accepted. A
/// bound above this is rejected as an invalid regular expression at
/// parse time, before the matcher can act on it.
pub(crate) const REPEAT_MAX: u32 = 0x0000_FFFF; // 65535

/// Maximum recursive-descent depth of the backtracking matcher
/// (`re_match_at` / `re_match_seq`) before it aborts with a clean
/// "too complex" error rather than overflowing the Rust call stack.
///
/// Chosen conservatively: matcher recursion depth grows with concat
/// length and nested group/alternation depth (bounded `{m,n}`
/// quantifiers iterate rather than recurse, so they cost no depth), so
/// the ceiling must sit below the frames that fit in a modest
/// worker-thread stack. The `redos_deep_match_returns_err_not_overflow`
/// test proves that a pattern driven past this depth returns `Err` —
/// not a stack overflow — even on a 1 MiB stack (well under tokio's
/// 2 MiB / pthread's 8 MiB defaults). It is still far above any
/// legitimate pattern's backtracking depth (real patterns are tens of
/// tokens, not hundreds).
pub(crate) const MATCH_DEPTH_LIMIT: u32 = 500;

/// Maximum total number of backtracking steps (matcher entries) the
/// engine will spend on a single `re_find` invocation before it aborts
/// with a clean "too complex" error.
///
/// this is the TIME bound, independent of and
/// complementary to `MATCH_DEPTH_LIMIT` (the STACK bound). Catastrophic
/// backtracking (`(a+)+$`, `(a|aa)*b`, …) recurses only shallowly but
/// explores exponentially many paths, so a depth cap alone leaves the
/// matcher able to burn CPU without limit. A single monotonic counter,
/// incremented once per `re_match_at`/`re_match_seq` entry and shared
/// across ALL backtracking branches and ALL start positions of one
/// find, caps the total work so a runaway pattern fails fast instead of
/// hanging the connection thread.
///
/// Chosen generously: 10 million steps is orders of magnitude above any
/// legitimate pattern×input (a linear, non-pathological match spends
/// roughly O(pattern × input) entries — thousands, not millions, even
/// for large inputs), yet a modern core executes it in well under a
/// second, so an exponential backtracker aborts near-instantly. The
/// `redos_catastrophic_backtracking_returns_err_fast` test proves a
/// classic ReDoS pattern trips this bound quickly rather than hanging.
pub(crate) const MATCH_STEP_LIMIT: u64 = 10_000_000;

/// the largest input length
/// for which the dot-repetition length short-circuit is engaged.
///
/// The short-circuit replaces the backtracker for a whole-string match
/// against a fully-anchored pure dot-repetition (`^.*$`, `^.{m,n}$`, …).
/// It must produce byte-for-byte the SAME answer the backtracker would —
/// including the backtracker's ReDoS `MATCH_STEP_LIMIT` behavior. For a
/// pattern with at most one variable-width quantifier (the restriction
/// `matchall_length_bounds` enforces) the backtracker spends O(len)
/// steps, so gating the short-circuit at a length well below the step
/// budget guarantees the backtracker would NOT have erred at this input —
/// hence the short-circuit's bool is exactly the backtracker's bool.
/// Above this length we fall through to the backtracker unchanged, so
/// whatever it does (match / non-match / step-budget error) is preserved.
/// 2_000_000 leaves a ~5× margin under the 10M step budget.



#[derive(Debug, Clone)]
pub(crate) enum ReNode {
    /// Single literal byte. ASCII fast-path; non-ASCII falls through
    /// to Any since the engine doesn't decode UTF-8 here.
    Literal(char),
    /// Any single character.
    AnyChar,
    /// Character class: (positive members list, negated flag).
    Class {
        members: Vec<ClassMember>,
        negated: bool,
    },
    /// Anchor start.
    Start,
    /// Anchor end.
    End,
    /// Word-boundary zero-width assertion (PG ARE `\y \m \M \b \B \Y`).
    /// Consumes no input; asserts on the word-ness of the chars flanking
    /// the current position. See `WordBoundaryKind`.
    WordBoundary(WordBoundaryKind),
    /// Repetition quantifier. `greedy` = the PG default (`X*`, `X+`,
    /// `X?`, `X{m,n}`): match as MANY reps as possible, give back on
    /// backtrack. `greedy == false` is the lazy / non-greedy form (the
    /// `?`-suffixed `X*?`, `X+?`, `X??`, `X{m,n}?`): match as FEW reps
    /// as possible, take more only when the continuation fails. Both
    /// forms reach the SAME set of end positions and run under the SAME
    /// ReDoS step/depth guards — only the order the matcher tries those
    /// positions differs (longest-first vs shortest-first).
    Quant {
        inner: Box<ReNode>,
        min: usize,
        max: Option<usize>,
        greedy: bool,
    },
    /// Concatenation of sub-nodes.
    Concat(Vec<ReNode>),
    /// Alternation.
    Alt(Vec<ReNode>),
    /// Zero-width lookahead assertion. `(?=inner)` (negative == false) succeeds
    /// at a position iff `inner` matches starting there; `(?!inner)` (negative
    /// == true) succeeds iff it does NOT. Either way it consumes no input. Runs
    /// under the same ReDoS step/depth budget as every other node.
    Lookahead { negative: bool, inner: Box<ReNode> },
    /// a capturing group `(inner)`. `idx` is the 1-based
    /// group number (assigned left-to-right at parse time). Matching is
    /// transparent — it matches exactly what `inner` matches — but the matcher
    /// additionally records the `[start, end)` span it spanned into the
    /// captures array so regexp_replace `\N`, regexp_matches and
    /// substring(from pattern) can read the sub-match. `(?:…)` non-capturing
    /// groups and lookarounds are NOT wrapped in this node.
    Group { idx: usize, inner: Box<ReNode> },
    /// in-pattern backreference `\1`..`\9`: matches the
    /// literal text captured by group `idx`. `ci` is set by `fold_case` for the
    /// `~*` case-insensitive path (the comparison folds both sides).
    Backref { idx: usize, ci: bool },
}

#[derive(Debug, Clone)]
pub(crate) enum ClassMember {
    Single(char),
    Range(char, char),
    /// A shortcut-class complement used *inside* a bracket expression:
    /// `[\D]`, `[\W]`, `[\S]`. Matches iff the char is NOT in any of the
    /// held sub-members. PG ARE recognises these shortcuts within
    /// `[...]` (e.g. `[\D]` = a non-digit); the positive forms
    /// (`\d`/`\w`/`\s`) expand inline into ordinary Single/Range members
    /// and need no variant. Union semantics across the whole class are
    /// preserved: `[a\D]` matches `'a'` OR any non-digit.
    NotInSet(Vec<ClassMember>),
}

/// PG ARE word-boundary assertion flavours (regc_locale.c semantics).
/// A "word character" is `[[:alnum:]_]`; `before`/`after` = whether the
/// char immediately left/right of the position is a word char (false at a
/// string edge). All are zero-width — they match a position, not a char.
/// (PG's `\b`/`\B` are NOT word boundaries in ARE — they are the backspace
/// char and a literal backslash respectively — so they are not here.)
#[derive(Debug, Clone, Copy)]
pub(crate) enum WordBoundaryKind {
    /// `\y` — at a word boundary: `before != after`.
    Boundary,
    /// `\Y` — NOT a word boundary: `before == after`.
    NonBoundary,
    /// `\m` — beginning of a word: `!before && after`.
    BegWord,
    /// `\M` — end of a word: `before && !after`.
    EndWord,
}

/// PG word character: alphanumeric or underscore. ASCII-scoped to match
/// this engine's `\w`/`[[:alnum:]]` handling (both ASCII-only here).
pub(crate) fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}


pub(crate) const CAP_MATCH_DEPTH_LIMIT: u32 = 300;

pub(crate) type Caps = Vec<Option<(usize, usize)>>;

/// a match span `(start, end)` plus its capture groups.
pub(crate) type MatchWithCaps = ((usize, usize), Caps);
/// Undo log: `(group index, previous value)` recorded before each write, so a
/// failed backtrack branch restores exactly the captures it overwrote.
pub(crate) type CapJournal = Vec<(usize, Option<(usize, usize)>)>;
