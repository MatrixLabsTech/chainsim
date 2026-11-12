//! `set price`: a Pyth price account written with a price, a confidence and
//! a publish time equal to the fork's Clock, so a program that checks the
//! age of a price accepts it after the clock has moved.

use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

use crate::rpc::Rpc;
use crate::send::base64_decode;
use crate::time::clock;
use crate::units::{format_amount, parse_address, parse_amount};
use crate::SolanaAdapter;

/// The Pyth Solana receiver program, which owns every price account.
pub const RECEIVER: &str = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ";
/// The first 8 bytes of a `PriceUpdateV2` account.
const DISCRIMINATOR: [u8; 8] = [0x22, 0xf1, 0x23, 0x63, 0x9d, 0x7e, 0xf4, 0xcd];
/// The verification level byte of a fully verified update. A partly verified
/// one has one more byte there, so the fields behind it sit elsewhere.
const FULL: u8 = 1;
const VERIFICATION: usize = 40;
const FEED_ID: usize = 41;
const PRICE: usize = 73;
const CONF: usize = 81;
const EXPONENT: usize = 89;
const PUBLISH_TIME: usize = 93;
const PREV_PUBLISH_TIME: usize = 101;
const EMA_PRICE: usize = 109;
const EMA_CONF: usize = 117;
const POSTED_SLOT: usize = 125;
const LEN: usize = 133;

/// The price feed accounts chainsim knows by symbol: the accounts Pyth keeps
/// updated on Solana Mainnet for these feeds (shard 0 of its push oracle).
pub const FEEDS: &[(&str, &str)] = &[
    ("SOL/USD", "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE"),
    ("BTC/USD", "4cSM2e6rvbGQUFiJbqytoVMi5GgghSMr8LwVrT9VPSPo"),
    ("ETH/USD", "42amVS4KgzR9rA28tkVYqVXjq9Qa8dcZQMbH5EYFX6XC"),
    ("USDC/USD", "Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX"),
];

/// A feed symbol or a price account address to (symbol, or the address of a feed chainsim has no symbol for, and the account).
pub fn resolve(feed: &str) -> Result<(String, String)> {
    if let Some((symbol, account)) = FEEDS.iter().find(|(s, a)| s.eq_ignore_ascii_case(feed) || *a == feed) {
        return Ok((symbol.to_string(), account.to_string()));
    }
    match parse_address(feed) {
        Ok(a) => Ok((a.clone(), a)),
        Err(_) => {
            let symbols: Vec<_> = FEEDS.iter().map(|(s, _)| *s).collect();
            Err(Error::new(ErrorCode::Usage, format!("no price feed {feed}: use one of {} or the address of a Pyth price account", symbols.join(", "))))
        }
    }
}

/// The fields of a price account that `set price` reads and writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Price {
    pub price: i64,
    pub conf: u64,
    pub exponent: i32,
    pub publish_time: i64,
}

fn le<const N: usize>(data: &[u8], at: usize) -> [u8; N] {
    data[at..at + N].try_into().expect("the length was checked")
}

/// Reads a fully verified `PriceUpdateV2`, or None when the bytes are not one.
pub fn parse(data: &[u8]) -> Option<Price> {
    if data.len() < LEN || data[..8] != DISCRIMINATOR || data[VERIFICATION] != FULL {
        return None;
    }
    Some(Price { price: i64::from_le_bytes(le(data, PRICE)), conf: u64::from_le_bytes(le(data, CONF)), exponent: i32::from_le_bytes(le(data, EXPONENT)), publish_time: i64::from_le_bytes(le(data, PUBLISH_TIME)) })
}

/// The feed id of a price account, as hex.
pub fn feed_id(data: &[u8]) -> String {
    hex(&data[FEED_ID..FEED_ID + 32])
}

/// Writes a price into the account bytes: the price and the confidence, the
/// publish time, the old publish time as the previous one, the slot it was
/// posted at, and the EMA price and confidence set to the same values.
pub fn write(data: &mut [u8], price: i64, conf: u64, publish_time: i64, slot: u64) {
    let previous: [u8; 8] = le(data, PUBLISH_TIME);
    data[PRICE..PRICE + 8].copy_from_slice(&price.to_le_bytes());
    data[CONF..CONF + 8].copy_from_slice(&conf.to_le_bytes());
    data[PUBLISH_TIME..PUBLISH_TIME + 8].copy_from_slice(&publish_time.to_le_bytes());
    data[PREV_PUBLISH_TIME..PREV_PUBLISH_TIME + 8].copy_from_slice(&previous);
    data[EMA_PRICE..EMA_PRICE + 8].copy_from_slice(&price.to_le_bytes());
    data[EMA_CONF..EMA_CONF + 8].copy_from_slice(&conf.to_le_bytes());
    data[POSTED_SLOT..POSTED_SLOT + 8].copy_from_slice(&slot.to_le_bytes());
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A decimal amount such as `82.5` to the feed's integer at `exponent`.
pub fn to_raw(amount: &str, exponent: i32) -> Result<u64> {
    let decimals = u8::try_from(-exponent).map_err(|_| Error::unsupported(&format!("a price feed with exponent {exponent}"), "chainsim call <name> surfnet_setAccount writes the account as hex"))?;
    parse_amount(amount, decimals)
}

/// A feed integer at `exponent` to a decimal amount.
pub fn from_raw(raw: u64, exponent: i32) -> String {
    format_amount(raw, u8::try_from(-exponent).unwrap_or(0))
}

/// The bytes of a price account on the fork. An account that is not a fully
/// verified Pyth price account exits 3.
pub async fn read(rpc: &Rpc<'_>, account: &str) -> Result<Vec<u8>> {
    let not_a_feed = |what: &str| Error::unsupported(&format!("{account} {what}, so set price"), "pass a feed symbol or the address of a Pyth price account (PriceUpdateV2)");
    let v = rpc.call("getAccountInfo", json!([account, { "encoding": "base64", "commitment": "processed" }])).await?;
    let value = v.get("value").filter(|a| !a.is_null()).ok_or_else(|| not_a_feed("does not exist"))?;
    if value["owner"] != RECEIVER {
        return Err(not_a_feed("is not owned by the Pyth receiver program"));
    }
    let data = value["data"][0].as_str().and_then(base64_decode).unwrap_or_default();
    if parse(&data).is_none() {
        return Err(not_a_feed("is not a fully verified Pyth price account"));
    }
    Ok(data)
}

/// Writes `data` to the account and reads it back. Returns the price read.
pub async fn store(rpc: &Rpc<'_>, account: &str, data: &[u8]) -> Result<Price> {
    rpc.call("surfnet_setAccount", json!([account, { "data": hex(data) }])).await?;
    let back = read(rpc, account).await?;
    if back != data {
        return Err(Error::new(ErrorCode::ReadbackMismatch, format!("price account {account} does not read back as written")));
    }
    Ok(parse(&back).expect("read checked it"))
}

pub async fn set_price(adapter: &SolanaAdapter, name: &str, feed: &str, price: &str, conf: Option<&str>) -> Result<Value> {
    let (feed, account) = resolve(feed)?;
    let (handle, profile) = adapter.open(name).await?;
    let rpc = adapter.rpc(&handle, &profile);
    let mut data = read(&rpc, &account).await?;
    let old = parse(&data).expect("read checked it");
    let raw = to_raw(price, old.exponent)?;
    let raw_price = i64::try_from(raw).map_err(|_| Error::new(ErrorCode::Usage, format!("{price} is too large for a price")))?;
    let raw_conf = match conf {
        Some(c) => to_raw(c, old.exponent)?,
        None => old.conf,
    };
    let (slot, now) = clock(&rpc).await?;
    write(&mut data, raw_price, raw_conf, now as i64, slot);
    let read = store(&rpc, &account, &data).await?;
    let (amount, conf) = (from_raw(read.price as u64, read.exponent), from_raw(read.conf, read.exponent));
    adapter.record(name, json!({ "kind": "set-price", "feed": feed, "account": account, "price": amount, "conf": conf, "publishTime": read.publish_time })).await?;
    Ok(json!({
        "kind": "price", "feed": feed, "account": account, "feedId": feed_id(&data),
        "price": amount, "conf": conf, "raw": read.price.to_string(), "rawConf": read.conf.to_string(), "exponent": read.exponent,
        "publishTime": read.publish_time, "clock": now, "slot": slot,
        "previous": { "price": from_raw(old.price.max(0) as u64, old.exponent), "publishTime": old.publish_time },
        "note": chainsim_core::mods::NOTE,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SOL/USD price account as Solana Mainnet had it on 2026-11-12.
    fn sample() -> Vec<u8> {
        let mut d = vec![0u8; 134];
        d[..8].copy_from_slice(&DISCRIMINATOR);
        d[VERIFICATION] = FULL;
        d[PRICE..PRICE + 8].copy_from_slice(&8_133_128_092i64.to_le_bytes());
        d[CONF..CONF + 8].copy_from_slice(&2_719_535u64.to_le_bytes());
        d[EXPONENT..EXPONENT + 4].copy_from_slice(&(-8i32).to_le_bytes());
        d[PUBLISH_TIME..PUBLISH_TIME + 8].copy_from_slice(&1_794_489_559i64.to_le_bytes());
        d
    }

    #[test]
    fn a_price_account_reads_and_writes_its_fields() {
        let mut d = sample();
        assert_eq!(parse(&d), Some(Price { price: 8_133_128_092, conf: 2_719_535, exponent: -8, publish_time: 1_794_489_559 }));
        write(&mut d, 9_000_000_000, 1_000_000, 1_794_575_959, 468_900_000);
        assert_eq!(parse(&d), Some(Price { price: 9_000_000_000, conf: 1_000_000, exponent: -8, publish_time: 1_794_575_959 }));
        assert_eq!(i64::from_le_bytes(le(&d, PREV_PUBLISH_TIME)), 1_794_489_559);
        assert_eq!((i64::from_le_bytes(le(&d, EMA_PRICE)), u64::from_le_bytes(le(&d, EMA_CONF))), (9_000_000_000, 1_000_000));
        assert_eq!(u64::from_le_bytes(le(&d, POSTED_SLOT)), 468_900_000);
        assert_eq!(d.len(), 134);
        // Not a price account: too short, another discriminator, a partly verified update.
        assert_eq!(parse(&d[..100]), None);
        let mut other = sample();
        other[0] = 0;
        assert_eq!(parse(&other), None);
        let mut partial = sample();
        partial[VERIFICATION] = 0;
        assert_eq!(parse(&partial), None);
    }

    #[test]
    fn prices_convert_at_the_feed_exponent() {
        assert_eq!(to_raw("81.33128092", -8).unwrap(), 8_133_128_092);
        assert_eq!(to_raw("100", -8).unwrap(), 10_000_000_000);
        assert_eq!(from_raw(8_133_128_092, -8), "81.33128092");
        assert_eq!(to_raw("1.123456789", -8).unwrap_err().code, ErrorCode::Usage);
        assert_eq!(to_raw("1", 2).unwrap_err().code, ErrorCode::Unsupported);
    }

    #[test]
    fn feeds_resolve_by_symbol_or_address() {
        assert_eq!(resolve("sol/usd").unwrap(), ("SOL/USD".to_string(), "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE".to_string()));
        assert_eq!(resolve("4cSM2e6rvbGQUFiJbqytoVMi5GgghSMr8LwVrT9VPSPo").unwrap().0, "BTC/USD");
        let address = "US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELGZ";
        assert_eq!(resolve(address).unwrap(), (address.to_string(), address.to_string()));
        assert_eq!(resolve("DOGE/USD").unwrap_err().code, ErrorCode::Usage);
        for (_, account) in FEEDS {
            assert!(parse_address(account).is_ok());
        }
    }
}
