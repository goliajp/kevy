use std::error::Error;

use kevy_resp::{Argv, CmdError};
use kevy_store::{Store, StoreError};

use super::{StoreSearchError, store_search};

fn argv(cmd: &str) -> Vec<Vec<u8>> {
    cmd.split(' ').map(|s| s.as_bytes().to_vec()).collect()
}

fn geo_store() -> Store {
    let mut store = Store::new();
    let add = Argv::from(argv("GEOADD src 13.361389 38.115556 Palermo"));
    crate::exec(&mut store, b"GEOADD", &add, &mut Vec::new());
    crate::exec(&mut store, b"SET", &Argv::from(argv("SET str v")), &mut Vec::new());
    store
}

#[test]
fn a_storing_search_refuses_a_missing_member_and_a_wrong_type_source() {
    let mut store = geo_store();
    let member = "GEOSEARCHSTORE dst src FROMMEMBER Rome BYRADIUS 10 km";
    assert_eq!(store_search(&mut store, &argv(member)), Err(StoreSearchError::NoMember));
    let by_member = "GEORADIUSBYMEMBER src Rome 10 km STORE dst";
    assert_eq!(store_search(&mut store, &argv(by_member)), Err(StoreSearchError::NoMember));
    let wrong = "GEORADIUS str 13 38 10 km STORE dst";
    assert_eq!(
        store_search(&mut store, &argv(wrong)),
        Err(StoreSearchError::Store(StoreError::WrongType))
    );
    let hits = store_search(&mut store, &argv("GEORADIUS src 13 38 200 km STORE dst"));
    assert_eq!(hits.expect("a hit")[0].0, b"Palermo");
}

#[test]
fn a_verb_that_does_not_store_is_refused_as_unknown() {
    let mut store = geo_store();
    let r = store_search(&mut store, &argv("GEOSEARCH src FROMLONLAT 13 38 BYRADIUS 1 km"));
    assert_eq!(r, Err(StoreSearchError::Refused(CmdError::Wire("ERR unknown command"))));
}

#[test]
fn a_store_search_error_words_itself_as_its_reply_and_chains_its_cause() {
    let refused = StoreSearchError::Refused(CmdError::Wire("ERR syntax error"));
    let store = StoreSearchError::Store(StoreError::WrongType);
    let none = StoreSearchError::NoMember;
    assert_eq!(refused.as_wire(), "ERR syntax error");
    assert_eq!(none.as_wire(), "ERR could not decode requested zset member");
    assert_eq!(store.as_wire(), StoreError::WrongType.as_wire());
    assert_eq!(refused.to_string(), "syntax error");
    assert_eq!(none.to_string(), "could not decode requested zset member");
    assert_eq!(store.to_string(), "wrong type for this operation");
    let cause = |e: &StoreSearchError| e.source().map(|s| s.to_string());
    assert_eq!(cause(&refused).as_deref(), Some("ERR syntax error"));
    assert_eq!(cause(&store).as_deref(), Some("wrong type for this operation"));
    assert_eq!(cause(&none), None);
}
