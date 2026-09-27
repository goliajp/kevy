//! `TABLE.DESCRIBE` / `IDX.DESCRIBE` / `VIEW.DESCRIBE`. The reply tree
//! comes from `kevy_index` — the same builder the server encodes — so
//! only the lookup and the encoding live here, and the error wording
//! mirrors `crates/kevy/src/cmd_describe.rs`.

use kevy_index::{Described, describe_index, describe_table, describe_view};

use super::util::{arr, bulk, err};
use crate::store::Store;

/// One DESCRIBE request; `false` = verb not in this group.
pub(super) fn dispatch(s: &Store, up: &[u8], argv: &[Vec<u8>], out: &mut Vec<u8>) -> bool {
    let (noun, lister) = match up {
        b"TABLE.DESCRIBE" => ("table", "TABLE.LIST"),
        b"IDX.DESCRIBE" => ("index", "IDX.LIST"),
        b"VIEW.DESCRIBE" => ("view", "VIEW.LIST"),
        _ => return false,
    };
    if argv.len() != 2 {
        let verb = String::from_utf8_lossy(up);
        err(out, &format!("ERR usage: {verb} name"));
        return true;
    }
    let name = &argv[1];
    let tables = s.table_list();
    let described = match up {
        b"TABLE.DESCRIBE" => tables.iter().find(|t| t.name == *name).map(describe_table),
        b"IDX.DESCRIBE" => s.idx_spec(name).map(|spec| describe_index(&spec, &tables)),
        _ => s.view_spec(name).map(|spec| describe_view(&spec)),
    };
    match described {
        Some(d) => encode(out, &d),
        None => {
            let shown = String::from_utf8_lossy(name);
            err(out, &format!("ERR no such {noun} '{shown}' ({lister} enumerates them)"));
        }
    }
    true
}

fn encode(out: &mut Vec<u8>, d: &Described) {
    match d {
        Described::Bulk(b) => bulk(out, b),
        Described::Array(items) => {
            arr(out, items.len());
            for item in items {
                encode(out, item);
            }
        }
    }
}
