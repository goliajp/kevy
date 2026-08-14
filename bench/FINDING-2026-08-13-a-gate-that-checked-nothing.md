# Four gates whose success message meant they had checked nothing

Rebuilding the site produced a class of defect worth naming on its own,
because it appeared four times in one day and each instance looked
exactly like a passing check.

    ok: 0 commands in the site's examples, all of them run
    ok: 0 Chinese/Japanese files, no half-width punctuation in prose
    ok: 423 text fragments across 32 pages, all present on the built site

The first two say zero out loud. The third does not: 423 is a plausible
number, and it was produced by a walk that never descended into the list
holding every command, card, recipe step and tab on the site. The real
figure is 962.

## The four

**check_site_commands** looks for `<pre><code>`; the new block components
emitted a bare `<pre>`. It reported zero commands found and called that
success. There are 219.

**check_site_content_parity** recursed only into keys it recognised as
prose, so `items` — a container, not prose — terminated the walk. Every
per-item field in the site's content was invisible to it. It had been
reporting "all present" about a set that excluded the commands.

**check_cjk_punct** had three target directories, two of them under
`site/`. Deleting `site/` left it scanning one, then none, and it went on
printing its ok line.

**The floor added to fix the third one was itself wrong.** It counted
total files and passed on a working tree at 224 while failing a fresh
clone at 84 — `tools/` holds caches and generated JSON that differ
between the two. A floor that measures the environment instead of the
subject is a new false signal in the place a false signal was just
removed. It counts translated chapters now: 34 in each of two languages,
a number that only grows.

## What they have in common

Each gate answered a question shaped like "is anything wrong with what I
found", and each found nothing. "Nothing is wrong with nothing" is true,
and it is not the question any of them exists to answer.

This is the same shape as the empty-predicate rule smix sent us — *would
an empty data directory give the same answer?* — but arriving from the
other direction. That rule is about a predicate that an empty input
satisfies. This is about a **selector** that returns an empty input: the
predicate is fine and never runs.

It is also the same shape as the port that "looked right", the mirror
verified inside the tree it was extracted from, and the nuspec that was
read instead of installed. All of them are checks that passed for a
reason other than the one intended.

## The rule

**Every gate states a floor, and the floor counts the subject.**

- the floor is on what the gate is *for* — translated chapters, RESP
  examples, pages — not on a total that includes whatever else happens to
  be in the tree;
- finding fewer than the floor fails with "the selector is wrong", not
  with silence;
- and the floor is red-green verified, by breaking the selector and
  watching the gate go red. Every one of the four here was.

A gate that cannot fail on an empty input is not a gate. It is a line of
output that reads like one.

## Where each fix lives

| gate | floor | verified by |
|---|---|---|
| `tools/check_site_commands.py` | 50 commands | mangling the `<pre><code>` selector |
| `tools/check_site_content_parity.py` | non-empty comparison | injecting a command that is on no page |
| `tools/check_cjk_punct.py` | 50 translated chapters | pointing the targets at directories that do not exist |
| `web/check.mjs` | 100 pages | (had one from the start, for this reason) |
| `scripts/mirror-go-module.sh` | non-zero passing tests | (added when written; a fully skipped suite is also green) |
