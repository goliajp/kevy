//! Std-only RESP parser fuzz harness.
//!
//! Drives randomized byte streams through [`parse_command`] and
//! asserts that every call terminates in bounded time with one of
//! `Ok(Some)` / `Ok(None)` / `Err(_)`. Never panics, never hangs.
//!
//! 0-dep: uses a fixed-seed PCG-style LCG for determinism — no
//! `rand` crate, no `quickcheck`, no AFL. Each call records the seed
//! that produced it, so any failing input is bit-for-bit reproducible.
//!
//! Strategies:
//! - [`Strategy::Uniform`] — pure random bytes.
//! - [`Strategy::StructuredJunk`] — bytes that look like RESP type
//!   markers (`*`, `$`, `+`, `-`, `:`) followed by garbage.
//! - [`Strategy::MutatedValid`] — a valid SET frame with one random
//!   byte flipped.
//! - [`Strategy::OversizedClaim`] — `*<huge>\r\n` headers without
//!   matching body.
//! - [`Strategy::NegativeLengths`] — `$-99\r\n` / `*-99\r\n` etc.
//!
//! Run with [`run_one`] for one stream + [`run_n`] for a campaign.
//!
//! ```
//! use kevy_resp::fuzz::{FuzzOutcome, Strategy, run_n, run_one};
//!
//! let one = run_one(Strategy::OversizedClaim, 7);
//! assert!(!matches!(one.outcome, FuzzOutcome::Timeout { .. }));
//! run_n(500, 0xC0DE).assert_clean(500);
//! ```

use crate::request::parse_command;

/// Std-only LCG PRNG (MMIX constants). Deterministic per seed.
///
/// The state is private because zero is the generator's fixed point:
/// [`Lcg::new`] is the only way in, and it never admits it.
///
/// ```
/// use kevy_resp::fuzz::Lcg;
///
/// let mut a = Lcg::new(42);
/// let mut b = Lcg::new(42);
/// // the same seed replays the same stream
/// assert_eq!((a.next_u64(), a.next_u8()), (b.next_u64(), b.next_u8()));
/// assert!(a.bound(10) < 10);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Lcg(u64);

impl Lcg {
    /// Seed the generator; zero is replaced, since it is this LCG's
    /// fixed point and would emit the same value forever.
    ///
    /// ```
    /// use kevy_resp::fuzz::Lcg;
    /// assert_eq!(Lcg::new(7).state(), 7);
    /// assert_ne!(Lcg::new(0).state(), 0);
    /// ```
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        // Avoid the zero fixed-point.
        Self(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }
    /// The current state: the seed before the first draw, then the last
    /// value drawn — what a failing run records to be replayed.
    ///
    /// ```
    /// let mut r = kevy_resp::fuzz::Lcg::new(3);
    /// let v = r.next_u64();
    /// assert_eq!(r.state(), v);
    /// ```
    #[must_use]
    pub const fn state(self) -> u64 {
        self.0
    }

    /// The next 64 bits of the stream.
    ///
    /// Deterministic per seed — the same seed replays the same run.
    ///
    /// ```
    /// use kevy_resp::fuzz::Lcg;
    /// let a: Vec<u64> = (0..4).map(|_| Lcg::new(1).next_u64()).collect();
    /// assert!(a.windows(2).all(|w| w[0] == w[1]), "a fresh Lcg(1) always starts the same");
    /// let mut r = Lcg::new(1);
    /// assert_ne!(r.next_u64(), r.next_u64());
    /// ```
    pub fn next_u64(&mut self) -> u64 {
        self.0 =
            self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        self.0
    }
    /// One byte, taken from the high end where this LCG mixes best.
    ///
    /// ```
    /// use kevy_resp::fuzz::Lcg;
    /// let mut r = Lcg::new(42);
    /// let bytes: Vec<u8> = (0..64).map(|_| r.next_u8()).collect();
    /// assert!(bytes.iter().collect::<std::collections::HashSet<_>>().len() > 20);
    /// ```
    pub fn next_u8(&mut self) -> u8 {
        (self.next_u64() >> 24) as u8
    }
    /// Returns a value in `0..bound` (uniform-ish, biased for small
    /// bound; fine for fuzz purposes).
    ///
    /// ```
    /// use kevy_resp::fuzz::Lcg;
    /// let mut r = Lcg::new(5);
    /// assert!((0..100).all(|_| r.bound(6) < 6));
    /// // a zero bound is treated as one
    /// assert_eq!(r.bound(0), 0);
    /// ```
    pub fn bound(&mut self, bound: usize) -> usize {
        (self.next_u64() as usize) % bound.max(1)
    }
}

/// Fuzz strategy. Each picks a different distribution of byte streams.
///
/// ```
/// use kevy_resp::fuzz::{Strategy, generate};
/// for s in Strategy::ALL {
///     // each strategy is deterministic in its seed
///     assert_eq!(generate(s, 11), generate(s, 11));
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Strategy {
    /// Pure uniform random bytes.
    ///
    /// ```
    /// use kevy_resp::fuzz::{Strategy, generate};
    /// assert!(generate(Strategy::Uniform, 3).len() < 2048);
    /// ```
    Uniform,
    /// First byte is a RESP type marker, then junk.
    ///
    /// ```
    /// use kevy_resp::fuzz::{Strategy, generate};
    /// let input = generate(Strategy::StructuredJunk, 3);
    /// assert!(b"*$+-:_;>".contains(&input[0]));
    /// ```
    StructuredJunk,
    /// A valid `SET key value` with one byte flipped.
    ///
    /// ```
    /// use kevy_resp::fuzz::{Strategy, generate};
    /// let valid = b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
    /// let input = generate(Strategy::MutatedValid, 3);
    /// assert_eq!(input.len(), valid.len());
    /// assert!(input.iter().zip(valid).filter(|(a, b)| a != b).count() <= 1);
    /// ```
    MutatedValid,
    /// `*<huge>\r\n` claim header, then short body.
    ///
    /// ```
    /// use kevy_resp::fuzz::{Strategy, generate};
    /// let input = generate(Strategy::OversizedClaim, 3);
    /// assert_eq!(input[0], b'*');
    /// assert!(input.len() < 64 + 16);
    /// ```
    OversizedClaim,
    /// `$-99\r\n` or `*-99\r\n` — negative bulk/array lengths.
    ///
    /// ```
    /// use kevy_resp::fuzz::{Strategy, generate};
    /// let input = generate(Strategy::NegativeLengths, 3);
    /// assert!(input.starts_with(b"$-") || input.starts_with(b"*-") || input[1] == b'0');
    /// ```
    NegativeLengths,
}

impl Strategy {
    /// Every strategy, so a campaign can cover all of them.
    ///
    /// ```
    /// use kevy_resp::fuzz::Strategy;
    /// assert_eq!(Strategy::ALL.len(), 5);
    /// ```
    pub const ALL: [Self; 5] = [
        Self::Uniform,
        Self::StructuredJunk,
        Self::MutatedValid,
        Self::OversizedClaim,
        Self::NegativeLengths,
    ];
    /// One strategy drawn from [`Self::ALL`].
    ///
    /// ```
    /// use kevy_resp::fuzz::{Lcg, Strategy};
    /// let mut r = Lcg::new(9);
    /// assert!(Strategy::ALL.contains(&Strategy::pick(&mut r)));
    /// ```
    pub fn pick(rng: &mut Lcg) -> Self {
        Self::ALL[rng.bound(Self::ALL.len())]
    }
}

/// Generate one fuzz input under the given strategy + seed.
///
/// ```
/// use kevy_resp::fuzz::{Strategy, generate};
/// let input = generate(Strategy::NegativeLengths, 42);
/// assert!(input.ends_with(b"ignored body"));
/// // replaying a seed reproduces the input bit for bit
/// assert_eq!(input, generate(Strategy::NegativeLengths, 42));
/// ```
#[must_use]
pub fn generate(strategy: Strategy, seed: u64) -> Vec<u8> {
    let mut rng = Lcg::new(seed);
    match strategy {
        Strategy::Uniform => {
            let len = rng.bound(2048);
            (0..len).map(|_| rng.next_u8()).collect()
        }
        Strategy::StructuredJunk => {
            let marker = b"*$+-:_;>"[rng.bound(8)];
            let mut out = vec![marker];
            let len = rng.bound(512);
            out.extend((0..len).map(|_| rng.next_u8()));
            out
        }
        Strategy::MutatedValid => {
            // Start from `*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n`.
            let mut out = b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n".to_vec();
            let idx = rng.bound(out.len());
            out[idx] = rng.next_u8();
            out
        }
        Strategy::OversizedClaim => {
            // Claim 10^9 args, but provide ~50 bytes of body.
            let claim = format!("*{}\r\n", rng.next_u64() % 1_000_000_000);
            let mut out = claim.into_bytes();
            let tail_len = rng.bound(64);
            out.extend((0..tail_len).map(|_| rng.next_u8()));
            out
        }
        Strategy::NegativeLengths => {
            let marker = if rng.bound(2) == 0 { '$' } else { '*' };
            let n: i64 = -(rng.bound(99) as i64);
            format!("{marker}{n}\r\nignored body").into_bytes()
        }
    }
}

/// Outcome of one fuzz call.
///
/// ```
/// let r = kevy_resp::fuzz::run_one(kevy_resp::fuzz::Strategy::NegativeLengths, 5);
/// assert!(r.input_len > 0);
/// assert!(!matches!(r.outcome, kevy_resp::fuzz::FuzzOutcome::Timeout { .. }));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct FuzzResult {
    /// Which distribution produced the input.
    ///
    /// ```
    /// use kevy_resp::fuzz::{Strategy, run_one};
    /// assert_eq!(run_one(Strategy::Uniform, 1).strategy, Strategy::Uniform);
    /// ```
    pub strategy: Strategy,
    /// The seed that produced it — replaying this reproduces the call
    /// bit for bit.
    ///
    /// ```
    /// use kevy_resp::fuzz::{Strategy, generate, run_one};
    /// let r = run_one(Strategy::MutatedValid, 99);
    /// assert_eq!(r.seed, 99);
    /// assert_eq!(generate(r.strategy, r.seed).len(), r.input_len);
    /// ```
    pub seed: u64,
    /// Length of the generated input in bytes.
    ///
    /// ```
    /// use kevy_resp::fuzz::{Strategy, generate, run_one};
    /// let r = run_one(Strategy::StructuredJunk, 8);
    /// assert_eq!(r.input_len, generate(Strategy::StructuredJunk, 8).len());
    /// ```
    pub input_len: usize,
    /// What the parser did with it.
    ///
    /// ```
    /// use kevy_resp::fuzz::{FuzzOutcome, Strategy, run_one};
    /// // a claim far above the multi-bulk cap is rejected outright
    /// let r = (0..).map(|seed| run_one(Strategy::OversizedClaim, seed))
    ///     .find(|r| r.outcome == FuzzOutcome::ParseError)
    ///     .expect("some claim exceeds the cap");
    /// assert_eq!(r.strategy, Strategy::OversizedClaim);
    /// ```
    pub outcome: FuzzOutcome,
}

/// What one call to the parser did. Anything outside these four is a
/// failure of the harness's own promise: bounded time, no panic.
///
/// ```
/// use kevy_resp::fuzz::{FuzzOutcome, Strategy, run_one};
/// let outcome = run_one(Strategy::Uniform, 4).outcome;
/// assert!(matches!(
///     outcome,
///     FuzzOutcome::Parsed { .. } | FuzzOutcome::Incomplete | FuzzOutcome::ParseError
/// ));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FuzzOutcome {
    /// Parsed a complete frame; `consumed` ≤ input_len.
    ///
    /// ```
    /// use kevy_resp::fuzz::{FuzzOutcome, Strategy, run_one};
    /// let r = (0..).map(|seed| run_one(Strategy::MutatedValid, seed))
    ///     .find(|r| matches!(r.outcome, FuzzOutcome::Parsed { .. }))
    ///     .expect("some flips leave the frame valid");
    /// assert!(matches!(r.outcome, FuzzOutcome::Parsed { .. }));
    /// ```
    Parsed {
        /// Bytes the parser took, never more than the input.
        ///
        /// ```
        /// use kevy_resp::fuzz::{FuzzOutcome, Strategy, run_one};
        /// let r = (0..).map(|seed| run_one(Strategy::MutatedValid, seed))
        ///     .find(|r| matches!(r.outcome, FuzzOutcome::Parsed { .. }))
        ///     .expect("some flips leave the frame valid");
        /// let FuzzOutcome::Parsed { consumed } = r.outcome else { unreachable!() };
        /// assert!(consumed <= r.input_len);
        /// ```
        consumed: usize,
    },
    /// Incomplete; needs more bytes.
    ///
    /// ```
    /// use kevy_resp::fuzz::{FuzzOutcome, Strategy, run_one};
    /// // a huge claim within the cap waits for elements that never arrive
    /// let found = (0..).map(|seed| run_one(Strategy::OversizedClaim, seed))
    ///     .any(|r| r.outcome == FuzzOutcome::Incomplete);
    /// assert!(found);
    /// ```
    Incomplete,
    /// Parser returned an error (well-formed `ProtocolError`).
    ///
    /// ```
    /// use kevy_resp::fuzz::{FuzzOutcome, Strategy, run_one};
    /// // an array claim above the multi-bulk cap is refused, not attempted
    /// let refused = (0..).map(|seed| run_one(Strategy::OversizedClaim, seed))
    ///     .any(|r| r.outcome == FuzzOutcome::ParseError);
    /// assert!(refused);
    /// ```
    ParseError,
    /// Parser took longer than the per-call timeout — indicates a
    /// runaway. Never observed in correct code; the harness records
    /// the offending seed for reproduction.
    ///
    /// ```
    /// use kevy_resp::fuzz::{FuzzOutcome, PER_CALL_TIMEOUT_MICROS, Strategy, run_one};
    /// let r = run_one(Strategy::Uniform, 1);
    /// if let FuzzOutcome::Timeout { elapsed_micros } = r.outcome {
    ///     panic!("runaway parse: seed {} took {elapsed_micros} µs", r.seed);
    /// }
    /// let runaway = FuzzOutcome::Timeout { elapsed_micros: PER_CALL_TIMEOUT_MICROS + 1 };
    /// assert_ne!(r.outcome, runaway);
    /// ```
    Timeout {
        /// How long it actually took, for the record that names the seed.
        ///
        /// ```
        /// use kevy_resp::fuzz::{FuzzOutcome, PER_CALL_TIMEOUT_MICROS};
        /// let outcome = FuzzOutcome::Timeout { elapsed_micros: 25_000 };
        /// let FuzzOutcome::Timeout { elapsed_micros } = outcome else { unreachable!() };
        /// assert!(elapsed_micros > PER_CALL_TIMEOUT_MICROS);
        /// ```
        elapsed_micros: u128,
    },
}

/// Per-call wall-clock budget. RESP parsing of ≤ 2 KiB inputs should
/// finish in microseconds; 10 ms is a generous ceiling.
///
/// ```
/// assert_eq!(kevy_resp::fuzz::PER_CALL_TIMEOUT_MICROS, 10_000);
/// ```
pub const PER_CALL_TIMEOUT_MICROS: u128 = 10_000;

/// Run one fuzz stream and return its outcome. Nothing here catches a
/// panic: a parser that panics on the generated input panics the caller,
/// which is exactly the failure the campaign exists to surface, with the
/// seed in [`FuzzResult`]'s place replayable through [`generate`].
///
/// ```
/// use kevy_resp::fuzz::{FuzzOutcome, Strategy, run_one};
/// let r = run_one(Strategy::NegativeLengths, 12);
/// assert_eq!(r.strategy, Strategy::NegativeLengths);
/// assert!(!matches!(r.outcome, FuzzOutcome::Timeout { .. }));
/// ```
#[must_use]
pub fn run_one(strategy: Strategy, seed: u64) -> FuzzResult {
    let input = generate(strategy, seed);
    let start = std::time::Instant::now();
    let result = parse_command(&input);
    let elapsed = start.elapsed().as_micros();
    let outcome = if elapsed > PER_CALL_TIMEOUT_MICROS {
        FuzzOutcome::Timeout { elapsed_micros: elapsed }
    } else {
        match result {
            Ok(Some((_, consumed))) => FuzzOutcome::Parsed { consumed },
            Ok(None) => FuzzOutcome::Incomplete,
            Err(_) => FuzzOutcome::ParseError,
        }
    };
    FuzzResult { strategy, seed, input_len: input.len(), outcome }
}

/// What a campaign saw. `total` is the count with a floor under it: a
/// run that parsed nothing is a broken harness, not a clean parser.
///
/// ```
/// let s = kevy_resp::fuzz::run_n(200, 1);
/// assert_eq!(s.total, 200);
/// assert_eq!(s.parsed + s.incomplete + s.errored, s.total - s.timed_out.len() as u64);
/// assert!(s.timed_out.is_empty(), "a timeout is a runaway, not a slow machine");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct Summary {
    /// Calls made.
    ///
    /// ```
    /// assert_eq!(kevy_resp::fuzz::run_n(50, 1).total, 50);
    /// ```
    pub total: u64,
    /// Calls that returned a complete frame.
    ///
    /// ```
    /// // uniform noise often forms an inline command line
    /// assert!(kevy_resp::fuzz::run_n(300, 1).parsed > 0);
    /// ```
    pub parsed: u64,
    /// Calls that asked for more bytes.
    ///
    /// ```
    /// assert!(kevy_resp::fuzz::run_n(300, 1).incomplete > 0);
    /// ```
    pub incomplete: u64,
    /// Calls that returned a well-formed protocol error.
    ///
    /// ```
    /// let s = kevy_resp::fuzz::run_n(300, 1);
    /// assert!(s.errored > 0 && s.errored < s.total);
    /// ```
    pub errored: u64,
    /// `(strategy, seed, micros)` for every call that outran the
    /// per-call budget — each one replayable from its seed.
    ///
    /// ```
    /// // a correct parser never outruns the budget
    /// assert!(kevy_resp::fuzz::run_n(300, 1).timed_out.is_empty());
    /// ```
    pub timed_out: Vec<(Strategy, u64, u128)>,
}

impl Summary {
    /// Strict assertion helper: every call must have produced one of
    /// the three valid outcomes within the per-call timeout, and the
    /// total must match the campaign size.
    ///
    /// # Panics
    ///
    /// Panics when the campaign was not clean: the seed count differs
    /// from `expected_total`, or any call timed out.
    ///
    /// ```
    /// kevy_resp::fuzz::run_n(250, 0xFEED).assert_clean(250);
    /// ```
    pub fn assert_clean(&self, expected_total: u64) {
        assert_eq!(self.total, expected_total, "fuzz campaign skipped seeds");
        assert!(
            self.timed_out.is_empty(),
            "fuzz campaign hit {} timeouts: {:?}",
            self.timed_out.len(),
            self.timed_out
        );
    }
}

#[path = "fuzz_campaign.rs"]
mod campaign;
pub use campaign::run_n;

#[cfg(test)]
#[path = "fuzz_tests.rs"]
mod tests;
