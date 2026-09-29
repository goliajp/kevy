use super::*;

/// A toy shard-state type. Carries a small in-memory keyspace plus a
/// dispatch implementation analogous to kevy's command path.
#[derive(Default)]
struct ToyStore {
    kv: std::collections::HashMap<Vec<u8>, Vec<u8>>,
    calls_seen: u32,
}

impl ToyStore {
    fn run(&mut self, argv: &[&[u8]], read_only: bool) -> Vec<u8> {
        self.calls_seen += 1;
        if argv.is_empty() {
            return b"-ERR no command\r\n".to_vec();
        }
        let cmd: Vec<u8> = argv[0].iter().map(|b| b.to_ascii_uppercase()).collect();
        // Toy write-flag table (matches kevy's `is_write_verb`
        // shape; production wiring delegates to kevy::cmd).
        let is_write = matches!(cmd.as_slice(), b"SET" | b"DEL");
        if read_only && is_write {
            return b"-READONLY can't write against a read-only script\r\n".to_vec();
        }
        match cmd.as_slice() {
            b"SET" => {
                self.kv.insert(argv[1].to_vec(), argv[2].to_vec());
                b"+OK\r\n".to_vec()
            }
            b"GET" => match self.kv.get(argv[1]) {
                Some(v) => {
                    let mut out = format!("${}\r\n", v.len()).into_bytes();
                    out.extend_from_slice(v);
                    out.extend_from_slice(b"\r\n");
                    out
                }
                None => b"$-1\r\n".to_vec(),
            },
            b"DEL" => {
                let n = self.kv.remove(argv[1]).is_some() as i64;
                format!(":{n}\r\n").into_bytes()
            }
            _ => b"-ERR unknown\r\n".to_vec(),
        }
    }
}

fn make_host() -> LuaHost<ToyStore> {
    LuaHost::<ToyStore>::new(|store, argv, ro| store.run(argv, ro))
}

#[test]
fn eval_calls_dispatch_with_live_store() {
    let mut host = make_host();
    let mut store = ToyStore::default();
    let reply = host.eval(
        &mut store,
        b"redis.call('SET', KEYS[1], ARGV[1])\n\
          return redis.call('GET', KEYS[1])\n",
        &[b"k"],
        &[b"hello"],
    );
    assert_eq!(reply, b"$5\r\nhello\r\n");
    assert_eq!(store.kv.get(b"k".as_slice()), Some(&b"hello".to_vec()));
    assert_eq!(store.calls_seen, 2);
}

#[test]
fn eval_ro_blocks_writes() {
    let mut host = make_host();
    let mut store = ToyStore::default();
    let reply = host.eval_ro(&mut store, b"return redis.call('SET', KEYS[1], 'v')", &[b"k"], &[]);
    assert!(reply.starts_with(b"-READONLY "));
    assert!(!store.kv.contains_key(b"k".as_slice()));
}

#[test]
fn evalsha_round_trip() {
    let mut host = make_host();
    let mut store = ToyStore::default();
    let sha = host.script_load(b"return redis.call('GET', KEYS[1])");
    store.kv.insert(b"x".to_vec(), b"42".to_vec());
    let reply = host.evalsha(&mut store, sha, &[b"x"], &[]);
    assert_eq!(reply, b"$2\r\n42\r\n");
}

#[test]
fn dispatch_outside_scope_is_a_clear_error() {
    // No active `host.eval()` → `with_current` returns None and
    // the dispatch returns the documented -ERR reply.
    let r = with_current::<ToyStore, _>(|_| 1);
    assert!(r.is_none());
}

#[test]
fn pointer_is_cleared_after_eval_returns() {
    let mut host = make_host();
    let mut store = ToyStore::default();
    let _ = host.eval(&mut store, b"return 1", &[], &[]);
    // After eval returns, CURRENT has been reset.
    let r = with_current::<ToyStore, _>(|_| 1);
    assert!(r.is_none());
}

#[test]
fn nested_eval_calls_restore_outer_context() {
    // Set CURRENT to a sentinel address, call host.eval (which
    // pushes its own), confirm the sentinel comes back after.
    let sentinel = Some((0xdead_beef, TypeId::of::<u8>()));
    CURRENT.with(|c| c.set(sentinel));
    let mut host = make_host();
    let mut store = ToyStore::default();
    let _ = host.eval(&mut store, b"return 1", &[], &[]);
    let restored = CURRENT.with(Cell::get);
    assert_eq!(restored, sentinel);
    CURRENT.with(|c| c.set(None));
}

#[test]
fn a_context_is_lent_only_as_the_type_it_was_installed_as() {
    let mut store = ToyStore::default();
    let _guard = set_current(&mut store);
    assert_eq!(with_current::<u64, _>(|_| ()), None, "a different type gets nothing");
    assert_eq!(with_current::<ToyStore, _>(|_| ()), Some(()));
}

#[test]
fn a_lent_context_is_not_lent_twice() {
    let mut store = ToyStore::default();
    let _guard = set_current(&mut store);
    let nested = with_current::<ToyStore, _>(|_| with_current::<ToyStore, _>(|_| ()));
    assert_eq!(nested, Some(None), "the inner borrow finds the slot empty");
    assert_eq!(with_current::<ToyStore, _>(|_| ()), Some(()), "and it is back afterwards");
}

mod p7e_tests {
    use super::*;

    /// P7e — set_instr_budget on a busy-ish loop. Default budget is
    /// 200 M (5 s on modern hardware); we shrink to 100 instructions
    /// and confirm a 10 000-iter loop trips the budget. Then we
    /// flush and run the same script under unlimited (0) budget to
    /// confirm the setter is live.
    #[test]
    fn instr_budget_trips_on_long_loop() {
        let mut host = LuaHost::<()>::new(|_ctx, _argv, _ro| Vec::new());
        host.set_instr_budget(100); // very tight cap
        let mut nothing = ();
        let reply = host.eval(
            &mut nothing,
            b"local s = 0\nfor i = 1, 10000 do s = s + i end\nreturn s",
            &[],
            &[],
        );
        // Budget exceeded → the interpreter surfaces an error → bridge wraps in
        // -ERR. Don't be picky about the exact wording — just confirm
        // it's an error, not an integer result.
        assert!(
            reply.starts_with(b"-ERR "),
            "expected -ERR budget reply, got: {:?}",
            String::from_utf8_lossy(&reply)
        );
    }

    #[test]
    fn unlimited_budget_runs_to_completion() {
        let mut host = LuaHost::<()>::new(|_ctx, _argv, _ro| Vec::new());
        host.set_instr_budget(0); // unlimited
        let mut nothing = ();
        let reply = host.eval(
            &mut nothing,
            b"local s = 0\nfor i = 1, 10000 do s = s + i end\nreturn s",
            &[],
            &[],
        );
        // 1+...+10000 = 50005000
        assert_eq!(reply, b":50005000\r\n");
    }
}
