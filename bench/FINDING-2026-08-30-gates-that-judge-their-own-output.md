# Five ways a gate can be green about nothing

Written 2026-08-30, during the v6.1.0 release. Sibling to
`FINDING-2026-08-30-every-gate-read-the-tree-none-asked-the-world.md`,
which found that no gate in this repository had ever asked a registry a
question. This one is about gates that *do* ask, and still learn nothing.

Each item below was found in a working, passing, thoughtfully-commented
check. None of them were broken. That is the point: every one of them
answered a question adjacent to the one it appeared to answer.

---

## 1. A refusal that spoke for doors it had never looked at

`release.yml` publishes every binding package it can find. The size
ratchet added earlier this week refused `expo-kevy@6.1.0` because a Linux
runner packs 37,816 bytes where npm serves 110,723,661 — the prebuilt
iOS and Android engine is a gitignored build output that exists only on a
machine with those toolchains. The refusal was exactly right, and it was
its first real release.

Then `exit 1` under `set -e` ended the loop. Four packages sat after it in
the list; three of them are pure JavaScript that the runner could have
published without any toolchain at all. A door nobody could open hid four
that anyone could, and the job's red said only "npm publish failed".

The refusal now records the name and continues. What makes a release red
is the gate that asks the registries — which now runs when the release
workflow ends, rather than at 06:17 the next morning.

**Shape:** a per-item verdict wired to abort the whole pass.

---

## 2. A probe that asked an endpoint the user's tool does not use

The channel-parity gate asked `proxy.golang.org` for `@v/v6.1.0.info`.
The tag had been pushed seconds earlier; the proxy went to GitHub before
it had propagated, cached the miss, and served 404 for half an hour.

Meanwhile `@latest` returned v6.1.0, `.mod` and `.zip` returned 200, and

    GOPROXY=https://proxy.golang.org go get github.com/goliajp/kevy-go/v6@v6.1.0

installed it with no fallback to direct. `go` needs `.mod` and `.zip` to
resolve an exact version. `.info` is for `@latest` and metadata.

So the gate reported a door shut while users were walking through it. A
false red costs what a false green costs — both teach people that the gate
is not worth reading — and this one would have fired on every release,
because warming a cache before the source has propagated caches the
absence.

It asks `.mod` now, and by content: the answer must be this module's
go.mod, not merely a 200.

**Shape:** the probe's question was *near* the user's question.

---

## 3. A field that was optional at five call sites and filled in at one

`documentHtml` has taken an `alternates` list since the site was rebuilt.
Five functions call it. One filled the list in.

So 657 of 768 pages went out with no hreflang: every command page, both
localised home pages, every written page, and the English front door —
while the language switcher in the header rendered three links on all of
them. Search engines were told the site was trilingual on 111 pages and
monolingual on the rest.

`verify.mjs` has a check named "the document offers its translations". It
passes. It samples a reference page, which is one of the 111.

The list is no longer passed in. A page already declares `canonical`, and
the URL scheme is mechanical — `/docs/x/`, `/zh/docs/x/`, `/ja/docs/x/`.
The prefix comes off and each locale's goes back on, in the one place that
writes the document. A caller can still say which locales it is *missing*
from; it cannot forget to say where they are.

**Shape:** an optional field, and a check that sampled the one page type
where somebody had remembered it.

---

## 4. A canonical pointing at a page that answers 200

The release notes are rendered by `renderDocPage`, which builds
`canonical` from the slug as `/docs/<slug>/`, and written to `/changelog/`.
So for an unknown number of releases the page told search engines its real
address was `/docs/changelog/` — which this host answers with the SPA
shell and HTTP 200.

`check.mjs` resolves every internal link against `dist/`. Its second line
is `if (/^(https?:|mailto:|data:|#)/.test(href)) continue`, and canonical
and alternate hrefs are written absolute. They had never been asked to
resolve.

Found by deriving an hreflang from the canonical and asking whether the
target existed. `check.mjs` now resolves all 3,834 of them, requires each
page's canonical to appear among its own alternates, and carries a floor
for the day a build stops emitting them. Verified by putting the defect
back and watching it fail.

**Shape:** the skip-list that made the check tractable also made it blind,
on a host where the failure returns 200.

---

## 5. A gate that judged the file it had just written

CI runs `stone_report.py`, which overwrites `bench/STONE-REPORT.json`,
and then `check_stones.py`, which reads it. So CI always judges the report
it just took and never the one the repository ships — the copy a person
opens, and the one a local `stonegate` run refuses on.

That tracked copy has now described the wrong release twice: 5.4.1 in a
6.0.0 tree, and 6.0.0 in a 6.1.0 tree. The first time, the fix wrote the
diagnosis down in its own commit message and then corrected only the data.
Two days later it happened again.

CI now reads the committed copy with `git show HEAD:` — which is the whole
trick, because the working file has already been overwritten by then. That
also lets the check sit *after* the artifact upload, which matters: a
version bump is exactly when it fails, and the artifact it tells you to
fetch has to exist by then.

Version only. Coverage numbers move for honest reasons, and a gate that
demanded they match byte for byte would be red on noise.

**Shape:** the gate's input was its own output.

---

## What they have in common

Four of the five are the same mistake as the release doors: **a list that
is written down instead of derived**. The fifth is its close relative — a
reading taken of the wrong copy.

The repair in each case was not a bigger check. It was moving the question
one step closer to the thing a user actually experiences:

| Was asked | Is asked |
|---|---|
| did the publish step succeed | does the registry serve it |
| is `.info` there | can `go get` resolve it |
| did the caller pass alternates | does the canonical have siblings |
| do relative links resolve | do the page's own URLs exist |
| does the report I just wrote pass | does the report we ship describe this release |

A gate that samples proves the sample. A gate fed its own output proves
nothing at all.
