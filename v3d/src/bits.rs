//! Little-endian bit-field extraction for control-list packets and state records.

/// `n` bits (n <= 64) starting at bit `start` of the byte slice (bit 0 = LSB of byte 0).
#[inline]
pub fn ubits(b: &[u8], start: u32, n: u32) -> u64 {
    let first = (start / 8) as usize;
    let last = ((start + n - 1) / 8) as usize;
    let mut v: u128 = 0;
    for i in (first..=last).rev() { v = (v << 8) | *b.get(i).unwrap_or(&0) as u128; }
    let v = (v >> (start % 8)) as u64;
    if n >= 64 { v } else { v & ((1u64 << n) - 1) }
}

/// Signed `n`-bit field.
#[inline]
pub fn sbits(b: &[u8], start: u32, n: u32) -> i64 {
    let v = ubits(b, start, n);
    ((v << (64 - n)) as i64) >> (64 - n)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fields() {
        let b = [0x34, 0x12, 0xff, 0x80];
        assert_eq!(ubits(&b, 0, 16), 0x1234);
        assert_eq!(ubits(&b, 4, 8), 0x23);
        assert_eq!(sbits(&b, 24, 8), -128);
        assert_eq!(ubits(&b, 0, 32), 0x80ff1234);
    }
}
