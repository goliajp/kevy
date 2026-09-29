Build the **single-key** command the dispatcher will replay to
satisfy one watched `key` of a (possibly multi-key) blocking
command. `args` is the original command; `key` is one of its
watched keys. Returns an [`Argv`] that, when dispatched, pops /
reads only `key` — e.g. `BLPOP k1 k2 0` watching `k2` yields
`BLPOP k2 0`; `XREAD … STREAMS s1 s2 id1 id2` watching `s2`
yields `XREAD … STREAMS s2 id2`.

Any state-dependent positional arg (`$`) is left **literal** here —
it's frozen later by [`Self::resolve_block_argv`] on the key's
owning shard. No store access needed (pure argv slicing). Default:
the unchanged argv (single-key blocking commands need no rewrite).

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

let blpop = argv(&["BLPOP", "k1", "k2", "0"]);
// The default replays the original command; an override for a multi-key
// command would narrow it to `BLPOP k2 0`.
assert_eq!(Minimal.block_serve_argv(&blpop, BlockKind::Blpop, b"k2"), blpop);
```
