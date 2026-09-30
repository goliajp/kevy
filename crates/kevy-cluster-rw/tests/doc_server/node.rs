// A kevy node for the rustdoc examples, in a thread of the example's own
// process: the real command set, in memory, stopped and cleaned up on drop.
// Nodes are independent — nothing replicates between them — so an example
// can see which node a command reached. Included as items; the example
// picks the port.

use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct Node {
    pub port: u16,
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    server: Option<JoinHandle<std::io::Result<()>>>,
}

pub fn node(port: u16) -> Node {
    let dir = std::env::temp_dir().join(format!("kevy-rw-doc-{}-{port}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server = std::thread::spawn({
        let (stop, dir) = (Arc::clone(&stop), dir.clone());
        move || {
            kevy_rt::Runtime::builder(kevy::KevyCommands::sharded(1))
                .bind([127, 0, 0, 1], port)
                .shards(1)
                .with_data_dir(dir)
                .with_aof(false)
                .run(stop)
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "kevy did not start listening on {port}");
        std::thread::sleep(Duration::from_millis(5));
    }
    Node { port, dir, stop, server: Some(server) }
}

impl Drop for Node {
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
