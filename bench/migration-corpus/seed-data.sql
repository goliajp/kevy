-- The drill's data half — loaded into PG after seed-schema.sql.
-- Deterministic (generate_series only); ~52k rows. sql plan never
-- sees this file: the real chain feeds it pg_dump --schema-only.

INSERT INTO users
SELECT n,
       'user' || n || '@drill.example',
       'User ' || n,
       timestamp '2024-01-01 00:00:00' + (n % 365) * interval '1 day',
       n % 8
FROM generate_series(1, 2000) AS n;

INSERT INTO threads
SELECT n,
       (n % 2000) + 1,
       'Subject ' || n,
       timestamp '2024-06-01 00:00:00' + (n % 180) * interval '1 hour',
       (n % 25) + 1
FROM generate_series(1, 10000) AS n;

INSERT INTO messages
SELECT n,
       (n % 10000) + 1,
       (n % 2000) + 1,
       timestamp '2024-06-01 00:00:00' + n * interval '1 minute',
       'Message body ' || n || ' with some text to carry weight.',
       n % 100
FROM generate_series(1, 40000) AS n;

INSERT INTO billing
SELECT n, (n % 2000) + 1, (n * 17)::text::money, ('10.0.' || (n % 256) || '.1')::inet
FROM generate_series(1, 500) AS n;
