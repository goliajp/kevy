The directory this runtime snapshots to and loads from — the one
`Runtime::builder().with_data_dir()` set.

A `Commands` implementation carries its own configuration, and
nothing told it about this. A server built programmatically
therefore answered `CONFIG GET dir` from that configuration
while writing somewhere else entirely: one face reporting what
the other face is not doing. `kevy::serve` builds both from one
`Config` and never saw the gap, which is why it went unnoticed
until a test used `CONFIG GET dir` to identify its own server
and was handed `.`.

Called once per shard, on the shard's thread, beside
[`Self::on_shard_start`]. Default: no-op, so an implementor that
has no configuration to correct is unaffected:

```
use kevy_rt::{ArgvView, Commands, Route, Store, TxnKind};
use std::path::Path;

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

Minimal.on_data_dir(Path::new("/var/lib/kevy"));
```

An implementor that answers `CONFIG GET dir` overrides it and
points that answer here; kevy's own does exactly that.
