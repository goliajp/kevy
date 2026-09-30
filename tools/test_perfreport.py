#!/usr/bin/env python3
"""Unit tests for perfgate's arithmetic: counter parsing, per-op lines, verdicts.

Run: python3 -m unittest tools/test_perfreport.py
"""

import pathlib
import sys
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import perfreport as pr  # noqa: E402

PERF_X86 = """\
12000000000,,instructions:u,3000000000,100.00,,
6000000000,,instructions:k,3000000000,100.00,,
9000000000,,cycles,3000000000,100.00,,
11900.5,msec,task-clock,11900500000,100.00,3.967,CPUs utilized
3000000,,raw_syscalls:sys_enter,3000000000,100.00,,
"""

# big.LITTLE: one line per core type, the unused one not counted
PERF_HYBRID = """\
<not counted>,,armv8_cortex_a55/instructions:u/,0,0.00,,
4000,,armv8_cortex_a76/instructions:u/,100,100.00,,
<not counted>,,armv8_cortex_a55/instructions:k/,0,0.00,,
1000,,armv8_cortex_a76/instructions:k/,100,100.00,,
<not counted>,,armv8_cortex_a55/cycles/,0,0.00,,
3000,,armv8_cortex_a76/cycles/,100,100.00,,
2000.0,msec,task-clock,2000000000,100.00,,
"""


def window(cmds=3_000_000, perf=PERF_X86, srv_ticks=1180, gen_ticks=700, box_busy=1900):
    # 16 cpus x 300 ticks (3 s at 100 Hz); busy spread evenly
    cpu = {c: [box_busy // 16, 300] for c in range(16)}
    return {"srv_cpus": "0-3", "secs": 3, "cpu": cpu, "srv_ticks": srv_ticks,
            "gens": [[gen_ticks // 4, 2]] * 4, "cmds": cmds, "wall_ns": 3_000_000_000,
            "perf": pr.parse_perfstat(perf)}


class Counters(unittest.TestCase):
    def test_x86_line_set(self):
        got = pr.parse_perfstat(PERF_X86)
        self.assertEqual(got["instructions:u"], 12e9)
        self.assertEqual(got["raw_syscalls:sys_enter"], 3e6)

    def test_hybrid_lines_are_summed_and_syscalls_optional(self):
        got = pr.parse_perfstat(PERF_HYBRID)
        self.assertEqual(got["instructions:u"], 4000)
        self.assertEqual(got["cycles"], 3000)
        self.assertNotIn("raw_syscalls:sys_enter", got)

    def test_a_missing_required_counter_is_none(self):
        text = PERF_X86.replace("cycles", "bogus")
        self.assertIsNone(pr.parse_perfstat(text))

    def test_a_counter_no_core_counted_is_none(self):
        text = PERF_HYBRID.replace("3000,,armv8_cortex_a76/cycles/,100,100.00,,\n", "")
        self.assertIsNone(pr.parse_perfstat(text))


class Lines(unittest.TestCase):
    def test_per_op_numbers(self):
        w = pr.window_lines(window())
        self.assertAlmostEqual(w["ops"], 1e6)
        self.assertAlmostEqual(w["instr_u"], 4000)
        self.assertAlmostEqual(w["instr_k"], 2000)
        self.assertAlmostEqual(w["instr"], 6000)
        self.assertAlmostEqual(w["cycles"], 3000)
        self.assertAlmostEqual(w["sys"], 1.0)
        self.assertAlmostEqual(w["util"], 11.9005 / 3 / 4)

    def test_foreign_is_what_the_server_and_load_did_not_use(self):
        w = pr.window_lines(window(srv_ticks=1180, gen_ticks=700, box_busy=1920))
        self.assertAlmostEqual(w["foreign"], (1920 - 1180 - 700) / 4800)

    def test_client_share_of_its_threads(self):
        # 4 generators x 2 threads, 300 ticks per cpu: 2400 ticks is all of it
        w = pr.window_lines(window(gen_ticks=2400))
        self.assertAlmostEqual(w["client"], 1.0)

    def test_latency_window(self):
        self.assertEqual(pr.window_lines({"value_us": 240}), {"p95_us": 240.0})

    def test_observation_means_and_worst(self):
        a, b = window(cmds=3_000_000), window(cmds=6_000_000, box_busy=2400)
        o = pr.observation([a, b])
        self.assertAlmostEqual(o["ops"], 1.5e6)
        self.assertAlmostEqual(o["foreign"], max(pr.window_lines(a)["foreign"],
                                                 pr.window_lines(b)["foreign"]))


def pairs(ratios, metric="instr", base=1000.0):
    return [({metric: base}, {metric: base * r}) for r in ratios]


class Verdict(unittest.TestCase):
    def v(self, metric, rs):
        return pr.verdict(metric, rs, {"instr": 1.03, "ops": 0.92}[metric],
                          {"instr": 0.01, "ops": 0.04}[metric])

    def test_every_round_inside_passes_at_any_spread(self):
        self.assertEqual(self.v("instr", [0.95, 1.00, 1.029]), "ok")

    def test_every_round_beyond_fails_at_any_spread(self):
        self.assertEqual(self.v("instr", [1.031, 1.10, 1.20]), "FAIL")

    def test_straddling_with_a_wide_spread_is_noisy(self):
        self.assertEqual(self.v("instr", [1.00, 1.04, 1.05]), "NOISY")

    def test_straddling_inside_the_noise_bound_goes_by_the_median(self):
        self.assertEqual(self.v("instr", [1.025, 1.032, 1.035]), "FAIL")
        self.assertEqual(self.v("instr", [1.025, 1.028, 1.035]), "ok")

    def test_throughput_is_worse_when_lower(self):
        self.assertEqual(self.v("ops", [0.90, 0.91, 0.915]), "FAIL")
        self.assertEqual(self.v("ops", [1.10, 1.20, 1.30]), "ok")

    def test_no_rounds_is_no_verdict(self):
        self.assertEqual(self.v("instr", []), "—")

    def test_ratios_and_spread(self):
        s = pr.summarize(pairs([1.00, 1.02, 1.04]), "instr")
        self.assertAlmostEqual(s["median"], 1.02)
        self.assertAlmostEqual(s["spread"], 0.02)

    def test_a_round_missing_the_metric_is_dropped(self):
        ps = pairs([1.0, 1.1]) + [({"instr": None}, {"instr": 5.0})]
        self.assertEqual(len(pr.ratios(ps, "instr")), 2)


class Bootstrap(unittest.TestCase):
    def test_interval_brackets_the_ratio(self):
        a = [10.0, 10.5, 9.8, 10.2, 10.1]
        b = [5.0, 5.2, 4.9, 5.1, 5.0]
        lo, hi = pr.bootstrap_ratio(a, b)
        self.assertLess(lo, 2.0 + 0.1)
        self.assertGreater(hi, 2.0 - 0.1)
        self.assertLess(lo, hi)


if __name__ == "__main__":
    unittest.main()
