# What kevy will not do

Every product's shape is defined as much by its refusals as by its
features. This page collects kevy's, so the answer to "can it do X?"
arrives before you build on a guess. Each entry says *why*, and where
that need is met instead.

A refusal here is durable. These are not "not yet" — they are lines the
design is built against, and changing one is a redesign, not a feature
request.

## The four lines

**1. The Redis contract is not ours to break.** Anything kevy answers
under a Redis command name means what Redis means by it. New capability
arrives under its own namespace (`IDX.*`, `FEED.*`, `VIEW.*`, `FT.*`,
`VEC.*`) rather than by overloading an existing verb. So: no
"improved" SET semantics, no extra fields on an existing reply shape.

**2. Meaning and planning stay in your application.** kevy stores and
serves; it does not decide what your data means or how a query should
be executed. This is the line that refuses SQL-the-query-language,
joins, and a cost-based planner: the moment the engine chooses a plan,
your latency acquires a decision you did not make and cannot see. What
it *does* offer is declared access paths — you name the index, and the
cost is yours to predict.

**3. Topology is declared, not discovered.** The member table comes
from config. Roles are dynamic; membership is not.

**4. The network is trusted.** kevy has no authentication and no
transport encryption, by permanent decision — put it behind a proxy
that has both (Caddy, nginx, a service mesh, a private subnet).

## The refusals, by area

| Area | Refused | Why, and what to use instead |
|---|---|---|
| Query | SQL as a query language, joins, cost-based planner, ad-hoc predicates in views | line 2 — declare an index or a view; `IDX.QUERY` / `VIEW.QUERY` name the access path explicitly |
| Query | Write-path callbacks / triggers | your writer already knows what it wrote; a callback hides latency inside the write |
| Security | AUTH, TLS, ACLs, multi-user | line 4 — terminate them in a proxy |
| Cluster | Sharded multi-master, cross-DC active-active, CRDTs | single-DC, single-partition-tolerant by design; conflict-free merge is an application-level model |
| Cluster | Raft / strongly-consistent log replication | the quorum lease plus epoch fencing is the consistency this design offers; a full consensus log is a different product |
| Cluster | Online resharding, gossip discovery, dynamic membership | line 3 — change the config and restart the member |
| Storage | Cross-store transactions, tenancy semantics | outside the engine's boundary; compose them above it |
| Platform | `no_std` / MCU targets for the server | the five stone crates are `no_std`-capable already; the server needs an OS |

## Where the function surface stands

kevy's SQL-shaped surface is a **fold**, not a query engine: it
computes scalar expressions so an application migrating off a
relational database does not have to reimplement them, and it refuses —
by name, never silently — everything that would require a planner.

Two lines are gated on every change:

- **Capability**: 82.5% of the function subset this arc serves folds
  correctly, and that ratio may only rise.
- **Honesty**: across all 89 probe files, **zero wrong answers**. A
  refusal names itself; a wrong answer would not.

The second line is the one that matters when you are deciding whether
to depend on this. Refusals are visible at the call site, so you find
them while you are writing the code, not in production.
