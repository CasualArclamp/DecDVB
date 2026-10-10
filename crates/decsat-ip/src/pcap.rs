//! A classic libpcap file writer for raw IP packets (link type 101,
//! LINKTYPE_RAW: each record starts at the IP header, v4 or v6), which
//! Wireshark and tcpdump read as is.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// LINKTYPE_RAW.
const LINKTYPE_RAW: u32 = 101;
/// Longest packet stored whole.
const SNAPLEN: u32 = 65_535;

pub struct PcapWriter {
    out: BufWriter<File>,
    packets: u64,
    bytes: u64,
}

impl PcapWriter {
    /// Create `path` and write the global header.
    pub fn create(path: &Path) -> io::Result<Self> {
        let mut out = BufWriter::new(File::create(path)?);
        // Magic (microsecond timestamps, little-endian), version 2.4,
        // GMT offset 0, accuracy 0, snaplen, link type.
        out.write_all(&0xA1B2_C3D4u32.to_le_bytes())?;
        out.write_all(&2u16.to_le_bytes())?;
        out.write_all(&4u16.to_le_bytes())?;
        out.write_all(&0i32.to_le_bytes())?;
        out.write_all(&0u32.to_le_bytes())?;
        out.write_all(&SNAPLEN.to_le_bytes())?;
        out.write_all(&LINKTYPE_RAW.to_le_bytes())?;
        Ok(PcapWriter {
            out,
            packets: 0,
            bytes: 0,
        })
    }

    /// Append one packet stamped `at`.
    pub fn write(&mut self, at: SystemTime, packet: &[u8]) -> io::Result<()> {
        let t = at.duration_since(UNIX_EPOCH).unwrap_or_default();
        let stored = packet.len().min(SNAPLEN as usize);
        self.out.write_all(&(t.as_secs() as u32).to_le_bytes())?;
        self.out.write_all(&t.subsec_micros().to_le_bytes())?;
        self.out.write_all(&(stored as u32).to_le_bytes())?;
        self.out.write_all(&(packet.len() as u32).to_le_bytes())?;
        self.out.write_all(&packet[..stored])?;
        self.packets += 1;
        self.bytes += packet.len() as u64;
        Ok(())
    }

    pub fn packets(&self) -> u64 {
        self.packets
    }

    /// Bytes of packet data written (headers excluded).
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::udp_v4;

    #[test]
    fn writes_a_readable_file() {
        let path = std::env::temp_dir().join(format!("decsat-pcap-{}.pcap", std::process::id()));
        let p = udp_v4([10, 0, 0, 1], [10, 0, 0, 2], 5000, 6000, b"payload");
        {
            let mut w = PcapWriter::create(&path).unwrap();
            w.write(UNIX_EPOCH + std::time::Duration::from_micros(1_500_000), &p)
                .unwrap();
            w.write(SystemTime::now(), &p).unwrap();
            w.flush().unwrap();
            assert_eq!(w.packets(), 2);
        }
        let b = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(&b[..4], &[0xD4, 0xC3, 0xB2, 0xA1]);
        assert_eq!(u32::from_le_bytes(b[20..24].try_into().unwrap()), 101);
        // First record: ts 1 s 500 000 µs, lengths, then the packet.
        assert_eq!(u32::from_le_bytes(b[24..28].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(b[28..32].try_into().unwrap()), 500_000);
        assert_eq!(
            u32::from_le_bytes(b[32..36].try_into().unwrap()) as usize,
            p.len()
        );
        assert_eq!(&b[40..40 + p.len()], &p[..]);
        assert_eq!(b.len(), 24 + 2 * (16 + p.len()));
    }
}
