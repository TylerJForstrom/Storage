//! CRC-32 (IEEE 802.3 polynomial), written from scratch.
//!
//! Every frame in the write-ahead log ends with a CRC over its contents.
//! During crash recovery this is how we tell "a frame that was fully written
//! before the crash" apart from "a torn, half-written frame at the tail" —
//! the torn frame's checksum won't match, so recovery stops there.

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = make_table();

pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // Standard test vector for CRC-32/IEEE.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn detects_single_bit_flip() {
        let a = b"hello world".to_vec();
        let mut b = a.clone();
        b[3] ^= 0x01;
        assert_ne!(crc32(&a), crc32(&b));
    }
}
