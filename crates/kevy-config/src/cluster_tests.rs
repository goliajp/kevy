//! Token parsing and rendering for [`super::PeerEntry`] and
//! [`super::ScopeEntry`].

mod peer_entry_tests {
    use super::super::*;

    #[test]
    fn parse_one_basic() {
        let p = PeerEntry::parse_one("node-1@10.0.0.1:6004").unwrap();
        assert_eq!(p.node_id, "node-1");
        assert_eq!(p.host, "10.0.0.1");
        assert_eq!(p.port, 6004);
        assert_eq!(p.client_port, None);
    }

    #[test]
    fn parse_one_extended_form_sets_client_port() {
        // `id@host:elect_port:client_port` syntax — added after
        // finding MISDIRECTED replies used the elect_port instead
        // of the main client port.
        let p = PeerEntry::parse_one("node-1@10.0.0.1:6011:6004").unwrap();
        assert_eq!(p.node_id, "node-1");
        assert_eq!(p.host, "10.0.0.1");
        assert_eq!(p.port, 6011);
        assert_eq!(p.client_port, Some(6004));
    }

    #[test]
    fn parse_one_extended_form_dns_host() {
        let p = PeerEntry::parse_one("primary@db-east.local:6011:6004").unwrap();
        assert_eq!(p.host, "db-east.local");
        assert_eq!(p.port, 6011);
        assert_eq!(p.client_port, Some(6004));
    }

    #[test]
    fn parse_one_four_fields_sets_repl_port_base() {
        let p = PeerEntry::parse_one("node-1@10.0.0.1:6011:6004:7100").unwrap();
        assert_eq!((p.port, p.client_port, p.repl_port_base), (6011, Some(6004), Some(7100)));
        assert_eq!(p.to_token(), "node-1@10.0.0.1:6011:6004:7100");
        assert!(PeerEntry::parse_one("node-1@10.0.0.1:1:2:3:4").is_none());
        assert!(PeerEntry::parse_one("node-1@10.0.0.1:6011:6004:x").is_none());
    }

    #[test]
    fn parse_one_dns_host() {
        let p = PeerEntry::parse_one("primary@db-east.local:6105").unwrap();
        assert_eq!(p.host, "db-east.local");
        assert_eq!(p.port, 6105);
    }

    #[test]
    fn parse_one_rejects_empty_id_host_or_bad_port() {
        assert!(PeerEntry::parse_one("@host:6004").is_none());
        assert!(PeerEntry::parse_one("id@:6004").is_none());
        assert!(PeerEntry::parse_one("id@host:NaN").is_none());
        assert!(PeerEntry::parse_one("id@host:99999").is_none()); // u16 overflow
        assert!(PeerEntry::parse_one("no-at-or-colon").is_none());
    }

    #[test]
    fn parse_list_three_peers_trim_tolerated() {
        let s = "a@1.1.1.1:6004, b@1.1.1.2:6004 ,c@1.1.1.3:6004";
        let peers = PeerEntry::parse_list(s).unwrap();
        assert_eq!(peers.len(), 3);
        assert_eq!(peers[1].node_id, "b");
    }

    #[test]
    fn parse_list_trailing_comma_ok() {
        let peers = PeerEntry::parse_list("a@h:1,b@h:2,").unwrap();
        assert_eq!(peers.len(), 2);
    }

    #[test]
    fn parse_list_first_bad_token_errs() {
        let err = PeerEntry::parse_list("a@h:1,bad-token,c@h:3").unwrap_err();
        assert_eq!(err.to_string(), "bad peer token: \"bad-token\"");
    }

    #[test]
    fn parse_list_empty_is_empty() {
        assert_eq!(PeerEntry::parse_list("").unwrap(), Vec::<PeerEntry>::new());
        assert_eq!(PeerEntry::parse_list("  ").unwrap(), Vec::<PeerEntry>::new());
    }

    #[test]
    fn to_token_round_trips() {
        for tok in ["node-1@10.0.0.1:6004", "node-1@10.0.0.1:6011:6004", "p@db-east.local:6105"] {
            let p = PeerEntry::parse_one(tok).unwrap();
            assert_eq!(p.to_token(), tok);
            assert_eq!(PeerEntry::parse_one(&p.to_token()), Some(p));
        }
    }

    #[test]
    fn a_built_peer_is_the_one_its_token_parses_to() {
        let legacy = PeerEntry::new("n1".into(), "10.0.0.1".into(), 6204);
        assert_eq!(legacy.to_token(), "n1@10.0.0.1:6204");
        let full = legacy.clone().with_client_port(6004).with_repl_port_base(7100);
        assert_eq!(full.to_token(), "n1@10.0.0.1:6204:6004:7100");
        assert_eq!(PeerEntry::parse_one(&full.to_token()), Some(full));
        assert_eq!(PeerEntry::parse_one(&legacy.to_token()), Some(legacy));
    }
}

mod scope_entry_tests {
    use super::super::*;

    #[test]
    fn parse_one_writer_only() {
        let s = ScopeEntry::parse_one("app:billing:=embed-billing-1").unwrap();
        assert_eq!(s.prefix, b"app:billing:");
        assert_eq!(s.writer, "embed-billing-1");
        assert_eq!(s.fallback, None);
    }

    #[test]
    fn parse_one_writer_and_fallback() {
        let s = ScopeEntry::parse_one("app:billing:=embed-1|fb-server-eu").unwrap();
        assert_eq!(s.writer, "embed-1");
        assert_eq!(s.fallback.as_deref(), Some("fb-server-eu"));
    }

    #[test]
    fn parse_one_prefix_with_colons() {
        // Colon-heavy prefixes are the common case; only `=` and `,`
        // are reserved.
        let s = ScopeEntry::parse_one("ns:tenant:42:=w").unwrap();
        assert_eq!(s.prefix, b"ns:tenant:42:");
    }

    #[test]
    fn parse_one_rejects_empty_prefix_or_writer() {
        assert!(ScopeEntry::parse_one("=writer").is_none());
        assert!(ScopeEntry::parse_one("prefix=").is_none());
        assert!(ScopeEntry::parse_one("no-equals").is_none());
    }

    #[test]
    fn parse_one_rejects_empty_fallback_side() {
        assert!(ScopeEntry::parse_one("p=writer|").is_none());
        assert!(ScopeEntry::parse_one("p=|fb").is_none());
    }

    #[test]
    fn parse_one_rejects_embedded_comma() {
        // The split-on-comma in `parse_list` makes commas inside a
        // token a parse error — operator probably typo'd
        // `prefix=writer,fallback` instead of `prefix=writer|fallback`.
        assert!(ScopeEntry::parse_one("p=writer,other").is_none());
    }

    #[test]
    fn parse_list_two_scopes() {
        let v = ScopeEntry::parse_list("app:billing:=w-bill|fb, app:auth:=w-auth").unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].writer, "w-bill");
        assert_eq!(v[0].fallback.as_deref(), Some("fb"));
        assert_eq!(v[1].writer, "w-auth");
        assert!(v[1].fallback.is_none());
    }

    #[test]
    fn parse_list_first_bad_token_errs() {
        let err = ScopeEntry::parse_list("p1=w1,no-eq,p3=w3").unwrap_err();
        assert_eq!(err.to_string(), "bad scope token: \"no-eq\"");
    }

    #[test]
    fn to_token_round_trips() {
        for tok in ["app:billing:=embed-billing-1", "app:billing:=embed-1|fb-server-eu"] {
            let s = ScopeEntry::parse_one(tok).unwrap();
            assert_eq!(s.to_token(), tok);
            assert_eq!(ScopeEntry::parse_one(&s.to_token()), Some(s));
        }
    }

    #[test]
    fn a_built_scope_is_the_one_its_token_parses_to() {
        let plain = ScopeEntry::new(b"app:".to_vec(), "w1".into());
        assert_eq!(plain.to_token(), "app:=w1");
        let backed = plain.clone().with_fallback("f1".into());
        assert_eq!(backed.to_token(), "app:=w1|f1");
        assert_eq!(ScopeEntry::parse_one("app:=w1|f1"), Some(backed));
        assert_eq!(ScopeEntry::parse_one("app:=w1"), Some(plain));
    }
}
