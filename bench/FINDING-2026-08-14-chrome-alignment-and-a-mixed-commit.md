# The Golia Lab chrome, and a commit that did not say so

`99b4e94b` is titled "the translated bindings pages linked to a file
their language lacks" and carries thirteen files that have nothing to do
with that: the whole masthead and footer alignment with
tiktoken.golia.jp, the shared Brand marks, the GOLIA wordmark, and the
terminal's move from a dark ground to paper.

They went in together because the fix was staged with `git add -A` while
that work was uncommitted in the tree. The message describes one change
and the diff contains two, which is the shape of commit that is
impossible to revert and hard to read later.

Recorded here rather than rewritten: the commit is pushed, and a
force-push to fix a message is worse than a note that explains it.

## What that commit actually changed, beyond the doc links

**Masthead.** tiktoken carries a logo mark beside a monospace wordmark
and nothing else. kevy carried a text wordmark and a version number; the
version moved to `<meta name="generator">`, so every one of the 719 pages
still states it — and `check.mjs` still holds every one to the manifest —
without the chrome advertising it. Nav hover is the accent colour, and
the language control's selected state is accent rather than ink.

**Footer.** A 2px ink rule closes the page the way the figures strip
opens it. The organisation mark is the GOLIA wordmark image — the same
file, from rust-tiktoken — rather than the word set in type, and the
registry links carry their own brand marks. `Brand.tsx` is byte-identical
to rust-tiktoken's, copied rather than shared: a package for three SVG
paths is a dependency the zero-dependency rule would have to make an
exception for.

**The terminal is paper now.** It had a dark ground on the theory that a
shell is a quotation from another surface. Beside a paper page it read as
a hole punched in one. It is the same paper, rules and type as the
playground on tiktoken, with the accent colour marking the prompt and the
live indicator; five dark-palette variables went with it.

## The check that now covers this

`verify.mjs` compares the header across five pages by **computed style**,
not markup — element count, padding, font family, footer count. Every
difference in this class lived in the CSS rather than in the HTML, so a
markup diff showed nothing. 21 checks, live.
