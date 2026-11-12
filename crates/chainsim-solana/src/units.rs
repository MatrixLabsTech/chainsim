//! Amounts in whole units and base units, and base58 addresses.

use chainsim_core::{Error, ErrorCode, Result};

/// Whole units (`25` or `0.5`) to base units at `decimals`. Solana balances are u64.
pub fn parse_amount(s: &str, decimals: u8) -> Result<u64> {
    let bad = || Error::new(ErrorCode::Usage, format!("{s} is not an amount: use whole units such as 25 or 0.5"));
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() && frac.is_empty() || !int.chars().chain(frac.chars()).all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    if frac.len() > decimals as usize {
        return Err(Error::new(ErrorCode::Usage, format!("{s} has more than {decimals} decimals")));
    }
    let too_large = || Error::new(ErrorCode::Usage, format!("{s} is more than a u64 of base units"));
    let scale = 10u128.checked_pow(decimals as u32).ok_or_else(too_large)?;
    let int: u128 = if int.is_empty() { 0 } else { int.parse().map_err(|_| too_large())? };
    let frac: u128 = if frac.is_empty() { 0 } else { format!("{frac:0<width$}", width = decimals as usize).parse().map_err(|_| bad())? };
    let raw = int.checked_mul(scale).and_then(|v| v.checked_add(frac)).ok_or_else(too_large)?;
    u64::try_from(raw).map_err(|_| too_large())
}

/// Base units to whole units, without trailing zeros.
pub fn format_amount(raw: u64, decimals: u8) -> String {
    let scale = 10u128.pow(decimals as u32);
    let (int, frac) = (raw as u128 / scale, raw as u128 % scale);
    if frac == 0 {
        return int.to_string();
    }
    let frac = format!("{frac:0>width$}", width = decimals as usize);
    format!("{int}.{}", frac.trim_end_matches('0'))
}

const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// Base58 text to bytes, or None when a character is outside the alphabet.
pub fn base58_decode(s: &str) -> Option<Vec<u8>> {
    // Big-endian base-256 digits, grown one base58 digit at a time.
    let mut bytes: Vec<u8> = Vec::new();
    for c in s.bytes() {
        let mut carry = ALPHABET.iter().position(|a| *a == c)? as u32;
        for b in bytes.iter_mut().rev() {
            carry += *b as u32 * 58;
            *b = carry as u8;
            carry >>= 8;
        }
        while carry > 0 {
            bytes.insert(0, carry as u8);
            carry >>= 8;
        }
    }
    let zeros = s.bytes().take_while(|c| *c == b'1').count();
    let mut out = vec![0u8; zeros];
    out.extend(bytes);
    Some(out)
}

/// Bytes to base58 text.
pub fn base58_encode(bytes: &[u8]) -> String {
    let mut digits: Vec<u8> = Vec::new();
    for byte in bytes {
        let mut carry = *byte as u32;
        for d in digits.iter_mut().rev() {
            carry += (*d as u32) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.insert(0, (carry % 58) as u8);
            carry /= 58;
        }
    }
    let zeros = bytes.iter().take_while(|b| **b == 0).count();
    let mut out = "1".repeat(zeros);
    out.extend(digits.iter().map(|d| ALPHABET[*d as usize] as char));
    out
}

/// A Solana address: base58 of 32 bytes. Returns it as given.
pub fn parse_address(s: &str) -> Result<String> {
    match base58_decode(s) {
        Some(bytes) if bytes.len() == 32 => Ok(s.to_string()),
        _ => Err(Error::new(ErrorCode::Usage, format!("bad address {s:?}: use a base58 Solana address of 32 bytes"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts() {
        assert_eq!(parse_amount("5", 9).unwrap(), 5_000_000_000);
        assert_eq!(parse_amount("0.5", 9).unwrap(), 500_000_000);
        assert_eq!(parse_amount(".25", 6).unwrap(), 250_000);
        assert_eq!(parse_amount("18446744073.709551615", 9).unwrap(), u64::MAX);
        assert_eq!(parse_amount("18446744073.709551616", 9).unwrap_err().code, ErrorCode::Usage);
        assert_eq!(parse_amount("1.1234567", 6).unwrap_err().code, ErrorCode::Usage);
        assert_eq!(parse_amount("1e3", 6).unwrap_err().code, ErrorCode::Usage);
        assert_eq!(parse_amount("", 6).unwrap_err().code, ErrorCode::Usage);
        assert_eq!(format_amount(1_000_000_000, 6), "1000");
        assert_eq!(format_amount(1_500_000, 6), "1.5");
        assert_eq!(format_amount(u64::MAX, 9), "18446744073.709551615");
    }

    #[test]
    fn base58_round_trips() {
        let system = "11111111111111111111111111111111";
        assert_eq!(base58_decode(system).unwrap(), vec![0u8; 32]);
        assert_eq!(base58_encode(&[0u8; 32]), system);
        for a in ["EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", "SysvarC1ock11111111111111111111111111111111"] {
            let bytes = base58_decode(a).unwrap();
            assert_eq!(bytes.len(), 32, "{a}");
            assert_eq!(base58_encode(&bytes), a);
        }
        // The Token program id, byte for byte.
        assert_eq!(base58_decode("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()[..4], [6, 221, 246, 225]);
        assert_eq!(base58_decode("0OIl"), None);
    }

    #[test]
    fn addresses() {
        assert!(parse_address("US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELGZ").is_ok());
        for bad in ["", "0xdeadbeef", "abc", "US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELGZ1111"] {
            assert_eq!(parse_address(bad).unwrap_err().code, ErrorCode::Usage, "{bad}");
        }
    }
}
