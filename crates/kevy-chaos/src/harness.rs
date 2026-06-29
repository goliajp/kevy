//! Spawn + kill + restart a kevy child process. Public API is `Harness`.

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How to terminate the kevy child for crash simulation.
#[derive(Debug, Clone, Copy)]
pub enum KillSignal {
    /// `SIGKILL` — abrupt, no graceful shutdown. The standard chaos signal.
    Sigkill,
    /// `SIGTERM` — graceful. For comparison tests asserting that
    /// graceful shutdown loses NOTHING even at `everysec` fsync.
    Sigterm,
}

/// Config for one chaos run.
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    /// Path to the kevy binary. Default: `$KEVY_BIN` env var or
    /// `target/release/kevy` relative to the workspace root.
    pub kevy_bin: PathBuf,
    /// TCP port for kevy to bind. Default: ephemeral via `pick_free_port`.
    pub port: u16,
    /// kevy shard count (`--threads N`). Default: 2.
    pub threads: usize,
    /// kevy data directory (AOF + snapshots persist here across restart).
    /// Use a temp dir per test; harness does NOT clean up (the test owns it).
    pub data_dir: PathBuf,
    /// AOF fsync policy. `"always"` / `"everysec"` / `"no"`. Default: `"always"`.
    pub appendfsync: String,
    /// Timeout for "kevy ready" wait after spawn. Default: 10 s.
    pub spawn_timeout: Duration,
}

impl HarnessConfig {
    /// Build a config with the named data dir + port. Caller picks port to
    /// avoid collisions in parallel tests.
    #[must_use]
    pub fn new(data_dir: PathBuf, port: u16) -> Self {
        Self {
            kevy_bin: default_kevy_bin(),
            port,
            threads: 2,
            data_dir,
            appendfsync: "always".to_string(),
            spawn_timeout: Duration::from_secs(10),
        }
    }
    /// Override the AOF fsync policy.
    #[must_use]
    pub fn with_fsync(mut self, fsync: &str) -> Self {
        self.appendfsync = fsync.to_string();
        self
    }
    /// Override the shard count.
    #[must_use]
    pub fn with_threads(mut self, n: usize) -> Self {
        self.threads = n;
        self
    }
}

fn default_kevy_bin() -> PathBuf {
    if let Ok(p) = std::env::var("KEVY_BIN") {
        return PathBuf::from(p);
    }
    // Fall back to release binary at workspace root. Caller can override.
    PathBuf::from("target/release/kevy")
}

/// Active kevy child + ready port.
pub struct Harness {
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
        std::fs::write(
            &cfg_path,
            format!(
                "[server]\nport = {}\nthreads = {}\ndata_dir = \"{}\"\n\
                 [persistence]\nappendfsync = \"{}\"\n",
                self.config.port,
                self.config.threads,
                self.config.data_dir.display(),
                self.config.appendfsync,
            ),
        )?;
        // Route kevy's stderr to a file under the data dir so test
        // diagnostics (AOF replay summary, etc.) survive the test.
        let stderr_path = self.config.data_dir.join("kevy.stderr.log");
        let stderr_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stderr_path)?;
        let mut cmd = Command::new(&self.config.kevy_bin);
        cmd.arg("--config")
            .arg(&cfg_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file));
        let child = cmd.spawn()?;
        self.child = Some(child);
        self.wait_ready()
    }

    fn wait_ready(&self) -> io::Result<()> {
        let deadline = Instant::now() + self.config.spawn_timeout;
        let addr = (format!("127.0.0.1:{}", self.config.port).as_str())
            .to_socket_addrs()?
            .next()
            .expect("addr resolves");
        loop {
            if let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) {
                use std::io::{Read, Write};
                let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                if s.write_all(b"*1\r\n$4\r\nPING\r\n").is_ok() {
                    let mut buf = [0u8; 16];
                    if let Ok(n) = s.read(&mut buf) {
                        if n > 0 && buf.starts_with(b"+PONG") {
                            return Ok(());
                        }
                    }
                }
            }
            if Instant::now() > deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "kevy ready timeout"));
            }
            std::thread::sleep(Duration::from_millis(50));
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
                let _ = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .status();
            }
        }
        let _ = child.wait();
        Ok(())
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

/// Pick an ephemeral free port (bind 127.0.0.1:0 → return port → drop).
pub fn pick_free_port() -> io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}
