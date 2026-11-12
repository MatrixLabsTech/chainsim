//! `set <name> scope`: the Scope prices that Kamino Lend reads, written at the
//! fork's clock. Kamino refuses a price older than the reserve allows, so a
//! fork whose clock moved needs its Scope entries stamped again.
//!
//! Layouts, measured on Solana Mainnet on 2026-11-12.
//!
//! A Scope prices account is 28712 bytes: an 8 byte discriminator, the key of
//! its mappings account, then 512 entries of 56 bytes. An entry is the price
//! value (u64), its exponent (u64, the price is value / 10^exponent), the slot
//! of its last update (u64), the Unix time of its last update (u64), and 24
//! bytes this module leaves as they are.
//!
//! A Kamino Lend reserve names the prices account at byte 5112, a price chain
//! of four u16 entry indices at 5144 and a TWAP chain of four at 5152. Index
//! 65535 is unused, and the price is the product of the chain's entries. Before
//! them come the token's name (32 bytes at 5032), a price heuristic (lower,
//! upper and exponent as u64 at 5064), the largest allowed divergence of the
//! price from the TWAP in basis points (5088), and the largest age in seconds
//! of the price (5096) and of the TWAP (5104).

use std::collections::BTreeMap;

use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

use crate::mirror::{account_discriminator, key_at, parse_obligation, read, write, Key, KLEND, RESERVE_LEN};
use crate::rpc::Rpc;
use crate::time::clock;
use crate::units::{base58_encode, format_amount, parse_address, parse_amount};
use crate::SolanaAdapter;

/// The Scope oracle program, which owns the prices accounts.
pub const SCOPE: &str = "HFn8GnPADiny6XqUoWE8uRPPxb29ikn4yTuPa9MF2fWJ";

const PRICES_LEN: usize = 28712;
const ENTRIES: usize = 40;
const ENTRY_LEN: usize = 56;
const ENTRY_COUNT: u16 = 512;
const UNUSED: u16 = u16::MAX;

const RESERVE_TOKEN_NAME: usize = 5032;
const RESERVE_HEURISTIC: usize = 5064;
const RESERVE_MAX_TWAP_DIVERGENCE_BPS: usize = 5088;
const RESERVE_MAX_AGE_PRICE: usize = 5096;
const RESERVE_MAX_AGE_TWAP: usize = 5104;
const RESERVE_SCOPE_PRICES: usize = 5112;
const RESERVE_PRICE_CHAIN: usize = 5144;
const RESERVE_TWAP_CHAIN: usize = 5152;

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().expect("8 bytes"))
}

/// The entry indices of a chain that are in use.
fn chain(data: &[u8], offset: usize) -> Vec<u16> {
    (0..4).map(|i| u16::from_le_bytes([data[offset + 2 * i], data[offset + 2 * i + 1]])).filter(|i| *i != UNUSED).collect()
}

/// How a Kamino Lend reserve reads its price from Scope.
#[derive(Debug, PartialEq)]
pub struct ReserveOracle {
    pub token: String,
    /// The Scope prices account. All zero when the reserve does not use Scope.
    pub prices: Key,
    pub price_chain: Vec<u16>,
    pub twap_chain: Vec<u16>,
    /// Lower bound, upper bound and their exponent. Kamino refuses a price outside them.
    pub heuristic: (u64, u64, u64),
    pub max_twap_divergence_bps: u64,
    pub max_age_price: u64,
    pub max_age_twap: u64,
}

/// Reads a reserve account, or None when the bytes are not one.
pub fn reserve_oracle(data: &[u8]) -> Option<ReserveOracle> {
    if data.len() != RESERVE_LEN || data[..8] != account_discriminator("Reserve") {
        return None;
    }
    let name = &data[RESERVE_TOKEN_NAME..RESERVE_TOKEN_NAME + 32];
    Some(ReserveOracle {
        token: String::from_utf8_lossy(name).trim_end_matches('\0').to_string(),
        prices: key_at(data, RESERVE_SCOPE_PRICES),
        price_chain: chain(data, RESERVE_PRICE_CHAIN),
        twap_chain: chain(data, RESERVE_TWAP_CHAIN),
        heuristic: (u64_at(data, RESERVE_HEURISTIC), u64_at(data, RESERVE_HEURISTIC + 8), u64_at(data, RESERVE_HEURISTIC + 16)),
        max_twap_divergence_bps: u64_at(data, RESERVE_MAX_TWAP_DIVERGENCE_BPS),
        max_age_price: u64_at(data, RESERVE_MAX_AGE_PRICE),
        max_age_twap: u64_at(data, RESERVE_MAX_AGE_TWAP),
    })
}

impl ReserveOracle {
    /// Whether a price lies inside the heuristic bounds. A reserve with no upper bound has none.
    pub fn within_heuristic(&self, value: u64, exponent: u64) -> bool {
        let (lower, upper, exp) = self.heuristic;
        if upper == 0 {
            return true;
        }
        let price = value as f64 / 10f64.powi(exponent as i32);
        let scale = 10f64.powi(exp as i32);
        price >= lower as f64 / scale && price <= upper as f64 / scale
    }
}

/// True for the bytes of a Scope prices account.
pub fn is_prices(data: &[u8]) -> bool {
    data.len() == PRICES_LEN && data[..8] == account_discriminator("OraclePrices")
}

/// One entry of a Scope prices account.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Entry {
    pub value: u64,
    pub exponent: u64,
    pub slot: u64,
    pub timestamp: u64,
}

fn entry_offset(index: u16) -> usize {
    ENTRIES + ENTRY_LEN * index as usize
}

pub fn entry(prices: &[u8], index: u16) -> Entry {
    let at = entry_offset(index);
    Entry { value: u64_at(prices, at), exponent: u64_at(prices, at + 8), slot: u64_at(prices, at + 16), timestamp: u64_at(prices, at + 24) }
}

/// Writes an entry's four fields and leaves its last 24 bytes.
pub fn set_entry(prices: &mut [u8], index: u16, e: &Entry) {
    let at = entry_offset(index);
    for (i, field) in [e.value, e.exponent, e.slot, e.timestamp].into_iter().enumerate() {
        prices[at + 8 * i..at + 8 * i + 8].copy_from_slice(&field.to_le_bytes());
    }
}

/// A price such as 2.5 as a value and an exponent: the entry's own exponent
/// when the value fits a u64, else the largest smaller one that does.
pub fn price_value(price: &str, exponent: u64) -> Result<(u64, u64)> {
    let first = exponent.min(18) as u8;
    for exp in (0..=first).rev() {
        match parse_amount(price, exp) {
            Ok(0) => return Err(Error::new(ErrorCode::Usage, "the price must be more than 0")),
            Ok(value) => return Ok((value, exp as u64)),
            Err(_) => continue,
        }
    }
    // No exponent fits: say why the entry's own does not.
    parse_amount(price, first).map(|value| (value, first as u64))
}

fn entry_json(prices: &str, index: u16, role: &str, before: &Entry, after: &Entry) -> Value {
    json!({
        "prices": prices,
        "index": index,
        "role": role,
        "price": format_amount(after.value, after.exponent as u8),
        "previousPrice": format_amount(before.value, before.exponent.min(38) as u8),
        "value": after.value.to_string(),
        "exponent": after.exponent,
        "timestamp": after.timestamp,
        "previousTimestamp": before.timestamp,
        "slot": after.slot,
    })
}

fn raw_write() -> &'static str {
    "chainsim call <name> surfnet_setAccount '[\"<address>\", {\"data\": \"<hex>\", \"owner\": \"<program>\"}]' writes any account"
}

/// A Kamino Lend reserve on the fork and how it reads Scope.
async fn reserve(rpc: &Rpc<'_>, address: &str) -> Result<ReserveOracle> {
    read(rpc, address)
        .await?
        .filter(|a| a.owner == KLEND)
        .and_then(|a| reserve_oracle(&a.data))
        .ok_or_else(|| Error::new(ErrorCode::Usage, format!("{address} is not a Kamino Lend reserve")).next("pass a reserve address, such as one from reserves in the result of set <name> mirror kamino"))
}

/// A Scope prices account on the fork.
async fn prices(rpc: &Rpc<'_>, address: &str) -> Result<crate::mirror::Account> {
    read(rpc, address).await?.filter(|a| a.owner == SCOPE && is_prices(&a.data)).ok_or_else(|| Error::unsupported(&format!("the price account {address}, which is not a Scope prices account,"), raw_write()))
}

/// Sets the price Kamino reads for one reserve: the entry of its price chain
/// and the entry of its TWAP chain, stamped with the fork's clock.
pub async fn price(adapter: &SolanaAdapter, name: &str, reserve_address: &str, price: &str) -> Result<Value> {
    let (handle, profile) = adapter.open(name).await?;
    let rpc = adapter.rpc(&handle, &profile);
    let reserve_address = parse_address(reserve_address)?;
    let oracle = reserve(&rpc, &reserve_address).await?;
    if oracle.prices == [0; 32] || oracle.price_chain.is_empty() {
        return Err(Error::unsupported(&format!("set scope price on {reserve_address}, which does not read its price from Scope,"), raw_write()));
    }
    if oracle.price_chain.len() > 1 || oracle.twap_chain.len() > 1 {
        return Err(Error::unsupported(
            &format!("set scope price on {reserve_address}, whose price is a product of Scope entries {:?} with TWAP entries {:?},", oracle.price_chain, oracle.twap_chain),
            format!("chainsim set {name} scope refresh {reserve_address} stamps those entries with the clock and keeps their values"),
        ));
    }
    let address = base58_encode(&oracle.prices);
    let mut account = prices(&rpc, &address).await?;
    let (slot, now) = clock(&rpc).await?;
    let mut entries = Vec::new();
    let mut written = None;
    for (role, index) in oracle.price_chain.iter().map(|i| ("price", *i)).chain(oracle.twap_chain.iter().map(|i| ("twap", *i))) {
        let before = entry(&account.data, index);
        let (value, exponent) = price_value(price, before.exponent)?;
        let after = Entry { value, exponent, slot, timestamp: now };
        set_entry(&mut account.data, index, &after);
        entries.push(entry_json(&address, index, role, &before, &after));
        written.get_or_insert((value, exponent));
    }
    write(&rpc, &address, &account).await?;
    let (value, exponent) = written.expect("a price entry");
    let (lower, upper, exp) = oracle.heuristic;
    let shown = format_amount(value, exponent as u8);
    adapter.record(name, json!({ "kind": "scope-price", "reserve": reserve_address, "token": oracle.token, "price": shown })).await?;
    Ok(json!({
        "kind": "scope-price",
        "reserve": reserve_address,
        "token": oracle.token,
        "price": shown,
        "entries": entries,
        "heuristic": if upper == 0 { Value::Null } else { json!({ "lower": format_amount(lower, exp as u8), "upper": format_amount(upper, exp as u8) }) },
        "withinHeuristic": oracle.within_heuristic(value, exponent),
        "maxTwapDivergenceBps": oracle.max_twap_divergence_bps,
        "maxAgeSeconds": { "price": oracle.max_age_price, "twap": oracle.max_age_twap },
        "timestamp": now,
        "slot": slot,
        "note": chainsim_core::mods::NOTE,
    }))
}

/// Stamps every Scope entry that the reserves use with the fork's clock and
/// keeps each value. An address is a Kamino Lend reserve, or an obligation,
/// which stands for the reserves it holds.
pub async fn refresh(adapter: &SolanaAdapter, name: &str, addresses: &[String]) -> Result<Value> {
    let (handle, profile) = adapter.open(name).await?;
    let rpc = adapter.rpc(&handle, &profile);
    let mut reserves: Vec<String> = Vec::new();
    for address in addresses {
        let address = parse_address(address)?;
        let account = read(&rpc, &address).await?.filter(|a| a.owner == KLEND);
        match account.as_ref().and_then(|a| parse_obligation(&a.data)) {
            Some(o) => reserves.extend(o.deposits.iter().chain(&o.borrows).map(|k| base58_encode(k))),
            None if account.as_ref().is_some_and(|a| reserve_oracle(&a.data).is_some()) => reserves.push(address),
            None => return Err(Error::new(ErrorCode::Usage, format!("{address} is not a Kamino Lend reserve or obligation")).next("pass a reserve or an obligation, such as obligation in the result of set <name> mirror kamino")),
        }
    }
    reserves.sort();
    reserves.dedup();

    // The entries to stamp, by prices account.
    let mut wanted: BTreeMap<String, Vec<(u16, &'static str)>> = BTreeMap::new();
    let mut skipped = Vec::new();
    for address in &reserves {
        let oracle = reserve(&rpc, address).await?;
        if oracle.prices == [0; 32] || oracle.price_chain.is_empty() {
            skipped.push(address.clone());
            continue;
        }
        let list = wanted.entry(base58_encode(&oracle.prices)).or_default();
        for (role, index) in oracle.price_chain.iter().map(|i| ("price", *i)).chain(oracle.twap_chain.iter().map(|i| ("twap", *i))) {
            if index < ENTRY_COUNT && !list.iter().any(|(i, _)| *i == index) {
                list.push((index, role));
            }
        }
    }
    let (slot, now) = clock(&rpc).await?;
    let mut entries = Vec::new();
    for (address, list) in &wanted {
        let mut account = prices(&rpc, address).await?;
        for (index, role) in list {
            let before = entry(&account.data, *index);
            let after = Entry { slot, timestamp: now, ..before };
            set_entry(&mut account.data, *index, &after);
            entries.push(entry_json(address, *index, role, &before, &after));
        }
        write(&rpc, address, &account).await?;
    }
    adapter.record(name, json!({ "kind": "scope-refresh", "reserves": reserves.len(), "entries": entries.len(), "timestamp": now })).await?;
    Ok(json!({
        "kind": "scope-refresh",
        "reserves": reserves,
        "notScope": skipped,
        "entries": entries,
        "timestamp": now,
        "slot": slot,
        "note": chainsim_core::mods::NOTE,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mirror::key;

    /// The discriminators the accounts carry on Solana Mainnet.
    #[test]
    fn discriminators_match_mainnet() {
        assert_eq!(account_discriminator("OraclePrices"), [89, 128, 118, 221, 6, 72, 180, 146]);
        assert!(key(SCOPE).is_some());
    }

    fn reserve_bytes() -> Vec<u8> {
        let mut data = vec![0u8; RESERVE_LEN];
        data[..8].copy_from_slice(&account_discriminator("Reserve"));
        data[5032..5035].copy_from_slice(b"JLP");
        for (i, v) in [2u64, 6, 0, 1000, 250, 300].into_iter().enumerate() {
            data[5064 + 8 * i..5072 + 8 * i].copy_from_slice(&v.to_le_bytes());
        }
        data[5112..5144].fill(7);
        for (i, v) in [416u16, UNUSED, UNUSED, UNUSED, 501, UNUSED, UNUSED, UNUSED].into_iter().enumerate() {
            data[5144 + 2 * i..5146 + 2 * i].copy_from_slice(&v.to_le_bytes());
        }
        data
    }

    /// The JLP reserve of Solana Mainnet as read on 2026-11-12, field by field.
    #[test]
    fn a_reserve_names_its_scope_entries() {
        let data = reserve_bytes();
        let o = reserve_oracle(&data).unwrap();
        assert_eq!(o, ReserveOracle { token: "JLP".into(), prices: [7; 32], price_chain: vec![416], twap_chain: vec![501], heuristic: (2, 6, 0), max_twap_divergence_bps: 1000, max_age_price: 250, max_age_twap: 300 });
        assert!(o.within_heuristic(250, 2) && o.within_heuristic(2, 0) && o.within_heuristic(6, 0));
        assert!(!o.within_heuristic(199, 2) && !o.within_heuristic(601, 2));
        // No upper bound: no heuristic.
        assert!(ReserveOracle { heuristic: (0, 0, 0), ..o }.within_heuristic(1, 9));
        assert!(reserve_oracle(&data[1..]).is_none());
    }

    #[test]
    fn an_entry_is_written_in_place() {
        let mut prices: Vec<u8> = (0..PRICES_LEN).map(|i| (i % 253) as u8).collect();
        prices[..8].copy_from_slice(&account_discriminator("OraclePrices"));
        assert!(is_prices(&prices) && !is_prices(&prices[1..]));
        let before = prices.clone();
        let e = Entry { value: 250_000_000, exponent: 8, slot: 9, timestamp: 1_794_000_000 };
        set_entry(&mut prices, 416, &e);
        assert_eq!(entry(&prices, 416), e);
        // Only the entry's first 32 bytes changed: 40 + 56 x 416 = 23336.
        let changed: Vec<usize> = (0..PRICES_LEN).filter(|i| prices[*i] != before[*i]).collect();
        assert!(changed.iter().all(|i| (23336..23368).contains(i)) && !changed.is_empty(), "{changed:?}");
        assert_eq!(entry(&prices, 415), entry(&before, 415));
        assert_eq!(entry(&prices, 511).value, u64_at(&before, 40 + 56 * 511));
    }

    #[test]
    fn a_price_keeps_the_entry_exponent_when_it_fits() {
        assert_eq!(price_value("2.5", 8).unwrap(), (250_000_000, 8));
        assert_eq!(price_value("3.99", 17).unwrap(), (399_000_000_000_000_000, 17));
        // 200 x 10^17 is more than a u64, so the exponent goes down.
        assert_eq!(price_value("200", 17).unwrap(), (2_000_000_000_000_000_000, 16));
        assert_eq!(price_value("0.5", 30).unwrap(), (500_000_000_000_000_000, 18));
        assert!(price_value("0", 8).is_err() && price_value("-1", 8).is_err() && price_value("x", 8).is_err());
        assert!(price_value("1.123456789", 8).unwrap_err().message.contains("more than 8 decimals"));
    }
}
