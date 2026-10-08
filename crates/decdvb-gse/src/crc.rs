//! CRC-32/MPEG-2 (EN 300 468 Annex A), which GSE uses on fragmented PDUs:
//! g(x) = 0x04C11DB7, MSB first, register starting at all ones, no final XOR.

use std::sync::OnceLock;

pub fn crc32_mpeg2(bytes: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (v, e) in t.iter_mut().enumerate() {
            let mut c = (v as u32) << 24;
            for _ in 0..8 {
                c = if c & 0x8000_0000 != 0 {
                    (c << 1) ^ 0x04C1_1DB7
                } else {
                    c << 1
                };
            }
            *e = c;
        }
        t
    });
    bytes.iter().fold(0xFFFF_FFFFu32, |c, &b| {
        (c << 8) ^ t[((c >> 24) as u8 ^ b) as usize]
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn check_value() {
        // The CRC catalogue's check value for CRC-32/MPEG-2.
        assert_eq!(super::crc32_mpeg2(b"123456789"), 0x0376_E6E7);
    }
}
