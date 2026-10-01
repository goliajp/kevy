use super::*;

fn state(threads: usize, cluster: bool) -> Arc<RuntimeState> {
    let mut cfg = kevy_config::Config::default();
    cfg.server.threads = threads;
    cfg.cluster.enabled = cluster;
    Arc::new(RuntimeState::new(Arc::new(cfg), "", threads).unwrap())
}

#[test]
fn an_inner_call_on_another_shards_key_is_refused_under_either_routing() {
    for (cluster, routing) in
        [(false, kevy_persist::Routing::KevyHash), (true, kevy_persist::Routing::Slots)]
    {
        let st = state(4, cluster);
        let home = kevy_rt::shard_of_key(b"k1", 4, routing);
        let argv: [&[u8]; 2] = [b"GET", b"k1"];
        assert_eq!(cross_shard_violation(&st, home, &argv), None, "cluster={cluster}");
        let err = String::from_utf8(cross_shard_violation(&st, (home + 1) % 4, &argv).unwrap());
        assert!(err.unwrap().starts_with("-CROSSSLOT "), "cluster={cluster}");
    }
}

#[test]
fn a_single_shard_server_never_refuses_an_inner_call() {
    let st = state(1, false);
    let argv: [&[u8]; 2] = [b"GET", b"anything"];
    assert_eq!(cross_shard_violation(&st, 0, &argv), None);
}
