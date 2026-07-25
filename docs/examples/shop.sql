-- The shop schema, ported from PG (docs/cookbook.md §22).
-- Compile: kevy-cli sql compile docs/examples/shop.sql

CREATE TABLE users (
  id     bigserial PRIMARY KEY,
  email  text,
  name   text,
  plan   text
);
CREATE UNIQUE INDEX ON users (email);

CREATE TABLE orders (
  id          bigserial PRIMARY KEY,
  user_id     bigint,
  status      text,
  total       numeric(10,2),
  created_at  bigint       -- epoch seconds, app-encoded
);
-- INCLUDE = PG covering columns -> kevy stored VALUES (residual FILTER/SORT).
CREATE INDEX ON orders (status) INCLUDE (total, created_at);
-- Multi-column -> a composite ORDERPATH (the (user_id, created_at DESC) walk).
CREATE INDEX ON orders (user_id, created_at DESC);

CREATE TABLE order_items (
  id        bigserial PRIMARY KEY,
  order_id  bigint,
  sku       text,
  qty       int
);
CREATE INDEX ON order_items (order_id);

-- Constant predicates -> a named engine view.
CREATE VIEW paid_orders AS
  SELECT * FROM orders WHERE status = 'paid';

-- Parameterized -> a query card (an IDX.QUERY template with $N slots).
CREATE VIEW recent_orders_by_user AS
  SELECT id, status, total, created_at FROM orders
  WHERE user_id = $1
  ORDER BY created_at DESC
  LIMIT 20;
