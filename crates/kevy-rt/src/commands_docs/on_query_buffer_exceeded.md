A connection was closed because its accumulated unparsed input
crossed the query-buffer cap.

The enforcement path printed a line and marked the conn closing,
and there was nothing a test or an operator could ask about it —
so an intermittent "the server did not close" could not be told
from "the server decided and the close had not landed yet".
Those are different defects. Redis exposes the same count as
`client_query_buffer_limit_disconnections`.

Called on the closing decision, not on the close completing.
That is the whole point of the distinction: a decision that has
not reached the client yet is a different thing from a cap that
was never noticed, and only a counter taken here can tell them
apart.

The default does nothing, so an existing implementor gains the
hook without changing:

```
use kevy_rt::{ArgvView, Commands, Route, Store, TxnKind};

#[derive(Clone)]
struct Minimal;
impl Commands for Minimal {
    fn route<A: ArgvView + ?Sized>(&self, _a: &A) -> Route { Route::Local }
    fn dispatch<A: ArgvView + ?Sized>(&self, _s: &mut Store, _a: &A) -> Vec<u8> {
        b"+OK\r\n".to_vec()
    }
    fn is_quit<A: ArgvView + ?Sized>(&self, _a: &A) -> bool { false }
    fn is_write<A: ArgvView + ?Sized>(&self, _a: &A) -> bool { false }
    fn txn_kind<A: ArgvView + ?Sized>(&self, _a: &A) -> TxnKind { TxnKind::Other }
}

// The hook is optional; the default is a no-op.
Minimal.on_query_buffer_exceeded();
```

An implementor that wants the number overrides it and counts;
kevy's own does exactly that, and `INFO stats` reports the total
as `client_query_buffer_limit_disconnections`.
