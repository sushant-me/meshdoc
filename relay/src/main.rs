//! MeshDoc relay binary: a dumb, untrusted store-and-forward mailbox for opaque
//! encrypted blobs. See `README.md` for the wire contract and threat model.

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use clap::Parser;
use meshdoc_relay::{Config, Relay};

#[derive(Debug, Parser)]
#[command(
    name = "meshdoc-relay",
    version,
    about = "Dumb store-and-forward mailbox for opaque encrypted blobs (never reads content)"
)]
struct Cli {
    /// Directory holding relay data (one subdirectory per workspace).
    #[arg(long, default_value = "./relay-data")]
    data: PathBuf,

    /// TCP port to listen on. Use 0 to let the OS pick an ephemeral port.
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// IP address to bind.
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    /// Maximum accepted blob size in bytes (larger POSTs get 413).
    #[arg(long, default_value_t = 8 * 1024 * 1024)]
    max_blob_bytes: usize,

    /// Optional eviction: delete blobs whose mtime is older than this many hours.
    #[arg(long)]
    ttl_hours: Option<u64>,
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();

    let config = Config {
        data_dir: cli.data.clone(),
        port: cli.port,
        bind: cli.bind.clone(),
        max_blob_bytes: cli.max_blob_bytes,
        ttl_hours: cli.ttl_hours,
    };

    let relay = Relay::start(config)?;
    println!(
        "meshdoc-relay {} listening on http://{} (data dir: {}, max blob: {} bytes, ttl: {})",
        env!("CARGO_PKG_VERSION"),
        relay.addr(),
        relay.data_dir().display(),
        cli.max_blob_bytes,
        match cli.ttl_hours {
            Some(hours) => format!("{}h", hours),
            None => "disabled".to_string(),
        }
    );

    // The process is stopped with Ctrl-C / SIGTERM; nothing else to do here.
    loop {
        thread::park_timeout(Duration::from_secs(3600));
    }
}
