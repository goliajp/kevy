# A date literal admits a year the arithmetic behind it cannot hold

Found while auditing which stone entry points no test enters directly.
`kevy_scalar::render_date` was on that list; it is three lines over
`kevy_time::civil_from_epoch`, which *is* tested — so the only thing
`render_date` adds is `days * 86_400`. That multiplication is where this
starts.

## Measured

```
parse_date("2020-01-01")            -> Some(18262)
parse_date("99999999-01-01")        -> Some(36523530107)
parse_date("999999999999-01-01")    -> panic, kevy-time/src/lib.rs:109
render_date(1_000_000_000_000_000)  -> panic, days * 86_400
render_date(i64::MAX / 86_400)      -> "292277026596-12-04"
```

`epoch_from_civil` is `days_from_civil(y, m, d) * SECS_PER_DAY`. For a year
of twelve digits the day count alone exceeds what multiplying by 86,400 can
hold.

The accepted case is worse than the panicking one, because nothing says it
went wrong. `parse_date("99999999-01-01")` returns 36,523,530,107 days, and
four separate places multiply a `Scalar::Date` by `MICROS_PER_DAY`
(86,400,000,000) — `logic.rs:66` and `:67` when comparing a date against a
timestamp, `datetime.rs:121`, `datetime_fmt.rs:157` and `:241`. That product
is 3.15e21 against an `i64` ceiling of 9.22e18.

`[profile.release]` sets no `overflow-checks`, so **the shipped build does
not panic here — it wraps**. A comparison between a date and a timestamp
answers from a wrapped number. Debug and test builds panic instead, which
is how this surfaced at all.

## Reachability, stated plainly

Narrow. `Scalar::Date` is constructed in exactly two ways: `parse_date` on
a text literal, and `current_date`. Casting reaches `cast_datetime` only for
`Scalar::Text`, so an integer cannot become a date. `parse_date` is called
from `kevy-sql`, and `kevy-sql` is depended on by `kevy-cli` alone — the
server's command surface does not reference `kevy_sql::` at all.

So the path is an operator typing an absurd year into the CLI's SQL probe.
Not a network-reachable defect. It is still a stone handing back a wrong
answer for an input it accepted, which is the property `architecture.toml`
claims for a stone.

## The shape of the fix

A parser should not admit a value the operations on that type cannot
represent. The bound that makes every downstream multiplication safe is
`|days| <= i64::MAX / MICROS_PER_DAY`, about 106,751,991 days — year
±292,277. PostgreSQL stops at 5874897 AD, far inside that. Rejecting beyond
the bound turns a wrapped answer into `None`, which every caller already
handles: `parse_date` returns `Option` and the SQL layer already says
`bad date literal '{s}'`.
