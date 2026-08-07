-- Migration day 2: the operator's edited schema, produced by doing
-- exactly what `sql plan`'s refusal taught — billing.amount (money)
-- becomes integer minor units, billing.src_ip (inet) becomes its text
-- form app-side. The other three tables are unchanged from the dump.
-- migrationgate compiles and applies THIS file; the original dump is
-- what `plan` reports on.

CREATE TABLE users (
    id         bigint PRIMARY KEY,
    email      text,
    name       text,
    created_at timestamp,
    flags      bigint
);
CREATE UNIQUE INDEX ON users (email);

CREATE TABLE threads (
    tid        bigint PRIMARY KEY,
    owner_id   bigint,
    subject    text,
    updated_at timestamp,
    msg_count  bigint
);
CREATE INDEX ON threads (owner_id);
CREATE INDEX ON threads (updated_at);

CREATE TABLE messages (
    mid        bigint PRIMARY KEY,
    tid        bigint,
    author_id  bigint,
    sent_at    timestamp,
    body       text,
    spam_score bigint
);
CREATE INDEX ON messages (tid);
CREATE INDEX ON messages (sent_at);

CREATE TABLE billing (
    id           bigint PRIMARY KEY,
    user_id      bigint,
    amount_cents bigint,
    src_ip       text
);
