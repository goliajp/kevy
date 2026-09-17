//! The subcommands: their arity, their arguments and options as `--cluster
//! help` lists them.

/// One subcommand.
pub(crate) struct Sub {
    pub(crate) name: &'static str,
    /// Exactly this many arguments; negative: at least its magnitude.
    pub(crate) arity: i32,
    pub(crate) args: &'static str,
    pub(crate) options: &'static [&'static str],
}

const ENTRY: &str = "<host:port> or <host> <port> - separated by either colon or space";

/// In the order `--cluster help` lists them.
pub(crate) const SUBS: &[Sub] = &[
    Sub {
        name: "create",
        arity: -2,
        args: "host1:port1 ... hostN:portN",
        options: &["replicas <arg>"],
    },
    Sub { name: "check", arity: -1, args: ENTRY, options: &["search-multiple-owners"] },
    Sub { name: "info", arity: -1, args: ENTRY, options: &[] },
    Sub {
        name: "fix",
        arity: -1,
        args: ENTRY,
        options: &["search-multiple-owners", "fix-with-unreachable-masters"],
    },
    Sub {
        name: "reshard",
        arity: -1,
        args: ENTRY,
        options: &[
            "from <arg>",
            "to <arg>",
            "slots <arg>",
            "yes",
            "timeout <ms>",
            "pipeline <arg>",
            "replace",
        ],
    },
    Sub {
        name: "rebalance",
        arity: -1,
        args: ENTRY,
        options: &[
            "weight <node1=w1...nodeN=wN>",
            "use-empty-masters",
            "timeout <ms>",
            "simulate",
            "pipeline <arg>",
            "threshold <arg>",
            "replace",
        ],
    },
    Sub {
        name: "add-node",
        arity: 2,
        args: "new_host:new_port existing_host:existing_port",
        options: &["slave", "master-id <arg>"],
    },
    Sub { name: "del-node", arity: 2, args: "host:port node_id", options: &[] },
    Sub {
        name: "call",
        arity: -2,
        args: "host:port command arg arg .. arg",
        options: &["only-masters", "only-replicas"],
    },
    Sub { name: "set-timeout", arity: 2, args: "host:port milliseconds", options: &[] },
    Sub {
        name: "import",
        arity: 1,
        args: "host:port",
        options: &[
            "from <arg>",
            "from-user <arg>",
            "from-pass <arg>",
            "from-askpass",
            "copy",
            "replace",
        ],
    },
    Sub { name: "backup", arity: 2, args: "host:port backup_directory", options: &[] },
    Sub { name: "help", arity: 0, args: "", options: &[] },
];

/// `--cluster help`.
pub(crate) fn help() -> Vec<u8> {
    let mut out = String::from("Cluster Manager Commands:\n");
    for sub in SUBS {
        out.push_str(&format!("  {:<15}{}\n", sub.name, sub.args));
        for opt in sub.options {
            out.push_str(&format!("  {:<15}--cluster-{opt}\n", ""));
        }
    }
    out.push_str(
        "\nFor check, fix, reshard, del-node, set-timeout, info, rebalance, call, import, backup \
         you can specify the host and port of any working node in the cluster.\n\n\
         Cluster Manager Options:\n  --cluster-yes  Automatic yes to cluster commands prompts\n\n",
    );
    out.into_bytes()
}

/// Whether `n` arguments suit `sub`.
pub(crate) fn arity_ok(sub: &Sub, n: usize) -> bool {
    let want = sub.arity.unsigned_abs() as usize;
    match sub.arity {
        0 => true,
        a if a > 0 => n == want,
        _ => n >= want,
    }
}
