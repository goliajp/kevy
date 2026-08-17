# firehose-epoll reactor-gap bar: intermittent breaches, classified environmental

During the 5.3 full-tier rounds the firehose-epoll cell of tailgate
broke its 100 ms reactor-gap bar twice in eleven runs on lx64:

| run | verdict | detail |
|---|---|---|
| tier rounds 1–3, standalone #1 | PASS ×7 | gaps 34–69 ms typical |
| tier round 4 | FAIL | one of three probes: gap 193 ms (median of the three passed) |
| standalone #2 | FAIL | median gap 119 ms |
| container-A/B (paused ×2, running ×2) | PASS ×4 | pgcmp-container hypothesis refuted |
| released 5.1.0 binary, same box | PASS ×3 | control: pre-dates every recent change |

Classification: **environmental, intermittent (~1 in 5), not a
regression.** No engine change has touched the epoll reactor path since
5.2.0 (whose release tailgate was green on this box), the Postgres
container A/B came back clean both ways, and the released 5.1.0 binary
runs the same gate on the same disk. The magnitude and shape (百-ms
stalls during fsync-heavy firehose on ext4) match the jbd2
commit-window collisions the P8 arc documented; the P8 fix moved the
typical gap from ~790 ms to ~45 ms, and this is the surviving tail of
that phenomenon poking above a 100 ms bar roughly once in five runs.

tailgate stays hard: the bar is the shipped claim, and a red that
happens is information. A P8 follow-up (journal-collision avoidance, or
an fdatasync/journal-mode experiment) is the engine-side path to
margin; noted in the ROADMAP as a candidate arc, owner's priority call.
