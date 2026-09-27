//! redis-cli's options: what they hold (`Opts`) and their defaults.
//!
//! The parser is in `opts_parse`. Every flag redis-cli 8.10.1 accepts is
//! represented here, including the modes later phases implement, so that a
//! flag is never "unrecognized" merely because its mode is not written yet.

use super::format::{Delims, Output};

/// Connection and session options.
#[derive(Clone, Debug)]
pub(crate) struct Opts {
    pub(crate) host: Vec<u8>,
    pub(crate) port: i32,
    /// `-t`, seconds; `None` = no connect timeout.
    pub(crate) connect_timeout: Option<f64>,
    pub(crate) socket: Option<Vec<u8>>,
    pub(crate) repeat: i64,
    pub(crate) interval_us: u64,
    pub(crate) input_dbnum: i32,
    pub(crate) auth: Option<Vec<u8>>,
    pub(crate) user: Option<Vec<u8>>,
    pub(crate) askpass: bool,
    pub(crate) no_auth_warning: bool,
    pub(crate) output: Output,
    pub(crate) push_output: bool,
    pub(crate) delims: Delims,
    pub(crate) quoted_input: bool,
    pub(crate) stdin_lastarg: bool,
    pub(crate) stdin_tag: Option<Vec<u8>>,
    /// `-e`.
    pub(crate) set_errcode: bool,
    pub(crate) verbose: bool,
    /// `-c`.
    pub(crate) cluster_mode: bool,
    pub(crate) resp2: bool,
    /// 0 = RESP2 unless asked; 1 = `-3` (HELLO failure is fatal); 2 = implied
    /// by `--json`/`--quoted-json` (HELLO failure is reported and tolerated).
    pub(crate) resp3: u8,
    pub(crate) prefer_ipv4: bool,
    pub(crate) prefer_ipv6: bool,
    pub(crate) client_name: Option<Vec<u8>>,
    pub(crate) modes: Modes,
}

/// The special modes and their parameters (implemented in later phases).
#[derive(Clone, Debug, Default)]
pub(crate) struct Modes {
    pub(crate) eval: Option<Vec<u8>>,
    pub(crate) eval_ldb: bool,
    pub(crate) eval_ldb_sync: bool,
    pub(crate) latency: bool,
    pub(crate) latency_history: bool,
    pub(crate) latency_dist: bool,
    pub(crate) mono: bool,
    pub(crate) latency_percentiles: Vec<(f64, Vec<u8>)>,
    pub(crate) vset_recall: Option<Vec<u8>>,
    pub(crate) vset_recall_ele: i64,
    pub(crate) vset_recall_count: i64,
    pub(crate) vset_recall_ef: i64,
    pub(crate) lru_test: Option<i64>,
    pub(crate) replica: bool,
    pub(crate) stat: bool,
    pub(crate) scan: bool,
    pub(crate) pattern: Option<Vec<u8>>,
    pub(crate) count: i32,
    pub(crate) intrinsic_latency: Option<i32>,
    /// `--rdb` / `--functions-rdb` set their own mode flag and share the
    /// file name, which is how `--rdb a --functions-rdb b` is detected.
    pub(crate) rdb_file: Option<Vec<u8>>,
    pub(crate) getrdb: bool,
    pub(crate) functions_rdb: bool,
    pub(crate) pipe: bool,
    pub(crate) pipe_timeout: i32,
    pub(crate) bigkeys: bool,
    pub(crate) memkeys: bool,
    pub(crate) memkeys_samples: i64,
    pub(crate) hotkeys: bool,
    pub(crate) hotkeys_count: i32,
    pub(crate) keystats: bool,
    pub(crate) cursor: u64,
    pub(crate) top: u64,
    /// `--cluster <subcommand> [args]`, with the `--cluster-*` flags raw.
    pub(crate) cluster: Option<Vec<Vec<u8>>>,
    pub(crate) cluster_flags: Vec<(Vec<u8>, Option<Vec<u8>>)>,
    pub(crate) test_hint: Option<Vec<u8>>,
    pub(crate) test_hint_file: Option<Vec<u8>>,
}

impl Opts {
    /// redis-cli's `main` defaults. `stdout_is_tty` already folds in
    /// `FAKETTY`: the output mode is decided before options are parsed, so
    /// the flags override it.
    pub(crate) fn defaults(stdout_is_tty: bool) -> Opts {
        Opts {
            host: b"127.0.0.1".to_vec(),
            port: 6379,
            connect_timeout: None,
            socket: None,
            repeat: 1,
            interval_us: 0,
            input_dbnum: 0,
            auth: None,
            user: None,
            askpass: false,
            no_auth_warning: false,
            output: if stdout_is_tty { Output::Standard } else { Output::Raw },
            push_output: stdout_is_tty,
            delims: Delims { multibulk: b"\n".to_vec(), reply: b"\n".to_vec() },
            quoted_input: false,
            stdin_lastarg: false,
            stdin_tag: None,
            set_errcode: false,
            verbose: false,
            cluster_mode: false,
            resp2: false,
            resp3: 0,
            prefer_ipv4: false,
            prefer_ipv6: false,
            client_name: None,
            modes: Modes {
                vset_recall_ele: 1,
                vset_recall_count: 100,
                vset_recall_ef: 500,
                count: 10,
                pipe_timeout: 30,
                memkeys_samples: 0,
                hotkeys_count: 16,
                top: 10,
                ..Modes::default()
            },
        }
    }
}
