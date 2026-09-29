// A kevy server for the rustdoc examples, in a thread of the example's own
// process: the real command set, stopped and its data dir removed on drop.
// Included into an example as items. The example picks the port, so this
// file needs nothing beyond the server crates.

use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct Kevy {
    port: u16,
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    server: Option<JoinHandle<std::io::Result<()>>>,
}

// One shard on `port`.
#[allow(dead_code)]
pub fn kevy(port: u16) -> Kevy {
    start(port, 1, false)
}

// `shards` shards in cluster mode: shard `i` also listens on `port + 1 + i`,
// which is where a cluster client connects.
#[allow(dead_code)]
pub fn cluster(port: u16, shards: usize) -> Kevy {
    start(port, shards, true)
}

fn start(port: u16, shards: usize, cluster: bool) -> Kevy {
    let dir = std::env::temp_dir().join(format!("kevy-client-doc-{}-{port}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server = std::thread::spawn({
        let (stop, dir) = (Arc::clone(&stop), dir.clone());
        move || {
            let commands = if cluster {
                // CLUSTER SLOTS answers from the process-wide config
                let mut cfg = kevy_config::Config::default();
                cfg.server.port = port;
                cfg.server.threads = shards;
                cfg.cluster.enabled = true;
                cfg.cluster.port_base = port + 1;
                let state = kevy::RuntimeState::new(Arc::new(cfg), PathBuf::new(), shards).unwrap();
                kevy::KevyCommands::with_state(Arc::new(state))
            } else {
                kevy::KevyCommands::sharded(shards)
            };
            let rt = kevy_rt::Runtime::builder(commands)
                .bind([127, 0, 0, 1], port)
                .shards(shards)
                .with_data_dir(dir);
            let rt = if cluster { rt.with_cluster(port + 1) } else { rt };
            rt.run(stop)
        }
    });
    let last = if cluster { port + shards as u16 } else { port };
    for p in port..=last {
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", p)).is_err() {
            assert!(Instant::now() < deadline, "kevy did not start listening on {p}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    Kevy { port, dir, stop, server: Some(server) }
}

impl Drop for Kevy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // wakes a reactor parked on its listener
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(server) = self.server.take() {
            server.join().unwrap().unwrap();
        }
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}
