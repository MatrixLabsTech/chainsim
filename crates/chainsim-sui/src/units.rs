//! Amounts, addresses and base64 for protobuf-JSON request bodies.

use chainsim_core::{Error, ErrorCode, Result};

/// Whole units (`25` or `0.5`) to base units at `decimals`.
pub fn parse_amount(s: &str, decimals: u8) -> Result<u128> {
    let bad = || Error::new(ErrorCode::Usage, format!("{s} is not an amount: use whole units such as 25 or 0.5"));
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() && frac.is_empty() || !int.chars().chain(frac.chars()).all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    if frac.len() > decimals as usize {
        return Err(Error::new(ErrorCode::Usage, format!("{s} has more than {decimals} decimals")));
    }
    let scale = 10u128.checked_pow(decimals as u32).ok_or_else(bad)?;
    let int: u128 = if int.is_empty() { 0 } else { int.parse().map_err(|_| bad())? };
    let frac: u128 = if frac.is_empty() { 0 } else { format!("{frac:0<width$}", width = decimals as usize).parse().map_err(|_| bad())? };
    int.checked_mul(scale).and_then(|v| v.checked_add(frac)).ok_or_else(bad)
}

/// Base units to whole units, without trailing zeros.
pub fn format_amount(raw: u128, decimals: u8) -> String {
    let scale = 10u128.pow(decimals as u32);
    let (int, frac) = (raw / scale, raw % scale);
    if frac == 0 {
        return int.to_string();
    }
    let frac = format!("{frac:0>width$}", width = decimals as usize);
    format!("{int}.{}", frac.trim_end_matches('0'))
}

/// The 32 bytes of a normalized address (0x and 64 hex digits).
pub fn address_bytes(address: &str) -> [u8; 32] {
    let hex = address.trim_start_matches("0x");
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(2 * i..2 * i + 2).unwrap_or("00"), 16).unwrap_or(0);
    }
    out
}

/// Standard base64 with padding, as protobuf-JSON encodes `bytes`.
pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts() {
        assert_eq!(parse_amount("25", 9).unwrap(), 25_000_000_000);
        assert_eq!(parse_amount("0.5", 9).unwrap(), 500_000_000);
        assert_eq!(parse_amount(".5", 9).unwrap(), 500_000_000);
        assert!(parse_amount("1.0000000001", 9).is_err());
        assert!(parse_amount("1e3", 9).is_err());
        assert!(parse_amount("", 9).is_err());
        assert_eq!(format_amount(25_000_000_000, 9), "25");
        assert_eq!(format_amount(500_000_000, 9), "0.5");
        assert_eq!(format_amount(1, 9), "0.000000001");
    }

    #[test]
    fn encodes() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&1_000_000_000u64.to_le_bytes()), "AMqaOwAAAAA=");
        let a = address_bytes(&format!("0x{}c5", "0".repeat(62)));
        assert_eq!(a[31], 0xc5);
        assert!(a[..31].iter().all(|b| *b == 0));
    }
}
