# funcgate probe corpus

89 sqllogictest files, copied verbatim from spg's pg_regress-derived
corpus (`spg/xtests/sqllogictest/corpus/pg_regress/`, 2026-08-08) — the
same 89-probe set R4a's inventory counted. Vendored here because the
gate runs where spg's checkout does not (CI, the bench box).

Format: `query <type>` / SQL / `----` / expected rows; `statement ok` /
`statement error` for non-query steps. `(empty)` means the empty
string. The funcgate runner (`bench/funcgate.sh`) classifies every
probe as SERVED / REFUSED(named) / UNSUPPORTED — the last meaning a
silent failure, which fails the gate by itself.

Do not edit these files to make the gate pass: they are the ground
truth the scalar library is tested against, not an aspiration list.
