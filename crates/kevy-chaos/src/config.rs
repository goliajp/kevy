//! What a chaos run starts kevy with, and how it stops it.

use std::path::PathBuf;
use std::time::Duration;

/// How to terminate the kevy child for crash simulation.
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
/// let mut h = Harness::spawn(HarnessConfig { kevy_bin: bin, ..HarnessConfig::new(dir.join("data"), port) })?;
/// h.kill(KillSignal::Sigkill)?;
/// // the child is already reaped, so there is nothing left to wait for
/// assert_eq!(h.wait_exit(std::time::Duration::ZERO)?, Some(0));
///
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy)]
pub enum KillSignal {
    /// `SIGKILL` — abrupt, no graceful shutdown. The standard chaos signal.
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
    /// let mut h = Harness::spawn(HarnessConfig { kevy_bin: bin, ..HarnessConfig::new(dir.join("data"), port) })?;
    /// // crash it mid-run, then bring it back on the same data dir
    /// h.kill(KillSignal::Sigkill)?;
    /// h.restart()?;
    /// assert_eq!(h.port(), port);
    ///
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Sigkill,
    /// `SIGTERM` — graceful. For comparison tests asserting that
    /// graceful shutdown loses NOTHING even at `everysec` fsync.
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
    /// let mut h = Harness::spawn(HarnessConfig { kevy_bin: bin, ..HarnessConfig::new(dir.join("data"), port) })?;
    /// // a graceful stop; `kill` returns once the child has been reaped
    /// h.kill(KillSignal::Sigterm)?;
    /// assert_eq!(h.wait_exit(std::time::Duration::ZERO)?, Some(0));
    ///
    /// # std::fs::remove_dir_all(&dir)?;
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    Sigterm,
}

/// Config for one chaos run.
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
/// let cfg = HarnessConfig {
///     kevy_bin: bin,
///     max_clients: 16,
///     ..HarnessConfig::new(dir.join("data"), port)
/// }
/// .with_threads(4)
/// .with_fsync("everysec")
/// .with_extra_toml("[replication]\nrole = \"primary\"");
/// let h = Harness::spawn(cfg)?;
/// // the child was started from a kevy.toml rendered out of the config
/// let toml = std::fs::read_to_string(h.config.data_dir.join("kevy.toml"))?;
/// assert!(toml.contains(&format!("port = {port}")));
/// assert!(toml.contains("threads = 4"));
/// assert!(toml.contains("max_clients = 16"));
/// assert!(toml.contains("appendfsync = \"everysec\""));
/// assert!(toml.contains("[replication]\nrole = \"primary\""));
/// # drop(h);
///
/// # std::fs::remove_dir_all(&dir)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    /// Path to the kevy binary. Default: `$KEVY_BIN` env var or
    /// `target/release/kevy` relative to the workspace root.
    ///
    /// ```
    /// use kevy_chaos::HarnessConfig;
    ///
    /// let port = kevy_chaos::pick_free_port();
    /// let cfg = HarnessConfig::new(std::env::temp_dir().join("kevy-chaos-run"), port);
    /// if std::env::var_os("KEVY_BIN").is_none() {
    ///     assert_eq!(cfg.kevy_bin, std::path::PathBuf::from("target/release/kevy"));
    /// }
    /// let cfg = HarnessConfig { kevy_bin: "/opt/kevy/bin/kevy".into(), ..cfg };
    /// assert_eq!(cfg.kevy_bin, std::path::PathBuf::from("/opt/kevy/bin/kevy"));
    /// ```
    pub kevy_bin: PathBuf,
    /// TCP port for kevy to bind. Default: ephemeral via `pick_free_port`.
    ///
    /// ```
    /// use kevy_chaos::HarnessConfig;
    ///
    /// let port = kevy_chaos::pick_free_port();
    /// let cfg = HarnessConfig::new(std::env::temp_dir().join("kevy-chaos-run"), port);
    /// assert_eq!(cfg.port, port);
    /// ```
    pub port: u16,
    /// kevy shard count (`--threads N`). Default: 2.
    ///
    /// ```
    /// use kevy_chaos::HarnessConfig;
    ///
    /// let port = kevy_chaos::pick_free_port();
    /// let cfg = HarnessConfig::new(std::env::temp_dir().join("kevy-chaos-run"), port);
    /// assert_eq!(cfg.threads, 2);
    /// assert_eq!(cfg.with_threads(4).threads, 4);
    /// ```
    pub threads: usize,
    /// kevy data directory (AOF + snapshots persist here across restart).
    /// Use a temp dir per test; harness does NOT clean up (the test owns it).
    ///
    /// ```
    /// use kevy_chaos::HarnessConfig;
    ///
    /// let port = kevy_chaos::pick_free_port();
    /// let cfg = HarnessConfig::new(std::env::temp_dir().join("kevy-chaos-run"), port);
    /// assert_eq!(cfg.data_dir, std::env::temp_dir().join("kevy-chaos-run"));
    /// ```
    pub data_dir: PathBuf,
    /// AOF fsync policy. `"always"` / `"everysec"` / `"no"`. Default: `"always"`.
    ///
    /// ```
    /// use kevy_chaos::HarnessConfig;
    ///
    /// let port = kevy_chaos::pick_free_port();
    /// let cfg = HarnessConfig::new(std::env::temp_dir().join("kevy-chaos-run"), port);
    /// assert_eq!(cfg.appendfsync, "always");
    /// assert_eq!(cfg.with_fsync("everysec").appendfsync, "everysec");
    /// ```
    pub appendfsync: String,
    /// Optional: force frequent AOF rewrites by setting this low. Bytes.
    /// `None` keeps the kevy default (64 MiB).
    ///
    /// ```
    /// use kevy_chaos::HarnessConfig;
    ///
    /// let port = kevy_chaos::pick_free_port();
    /// let cfg = HarnessConfig::new(std::env::temp_dir().join("kevy-chaos-run"), port);
    /// assert_eq!(cfg.aof_rewrite_min_size, None);
    /// // rewrite as soon as the AOF passes 64 KiB
    /// let cfg = HarnessConfig { aof_rewrite_min_size: Some(64 * 1024), ..cfg };
    /// assert_eq!(cfg.aof_rewrite_min_size, Some(65_536));
    /// ```
    pub aof_rewrite_min_size: Option<u64>,
    /// Optional: percentage growth-since-last-rewrite that triggers an
    /// auto-rewrite. `None` keeps the kevy default (100 = 2× growth).
    ///
    /// ```
    /// use kevy_chaos::HarnessConfig;
    ///
    /// let port = kevy_chaos::pick_free_port();
    /// let cfg = HarnessConfig::new(std::env::temp_dir().join("kevy-chaos-run"), port);
    /// assert_eq!(cfg.aof_rewrite_pct, None);
    /// // rewrite once the AOF has grown by half since the last rewrite
    /// let cfg = HarnessConfig { aof_rewrite_pct: Some(50), ..cfg };
    /// assert_eq!(cfg.aof_rewrite_pct, Some(50));
    /// ```
    pub aof_rewrite_pct: Option<u32>,
    /// Free-form TOML appended to the spawned kevy's
    /// `kevy.toml`. Empty by default. Use to set `[replication]`
    /// sections for primary/replica chaos tests, or any other section
    /// not yet covered by typed fields above. NOTE: appended after
    /// `[persistence]`; lines without a section header attach to
    /// persistence. Use `[server]\n` etc. prefix if needed.
    ///
    /// ```
    /// use kevy_chaos::HarnessConfig;
    ///
    /// let port = kevy_chaos::pick_free_port();
    /// let cfg = HarnessConfig::new(std::env::temp_dir().join("kevy-chaos-run"), port);
    /// assert!(cfg.extra_toml.is_empty());
    /// let cfg = cfg.with_extra_toml("[replication]\nrole = \"primary\"\n");
    /// assert!(cfg.extra_toml.starts_with("[replication]"));
    /// ```
    pub extra_toml: String,
    /// `[server] max_clients = N`. `0` keeps the kevy
    /// default (10 000). Set explicitly for the maxclients chaos test.
    ///
    /// ```
    /// use kevy_chaos::HarnessConfig;
    ///
    /// let port = kevy_chaos::pick_free_port();
    /// let cfg = HarnessConfig::new(std::env::temp_dir().join("kevy-chaos-run"), port);
    /// assert_eq!(cfg.max_clients, 0, "0 keeps kevy's own default");
    /// let cfg = HarnessConfig { max_clients: 16, ..cfg };
    /// assert_eq!(cfg.max_clients, 16);
    /// ```
    pub max_clients: usize,
    /// `RLIMIT_NOFILE` for the spawned kevy. `0` = inherit
    /// from parent. Use to test fd-exhaustion behavior.
    ///
    /// ```
    /// use kevy_chaos::HarnessConfig;
    ///
    /// let port = kevy_chaos::pick_free_port();
    /// let cfg = HarnessConfig::new(std::env::temp_dir().join("kevy-chaos-run"), port);
    /// assert_eq!(cfg.rlimit_nofile, 0, "0 inherits the parent's limit");
    /// let cfg = HarnessConfig { rlimit_nofile: 64, ..cfg };
    /// assert_eq!(cfg.rlimit_nofile, 64);
    /// ```
    pub rlimit_nofile: u64,
    /// `RLIMIT_FSIZE` for the spawned kevy. `0` = inherit.
    /// Use to test disk-full / quota-exhaustion behavior. kevy writes
    /// past this limit get `SIGXFSZ` from the kernel; kevy must
    /// catch / report cleanly without panicking.
    ///
    /// ```
    /// use kevy_chaos::HarnessConfig;
    ///
    /// let port = kevy_chaos::pick_free_port();
    /// let cfg = HarnessConfig::new(std::env::temp_dir().join("kevy-chaos-run"), port);
    /// assert_eq!(cfg.rlimit_fsize, 0, "0 inherits the parent's limit");
    /// // the kernel refuses writes that would grow a file past 1 MiB
    /// let cfg = HarnessConfig { rlimit_fsize: 1 << 20, ..cfg };
    /// assert_eq!(cfg.rlimit_fsize, 1_048_576);
    /// ```
    pub rlimit_fsize: u64,
    /// Timeout for "kevy ready" wait after spawn. Default: 10 s.
    ///
    /// ```
    /// use kevy_chaos::HarnessConfig;
    ///
    /// let port = kevy_chaos::pick_free_port();
    /// let cfg = HarnessConfig::new(std::env::temp_dir().join("kevy-chaos-run"), port);
    /// assert_eq!(cfg.spawn_timeout, std::time::Duration::from_secs(10));
    /// let cfg = HarnessConfig { spawn_timeout: std::time::Duration::from_secs(30), ..cfg };
    /// assert_eq!(cfg.spawn_timeout.as_secs(), 30);
    /// ```
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
            aof_rewrite_min_size: None,
            aof_rewrite_pct: None,
            extra_toml: String::new(),
            max_clients: 0,
            rlimit_nofile: 0,
            rlimit_fsize: 0,
            spawn_timeout: Duration::from_secs(10),
        }
    }

    /// Builder for `extra_toml`.
    #[must_use]
    pub fn with_extra_toml(mut self, extra: impl Into<String>) -> Self {
        self.extra_toml = extra.into();
        self
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
