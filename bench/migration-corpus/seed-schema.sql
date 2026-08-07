-- The migration drill's seed database (V2 plan, M1). Deterministic:
-- every row comes from generate_series, so two runs of the drill see
-- byte-identical data — migrationgate depends on that.
--
-- The shape follows R4a's evidence: natural keys, bigint/text/
-- timestamp/flag columns, range + unique secondary indexes. Two
-- columns use types kevy REFUSES (money, inet) — the charter bar's
-- second half ("every type either moved or named") needs refusal
-- traffic, not just happy-path traffic.

CREATE TABLE users (
    id         bigint PRIMARY KEY,
    email      text NOT NULL,
    name       text NOT NULL,
    created_at timestamp NOT NULL,
    flags      bigint NOT NULL
);
CREATE UNIQUE INDEX ON users (email);

CREATE TABLE threads (
    tid        bigint PRIMARY KEY,
    owner_id   bigint NOT NULL,
    subject    text NOT NULL,
    updated_at timestamp NOT NULL,
    msg_count  bigint NOT NULL
);
CREATE INDEX ON threads (owner_id);
CREATE INDEX ON threads (updated_at);

CREATE TABLE messages (
    mid        bigint PRIMARY KEY,
    tid        bigint NOT NULL,
    author_id  bigint NOT NULL,
    sent_at    timestamp NOT NULL,
    body       text NOT NULL,
    spam_score bigint NOT NULL
);
CREATE INDEX ON messages (tid);
CREATE INDEX ON messages (sent_at);

-- The refusal traffic: kevy's sql face must name both of these, not
-- silently drop them (money -> integer minor units, inet -> app side;
-- both verdicts are R4a table rows).
CREATE TABLE billing (
    id      bigint PRIMARY KEY,
    user_id bigint NOT NULL,
    amount  money NOT NULL,
    src_ip  inet NOT NULL
);
