#!/usr/bin/env python3
"""The llvm side of the dead-region atlas: region records merged into
source regions, reconciled against llvm's own file summaries, and the
owning symbols demangled into stable names."""

import collections
import re
import subprocess
import sys

CODE_REGION = 0


def refuse(msg):
    print(f"atlas: REFUSED — {msg}", file=sys.stderr)
    sys.exit(2)


def merge_regions(data, scope):
    """(file, l1, c1, l2, c2) -> (summed count, owning symbols).

    Regions arrive per instantiation; llvm's file summary counts source
    locations. Summing across instantiations is what reconciles the two.

    llvm groups the records of one function by where it starts and counts
    the regions of the largest one. Usually every record has the same
    layout, but a function with a `#[cfg(test)]` block inside is compiled
    twice — into the unit-test binary with the block, into everything else
    without it — and rustc draws the regions around the body differently
    in each. The union of both layouts has regions no single build has, so
    only the largest layout's regions are kept; counts still sum over every
    record that has them.
    """
    groups = collections.defaultdict(list)
    for fn in data["functions"]:
        names = fn["filenames"]
        regions = collections.defaultdict(int)
        for r in fn["regions"]:
            if r[7] != CODE_REGION:
                continue
            src = names[r[5]] if r[5] < len(names) else names[0]
            if src in scope:
                regions[(src, r[0], r[1], r[2], r[3])] += r[4]
        if regions:
            first = fn["regions"][0]
            groups[(names[0], first[0], first[1])].append((fn["name"], regions))
    counts = collections.defaultdict(int)
    owners = collections.defaultdict(set)
    for records in groups.values():
        layout = max((r for _, r in records), key=len).keys()
        for name, regions in records:
            for key in layout & regions.keys():
                counts[key] += regions[key]
                owners[key].add(name)
    return counts, owners


def reconcile(data, counts):
    """Verify the enumeration against llvm, and report the definitional gap.

    llvm's per-file `summary.regions.count` is computed by something other
    than this script, so agreement on it is a real witness: across the
    workspace all 599 files agree and the global total matches exactly
    (160,062). The same region set is being enumerated.

    The *covered* verdict differs, and the difference is a definition
    rather than a defect. A span inside a generic can be reached by one
    instantiation and not another; llvm's merged model keeps those apart
    and counts the unexercised copy as an uncovered region, while summing
    across instantiations calls the span covered. For "how thoroughly is
    every instantiation exercised", llvm's reading is the right one. For
    "which source can I delete, or must I write a test for" — the question
    this atlas exists to answer — summing is: the line ran, so it is not
    dead and cannot be removed. Measured 2026-08-27, the two readings
    differ on 822 of 160,062 regions, 2.8% of the dead set.

    Three other reconstructions were tried and each disagreed with llvm's
    summary *and* with the others — segments (493 of 599 files off), LCOV
    DA records (81,235 unique lines against a declared LF of 83,962, with
    no duplicate records to explain the gap), and per-name merging. The
    summary comes from a merged model the export formats do not fully
    expose. Demanding equality with it would make this gate unusable
    without making it more correct, so the enumeration is enforced and the
    verdict gap is reported.
    """
    mine = collections.Counter()
    mine_zero = collections.Counter()
    for (src, *_), n in counts.items():
        mine[src] += 1
        if n == 0:
            mine_zero[src] += 1
    llvm_dead = 0
    for f in data["files"]:
        s = f["summary"]["regions"]
        got, want = mine[f["filename"]], s["count"]
        if got != want:
            refuse(
                f"enumeration mismatch for {f['filename']}: "
                f"parsed {got} regions, llvm maps {want}"
            )
        llvm_dead += s["count"] - s["covered"]
    return llvm_dead


def demangle(names):
    """rustfilt is an external tool, like llvm-cov itself. Required, because
    a baseline whose identities depend on whether a tool was installed is
    not a baseline."""
    names = sorted(names)
    if not names:
        return {}
    try:
        out = subprocess.run(["rustfilt"], input="\n".join(names),
                             capture_output=True, text=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        refuse("rustfilt not available; install with `cargo install rustfilt`")
    got = out.splitlines()
    if len(got) != len(names):
        refuse(f"rustfilt returned {len(got)} lines for {len(names)} names")
    return dict(zip(names, got))


def _split_angle(s):
    """Split `<inner>rest` at the matching `>`. Returns (inner, rest).

    Nesting-aware, which a regex is not: `<Foo<Bar>>::m` has to close on the
    second `>`, not the first.

    >>> _split_angle("<Foo>::m")
    ('Foo', '::m')
    >>> _split_angle("<Foo<Bar, Baz>>::m")
    ('Foo<Bar, Baz>', '::m')
    """
    depth = 0
    for i, ch in enumerate(s):
        if ch == "<":
            depth += 1
        elif ch == ">":
            depth -= 1
            if depth == 0:
                return s[1:i], s[i + 1:]
    return s, ""


def _drop_generic_args(s):
    """Drop every `<...>` group, honouring nesting.

    >>> _drop_generic_args("alloc::vec::Vec<u8>::push")
    'alloc::vec::Vec::push'
    >>> _drop_generic_args("a::B<C<D>, E>::f")
    'a::B::f'
    """
    out, depth = [], 0
    for ch in s:
        if ch == "<":
            depth += 1
        elif ch == ">":
            depth -= 1
        elif depth == 0:
            out.append(ch)
    return "".join(out)


def symbol_of(demangled):
    """Strip the closure/instantiation tail and the hash, and canonicalise
    a qualified path to the type that owns the method.

    This used to be three regexes, the last of which was `<[^<>]*>` -> "".
    On a generic instantiation that does what it should. On a trait impl —
    `<Type as Trait>::method`, which is what every derived `Debug` demangles
    to — the `<...>` **is** the type, so the identity collapsed to the bare
    method name and one identity absorbed every crate's copy of it. `::fmt`
    held 241 dead regions across at least twelve crates, so a regression in
    one crate's `Debug` was cancelled by an improvement in another's and the
    ratchet never saw either. Worse, whether it collapsed was arbitrary: the
    pattern cannot match nested angle brackets, so a type that happened to
    carry a generic parameter survived and a plain one did not.

    >>> symbol_of("kevy_time::eval")
    'kevy_time::eval'
    >>> symbol_of("<kevy_replicate::state::ReplState as core::fmt::Debug>::fmt")
    'kevy_replicate::state::ReplState::fmt'
    >>> symbol_of("<kevy_elect::vote::Ballot as core::fmt::Debug>::fmt")
    'kevy_elect::vote::Ballot::fmt'
    >>> symbol_of("<kevy_uring::ring::IoUring>::submit_and_wait")
    'kevy_uring::ring::IoUring::submit_and_wait'
    >>> symbol_of("<kevy_map::map::KevyMap<alloc::vec::Vec, u64>>::probe_by_borrow_slow")
    'kevy_map::map::KevyMap::probe_by_borrow_slow'
    >>> symbol_of("kevy::dispatch::dispatch_with_proto::{closure#0}")
    'kevy::dispatch::dispatch_with_proto'
    >>> symbol_of("<kevy_seg::builder::Builder>::push::hab12cd34ef567890")
    'kevy_seg::builder::Builder::push'

    The two crates above are distinct identities now, which is the point.
    """
    s = re.sub(r"::\{closure#\d+\}", "", demangled)
    s = re.sub(r"::h[0-9a-f]{16}$", "", s)
    if s.startswith("<"):
        inner, rest = _split_angle(s)
        # `<Type as Trait>::m` -> `Type::m`; `<Type>::m` -> `Type::m`.
        owner = inner.split(" as ", 1)[0]
        s = owner + rest
    return _drop_generic_args(s)
