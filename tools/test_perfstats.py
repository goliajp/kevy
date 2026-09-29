#!/usr/bin/env python3
"""Unit tests for the perf gate's statistics and sample rules.

Run: python3 -m unittest tools/test_perfstats.py
"""

import datetime
import math
import pathlib
import random
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import perfstats as ps  # noqa: E402
import perfwindow as pw  # noqa: E402

P = dict(ps.DEFAULT_SPRT)
DELTA_C = ps.delta_for("C.instr_u", ps.DEFAULT_BANDS)
TOPO = {"srv_cpus": "0-3", "idle_siblings": "8-11", "cli_cpus": "4-7,12-15",
        "cli_threads": 8}


def noisy(shift, sigma, n, seed):
    rng = random.Random(seed)
    return [math.log(1 + shift) + rng.gauss(0, sigma) for _ in range(n)]


class Sprt(unittest.TestCase):
    def test_three_percent_shift_is_red_and_confirmed(self):
        ds = noisy(0.03, 0.005, 20, seed=7)
        r = ps.judge_line(ds, DELTA_C, 0.005, P)
        self.assertEqual(r["state"], "red")
        self.assertEqual(r["confirm"]["state"], "red")
        # decided at n_min, confirmed on the next three pairs
        self.assertEqual(r["n"], P["n_min"] + P["confirm_pairs"])

    def test_zero_shift_with_calibrated_sigma_is_green(self):
        for seed in range(50):
            ds = noisy(0.0, 0.005, 12, seed=seed)
            r = ps.judge_line(ds, DELTA_C, 0.005, P)
            self.assertEqual(r["state"], "green", seed)
            self.assertEqual(r["n"], P["n_min"])

    def test_shift_inside_the_band_is_undecided_at_n_max(self):
        # half the band: each pair adds (almost) nothing either way
        ds = [DELTA_C / 2] * P["n_max"]
        r = ps.judge_line(ds, DELTA_C, 0.02, P)
        self.assertEqual(r["state"], "undecided")
        self.assertEqual(ps.finalize(r, len(ds), P["n_min"]), "undecided")

    def test_fewer_than_n_max_pairs_keep_running(self):
        r = ps.judge_line([DELTA_C / 2] * 5, DELTA_C, 0.02, P)
        self.assertEqual(r["state"], "running")
        self.assertTrue(ps.wants_more(r))

    def test_no_decision_before_n_min(self):
        state, n, _ = ps.sprt([0.2, 0.2, 0.2], DELTA_C, 0.001, 0.005, 0.005, 3, 12)
        self.assertEqual((state, n), ("red", 3))
        state, _, _ = ps.sprt([0.2, 0.2], DELTA_C, 0.001, 0.005, 0.005, 3, 12)
        self.assertEqual(state, "running")

    def test_red_waits_for_confirmation(self):
        ds = [0.03] * 4
        r = ps.judge_line(ds, DELTA_C, 0.005, P)
        self.assertEqual(r["state"], "confirming")
        self.assertTrue(ps.wants_more(r))
        self.assertEqual(ps.finalize(r, 4, P["n_min"]), "undecided")

    def test_unreproduced_red_is_green(self):
        ds = [0.03] * 3 + [0.0] * 3
        r = ps.judge_line(ds, DELTA_C, 0.005, P)
        self.assertEqual(r["confirm"]["state"], "green")
        self.assertEqual(r["state"], "green")

    def test_contradictory_confirmation_is_undecided(self):
        ds = [0.03] * 3 + [DELTA_C / 2] * 3
        r = ps.judge_line(ds, DELTA_C, 0.005, P)
        self.assertEqual(r["state"], "undecided")

    def test_too_few_valid_pairs_is_not_judged(self):
        r = ps.judge_line([0.0, 0.0], DELTA_C, 0.005, P)
        self.assertEqual(ps.finalize(r, 2, P["n_min"]), "not-judged")

    def test_orientation(self):
        self.assertGreater(ps.pair_d("T.ops", 100.0, 90.0), 0)
        self.assertGreater(ps.pair_d("C.instr_u", 100.0, 110.0), 0)
        self.assertGreater(ps.pair_d("S.util", 0.98, 0.90), 0)
        self.assertAlmostEqual(ps.delta_for("T.ops", ps.DEFAULT_BANDS), -math.log(0.92))
        self.assertAlmostEqual(ps.delta_for("S.util", ps.DEFAULT_BANDS), 0.03)

    def test_throughput_drop_is_red(self):
        delta = ps.delta_for("T.ops", ps.DEFAULT_BANDS)
        ds = [ps.pair_d("T.ops", 100.0, 88.0)] * 6
        self.assertEqual(ps.judge_line(ds, delta, 0.035, P)["state"], "red")

    def test_ci99_contains_the_mean(self):
        lo, hi = ps.ci99([0.01, 0.02, 0.03], 0.01)
        self.assertLess(lo, 0.02)
        self.assertGreater(hi, 0.02)


class Sigma(unittest.TestCase):
    TODAY = datetime.date(2026, 10, 1)

    def base(self, **kw):
        b = {"sigma": {"a": {"C.instr_u": 0.004, "T.ops": None}},
             "sigma_fingerprint": "fp", "sigma_recorded": "2026-09-30",
             "sigma_valid_days": 90}
        b.update(kw)
        return b

    def test_present(self):
        self.assertEqual(ps.sigma_for(self.base(), "a", "C.instr_u", "fp", self.TODAY), 0.004)

    def test_explicit_null_is_returned(self):
        self.assertIsNone(ps.sigma_for(self.base(), "a", "T.ops", "fp", self.TODAY))

    def test_missing_refuses(self):
        with self.assertRaises(ps.Refused):
            ps.sigma_for(self.base(), "a", "S.cyc", "fp", self.TODAY)
        with self.assertRaises(ps.Refused):
            ps.sigma_for(self.base(sigma={}), "a", "C.instr_u", "fp", self.TODAY)

    def test_zero_sigma_refuses(self):
        b = self.base(sigma={"a": {"C.instr_u": 0.0}})
        with self.assertRaises(ps.Refused):
            ps.sigma_for(b, "a", "C.instr_u", "fp", self.TODAY)

    def test_stale_refuses(self):
        b = self.base(sigma_recorded="2026-06-01")
        with self.assertRaises(ps.Refused):
            ps.sigma_for(b, "a", "C.instr_u", "fp", self.TODAY)

    def test_other_topology_refuses(self):
        with self.assertRaises(ps.Refused):
            ps.sigma_for(self.base(), "a", "C.instr_u", "other", self.TODAY)

    def test_undated_refuses(self):
        with self.assertRaises(ps.Refused):
            ps.sigma_for(self.base(sigma_recorded=None), "a", "C.instr_u", "fp", self.TODAY)

    def test_calibration_needs_enough_pairs(self):
        self.assertIsNone(ps.sigma_of([0.01] * 5))
        self.assertAlmostEqual(ps.sigma_of(noisy(0, 0.01, 2000, 3)), 0.01, places=3)


class Bootstrap(unittest.TestCase):
    def test_interval_brackets_the_ratio(self):
        rng = random.Random(5)
        b = [100 + rng.gauss(0, 3) for _ in range(15)]
        a = [2 * x + rng.gauss(0, 3) for x in b]
        lo, hi = ps.bootstrap_ratio(a, b, iters=2000)
        self.assertLess(lo, 2.0)
        self.assertGreater(hi, 2.0)
        self.assertGreater(lo, 1.9)

    def test_deterministic(self):
        a, b = [1.0, 2.0, 3.0], [1.0, 1.0, 1.0]
        self.assertEqual(ps.bootstrap_ratio(a, b, iters=500),
                         ps.bootstrap_ratio(a, b, iters=500))

    def test_unpaired_rejected(self):
        with self.assertRaises(ValueError):
            ps.bootstrap_ratio([1.0], [1.0, 2.0])


def window(**over):
    cpu = {c: [0, 300] for c in range(16)}
    for c in range(4):
        cpu[c] = [297, 300]
    for c in (4, 5, 6, 7, 12, 13, 14, 15):
        cpu[c] = [200, 300]
    w = {"cpu": cpu, "srv_ticks": 1188, "gens": [[1600, 8]], "cmds": 15_000_000,
         "wall_ns": 3_000_000_000, "secs": 3,
         "perf": {"instructions:u": 60e9, "instructions:k": 20e9, "cycles": 50e9,
                  "task-clock": 11_900.0, "raw_syscalls:sys_enter": 1e6}}
    w.update(over)
    return w


class Window(unittest.TestCase):
    def test_clean_window_is_kept(self):
        v = pw.classify(window(), TOPO)
        self.assertIsNone(v["discard"])
        self.assertTrue(v["c_ok"])
        self.assertTrue(v["t_ok"])

    def test_busy_sibling_discards(self):
        w = window()
        w["cpu"][9] = [9, 300]
        self.assertIn("sibling", pw.classify(w, TOPO)["discard"])

    def test_foreign_on_server_cores_discards(self):
        w = window(srv_ticks=1100)
        self.assertIn("server cpus", pw.classify(w, TOPO)["discard"])

    def test_client_bound_discards(self):
        w = window(gens=[[2100, 8]])
        self.assertIn("client-bound", pw.classify(w, TOPO)["discard"])
        w = window(gens=[[270, 1], [100, 1]])
        self.assertIn("client-bound", pw.classify(w, TOPO)["discard"])

    def test_unsaturated_withholds_c(self):
        w = window()
        w["perf"] = dict(w["perf"], **{"task-clock": 10_000.0})
        v = pw.classify(w, TOPO)
        self.assertIsNone(v["discard"])
        self.assertFalse(v["c_ok"])

    def test_box_noise_withholds_t_only(self):
        w = window()
        for c in (4, 5, 6, 7, 12, 13, 14, 15):
            w["cpu"][c] = [280, 300]
        v = pw.classify(w, TOPO)
        self.assertIsNone(v["discard"])
        self.assertFalse(v["t_ok"])
        self.assertTrue(v["c_ok"])

    def test_lines_use_rates(self):
        w = window(wall_ns=3_030_000_000, cmds=15_150_000)
        lines = pw.window_lines(w, TOPO)
        self.assertAlmostEqual(lines["C.instr_u"], 60e9 / 3 / 5e6)
        self.assertAlmostEqual(lines["S.util"], 11.9 / 3 / 4)

    def test_pair_lines(self):
        ok = {"lines": {"C.instr_u": 1.0, "T.ops": 1.0, "S.util": 0.99},
              "c_ok": True, "t_ok": True}
        low = dict(ok, c_ok=False, t_ok=False)
        self.assertEqual(pw.pair_lines(ok, ok, ["C.instr_u", "T.ops", "S.util"]),
                         ["C.instr_u", "T.ops", "S.util"])
        self.assertEqual(pw.pair_lines(ok, low, ["C.instr_u", "T.ops", "S.util"]),
                         ["S.util"])

    def test_perfstat_parse(self):
        text = ("1000,,instructions:u,5,100.00,,\n200,,instructions:k,5,100.00,,\n"
                "900,,cycles,5,100.00,,\n12.5,msec,task-clock,5,100.00,,\n"
                "7,,raw_syscalls:sys_enter,5,100.00,,\n")
        got = pw.parse_perfstat(text)
        self.assertEqual(got["task-clock"], 12.5)
        self.assertIsNone(pw.parse_perfstat(text.replace("200,", "<not counted>,")))
        self.assertIsNone(pw.parse_perfstat("1000,,cycles,5,100.00,,\n"))

    def test_proc_stat(self):
        a = pw.parse_proc_stat("cpu  1 2 3 4 5 6 7 8\ncpu0 10 0 10 100 5 0 0 0 0 0\n")
        b = pw.parse_proc_stat("cpu  1 2 3 4 5 6 7 8\ncpu0 20 0 15 180 5 1 0 0 0 0\n")
        self.assertEqual(pw.proc_stat_delta(a, b), {0: [16, 96]})
        self.assertEqual(pw.cpu_list("0-2,8,12-13"), [0, 1, 2, 8, 12, 13])


if __name__ == "__main__":
    unittest.main()
