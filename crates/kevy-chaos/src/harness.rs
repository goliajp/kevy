//! Spawn + kill + restart a kevy child process. Public API is `Harness`.

// `write!` into an in-memory buffer returns a `Result` because
// `fmt::Write` must, not because it can fail — `String`'s and `Vec`'s
// impls are infallible. Said once here rather than beside every line.
#![expect(clippy::let_underscore_must_use, reason = "writing to an in-memory buffer cannot fail")]

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{HarnessConfig, KillSignal};

/// Active kevy child + ready port.
///
/// ```
/// # use std::io::{Read, Write};
/// # use std::os::unix::fs::PermissionsExt as _;
/// # let port = kevy_chaos::pick_free_port();
/// # let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
/// # std::thread::spawn(move || {
/// #     for mut s in listener.incoming().flatten() {
/// #         let mut b = [0u8; 64];
/// #         while matches!(s.read(&mut b), Ok(n) if n > 0) {
/// #             if s.write_all(b"+PONG\r\n").is_err() { break; }
/// #         }
/// #     }
/// # });
/// # let dir = std::env::temp_dir().join(format!("kevy-chaos-doc-{}-{port}", std::process::id()));
/// # std::fs::create_dir_all(&dir)?;
/// # let bin = dir.join("kevy.sh");
/// # std::fs::write(&bin, "#!/bin/sh\nexec sleep 60\n")?;
/// # std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
/// // `bin` stands in for the kevy binary; the PING it answers comes from a
/// // listener in this process
///
/// use kevy_chaos::{Harness, HarnessConfig, KillSignal};
///
/// let cfg = HarnessConfig { kevy_bin: bin, ..HarnessConfig::new(dir.join("data"), port) };
/// let mut h = Harness::spawn(cfg)?;
/// assert_eq!(h.port(), port);
/// // crash, then recover on the same data dir
/// h.kill(KillSignal::Sigkill)?;
/// h.restart()?;
/// h.kill(KillSignal::Sigterm)?;
///
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct Harness {
    /// The settings this child was started from. Kept rather than consumed
    /// so a test can read back the port and paths it was given — including
    /// the ones the config chose for itself.
    ///
    /// ```
    /// # use std::io::{Read, Write};
    /// # use std::os::unix::fs::PermissionsExt as _;
    /// # let port = kevy_chaos::pick_free_port();
    /// # let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
    /// # std::thread::spawn(move || {
    /// #     for mut s in listener.incoming().flatten() {
    /// #         let mut b = [0u8; 64];
    /// #         while matches!(s.read(&mut b), Ok(n) if n > 0) {
    /// #             if s.write_all(b"+PONG\r\n").is_err() { break; }
    /// #         }
    /// #     }
    /// # });
    /// # let dir = std::env::temp_dir().join(format!("kevy-chaos-doc-{}-{port}", std::process::id()));
    /// # std::fs::create_dir_all(&dir)?;
    /// # let bin = dir.join("kevy.sh");
    /// # std::fs::write(&bin, "#!/bin/sh\nexec sleep 60\n")?;
    /// # std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
    /// // `bin` stands in for the kevy binary; the PING it answers comes from a
    /// // listener in this process
    ///
    /// use kevy_chaos::{Harness, HarnessConfig};
    ///
    /// let cfg = HarnessConfig { kevy_bin: bin, ..HarnessConfig::new(dir.join("data"), port) };
    /// let h = Harness::spawn(cfg)?;
    /// assert_eq!(h.config.port, port);
    /// // the child's config and stderr log live in the data dir it was given
    /// assert!(h.config.data_dir.join("kevy.toml").exists());
    /// assert!(h.config.data_dir.join("kevy.stderr.log").exists());
    /// # drop(h);
    ///
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub config: HarnessConfig,
    child: Option<Child>,
}

impl Harness {
    /// Spawn kevy as a child, wait until it accepts a TCP PING (or timeout).
    pub fn spawn(config: HarnessConfig) -> io::Result<Self> {
        let mut h = Self { config, child: None };
        h.start_child()?;
        Ok(h)
    }

    fn start_child(&mut self) -> io::Result<()> {
        std::fs::create_dir_all(&self.config.data_dir)?;
        // Build the kevy command line. `appendfsync` is set via env var
        // until kevy CLI supports a flag (the existing CLI accepts
        // `--no-aof` but not `--appendfsync`; the env-var path is the
        // documented override per kevy-config).
        let cfg_path = self.config.data_dir.join("kevy.toml");
        std::fs::write(&cfg_path, self.build_child_toml())?;
        // Route kevy's stderr to a file under the data dir so test
        // diagnostics (AOF replay summary, etc.) survive the test.
        let stderr_path = self.config.data_dir.join("kevy.stderr.log");
        let stderr_file =
            std::fs::OpenOptions::new().create(true).append(true).open(&stderr_path)?;
        let mut cmd = Command::new(&self.config.kevy_bin);
        cmd.arg("--config")
            .arg(&cfg_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file));
        // Apply RLIMIT_NOFILE / RLIMIT_FSIZE on Unix via
        // pre_exec. Run BEFORE exec so kevy starts with the cap.
        #[cfg(unix)]
        {
            let nofile = self.config.rlimit_nofile;
            let fsize = self.config.rlimit_fsize;
            use std::os::unix::process::CommandExt as _;
            // SAFETY: pre_exec runs in the forked child between fork and
            // exec; only async-signal-safe + simple syscalls. We do
            // setrlimit(2) calls — safe + signal-safe. No allocator.
            unsafe {
                cmd.pre_exec(move || apply_rlimits(nofile, fsize));
            }
        }
        let child = cmd.spawn()?;
        self.child = Some(child);
        self.wait_ready()
    }

    /// Render the spawned kevy's `kevy.toml` from this config.
    fn build_child_toml(&self) -> String {
        let mut toml = format!(
            "[server]\nport = {}\nthreads = {}\ndata_dir = \"{}\"\n",
            self.config.port,
            self.config.threads,
            self.config.data_dir.display(),
        );
        if self.config.max_clients > 0 {
            use std::fmt::Write as _;
            let _ = writeln!(toml, "max_clients = {}", self.config.max_clients);
        }
        let _ = std::fmt::Write::write_fmt(
            &mut toml,
            format_args!("[persistence]\nappendfsync = \"{}\"\n", self.config.appendfsync,),
        );
        if let Some(sz) = self.config.aof_rewrite_min_size {
            use std::fmt::Write as _;
            let _ = writeln!(toml, "auto_aof_rewrite_min_size = \"{sz}\"");
        }
        if let Some(pct) = self.config.aof_rewrite_pct {
            use std::fmt::Write as _;
            let _ = writeln!(toml, "auto_aof_rewrite_percentage = {pct}");
        }
        if !self.config.extra_toml.is_empty() {
            toml.push('\n');
            toml.push_str(&self.config.extra_toml);
            if !self.config.extra_toml.ends_with('\n') {
                toml.push('\n');
            }
        }
        toml
    }

    /// Wait until the child answers PING.
    ///
    /// A child that died and a child that is slow are not the same failure,
    /// and a probe that only connects reports them as one. Two primaries
    /// timed out here in a parallel `cargo test --workspace` and left an
    /// empty stderr log and a data dir holding nothing, so "kevy ready
    /// timeout" was the whole of what ten seconds could be asked about. Each
    /// round now asks the child whether it is still running, and an exit is
    /// reported as an exit.
    fn wait_ready(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + self.config.spawn_timeout;
        let addr = (format!("127.0.0.1:{}", self.config.port).as_str())
            .to_socket_addrs()?
            .next()
            .expect("addr resolves");
        loop {
            if self.answers_ping(&addr) {
                return Ok(());
            }
            match self.child.as_mut().map(Child::try_wait) {
                Some(Ok(Some(status))) => {
                    return Err(io::Error::other(format!(
                        "kevy exited with {status} before it listened on {}: {}",
                        self.config.port,
                        self.stderr_tail()
                    )));
                }
                // Not "still starting": we asked and were refused, and a probe
                // that cannot ask is not a probe that got a no.
                Some(Err(e)) => {
                    return Err(io::Error::other(format!(
                        "cannot tell whether kevy is still running: {e}"
                    )));
                }
                Some(Ok(None)) | None => {}
            }
            if Instant::now() > deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "kevy ready timeout: still running after {:?}, never answered PING \
                         on {}: {}",
                        self.config.spawn_timeout,
                        self.config.port,
                        self.stderr_tail()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// One PING round trip; false for any failure, which is a reason to keep
    /// waiting rather than a diagnosis.
    fn answers_ping(&self, addr: &std::net::SocketAddr) -> bool {
        use std::io::{Read, Write};
        let Ok(mut s) = TcpStream::connect_timeout(addr, Duration::from_millis(200)) else {
            return false;
        };
        let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
        if s.write_all(b"*1\r\n$4\r\nPING\r\n").is_err() {
            return false;
        }
        let mut buf = [0u8; 16];
        matches!(s.read(&mut buf), Ok(n) if n > 0 && buf.starts_with(b"+PONG"))
    }

    /// The last of what the child wrote to stderr, or that it wrote nothing
    /// — which is itself worth reading, and was the case both times.
    fn stderr_tail(&self) -> String {
        let path = self.config.data_dir.join("kevy.stderr.log");
        match std::fs::read_to_string(&path) {
            Ok(text) if !text.trim().is_empty() => {
                let tail: Vec<&str> = text.trim().lines().rev().take(3).collect();
                format!("stderr: {}", tail.into_iter().rev().collect::<Vec<_>>().join(" | "))
            }
            Ok(_) => "stderr empty".into(),
            Err(e) => format!("stderr unreadable: {e}"),
        }
    }

    /// Kill the kevy child with the given signal and reap it. Idempotent
    /// after the first call.
    pub fn kill(&mut self, sig: KillSignal) -> io::Result<()> {
        let Some(mut child) = self.child.take() else { return Ok(()) };
        match sig {
            KillSignal::Sigkill => {
                // SIGKILL is what `Child::kill` sends on Unix.
                child.kill()?;
            }
            KillSignal::Sigterm => {
                // Send SIGTERM via libc::kill. We can't depend on libc
                // directly per project rule; use std::process raw_fd
                // approach via std-only is not portable. Instead use
                // /proc/<pid>/something? Simplest: spawn `kill -TERM <pid>`.
                let pid = child.id();
                let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
            }
        }
        let _ = child.wait();
        Ok(())
    }

    /// Wait for kevy to exit on its own (e.g. after a `SHUTDOWN`
    /// command) up to `timeout`. Returns `Some(exit_code)` once the
    /// process is gone, `None` on timeout (the child stays owned so a
    /// follow-up `kill` can reap it). A signal-terminated child
    /// reports code `-1`.
    pub fn wait_exit(&mut self, timeout: std::time::Duration) -> io::Result<Option<i32>> {
        let Some(child) = self.child.as_mut() else { return Ok(Some(0)) };
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait()? {
                self.child = None;
                return Ok(Some(status.code().unwrap_or(-1)));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Restart kevy on the same data dir.
    pub fn restart(&mut self) -> io::Result<()> {
        if self.child.is_some() {
            self.kill(KillSignal::Sigkill)?;
        }
        self.start_child()
    }

    /// Returns the bound TCP port for clients to connect.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.config.port
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Best-effort cleanup on test panic / abnormal exit.
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Apply `RLIMIT_NOFILE` and `RLIMIT_FSIZE` to the calling process via
/// raw `setrlimit(2)` syscalls. Async-signal-safe; suitable for
/// `Command::pre_exec`. `0` for either limit = skip.
#[cfg(unix)]
fn apply_rlimits(nofile: u64, fsize: u64) -> io::Result<()> {
    #[repr(C)]
    struct RawRlimit {
        rlim_cur: u64,
        rlim_max: u64,
    }
    const RLIMIT_NOFILE: i32 = 7;
    #[cfg(target_os = "macos")]
    const RLIMIT_FSIZE: i32 = 1;
    #[cfg(target_os = "linux")]
    const RLIMIT_FSIZE: i32 = 1;
    unsafe extern "C" {
        fn setrlimit(resource: i32, rlim: *const RawRlimit) -> i32;
    }
    if nofile > 0 {
        let lim = RawRlimit { rlim_cur: nofile, rlim_max: nofile };
        // SAFETY: lim is on the stack and stays alive for the call; FFI
        // takes a const ptr; no aliasing.
        let rc = unsafe { setrlimit(RLIMIT_NOFILE, &lim) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    if fsize > 0 {
        let lim = RawRlimit { rlim_cur: fsize, rlim_max: fsize };
        // SAFETY: `lim` is a live `RawRlimit` on this frame and `setrlimit(2)` only reads
        // through the pointer for the duration of the call.
        let rc = unsafe { setrlimit(RLIMIT_FSIZE, &lim) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// A port for a server this process is about to start.
///
/// [`kevy_testnet::free_port`], which is the one implementation of this
/// question in the workspace. What stood here was the other one: bind
/// `127.0.0.1:0`, read the port, drop the listener — which leaves the port
/// unowned between that drop and the server's own bind, and under a parallel
/// `cargo test --workspace` something else can be in the gap. free_port hands
/// out from a block this process owns alone and probes by connecting, so it
/// holds nothing it hands over.
///
/// ```
/// let port = kevy_chaos::pick_free_port();
/// let server = std::net::TcpListener::bind(("127.0.0.1", port))?;
/// assert_eq!(server.local_addr()?.port(), port);
/// assert_ne!(kevy_chaos::pick_free_port(), port, "never handed out twice");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn pick_free_port() -> u16 {
    kevy_testnet::free_port()
}
