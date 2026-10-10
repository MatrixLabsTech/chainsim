//! Decimal amounts in whole units, base units, and the few ABI calls chainsim
//! makes.

use alloy_primitives::{Address, U256};
use chainsim_core::{Error, ErrorCode, Result};

/// `25` or `0.5` in whole units to base units with `decimals`.
pub fn parse_amount(s: &str, decimals: u8) -> Result<U256> {
    let bad = || Error::new(ErrorCode::Usage, format!("bad amount {s:?}: use a decimal number of whole units, such as 25 or 0.5"));
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if (int.is_empty() && frac.is_empty()) || !int.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
        return Err(bad());
    }
    let frac = frac.trim_end_matches('0');
    if frac.len() > decimals as usize {
        return Err(Error::new(ErrorCode::Usage, format!("amount {s} has more than {decimals} decimals")));
    }
    let digits = format!("{}{}{}", if int.is_empty() { "0" } else { int }, frac, "0".repeat(decimals as usize - frac.len()));
    U256::from_str_radix(&digits, 10).map_err(|_| bad())
}

/// Base units to whole units, without trailing zeros.
pub fn format_amount(raw: U256, decimals: u8) -> String {
    let s = raw.to_string();
    let d = decimals as usize;
    if d == 0 {
        return s;
    }
    let s = format!("{s:0>width$}", width = d + 1);
    let (int, frac) = s.split_at(s.len() - d);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        int.to_string()
    } else {
        format!("{int}.{frac}")
    }
}

pub fn parse_address(s: &str) -> Result<Address> {
    s.parse::<Address>().map_err(|_| Error::new(ErrorCode::Usage, format!("bad address {s:?}: use 0x and 40 hex digits")))
}

/// A quantity in JSON-RPC hex.
pub fn qty(v: U256) -> String {
    format!("{v:#x}")
}

/// Parses a JSON-RPC hex quantity.
pub fn parse_qty(v: &serde_json::Value) -> Result<U256> {
    let s = v.as_str().ok_or_else(|| Error::new(ErrorCode::RpcError, format!("expected a hex quantity, got {v}")))?;
    let hex = s.strip_prefix("0x").unwrap_or(s);
    if hex.is_empty() {
        return Ok(U256::ZERO);
    }
    U256::from_str_radix(hex, 16).map_err(|_| Error::new(ErrorCode::RpcError, format!("bad hex quantity {s}")))
}

fn word_address(a: Address) -> String {
    format!("{:0>64}", alloy_primitives::hex::encode(a.as_slice()))
}

fn word(v: U256) -> String {
    format!("{v:064x}")
}

pub fn balance_of(owner: Address) -> String {
    format!("0x70a08231{}", word_address(owner))
}

pub fn transfer(to: Address, amount: U256) -> String {
    format!("0xa9059cbb{}{}", word_address(to), word(amount))
}

pub fn approve(spender: Address, amount: U256) -> String {
    format!("0x095ea7b3{}{}", word_address(spender), word(amount))
}

pub const DECIMALS: &str = "0x313ce567";

/// The 4-byte calldata of a function with no arguments: `selector("arbBlockNumber()")`.
pub fn selector(signature: &str) -> String {
    format!("0x{}", alloy_primitives::hex::encode(&alloy_primitives::keccak256(signature.as_bytes())[..4]))
}

/// The storage slot of `mapping(address => uint256)` at `slot`: Solidity
/// hashes key then slot, Vyper hashes slot then key.
pub fn mapping_slot(owner: Address, slot: u64, vyper: bool) -> String {
    let key = alloy_primitives::hex::decode(word_address(owner)).unwrap();
    let pos = alloy_primitives::hex::decode(word(U256::from(slot))).unwrap();
    let data = if vyper { [pos, key].concat() } else { [key, pos].concat() };
    format!("{}", alloy_primitives::keccak256(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selectors() {
        assert_eq!(selector("decimals()"), DECIMALS);
        assert_eq!(selector("arbBlockNumber()"), "0xa3b1b31d");
    }

    #[test]
    fn amounts_round_trip() {
        assert_eq!(parse_amount("25", 6).unwrap(), U256::from(25_000_000u64));
        assert_eq!(parse_amount("0.5", 18).unwrap(), U256::from(500_000_000_000_000_000u64));
        assert_eq!(parse_amount("1.50", 6).unwrap(), U256::from(1_500_000u64));
        assert_eq!(parse_amount(".5", 1).unwrap(), U256::from(5u64));
        assert!(parse_amount("1.0000001", 6).is_err());
        assert!(parse_amount("-1", 6).is_err());
        assert!(parse_amount("1e6", 6).is_err());
        assert!(parse_amount("", 6).is_err());
        assert_eq!(format_amount(U256::from(25_000_000u64), 6), "25");
        assert_eq!(format_amount(U256::from(1_500_000u64), 6), "1.5");
        assert_eq!(format_amount(U256::from(5u64), 6), "0.000005");
        assert_eq!(format_amount(U256::ZERO, 18), "0");
    }

    #[test]
    fn abi() {
        let a: Address = "0x00000000000000000000000000000000000000aa".parse().unwrap();
        assert_eq!(balance_of(a), format!("0x70a08231{}aa", "0".repeat(62)));
        assert_eq!(transfer(a, U256::from(1u8)).len(), 2 + 8 + 128);
        assert_eq!(qty(U256::from(255u16)), "0xff");
        assert_eq!(parse_qty(&serde_json::json!("0x0")).unwrap(), U256::ZERO);
    }

    #[test]
    fn mapping_slots() {
        // keccak256(pad(0) . pad(0)): a well-known value.
        assert_eq!(mapping_slot(Address::ZERO, 0, false), "0xad3228b676f7d3cd4284a5443f17f1962b36e491b30a40b2405849e597ba5fb5");
    }
}
