Command-set semantics injected into the runtime. Cloned to every core, so it
must be cheap/stateless to clone.

Designed to be implemented outside this crate: the kevy server's
command set implements it, and so can an embedder's. Methods added in
later versions come with a default, so an implementation keeps
compiling.

# Implementation contract

- **Classification is a pure function of the argv.** `route`,
  `resolve`, `is_write`, `is_quit`, `txn_kind`, `block_hint`,
  `notify_class` and `queue_error` answer the same for the same
  arguments on every shard and every call: the runtime asks on the
  connection's shard and acts on another.
- **`resolve` agrees with the per-attribute methods.** An override
  must return what `route`, `txn_kind`, `is_quit`, `is_write` and
  `block_hint` would; the runtime uses either path.
- **`is_write` is the durability gate.** A command that mutates the
  store must answer `true`, or its effect is missing from the AOF and
  the replication stream.
- **The route names every key the command touches.** A multi-key
  command routed [`Route::Single`] runs wholly on that key's shard; a
  second key on another shard is silently read or written in the
  wrong keyspace.
- **Dispatch writes one complete RESP reply** per command (none only
  where a blocking command parks), and runs on the shard thread
  without blocking it.
- **Hooks run on the shard thread** at tick or command cadence and
  must be cheap when their feature is off; they may keep per-shard
  state in thread-locals, since the answering thread *is* the shard.

```
use kevy_rt::{Argv, ArgvView, Commands, Route, Store, TxnKind};

#[derive(Clone)]
struct Kv;
impl Commands for Kv {
    fn route<A: ArgvView + ?Sized>(&self, a: &A) -> Route {
        if a.len() > 1 { Route::Single(1) } else { Route::Local }
    }
    fn dispatch<A: ArgvView + ?Sized>(&self, s: &mut Store, a: &A) -> Vec<u8> {
        match a.first() {
            Some(b"RPUSH") => match s.rpush(&a[1], &[&a[2]]) {
                Ok(n) => format!(":{n}\r\n").into_bytes(),
                Err(_) => b"-WRONGTYPE\r\n".to_vec(),
            },
            _ => b"+PONG\r\n".to_vec(),
        }
    }
    fn is_quit<A: ArgvView + ?Sized>(&self, a: &A) -> bool {
        a.first() == Some(&b"QUIT"[..])
    }
    fn is_write<A: ArgvView + ?Sized>(&self, a: &A) -> bool {
        a.first() == Some(&b"RPUSH"[..])
    }
    fn txn_kind<A: ArgvView + ?Sized>(&self, a: &A) -> TxnKind {
        match a.first() {
            Some(b"MULTI") => TxnKind::Multi,
            Some(b"EXEC") => TxnKind::Exec,
            _ => TxnKind::Other,
        }
    }
}
fn argv(parts: &[&str]) -> Argv {
    Argv::from(parts.iter().map(|p| p.as_bytes().to_vec()).collect::<Vec<_>>())
}

// Classification happens once, on the connection's shard...
let rpush = argv(&["RPUSH", "jobs", "a"]);
let resolved = Kv.resolve(&rpush);
assert_eq!(resolved.route, Route::Single(1));
assert!(resolved.is_write);

// ...and dispatch runs on the shard that owns `jobs`.
let mut store = Store::new();
assert_eq!(Kv.dispatch(&mut store, &rpush), b":1\r\n");
assert_eq!(store.llen(b"jobs")?, 1);
# Ok::<(), Box<dyn std::error::Error>>(())
```
