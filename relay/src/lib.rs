//! MeshDoc relay.
//!
//! A deliberately dumb, untrusted store-and-forward mailbox. It persists opaque
//! encrypted blobs per workspace, assigns per-workspace monotonic sequence
//! numbers, and hands blobs back on demand. It never inspects or interprets blob
//! contents: the payload is only ever hashed, stored verbatim and returned
//! verbatim.
//!
//! The [`Store`] type contains the on-disk logic; [`Relay`] wires it up to a
//! thread-based `tiny_http` server (no async runtime).

pub mod store;

mod http;

pub use store::{BlobRecord, ListOutcome, PutOutcome, Store};

use std::error::Error;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Errors surfaced by [`Relay::start`].
pub type RelayError = Box<dyn Error + Send + Sync + 'static>;

/// Number of HTTP worker threads. tiny_http is thread-based; each worker pulls
/// one request at a time from a shared queue.
const HTTP_WORKERS: usize = 4;

/// How often the optional TTL reaper scans the data directory.
const EVICTION_INTERVAL: Duration = Duration::from_secs(60);

/// Runtime configuration for the relay.
#[derive(Debug, Clone)]
pub struct Config {
    /// Root directory holding one subdirectory per workspace.
    pub data_dir: PathBuf,
    /// TCP port (0 = ephemeral, useful for tests).
    pub port: u16,
    /// IP address to bind, e.g. `127.0.0.1`.
    pub bind: String,
    /// Maximum accepted blob size in bytes (413 above this).
    pub max_blob_bytes: usize,
    /// If set, blobs older than this many hours (mtime) are evicted.
    pub ttl_hours: Option<u64>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            data_dir: PathBuf::from("./relay-data"),
            port: 8080,
            bind: "127.0.0.1".to_string(),
            max_blob_bytes: 8 * 1024 * 1024,
            ttl_hours: None,
        }
    }
}

/// A running relay. Drop or [`Relay::stop`] it to shut down the listener and
/// worker threads.
pub struct Relay {
    server: Arc<tiny_http::Server>,
    store: Arc<Store>,
    shutdown: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    evictor: Option<JoinHandle<()>>,
}

impl Relay {
    /// Open the data directory, rebuild the index, bind the socket and start
    /// serving. Returns once the listener is ready.
    pub fn start(config: Config) -> Result<Relay, RelayError> {
        let store = Arc::new(Store::open(config.data_dir)?);
        let bind_address = format!("{}:{}", config.bind, config.port);
        let server = Arc::new(tiny_http::Server::http(bind_address.as_str())?);
        let shutdown = Arc::new(AtomicBool::new(false));

        let mut workers = Vec::with_capacity(HTTP_WORKERS);
        for index in 0..HTTP_WORKERS {
            let server = Arc::clone(&server);
            let store = Arc::clone(&store);
            let shutdown = Arc::clone(&shutdown);
            let max_blob_bytes = config.max_blob_bytes;
            let handle = thread::Builder::new()
                .name(format!("relay-http-{}", index))
                .spawn(move || {
                    while !shutdown.load(Ordering::Relaxed) {
                        match server.recv_timeout(Duration::from_millis(100)) {
                            Ok(Some(request)) => {
                                let store = Arc::clone(&store);
                                // A panic in one handler must not kill the worker.
                                let result =
                                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                        http::handle(request, &store, max_blob_bytes);
                                    }));
                                if result.is_err() {
                                    eprintln!("relay: request handler panicked");
                                }
                            }
                            Ok(None) => {}
                            Err(err) => {
                                eprintln!("relay: accept error: {}", err);
                                thread::sleep(Duration::from_millis(50));
                            }
                        }
                    }
                })?;
            workers.push(handle);
        }

        let evictor = if let Some(hours) = config.ttl_hours {
            let store = Arc::clone(&store);
            let shutdown = Arc::clone(&shutdown);
            let max_age = Duration::from_secs(hours.saturating_mul(3600));
            Some(
                thread::Builder::new()
                    .name("relay-evict".to_string())
                    .spawn(move || {
                        let mut next_sweep = Instant::now();
                        while !shutdown.load(Ordering::Relaxed) {
                            if Instant::now() >= next_sweep {
                                match store.evict_older_than(max_age) {
                                    Ok(0) => {}
                                    Ok(removed) => {
                                        println!("relay: evicted {} expired blob(s)", removed)
                                    }
                                    Err(err) => eprintln!("relay: eviction sweep failed: {}", err),
                                }
                                next_sweep = Instant::now() + EVICTION_INTERVAL;
                            }
                            thread::sleep(Duration::from_millis(200));
                        }
                    })?,
            )
        } else {
            None
        };

        Ok(Relay {
            server,
            store,
            shutdown,
            workers,
            evictor,
        })
    }

    /// Actual bound socket address (useful when `port == 0`).
    pub fn addr(&self) -> SocketAddr {
        self.server
            .server_addr()
            .to_ip()
            .expect("relay binds only IP sockets")
    }

    /// Root data directory in use.
    pub fn data_dir(&self) -> &Path {
        self.store.root()
    }

    /// Shut down workers and the reaper, then release the listener.
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        if let Some(evictor) = self.evictor.take() {
            let _ = evictor.join();
        }
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop();
    }
}
