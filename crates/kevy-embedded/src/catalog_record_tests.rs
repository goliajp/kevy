//! Which catalog frames the store takes: a newer one, another lineage's on
//! a full sync, nothing it cannot read; and which of two frames a restore
//! keeps.

use std::sync::{Arc, PoisonError};

use kevy_index::{Catalog, IndexKind, IndexSpec, ValType};
use kevy_resp::Argv;

use super::{CatalogRegs, decode};
use crate::shard_restore::keep_newest;

fn frame(parts: &[&[u8]]) -> Argv {
    Argv::from(parts.iter().map(|p| p.to_vec()).collect::<Vec<_>>())
}

fn verb() -> &'static [u8] {
    kevy_resp::ops_table::CATALOG.as_bytes()
}

/// An index catalog's side-file text holding one index, `name`.
fn index_text(name: &str) -> String {
    let mut cat = Catalog::new();
    let spec = IndexSpec::builder(name, "u:", IndexKind::Range, ValType::I64).with_field("age");
    cat.create(spec.build().unwrap()).unwrap();
    cat.to_sidecar()
}

fn at(lineage: &str, version: &str, index: &str) -> Argv {
    frame(&[verb(), lineage.as_bytes(), version.as_bytes(), index.as_bytes(), b"", b""])
}

fn held_index(regs: &CatalogRegs) -> String {
    regs.indexes.catalog.read().unwrap_or_else(PoisonError::into_inner).1.to_sidecar()
}

#[test]
fn a_frame_the_store_cannot_read_decodes_to_nothing() {
    let text = index_text("age");
    let bad_utf8: &[u8] = &[0xff];
    let malformed = [
        frame(&[verb(), b"1", b"1"]),
        frame(&[verb(), b"1", b"1", text.as_bytes()]),
        frame(&[verb(), b"1", b"1", text.as_bytes(), b""]),
        frame(&[verb(), b"1", b"1", bad_utf8, b"", b""]),
        frame(&[verb(), b"1", b"1", b"not a catalog", b"", b""]),
        frame(&[verb(), b"1", b"1", b"", b"not a catalog", b""]),
        frame(&[verb(), b"1", b"1", b"", b"", b"not a catalog"]),
        frame(&[verb(), b"one", b"1", b"", b"", b""]),
        frame(&[verb(), b"1", b"one", b"", b"", b""]),
        frame(&[verb(), bad_utf8, b"1", b"", b"", b""]),
    ];
    for (i, f) in malformed.iter().enumerate() {
        assert!(decode(f).is_none(), "frame {i} decoded");
    }
    let (pos, icat, vcat, tcat) = decode(&at("7", "3", "")).unwrap();
    assert_eq!(pos, (7, 3));
    assert!(icat.is_empty() && vcat.is_empty() && tcat.is_empty());
    let (_, icat, _, _) = decode(&at("7", "3", &text)).unwrap();
    assert_eq!(icat.to_sidecar(), text);
}

#[test]
fn a_store_takes_a_newer_frame_and_on_a_full_sync_another_lineage() {
    let regs = CatalogRegs::new(Arc::default());
    let (one, two) = (index_text("one"), index_text("two"));
    regs.adopt(Some(&at("5", "2", &one)), false);
    assert_eq!((regs.at(), held_index(&regs)), ((5, 2), one.clone()));

    regs.adopt(Some(&at("5", "1", &two)), false);
    assert_eq!((regs.at(), held_index(&regs)), ((5, 2), one.clone()), "an older version");
    regs.adopt(Some(&at("3", "1", &two)), false);
    assert_eq!(regs.at(), (5, 2), "another lineage outside a full sync");
    regs.adopt(Some(&at("5", "1", "not a catalog")), true);
    assert_eq!((regs.at(), held_index(&regs)), ((5, 2), one), "a frame it cannot read");

    regs.adopt(Some(&at("3", "1", &two)), true);
    assert_eq!((regs.at(), held_index(&regs)), ((3, 1), two), "another lineage on a full sync");
    regs.adopt(None, false);
    assert_eq!(regs.at(), (3, 1), "no frame outside a full sync changes nothing");
    regs.adopt(None, true);
    assert_eq!((regs.at(), held_index(&regs)), ((0, 0), Catalog::new().to_sidecar()));
    regs.adopt(None, true);
    assert_eq!(regs.at(), (0, 0));
}

#[test]
fn a_restore_keeps_the_newest_frame_it_can_read() {
    let mut held = None;
    let bad_utf8: &[u8] = &[0xff];
    keep_newest(&mut held, at("4", "2", ""));
    for (why, f) in [
        ("an older version", at("4", "1", "")),
        ("no version", frame(&[verb(), b"9"])),
        ("a lineage that is not text", frame(&[verb(), bad_utf8, b"9"])),
    ] {
        keep_newest(&mut held, f);
        assert_eq!(held, Some(at("4", "2", "")), "{why}");
    }
    keep_newest(&mut held, at("4", "3", ""));
    assert_eq!(held, Some(at("4", "3", "")));
}
