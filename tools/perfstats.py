#!/usr/bin/env python3
"""Statistics behind the perf gate: paired differences, SPRT, sigma, bootstrap.

Pure functions only; bench/perfgate2_judge.py does the file I/O.

A regression verdict is a sequential probability ratio test on paired
differences. Each pair is a reference observation and a candidate
observation taken back to back (the order alternates, ABBA), and its
difference d is oriented so that a positive d is always worse:

    cost lines (C.*, S.cyc, L.us)   d = ln(cand / ref)
    throughput (T.ops)              d = ln(ref / cand)
    utilisation (S.util)            d = ref - cand    (fraction, not a ratio)

Under H0 the pair mean is 0, under H1 it is delta, the line's band. Sigma is
the pair-to-pair standard deviation measured by an A/A calibration of the same
binary on both sides; the gate never guesses it.
"""

import math
import random
import statistics

LINES = ("C.instr_u", "C.instr_k", "C.sys", "S.cyc", "S.util", "T.ops", "L.us")
COST_LINES = ("C.instr_u", "C.instr_k", "C.sys", "S.cyc", "L.us")
C_LINES = ("C.instr_u", "C.instr_k", "C.sys")

DEFAULT_BANDS = {"C.instr_u": 1.03, "C.instr_k": 1.03, "C.sys": 1.03,
                 "S.cyc": 1.05, "S.util": -0.03, "T.ops": 0.92, "L.us": 0.83}
DEFAULT_SPRT = {"alpha": 0.005, "beta": 0.005, "n_min": 3, "n_max": 12,
                "confirm_pairs": 3}
# fewer valid A/A pairs than this and a sigma is not an estimate
MIN_CALIBRATION_PAIRS = 6


class Refused(Exception):
    """The gate cannot judge; the message says what to fix."""


def pair_d(line, ref, cand):
    """Oriented paired difference: positive means the candidate is worse."""
    if line == "S.util":
        return ref - cand
    if line == "T.ops":
        return math.log(ref / cand)
    return math.log(cand / ref)


def delta_for(line, bands):
    """The H1 shift for a line, on the same scale as pair_d."""
    band = bands[line]
    return abs(band) if line == "S.util" else abs(math.log(band))


def thresholds(alpha, beta):
    return math.log((1 - beta) / alpha), math.log(beta / (1 - alpha))


def llr_step(d, delta, sigma):
    """Log-likelihood ratio increment of one pair, Gaussian with known sigma."""
    return (delta / sigma ** 2) * (d - delta / 2)


def sprt(ds, delta, sigma, alpha, beta, n_min, n_max):
    """Run the test over ds in order.

    Returns (state, n, llr): state is 'red' or 'green' with n the pair at which
    the boundary was crossed, 'undecided' when n_max pairs crossed nothing, or
    'running' when fewer than n_max pairs have been seen.
    """
    upper, lower = thresholds(alpha, beta)
    llr = 0.0
    for i, d in enumerate(ds[:n_max], start=1):
        llr += llr_step(d, delta, sigma)
        if i >= n_min and llr >= upper:
            return "red", i, llr
        if i >= n_min and llr <= lower:
            return "green", i, llr
    state = "undecided" if len(ds) >= n_max else "running"
    return state, min(len(ds), n_max), llr


def judge_line(ds, delta, sigma, p):
    """Stage one SPRT; a red is only a red when an independent SPRT on the
    next confirm_pairs pairs is red as well.

    States: green, red, undecided (evidence exhausted or contradictory),
    running / confirming (more pairs would change the answer).
    """
    state, n, llr = sprt(ds, delta, sigma, p["alpha"], p["beta"],
                         p["n_min"], p["n_max"])
    out = {"state": state, "n": n, "llr": llr, "confirm": None}
    if state != "red":
        return out
    extra = ds[n:n + p["confirm_pairs"]]
    if len(extra) < p["confirm_pairs"]:
        out["state"] = "confirming"
        return out
    cp = p["confirm_pairs"]
    c_state, _, c_llr = sprt(extra, delta, sigma, p["alpha"], p["beta"], cp, cp)
    out["confirm"] = {"state": c_state, "llr": c_llr}
    # a first-stage red that the confirmation contradicts was a draw
    out["state"] = {"red": "red", "green": "green"}.get(c_state, "undecided")
    out["n"] = n + cp
    return out


def finalize(result, n_valid, n_min):
    """The verdict once no more pairs will be taken."""
    state = result["state"]
    if state in ("green", "red", "undecided"):
        return state
    if state == "running" and n_valid < n_min:
        return "not-judged"
    return "undecided"


def wants_more(result):
    return result["state"] in ("running", "confirming")


def ci99(ds, sigma):
    """99% interval of the mean pair difference with the calibrated sigma."""
    if not ds:
        return None
    m = statistics.fmean(ds)
    half = 2.5758 * sigma / math.sqrt(len(ds))
    return m - half, m + half


def sigma_of(ds):
    """A/A pair spread; None when there are too few pairs to call it one."""
    if len(ds) < MIN_CALIBRATION_PAIRS:
        return None
    return statistics.stdev(ds)


def sigma_for(baseline, angle, line, fingerprint, today):
    """The calibrated sigma for one angle's line, or Refused.

    An explicit null means calibration ran and could not measure this line
    (for example an angle that never saturates the server): the line is not
    judged, and the report says so. A missing entry, a sigma from another
    topology, or one older than sigma_valid_days is a refusal: no default.
    """
    table = baseline.get("sigma") or {}
    if angle not in table or line not in table[angle]:
        raise Refused(f"no calibrated sigma for {angle} {line} — run --calibrate")
    if baseline.get("sigma_fingerprint") != fingerprint:
        raise Refused("sigma was calibrated on another topology "
                      f"({baseline.get('sigma_fingerprint')!r} != {fingerprint!r}) "
                      "— run --calibrate")
    recorded = baseline.get("sigma_recorded")
    if not recorded:
        raise Refused("sigma has no calibration date — run --calibrate")
    age = (today - _date(recorded)).days
    if age > baseline.get("sigma_valid_days", 90):
        raise Refused(f"sigma is {age} days old (limit "
                      f"{baseline.get('sigma_valid_days', 90)}) — run --calibrate")
    return table[angle][line]


def _date(text):
    import datetime
    return datetime.date.fromisoformat(text[:10])


def bootstrap_ratio(a, b, level=0.99, iters=10000, seed=1):
    """Paired bootstrap interval of mean(a) / mean(b).

    a[i] and b[i] were taken in the same round and slot, so they are resampled
    together: whatever moved the box in that slot moves both.
    """
    if len(a) != len(b) or not a:
        raise ValueError("paired samples must be non-empty and equal length")
    rng = random.Random(seed)
    n = len(a)
    ratios = []
    for _ in range(iters):
        idx = [rng.randrange(n) for _ in range(n)]
        ratios.append(sum(a[i] for i in idx) / sum(b[i] for i in idx))
    ratios.sort()
    tail = (1 - level) / 2
    lo = ratios[int(math.floor(tail * (iters - 1)))]
    hi = ratios[int(math.ceil((1 - tail) * (iters - 1)))]
    return lo, hi


def fingerprint(lscpu_text, topology):
    """Topology identity: the CPU map plus the pinning this gate uses."""
    import hashlib
    import json
    block = json.dumps({k: v for k, v in topology.items() if k != "fingerprint"},
                       sort_keys=True)
    h = hashlib.sha256((lscpu_text.strip() + "\n" + block).encode())
    return h.hexdigest()[:16]
