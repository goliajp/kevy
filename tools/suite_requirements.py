#!/usr/bin/env python3
"""Where each suite requirement is met on this machine, and what the
checks' subprocesses have cost. Split from tools/suite.py."""

import os
import pathlib
import shutil
import subprocess

ROOT = pathlib.Path(__file__).resolve().parent.parent

# ── requirement detection ────────────────────────────────────────────
# Each answers (available, why-not). Detection is cheap and honest:
# where we cannot know, the answer is "not here", said as such.

def _have_binaries(profile):
    """Build the binaries, rather than judge whether those on disk are current.

    Existence alone was not enough: a `target/debug/kevy` from an earlier
    checkout satisfied it while `doc-toml` used that binary to load the
    documentation's config blocks, and reported `packed_rows` as an unknown
    `[server]` key — hours after the source in the same tree had gained it.
    On the box that binary was rebuilt later in the same tier, by
    `workspace-tests`, which is why the same check passed on a re-run.

    The first fix compared the binary's mtime against the sources, and was
    wrong in a way worth recording: cargo decides freshness by hashing
    content, so a `git checkout` or a merge moves mtimes without changing
    anything, and the check cried stale after every branch operation. A gate
    that cries wolf gets worked around.

    So this asks cargo, which is the tool whose job that is. A fresh tree
    costs ~0.1 s; a stale one costs a build, which is the honest price of
    the guarantee.

    Both binaries, because the gates that name this need both and the
    guarantee only ever covered one. A two-day-old `target/release/kevy-cli`
    failed cookbook, crossgate and site-commands in one prerelease run, each
    in a way that reads as a defect in the tree: cookbook reported the CLI
    refusing `-e`, an option the same tree had added. The requirement was
    called `server-*` while ten checks under it drive kevy-cli, which is why
    it is not called that any more.
    """
    flags = ["--release"] if profile == "release" else []
    for pkg, bin_name in (("kevy", "kevy"), ("kevy-cli", "kevy-cli")):
        r = subprocess.run(["cargo", "build", "-p", pkg, "--bin", bin_name, "--quiet", *flags],
                           cwd=ROOT, capture_output=True, text=True)
        if r.returncode != 0:
            tail = (r.stderr or r.stdout).strip().splitlines()
            why = tail[-1] if tail else f"cargo build --bin {bin_name} failed ({r.returncode})"
            return False, f"target/{profile}/{bin_name} does not build: {why[:120]}"
        if not (ROOT / f"target/{profile}/{bin_name}").exists():
            return False, f"cargo build succeeded but target/{profile}/{bin_name} is not there"
    return True, ""


def _have_linux():
    import platform
    return (platform.system() == "Linux"), "not a Linux host"


def _have_box():
    import platform
    if platform.system() != "Linux" or (os_cpus() or 0) < 16:
        return False, "needs the 16-core Linux box (quiet, core-pinnable)"
    # A box gate measures; on a box other work is loading, it measures the
    # neighbours. Busy is NOT-RUN, said with the load, never a number.
    limit = float(os.environ.get("KEVY_SUITE_BOX_LOAD_MAX", "2.0"))
    with open("/proc/loadavg") as f:
        load = float(f.read().split()[0])
    if load > limit:
        return False, f"the box is busy (1-minute load {load:.1f} > {limit:.1f}); a measurement now would measure the other work"
    return True, ""


def os_cpus():
    try:
        return len(os.sched_getaffinity(0))  # type: ignore[attr-defined]
    except AttributeError:
        return os.cpu_count()


def _have_node():
    if shutil.which("node"):
        return True, ""
    return False, "node is not on PATH"


def _have_chromium():
    """A browser, not the library that drives one.

    This asked whether `web/node_modules/playwright-core` existed, which
    is a different question with a different answer: Playwright installs
    its browsers separately from itself. On the bench box — library
    present, browser absent — the requirement read as satisfied and
    sitegate then failed inside `chromium.launch`, leaving a stack trace
    that says nothing about the site and everything about the machine.
    A missing requirement is supposed to be a loud NOT-RUN.

    `web/find-browser.mjs` is the one place that knows where a browser
    is; run directly it prints the path or exits 1, which is exactly this
    question. Asking it rather than restating its rules here keeps the
    runner and `verify.mjs` from ever disagreeing about what counts.
    """
    if not shutil.which("node"):
        return False, "node is not on PATH, so no browser can be located"
    probe = ROOT / "web/find-browser.mjs"
    if not probe.exists():
        return False, f"{probe.relative_to(ROOT)} is missing"
    r = subprocess.run(
        ["node", str(probe)], capture_output=True, text=True, cwd=ROOT, timeout=30
    )
    if r.returncode == 0 and r.stdout.strip():
        return True, ""
    return False, "no Chromium: set CHROME_PATH, or `npx playwright install chromium` in web/"


def _have_web_deps():
    # Distinct from node itself: the box has node and no web/node_modules,
    # and the first box run failed four site checks that should have been
    # honest NOT-RUNs for exactly this gap.
    if (ROOT / "web/node_modules").exists():
        return True, ""
    return False, "web/node_modules is not installed (npm ci in web/)"


def _have_wasm_artifact():
    if (ROOT / "crates/kevy-wasm/pkg/kevy.wasm").exists():
        return True, ""
    return False, "crates/kevy-wasm/pkg/kevy.wasm is not built (npm run engine in web/)"


def _have_docker():
    if not shutil.which("docker"):
        return False, "docker is not on PATH"
    r = subprocess.run(["docker", "info"], capture_output=True, timeout=15)
    return (r.returncode == 0), "docker daemon is not running"


def _have_pgcmp_infra():
    import socket
    try:
        socket.create_connection(("127.0.0.1", 15499), timeout=2).close()
    except OSError:
        return False, "no Postgres on 127.0.0.1:15499 (root starts kevy-pgcmp once; see bench/pgcompare.sh)"
    venv = pathlib.Path.home() / "pgbench-venv/bin/python"
    if not venv.exists():
        return False, "no psycopg venv at ~/pgbench-venv"
    return True, ""


def _have_nightly_rustdoc():
    r = subprocess.run(["rustup", "run", "nightly", "rustdoc", "--version"],
                       capture_output=True, text=True)
    if r.returncode == 0:
        return True, ""
    return False, "no nightly toolchain (rustup toolchain install nightly)"




def _have_semver_checks():
    if shutil.which("cargo-semver-checks"):
        return True, ""
    return False, "cargo-semver-checks is not installed"


# Set when a tier starts: an input another row produces must come from this
# run, not from the copy git tracks.
RUN_STARTED = 0.0


def _fresh_doc_coverage():
    tables = list((ROOT / "target/doc").glob("*.txt"))
    if tables and min(t.stat().st_mtime for t in tables) >= RUN_STARTED:
        return True, ""
    return False, "rustdoc-coverage did not write the tables in target/doc in this run"


def _fresh_dead_set():
    """deadgate's reading of this run's corpus. The file is tracked, so a
    stone report over it without a fresh run reads another tree's corpus."""
    p = ROOT / "bench/DEAD-SET.json"
    if p.exists() and p.stat().st_mtime >= RUN_STARTED:
        return True, ""
    return False, "deadgate did not write bench/DEAD-SET.json in this run"


def _fresh_stone_report():
    """The report stone-report wrote in this run. The file is tracked, so
    existing proves nothing: stonegate would judge the checked-in copy."""
    p = ROOT / "bench/STONE-REPORT.json"
    if p.exists() and p.stat().st_mtime >= RUN_STARTED:
        return True, ""
    return False, "stone-report did not write bench/STONE-REPORT.json in this run"


def _have_targets(*triples):
    r = subprocess.run(["rustup", "target", "list", "--installed"], capture_output=True, text=True)
    missing = [t for t in triples if t not in r.stdout.split()]
    if missing:
        return False, f"rustup target add {' '.join(missing)}"
    return True, ""


def _have_iot_toolchain():
    ok, why = _have_targets("aarch64-unknown-linux-musl", "armv7-unknown-linux-musleabihf",
                            "arm-unknown-linux-musleabihf", "x86_64-unknown-linux-musl",
                            "riscv64gc-unknown-linux-musl", "thumbv7em-none-eabihf")
    if not ok:
        return ok, why
    missing = [t for t in ("riscv64-linux-gnu-gcc", "qemu-system-arm") if not shutil.which(t)]
    if missing:
        return False, f"not installed: {', '.join(missing)}"
    return True, ""


def _have_miri():
    r = subprocess.run(["rustup", "component", "list", "--toolchain", "nightly", "--installed"],
                       capture_output=True, text=True)
    if any(l.startswith("miri") for l in r.stdout.splitlines()):
        return True, ""
    return False, "rustup component add miri rust-src --toolchain nightly"


def _fresh_web_dist():
    """The site site-build wrote in this run; a dist left from an earlier
    build is a different tree's site."""
    p = ROOT / "web/dist"
    if p.exists() and p.stat().st_mtime >= RUN_STARTED:
        return True, ""
    return False, "site-build did not write web/dist in this run"


PROBES = {
    "web/dist from site-build": lambda: _fresh_web_dist(),
    "wasm targets": lambda: _have_targets("wasm32-unknown-unknown", "wasm32-wasip1"),
    "iot toolchain": lambda: _have_iot_toolchain(),
    "nightly miri": lambda: _have_miri(),
    "binaries-debug": lambda: _have_binaries("debug"),
    "binaries-release": lambda: _have_binaries("release"),
    "linux": lambda: _have_linux(),
    "box": lambda: _have_box(),
    "node": lambda: _have_node(),
    "chromium": lambda: _have_chromium(),
    "docker": lambda: _have_docker(),
    "web-deps": lambda: _have_web_deps(),
    "pgcmp-infra": lambda: _have_pgcmp_infra(),
    "wasm-artifact": lambda: _have_wasm_artifact(),
    "device": lambda: _have_device(),
    "nightly rustdoc": lambda: _have_nightly_rustdoc(),
    "rustdoc coverage tables from rustdoc-coverage": lambda: _fresh_doc_coverage(),
    "bench/DEAD-SET.json from deadgate": lambda: _fresh_dead_set(),
    "cargo-semver-checks": lambda: _have_semver_checks(),
    "bench/STONE-REPORT.json from stone-report": lambda: _fresh_stone_report(),
    "ci": lambda: (False, "runs in CI, not locally"),
}


def _have_device():
    if os.environ.get("KEVY_DEVICE") == "1":
        return True, ""
    return False, "no device session (set KEVY_DEVICE=1 on the machine that has one)"


def children_cpu():
    """CPU the checks' own subprocesses have used, user plus system.

    A duration is only a cost when the machine was the check's. `cargo test
    -p kevy --test differential_server_vs_embedded` took 5.9s in prerelease
    and 578.2s in full, from the same tree minutes apart: run by hand it is
    0.14s of test inside 27s of wall and 1.9s of CPU, because another cargo
    on this machine held the target lock. Recording wall alone makes that row
    read as a check that costs ten minutes.
    """
    t = os.times()
    return t.children_user + t.children_system


def requirement_gap(check):
    """The first unmet requirement, or None."""
    for r in check.get("requires", []):
        ok, why = PROBES[r]()
        if not ok:
            return f"{r}: {why}"
    return None
