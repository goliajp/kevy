//! kevy-cli's redis-cli half against a real kevy server.
//!
//! The byte-for-byte comparison with redis-cli itself is `bench/cligate.py`,
//! which needs docker and a Redis. These cases run under `cargo test`
//! instead, so coverage sees the code. The expected bytes are written from
//! redis-cli 8.10.1's output rules — the ones that gate checks against the
//! real binary — not taken from kevy-cli's own output. Error texts that come
//! from the server are matched by prefix, since kevy's wording is its own.

use std::io::Write;
use std::process::{Child, Command, Stdio};

struct Srv {
    child: Child,
    port: u16,
    dir: std::path::PathBuf,
}

impl Srv {
    fn start() -> Srv {
        let port = kevy_testnet::free_port();
        let bin =
            std::path::Path::new(env!("CARGO_BIN_EXE_kevy-cli")).parent().unwrap().join("kevy");
        if !bin.exists() {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
            let status = Command::new(cargo)
                .args(["build", "-p", "kevy", "--bin", "kevy"])
                .status()
                .expect("spawn cargo build");
            assert!(status.success(), "cargo build -p kevy --bin kevy failed");
        }
        let dir = std::env::temp_dir().join(format!("kevy-rcli-{port}"));
        std::fs::create_dir_all(&dir).unwrap();
        let child = Command::new(&bin)
            .args(["--port", &port.to_string(), "--threads", "1", "--no-aof"])
            .args(["--dir", dir.to_str().unwrap()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kevy server");
        kevy_testnet::assert_listening(port, "the server under test");
        Srv { child, port, dir }
    }

    fn port(&self) -> String {
        self.port.to_string()
    }
}

impl Drop for Srv {
    /// Ask the server to stop and wait for it; kill only one that will not.
    ///
    /// Killing outright raced a server already on its way out after a test's
    /// SHUTDOWN: under coverage it was cut down while writing its profile, and
    /// one torn profile makes llvm-profdata refuse to merge any of them.
    fn drop(&mut self) {
        let pid = self.child.id().to_string();
        let _ = Command::new("kill").args(["-TERM", &pid]).status(); // may have exited already
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while matches!(self.child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Out {
    stdout: String,
    stderr: String,
    code: i32,
}

/// Run kevy-cli with `args`, `stdin` and extra environment, as a pipe (so
/// stdout is not a terminal unless `FAKETTY` says otherwise).
fn cli(args: &[&str], stdin: &[u8], env: &[(&str, &str)]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kevy-cli"));
    cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    for var in ["FAKETTY", "REDISCLI_AUTH", "VALKEYCLI_AUTH", "KEVYCLI_AUTH"] {
        cmd.env_remove(var);
    }
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

const TTY: &[(&str, &str)] = &[("FAKETTY", "1")];

#[test]
fn output_mode_follows_the_terminal_and_the_flags() {
    let s = Srv::start();
    let p = s.port();
    assert_eq!(cli(&["-p", &p, "SET", "k", "a b"], b"", &[]).stdout, "OK\n");
    assert_eq!(cli(&["-p", &p, "GET", "k"], b"", &[]).stdout, "a b\n");
    assert_eq!(cli(&["-p", &p, "GET", "k"], b"", TTY).stdout, "\"a b\"\n");
    assert_eq!(cli(&["--no-raw", "-p", &p, "GET", "k"], b"", &[]).stdout, "\"a b\"\n");
    assert_eq!(cli(&["--raw", "-p", &p, "GET", "k"], b"", TTY).stdout, "a b\n");
    assert_eq!(cli(&["-p", &p, "GET", "missing"], b"", TTY).stdout, "(nil)\n");
    assert_eq!(cli(&["-p", &p, "INCR", "n"], b"", TTY).stdout, "(integer) 1\n");
    cli(&["-p", &p, "RPUSH", "l", "a", "b c", "1"], b"", &[]);
    assert_eq!(
        cli(&["--csv", "-p", &p, "LRANGE", "l", "0", "-1"], b"", &[]).stdout,
        "\"a\",\"b c\",\"1\"\n"
    );
    assert_eq!(
        cli(&["--json", "-p", &p, "LRANGE", "l", "0", "-1"], b"", &[]).stdout,
        "[\"a\",\"b c\",\"1\"]\n"
    );
    assert_eq!(cli(&["--json", "-p", &p, "GET", "missing"], b"", &[]).stdout, "null\n");
    assert_eq!(cli(&["-d", ",", "-p", &p, "LRANGE", "l", "0", "-1"], b"", &[]).stdout, "a,b c,1\n");
    assert_eq!(
        cli(&["-p", &p, "LRANGE", "l", "0", "-1"], b"", TTY).stdout,
        "1) \"a\"\n2) \"b c\"\n3) \"1\"\n"
    );
    cli(&["-p", &p, "HSET", "h", "f", "v"], b"", &[]);
    assert_eq!(cli(&["-3", "-p", &p, "HGETALL", "h"], b"", TTY).stdout, "1# \"f\" => \"v\"\n");
    assert_eq!(cli(&["--json", "-p", &p, "HGETALL", "h"], b"", &[]).stdout, "{\"f\":\"v\"}\n");
    assert_eq!(
        cli(&["--quoted-json", "-x", "-p", &p, "ECHO"], b"a\nb\xff", &[]).stdout,
        "\"a\\\\nb\\\\xff\"\n"
    );
}

#[test]
fn input_repeat_and_error_flags() {
    let s = Srv::start();
    let p = s.port();
    assert_eq!(cli(&["-x", "-p", &p, "ECHO"], b"a\0b\xff", TTY).stdout, "\"a\\x00b\\xff\"\n");
    assert_eq!(cli(&["-X", "TAG", "-p", &p, "SET", "k", "TAG"], b"payload", &[]).stdout, "OK\n");
    assert_eq!(cli(&["-p", &p, "GET", "k"], b"", &[]).stdout, "payload\n");
    let missing = cli(&["-X", "TAG", "-p", &p, "SET", "k", "v"], b"payload", &[]);
    assert_eq!(
        (missing.stderr.as_str(), missing.code),
        ("Using -X option but stdin tag not match.\n", 1)
    );
    assert_eq!(
        cli(&["-r", "3", "-i", "0.01", "-p", &p, "INCR", "c"], b"", &[]).stdout,
        "1\n2\n3\n"
    );
    assert_eq!(cli(&["-r", "0", "-p", &p, "INCR", "c"], b"", &[]).stdout, "");
    assert_eq!(cli(&["-D", "|", "-r", "2", "-p", &p, "INCR", "d"], b"", &[]).stdout, "1|2|");
    let quoted = cli(&["--quoted-input", "-p", &p, "ECHO", "\"a\\x41\\n\""], b"", TTY);
    assert_eq!(quoted.stdout, "\"aA\\n\"\n");
    let bad = cli(&["--quoted-input", "-p", &p, "ECHO", "\"open"], b"", &[]);
    assert_eq!((bad.stdout.as_str(), bad.code), ("Invalid quoted string\n", 1));

    cli(&["-p", &p, "SET", "s", "word"], b"", &[]);
    let soft = cli(&["-p", &p, "INCR", "s"], b"", TTY);
    assert!(soft.stdout.starts_with("(error) ERR"), "{}", soft.stdout);
    assert_eq!(soft.code, 0, "an error reply exits 0 without -e, as redis-cli");
    let hard = cli(&["-e", "-r", "3", "-p", &p, "INCR", "s"], b"", TTY);
    assert_eq!((hard.stdout.as_str(), hard.code), ("", 1));
    assert!(hard.stderr.starts_with("ERR"), "{}", hard.stderr);
}

#[test]
fn piped_repl_quotes_repeats_and_state() {
    let s = Srv::start();
    let p = s.port();
    let lines = b"SET k \"a b\\x41\"\nGET k\n3 INCR n\n0 INCR n\nSET k \"open\n\nMULTI\nINCR n\nEXEC\nquit\nPING\n";
    let out = cli(&["-p", &p], lines, &[]);
    assert_eq!(
        out.stdout,
        "OK\na bA\n1\n2\n3\nInvalid kevy-cli repeat command option value.\nInvalid argument(s)\nOK\nQUEUED\n4\n"
    );
    assert_eq!(out.code, 0);
    let formatted =
        cli(&["-p", &p], b"GET k\nSELECT 0\n:set hints\n:set nope\n:nope\nrestart\n", TTY);
    assert_eq!(
        formatted.stdout,
        "\"a bA\"\nOK\nunknown kevy-cli preference 'nope'\nunknown kevy-cli internal command ':nope'\nUse 'restart' only in Lua debugging mode.\n"
    );
    let e_ignored = cli(&["-e", "-p", &p], b"INCR k\nPING\n", &[]);
    assert!(e_ignored.stdout.ends_with("PONG\n") && e_ignored.code == 0, "{}", e_ignored.stdout);
    let sub = cli(&["-p", &p], b"SUBSCRIBE c\n", &[]);
    assert_eq!(sub.stdout, "subscribe\nc\n1\n");
}

#[test]
fn connection_options_and_session_setup() {
    let s = Srv::start();
    let p = s.port();
    assert_eq!(cli(&["-h", "127.0.0.1", "-p", &p, "-t", "1.5", "PING"], b"", &[]).stdout, "PONG\n");
    assert_eq!(
        cli(&["-u", &format!("redis://127.0.0.1:{p}/0"), "PING"], b"", &[]).stdout,
        "PONG\n"
    );
    assert_eq!(cli(&["-u", &format!("valkey://127.0.0.1:{p}"), "PING"], b"", &[]).stdout, "PONG\n");
    assert_eq!(
        cli(&["-n", "0", "--name", "rcli", "-p", &p, "CLIENT", "GETNAME"], b"", &[]).stdout,
        "rcli\n"
    );
    assert_eq!(cli(&["-3", "-p", &p, "PING"], b"", &[]).stdout, "PONG\n");
    let refused = cli(&["-p", "1", "PING"], b"", &[]);
    assert_eq!(
        (refused.stderr.as_str(), refused.code),
        ("Could not connect to Redis at 127.0.0.1:1: Connection refused\n", 1)
    );
    let askpass = cli(&["--askpass", "-p", "1", "PING"], b"secret\n", &[]);
    assert_eq!(askpass.code, 1);
    let env_auth = cli(&["-p", &p, "PING"], b"", &[("REDISCLI_AUTH", "x")]);
    assert!(env_auth.stderr.starts_with("AUTH failed: "), "kevy has no AUTH: {}", env_auth.stderr);
    let warned = cli(&["-a", "x", "-p", "1", "PING"], b"", &[]);
    assert!(
        warned.stderr.starts_with("Warning: Using a password with '-a' or '-u' option"),
        "{}",
        warned.stderr
    );
}

#[test]
fn option_errors_exit_with_redis_cli_messages() {
    let err = |args: &[&str]| {
        let o = cli(args, b"", &[]);
        (o.stderr, o.code)
    };
    let one = |msg: &str| (format!("{msg}\n"), 1);
    assert_eq!(err(&["-2", "-3", "PING"]), one("Options -2 and -3 are mutually exclusive."));
    assert_eq!(err(&["-x", "-X", "t", "PING"]), one("Options -x and -X are mutually exclusive."));
    assert_eq!(err(&["-4", "-6", "PING"]), one("Options -4 and -6 are mutually exclusive."));
    assert_eq!(
        err(&["-c", "-s", "/tmp/x", "PING"]),
        one("Options -c and -s are mutually exclusive.")
    );
    assert_eq!(err(&["-p", "70000", "PING"]), one("Invalid server port."));
    assert_eq!(err(&["-p", "abc", "PING"]), one("Invalid server port."));
    assert_eq!(err(&["-t", "soon", "PING"]), one("Invalid connection timeout for -t."));
    assert_eq!(
        err(&["--no-such"]),
        one("Unrecognized option or bad number of args for: '--no-such'")
    );
    assert_eq!(err(&["-p"]), one("Unrecognized option or bad number of args for: '-p'"));
    assert_eq!(err(&["-u", "http://x"]), one("Invalid URI scheme"));
    assert_eq!(err(&["-u", "redis://a%zz@h"]), one("Illegal character in URI encoding"));
    assert_eq!(err(&["-u", "redis://a%@h"]), one("Incomplete URI encoding"));
    assert_eq!(
        err(&["--tls", "PING"]),
        one("kevy-cli: --tls is not supported: kevy-cli does not implement TLS")
    );
    assert_eq!(
        err(&["--rdb", "a", "--functions-rdb", "b"]),
        one("Option --functions-rdb and --rdb are mutually exclusive.")
    );
    assert_eq!(
        err(&["--quoted-pattern", "\"x"]),
        one("Invalid quoted string specified for --quoted-pattern.")
    );
    assert_eq!(err(&["--memkeys-samples", "3x"]), one("--memkeys-samples conversion error."));
    assert_eq!(
        err(&["--keystats-samples", "-1"]),
        one("--keystats-samples value should be positive.")
    );
    assert_eq!(err(&["--cursor", "-5"]), one("--cursor should be followed by a positive integer."));
    assert_eq!(err(&["--top", "1y"]), one("--top conversion error."));
    assert_eq!(
        err(&["--latency-percentiles", "50,101"]),
        one(
            "Invalid percentile '101' in --latency-percentiles (must be a number between 0 and 100)."
        )
    );
    assert_eq!(err(&["--pipe"]), one("kevy-cli: --pipe is not implemented yet"));
    let help = cli(&["--help"], b"", &[]);
    assert!(
        help.stdout.contains("-p <port>") && help.stdout.contains("sql compile") && help.code == 0
    );
    assert_eq!(cli(&["-v"], b"", &[]).stdout, format!("kevy-cli {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn failure_paths_and_rare_modes() {
    let s = Srv::start();
    let p = s.port();
    let select = cli(&["-n", "3", "-p", &p, "PING"], b"", &[]);
    assert!(select.stderr.starts_with("SELECT 3 failed: ERR"), "{}", select.stderr);
    assert_eq!(
        select.stdout, "PONG\n",
        "a failed handshake leaves the connection, as redis-cli's does"
    );
    let name = cli(&["--name", "a b", "-p", &p, "PING"], b"", &[]);
    assert!(name.stderr.starts_with("CLIENT SETNAME failed: ERR"), "{}", name.stderr);
    let unix = cli(&["-s", "/nonexistent/kevy.sock", "PING"], b"", &[]);
    let unix_msg =
        "Could not connect to Redis at /nonexistent/kevy.sock: No such file or directory\n";
    assert_eq!((unix.stderr.as_str(), unix.code), (unix_msg, 1));
    let v6 = cli(&["-u", "redis://[::1]:1", "PING"], b"", &[]);
    assert_eq!(
        (v6.stderr.as_str(), v6.code),
        ("Could not connect to Redis at ::1:1: Connection refused\n", 1)
    );
    let timeout = cli(&["-t", "1", "-p", "1", "PING"], b"", &[]);
    assert_eq!(timeout.stderr, "Could not connect to Redis at 127.0.0.1:1: Connection refused\n");

    // The server hangs up after a reply. Which error the next command meets
    // is a race: the FIN reads as "closed", the RST answering the write after
    // the close as "reset", and hiredis prints whichever arrives first — so
    // does kevy-cli. (A canned server, not QUIT to kevy: on Linux kevy has
    // been seen answering a second QUIT before it closes.)
    let hung_up = ["Error: Server closed the connection\n", "Error: Connection reset by peer\n"];
    let dropped = canned(b"+OK\r\n", &["-r", "2", "PING"], b"");
    assert_eq!((dropped.stdout.as_str(), dropped.code), ("OK\n", 1));
    assert!(hung_up.contains(&dropped.stderr.as_str()), "{}", dropped.stderr);
    // In the REPL the hang-up is reported and the next command reconnects.
    let (port, server) = fake_server_seq(&[b"+OK\r\n", b"+PONG\r\n"], None);
    let repl = cli(&["-p", &port.to_string()], b"2 PING\nPING\n", &[]);
    assert_eq!(repl.stdout, "OK\nPONG\n");
    assert!(hung_up.contains(&repl.stderr.as_str()), "{}", repl.stderr);
    server.join().unwrap();

    // MONITOR the server refuses leaves monitor mode on the error.
    let monitor = cli(&["-p", &p, "MONITOR"], b"", TTY);
    assert!(monitor.stdout.starts_with("(error) ERR") && monitor.code == 0, "{}", monitor.stdout);

    // The pub/sub prompt, with the terminal's colour.
    let sub = cli(&["-p", &p], b"SUBSCRIBE c\n", &[("FAKETTY", "1"), ("TERM", "xterm")]);
    let prompt =
        "\x1b[1;90mReading messages... (press Ctrl-C to quit or any key to type command)\r\x1b[0m";
    assert!(sub.stdout.contains(prompt), "{:?}", sub.stdout);
}

/// A prompt kept away from the user's history and preferences files.
fn prompt_env(dir: &std::path::Path) -> Vec<(&'static str, String)> {
    vec![
        ("TERM", "xterm".into()),
        ("KEVYCLI_HISTFILE", dir.join("history").to_string_lossy().into_owned()),
        ("KEVYCLI_RCFILE", "/dev/null".into()),
    ]
}

#[test]
fn help_hints_and_completion_come_from_the_servers_reference() {
    let s = Srv::start();
    let p = s.port();
    // A block per entry: bold name, grey syntax, yellow labels, CRLF lines.
    let get = cli(&["-p", &p, "help", "get"], b"", &[]);
    assert!(
        get.stdout.starts_with(
            "\r\n  \x1b[1mGET\x1b[0m \x1b[90mkey\x1b[0m\r\n  \x1b[33msummary:\x1b[0m "
        ),
        "{:?}",
        get.stdout
    );
    assert!(get.stdout.ends_with("  \x1b[33mgroup:\x1b[0m string\r\n\r\n"), "{:?}", get.stdout);
    // kevy's note on a command that differs from Redis is part of its help.
    assert!(cli(&["-p", &p, "help", "hscan"], b"", &[]).stdout.contains("\x1b[33mcompat:\x1b[0m "));
    let group = cli(&["-p", &p, "?", "@string"], b"", &[]).stdout;
    assert!(group.contains("\x1b[1mINCRBY\x1b[0m") && !group.contains("group:"), "{group:?}");
    assert_eq!(cli(&["-p", &p, "help", "nosuch"], b"", &[]).stdout, "\r\n");
    let overview = cli(&["-p", &p, "help"], b"", &[]).stdout;
    assert!(
        overview.starts_with("kevy-cli ") && overview.contains("\"help @<group>\""),
        "{overview}"
    );
    // No server: the same reference, embedded.
    assert_eq!(cli(&["-p", "1", "help", "get"], b"", &[]).stdout, get.stdout);

    // A kevy server documents syntax lines, shown until an argument is typed.
    let hint = |input: &str| cli(&["-p", &p, "--test_hint", input], b"", &[]).stdout;
    assert_eq!(hint("set "), "key value [EX seconds|PX milliseconds] [NX|XX]\n");
    assert_eq!(hint("set k "), "\n");
    assert_eq!(hint("nosuch "), "\n");

    let dir = std::env::temp_dir().join(format!("kevy-rcli-hints-{}", s.port));
    std::fs::create_dir_all(&dir).unwrap();
    let cases = dir.join("hints.txt");
    std::fs::write(&cases, "# kevy\n\"get \" \"key\"\n\"get \" \"wrong\"\n\n").unwrap();
    let file = cli(&["-p", &p, "--verbose", "--test_hint_file", cases.to_str().unwrap()], b"", &[]);
    assert_eq!(
        file.stdout,
        "Input: 'get ', Expected: 'key', Hint: 'key'\nInput: 'get ', Expected: 'wrong', Hint: 'key'\nFAILURE: 1/2 passed\n"
    );
    assert_eq!(
        (file.stderr.as_str(), file.code),
        ("Test case 'get ' FAILED: expected 'wrong', got 'key'\n", 1)
    );
    std::fs::write(&cases, "\"get \"\n").unwrap();
    let missing = cli(&["-p", &p, "--test_hint_file", cases.to_str().unwrap()], b"", &[]);
    assert_eq!(
        (missing.stderr.as_str(), missing.code),
        ("Missing expected hint for input 'get '\n", 255)
    );
    let absent = cli(&["-p", &p, "--test_hint_file", "/nonexistent/hints"], b"", &[]);
    assert_eq!(
        (absent.stderr.as_str(), absent.code),
        ("Can't open file '/nonexistent/hints': No such file or directory\n", 255)
    );

    // At a terminal: the grey hint follows `get `, Tab turns `ec` into `ECHO`,
    // and `:set nohints` turns hints off. Each line waits for its prompt: between
    // lines the terminal is not raw, and a Ctrl-U typed then is the terminal's.
    let prompt = format!("127.0.0.1:{p}> ");
    let mut term = PtyLive::start(&["-p", &p], &prompt_env(&dir));
    term.wait_for(&prompt);
    term.type_keys("get ");
    term.wait_for("get \x1b[0;90;49mkey");
    term.type_keys("\x15ec\t hi\r");
    term.wait_for("\"hi\"\r\n");
    term.type_keys(":set nohints\r");
    term.wait_count(&prompt, 3);
    term.type_keys("get x");
    term.wait_for("get x");
    assert!(
        !term.output().rsplit(":set nohints").next().unwrap_or_default().contains("\x1b[0;90;49m")
    );
    term.type_keys("\x15\x04");
    assert_eq!(term.finish(), 0);
    // FAKETTY_WITH_PROMPT edits a piped stdin but, as with redis-cli, fetches
    // no reference: no hints there.
    let faked = cli(
        &["-p", &p],
        b"get \x15\x04",
        &[("FAKETTY_WITH_PROMPT", "1"), ("KEVYCLI_HISTFILE", "/dev/null/none")],
    );
    assert!(!faked.stdout.contains("\x1b[0;90;49m"), "{:?}", faked.stdout);
    // --askpass at a prompt masks what is typed.
    let mut ask = PtyLive::start(&["--askpass", "-p", &p, "PING"], &prompt_env(&dir));
    ask.wait_for("Please input password: ");
    ask.type_keys("abc");
    ask.wait_for("***");
    ask.type_keys("\r");
    ask.wait_for("PONG");
    let shown = ask.output();
    assert!(!shown.contains("abc"), "{shown:?}");
    assert_eq!(ask.finish(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cluster_manager_flags_parse_before_the_mode_is_refused() {
    let refused = |args: &[&str]| cli(args, b"", &[]).stderr;
    let msg = "kevy-cli: --cluster is not implemented yet\n";
    assert_eq!(
        refused(&["--cluster", "create", "--cluster-replicas", "1", "h:1", "h:2", "--cluster-yes"]),
        msg
    );
    assert_eq!(refused(&["--cluster", "create", "--cluster-yes", "h:1", "h:2", "stray"]), msg);
    assert_eq!(refused(&["--cluster-weight", "a=1", "b=2", "--cluster", "rebalance", "h:1"]), msg);
    assert_eq!(
        refused(&["--cluster-weight", "a=1", "--cluster-weight", "b=1"]),
        "WARNING: you cannot use --cluster-weight more than once.\nYou can set more weights by adding them as a space-separated list, ie:\n--cluster-weight n1=w n2=w\n"
    );
    assert_eq!(cli(&["--cluster"], b"", &[]).code, 1);
}

/// A server that answers the first command on each connection with one
/// canned byte string and hangs up: enough to show kevy-cli frames kevy
/// cannot send (a RESP3 push), a unix socket, which the kevy server does not
/// listen on, and a reconnect, when given more than one reply.
fn fake_server(
    reply: &'static [u8],
    unix: Option<&std::path::Path>,
) -> (u16, std::thread::JoinHandle<()>) {
    fake_server_seq(&[reply], unix)
}

fn fake_server_seq(
    replies: &[&'static [u8]],
    unix: Option<&std::path::Path>,
) -> (u16, std::thread::JoinHandle<()>) {
    use std::io::Read;
    fn serve<S: Read + Write>(mut conn: S, reply: &[u8]) {
        let mut buf = [0u8; 1024];
        if matches!(conn.read(&mut buf), Ok(n) if n > 0) {
            let _ = conn.write_all(reply);
        }
    }
    let replies = replies.to_vec();
    match unix {
        Some(path) => {
            let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
            (
                0,
                std::thread::spawn(move || {
                    replies.iter().for_each(|r| serve(listener.accept().unwrap().0, r))
                }),
            )
        }
        None => {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            (
                port,
                std::thread::spawn(move || {
                    replies.iter().for_each(|r| serve(listener.accept().unwrap().0, r))
                }),
            )
        }
    }
}

#[test]
fn unix_sockets_and_server_pushes() {
    let dir = std::env::temp_dir().join(format!("kevy-rcli-unix-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("s.sock");
    let (_, server) = fake_server(b"+PONG\r\n", Some(&sock));
    assert_eq!(cli(&["-s", sock.to_str().unwrap(), "PING"], b"", &[]).stdout, "PONG\n");
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // A client-side-caching invalidation arriving ahead of the reply is
    // printed by the push handler, redis-cli's `-> invalidate:` line.
    let push = b">2\r\n$10\r\ninvalidate\r\n*1\r\n$1\r\nk\r\n+PONG\r\n";
    let (port, server) = fake_server(push, None);
    let out = cli(&["-p", &port.to_string(), "PING"], b"", TTY);
    assert_eq!(out.stdout, "-> invalidate: 'k'\nPONG\n");
    server.join().unwrap();

    // Name resolution fails before any connect: reported, exit 1.
    let dns = cli(&["-t", "1", "-h", "no-such-host.invalid", "PING"], b"", &[]);
    assert!(
        dns.stderr.starts_with("Could not connect to Redis at no-such-host.invalid:6379: ")
            && dns.code == 1,
        "{}",
        dns.stderr
    );
}

#[test]
fn repl_state_tracking_against_kevy() {
    let s = Srv::start();
    let p = s.port();
    // The same lines bench/cligate.py runs against Redis, where kevy agrees.
    let tx = cli(
        &["-p", &p],
        b"MULTI\nSET a 1\nDISCARD\nGET a\nWATCH k\nSET k 1\nMULTI\nINCR k\nEXEC\n",
        &[],
    );
    assert_eq!(tx.stdout, "OK\nQUEUED\nOK\n\nOK\nOK\nOK\nQUEUED\n\n");
    let sub = cli(&["-3", "-p", &p], b"SUBSCRIBE c\nPUBLISH c hi\nUNSUBSCRIBE c\nPING\n", &[]);
    assert_eq!(sub.stdout, "subscribe\nc\n1\n1\nmessage\nc\nhi\nunsubscribe\nc\n0\nPONG\n");
    let reset = cli(
        &["-p", &p],
        b"RESET\nHELLO 3\nHELLO 2\nAUTH x\nclear\nconnect 127.0.0.1 1\nPING\n",
        &[],
    );
    assert!(reset.stdout.starts_with("ERR unknown command 'RESET'"), "{}", reset.stdout);
    assert!(reset.stdout.contains("\x1b[H\x1b[2J"), "clear: {:?}", reset.stdout);
    assert!(
        reset.stderr.contains("Could not connect to Redis at 127.0.0.1:1: Connection refused"),
        "{}",
        reset.stderr
    );
    let slow = cli(&["-p", &p], b"DEBUG SLEEP 0.6\n", TTY);
    assert!(slow.stdout.starts_with("OK\n(0.") && slow.stdout.ends_with("s)\n"), "{}", slow.stdout);
    let pushes_on = cli(&["--show-pushes", "y", "-p", &p], b"SUBSCRIBE c\n", &[]);
    assert_eq!(pushes_on.stdout, "subscribe\nc\n1\n");
    assert_eq!(
        cli(&["-p", &p, "SHUTDOWN", "NOSAVE"], b"", &[]).code,
        0,
        "SHUTDOWN's hang-up is its success"
    );
}

#[test]
fn handshake_and_protocol_failures() {
    // AUTH with a user sends both; the canned reply refuses it.
    let (port, server) = fake_server(b"-WRONGPASS invalid username-password pair\r\n", None);
    let auth = cli(
        &["--user", "u", "-a", "p", "--no-auth-warning", "-p", &port.to_string(), "PING"],
        b"",
        &[],
    );
    assert!(auth.stderr.starts_with("AUTH failed: WRONGPASS"), "{}", auth.stderr);
    server.join().unwrap();

    // HELLO 3 refused: fatal with -3, reported and tolerated with --json.
    let (port, server) = fake_server(b"-NOPROTO unsupported protocol version\r\n", None);
    let fatal = cli(&["-3", "-p", &port.to_string(), "PING"], b"", &[]);
    assert!(fatal.stderr.starts_with("HELLO 3 failed: NOPROTO"), "{}", fatal.stderr);
    server.join().unwrap();

    // The server closes during the handshake.
    let (port, server) = fake_server(b"", None);
    let io = cli(&["-n", "1", "-p", &port.to_string(), "PING"], b"", &[]);
    assert!(io.stderr.starts_with("\nI/O error\n"), "{:?}", io.stderr);
    server.join().unwrap();

    // A reply that is not RESP, in hiredis's words.
    let (port, server) = fake_server(b"@nonsense\r\n", None);
    let bad = cli(&["-p", &port.to_string(), "PING"], b"", &[]);
    assert_eq!(
        (bad.stderr.as_str(), bad.code),
        ("Error: Protocol error, got \"@\" as reply type byte\n", 1)
    );
    server.join().unwrap();
}

#[test]
fn rarer_repl_lines_against_kevy() {
    let s = Srv::start();
    let p = s.port();
    let out = cli(&["-p", &p], b"   \nAUTH u p\nHELLO 9\nUNSUBSCRIBE c\nPING\n", TTY);
    assert!(out.stdout.starts_with("(error) ERR"), "AUTH u p: {}", out.stdout);
    assert!(out.stdout.ends_with("PONG\n"), "{}", out.stdout);
    let askpass = cli(&["--askpass", "-p", &p, "PING"], b"", &[]);
    assert_eq!(askpass.stdout, "PONG\n", "no password on stdin means no AUTH");
    let killed = cli(&["-r", "2", "-p", &p, "SHUTDOWN", "NOSAVE"], b"", &[]);
    assert_eq!(killed.code, 1, "the repeat has no connection left to send on");
}

/// Canned-reply cases: the server sends `reply` to the first command and
/// hangs up.
fn canned(reply: &'static [u8], args: &[&str], stdin: &[u8]) -> Out {
    let (port, server) = fake_server(reply, None);
    let port = port.to_string();
    let mut full: Vec<&str> = vec!["-p", &port];
    full.extend_from_slice(args);
    let out = cli(&full, stdin, &[]);
    server.join().unwrap();
    out
}

#[test]
fn servers_that_say_unusual_things() {
    assert_eq!(canned(b"+RESET\r\n", &[], b"RESET\n").stdout, "RESET\n");
    let nested = canned(b"*1\r\n@x\r\n", &["PING"], b"");
    assert_eq!((nested.stderr.as_str(), nested.code), ("Error: Protocol error\n", 1));
    let blob = canned(b"!8\r\nERR blob\r\n", &["-e", "PING"], b"");
    assert_eq!((blob.stderr.as_str(), blob.code), ("ERR blob\n", 1));
    let hello = canned(b"!7\r\nNOPROTO\r\n", &["-3", "PING"], b"");
    assert!(hello.stderr.starts_with("HELLO 3 failed: NOPROTO\n"), "{}", hello.stderr);
    let name = canned(b"!4\r\nERRx\r\n", &["--name", "n", "PING"], b"");
    assert!(name.stderr.starts_with("CLIENT SETNAME failed: ERRx\n"), "{}", name.stderr);

    let closed = "Error: Server closed the connection\n";
    let confirm = b"*3\r\n$9\r\nsubscribe\r\n$1\r\nc\r\n:1\r\n";
    let oneshot = canned(confirm, &["SUBSCRIBE", "c"], b"");
    assert_eq!(
        (oneshot.stdout.as_str(), oneshot.stderr.as_str(), oneshot.code),
        ("subscribe\nc\n1\n", closed, 1)
    );
    let monitor = canned(b"+OK\r\n", &["MONITOR"], b"");
    assert_eq!(
        (monitor.stdout.as_str(), monitor.stderr.as_str(), monitor.code),
        ("OK\n", closed, 1)
    );
    // While a subscribe confirmation is awaited: a push that is not pub/sub
    // and a frame that only looks like one are both read past.
    let noise = b">2\r\n$10\r\ninvalidate\r\n*0\r\n*3\r\n:1\r\n:2\r\n:3\r\n";
    let skipped = canned(noise, &["SUBSCRIBE", "c"], b"");
    assert_eq!(skipped.stdout, "invalidate\n\n1\n2\n3\n");
}

#[test]
fn handshake_successes_and_pubsub_breakage() {
    // SELECT answered +OK: the db is switched, then the server hangs up.
    let selected = canned(b"+OK\r\n", &["-n", "2", "PING"], b"");
    assert_eq!(selected.code, 1);
    // HELLO met by a hang-up.
    let hello = canned(b"", &["-3", "PING"], b"");
    assert!(hello.stderr.starts_with("\nI/O error\n"), "{:?}", hello.stderr);
    // A malformed frame right behind the subscribe confirmation, read by the
    // pub/sub prompt's drain.
    let broken =
        canned(b"*3\r\n$9\r\nsubscribe\r\n$1\r\nc\r\n:1\r\n@bad\r\n", &[], b"SUBSCRIBE c\n");
    assert_eq!(
        (broken.stderr.as_str(), broken.code),
        ("Error: Protocol error, got \"@\" as reply type byte\n", 1)
    );

    let s = Srv::start();
    let p = s.port();
    let plain = cli(&["-p", &p], b"SUBSCRIBE c\n", &[("FAKETTY", "1"), ("TERM", "dumb")]);
    assert!(
        plain.stdout.contains(
            "Reading messages... (press Ctrl-C to quit or any key to type command)\r\x1b[K"
        ),
        "{:?}",
        plain.stdout
    );
    // A password line without its newline is still the password: AUTH is sent.
    let unterminated = cli(&["--askpass", "-p", &p, "PING"], b"no-newline", &[]);
    assert!(unterminated.stderr.starts_with("AUTH failed: "), "{}", unterminated.stderr);
}

/// A kevy-cli REPL driven over time: type, wait for output, send signals.
struct Live {
    child: Child,
    stdout: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl Live {
    fn start(args: &[&str], env: &[(&str, &str)]) -> Live {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kevy-cli"));
        cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        cmd.env_remove("FAKETTY").envs(env.iter().copied());
        let mut child = cmd.spawn().expect("run kevy-cli");
        let stdout = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut pipe, sink) = (child.stdout.take().unwrap(), stdout.clone());
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = pipe.read(&mut buf) {
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Live { child, stdout }
    }

    fn type_line(&mut self, line: &str) {
        self.child.stdin.as_mut().unwrap().write_all(line.as_bytes()).unwrap();
    }

    /// Wait until stdout contains `text`; panics with what it holds after 10s.
    fn wait_for(&self, text: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let seen = String::from_utf8_lossy(&self.stdout.lock().unwrap()).into_owned();
            if seen.contains(text) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never printed {text:?}; printed {seen:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn interrupt(&self) {
        let pid = self.child.id().to_string();
        assert!(Command::new("kill").args(["-INT", &pid]).status().unwrap().success());
    }

    /// Close stdin and wait; the exit code.
    fn finish(mut self) -> i32 {
        drop(self.child.stdin.take());
        self.child.wait().unwrap().code().unwrap_or(-1)
    }
}

#[test]
fn ctrl_c_cuts_a_stream_loose_and_ends_anything_else() {
    let s = Srv::start();
    let p = s.port();
    // Subscribed: Ctrl-C drops the subscription for a fresh connection.
    let mut sub = Live::start(&["-p", &p], TTY);
    sub.type_line("SUBSCRIBE c\n");
    sub.wait_for("Reading messages");
    sub.interrupt();
    sub.type_line("PING\n");
    sub.wait_for("PONG");
    assert_eq!(sub.finish(), 0);

    // Monitoring: the same, against a server that streams until cut.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mport = listener.local_addr().unwrap().port().to_string();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 256];
        let (mut first, _) = listener.accept().unwrap();
        let _ = first.read(&mut buf);
        first.write_all(b"+OK\r\n").unwrap();
        let (mut second, _) = listener.accept().unwrap();
        let _ = second.read(&mut buf);
        second.write_all(b"+PONG\r\n").unwrap();
        drop(first);
    });
    let mut monitor = Live::start(&["-p", &mport], &[]);
    monitor.type_line("MONITOR\n");
    monitor.wait_for("OK\n");
    monitor.interrupt();
    monitor.type_line("PING\n");
    monitor.wait_for("PONG\n");
    assert_eq!(monitor.finish(), 0);
    server.join().unwrap();

    // Blocked in a command, or waiting at the prompt: Ctrl-C exits 1.
    let mut blocked = Live::start(&["-p", &p], &[]);
    blocked.type_line("CLIENT SETNAME blocked\n");
    blocked.wait_for("OK");
    blocked.type_line("BLPOP nokey 0\n");
    std::thread::sleep(std::time::Duration::from_millis(300));
    blocked.interrupt();
    assert_eq!(blocked.child.wait().unwrap().code(), Some(1));
    let idle = Live::start(&["-p", &p], &[]);
    std::thread::sleep(std::time::Duration::from_millis(300));
    idle.interrupt();
    assert_eq!(idle.finish(), 1);
}

/// kevy-cli on a pseudo-terminal: stdin, stdout and stderr are the terminal.
struct PtyLive {
    child: Child,
    keys: std::fs::File,
    seen: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl PtyLive {
    fn start(args: &[&str], env: &[(&'static str, String)]) -> PtyLive {
        let (parent, child) = kevy_sys::open_pty().expect("a pty");
        let side = || child.try_clone().unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_kevy-cli"));
        cmd.args(args).stdin(side()).stdout(side()).stderr(side());
        cmd.env_remove("FAKETTY").env_remove("FAKETTY_WITH_PROMPT");
        cmd.envs(env.iter().map(|(k, v)| (*k, v.as_str())));
        let child = cmd.spawn().expect("run kevy-cli on a pty");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut out, sink) = (parent.try_clone().unwrap(), seen.clone());
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            // The parent side reads an error, not end of file, once the child is gone.
            while let Ok(n @ 1..) = out.read(&mut buf) {
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        PtyLive { child, keys: parent, seen }
    }

    fn type_keys(&mut self, keys: &str) {
        self.keys.write_all(keys.as_bytes()).unwrap();
    }

    fn output(&self) -> String {
        String::from_utf8_lossy(&self.seen.lock().unwrap()).into_owned()
    }

    fn wait_count(&self, text: &str, count: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while self.output().matches(text).count() < count {
            assert!(
                std::time::Instant::now() < deadline,
                "never printed {text:?} x{count}: {:?}",
                self.output()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn wait_for(&self, text: &str) {
        self.wait_count(text, 1);
    }

    fn finish(mut self) -> i32 {
        self.child.wait().unwrap().code().unwrap_or(-1)
    }
}

#[test]
fn prompt_forms_history_file_and_preferences() {
    let s = Srv::start();
    let p = s.port();
    let dir = std::env::temp_dir().join(format!("kevy-rcli-prompt-{}", s.port));
    std::fs::create_dir_all(&dir).unwrap();
    let history = dir.join("history");
    let rc = dir.join("rc");
    std::fs::write(&rc, "# a comment\n:set nohints\n:set colours\n\n").unwrap();
    let env = [
        ("FAKETTY_WITH_PROMPT", "1"),
        ("KEVYCLI_HISTFILE", history.to_str().unwrap()),
        ("KEVYCLI_RCFILE", rc.to_str().unwrap()),
    ];
    let typed = b"MULTI\rPING\rEXEC\rAUTH secret\rHELLO 3 AUTH u pw\rSUBSCRIBE c\r\x04";
    let out = cli(&["-p", &p], typed, &env).stdout;
    // The preferences file speaks first, naming itself.
    assert!(out.starts_with(".kevyclirc: unknown kevy-cli internal command '#'\n.kevyclirc: unknown kevy-cli preference 'colours'\n"), "{out:?}");
    for form in [format!("127.0.0.1:{p}(TX)> "), format!("127.0.0.1:{p}(subscribed mode)> ")] {
        assert!(out.contains(&form), "no {form:?} in {out:?}");
    }
    // The file keeps what was typed, minus the lines carrying credentials.
    let kept = std::fs::read_to_string(&history).unwrap();
    assert_eq!(kept, "MULTI\nPING\nEXEC\nSUBSCRIBE c\n");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&history).unwrap().permissions().mode() & 0o777, 0o600);
    // Next session: the history is there to recall.
    let again = cli(&["-p", &p], b"\x10\x10\r\x04", &env).stdout;
    assert!(again.contains("> EXEC\r"), "the second-newest line comes back: {again:?}");
    // No server: the prompt says so. A unix socket prompt names kevy (DEV-011).
    assert!(cli(&["-p", "1"], b"PING\r\x04", &env).stdout.starts_with(".kevyclirc: "));
    let none = [
        ("FAKETTY_WITH_PROMPT", "1"),
        ("KEVYCLI_HISTFILE", history.to_str().unwrap()),
        ("KEVYCLI_RCFILE", "/dev/null"),
    ];
    assert!(cli(&["-p", "1"], b"\x04", &none).stdout.contains("not connected> "));
    let sock = cli(&["-s", "/nonexistent/k.sock"], b"\x04", &none).stdout;
    assert!(sock.contains("not connected> "), "{sock:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_line_typed_ahead_of_a_subscription_runs_while_subscribed() {
    let s = Srv::start();
    let p = s.port();
    // Both lines in one write, and standard input stays open: the second
    // line is already read into kevy-cli's buffer when the subscribed wait
    // starts, so waiting on the descriptor alone would never see it.
    let mut live = Live::start(&["-p", &p], &[]);
    live.type_line("SUBSCRIBE c\nPING\n");
    live.wait_for("subscribe\nc\n1\nPONG\n"); // kevy answers PING plainly when subscribed
    assert_eq!(live.finish(), 0);
}

#[test]
fn help_is_asked_for_once_and_not_on_a_subscribed_connection() {
    let s = Srv::start();
    let p = s.port();
    // Over RESP3 the reply is a map; the help reads the same.
    let resp2 = cli(&["-p", &p, "help", "get"], b"", &[]).stdout;
    assert_eq!(cli(&["-3", "-p", &p, "help", "get"], b"", &[]).stdout, resp2);
    // Asked twice, the second answer comes from the first.
    let twice = cli(&["-p", &p], b"help get\nhelp get\n", &[]).stdout;
    assert_eq!(twice, resp2.repeat(2));
    // Subscribed, the connection cannot be asked: kevy's own reference answers.
    let sub = cli(&["-p", &p], b"SUBSCRIBE c\nhelp get\n", &[]).stdout;
    assert!(sub.ends_with(&resp2), "{sub:?}");

    // The preferences file falls back to ~/.kevyclirc.
    let home = std::env::temp_dir().join(format!("kevy-rcli-home-{}", s.port));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join(".kevyclirc"), ":set bogus\n").unwrap();
    let env = [
        ("FAKETTY_WITH_PROMPT", "1"),
        ("HOME", home.to_str().unwrap()),
        ("KEVYCLI_HISTFILE", "/nonexistent/h"),
    ];
    let out = cli(&["-p", &p], b"\x04", &env).stdout;
    assert!(out.starts_with(".kevyclirc: unknown kevy-cli preference 'bogus'\n"), "{out:?}");
    let valkey = [
        ("FAKETTY_WITH_PROMPT", "1"),
        ("VALKEYCLI_RCFILE", "/dev/null"),
        ("HOME", home.to_str().unwrap()),
        ("KEVYCLI_HISTFILE", "/nonexistent/h"),
    ];
    assert!(
        !cli(&["-p", &p], b"\x04", &valkey).stdout.contains("bogus"),
        "/dev/null reads nothing"
    );
    let _ = std::fs::remove_dir_all(&home);

    // A hint file failing on a line that names no command shows `(null)`.
    let cases = std::env::temp_dir().join(format!("kevy-rcli-nullhint-{}", s.port));
    std::fs::write(&cases, "\"nosuch \" \"x\"\n\"get \" \"key\"\n").unwrap();
    let file = cli(&["-p", &p, "--test_hint_file", cases.to_str().unwrap()], b"", &[]);
    assert_eq!(file.stderr, "Test case 'nosuch ' FAILED: expected 'x', got '(null)'\n");
    assert_eq!((file.stdout.as_str(), file.code), ("FAILURE: 1/2 passed\n", 1));
    let _ = std::fs::remove_file(&cases);
}

#[test]
fn get_pubsub_reports_the_subscription() {
    let s = Srv::start();
    let p = s.port();
    let out = cli(
        &["-p", &p],
        b":get pubsub\n:get colour\n:set hints\n:unset\nSUBSCRIBE c\n:get pubsub\n",
        &[],
    );
    assert_eq!(
        out.stdout,
        "0\nunknown kevy-cli get option 'colour'\nunknown kevy-cli internal command ':unset'\nsubscribe\nc\n1\n1\n"
    );
}

#[test]
fn scan_lists_keys_and_reports_what_it_cannot_read() {
    let s = Srv::start();
    let p = s.port();
    cli(&["-p", &p, "MSET", "a", "1", "sp ace", "2", "k:1", "3"], b"", &[]);
    let sorted = |out: String| {
        let mut lines: Vec<String> = out.lines().map(str::to_string).collect();
        lines.sort();
        lines
    };
    assert_eq!(sorted(cli(&["-p", &p, "--scan"], b"", &[]).stdout), ["a", "k:1", "sp ace"]);
    assert_eq!(
        sorted(cli(&["-p", &p, "--scan", "--count", "1"], b"", TTY).stdout),
        ["\"a\"", "\"k:1\"", "\"sp ace\""]
    );
    assert_eq!(
        cli(&["-p", &p, "--scan", "--pattern", "k:*", "-i", "0.001"], b"", &[]).stdout,
        "k:1\n"
    );
    // What SCAN answered, when it is not a page of keys.
    for (reply, message) in [
        (&b"-ERR no\r\n"[..], "SCAN error: ERR no\n"),
        (b":1\r\n", "Non ARRAY response from SCAN!\n"),
        (b"*1\r\n$1\r\n0\r\n", "Invalid element count from SCAN!\n"),
        (b"*2\r\n:0\r\n*0\r\n", "Non ARRAY response from SCAN!\n"),
        (b"*2\r\n$1\r\n0\r\n*1\r\n:5\r\n", "Non ARRAY response from SCAN!\n"),
        (b"", "\nI/O error\n"),
    ] {
        let out = canned(reply, &["--scan"], b"");
        assert_eq!((out.stderr.as_str(), out.code), (message, 1), "{reply:?}");
    }
    assert_eq!(cli(&["-p", "1", "--scan"], b"", &[]).code, 1);
}

#[test]
fn ctrl_c_stops_a_scan_cleanly() {
    // A server whose SCAN never finishes: every page points at the next.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = [0u8; 256];
        while matches!(conn.read(&mut buf), Ok(n) if n > 0) {
            if conn.write_all(b"*2\r\n$1\r\n7\r\n*1\r\n$3\r\nkey\r\n").is_err() {
                break;
            }
        }
    });
    let scan = Live::start(&["-p", &port, "--scan", "-i", "0.01"], &[]);
    scan.wait_for("key\nkey\n");
    scan.interrupt();
    assert_eq!(scan.finish(), 0, "a stopped scan is a finished scan");
    server.join().unwrap();
}

/// A server that answers each read with the next canned bytes, on one
/// connection. A pipeline arrives as one write, so its replies go together.
fn scripted_server(script: Vec<&'static [u8]>) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        for reply in script {
            if !matches!(conn.read(&mut buf), Ok(n) if n > 0) || conn.write_all(reply).is_err() {
                return;
            }
        }
    });
    (port, server)
}

#[test]
fn bigkeys_and_memkeys_report_and_explain_what_stopped_them() {
    let s = Srv::start();
    let p = s.port();
    cli(&["-p", &p, "MSET", "s1", "x", "s2", "xxxxx"], b"", &[]);
    cli(&["-p", &p, "RPUSH", "l", "a", "b"], b"", &[]);
    let big = cli(&["-p", &p, "--bigkeys"], b"", &[]).stdout;
    assert!(
        big.contains("[00.00%] Biggest string found so far \"s2\" with 5 bytes\n")
            || big.contains("found so far \"s1\" with 1 bytes\n"),
        "{big}"
    );
    assert!(
        big.contains(
            "Sampled 3 keys in the keyspace!\nTotal key length in bytes is 5 (avg len 1.67)\n"
        ),
        "{big}"
    );
    assert!(big.ends_with("0 streams with 0 entries (00.00% of keys, avg size 0.00)\n"), "{big}");
    let mem = cli(&["-p", &p, "--memkeys", "--memkeys-samples", "2"], b"", TTY).stdout;
    assert!(
        mem.contains("Keys sampled: 3\n")
            && mem.contains("strings with ")
            && mem.contains(" bytes ("),
        "{mem}"
    );
    assert!(!mem.contains("Sampled 3 keys"), "a terminal already saw the count");

    let fatal = |script: Vec<&'static [u8]>, message: &str| {
        let (port, server) = scripted_server(script);
        let out = cli(&["-p", &port, "--bigkeys"], b"", &[]);
        assert_eq!((out.stderr.as_str(), out.code), (message, 1));
        server.join().unwrap();
    };
    fatal(vec![b"-ERR no\r\n"], "Couldn't determine DBSIZE: ERR no\n");
    fatal(vec![b"+OK\r\n"], "Non INTEGER response from DBSIZE!\n");
    fatal(vec![b":1\r\n", b"-ERR busy\r\n"], "Error: ERR busy\n");
    fatal(
        vec![b":1\r\n", b"+OK\r\n", b"*2\r\n$1\r\n0\r\n*1\r\n$1\r\nk\r\n", b"-ERR type\r\n"],
        "TYPE returned an error: ERR type\n",
    );
    fatal(vec![b":1\r\n", b"+OK\r\n", b"*2\r\n$1\r\n0\r\n*1\r\n$1\r\nk\r\n"], "\nI/O error\n");
    fatal(
        vec![b":1\r\n", b"+OK\r\n", b"*2\r\n$1\r\n0\r\n*1\r\n$1\r\nk\r\n", b"+string\r\n"],
        "\nI/O error\n",
    );
    fatal(vec![], "\nI/O error\n");

    // A size that fails is a warning, and the key counts with size 0.
    let (port, server) = scripted_server(vec![
        b":2\r\n",
        b"-ERR unknown command 'READONLY'\r\n",
        b"*2\r\n$1\r\n0\r\n*3\r\n$1\r\nk\r\n$4\r\ngone\r\n$1\r\nm\r\n",
        b"+string\r\n+none\r\n+vectorset\r\n",
        b"-WRONGTYPE\r\n",
    ]);
    let out = cli(&["-p", &port, "--bigkeys", "-i", "0.001"], b"", &[]);
    assert_eq!(out.stderr, "Warning:  STRLEN on 'k' failed (may have changed type)\n");
    assert!(
        out.stdout.contains("Sampled 2 keys in the keyspace!\n")
            && out.stdout.contains("1 vectorsets with 0 ? (50.00% of keys"),
        "{}",
        out.stdout
    );
    server.join().unwrap();
}

#[test]
fn ctrl_c_ends_bigkeys_with_the_summary_so_far() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let mut n = 0;
        while let Ok(got @ 1..) = conn.read(&mut buf) {
            let asked = &buf[..got];
            let reply: &[u8] = if asked.windows(6).any(|w| w == b"DBSIZE") {
                b":1000\r\n"
            } else if asked.windows(4).any(|w| w == b"SCAN") {
                b"*2\r\n$1\r\n7\r\n*1\r\n$1\r\nk\r\n"
            } else if asked.windows(4).any(|w| w == b"TYPE") {
                b"+list\r\n"
            } else if asked.windows(4).any(|w| w == b"LLEN") {
                n += 1;
                if n == 1 { b":5\r\n" } else { b":1\r\n" }
            } else {
                b"+OK\r\n"
            };
            if conn.write_all(reply).is_err() {
                break;
            }
        }
    });
    let walk = Live::start(&["-p", &port, "--bigkeys"], &[]);
    walk.wait_for("Biggest list   found so far \"k\" with 5 items\n");
    walk.interrupt();
    walk.wait_for(" keys in the keyspace!\n");
    let seen = String::from_utf8_lossy(&walk.stdout.lock().unwrap()).into_owned();
    assert!(
        seen.contains("\n-------- summary -------\n\n["),
        "stopped early, it says how far: {seen}"
    );
    assert_eq!(walk.finish(), 0);
    server.join().unwrap();
}

#[test]
fn hotkeys_keeps_the_hottest_and_stops_on_an_error() {
    let page: &'static [u8] = b"*2\r\n$1\r\n0\r\n*3\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n";
    let (port, server) =
        scripted_server(vec![b":3\r\n", b"+OK\r\n", page, b":6\r\n+nope\r\n:5\r\n"]);
    let out = cli(&["-p", &port, "--hotkeys", "-i", "0.001"], b"", &[]);
    assert_eq!(out.stderr, "Warning: OBJECT freq on '\"b\"' failed (may have been deleted)\n");
    assert!(
        out.stdout.ends_with("Sampled 3 keys in the keyspace!\nhot key found with counter: 6\tkeyname: \"a\"\nhot key found with counter: 5\tkeyname: \"c\"\n"),
        "{}",
        out.stdout
    );
    server.join().unwrap();

    let (port, server) =
        scripted_server(vec![b":3\r\n", b"+OK\r\n", page, b"-ERR no LFU\r\n:1\r\n:1\r\n"]);
    let out = cli(&["-p", &port, "--hotkeys"], b"", TTY);
    assert_eq!((out.stderr.as_str(), out.code), ("Error: ERR no LFU\n", 1));
    server.join().unwrap();

    let (port, server) = scripted_server(vec![b":3\r\n", b"+OK\r\n", page, b":7\r\n:1\r\n:2\r\n"]);
    let out = cli(&["-p", &port, "--hotkeys", "--hotkeys-count", "2"], b"", TTY);
    assert!(
        out.stdout.contains("Keys sampled: 3\n")
            && out.stdout.ends_with(
                "counter: 7\tkeyname: \"a\"\nhot key found with counter: 2\tkeyname: \"c\"\n"
            ),
        "{:?}",
        out.stdout
    );
    server.join().unwrap();
}

#[test]
fn keystats_reports_and_says_where_to_resume() {
    let s = Srv::start();
    let p = s.port();
    cli(&["-p", &p, "MSET", "s1", "x", "s2", "xxxxxxxxxxxxxxxxxxxxxxxxxxxx"], b"", &[]);
    cli(&["-p", &p, "RPUSH", "l1", "a", "b", "c"], b"", &[]);
    let out = cli(&["-p", &p, "--keystats", "--top", "2"], b"", &[]).stdout;
    for part in [
        "100.00% keys scanned\nKeys sampled: 3\n",
        "--- Top 2 key sizes ---\n  1 ",
        "Key size Percentile Total keys\n",
        "Total key length is 6B (2B avg)\n",
        "string               2  66.67% ",
    ] {
        assert!(out.contains(part), "no {part:?} in {out}");
    }
    let screen = cli(&["-p", &p, "--keystats-samples", "2", "--keystats"], b"", TTY).stdout;
    assert!(screen.contains("\x1b[2K\rKeys sampled: 3\n"), "{screen:?}");
    let empty = cli(&["-p", &p, "--keystats", "--pattern", "nothing*"], b"", &[]).stdout;
    assert!(
        empty.contains("No key size samples collected\n") && empty.contains("(0 avg)"),
        "{empty}"
    );

    // A walk that never ends, stopped: the report says which cursor to resume at.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        while let Ok(got @ 1..) = conn.read(&mut buf) {
            let asked = &buf[..got];
            let has = |w: &[u8]| asked.windows(w.len()).any(|x| x == w);
            let reply: &[u8] = if has(b"DBSIZE") {
                b":1000\r\n"
            } else if has(b"SCAN") {
                b"*2\r\n$2\r\n42\r\n*1\r\n$1\r\nk\r\n"
            } else if has(b"TYPE") {
                b"+hash\r\n"
            } else if has(b"MEMORY") {
                b":64\r\n:2\r\n"
            } else {
                b"+OK\r\n"
            };
            if conn.write_all(reply).is_err() {
                break;
            }
        }
    });
    let walk = Live::start(&["-p", &port, "--keystats", "--cursor", "9"], &[]);
    std::thread::sleep(std::time::Duration::from_millis(200));
    walk.interrupt();
    walk.wait_for("to restart from the last cursor.\n");
    let seen = String::from_utf8_lossy(&walk.stdout.lock().unwrap()).into_owned();
    assert!(seen.contains("\nScan interrupted:\nUse 'kevy-cli --keystats --cursor 42' to restart from the last cursor.\n"), "{seen}");
    assert_eq!(walk.finish(), 0);
    server.join().unwrap();
}

#[test]
fn stat_prints_rows_reconnects_and_reports_refusals() {
    let s = Srv::start();
    let p = s.port();
    cli(&["-p", &p, "MSET", "a", "1", "b", "2"], b"", &[]);
    let stat = Live::start(&["-p", &p, "--stat", "-i", "0.05"], &[]);
    stat.wait_for("keys       mem      clients blocked requests            connections          \n2          ");
    stat.wait_for(" (+1)");
    stat.interrupt();
    let _ = stat.finish();

    let body = "used_memory:100\r\ntotal_commands_processed:7\r\ndb0:keys=3,x=1\r\n";
    let info: &'static [u8] = format!("${}\r\n{body}\r\n", body.len()).into_bytes().leak();
    // CONFIG GET refused, INFO refused.
    let (port, server) = scripted_server(vec![b"-ERR unknown subcommand\r\n", b"-ERR no info\r\n"]);
    let out = cli(&["-p", &port, "--stat"], b"", &[]);
    assert_eq!(
        out.stderr,
        "CONFIG GET databases fails: ERR unknown subcommand, use default value 16 instead\nERROR: ERR no info\n"
    );
    assert_eq!(out.code, 1);
    server.join().unwrap();

    // The first connection answers once and drops; the second is found again.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        let (mut first, _) = listener.accept().unwrap();
        let _ = first.read(&mut buf);
        first.write_all(b"*2\r\n$9\r\ndatabases\r\n$1\r\n1\r\n").unwrap();
        let _ = first.read(&mut buf);
        first.write_all(info).unwrap();
        let _ = first.read(&mut buf);
        drop(first);
        let (mut second, _) = listener.accept().unwrap();
        while let Ok(1..) = second.read(&mut buf) {
            if second.write_all(info).is_err() {
                break;
            }
        }
    });
    let stat = Live::start(&["-p", &port, "--stat", "-i", "0.01"], &[]);
    stat.wait_for("3          100B     0       0       7 (+0)              0           \n");
    stat.wait_for("\r\x1b[0KReconnecting... 1\r");
    stat.wait_for("\r\x1b[0K3          100B");
    stat.interrupt();
    let _ = stat.finish();
    server.join().unwrap();
    // A reply that is not RESP cannot be reconnected away.
    let (port, server) = scripted_server(vec![b"*0\r\n", b"@@@\r\n"]);
    let out = cli(&["-p", &port, "--stat"], b"", &[]);
    assert_eq!(
        (out.stderr.as_str(), out.code),
        ("Error: Protocol error, got \"@\" as reply type byte\n", 1)
    );
    server.join().unwrap();
}

#[test]
fn latency_modes_report_in_every_output_format() {
    let s = Srv::start();
    let p = s.port();
    let shape = |args: &[&str]| -> String {
        let mut full = vec!["-p", p.as_str(), "--latency", "-i", "0.05"];
        full.extend_from_slice(args);
        let out = cli(&full, b"", &[]);
        assert_eq!(out.code, 0, "{args:?}");
        out.stdout
            .chars()
            .map(|c| if c.is_ascii_digit() { '#' } else { c })
            .collect::<String>()
            .replace("#.###", "N")
            .replace("##", "#")
    };
    assert!(shape(&["--raw"]).starts_with("N N N #"), "{}", shape(&["--raw"]));
    assert!(shape(&["--csv", "--latency-percentiles", "50"]).starts_with("N,N,N,#"));
    let json = shape(&["--json", "--latency-percentiles", "50,99.9"]);
    assert!(
        json.starts_with("{\"min\": N, \"max\": N, \"avg\": N, \"count\": #")
            && json.contains("\"percentiles\": {\"#\": N, \"#.#\": N}}"),
        "{json}"
    );
    assert_eq!(shape(&["--quoted-json"]), "");

    let history = Live::start(&["-p", &p, "--latency-history", "-i", "0.1", "--raw"], &[]);
    history.wait_for(" seconds range\n");
    history.interrupt();
    let _ = history.finish();
    let terminal = Live::start(&["-p", &p, "--latency", "--latency-percentiles", "90"], TTY);
    terminal.wait_for(" samples), p90: ");
    terminal.interrupt();
    let _ = terminal.finish();
    for mono in [false, true] {
        let mut args = vec!["-p", p.as_str(), "--latency-dist", "-i", "0.1"];
        if mono {
            args.push("--mono");
        }
        let dist = Live::start(&args, &[]);
        dist.wait_for("From 0 to 100%: ");
        dist.wait_for("\x1b[0m\n\x1b[38;5;0m");
        dist.interrupt();
        let _ = dist.finish();
    }
}

#[test]
fn load_and_local_measurement_modes() {
    let s = Srv::start();
    let p = s.port();
    let lru = Live::start(&["-p", &p, "--lru-test", "100"], &[]);
    lru.wait_for(" Gets/sec | Hits: ");
    lru.interrupt();
    let _ = lru.finish();

    let local = cli(&["--intrinsic-latency", "0"], b"", &[]);
    assert!(
        local.stdout.starts_with("Max latency so far: ")
            && local.stdout.contains(" total runs (avg latency: "),
        "{}",
        local.stdout
    );
    let stopped = Live::start(&["--intrinsic-latency", "100"], &[]);
    stopped.wait_for("Max latency so far: ");
    stopped.interrupt();
    stopped.wait_for("longer than the average latency.\n");
    assert_eq!(stopped.finish(), 0);

    // kevy has no vector sets: VDIM is refused.
    let refused = cli(&["-p", &p, "--vset-recall", "v"], b"", &[]);
    assert_eq!(
        (refused.stderr.as_str(), refused.code),
        ("Error: Cannot get dimension for key v\n", 1)
    );
}

#[test]
fn vset_recall_compares_approximate_with_exact_search() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let mut answered_exact = false;
        while let Ok(got @ 1..) = conn.read(&mut buf) {
            let asked = &buf[..got];
            let has = |w: &[u8]| asked.windows(w.len()).any(|x| x == w);
            let reply: &[u8] = if has(b"VDIM") {
                b":2\r\n"
            } else if has(b"VRANDMEMBER") {
                b"*2\r\n$1\r\na\r\n$1\r\nb\r\n"
            } else if has(b"VEMB") {
                b"*2\r\n$1\r\n1\r\n$3\r\n2.5\r\n*2\r\n,3\r\n:4\r\n"
            } else if has(b"TRUTH") {
                answered_exact = true;
                // approximate finds a and c; exact is a and b: recall 50%
                b"*2\r\n$1\r\na\r\n$1\r\nc\r\n*2\r\n$1\r\na\r\n$1\r\nb\r\n"
            } else {
                b"+OK\r\n"
            };
            if conn.write_all(reply).is_err() {
                break;
            }
        }
        assert!(answered_exact);
    });
    let recall = Live::start(&["-p", &port, "--vset-recall", "vs", "-i", "0.01"], &[]);
    recall.wait_for("# Mixing 1 random element vectors, top 100 results, EF=500\n\nQueries: 1 | Avg recall: 50.00%\n");
    recall.interrupt();
    recall.wait_for("  50.0%          99.90%\n  60.0%           0.00%\n");
    assert_eq!(recall.finish(), 0);
    server.join().unwrap();
}

#[test]
fn eval_runs_a_script_file() {
    let s = Srv::start();
    let p = s.port();
    let dir = std::env::temp_dir().join(format!("kevy-rcli-eval-{}", s.port));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("s.lua");
    std::fs::write(&file, "return {KEYS[1], ARGV[1], #KEYS, #ARGV}").unwrap();
    let path = file.to_str().unwrap();
    assert_eq!(
        cli(&["-p", &p, "--eval", path, "k1", "k2", ",", "a1"], b"", &[]).stdout,
        "k1\na1\n2\n1\n"
    );
    // ARGV[1] is nil, which ends the Lua table after KEYS[1].
    assert_eq!(cli(&["-p", &p, "-r", "2", "--eval", path, "k", ","], b"", &[]).stdout, "k\nk\n");
    let missing = cli(&["-p", &p, "--eval", "/nonexistent/s.lua"], b"", &[]);
    assert_eq!(
        (missing.stderr.as_str(), missing.code),
        ("Can't open file '/nonexistent/s.lua': No such file or directory\n", 1)
    );
    assert_eq!(cli(&["-p", &p, "--eval", path, "--ldb"], b"", &[]).code, 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn pipe_sends_input_as_it_is_and_counts_replies() {
    let s = Srv::start();
    let p = s.port();
    let out =
        cli(&["-p", &p, "--pipe"], b"SET b 2\r\n*2\r\n$4\r\nINCR\r\n$1\r\nb\r\nNOSUCH\r\n", &[]);
    assert_eq!(
        out.stdout,
        "All data transferred. Waiting for the last reply...\nLast reply received from server.\nerrors: 1, replies: 3\n"
    );
    assert!(out.stderr.starts_with("ERR unknown command"), "{}", out.stderr);
    assert_eq!(out.code, 1);
    assert_eq!(cli(&["-p", &p, "GET", "b"], b"", &[]).stdout, "3\n");

    // A server that never answers: the timeout ends the wait.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = [0u8; 256];
        while let Ok(1..) = conn.read(&mut buf) {}
    });
    let silent = cli(&["-p", &port, "--pipe", "--pipe-timeout", "1"], b"PING\r\n", &[]);
    assert_eq!((silent.stderr.as_str(), silent.code), ("No replies for 1 seconds: exiting.\n", 1));
    assert!(silent.stdout.ends_with("errors: 1, replies: 0\n"), "{}", silent.stdout);
    server.join().unwrap();
    // A server that hangs up.
    let (port, server) = fake_server(b"", None);
    let gone = cli(&["-p", &port.to_string(), "--pipe"], b"PING\r\n", &[]);
    assert_eq!((gone.stderr.as_str(), gone.code), ("Error reading replies from server\n", 1));
    server.join().unwrap();
}

/// A master that answers REPLCONF with +OK and SYNC with `sync`, sent in
/// pieces a byte apart so marks and headers straddle reads, then `after`.
fn fake_master(
    sync: Vec<u8>,
    after: &'static [u8],
    refuse_filter: bool,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        let (mut conn, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        while let Ok(n @ 1..) = conn.read(&mut buf) {
            let asked = buf[..n].to_vec();
            let has = |w: &[u8]| asked.windows(w.len()).any(|x| x == w);
            if has(b"SYNC") {
                for piece in sync.chunks(7) {
                    if conn.write_all(piece).is_err() {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                let _ = conn.write_all(after);
                // Take the replica's ACK (or the client's end) before closing,
                // so the close is not a reset over unread bytes.
                let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                let _ = conn.read(&mut buf);
                return;
            }
            let reply: &[u8] =
                if refuse_filter && has(b"functions") { b"-ERR no filter\r\n" } else { b"+OK\r\n" };
            if conn.write_all(reply).is_err() {
                return;
            }
        }
    });
    (port, server)
}

#[test]
fn rdb_and_replica_modes_read_a_snapshot_stream() {
    let mark = "0123456789012345678901234567890123456789";
    let dir = std::env::temp_dir().join(format!("kevy-rcli-rdb-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("dump.rdb");
    std::fs::write(&file, vec![b'x'; 5000]).unwrap();
    let path = file.to_str().unwrap();

    let body = "REDIS0011snapshot-bytes";
    let (port, server) =
        fake_master(format!("\n\n$EOF:{mark}\r\n{body}{mark}").into_bytes(), b"", false);
    let out = cli(&["-p", &port, "--rdb", path], b"", &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(out.stderr.ends_with(&format!("SYNC sent to master, writing bytes of bulk transfer until EOF marker to '{path}'\nTransfer finished with success after 23 bytes\n")), "{}", out.stderr);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        body.as_bytes(),
        "an older, longer file is cut to the snapshot"
    );
    server.join().unwrap();

    let (port, server) = fake_master(b"$5\r\nhello".to_vec(), b"", false);
    let out = cli(&["-p", &port, "--functions-rdb", "-"], b"", &[]);
    assert_eq!((out.stdout.as_str(), out.code), ("hello", 0));
    assert!(out.stderr.contains("sending REPLCONF rdb-filter-only functions\nSYNC sent to master, writing 5 bytes to '-'\nTransfer finished with success.\n"), "{}", out.stderr);
    server.join().unwrap();

    let (port, server) = fake_master(Vec::new(), b"", true);
    let out = cli(&["-p", &port, "--functions-rdb", path], b"", &[]);
    assert!(out.stderr.ends_with("REPLCONF rdb-filter-only error: ERR no filter\nFailed requesting functions only RDB from server, aborting\n"), "{}", out.stderr);
    server.join().unwrap();

    let (port, server) = fake_master(b"-ERR busy\r\n".to_vec(), b"", false);
    let out = cli(&["-p", &port, "--rdb", "/nonexistent/dir/x.rdb"], b"", &[]);
    assert!(out.stderr.ends_with("SYNC with master failed: -ERR busy\r\n"), "{:?}", out.stderr);
    server.join().unwrap();
    let (port, server) = fake_master(format!("$EOF:{mark}\r\n{mark}").into_bytes(), b"", false);
    let out = cli(&["-p", &port, "--rdb", "/nonexistent/dir/x.rdb"], b"", &[]);
    assert!(
        out.stderr.ends_with("Error opening '/nonexistent/dir/x.rdb': No such file or directory\n"),
        "{}",
        out.stderr
    );
    server.join().unwrap();

    // A replica: the snapshot is discarded, then every frame prints as CSV.
    let (port, server) = fake_master(
        format!("$EOF:{mark}\r\nrdb{mark}*2\r\n$6\r\nSELECT\r\n$1\r\n3\r\n").into_bytes(),
        b"*3\r\n$3\r\nSET\r\n$1\r\nb\r\n$1\r\n2\r\n",
        false,
    );
    let out = cli(&["-p", &port, "--replica"], b"", &[]);
    assert_eq!((out.stdout.as_str(), out.code), ("\"SELECT\",\"3\"\n\"SET\",\"b\",\"2\"\n", 1));
    assert!(out.stderr.contains("Full resync done after 3 bytes. Logging commands from master.\nsending REPLCONF ACK 0\nError: Server closed the connection\n"), "{}", out.stderr);
    server.join().unwrap();
    let (port, server) = fake_master(b"$3\r\nrdb".to_vec(), b"", false);
    let out = cli(&["-p", &port, "--replica"], b"", &[]);
    assert!(out.stderr.contains("discarding 3 bytes of bulk transfer...\nFull resync done. Logging commands from master.\n"), "{}", out.stderr);
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
