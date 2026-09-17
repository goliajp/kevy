//! kevy-cli's cluster manager (`--cluster`) and `-c`, against a fake cluster.
//!
//! bench/cligate.py compares these commands byte for byte with redis-cli on a
//! real Redis cluster; these tests run under `cargo test` so coverage sees
//! the code, with expected outputs written from the rules that gate checks.

mod cluster_fake;

use cluster_fake::{Shared, bulk, cluster, start};
use std::io::Write;
use std::process::{Command, Stdio};

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

fn cli(args: &[&str], stdin: &[u8], env: &[(&str, &str)]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kevy-cli"));
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.env_remove("REDISCLI_CLUSTER_YES").env("TERM", "dumb");
    cmd.envs(env.iter().copied());
    let mut child = cmd.spawn().expect("run kevy-cli");
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    Out {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

fn at(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

fn id(n: usize) -> String {
    format!("{:040x}", 0xa000 + n)
}

/// Every command node `n` received, as words.
fn received(shared: &Shared, n: usize) -> Vec<Vec<String>> {
    shared.lock().unwrap().log.iter().filter(|(i, _)| *i == n).map(|(_, w)| w.clone()).collect()
}

#[test]
fn subcommands_and_addresses_are_validated_before_connecting() {
    let err = |args: &[&str]| {
        let o = cli(args, b"", &[]);
        (o.stdout, o.stderr, o.code)
    };
    let invalid = "[ERR] Invalid arguments: you need to pass either a valid address (ie. 120.0.0.1:7000) or space separated IP and port (ie. 120.0.0.1 7000)\n";
    assert_eq!(
        err(&["--cluster", "HELP"]),
        (String::new(), "Unknown --cluster subcommand\n".into(), 1)
    );
    let arity = "[ERR] Wrong number of arguments for specified --cluster sub command\n";
    assert_eq!(err(&["--cluster", "call", "127.0.0.1:1"]).1, arity);
    assert_eq!(err(&["--cluster", "info", "127.0.0.1"]).1, invalid);
    assert_eq!(err(&["--cluster", "info", "127.0.0.1", "0"]).1, invalid);
    assert_eq!(err(&["--cluster", "call", "127.0.0.1", "PING"]).1, invalid);
    assert_eq!(
        err(&["--cluster", "info", "127.0.0.1:-1"]).1,
        "Could not connect to Redis at 127.0.0.1:-1: Servname not supported for ai_socktype\n"
    );
    let refused = err(&["--cluster", "check", "127.0.0.1", "1"]);
    assert_eq!(
        (refused.1.as_str(), refused.2),
        ("Could not connect to Redis at 127.0.0.1:1: Connection refused\n", 1)
    );
    let help = err(&["--cluster", "help", "extra"]);
    assert!(help.0.starts_with("Cluster Manager Commands:\n  create         host1:port1 ... hostN:portN\n                 --cluster-replicas <arg>\n"));
    assert!(
        help.0.ends_with("  --cluster-yes  Automatic yes to cluster commands prompts\n\n")
            && help.2 == 1
    );
    let small = "Setting a node timeout of less than 100 milliseconds is a bad idea.\n";
    assert_eq!(err(&["--cluster", "set-timeout", "127.0.0.1:1", "abc"]).1, small);
}

#[test]
fn info_and_check_on_a_healthy_cluster() {
    let shared = cluster(3, 3);
    shared.lock().unwrap().nodes[1].keys = 17;
    let ports = start(&shared);
    let info = cli(&["--cluster", "info", "127.0.0.1", &ports[0].to_string()], b"", &[]);
    let masters = format!(
        "{} (00000000...) -> 0 keys | 5461 slots | 1 slaves.\n{} (00000000...) -> 17 keys | 5461 slots | 1 slaves.\n{} (00000000...) -> 0 keys | 5462 slots | 1 slaves.\n[OK] 17 keys in 3 masters.\n0.00 keys per slot on average.\n",
        at(ports[0]),
        at(ports[1]),
        at(ports[2])
    );
    assert_eq!((info.stdout.as_str(), info.code), (masters.as_str(), 0));
    let check = cli(&["--cluster", "check", &at(ports[3])], b"", &[]);
    let expected = format!(
        ">>> Performing Cluster Check (using node {r0})\nS: {i3} {r0}\n   slots: (0 slots) slave\n   replicates {i0}\nM: {i0} {m0}\n   slots:[0-5460] (5461 slots) master\n   1 additional replica(s)\n",
        r0 = at(ports[3]),
        m0 = at(ports[0]),
        i0 = id(0),
        i3 = id(3)
    );
    assert!(check.stdout.contains(&expected), "{}", check.stdout);
    assert!(check.stdout.ends_with("[OK] All nodes agree about slots configuration.\n>>> Check for open slots...\n>>> Check slots coverage...\n[OK] All 16384 slots covered.\n"));
    assert_eq!(check.code, 0);
}

#[test]
fn check_reports_open_slots_holes_disagreement_and_owners() {
    let shared = cluster(3, 1);
    {
        let mut st = shared.lock().unwrap();
        st.nodes[0].slots = vec![(0, 99), (201, 299), (300, 300), (301, 5460)];
        st.nodes[0].migrating = vec![(5, 1), (6, 1)];
        st.nodes[1].importing = vec![(6, 0)];
        st.nodes[3].down = true;
        st.nodes[2].key_slots = vec![7];
        // Node 2 sees slot 16383 unowned: its view disagrees.
        st.hook = Some(Box::new(|node, argv| {
            (node == 2 && argv.len() == 2 && argv[1].eq_ignore_ascii_case(b"NODES")).then(|| {
                bulk(&format!(
                    "{} 127.0.0.1:1@2 myself,master - 0 0 1 connected 10922-16382\n",
                    id(2)
                ))
            })
        }));
    }
    let ports = start(&shared);
    let check =
        cli(&["--cluster", "check", &at(ports[0]), "--cluster-search-multiple-owners"], b"", &[]);
    assert_eq!(
        check.stderr,
        format!("Could not connect to Redis at {}: Connection refused\n", at(ports[3]))
    );
    let tail = format!(
        "   slots:[0-99],[201-5460] (5360 slots) master\nM: {i1} {p1}\n   slots:[5461-10921] (5461 slots) master\nM: {i2} {p2}\n   slots:[10922-16383] (5462 slots) master\n[ERR] Nodes don't agree about configuration!\n>>> Check for open slots...\n[WARNING] Node {p0} has slots in migrating state 5,6.\n[WARNING] Node {p1} has slots in importing state 6.\n[WARNING] The following slots are open: 5,6.\n>>> Check slots coverage...\n[ERR] Not all 16384 slots are covered by nodes.\n\n>>> Check for multiple slot owners...\n[WARNING] Slot 7 has 2 owners:\n    {p0}\n    {p2}\n",
        p0 = at(ports[0]),
        p1 = at(ports[1]),
        p2 = at(ports[2]),
        i1 = id(1),
        i2 = id(2)
    );
    assert!(check.stdout.ends_with(&tail), "{}", check.stdout);
    assert_eq!(check.code, 1);
    let info = cli(&["--cluster", "info", &at(ports[0])], b"", &[("TERM", "xterm-256color")]);
    assert!(info.stdout.contains("\x1b[32;1m[OK] 0 keys in 3 masters.\n\x1b[0m0.00 keys per slot"));
}

#[test]
fn a_node_without_cluster_support_is_named() {
    let shared = cluster(1, 0);
    shared.lock().unwrap().hook = Some(Box::new(|_, _| {
        Some(b"-ERR This instance has cluster support disabled\r\n".to_vec())
    }));
    let ports = start(&shared);
    let o = cli(&["--cluster", "check", &at(ports[0])], b"", &[]);
    assert_eq!(
        (o.stdout, o.code),
        (format!("[ERR] Node {} is not configured as a cluster node.\n", at(ports[0])), 1)
    );
}

#[test]
fn call_runs_a_command_on_the_selected_nodes() {
    let shared = cluster(2, 1);
    shared.lock().unwrap().hook = Some(Box::new(|node, argv| match (node, argv[0].as_slice()) {
        (1, b"GET") => Some(b"-MOVED 1 127.0.0.1:1\r\n".to_vec()),
        (2, b"GET") => Some(Vec::new()), // hangs up: Failed!
        (_, b"GET") => Some(b"*2\r\n$1\r\na\r\n:1\r\n".to_vec()),
        _ => None,
    }));
    let ports = start(&shared);
    let o = cli(&["-d", "X", "--cluster", "call", &at(ports[0]), "GET", "k"], b"", &[]);
    let want = format!(
        ">>> Calling GET k\n{}: aX1\n{}: MOVED 1 127.0.0.1:1\n\n{}: Failed!\n",
        at(ports[0]),
        at(ports[1]),
        at(ports[2])
    );
    assert_eq!((o.stdout.as_str(), o.code), (want.as_str(), 0));
    let masters =
        cli(&["--cluster", "call", &at(ports[0]), "PING", "--cluster-only-masters"], b"", &[]);
    assert_eq!(
        masters.stdout,
        format!(">>> Calling PING\n{}: PONG\n{}: PONG\n", at(ports[0]), at(ports[1]))
    );
    let replicas = cli(
        &["--cluster", "call", &at(ports[0]), "PING", "--cluster-only-replicas"],
        b"",
        &[("TERM", "xterm")],
    );
    assert_eq!(
        replicas.stdout,
        format!("\x1b[29;1m>>> Calling\x1b[0m PING\n{}: PONG\n", at(ports[2]))
    );
}

#[test]
fn set_timeout_sets_and_persists_everywhere() {
    let shared = cluster(3, 0);
    shared.lock().unwrap().hook = Some(Box::new(|node, argv| match (node, argv[1].as_slice()) {
        (1, b"SET") => Some(b"-ERR bad value\r\n".to_vec()),
        (2, b"REWRITE") => Some(b"-ERR The server is running without a config file\r\n".to_vec()),
        _ => None,
    }));
    let ports = start(&shared);
    let o = cli(&["--cluster", "set-timeout", &at(ports[0]), "1500"], b"", &[]);
    let want = format!(
        ">>> Reconfiguring node timeout in every cluster node...\n*** New timeout set for {}\nERR setting node-timeout for {}: ERR bad value\nERR setting node-timeout for {}: ERR The server is running without a config file\n>>> New node timeout set. 1 OK, 2 ERR.\n",
        at(ports[0]),
        at(ports[1]),
        at(ports[2])
    );
    assert_eq!((o.stdout.as_str(), o.code), (want.as_str(), 0));
    assert_eq!(received(&shared, 0)[1], ["CONFIG", "SET", "cluster-node-timeout", "1500"]);
}

#[test]
fn dash_c_follows_moved_and_ask_and_stays_where_it_went() {
    let shared = cluster(3, 0);
    let ports = start(&shared);
    let (p1, p2) = (ports[1], ports[2]);
    shared.lock().unwrap().hook =
        Some(Box::new(move |node, argv| match (node, argv[0].to_ascii_uppercase().as_slice()) {
            (0, b"GET") => Some(format!("-MOVED 866 127.0.0.1:{p1}\r\n").into_bytes()),
            (1, b"GET") => Some(format!("-ASK 866 :{p2}\r\n").into_bytes()),
            (2, b"ASKING") => Some(b"+OK\r\n".to_vec()),
            (2, b"GET") => Some(b"$1\r\nv\r\n".to_vec()),
            (_, b"SET") => Some(b"-MOVED 1 127.0.0.1:1\r\n".to_vec()),
            _ => None,
        }));
    let one = cli(&["-c", "-p", &ports[0].to_string(), "GET", "k"], b"", &[]);
    assert_eq!((one.stdout.as_str(), one.code), ("v\n", 0));
    assert_eq!(received(&shared, 2)[0], ["ASKING"]);
    let repl = cli(&["-c", "-p", &ports[0].to_string()], b"get k\nset k v\nping\n", &[]);
    let want = format!(
        "-> Redirected to slot [866] located at 127.0.0.1:{p1}\n-> Redirected to slot [866] located at 127.0.0.1:{p2}\nv\n-> Redirected to slot [1] located at 127.0.0.1:1\n"
    );
    assert_eq!(repl.stdout, want);
    let refused = "Could not connect to Redis at 127.0.0.1:1: Connection refused\n";
    assert_eq!(repl.stderr, format!("{refused}{refused}"));
    let without = cli(&["-p", &ports[0].to_string(), "GET", "k"], b"", &[]);
    assert_eq!(without.stdout, format!("MOVED 866 127.0.0.1:{p1}\n\n"));
}

fn ports_of(ports: &[u16], args: &[&str]) -> Vec<String> {
    ports.iter().map(|p| at(*p)).chain(args.iter().map(|a| a.to_string())).collect()
}

#[test]
fn create_plans_confirms_and_joins() {
    let shared = cluster_fake::fresh(6);
    let ports = start(&shared);
    let mut args = vec!["--cluster".to_string(), "create".into()];
    args.extend(ports_of(&ports, &["--cluster-replicas", "1"]));
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let declined = cli(&argv, b"no\n", &[]);
    let (p, i) = (|n: usize| at(ports[n]), id);
    let plan = format!(
        ">>> Performing hash slots allocation on 6 nodes...\nMaster[0] -> Slots 0 - 5460\nMaster[1] -> Slots 5461 - 10922\nMaster[2] -> Slots 10923 - 16383\nAdding replica {} to {}\nAdding replica {} to {}\nAdding replica {} to {}\n>>> Trying to optimize slaves allocation for anti-affinity\n[WARNING] Some slaves are in the same host as their master\nM: {} {}\n   slots:[0-5460] (5461 slots) master\n",
        p(4),
        p(0),
        p(5),
        p(1),
        p(3),
        p(2),
        i(0),
        p(0)
    );
    assert!(declined.stdout.starts_with(&plan), "{}", declined.stdout);
    let replica_line = format!("S: {} {}\n   replicates {}\n", i(3), p(3), i(2));
    assert!(declined.stdout.contains(&replica_line));
    assert!(
        declined.stdout.ends_with("Can I set the above configuration? (type 'yes' to accept): ")
    );
    assert_eq!((declined.code, received(&shared, 0).len()), (0, 3));
    let made = cli(&argv, b"yes\n", &[]);
    assert!(made.stdout.contains(">>> Nodes configuration updated\n>>> Assign a different config epoch to each node\n>>> Sending CLUSTER MEET messages to join the cluster\nWaiting for the cluster to join\n"), "{}", made.stdout);
    assert!(
        made.stdout.ends_with("[OK] All 16384 slots covered.\n") && made.code == 0,
        "{}",
        made.stdout
    );
    let st = shared.lock().unwrap();
    assert_eq!(
        (st.nodes[1].slots.clone(), st.nodes[3].master, st.nodes[5].epoch),
        (vec![(5461, 10922)], Some(2), 6)
    );
}

#[test]
fn create_refuses_nodes_and_configurations_it_cannot_use() {
    let shared = cluster_fake::fresh(4);
    shared.lock().unwrap().nodes[2].keys = 1;
    let ports = start(&shared);
    let run = |args: &[String]| {
        let mut argv = vec!["--cluster", "create"];
        argv.extend(args.iter().map(String::as_str));
        cli(&argv, b"", &[])
    };
    let few = run(&ports_of(&ports[..2], &["--cluster-yes"]));
    assert_eq!(
        (few.stdout.as_str(), few.code),
        (
            "*** ERROR: Invalid configuration for cluster creation.\n*** Redis Cluster requires at least 3 master nodes.\n*** This is not possible with 2 nodes and 0 replicas per node.\n*** At least 3 nodes are required.\n",
            1
        )
    );
    let busy = run(&ports_of(&ports[..3], &[]));
    let msg = format!(
        "[ERR] Node {} is not empty. Either the node already knows other nodes (check with CLUSTER NODES) or contains some key in database 0.\n",
        at(ports[2])
    );
    assert_eq!((busy.stdout, busy.code), (msg, 1));
    let bad = run(&["127.0.0.1".to_string()]);
    assert_eq!((bad.stderr.as_str(), bad.code), ("Invalid address format: 127.0.0.1\n", 1));
    shared.lock().unwrap().hook = Some(Box::new(|node, argv| {
        (node == 3 && argv[1].eq_ignore_ascii_case(b"ADDSLOTS"))
            .then(|| b"-ERR Slot 1 is already busy\r\n".to_vec())
    }));
    shared.lock().unwrap().nodes[2].keys = 0;
    let refused = run(&[at(ports[3]), at(ports[0]), at(ports[1]), "--cluster-yes".into()]);
    assert!(
        refused.stdout.ends_with(&format!(
            "Node {} replied with error:\nERR Slot 1 is already busy\n",
            at(ports[3])
        )),
        "{}",
        refused.stdout
    );
    assert_eq!(refused.code, 1);
}

#[test]
fn add_node_as_master_or_replica() {
    let shared = cluster(2, 1);
    shared.lock().unwrap().nodes.push(cluster_fake::Node {
        id: id(3),
        alone: true,
        ..Default::default()
    });
    let ports = start(&shared);
    let (new, existing) = (at(ports[3]), at(ports[0]));
    let master = cli(&["--cluster", "add-node", &new, &existing], b"", &[]);
    let tail = format!(
        "[OK] All 16384 slots covered.\n>>> Getting functions from cluster\n>>> Send FUNCTION LIST to {new} to verify there is no functions in it\n>>> Send FUNCTION RESTORE to {new}\n>>> Send CLUSTER MEET to node {new} to make it join the cluster.\n[OK] New node added correctly.\n"
    );
    assert!(master.stdout.starts_with(&format!(
        ">>> Adding node {new} to cluster {existing}\n>>> Performing Cluster Check"
    )));
    assert!(master.stdout.ends_with(&tail) && master.code == 0, "{}", master.stdout);
    assert!(received(&shared, 3).contains(&vec![
        "FUNCTION".into(),
        "RESTORE".into(),
        "payload".into()
    ]));
    // Joined now; make it alone again to add it as a replica of the master
    // with fewest replicas (node 1: node 2 replicates node 0).
    shared.lock().unwrap().nodes[3].alone = true;
    let replica = cli(&["--cluster", "add-node", &new, &existing, "--cluster-slave"], b"", &[]);
    let tail = format!(
        "Automatically selected master {m}\n>>> Send CLUSTER MEET to node {new} to make it join the cluster.\nWaiting for the cluster to join\n\n>>> Configure node as replica of {m}.\n[OK] New node added correctly.\n",
        m = at(ports[1])
    );
    assert!(replica.stdout.ends_with(&tail), "{}", replica.stdout);
    assert_eq!(shared.lock().unwrap().nodes[3].master, Some(1));
    let named = cli(
        &[
            "--cluster",
            "add-node",
            &new,
            &existing,
            "--cluster-slave",
            "--cluster-master-id",
            "nope",
        ],
        b"",
        &[],
    );
    assert!(named.stdout.ends_with("[ERR] No such master ID nope\n") && named.code == 1);
    let busy = cli(&["--cluster", "add-node", &at(ports[1]), &existing], b"", &[]);
    assert!(busy.stdout.ends_with(&format!("[ERR] Node {} is not empty. Either the node already knows other nodes (check with CLUSTER NODES) or contains some key in database 0.\n", at(ports[1]))));
    shared.lock().unwrap().nodes[1].importing = vec![(5, 0)];
    let open = cli(&["--cluster", "add-node", &new, &existing], b"", &[]);
    assert!(
        open.stdout.ends_with(">>> Check slots coverage...\n[OK] All 16384 slots covered.\n")
            && open.code == 1
    );
    shared.lock().unwrap().nodes[1].importing.clear();
    let gone = cli(&["--cluster", "add-node", "127.0.0.1:1", &existing], b"", &[]);
    assert!(
        gone.stdout.ends_with("[ERR] Sorry, can't connect to node 127.0.0.1:1\n") && gone.code == 1
    );
    assert_eq!(gone.stderr, "Could not connect to Redis at 127.0.0.1:1: Connection refused\n");
}

#[test]
fn del_node_moves_replicas_forgets_and_resets() {
    let shared = cluster(3, 1);
    shared.lock().unwrap().nodes[0].slots.clear();
    shared.lock().unwrap().nodes.push(cluster_fake::Node {
        id: id(4),
        master: Some(1),
        ..Default::default()
    });
    let ports = start(&shared);
    let o = cli(&["--cluster", "del-node", &at(ports[1]), &id(0)], b"", &[]);
    let want = format!(
        ">>> Removing node {} from cluster {}\n>>> Sending CLUSTER FORGET messages to the cluster...\n>>> {} as replica of {}\n>>> Sending CLUSTER RESET SOFT to the deleted node.\n",
        id(0),
        at(ports[1]),
        at(ports[3]),
        at(ports[2])
    );
    assert_eq!((o.stdout.as_str(), o.code), (want.as_str(), 0));
    assert_eq!(received(&shared, 0).last().unwrap(), &["CLUSTER", "RESET", "SOFT"]);
    assert!(received(&shared, 2).iter().any(|w| w[..2] == ["CLUSTER", "FORGET"]));
    let full = cli(&["--cluster", "del-node", &at(ports[0]), &id(1)], b"", &[]);
    assert!(full.stdout.ends_with(&format!(
        "[ERR] Node {} is not empty! Reshard data away and try again.\n",
        at(ports[1])
    )));
    let none = cli(&["--cluster", "del-node", &at(ports[0]), "abc"], b"", &[]);
    assert_eq!((none.stdout.ends_with("[ERR] No such node ID abc\n"), none.code), (true, 1));
    assert_eq!(cli(&["--cluster", "del-node", "127.0.0.1", "abc"], b"", &[]).code, 1);
}
