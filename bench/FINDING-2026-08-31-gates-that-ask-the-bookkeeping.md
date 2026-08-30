# Two gates asked the bookkeeping, not the authority

2026-08-31, during the v6.2.0 release.

Both were found by the release completing correctly and a gate disagreeing.

## 1. The Java door was reported shut while repo1 was serving it

`maven-central.yml` ended red. The publish had worked: repo1 answered 200
for `kevy-6.2.0.pom` and `kevy-6.2.0.jar` at the time the workflow was
marked failed.

The script polls the Portal's `deploymentState` for twenty minutes and
exits 1 if it has not flipped to `PUBLISHED`. That day the org was over
three monthly publishing limits (enforcement starts October 1), the
Portal stayed on `PUBLISHING`, and the script gave up — *before* running
the block directly beneath it, which already carried the right idea:

> The Portal saying PUBLISHED is not the same as a stranger being able to
> depend on it. Ask the thing their build will ask.

The knowledge was present and unreachable. A hard gate on the weaker
question stood in front of the stronger one.

**Fix**: the timeout is no longer a verdict — it prints a note and falls
through to the repo1 check, which decides. `FAILED` still exits at once,
because that is an answer. A release that is out must not be reported as
one that failed; of the two available lies that is the more expensive.

## 2. A gate that would have said the same thing on every release

`check_wasm_published.py` compares the wasm the site will deploy against
the one npm serves. It exists because on 2026-08-14 the site demonstrated
`IDX.CREATE` against a package built without it, both labelled 5.1.0.

At 6.2.0 it reported: same marker verbs, 1458 KB vs 1457 KB, "a rebuild,
or a change below the marker level — deploy anyway only if that is a
decision somebody made".

Nobody had to make a decision. The tree was byte-identical to `v6.2.0`,
the tag `release.yml` built the package from. Same source; CI compiles on
Linux, the deploy happens from macOS, and wasm codegen is not identical
across them. **The site is deployed from this machine every time, so this
gate would have asked for the same non-decision at every release** — and
an alarm that fires on every happy path is one people learn to click
through. That is how it would eventually wave through the real case.

**Fix**: when the markers match, ask `git diff --stat vX.Y.Z`. Identical
tree → say so and exit 0. Different tree → that is the real "change below
the marker level", named with a file count. No tag → say the question
could not be asked. The case the tool exists for — a verb the site has
and the package does not — still fails **even when the source is
identical**, which is the branch that had to be proven rather than
assumed.

## The shape

A gate is only as good as the question it asks. Both of these asked
something adjacent and cheaper to observe — a vendor's status field, a
byte comparison — and treated the answer as the thing itself. The
authority in each case was one call away and, in the Maven script,
already written down four lines below.

Related: [FINDING-2026-08-30-gates-that-judge-their-own-output.md]
(a gate that regenerated the artifact it was about to check) and the
release skill's standing rule — verify channels **by content, never by
status code**.
