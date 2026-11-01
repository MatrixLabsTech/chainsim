//! Amounts in whole units and base units.

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

/// Whole units to base units that fit a Move u64.
pub fn parse_u64_amount(s: &str, decimals: u8, symbol: &str) -> Result<u64> {
    u64::try_from(parse_amount(s, decimals)?).map_err(|_| Error::new(ErrorCode::Usage, format!("{s} {symbol} is more than a u64 of base units")))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts() {
        assert_eq!(parse_amount("10", 8).unwrap(), 1_000_000_000);
        assert_eq!(parse_amount("0.1", 8).unwrap(), 10_000_000);
        assert_eq!(format_amount(1_000_000_000, 6), "1000");
        assert_eq!(format_amount(1_500_000, 6), "1.5");
        assert!(parse_amount("1.123456789", 8).is_err());
        assert!(parse_u64_amount("1000000000000", 8, "APT").is_err());
    }
}
