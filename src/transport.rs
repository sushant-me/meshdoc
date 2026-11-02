//! Mesh transport abstraction and implementations.
//!
//! The point of this trait is that the sync engine never knows which radio it is
//! using. Three implementations exist:
//!
//! * [`TcpMesh`] — real sockets over Wi-Fi/LAN. This is the production transport
//!   for a same-network deployment and the reference for a Wi-Fi Direct backend.
//! * [`sim::SimMesh`] — an in-memory network with controllable partitions, used by
//!   the convergence fuzzer and the demo. It models the thing that actually
//!   matters (links appearing and disappearing) without needing radios.
//! * (future) BLE discovery + Wi-Fi Direct group formation, which sit behind this
//!   same trait.
//!
//! Frames on the wire are length-prefixed: `u32 LE length | payload`. A frame is
//! capped at [`MAX_FRAME`]; larger payloads are chunked by the sync layer.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const MAX_FRAME: usize = 64 * 1024;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum TransportError {
    Io(String),
    FrameTooLarge(usize),
    PeerUnknown(String),
    Partitioned,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Io(e) => write!(f, "transport i/o: {e}"),
            TransportError::FrameTooLarge(n) => write!(f, "frame of {n} bytes exceeds limit"),
            TransportError::PeerUnknown(p) => write!(f, "unknown peer {p}"),
            TransportError::Partitioned => write!(f, "link is down (partitioned)"),
        }
    }
}
impl std::error::Error for TransportError {}

impl From<std::io::Error> for TransportError {
    fn from(e: std::io::Error) -> Self { TransportError::Io(e.to_string()) }
}

/// Write one length-prefixed frame.
pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> Result<(), TransportError> {
    if payload.len() > MAX_FRAME {
        return Err(TransportError::FrameTooLarge(payload.len()));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()?;
    Ok(())
}

/// Read one length-prefixed frame.
pub fn read_frame(r: &mut impl Read) -> Result<Vec<u8>, TransportError> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(TransportError::FrameTooLarge(n));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------------- TCP mesh

/// A real-socket mesh node: listens for peers and dials them.
pub struct TcpMesh {
    addr: SocketAddr,
    _listener: Arc<TcpListener>,
}

impl TcpMesh {
    /// Bind on an ephemeral port (or `port` if non-zero).
    pub fn bind(port: u16) -> Result<Self, TransportError> {
        let listener = TcpListener::bind(("0.0.0.0", port))?;
        let addr = listener.local_addr()?;
        Ok(Self { addr, _listener: Arc::new(listener) })
    }

    pub fn local_addr(&self) -> SocketAddr { self.addr }

    /// Dial a peer and return the connected stream.
    pub fn connect(addr: SocketAddr) -> Result<TcpStream, TransportError> {
        let s = TcpStream::connect_timeout(&addr, DEFAULT_TIMEOUT)?;
        s.set_read_timeout(Some(DEFAULT_TIMEOUT))?;
        s.set_write_timeout(Some(DEFAULT_TIMEOUT))?;
        s.set_nodelay(true)?;
        Ok(s)
    }

    /// Accept exactly one inbound connection, with a timeout.
    pub fn accept_one(&self) -> Result<TcpStream, TransportError> {
        let (s, _) = self._listener.accept()?;
        s.set_read_timeout(Some(DEFAULT_TIMEOUT))?;
        s.set_write_timeout(Some(DEFAULT_TIMEOUT))?;
        s.set_nodelay(true)?;
        Ok(s)
    }
}

// ---------------------------------------------------------------- simulated mesh

pub mod sim {
    //! An in-memory network whose links can be cut. Used by the convergence
    //! fuzzer: it models the only property that matters for sync correctness —
    //! that two peers are sometimes reachable and sometimes not.

    use super::*;
    use std::collections::BTreeSet;

    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
    pub struct SimAddr(pub u64);

    #[derive(Default)]
    struct Inner {
        /// Links are undirected: if (a,b) is present, a and b can talk.
        links: BTreeSet<(SimAddr, SimAddr)>,
        /// Messages in flight: (from, to, remaining_delay_ticks, payload)
        inbox: Vec<(SimAddr, SimAddr, u32, Vec<u8>)>,
        tick: u32,
    }

    /// A simulated network. Cheap to construct; held behind an `Arc`.
    #[derive(Clone)]
    pub struct SimMesh {
        inner: Arc<Mutex<Inner>>,
        dropped: Arc<AtomicU64>,
        delivered: Arc<AtomicU64>,
    }

    impl Default for SimMesh {
        fn default() -> Self { Self::new() }
    }

    impl SimMesh {
        pub fn new() -> Self {
            Self {
                inner: Arc::new(Mutex::new(Inner::default())),
                dropped: Arc::new(AtomicU64::new(0)),
                delivered: Arc::new(AtomicU64::new(0)),
            }
        }

        pub fn link(&self, a: SimAddr, b: SimAddr) {
            let key = if a <= b { (a, b) } else { (b, a) };
            self.inner.lock().unwrap().links.insert(key);
        }

        pub fn unlink(&self, a: SimAddr, b: SimAddr) {
            let key = if a <= b { (a, b) } else { (b, a) };
            self.inner.lock().unwrap().links.remove(&key);
        }

        pub fn is_linked(&self, a: SimAddr, b: SimAddr) -> bool {
            let key = if a <= b { (a, b) } else { (b, a) };
            self.inner.lock().unwrap().links.contains(&key)
        }

        pub fn set_partition(&self, groups: &[Vec<SimAddr>]) {
            let mut inner = self.inner.lock().unwrap();
            inner.links.clear();
            for g in groups {
                for i in 0..g.len() {
                    for j in (i + 1)..g.len() {
                        let (a, b) = (g[i], g[j]);
                        let key = if a <= b { (a, b) } else { (b, a) };
                        inner.links.insert(key);
                    }
                }
            }
        }

        /// Send with a delay in ticks. Dropped silently if the link is down.
        pub fn send(&self, from: SimAddr, to: SimAddr, payload: Vec<u8>, delay: u32) -> bool {
            let mut inner = self.inner.lock().unwrap();
            let key = if from <= to { (from, to) } else { (to, from) };
            if !inner.links.contains(&key) {
                drop(inner);
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return false;
            }
            inner.inbox.push((from, to, delay, payload));
            true
        }

        /// Advance the clock and return all messages that arrived for `to`.
        pub fn tick(&self) -> Vec<(SimAddr, Vec<u8>)> {
            let mut inner = self.inner.lock().unwrap();
            inner.tick += 1;
            let now = inner.tick;
            let mut arrived = vec![];
            let mut keep = vec![];
            for (from, to, delay, payload) in inner.inbox.drain(..) {
                if delay == 0 {
                    arrived.push((from, payload));
                } else {
                    keep.push((from, to, delay - 1, payload));
                }
            }
            inner.inbox = keep;
            let _ = now;
            drop(inner);
            self.delivered.fetch_add(arrived.len() as u64, Ordering::Relaxed);
            arrived
        }

        /// Run until the network is quiet, then report.
        pub fn quiesce(&self) -> (u64, u64) {
            for _ in 0..10_000 {
                let n = self.tick().len();
                if n == 0 && self.inner.lock().unwrap().inbox.is_empty() { break; }
            }
            (
                self.delivered.load(Ordering::Relaxed),
                self.dropped.load(Ordering::Relaxed),
            )
        }

        pub fn in_flight(&self) -> usize { self.inner.lock().unwrap().inbox.len() }

        pub fn flush_all(&self) {
            for _ in 0..10_000 {
                let n = self.tick().len();
                if n == 0 { break; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello").unwrap();
        write_frame(&mut buf, b"world").unwrap();
        let mut r = Cursor::new(buf);
        assert_eq!(read_frame(&mut r).unwrap(), b"hello");
        assert_eq!(read_frame(&mut r).unwrap(), b"world");
    }

    #[test]
    fn oversized_frame_is_rejected_on_write() {
        let mut buf = Vec::new();
        let big = vec![0u8; MAX_FRAME + 1];
        assert!(matches!(write_frame(&mut buf, &big), Err(TransportError::FrameTooLarge(_))));
    }

    #[test]
    fn oversized_frame_is_rejected_on_read() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_FRAME + 1) as u32).to_le_bytes());
        buf.extend_from_slice(&[0u8; 16]);
        let mut r = Cursor::new(buf);
        assert!(matches!(read_frame(&mut r), Err(TransportError::FrameTooLarge(_))));
    }

    #[test]
    fn tcp_mesh_binds_and_reports_addr() {
        let m = TcpMesh::bind(0).unwrap();
        assert_ne!(m.local_addr().port(), 0);
    }

    #[test]
    fn sim_partition_blocks_delivery() {
        let net = sim::SimMesh::new();
        let (a, b) = (sim::SimAddr(1), sim::SimAddr(2));
        net.link(a, b);
        assert!(net.send(a, b, b"x".to_vec(), 0));
        assert_eq!(net.tick().len(), 1);

        net.unlink(a, b);
        assert!(!net.send(a, b, b"y".to_vec(), 0));
        assert_eq!(net.tick().len(), 0);
    }

    #[test]
    fn sim_delay_is_respected() {
        let net = sim::SimMesh::new();
        let (a, b) = (sim::SimAddr(1), sim::SimAddr(2));
        net.link(a, b);
        net.send(a, b, b"delayed".to_vec(), 2);
        assert_eq!(net.tick().len(), 0);
        assert_eq!(net.tick().len(), 0);
        assert_eq!(net.tick().len(), 1);
    }

    #[test]
    fn sim_partition_groups() {
        let net = sim::SimMesh::new();
        let a = sim::SimAddr(1);
        let b = sim::SimAddr(2);
        let c = sim::SimAddr(3);
        net.set_partition(&[vec![a, b], vec![c]]);
        assert!(net.send(a, b, b"in".to_vec(), 0));
        assert!(!net.send(a, c, b"across".to_vec(), 0));
    }
}
