//! Live statistics over the IP packets a VFO extracts.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;

use crate::packet::IpInfo;

/// Flows kept for the top-talkers list. Past this the smallest are dropped,
/// so a scan of millions of addresses cannot grow the table without bound.
const MAX_FLOWS: usize = 4096;

/// One direction of traffic between two addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flow {
    pub src: IpAddr,
    pub dst: IpAddr,
    pub packets: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct IpStats {
    pub packets: u64,
    pub bytes: u64,
    pub ipv4: u64,
    pub ipv6: u64,
    /// Packets by IP protocol number.
    pub protocols: BTreeMap<u8, u64>,
    flows: HashMap<(IpAddr, IpAddr), (u64, u64)>,
}

impl IpStats {
    pub fn add(&mut self, p: &IpInfo) {
        self.packets += 1;
        self.bytes += p.len as u64;
        if p.src.is_ipv4() {
            self.ipv4 += 1;
        } else {
            self.ipv6 += 1;
        }
        *self.protocols.entry(p.protocol).or_default() += 1;
        if self.flows.len() >= MAX_FLOWS && !self.flows.contains_key(&(p.src, p.dst)) {
            // Make room: drop the smallest quarter.
            let mut sizes: Vec<u64> = self.flows.values().map(|v| v.1).collect();
            sizes.sort_unstable();
            let cut = sizes[sizes.len() / 4];
            self.flows.retain(|_, v| v.1 > cut);
        }
        let f = self.flows.entry((p.src, p.dst)).or_default();
        f.0 += 1;
        f.1 += p.len as u64;
    }

    /// The `n` flows with the most bytes.
    pub fn top_flows(&self, n: usize) -> Vec<Flow> {
        let mut v: Vec<Flow> = self
            .flows
            .iter()
            .map(|(&(src, dst), &(packets, bytes))| Flow {
                src,
                dst,
                packets,
                bytes,
            })
            .collect();
        v.sort_unstable_by_key(|f| std::cmp::Reverse(f.bytes));
        v.truncate(n);
        v
    }

    pub fn flow_count(&self) -> usize {
        self.flows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{parse, udp_v4};

    #[test]
    fn counts_and_ranks_flows() {
        let mut s = IpStats::default();
        let big = udp_v4([1, 1, 1, 1], [2, 2, 2, 2], 1, 2, &[0; 1000]);
        let small = udp_v4([3, 3, 3, 3], [4, 4, 4, 4], 1, 2, &[0; 10]);
        for _ in 0..3 {
            s.add(&parse(&big).unwrap());
        }
        s.add(&parse(&small).unwrap());
        assert_eq!(s.packets, 4);
        assert_eq!(s.ipv4, 4);
        assert_eq!(s.protocols[&17], 4);
        let top = s.top_flows(5);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].packets, 3);
        assert_eq!(top[0].bytes, 3 * big.len() as u64);
    }

    #[test]
    fn the_flow_table_stays_bounded() {
        let mut s = IpStats::default();
        for i in 0..20_000u32 {
            let b = i.to_be_bytes();
            let p = udp_v4([10, b[1], b[2], b[3]], [10, 0, 0, 1], 1, 2, &[]);
            s.add(&parse(&p).unwrap());
        }
        assert!(s.flow_count() <= MAX_FLOWS);
        assert_eq!(s.packets, 20_000);
    }
}
