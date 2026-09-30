//! The declared indexes, views and tables survive host-mediated
//! persistence: the log carries the catalog frame, the compacted image
//! carries it too, and a fresh store fed either answers the same queries.
//!
//! The `unsafe` calls follow the note at the top of `abi_tests.rs`: live
//! handles, and pointer/length pairs taken from locals in scope.

use crate::abi_aof::{kevy_aof_dump, kevy_aof_frame_in, kevy_aof_frames_out};
use crate::abi_cmd::kevy_cmd;
use crate::abi_core::{OPEN_CAPTURE_AOF, kevy_close, kevy_open, kevy_out_len, kevy_out_ptr};

fn out(h: u32) -> Vec<u8> {
    let ptr = kevy_out_ptr(h);
    // SAFETY: pointer and length are the pair the ABI just produced.
    unsafe { std::slice::from_raw_parts(ptr, kevy_out_len(h) as usize) }.to_vec()
}

fn cmd(h: u32, line: &str) -> String {
    let mut packed = Vec::new();
    for p in line.split(' ') {
        packed.extend_from_slice(&(p.len() as u32).to_le_bytes());
        packed.extend_from_slice(p.as_bytes());
    }
    // SAFETY: live handle, `packed` readable for the call.
    let s = unsafe { kevy_cmd(h, packed.as_ptr(), packed.len() as u32) };
    assert!(s >= 0, "{line}: status {s}");
    String::from_utf8(out(h)).unwrap()
}

fn feed(h: u32, log: &[u8]) {
    // SAFETY: live handle, `log` readable for the call.
    let n = unsafe { kevy_aof_frame_in(h, log.as_ptr(), log.len() as u32) };
    assert!(n > 0, "the log applied nothing");
}

fn drain(h: u32) -> Vec<u8> {
    kevy_aof_frames_out(h);
    out(h)
}

/// An index, a view over two indexes, a declared table, and rows under
/// each.
fn declare(h: u32) {
    for line in [
        "HSET user:1 name alice city tokyo age 34",
        "HSET user:2 name bob city osaka age 41",
        "HSET user:3 name carol city tokyo age 29",
        "IDX.CREATE by_city ON PREFIX user: FIELD city TYPE str KIND range",
        "IDX.CREATE by_age ON PREFIX user: FIELD age TYPE i64 KIND range",
        "VIEW.CREATE tokyo_by_age QUERY by_city EQ tokyo ORDER BY by_age DESC",
        "TABLE.DECLARE people PREFIX p: PK id COLUMN id i64 COLUMN name str",
        "HSET user:4 name dave city tokyo age 52",
    ] {
        assert!(!cmd(h, line).starts_with('-'), "{line}");
    }
}

/// `IDX.LIST` without its memory figure: a segment built in one pass from
/// the keyspace is smaller than one grown write by write.
fn listed(h: u32) -> String {
    let reply = cmd(h, "IDX.LIST");
    let lines: Vec<&str> = reply.split("\r\n").collect();
    let mut kept = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let is_size = i >= 2 && lines[i - 2] == "bytes";
        kept.push(if is_size { "<bytes>" } else { l });
    }
    kept.join("\r\n")
}

/// What a reload must reproduce.
fn answers(h: u32) -> Vec<String> {
    let mut all = vec![listed(h)];
    all.extend(
        [
            "IDX.QUERY by_city EQ tokyo LIMIT 10",
            "IDX.QUERY by_age RANGE 30 60 LIMIT 10",
            "VIEW.QUERY tokyo_by_age LIMIT 10",
            "VIEW.LIST",
            "TABLE.LIST",
        ]
        .iter()
        .map(|line| cmd(h, line)),
    );
    all
}

#[test]
fn a_reload_from_the_log_rebuilds_the_declared_indexes() {
    let src = kevy_open(OPEN_CAPTURE_AOF);
    declare(src);
    let want = answers(src);
    assert!(want[1].contains("user:4") && want[3].contains("user:4"), "{want:?}");
    assert!(want[0].contains("by_city") && want[0].contains("by_age"), "{want:?}");
    let log = drain(src);

    let dst = kevy_open(OPEN_CAPTURE_AOF);
    feed(dst, &log);
    assert_eq!(answers(dst), want);
    // the reloaded catalog is the one this store goes on recording: a
    // later declaration and a reload of the whole log keep both
    cmd(dst, "IDX.CREATE by_name ON PREFIX user: FIELD name TYPE str KIND range");
    let more = drain(dst);
    let third = kevy_open(0);
    feed(third, &log);
    feed(third, &more);
    assert_eq!(
        cmd(third, "IDX.QUERY by_name EQ carol LIMIT 10"),
        cmd(dst, "IDX.QUERY by_name EQ carol LIMIT 10")
    );
    assert!(cmd(third, "IDX.LIST").contains("by_name"));
    for h in [src, dst, third] {
        kevy_close(h);
    }
}

#[test]
fn a_compacted_image_carries_the_catalog() {
    let src = kevy_open(OPEN_CAPTURE_AOF);
    declare(src);
    kevy_aof_dump(src);
    let image = out(src);
    let dst = kevy_open(0);
    feed(dst, &image);
    assert_eq!(answers(dst), answers(src));
    kevy_close(src);
    kevy_close(dst);
}

#[test]
fn a_dropped_index_stays_dropped_after_a_reload() {
    let src = kevy_open(OPEN_CAPTURE_AOF);
    declare(src);
    cmd(src, "IDX.DROP by_age");
    let log = drain(src);
    let dst = kevy_open(0);
    feed(dst, &log);
    assert_eq!(listed(dst), listed(src));
    assert!(!cmd(dst, "IDX.LIST").contains("by_age"));
    kevy_close(src);
    kevy_close(dst);
}

#[test]
fn an_image_of_a_store_without_indexes_holds_only_its_keys() {
    let src = kevy_open(OPEN_CAPTURE_AOF);
    cmd(src, "SET k v");
    kevy_aof_dump(src);
    let image = out(src);
    assert!(!image.windows(17).any(|w| w == b"XINTERNAL.CATALOG"));
    kevy_close(src);
}
