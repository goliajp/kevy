Rewrite `args` into the owned [`Argv`] that the dispatcher will
store as the parked waiter's command and replay on wake. Lets a
command set normalise positional ID / cursor arguments that would
otherwise re-resolve to a different value on retry — most notably
`XREAD BLOCK ... STREAMS k $`, where leaving `$` literal in the
retried argv causes a fresh re-resolve to the post-`XADD` last_id
and zero matching entries (the wake hangs).

Default: just materialise the argv unchanged. Concrete impls only
need to override when a registered command carries an arg whose
meaning depends on store state at park time (`XREAD $`, the
classic case).

For the cross-shard arbiter this runs on the **target** shard (the
one that owns the key) when the waiter is armed, so `$` snapshots
the target's real `last_id` — not the origin shard's (which may not
hold the stream at all).

```
# use kevy_rt::{ArgvView, Commands, Route, Store, TxnKind};
# #[derive(Clone)]
# struct Minimal;
# impl Commands for Minimal {
#     fn route<A: ArgvView + ?Sized>(&self, _a: &A) -> Route { Route::Local }
#     fn dispatch<A: ArgvView + ?Sized>(&self, _s: &mut Store, _a: &A) -> Vec<u8> {
#         b"+OK\r\n".to_vec()
#     }
#     fn is_quit<A: ArgvView + ?Sized>(&self, _a: &A) -> bool { false }
#     fn is_write<A: ArgvView + ?Sized>(&self, _a: &A) -> bool { false }
#     fn txn_kind<A: ArgvView + ?Sized>(&self, _a: &A) -> TxnKind { TxnKind::Other }
# }
# fn argv(parts: &[&str]) -> kevy_rt::Argv {
#     kevy_rt::Argv::from(parts.iter().map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>())
# }
use kevy_rt::BlockKind;

let mut store = Store::new();
let xread = argv(&["XREAD", "BLOCK", "0", "STREAMS", "s", "$"]);
// The default stores the command unchanged — `$` stays literal.
let parked = Minimal.resolve_block_argv(&mut store, &xread, BlockKind::XReadBlock);
assert_eq!(parked, xread);
```
