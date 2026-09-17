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

fn reshard_args<'a>(entry: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    [&["--cluster", "reshard", entry][..], extra].concat()
}

#[test]
fn reshard_moves_slots_atomically_and_reports_a_failed_task() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let shared = cluster(3, 0);
    let polls = std::sync::Arc::new(AtomicUsize::new(0));
    let seen = polls.clone();
    shared.lock().unwrap().hook = Some(Box::new(move |node, argv| {
        let words: Vec<String> =
            argv.iter().map(|a| String::from_utf8_lossy(a).to_uppercase()).collect();
        match (node, words.iter().map(String::as_str).collect::<Vec<_>>().as_slice()) {
            (1, ["CLUSTER", "MIGRATION", "IMPORT", ..]) => Some(bulk("t1")),
            (1, ["CLUSTER", "MIGRATION", "STATUS", ..]) => {
                let state =
                    if seen.fetch_add(1, Ordering::Relaxed) == 0 { "running" } else { "completed" };
                Some(
                    format!(
                        "*1\r\n*4\r\n$5\r\nstate\r\n{}{}$10\r\nlast_error\r\n$0\r\n\r\n",
                        format_args!("${}\r\n", state.len()),
                        format_args!("{state}\r\n")
                    )
                    .into_bytes(),
                )
            }
            (2, ["CLUSTER", "MIGRATION", "IMPORT", ..]) => Some(bulk("t2")),
            (2, ["CLUSTER", "MIGRATION", "STATUS", ..]) => Some(
                b"*1\r\n*4\r\n$5\r\nstate\r\n$6\r\nfailed\r\n$10\r\nlast_error\r\n$4\r\nboom\r\n"
                    .to_vec(),
            ),
            _ => None,
        }
    }));
    let ports = start(&shared);
    let entry = at(ports[0]);
    let o = cli(
        &reshard_args(
            &entry,
            &[
                "--cluster-from",
                &id(0),
                "--cluster-to",
                &id(1),
                "--cluster-slots",
                "3",
                "--cluster-yes",
            ],
        ),
        b"",
        &[],
    );
    let tail = format!(
        "\nReady to move 3 slots.\n  Source nodes:\n    M: {i0} {p0}\n       slots:[0-5460] (5461 slots) master\n  Destination node:\n    M: {i1} {p1}\n       slots:[5461-10921] (5461 slots) master\n  Resharding plan:\n    Moving slot 0 from {i0}\n    Moving slot 1 from {i0}\n    Moving slot 2 from {i0}\nMoving 3 slots from {p0} to {p1}\nWaiting for migration task t1 to complete.\n",
        i0 = id(0),
        i1 = id(1),
        p0 = at(ports[0]),
        p1 = at(ports[1])
    );
    assert!(o.stdout.ends_with(&tail) && o.code == 0, "{}", o.stdout);
    assert!(received(&shared, 1).contains(&vec![
        "CLUSTER".into(),
        "MIGRATION".into(),
        "IMPORT".into(),
        "0".into(),
        "2".into()
    ]));
    assert!(polls.load(Ordering::Relaxed) >= 2);
    let failed = cli(
        &reshard_args(
            &entry,
            &[
                "--cluster-from",
                &id(0),
                "--cluster-to",
                &id(2),
                "--cluster-slots",
                "1",
                "--cluster-yes",
            ],
        ),
        b"",
        &[],
    );
    assert!(
        failed.stdout.ends_with(
            "Waiting for migration task t2 to complete.\n[ERR] Migration task t2 failed: boom\n"
        ) && failed.code == 1,
        "{}",
        failed.stdout
    );
}

#[test]
fn reshard_slot_by_slot_replaces_keys_whose_values_match() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let shared = cluster(2, 0);
    shared.lock().unwrap().nodes[1].version = "7.4.10".into();
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let n = calls.clone();
    shared.lock().unwrap().hook = Some(Box::new(move |node, argv| {
        let verb = String::from_utf8_lossy(&argv[0]).to_uppercase();
        let sub =
            argv.get(1).map(|a| String::from_utf8_lossy(a).to_uppercase()).unwrap_or_default();
        match (node, verb.as_str(), sub.as_str()) {
            (0, "CLUSTER", "GETKEYSINSLOT") if n.fetch_add(1, Ordering::Relaxed) == 0 => {
                Some(b"*2\r\n$1\r\na\r\n$1\r\nb\r\n".to_vec())
            }
            (0, "CLUSTER", "GETKEYSINSLOT") => Some(b"*0\r\n".to_vec()),
            (0, "MIGRATE", _) if !argv.iter().any(|a| a == b"REPLACE") => {
                Some(b"-ERR Target instance replied with error: BUSYKEY Target key name already exists.\r\n".to_vec())
            }
            (0, "MIGRATE", _) => Some(b"+OK\r\n".to_vec()),
            (_, "DEBUG", _) => Some(b"*2\r\n+abc\r\n+0000\r\n".to_vec()),
            _ => None,
        }
    }));
    let ports = start(&shared);
    let entry = at(ports[0]);
    let answers = format!("1\n{}\n{}\ndone\nyes\n", id(1), id(0));
    let o = cli(&reshard_args(&entry, &["--cluster-pipeline", "2"]), answers.as_bytes(), &[]);
    let tail = format!(
        "Do you want to proceed with the proposed reshard plan (yes/no)? Moving slot 0 from {} to {}: \n*** Target key exists\n*** Checking key values on both nodes...\n*** Replacing target keys...\n..\n",
        at(ports[0]),
        at(ports[1])
    );
    assert!(o.stdout.ends_with(&tail) && o.code == 0, "{}", o.stdout);
    assert!(o.stdout.contains("How many slots do you want to move (from 1 to 16384)? What is the receiving node ID? Please enter all the source node IDs.\n"));
    assert!(
        received(&shared, 1).iter().any(|w| w[..2] == ["CLUSTER", "SETSLOT"] && w[3] == "NODE")
    );
}

#[test]
fn reshard_refuses_what_it_cannot_do() {
    let shared = cluster(2, 1);
    let ports = start(&shared);
    let entry = at(ports[0]);
    let run = |extra: &[&str], stdin: &[u8]| cli(&reshard_args(&entry, extra), stdin, &[]);
    let replica =
        run(&["--cluster-from", "all", "--cluster-to", &id(2), "--cluster-slots", "1"], b"");
    assert!(
        replica.stdout.ends_with(&format!(
            "*** The specified node ({}) is not known or not a master, please retry.\n",
            id(2)
        )) && replica.code == 1
    );
    let itself =
        run(&["--cluster-from", &id(0), "--cluster-to", &id(0), "--cluster-slots", "1"], b"");
    assert!(
        itself.stdout.ends_with("*** It is not possible to use the target node as source node.\n")
    );
    assert_eq!(
        (itself.stderr.as_str(), itself.code),
        ("*** No source nodes given, operation aborted.\n", 1)
    );
    let asked = run(&[], format!("0\n2\n{}\n{}\nnope\n", id(1), id(1)).as_bytes());
    assert!(asked.stdout.ends_with("*** It is not possible to use the target node as source node.\nSource node #1: *** The specified node (nope) is not known or not a master, please retry.\n"), "{}", asked.stdout);
    let declined =
        run(&["--cluster-from", "all", "--cluster-to", &id(1), "--cluster-slots", "1"], b"no\n");
    assert!(declined.stdout.ends_with("(yes/no)? ") && declined.code == 1);
    shared.lock().unwrap().nodes[1].importing = vec![(1, 0)];
    let broken = run(&["--cluster-slots", "1"], b"");
    assert_eq!(
        (broken.stderr.as_str(), broken.code),
        ("*** Please fix your cluster problems before resharding\n", 1)
    );
}

fn uneven(extra_empty_master: bool) -> Shared {
    let shared = cluster(3, 0);
    {
        let mut st = shared.lock().unwrap();
        st.nodes[0].slots = vec![(0, 6000)];
        st.nodes[1].slots = vec![(6001, 12000)];
        st.nodes[2].slots = vec![(12001, 16383)];
        if extra_empty_master {
            st.nodes.push(cluster_fake::Node { id: id(3), ..Default::default() });
        }
    }
    shared
}

#[test]
fn rebalance_plans_by_weight_and_threshold() {
    let shared = uneven(true);
    let ports = start(&shared);
    let entry = at(ports[0]);
    let run = |extra: &[&str]| {
        cli(&[&["--cluster", "rebalance", entry.as_str()][..], extra].concat(), b"", &[])
    };
    let head = format!(
        ">>> Performing Cluster Check (using node {entry})\n[OK] All nodes agree about slots configuration.\n>>> Check for open slots...\n>>> Check slots coverage...\n[OK] All 16384 slots covered.\n"
    );
    let sim = run(&["--cluster-simulate", "--verbose"]);
    let want = format!(
        "{head}>>> Rebalancing across 3 nodes. Total weight = 3.00\n{p2} balance is -1079 slots\n{p1} balance is 539 slots\n{p0} balance is 540 slots\nMoving 540 slots from {p0} to {p2}\n{}\nMoving 539 slots from {p1} to {p2}\n{}\n",
        "#".repeat(540),
        "#".repeat(539),
        p0 = at(ports[0]),
        p1 = at(ports[1]),
        p2 = at(ports[2])
    );
    assert_eq!((sim.stdout.as_str(), sim.code), (want.as_str(), 0));
    let within = run(&["--cluster-threshold", "30"]);
    assert_eq!(
        within.stdout,
        format!("{head}*** No rebalancing needed! All nodes are within the 30.00% threshold.\n")
    );
    // Node 1 weighs 2 of 5: balances 2725, -553, 1107, -3276 plus the three
    // slots rounding leaves, charged to the short nodes in table order.
    let empty = run(&[
        "--cluster-use-empty-masters",
        "--cluster-simulate",
        "--verbose",
        "--cluster-weight",
        &format!("{}=2", id(1)),
    ]);
    let order = format!(
        ">>> Rebalancing across 4 nodes. Total weight = 5.00\n{p3} balance is -3277 slots\n{p1} balance is -555 slots\n{p2} balance is 1107 slots\n{p0} balance is 2725 slots\nMoving 2725 slots from {p0} to {p3}\n",
        p0 = at(ports[0]),
        p1 = at(ports[1]),
        p2 = at(ports[2]),
        p3 = at(ports[3])
    );
    assert!(empty.stdout.contains(&order), "{}", empty.stdout);
    // A prefix every id shares picks the first master in table order.
    let shared_prefix = run(&["--cluster-simulate", "--verbose", "--cluster-weight", "0000=2"]);
    assert!(
        shared_prefix.stdout.contains(&format!("{} balance is -2191 slots\n", at(ports[0]))),
        "{}",
        shared_prefix.stdout
    );
    let nobody = run(&["--cluster-weight", "ffff=2"]);
    assert_eq!((nobody.stdout.as_str(), nobody.code), ("*** No such master node ffff\n", 1));
    shared.lock().unwrap().nodes[1].importing = vec![(1, 0)];
    let broken = run(&[]);
    assert!(
        broken.stdout.ends_with("*** Please fix your cluster problems before rebalancing\n")
            && broken.code == 1
    );
}

#[test]
fn rebalance_moves_slots_one_at_a_time_on_older_servers() {
    let shared = uneven(false);
    {
        let mut st = shared.lock().unwrap();
        st.nodes[2].version = "7.4.10".into();
        st.hook = Some(Box::new(|_, argv| {
            argv.get(1)
                .is_some_and(|a| a.eq_ignore_ascii_case(b"GETKEYSINSLOT"))
                .then(|| b"*0\r\n".to_vec())
        }));
    }
    let ports = start(&shared);
    let o = cli(&["--cluster", "rebalance", &at(ports[0]), "--cluster-threshold", "20"], b"", &[]);
    assert!(
        o.stdout.ends_with(&format!(
            "Moving 539 slots from {} to {}\n{}\n",
            at(ports[1]),
            at(ports[2]),
            "#".repeat(539)
        )),
        "{}",
        o.stdout
    );
    assert_eq!(o.code, 0);
    let assigned = received(&shared, 2).iter().filter(|w| w.len() == 5 && w[3] == "NODE").count();
    assert_eq!(assigned, 540 + 539);
}

fn fix(entry: &str, extra: &[&str], stdin: &[u8]) -> Out {
    cli(&[&["--cluster", "fix", entry][..], extra].concat(), stdin, &[])
}

#[test]
fn fix_closes_open_slots_by_their_marks() {
    let shared = cluster(3, 0);
    {
        let mut st = shared.lock().unwrap();
        st.nodes[0].migrating = vec![(1, 1), (2, 1), (3, 1)];
        st.nodes[1].importing = vec![(1, 0), (3, 0)];
        st.nodes[2].importing = vec![(3, 0), (5, 0)];
        st.nodes[1].migrating = vec![(5461, 2)];
        st.nodes[2].importing.push((5461, 1));
        st.nodes[0].importing = vec![(5462, 1)];
        st.nodes[2].migrating = vec![(10930, 0)];
        st.nodes[1].importing.push((10930, 2));
        // A migrating mark on a node that does not own the slot: no case.
        st.nodes[1].migrating.push((10, 2));
    }
    let ports = start(&shared);
    let (p0, p1, p2) = (at(ports[0]), at(ports[1]), at(ports[2]));
    let o = fix(&p0, &[], b"");
    let cases = [
        format!(
            ">>> Fixing open slot 1\nSet as migrating in: {p0}\nSet as importing in: {p1}\n>>> Case 1: Moving slot 1 from {p0} to {p1}\nMoving slot 1 from {p0} to {p1}: \n"
        ),
        format!(
            ">>> Fixing open slot 2\nSet as migrating in: {p0}\n>>> Case 4: Closing slot 2 on {p0}\n"
        ),
        format!(
            ">>> Fixing open slot 3\nSet as migrating in: {p0}\nSet as importing in: {p1},{p2}\n>>> Case 3: Moving slot 3 from {p0} to {p1} and closing it on all the other importing nodes.\nMoving slot 3 from {p0} to {p1}: \n"
        ),
        format!(
            ">>> Fixing open slot 5\nSet as importing in: {p2}\n>>> Case 2: Moving all the 5 slot keys to its owner {p0}\nMoving slot 5 from {p2} to {p0}: \n>>> Setting 5 as STABLE in {p2}\n"
        ),
        format!(
            ">>> Fixing open slot 10930\nSet as migrating in: {p2}\nSet as importing in: {p1}\n>>> Case 1: Moving slot 10930 from {p2} to {p1}\n"
        ),
        format!(
            ">>> Fixing open slot 10\nSet as migrating in: {p1}\n[ERR] Sorry, kevy-cli can't fix this slot yet (work in progress). Slot is set as migrating in {p1}, as importing in , owner is {p0}\n"
        ),
    ];
    for case in &cases {
        assert!(o.stdout.contains(case.as_str()), "missing:\n{case}\nin:\n{}", o.stdout);
    }
    assert_eq!(o.code, 0);
}

#[test]
fn fix_settles_an_owner_by_keys_and_covers_slots() {
    let shared = cluster(3, 0);
    {
        let mut st = shared.lock().unwrap();
        st.nodes[1].importing = vec![(7, 0)];
        st.nodes[1].key_slots = vec![7, 100];
        st.nodes[0].slots = vec![(0, 99), (103, 5460)];
        st.nodes[0].key_slots = vec![101];
        st.nodes[2].key_slots = vec![101];
        st.hook = Some(Box::new(|node, argv| {
            (node == 2
                && argv[0].eq_ignore_ascii_case(b"CLUSTER")
                && argv[1].eq_ignore_ascii_case(b"COUNTKEYSINSLOT")
                && argv[2] == b"101")
                .then(|| b":3\r\n".to_vec())
        }));
    }
    let ports = start(&shared);
    let (p0, p1, p2) = (at(ports[0]), at(ports[1]), at(ports[2]));
    let declined = fix(&p0, &[], b"no\n");
    assert!(declined.stdout.contains(&format!("*** Found keys about slot 7 in non-owner node {p1}!\nSet as importing in: {p1}\n>>> No single clear owner for the slot, selecting an owner by # of keys...\n*** Configuring {p1} as the slot owner\n")), "{}", declined.stdout);
    assert!(declined.stdout.ends_with("The following uncovered slots have no keys across the cluster:\n[102]\nFix these slots by covering with a random node? (type 'yes' to accept): ") && declined.code == 1, "{}", declined.stdout);
    let fixed = fix(&p0, &[], b"yes\nyes\nyes\n");
    let tail = format!(
        ">>> Covering slot 102 with {p0}\nThe following uncovered slots have keys in just one node:\n[100]\nFix these slots by covering with those nodes? (type 'yes' to accept): >>> Covering slot 100 with {p1}\nThe following uncovered slots have keys in multiple nodes:\n[101]\nFix these slots by moving keys into a single node? (type 'yes' to accept): >>> Covering slot 101 moving keys to {p2}\nMoving slot 101 from {p0} to {p2}: \n"
    );
    assert!(fixed.stdout.ends_with(&tail) && fixed.code == 0, "{}", fixed.stdout);
}

#[test]
fn fix_refuses_unreachable_masters_and_reports_owners_it_cannot_merge() {
    let shared = cluster(3, 0);
    shared.lock().unwrap().nodes[2].down = true;
    let ports = start(&shared);
    let o = fix(&at(ports[0]), &[], b"");
    assert!(o.stdout.ends_with("*** Fixing slots coverage with 1 unreachable masters is dangerous: kevy-cli will assume that slots about masters that are not reachable are not covered, and will try to reassign them to the reachable nodes. This can cause data loss and is rarely what you want to do. If you really want to proceed use the --cluster-fix-with-unreachable-masters option.\n") && o.code == 1, "{}", o.stdout);
    let shared = cluster(2, 0);
    {
        let mut st = shared.lock().unwrap();
        // Node 0 owns slot 9 and holds more of its keys than node 1 does.
        st.nodes[1].key_slots = vec![9];
        st.hook =
            Some(Box::new(|node, argv| match (node, argv[0].to_ascii_uppercase().as_slice()) {
                (0, b"CLUSTER")
                    if argv[1].eq_ignore_ascii_case(b"COUNTKEYSINSLOT") && argv[2] == b"9" =>
                {
                    Some(b":2\r\n".to_vec())
                }
                (1, b"CLUSTER") if argv[1].eq_ignore_ascii_case(b"GETKEYSINSLOT") => {
                    Some(b"*1\r\n$1\r\nk\r\n".to_vec())
                }
                (1, b"MIGRATE") => Some(b"-MOVED 9 127.0.0.1:1\r\n".to_vec()),
                _ => None,
            }));
    }
    let ports = start(&shared);
    let (p0, p1) = (at(ports[0]), at(ports[1]));
    let owners = fix(&p0, &["--cluster-search-multiple-owners"], b"");
    let tail = format!(
        "[WARNING] Slot 9 has 2 owners:\n    {p0}\n    {p1}\n>>> Fixing multiple owners for slot 9...\n>>> Setting slot 9 owner: {p0}\nMoving slot 9 from {p1} to {p0}: \nNode {p1} replied with error:\nMOVED 9 127.0.0.1:1\n\nFailed to fix multiple owners for slot 9\n"
    );
    assert!(owners.stdout.ends_with(&tail), "{}", owners.stdout);
    assert_eq!(owners.code, 1);
}

#[test]
fn import_migrates_each_key_to_its_slots_owner() {
    let shared = cluster(2, 0);
    // Node 2 stands for the standalone source.
    shared.lock().unwrap().nodes.push(cluster_fake::Node {
        id: id(2),
        alone: true,
        ..Default::default()
    });
    shared.lock().unwrap().hook = Some(Box::new(|node, argv| {
        let verb = argv[0].to_ascii_uppercase();
        match (node, verb.as_slice()) {
            (2, b"AUTH") if argv.last().is_some_and(|p| p == b"wrong") => Some(b"-WRONGPASS invalid username-password pair\r\n".to_vec()),
            (2, b"AUTH") => Some(b"+OK\r\n".to_vec()),
            (2, b"INFO") => Some(bulk("# Cluster\r\ncluster_enabled:0\r\n")),
            (2, b"DBSIZE") => Some(b":3\r\n".to_vec()),
            (2, b"SCAN") => Some(b"*2\r\n$1\r\n0\r\n*3\r\n$1\r\na\r\n$1\r\nb\r\n$3\r\nbad\r\n".to_vec()),
            (2, b"MIGRATE") if argv[3] == b"bad" => Some(b"-ERR Target instance replied with error: BUSYKEY Target key name already exists.\r\n".to_vec()),
            (2, b"MIGRATE") => Some(b"+OK\r\n".to_vec()),
            _ => None,
        }
    }));
    let ports = start(&shared);
    let (p0, p1, src) = (at(ports[0]), at(ports[1]), at(ports[2]));
    let o = cli(
        &[
            "--cluster",
            "import",
            &p0,
            "--cluster-from",
            &src,
            "--cluster-copy",
            "--cluster-replace",
            "--cluster-from-pass",
            "pw",
        ],
        b"",
        &[],
    );
    // a is slot 15495 and b 3300: node 1 owns the upper half.
    let tail = format!(
        "*** Importing 3 keys from DB 0\nMigrating a to {p1}: OK\nMigrating b to {p0}: OK\nMigrating bad to {p1}: Source {src} replied with error:\nERR Target instance replied with error: BUSYKEY Target key name already exists.\n"
    );
    assert!(o.stdout.ends_with(&tail), "{}", o.stdout);
    assert_eq!(o.code, 1);
    assert!(
        received(&shared, 2).contains(
            &["MIGRATE", "127.0.0.1", &ports[1].to_string(), "a", "0", "60000", "COPY", "REPLACE"]
                .map(String::from)
                .to_vec()
        )
    );
    let refused = cli(
        &["--cluster", "import", &p0, "--cluster-from", &src, "--cluster-from-pass", "wrong"],
        b"",
        &[],
    );
    assert!(
        refused.stdout.ends_with(&format!(
            "Source {src} replied with error:\nWRONGPASS invalid username-password pair\n"
        )) && refused.code == 1
    );
    let member = cli(&["--cluster", "import", &p0, "--cluster-from", &p1], b"", &[]);
    assert!(
        member
            .stdout
            .ends_with(&format!("Source {p1} replied with error:\nERR unknown command 'INFO'\n")),
        "{}",
        member.stdout
    );
    let gone = cli(&["--cluster", "import", &p0, "--cluster-from", "127.0.0.1:1"], b"", &[]);
    assert_eq!(
        (gone.stderr.as_str(), gone.code),
        ("Could not connect to Redis at 127.0.0.1:1: Connection refused.\n", 1)
    );
    let bad = cli(&["--cluster", "import", &p0, "--cluster-from", "nothost"], b"", &[]);
    assert_eq!(
        bad.stderr,
        "[ERR] Invalid --cluster-from host. You need to pass a valid address (ie. 120.0.0.1:7000).\n"
    );
    let missing = cli(&["--cluster", "import", &p0], b"", &[]);
    assert_eq!(
        missing.stderr,
        "[ERR] Option '--cluster-from' is required for subcommand 'import'.\n"
    );
}

#[test]
fn backup_saves_each_master_and_the_layout() {
    let shared = cluster(2, 1);
    shared.lock().unwrap().nodes[1].importing = vec![(9000, 0)];
    shared.lock().unwrap().hook = Some(Box::new(|_, argv| {
        argv[0].eq_ignore_ascii_case(b"SYNC").then(|| b"$5\r\nhello".to_vec())
    }));
    let ports = start(&shared);
    let dir = std::env::temp_dir().join(format!("kevy-rcli-backup-{}", ports[0]));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_str().unwrap();
    let o = cli(&["--cluster", "backup", &at(ports[0]), d], b"", &[]);
    let tail = format!(
        ">>> Node {p0} -> Saving RDB...\n>>> Node {p1} -> Saving RDB...\nSaving cluster configuration to: {d}/nodes.json\n*** Cluster seems to have some problems, please be aware of it if you're going to restore this backup.\n[OK] Backup created into: {d}\n",
        p0 = at(ports[0]),
        p1 = at(ports[1])
    );
    assert!(o.stdout.ends_with(&tail) && o.code == 0, "{}", o.stdout);
    let file = dir.join(format!("redis-node-127.0.0.1-{}-{}.rdb", ports[0], id(0)));
    assert_eq!(std::fs::read(file).unwrap(), b"hello");
    let json = std::fs::read_to_string(dir.join("nodes.json")).unwrap();
    let node1 = format!(
        "  {{\n    \"name\": \"{}\",\n    \"host\": \"127.0.0.1\",\n    \"port\": {},\n    \"replicate\": null,\n    \"slots\": [[8192,16383]],\n    \"slots_count\": 8192,\n    \"flags\": \"master\",\n    \"current_epoch\": 1,\n    \"cluster_errors\": 1,\n    \"importing\": {{\"9000\": \"{}\"}}\n  }}",
        id(1),
        ports[1],
        id(0)
    );
    assert!(
        json.starts_with("[\n  {\n") && json.ends_with("\n]") && json.contains(&node1),
        "{json}"
    );
    assert!(json.contains(&format!("\"replicate\": \"{}\"", id(0))));
    assert_eq!(o.stderr.matches("SYNC sent to master, writing 5 bytes to '").count(), 2);
    let missing = cli(&["--cluster", "backup", &at(ports[0]), "/nonexistent"], b"", &[]);
    assert!(missing.stdout.ends_with("[ERR] The specified backup directory '/nonexistent' does not exist.\n[ERR] Failed to back cluster!\n") && missing.code == 1);
    let _ = std::fs::remove_dir_all(&dir);
}
