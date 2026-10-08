//! Where a transport stream goes: a `.ts` file, UDP datagrams, or a TCP
//! server that media players connect to.
//!
//! - **UDP**: 7 packets (1316 bytes) per datagram, the usual size for TS over
//!   UDP. VLC: `udp://@:1234`. PotPlayer: `udp://127.0.0.1:1234`.
//! - **TCP**: a server; each client gets the live stream. A client that
//!   speaks HTTP (`GET …`) gets an HTTP response first, so VLC and PotPlayer
//!   can open `http://127.0.0.1:8001/`; one that says nothing (VLC's
//!   `tcp://127.0.0.1:8001`) gets the raw stream. Each client has its own
//!   thread and a bounded queue, so a slow or stalled player loses data
//!   rather than holding up the receiver.
//!
//! Both default to 127.0.0.1 in the GUI: reachable from this machine only,
//! unless pointed elsewhere on purpose.

use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::deframe::TS_LEN;

/// Packets per UDP datagram.
const PER_DATAGRAM: usize = 7;
/// Chunks a TCP client may have queued before new ones are dropped for it.
const CLIENT_QUEUE: usize = 256;

/// A `.ts` file.
pub struct TsFile {
    out: BufWriter<File>,
    pub path: PathBuf,
    pub packets: u64,
}

impl TsFile {
    pub fn create(path: &Path) -> io::Result<Self> {
        Ok(TsFile {
            out: BufWriter::new(File::create(path)?),
            path: path.to_path_buf(),
            packets: 0,
        })
    }

    pub fn write(&mut self, packets: &[[u8; TS_LEN]]) -> io::Result<()> {
        for p in packets {
            self.out.write_all(p)?;
        }
        self.packets += packets.len() as u64;
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// TS over UDP to one address.
pub struct UdpSink {
    socket: UdpSocket,
    pub target: SocketAddr,
    pending: Vec<u8>,
    pub datagrams: u64,
    pub errors: u64,
}

impl UdpSink {
    pub fn new(target: SocketAddr) -> io::Result<Self> {
        let bind: SocketAddr = if target.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let socket = UdpSocket::bind(bind)?;
        if target.ip().is_multicast() {
            let _ = socket.set_multicast_ttl_v4(1);
        }
        Ok(UdpSink {
            socket,
            target,
            pending: Vec::with_capacity(PER_DATAGRAM * TS_LEN),
            datagrams: 0,
            errors: 0,
        })
    }

    pub fn write(&mut self, packets: &[[u8; TS_LEN]]) {
        for p in packets {
            self.pending.extend_from_slice(p);
            if self.pending.len() == PER_DATAGRAM * TS_LEN {
                match self.socket.send_to(&self.pending, self.target) {
                    Ok(_) => self.datagrams += 1,
                    Err(_) => self.errors += 1,
                }
                self.pending.clear();
            }
        }
    }
}

struct Client {
    peer: SocketAddr,
    tx: SyncSender<Arc<Vec<u8>>>,
    alive: Arc<AtomicBool>,
}

/// A TCP server streaming TS to every connected client.
pub struct TcpSink {
    pub addr: SocketAddr,
    clients: Arc<Mutex<Vec<Client>>>,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    pub dropped_chunks: u64,
}

impl TcpSink {
    /// Listen on `addr` (port 0 picks a free one; see `addr` after).
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let clients: Arc<Mutex<Vec<Client>>> = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let accept = {
            let (clients, stop) = (clients.clone(), stop.clone());
            std::thread::Builder::new()
                .name("decdvb-ts-tcp".into())
                .spawn(move || accept_loop(listener, clients, stop))?
        };
        Ok(TcpSink {
            addr,
            clients,
            stop,
            accept: Some(accept),
            dropped_chunks: 0,
        })
    }

    /// Addresses of the players connected now.
    pub fn clients(&self) -> Vec<SocketAddr> {
        let mut c = self.clients.lock().unwrap();
        c.retain(|c| c.alive.load(Ordering::Relaxed));
        c.iter().map(|c| c.peer).collect()
    }

    pub fn write(&mut self, packets: &[[u8; TS_LEN]]) {
        if packets.is_empty() {
            return;
        }
        let mut c = self.clients.lock().unwrap();
        if c.is_empty() {
            return;
        }
        let chunk = Arc::new(packets.concat());
        c.retain(|cl| {
            if !cl.alive.load(Ordering::Relaxed) {
                return false;
            }
            match cl.tx.try_send(chunk.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    self.dropped_chunks += 1;
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
    }
}

impl Drop for TcpSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Dropping the senders ends the client threads.
        self.clients.lock().unwrap().clear();
        if let Some(j) = self.accept.take() {
            let _ = j.join();
        }
    }
}

fn accept_loop(listener: TcpListener, clients: Arc<Mutex<Vec<Client>>>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let (tx, rx) = mpsc::sync_channel::<Arc<Vec<u8>>>(CLIENT_QUEUE);
                let alive = Arc::new(AtomicBool::new(true));
                let a = alive.clone();
                let spawned = std::thread::Builder::new()
                    .name("decdvb-ts-client".into())
                    .spawn(move || {
                        serve_client(stream, rx);
                        a.store(false, Ordering::Relaxed);
                    });
                if spawned.is_ok() {
                    clients.lock().unwrap().push(Client { peer, tx, alive });
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Answer an HTTP request if the client sends one, then stream.
fn serve_client(mut s: TcpStream, rx: mpsc::Receiver<Arc<Vec<u8>>>) {
    let _ = s.set_nonblocking(false);
    let _ = s.set_nodelay(true);
    // A player speaking HTTP sends its request at once; a raw TCP client
    // sends nothing. Wait briefly to tell which.
    let _ = s.set_read_timeout(Some(Duration::from_millis(400)));
    let mut req = Vec::new();
    let mut buf = [0u8; 1024];
    while req.len() < 8192 {
        match s.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                req.extend_from_slice(&buf[..n]);
                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break, // timeout: a raw client
        }
    }
    if req.starts_with(b"GET ") || req.starts_with(b"HEAD ") {
        let head = "HTTP/1.0 200 OK\r\nContent-Type: video/mp2t\r\nCache-Control: no-cache\r\n\
                    Connection: close\r\nServer: DecDVB\r\n\r\n";
        if s.write_all(head.as_bytes()).is_err() || req.starts_with(b"HEAD ") {
            return;
        }
    }
    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
    while let Ok(chunk) = rx.recv() {
        if s.write_all(&chunk).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkts(n: usize) -> Vec<[u8; TS_LEN]> {
        (0..n)
            .map(|k| {
                let mut p = [k as u8; TS_LEN];
                p[0] = 0x47;
                p
            })
            .collect()
    }

    #[test]
    fn udp_sends_seven_packets_per_datagram() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut tx = UdpSink::new(rx.local_addr().unwrap()).unwrap();
        tx.write(&pkts(15));
        let mut buf = [0u8; 2048];
        let (n, _) = rx.recv_from(&mut buf).unwrap();
        assert_eq!(n, 7 * TS_LEN);
        assert_eq!(buf[0], 0x47);
        assert_eq!(buf[TS_LEN + 1], 1);
        assert_eq!(tx.datagrams, 2);
    }

    #[test]
    fn tcp_serves_raw_and_http_clients() {
        let mut sink = TcpSink::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = sink.addr;
        let mut raw = TcpStream::connect(addr).unwrap();
        let mut http = TcpStream::connect(addr).unwrap();
        http.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        // Let both be accepted and classified.
        let t0 = std::time::Instant::now();
        while sink.clients().len() < 2 && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(600));
        sink.write(&pkts(10));

        raw.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut got = vec![0u8; 10 * TS_LEN];
        raw.read_exact(&mut got).unwrap();
        assert_eq!(got[0], 0x47);
        assert_eq!(got[9 * TS_LEN + 1], 9);

        http.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            http.read_exact(&mut b).unwrap();
            head.push(b[0]);
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("HTTP/1.0 200 OK"));
        assert!(head.contains("video/mp2t"));
        let mut got = vec![0u8; TS_LEN];
        http.read_exact(&mut got).unwrap();
        assert_eq!(got[0], 0x47);
    }
}
