//! Lua → RESP value marshaling. Composes the pure `resp` encoders
//! with luna-specific knowledge of `Value` and `Table` shapes.

use crate::resp;
use luna_core::runtime::Table;
use luna_core::runtime::heap::Gc;
use luna_core::runtime::value::Value;
use luna_core::vm::exec::Vm;

/// Marshal a luna `Value` into a RESP reply per the Redis EVAL rules:
///
/// | Lua                       | RESP                            |
/// |---------------------------|---------------------------------|
/// | nil                       | `$-1\r\n` (nil bulk)            |
/// | boolean true              | `:1\r\n`                        |
/// | boolean false             | `$-1\r\n` (nil bulk)            |
/// | integer                   | `:N\r\n`                        |
/// | integral float            | `:N\r\n` (5.1 returns 1 as 1.0) |
/// | non-integral float        | bulk string `$N\r\n<digits>\r\n`|
/// | string                    | bulk string (binary-safe)       |
/// | `{ok = "msg"}` table      | `+msg\r\n` (simple string)      |
/// | `{err = "msg"}` table     | `-msg\r\n` (error, no `ERR `)   |
/// | array table {v1, v2, …}   | `*N\r\n` + N marshaled elems    |
///
/// Array table iteration follows the Redis first-nil rule: the array
/// length is the number of consecutive non-nil values starting at
/// index 1. `{1, nil, 3}` produces `*1\r\n:1\r\n`, not `*3\r\n…`.
/// Closure / native fn / coroutine / userdata / lightuserdata cannot
/// be returned from a script to the wire and become nil-bulk replies.
///
/// Recursive on nested tables. luna's `Vm::with_instr_budget` caps
/// the recursion depth — no separate guard needed here.
pub(crate) fn value(vm: &mut Vm, v: Value) -> Vec<u8> {
    match v {
        Value::Nil => resp::nil_bulk(),
        Value::Bool(true) => resp::integer(1),
        Value::Bool(false) => resp::nil_bulk(),
        Value::Int(n) => resp::integer(n),
        Value::Float(f) => resp::float(f),
        Value::Str(s) => resp::bulk(s.as_bytes()),
        Value::Table(t) => table(vm, t),
        Value::Closure(_)
        | Value::Native(_)
        | Value::Coro(_)
        | Value::Userdata(_)
        | Value::LightUserdata(_) => resp::nil_bulk(),
    }
}

/// Implement the table-→RESP rules. Order matters per Redis
/// semantics: check `err` first (PUC scripting.c also short-circuits
/// on err), then `ok`, then array.
fn table(vm: &mut Vm, t: Gc<Table>) -> Vec<u8> {
    // 1. {err = "..."} — RESP error.
    let err_key = Value::Str(vm.heap.intern(b"err"));
    if let Value::Str(s) = t.get(err_key) {
        return resp::err(s.as_bytes());
    }
    // 2. {ok = "..."} — RESP simple string.
    let ok_key = Value::Str(vm.heap.intern(b"ok"));
    if let Value::Str(s) = t.get(ok_key) {
        return resp::simple_string(s.as_bytes());
    }
    // 3. Otherwise array — iterate from index 1, stop at first nil.
    let mut items: Vec<Vec<u8>> = Vec::new();
    let mut i: i64 = 1;
    loop {
        let v = t.get_int(i);
        if matches!(v, Value::Nil) {
            break;
        }
        items.push(value(vm, v));
        i += 1;
    }
    let mut out = resp::array_header(items.len() as i64);
    for item in items {
        out.extend_from_slice(&item);
    }
    out
}
