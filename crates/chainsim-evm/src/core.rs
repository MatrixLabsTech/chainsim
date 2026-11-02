//! A local HyperCore for a HyperEVM fork: Core state per account, the
//! CoreWriter actions and bridge transfers applied to it in block order, and
//! the precompile answers that state produces.
//!
//! This module is pure: state and ordered events go in, the new state, the
//! users it touched and the EVM credits it owes come out. It knows nothing of
//! the CLI, Docker or RPC, so any driver (each chainsim command today) can run
//! the same apply on every block.
//!
//! The model follows Hyperliquid's documentation and the HyperCore model in
//! hyper-evm-lib (MIT, https://github.com/hyperliquid-dev/hyper-evm-lib). No
//! code is copied from it.
//!
//! Units. Prices and sizes inside the model are fixed point with 8 decimals
//! (`x8`), the scale CoreWriter limit orders use. USD amounts are fixed point
//! with 6 decimals (`x6`), the perp USD scale. Spot balances are in each
//! token's wei (its `weiDecimals`).

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{keccak256, Address, U256};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::hypercore::{self, word_addr, word_i, word_u, Entry};

/// The HYPE spot token, whose EVM side is the native balance.
pub const HYPE_TOKEN: u64 = 150;
/// HYPE has 18 decimals on the EVM and 8 on Core.
pub const HYPE_EXTRA_DECIMALS: u32 = 10;
/// The HYPE system address.
pub const HYPE_SYSTEM: Address = Address::repeat_byte(0x22);
/// Spot orders use asset ids from 10000.
pub const SPOT_ASSET_BASE: u32 = 10_000;
/// The default leverage of a new cross position, capped by the perp's maximum.
pub const DEFAULT_LEVERAGE: u32 = 20;
/// How many action results the state keeps.
const MAX_RESULTS: usize = 32;
/// The state format.
pub const VERSION: u8 = 1;

/// x8 times x8 is x16; dividing by 1e10 gives x6.
const X16_TO_X6: u128 = 10_000_000_000;

/// The topic of `Received(address,uint256)`, emitted by the HYPE system contract.
pub fn received_topic() -> String {
    format!("{}", keccak256("Received(address,uint256)"))
}

/// The topic of ERC-20 `Transfer(address,address,uint256)`.
pub const TRANSFER_TOPIC: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// The system address of a spot token: 0x20, then zeros, then the index big-endian.
pub fn system_address(token: u64) -> Address {
    if token == HYPE_TOKEN {
        return HYPE_SYSTEM;
    }
    let mut b = [0u8; 20];
    b[0] = 0x20;
    b[12..].copy_from_slice(&token.to_be_bytes());
    Address::from(b)
}

/// The token a system address stands for, if it is one.
pub fn system_token(a: Address) -> Option<u64> {
    if a == HYPE_SYSTEM {
        return Some(HYPE_TOKEN);
    }
    let b = a.as_slice();
    (b[0] == 0x20 && b[1..12].iter().all(|x| *x == 0)).then(|| u64::from_be_bytes(b[12..].try_into().expect("8 bytes")))
}

/// A perp's metadata and current mark price.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Perp {
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "d")]
    pub sz_decimals: u8,
    #[serde(rename = "l")]
    pub max_leverage: u32,
    #[serde(rename = "m")]
    pub mark_x8: u64,
}

/// A spot token's metadata. `evm` is the linked ERC-20 and its extra wei
/// decimals on the EVM side; HYPE has none and uses the native balance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenMeta {
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "w")]
    pub wei_decimals: u8,
    #[serde(rename = "e", default, skip_serializing_if = "Option::is_none")]
    pub evm: Option<(String, i8)>,
}

/// One side of a book: (price x8, size x8), best first.
pub type Levels = Vec<(u64, u64)>;

/// An order book snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Book {
    #[serde(rename = "b")]
    pub bids: Levels,
    #[serde(rename = "a")]
    pub asks: Levels,
    /// The snapshot time in ms, from the info API.
    #[serde(rename = "t")]
    pub time: u64,
}

/// A spot balance: total, hold and entry notional.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spot(pub u64, pub u64, pub u64);

/// A perp position.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pos {
    /// Signed size, x8.
    #[serde(rename = "z")]
    pub szi_x8: i64,
    /// Entry notional, x6.
    #[serde(rename = "e")]
    pub entry_ntl: u64,
    #[serde(rename = "l")]
    pub leverage: u32,
    #[serde(rename = "i", default, skip_serializing_if = "is_false")]
    pub isolated: bool,
    #[serde(rename = "r", default, skip_serializing_if = "is_zero")]
    pub isolated_raw_usd: i64,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

/// One account's Core state. Spot and perp entries stay at zero once
/// created, so their precompile answers are rewritten as zero.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    #[serde(rename = "s", default)]
    pub spot: BTreeMap<u64, Spot>,
    /// Perp USD balance (cross), x6.
    #[serde(rename = "u", default)]
    pub usd: i64,
    #[serde(rename = "q", default)]
    pub positions: BTreeMap<u32, Pos>,
}

/// A resting order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    #[serde(rename = "o")]
    pub oid: u64,
    #[serde(rename = "c")]
    pub cloid: u128,
    #[serde(rename = "u")]
    pub user: Address,
    #[serde(rename = "a")]
    pub asset: u32,
    #[serde(rename = "b")]
    pub is_buy: bool,
    #[serde(rename = "p")]
    pub px_x8: u64,
    #[serde(rename = "s")]
    pub sz_x8: u64,
    #[serde(rename = "t")]
    pub tif: u8,
}

/// What became of one action: its block and log index, and a status line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    #[serde(rename = "k")]
    pub at: (u64, u64),
    #[serde(rename = "s")]
    pub status: String,
}

/// The whole Core state of a fork.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(rename = "v")]
    pub version: u8,
    /// The last applied block: number and hash.
    #[serde(rename = "c")]
    pub cursor: Option<(u64, String)>,
    #[serde(rename = "o")]
    pub next_oid: u64,
    #[serde(rename = "p", default)]
    pub perps: BTreeMap<u32, Perp>,
    #[serde(rename = "t", default)]
    pub tokens: BTreeMap<u64, TokenMeta>,
    #[serde(rename = "b", default)]
    pub books: BTreeMap<u32, Book>,
    #[serde(rename = "a", default)]
    pub accounts: BTreeMap<Address, Account>,
    #[serde(rename = "r", default)]
    pub orders: Vec<Order>,
    #[serde(rename = "x", default)]
    pub results: Vec<Outcome>,
}

/// An event the fork emitted, in the order Core applies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A CoreWriter `RawAction(user, data)`.
    Action { user: Address, payload: Vec<u8> },
    /// HYPE sent to the HYPE system address.
    HypeIn { user: Address, amount: U256 },
    /// An ERC-20 `Transfer` to a token system address.
    TokenIn { contract: Address, from: Address, to: Address, value: U256 },
}

/// An event with its place in the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logged {
    pub block: u64,
    pub log_index: u64,
    pub event: Event,
}

/// A credit Core owes the EVM side: native when `token` is None, else the ERC-20.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credit {
    pub to: Address,
    pub token: Option<Address>,
    pub amount: U256,
}

/// What one apply did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    pub users: BTreeSet<Address>,
    pub credits: Vec<Credit>,
    pub outcomes: Vec<Outcome>,
}

fn hex_u64(v: &Value) -> Option<u64> {
    u64::from_str_radix(v.as_str()?.trim_start_matches("0x"), 16).ok()
}

fn topic_address(v: Option<&Value>) -> Option<Address> {
    let b = alloy_primitives::hex::decode(v?.as_str()?).ok()?;
    (b.len() == 32).then(|| Address::from_slice(&b[12..]))
}

fn data_words(v: &Value) -> Vec<u8> {
    v.as_str().and_then(|d| alloy_primitives::hex::decode(d).ok()).unwrap_or_default()
}

fn word(data: &[u8], i: usize) -> Option<U256> {
    data.get(i * 32..i * 32 + 32).map(U256::from_be_slice)
}

/// The Core events in `eth_getLogs` output, sorted by block and log index.
/// Logs that are not RawAction, Received or a Transfer to a system address
/// are skipped.
pub fn events_from_logs(logs: &[Value]) -> Vec<Logged> {
    let core_writer: Address = hypercore::CORE_WRITER.parse().expect("address");
    let received = received_topic();
    let mut out = Vec::new();
    for log in logs {
        if log["removed"] == json!(true) {
            continue;
        }
        let (Some(block), Some(log_index)) = (hex_u64(&log["blockNumber"]), hex_u64(&log["logIndex"])) else { continue };
        let Some(address) = log["address"].as_str().and_then(|a| a.parse::<Address>().ok()) else { continue };
        let topics = log["topics"].as_array().cloned().unwrap_or_default();
        let topic0 = topics.first().and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase();
        let data = data_words(&log["data"]);
        let event = if address == core_writer && topic0 == hypercore::RAW_ACTION_TOPIC {
            match (topic_address(topics.get(1)), hypercore::log_payload(&data)) {
                (Some(user), Some(payload)) => Event::Action { user, payload },
                _ => continue,
            }
        } else if address == HYPE_SYSTEM && topic0 == received {
            // Received(address user, uint256 amount), indexed or not.
            match topic_address(topics.get(1)) {
                Some(user) => Event::HypeIn { user, amount: word(&data, 0).unwrap_or_default() },
                None => match (word(&data, 0), word(&data, 1)) {
                    (Some(u), Some(amount)) => Event::HypeIn { user: Address::from_slice(&u.to_be_bytes::<32>()[12..]), amount },
                    _ => continue,
                },
            }
        } else if topic0 == TRANSFER_TOPIC && topics.len() == 3 {
            match (topic_address(topics.get(1)), topic_address(topics.get(2))) {
                (Some(from), Some(to)) if system_token(to).is_some() => Event::TokenIn { contract: address, from, to, value: word(&data, 0).unwrap_or_default() },
                _ => continue,
            }
        } else {
            continue;
        };
        out.push(Logged { block, log_index, event });
    }
    out.sort_by_key(|l| (l.block, l.log_index));
    out
}

/// A CoreWriter action the model applies.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    Limit { asset: u32, is_buy: bool, px: u64, sz: u64, reduce_only: bool, tif: u8, cloid: u128 },
    SpotSend { destination: Address, token: u64, wei: u64 },
    UsdClass { ntl: u64, to_perp: bool },
    CancelOid { asset: u32, oid: u64 },
    CancelCloid { asset: u32, cloid: u128 },
    Other(u32),
}

fn small_word(body: &[u8], i: usize, bits: u32) -> Option<u128> {
    let w = word(body, i)?;
    (bits == 128 || w >> bits as usize == U256::ZERO).then(|| w.to::<u128>()).filter(|_| w >> 128 == U256::ZERO)
}

fn bool_word(body: &[u8], i: usize) -> Option<bool> {
    match small_word(body, i, 8)? {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

fn address_word(body: &[u8], i: usize) -> Option<Address> {
    let w = word(body, i)?.to_be_bytes::<32>();
    w[..12].iter().all(|b| *b == 0).then(|| Address::from_slice(&w[12..]))
}

/// Decodes the actions the model applies. Anything else, or a payload that
/// does not decode, is `Other` and has no effect.
fn parse_action(p: &[u8]) -> Action {
    if p.len() < 4 || p[0] != 1 {
        return Action::Other(0);
    }
    let id = u32::from_be_bytes([0, p[1], p[2], p[3]]);
    let b = &p[4..];
    let parsed = match id {
        1 => (|| {
            Some(Action::Limit {
                asset: small_word(b, 0, 32)? as u32,
                is_buy: bool_word(b, 1)?,
                px: small_word(b, 2, 64)? as u64,
                sz: small_word(b, 3, 64)? as u64,
                reduce_only: bool_word(b, 4)?,
                tif: small_word(b, 5, 8)? as u8,
                cloid: small_word(b, 6, 128)?,
            })
        })(),
        6 => (|| Some(Action::SpotSend { destination: address_word(b, 0)?, token: small_word(b, 1, 64)? as u64, wei: small_word(b, 2, 64)? as u64 }))(),
        7 => (|| Some(Action::UsdClass { ntl: small_word(b, 0, 64)? as u64, to_perp: bool_word(b, 1)? }))(),
        10 => (|| Some(Action::CancelOid { asset: small_word(b, 0, 32)? as u32, oid: small_word(b, 1, 64)? as u64 }))(),
        11 => (|| Some(Action::CancelCloid { asset: small_word(b, 0, 32)? as u32, cloid: small_word(b, 1, 128)? }))(),
        _ => None,
    };
    parsed.unwrap_or(Action::Other(id))
}

/// Notional in x6 of a size and a price, both x8.
pub fn notional(sz_x8: u64, px_x8: u64) -> u64 {
    ((sz_x8 as u128 * px_x8 as u128) / X16_TO_X6) as u64
}

/// A decimal string as a fixed-point integer with `decimals` digits; extra
/// digits are cut.
pub fn parse_dec(s: &str, decimals: u32) -> Option<i128> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if int.is_empty() && frac.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut frac: String = frac.chars().take(decimals as usize).collect();
    while frac.len() < decimals as usize {
        frac.push('0');
    }
    let v: i128 = format!("{}{frac}", if int.is_empty() { "0" } else { int }).parse().ok()?;
    Some(if neg { -v } else { v })
}

/// A fixed-point integer as a decimal string the way the info API writes
/// them: trailing zeros cut, at least one decimal digit.
pub fn fmt_dec(v: i128, decimals: u32) -> String {
    let neg = v < 0;
    let a = v.unsigned_abs();
    let scale = 10u128.pow(decimals);
    let mut frac = format!("{:0width$}", a % scale, width = decimals as usize);
    while frac.len() > 1 && frac.ends_with('0') {
        frac.pop();
    }
    if frac.is_empty() {
        frac.push('0');
    }
    format!("{}{}.{frac}", if neg { "-" } else { "" }, a / scale)
}

/// A user's perp margin figures, all x6.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Margin {
    pub account_value: i64,
    pub margin_used: u64,
    pub ntl_pos: u64,
    pub raw_usd: i64,
    pub withdrawable: u64,
}

impl State {
    pub fn new() -> Self {
        Self { version: VERSION, ..Default::default() }
    }

    /// A perp's precompile price (raw units, 6 minus szDecimals decimals) as x8.
    pub fn px_from_raw(&self, asset: u32, raw: u64) -> Option<u64> {
        let d = self.perps.get(&asset)?.sz_decimals as u32;
        raw.checked_mul(10u64.pow(2 + d))
    }

    /// A perp's x8 price in the precompile's raw units.
    pub fn px_to_raw(&self, asset: u32, px_x8: u64) -> Option<u64> {
        let d = self.perps.get(&asset)?.sz_decimals as u32;
        Some(px_x8 / 10u64.pow(2 + d))
    }

    /// A perp's x8 size in szDecimals units, and back.
    pub fn sz_to_raw(&self, asset: u32, sz_x8: i64) -> i64 {
        let d = self.perps.get(&asset).map_or(8, |p| p.sz_decimals as u32).min(8);
        sz_x8 / 10i64.pow(8 - d)
    }

    pub fn sz_from_raw(&self, asset: u32, raw: i64) -> Option<i64> {
        let d = self.perps.get(&asset)?.sz_decimals as u32;
        raw.checked_mul(10i64.pow(8 - d.min(8)))
    }

    fn account(&mut self, user: Address) -> &mut Account {
        self.accounts.entry(user).or_default()
    }

    /// A user's margin figures at the current marks.
    pub fn margin(&self, user: Address) -> Margin {
        let Some(acct) = self.accounts.get(&user) else { return Margin::default() };
        let mut m = Margin::default();
        let (mut upnl, mut signed_entry) = (0i128, 0i128);
        for (asset, p) in &acct.positions {
            if p.szi_x8 == 0 {
                continue;
            }
            let size = p.szi_x8.unsigned_abs();
            let mark = self.perps.get(asset).map_or_else(|| (p.entry_ntl as u128 * X16_TO_X6 / size as u128) as u64, |x| x.mark_x8);
            let value = notional(size, mark);
            let sign: i128 = if p.szi_x8 > 0 { 1 } else { -1 };
            m.ntl_pos += value;
            m.margin_used += value / p.leverage.max(1) as u64;
            upnl += sign * (value as i128 - p.entry_ntl as i128);
            signed_entry += sign * p.entry_ntl as i128;
        }
        let order_margin: u64 = self.orders.iter().filter(|o| o.user == user && o.asset < SPOT_ASSET_BASE).map(|o| notional(o.sz_x8, o.px_x8) / self.leverage_of(user, o.asset) as u64).sum();
        m.account_value = (acct.usd as i128 + upnl) as i64;
        m.raw_usd = (acct.usd as i128 - signed_entry) as i64;
        m.withdrawable = (m.account_value as i128 - m.margin_used as i128 - order_margin as i128).max(0) as u64;
        m
    }

    fn leverage_of(&self, user: Address, asset: u32) -> u32 {
        match self.accounts.get(&user).and_then(|a| a.positions.get(&asset)).filter(|p| p.leverage > 0) {
            Some(p) => p.leverage,
            None => self.perps.get(&asset).map_or(DEFAULT_LEVERAGE, |p| DEFAULT_LEVERAGE.min(p.max_leverage.max(1))),
        }
    }

    /// Applies one fill to a user's position: size and price x8.
    fn fill(&mut self, user: Address, asset: u32, is_buy: bool, q: u64, px: u64) {
        let lev = self.leverage_of(user, asset);
        let acct = self.account(user);
        let pos = acct.positions.entry(asset).or_insert(Pos { leverage: lev, ..Default::default() });
        if pos.leverage == 0 {
            pos.leverage = lev;
        }
        let dq: i64 = if is_buy { q as i64 } else { -(q as i64) };
        if pos.szi_x8 == 0 || (pos.szi_x8 > 0) == is_buy {
            pos.szi_x8 += dq;
            pos.entry_ntl += notional(q, px);
            return;
        }
        let size = pos.szi_x8.unsigned_abs();
        let closing = q.min(size);
        let closed_entry = (pos.entry_ntl as u128 * closing as u128 / size as u128) as u64;
        let exit = notional(closing, px) as i64;
        let realized = if pos.szi_x8 > 0 { exit - closed_entry as i64 } else { closed_entry as i64 - exit };
        pos.entry_ntl -= closed_entry;
        pos.szi_x8 += if is_buy { closing as i64 } else { -(closing as i64) };
        let left = q - closing;
        if left > 0 {
            pos.szi_x8 = if is_buy { left as i64 } else { -(left as i64) };
            pos.entry_ntl = notional(left, px);
        }
        if pos.szi_x8 == 0 {
            pos.entry_ntl = 0;
        }
        acct.usd += realized;
    }

    fn fmt_sz(&self, sz_x8: u64) -> String {
        fmt_dec(sz_x8 as i128, 8)
    }

    /// A limit order: matched against the book and other users' resting
    /// orders, then the remainder rests (GTC, ALO) or is dropped (IOC).
    #[allow(clippy::too_many_arguments)]
    fn limit(&mut self, user: Address, asset: u32, is_buy: bool, px: u64, mut sz: u64, reduce_only: bool, tif: u8, cloid: u128, applied: &mut Applied) -> String {
        if asset >= SPOT_ASSET_BASE {
            return "rejected: spot orders are not modeled".into();
        }
        if !self.perps.contains_key(&asset) || !self.books.contains_key(&asset) {
            return format!("rejected: no book for perp {asset}, run chainsim set <name> hypercore book {asset} --live");
        }
        if !(1..=3).contains(&tif) {
            return format!("rejected: unknown time in force {tif}");
        }
        if px == 0 || sz == 0 {
            return "rejected: zero price or size".into();
        }
        let pos = self.accounts.get(&user).and_then(|a| a.positions.get(&asset)).map_or(0, |p| p.szi_x8);
        if reduce_only {
            if pos == 0 || (pos > 0) == is_buy {
                return "rejected: reduce only, and the order would not reduce a position".into();
            }
            sz = sz.min(pos.unsigned_abs());
        }
        // Makers: book levels then resting orders of other users, best price first.
        let book = &self.books[&asset];
        let side = if is_buy { &book.asks } else { &book.bids };
        let crosses = |p: u64| if is_buy { p <= px } else { p >= px };
        let mut makers: Vec<(u64, Option<u64>)> = side.iter().filter(|(p, _)| crosses(*p)).map(|(p, _)| (*p, None)).collect();
        makers.extend(self.orders.iter().filter(|o| o.asset == asset && o.is_buy != is_buy && o.user != user && crosses(o.px_x8)).map(|o| (o.px_x8, Some(o.oid))));
        makers.sort_by(|a, b| if is_buy { a.0.cmp(&b.0) } else { b.0.cmp(&a.0) }.then(a.1.is_some().cmp(&b.1.is_some())));
        makers.dedup();
        if tif == 1 && !makers.is_empty() {
            return "rejected: ALO order would cross the book".into();
        }
        // Margin for the part that adds exposure, at the limit price.
        let reducing = if pos != 0 && (pos > 0) != is_buy { sz.min(pos.unsigned_abs()) } else { 0 };
        let need = notional(sz - reducing, px) / self.leverage_of(user, asset) as u64;
        if need > self.margin(user).withdrawable {
            return format!("rejected: insufficient margin, needs {} USD", fmt_dec(need as i128, 6));
        }
        let mut left = sz;
        let (mut filled, mut cost) = (0u64, 0u128);
        for (mpx, oid) in makers {
            if left == 0 {
                break;
            }
            let q = match oid {
                None => {
                    let book = self.books.get_mut(&asset).expect("book");
                    let side = if is_buy { &mut book.asks } else { &mut book.bids };
                    let Some(i) = side.iter().position(|(p, _)| *p == mpx) else { continue };
                    let q = left.min(side[i].1);
                    side[i].1 -= q;
                    if side[i].1 == 0 {
                        side.remove(i);
                    }
                    q
                }
                Some(oid) => {
                    let Some(i) = self.orders.iter().position(|o| o.oid == oid) else { continue };
                    let q = left.min(self.orders[i].sz_x8);
                    let maker = self.orders[i].user;
                    self.orders[i].sz_x8 -= q;
                    if self.orders[i].sz_x8 == 0 {
                        self.orders.remove(i);
                    }
                    self.fill(maker, asset, !is_buy, q, mpx);
                    applied.users.insert(maker);
                    q
                }
            };
            self.fill(user, asset, is_buy, q, mpx);
            left -= q;
            filled += q;
            cost += q as u128 * mpx as u128;
        }
        let mut parts = Vec::new();
        if filled > 0 {
            parts.push(format!("filled {} at {}", self.fmt_sz(filled), fmt_dec((cost / filled as u128) as i128, 8)));
        }
        if left > 0 {
            if tif == 3 {
                parts.push(format!("{} dropped (IOC)", self.fmt_sz(left)));
            } else {
                let oid = self.next_oid.max(1);
                self.next_oid = oid + 1;
                self.orders.push(Order { oid, cloid, user, asset, is_buy, px_x8: px, sz_x8: left, tif });
                parts.push(format!("{} resting as oid {oid}", self.fmt_sz(left)));
            }
        }
        parts.join(", ")
    }

    fn cancel(&mut self, user: Address, asset: u32, matches: impl Fn(&Order) -> bool) -> String {
        match self.orders.iter().position(|o| o.user == user && o.asset == asset && matches(o)) {
            Some(i) => format!("canceled oid {}", self.orders.remove(i).oid),
            None => "rejected: no such open order".into(),
        }
    }

    /// The EVM amount for a Core amount of `token`, or None when the token has no EVM side.
    fn evm_amount(&self, token: u64, wei: u64) -> Option<(Option<Address>, U256)> {
        if token == HYPE_TOKEN {
            return Some((None, U256::from(wei) * U256::from(10u64).pow(U256::from(HYPE_EXTRA_DECIMALS))));
        }
        let (addr, extra) = self.tokens.get(&token)?.evm.clone()?;
        let addr: Address = addr.parse().ok()?;
        let v = U256::from(wei);
        let ten = U256::from(10u64);
        Some((Some(addr), if extra >= 0 { v * ten.pow(U256::from(extra as u32)) } else { v / ten.pow(U256::from((-extra) as u32)) }))
    }

    /// The Core amount for an EVM amount of `token` (cut to Core's precision).
    fn core_amount(&self, token: u64, amount: U256) -> Option<u64> {
        let ten = U256::from(10u64);
        let v = if token == HYPE_TOKEN {
            amount / ten.pow(U256::from(HYPE_EXTRA_DECIMALS))
        } else {
            let (_, extra) = self.tokens.get(&token)?.evm.clone()?;
            if extra >= 0 {
                amount / ten.pow(U256::from(extra as u32))
            } else {
                amount * ten.pow(U256::from((-extra) as u32))
            }
        };
        (v <= U256::from(u64::MAX)).then(|| v.to::<u64>())
    }

    fn spot_send(&mut self, user: Address, destination: Address, token: u64, wei: u64, applied: &mut Applied) -> String {
        let have = self.accounts.get(&user).and_then(|a| a.spot.get(&token)).map_or(0, |s| s.0.saturating_sub(s.1));
        if wei > have {
            return format!("rejected: insufficient spot balance of token {token}");
        }
        if system_token(destination) == Some(token) {
            let Some((evm, amount)) = self.evm_amount(token, wei) else { return format!("rejected: token {token} has no EVM side") };
            self.account(user).spot.entry(token).or_default().0 -= wei;
            applied.credits.push(Credit { to: user, token: evm, amount });
            return format!("sent {wei} wei of token {token} to the EVM");
        }
        self.account(user).spot.entry(token).or_default().0 -= wei;
        self.account(destination).spot.entry(token).or_default().0 += wei;
        applied.users.insert(destination);
        format!("sent {wei} wei of token {token} to {destination}")
    }

    fn usd_class(&mut self, user: Address, ntl: u64, to_perp: bool) -> String {
        // Spot USDC (token 0) has 8 wei decimals, perp USD 6.
        let wei = ntl as u128 * 100;
        if to_perp {
            let have = self.accounts.get(&user).and_then(|a| a.spot.get(&0)).map_or(0, |s| s.0.saturating_sub(s.1));
            if wei > have as u128 {
                return "rejected: insufficient spot USDC".into();
            }
            let a = self.account(user);
            a.spot.entry(0).or_default().0 -= wei as u64;
            a.usd += ntl as i64;
        } else {
            if ntl > self.margin(user).withdrawable {
                return "rejected: insufficient withdrawable perp USD".into();
            }
            let a = self.account(user);
            a.usd -= ntl as i64;
            a.spot.entry(0).or_default().0 += wei as u64;
        }
        format!("moved {} USD to {}", fmt_dec(ntl as i128, 6), if to_perp { "perp" } else { "spot" })
    }

    /// Applies events in order and moves the cursor to `head` (number, hash).
    /// Events at or below the cursor are skipped, so re-applying a range is a no-op.
    pub fn apply(&mut self, events: &[Logged], head: (u64, String)) -> Applied {
        let mut applied = Applied::default();
        let after = self.cursor.as_ref().map(|c| c.0);
        for l in events {
            if after.is_some_and(|c| l.block <= c) || l.block > head.0 {
                continue;
            }
            let status = match &l.event {
                Event::Action { user, payload } => {
                    applied.users.insert(*user);
                    match parse_action(payload) {
                        Action::Limit { asset, is_buy, px, sz, reduce_only, tif, cloid } => self.limit(*user, asset, is_buy, px, sz, reduce_only, tif, cloid, &mut applied),
                        Action::SpotSend { destination, token, wei } => self.spot_send(*user, destination, token, wei, &mut applied),
                        Action::UsdClass { ntl, to_perp } => self.usd_class(*user, ntl, to_perp),
                        Action::CancelOid { asset, oid } => self.cancel(*user, asset, |o| o.oid == oid),
                        Action::CancelCloid { asset, cloid } => self.cancel(*user, asset, |o| o.cloid == cloid && cloid != 0),
                        Action::Other(id) => format!("recorded: action {id} is not modeled"),
                    }
                }
                Event::HypeIn { user, amount } => match self.core_amount(HYPE_TOKEN, *amount) {
                    Some(wei) => {
                        self.account(*user).spot.entry(HYPE_TOKEN).or_default().0 += wei;
                        applied.users.insert(*user);
                        format!("credited {wei} wei of HYPE on Core")
                    }
                    None => "rejected: amount too large".into(),
                },
                Event::TokenIn { contract, from, to, value } => {
                    let token = system_token(*to).expect("filtered");
                    let linked = self.tokens.get(&token).and_then(|t| t.evm.as_ref()).and_then(|(a, _)| a.parse::<Address>().ok()) == Some(*contract);
                    match self.core_amount(token, *value).filter(|_| linked) {
                        Some(wei) => {
                            self.account(*from).spot.entry(token).or_default().0 += wei;
                            applied.users.insert(*from);
                            format!("credited {wei} wei of token {token} on Core")
                        }
                        None => continue,
                    }
                }
            };
            applied.outcomes.push(Outcome { at: (l.block, l.log_index), status });
        }
        self.results.extend(applied.outcomes.iter().cloned());
        let drop = self.results.len().saturating_sub(MAX_RESULTS);
        self.results.drain(..drop);
        self.cursor = Some(head);
        applied
    }

    /// The precompile answers for these users: spot balances (0x801),
    /// positions (0x800), withdrawable (0x803) and the margin summary (0x80f).
    pub fn answers(&self, users: &BTreeSet<Address>) -> Vec<Entry> {
        let mut out = Vec::new();
        for user in users {
            let Some(acct) = self.accounts.get(user) else { continue };
            for (token, s) in &acct.spot {
                out.push(hypercore::spot_balance(*user, *token, s.0, s.1, s.2));
            }
            for (asset, p) in &acct.positions {
                let Ok(perp) = u16::try_from(*asset) else { continue };
                let pos = hypercore::Position { szi: self.sz_to_raw(*asset, p.szi_x8), entry_ntl: p.entry_ntl, isolated_raw_usd: p.isolated_raw_usd, leverage: p.leverage, is_isolated: p.isolated };
                out.push(hypercore::position(*user, perp, pos));
            }
            let m = self.margin(*user);
            out.push(Entry { precompile: hypercore::WITHDRAWABLE, calldata: word_addr(*user).to_vec(), result: word_u(m.withdrawable as u128).to_vec() });
            out.push(Entry {
                precompile: hypercore::ACCOUNT_MARGIN_SUMMARY,
                calldata: [word_u(0), word_addr(*user)].concat(),
                result: [word_i(m.account_value), word_u(m.margin_used as u128), word_u(m.ntl_pos as u128), word_i(m.raw_usd)].concat(),
            });
        }
        out
    }

    /// The status lines of applied actions at these (block, log index) places.
    pub fn result_of(&self, at: (u64, u64)) -> Option<&str> {
        self.results.iter().rev().find(|o| o.at == at).map(|o| o.status.as_str())
    }

    /// A user's Core state in the info API's shapes: `clearinghouseState`,
    /// `spotClearinghouseState`, and the open orders.
    pub fn info(&self, user: Address, time_ms: u64) -> Value {
        let m = self.margin(user);
        let usd = |v: i128| fmt_dec(v, 6);
        let summary = json!({
            "accountValue": usd(m.account_value as i128), "totalNtlPos": usd(m.ntl_pos as i128),
            "totalRawUsd": usd(m.raw_usd as i128), "totalMarginUsed": usd(m.margin_used as i128),
        });
        let acct = self.accounts.get(&user).cloned().unwrap_or_default();
        let mut positions = Vec::new();
        for (asset, p) in acct.positions.iter().filter(|(_, p)| p.szi_x8 != 0) {
            let perp = self.perps.get(asset);
            let size = p.szi_x8.unsigned_abs();
            let mark = perp.map_or(0, |x| x.mark_x8);
            let value = notional(size, mark) as i128;
            let sign: i128 = if p.szi_x8 > 0 { 1 } else { -1 };
            let upnl = sign * (value - p.entry_ntl as i128);
            let margin = value / p.leverage.max(1) as i128;
            let entry_px = (p.entry_ntl as u128 * X16_TO_X6 / size as u128) as i128;
            let roe = if p.entry_ntl > 0 { upnl * p.leverage as i128 * 10_000_000_000 / p.entry_ntl as i128 } else { 0 };
            positions.push(json!({
                "type": "oneWay",
                "position": {
                    "coin": perp.map_or_else(|| asset.to_string(), |x| x.name.clone()),
                    "szi": fmt_dec(p.szi_x8 as i128, 8),
                    "leverage": { "type": if p.isolated { "isolated" } else { "cross" }, "value": p.leverage },
                    "entryPx": fmt_dec(entry_px, 8),
                    "positionValue": usd(value),
                    "unrealizedPnl": usd(upnl),
                    "returnOnEquity": fmt_dec(roe, 10),
                    "liquidationPx": null,
                    "marginUsed": usd(margin),
                    "maxLeverage": perp.map_or(0, |x| x.max_leverage),
                    "cumFunding": { "allTime": "0.0", "sinceOpen": "0.0", "sinceChange": "0.0" },
                },
            }));
        }
        let balances: Vec<Value> = acct
            .spot
            .iter()
            .map(|(token, s)| {
                let meta = self.tokens.get(token);
                let w = meta.map_or(8, |t| t.wei_decimals as u32);
                json!({
                    "coin": meta.map_or_else(|| token.to_string(), |t| t.name.clone()), "token": token,
                    "total": fmt_dec(s.0 as i128, w), "hold": fmt_dec(s.1 as i128, w), "entryNtl": usd(s.2 as i128),
                })
            })
            .collect();
        let orders: Vec<Value> = self
            .orders
            .iter()
            .filter(|o| o.user == user)
            .map(|o| {
                json!({
                    "coin": self.perps.get(&o.asset).map_or_else(|| o.asset.to_string(), |x| x.name.clone()), "asset": o.asset,
                    "side": if o.is_buy { "B" } else { "A" }, "limitPx": fmt_dec(o.px_x8 as i128, 8), "sz": fmt_dec(o.sz_x8 as i128, 8),
                    "oid": o.oid, "cloid": (o.cloid != 0).then(|| format!("0x{:032x}", o.cloid)),
                    "tif": match o.tif { 1 => "Alo", 2 => "Gtc", _ => "Ioc" },
                })
            })
            .collect();
        json!({
            "user": user.to_string(),
            "clearinghouseState": {
                "marginSummary": summary, "crossMarginSummary": summary, "crossMaintenanceMarginUsed": "0.0",
                "withdrawable": usd(m.withdrawable as i128), "assetPositions": positions, "time": time_ms,
            },
            "spotClearinghouseState": { "balances": balances },
            "openOrders": orders,
        })
    }

    /// Sets a user's perp state from the info API's `clearinghouseState`.
    /// Perps must already be in `perps`; positions in other perps are skipped
    /// and returned by name.
    pub fn mirror_perps(&mut self, user: Address, ch: &Value) -> Vec<String> {
        let by_name: BTreeMap<String, u32> = self.perps.iter().map(|(a, p)| (p.name.clone(), *a)).collect();
        let mut positions = BTreeMap::new();
        let mut skipped = Vec::new();
        for ap in ch["assetPositions"].as_array().cloned().unwrap_or_default() {
            let p = &ap["position"];
            let coin = p["coin"].as_str().unwrap_or_default().to_string();
            let Some(asset) = by_name.get(&coin).copied() else {
                skipped.push(coin);
                continue;
            };
            let (Some(szi), Some(entry_px)) = (p["szi"].as_str().and_then(|s| parse_dec(s, 8)), p["entryPx"].as_str().and_then(|s| parse_dec(s, 8))) else {
                skipped.push(coin);
                continue;
            };
            let leverage = p["leverage"]["value"].as_u64().unwrap_or(DEFAULT_LEVERAGE as u64) as u32;
            let isolated = p["leverage"]["type"] == "isolated";
            positions.insert(asset, Pos { szi_x8: szi as i64, entry_ntl: notional(szi.unsigned_abs() as u64, entry_px as u64), leverage, isolated, isolated_raw_usd: 0 });
        }
        let account_value = ch["marginSummary"]["accountValue"].as_str().and_then(|s| parse_dec(s, 6)).unwrap_or(0);
        let acct = self.account(user);
        // Old positions go to zero, so their answers are rewritten.
        for p in acct.positions.values_mut() {
            *p = Pos { leverage: p.leverage, ..Default::default() };
        }
        acct.positions.extend(positions);
        acct.usd = 0;
        // Pick the USD balance that gives the same account value at our marks.
        let upnl = self.margin(user).account_value as i128;
        self.account(user).usd = (account_value - upnl) as i64;
        skipped
    }

    /// Sets a user's spot balances from the info API's `spotClearinghouseState`.
    /// Tokens must already be in `tokens`; others are skipped and returned.
    pub fn mirror_spot(&mut self, user: Address, sch: &Value) -> Vec<String> {
        let mut skipped = Vec::new();
        let mut spot = BTreeMap::new();
        for b in sch["balances"].as_array().cloned().unwrap_or_default() {
            let token = b["token"].as_u64().unwrap_or(u64::MAX);
            let Some(meta) = self.tokens.get(&token) else {
                skipped.push(b["coin"].as_str().unwrap_or_default().to_string());
                continue;
            };
            let w = meta.wei_decimals as u32;
            let get = |k: &str, d: u32| b[k].as_str().and_then(|s| parse_dec(s, d)).unwrap_or(0).max(0) as u64;
            spot.insert(token, Spot(get("total", w), get("hold", w), get("entryNtl", 6)));
        }
        let acct = self.account(user);
        for s in acct.spot.values_mut() {
            *s = Spot::default();
        }
        acct.spot.extend(spot);
        skipped
    }

    /// A book from the info API's `l2Book`: bids then asks, prices and sizes as x8.
    pub fn book_from_info(l2: &Value) -> Option<Book> {
        let levels = l2["levels"].as_array()?;
        let side = |v: &Value| -> Option<Levels> { v.as_array()?.iter().map(|l| Some((parse_dec(l["px"].as_str()?, 8)? as u64, parse_dec(l["sz"].as_str()?, 8)? as u64))).collect() };
        Some(Book { bids: side(levels.first()?)?, asks: side(levels.get(1)?)?, time: l2["time"].as_u64().unwrap_or(0) })
    }

    /// Serialized for storage on the fork.
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("state serializes")
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        serde_json::from_slice(b).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hypercore::{encode_action, limit_order};

    const ONE_X8: i128 = 100_000_000;
    const ALICE: Address = Address::repeat_byte(0xa1);
    const BOB: Address = Address::repeat_byte(0xb0);

    /// BTC (szDecimals 5) at mark 100, asks 101 x 1 and 102 x 2, bids 99 x 1 and 98 x 3.
    fn state() -> State {
        let mut s = State::new();
        s.perps.insert(0, Perp { name: "BTC".into(), sz_decimals: 5, max_leverage: 40, mark_x8: 100 * ONE_X8 as u64 });
        let x = |v: u64| v * ONE_X8 as u64;
        s.books.insert(0, Book { bids: vec![(x(99), x(1)), (x(98), x(3))], asks: vec![(x(101), x(1)), (x(102), x(2))], time: 1 });
        s.tokens.insert(0, TokenMeta { name: "USDC".into(), wei_decimals: 8, evm: None });
        s.tokens.insert(HYPE_TOKEN, TokenMeta { name: "HYPE".into(), wei_decimals: 8, evm: None });
        s.tokens.insert(200, TokenMeta { name: "TKN".into(), wei_decimals: 6, evm: Some(("0x00000000000000000000000000000000000000aa".into(), 12)) });
        s.account(ALICE).usd = 10_000 * 1_000_000;
        s.cursor = Some((10, "0x10".into()));
        s
    }

    fn act(block: u64, user: Address, payload: Vec<u8>) -> Logged {
        Logged { block, log_index: 0, event: Event::Action { user, payload } }
    }

    fn x8(v: f64) -> u64 {
        (v * 1e8).round() as u64
    }

    fn run(s: &mut State, block: u64, user: Address, payload: Vec<u8>) -> String {
        let a = s.apply(&[act(block, user, payload)], (block, format!("0x{block:x}")));
        a.outcomes.last().map(|o| o.status.clone()).unwrap_or_default()
    }

    #[test]
    fn a_buy_fills_fully_at_the_best_ask() {
        let mut s = state();
        let r = run(&mut s, 11, ALICE, limit_order(0, true, x8(101.0), x8(0.5), false, 3, 0));
        assert_eq!(r, "filled 0.5 at 101.0");
        let p = s.accounts[&ALICE].positions[&0];
        assert_eq!((p.szi_x8, p.entry_ntl, p.leverage), (x8(0.5) as i64, 50_500_000, 20));
        // The level lost the filled depth.
        assert_eq!(s.books[&0].asks[0], (x8(101.0), x8(0.5)));
        assert_eq!(s.cursor, Some((11, "0xb".into())));
    }

    #[test]
    fn a_buy_walks_the_book_and_rests_the_rest_or_drops_it() {
        let mut s = state();
        // GTC at 102 for 4: 1 at 101, 2 at 102, 1 rests.
        let r = run(&mut s, 11, ALICE, limit_order(0, true, x8(102.0), x8(4.0), false, 2, 7));
        assert_eq!(r, "filled 3.0 at 101.66666666, 1.0 resting as oid 1");
        assert!(s.books[&0].asks.is_empty());
        assert_eq!(s.orders.len(), 1);
        assert_eq!((s.orders[0].px_x8, s.orders[0].sz_x8, s.orders[0].cloid), (x8(102.0), x8(1.0), 7));
        assert_eq!(s.accounts[&ALICE].positions[&0].entry_ntl, 305_000_000);
        // IOC with nothing left on the ask side: all dropped.
        let r = run(&mut s, 12, ALICE, limit_order(0, true, x8(105.0), x8(1.0), false, 3, 0));
        assert_eq!(r, "1.0 dropped (IOC)");
        assert_eq!(s.orders.len(), 1);
    }

    #[test]
    fn a_partial_ioc_drops_the_remainder() {
        let mut s = state();
        let r = run(&mut s, 11, ALICE, limit_order(0, true, x8(101.0), x8(1.5), false, 3, 0));
        assert_eq!(r, "filled 1.0 at 101.0, 0.5 dropped (IOC)");
        assert!(s.orders.is_empty());
    }

    #[test]
    fn alo_that_would_cross_is_rejected_and_one_below_rests() {
        let mut s = state();
        assert_eq!(run(&mut s, 11, ALICE, limit_order(0, true, x8(101.0), x8(1.0), false, 1, 0)), "rejected: ALO order would cross the book");
        assert!(s.orders.is_empty() && s.accounts[&ALICE].positions.is_empty());
        assert_eq!(run(&mut s, 12, ALICE, limit_order(0, true, x8(100.0), x8(1.0), false, 1, 0)), "1.0 resting as oid 1");
    }

    #[test]
    fn resting_orders_cancel_by_oid_and_cloid_and_hold_margin() {
        let mut s = state();
        run(&mut s, 11, ALICE, limit_order(0, true, x8(90.0), x8(2.0), false, 2, 0x55));
        run(&mut s, 12, ALICE, limit_order(0, true, x8(80.0), x8(1.0), false, 2, 0));
        // 2 x 90 / 20 + 1 x 80 / 20 = 13 USD held.
        assert_eq!(s.margin(ALICE).withdrawable, 9_987_000_000);
        assert_eq!(run(&mut s, 13, BOB, encode_action(11, &[word_u(0), word_u(0x55)].concat())), "rejected: no such open order");
        assert_eq!(run(&mut s, 14, ALICE, encode_action(11, &[word_u(0), word_u(0x55)].concat())), "canceled oid 1");
        assert_eq!(run(&mut s, 15, ALICE, encode_action(10, &[word_u(0), word_u(2)].concat())), "canceled oid 2");
        assert!(s.orders.is_empty());
        assert_eq!(s.margin(ALICE).withdrawable, 10_000_000_000);
    }

    #[test]
    fn a_taker_fills_against_another_users_resting_order() {
        let mut s = state();
        s.account(BOB).usd = 1_000_000_000;
        run(&mut s, 11, BOB, limit_order(0, false, x8(100.5), x8(0.2), false, 2, 0));
        let a = s.apply(&[act(12, ALICE, limit_order(0, true, x8(101.0), x8(0.3), false, 3, 0))], (12, "0xc".into()));
        assert_eq!(a.outcomes[0].status, "filled 0.3 at 100.66666666");
        assert!(a.users.contains(&BOB));
        assert_eq!(s.accounts[&BOB].positions[&0].szi_x8, -(x8(0.2) as i64));
        assert!(s.orders.is_empty());
    }

    #[test]
    fn closing_realizes_pnl_and_margin_limits_size() {
        let mut s = state();
        run(&mut s, 11, ALICE, limit_order(0, true, x8(101.0), x8(1.0), false, 3, 0));
        // Sell 1 at the best bid 99: 2 USD loss.
        assert_eq!(run(&mut s, 12, ALICE, limit_order(0, false, x8(99.0), x8(1.0), true, 3, 0)), "filled 1.0 at 99.0");
        assert_eq!(s.accounts[&ALICE].usd, 9_998_000_000);
        assert_eq!(s.accounts[&ALICE].positions[&0], Pos { leverage: 20, ..Default::default() });
        // 3000 BTC at 102 needs 15,300 USD of margin.
        assert!(run(&mut s, 13, ALICE, limit_order(0, true, x8(102.0), x8(3000.0), false, 3, 0)).starts_with("rejected: insufficient margin"));
        assert!(run(&mut s, 14, ALICE, limit_order(0, true, x8(102.0), x8(1.0), true, 3, 0)).starts_with("rejected: reduce only"));
        assert!(run(&mut s, 15, ALICE, limit_order(3, true, 1, 1, false, 3, 0)).starts_with("rejected: no book for perp 3"));
    }

    #[test]
    fn usd_class_transfer_moves_between_spot_and_perp() {
        let mut s = state();
        s.account(BOB).spot.insert(0, Spot(500 * 100_000_000, 0, 0));
        assert_eq!(run(&mut s, 11, BOB, encode_action(7, &[word_u(200_000_000), word_u(1)].concat())), "moved 200.0 USD to perp");
        assert_eq!((s.accounts[&BOB].spot[&0].0, s.accounts[&BOB].usd), (300 * 100_000_000, 200_000_000));
        assert_eq!(run(&mut s, 12, BOB, encode_action(7, &[word_u(50_000_000), word_u(0)].concat())), "moved 50.0 USD to spot");
        assert_eq!((s.accounts[&BOB].spot[&0].0, s.accounts[&BOB].usd), (350 * 100_000_000, 150_000_000));
        assert!(run(&mut s, 13, BOB, encode_action(7, &[word_u(1_000_000_000), word_u(1)].concat())).starts_with("rejected"));
        assert!(run(&mut s, 14, BOB, encode_action(7, &[word_u(1_000_000_000), word_u(0)].concat())).starts_with("rejected"));
    }

    #[test]
    fn spot_send_moves_balances_and_to_a_system_address_credits_the_evm() {
        let mut s = state();
        s.account(ALICE).spot.insert(HYPE_TOKEN, Spot(5 * 100_000_000, 0, 0));
        s.account(ALICE).spot.insert(200, Spot(7_000_000, 0, 0));
        let a = s.apply(&[act(11, ALICE, encode_action(6, &[word_addr(BOB), word_u(HYPE_TOKEN as u128), word_u(100_000_000)].concat()))], (11, "0xb".into()));
        assert!(a.users.contains(&BOB) && a.credits.is_empty());
        assert_eq!((s.accounts[&ALICE].spot[&HYPE_TOKEN].0, s.accounts[&BOB].spot[&HYPE_TOKEN].0), (400_000_000, 100_000_000));
        // To 0x2222…2222: native HYPE on the EVM, 18 decimals.
        let a = s.apply(&[act(12, ALICE, encode_action(6, &[word_addr(HYPE_SYSTEM), word_u(HYPE_TOKEN as u128), word_u(150_000_000)].concat()))], (12, "0xc".into()));
        assert_eq!(a.credits, vec![Credit { to: ALICE, token: None, amount: U256::from(15u64) * U256::from(10u64).pow(U256::from(17u64)) }]);
        assert_eq!(s.accounts[&ALICE].spot[&HYPE_TOKEN].0, 250_000_000);
        // A linked ERC-20 with 12 extra decimals.
        let a = s.apply(&[act(13, ALICE, encode_action(6, &[word_addr(system_address(200)), word_u(200), word_u(2_000_000)].concat()))], (13, "0xd".into()));
        assert_eq!(a.credits[0].token, Some("0x00000000000000000000000000000000000000aa".parse().unwrap()));
        assert_eq!(a.credits[0].amount, U256::from(2_000_000u64) * U256::from(10u64).pow(U256::from(12u64)));
        assert!(run(&mut s, 14, ALICE, encode_action(6, &[word_addr(BOB), word_u(0), word_u(1)].concat())).starts_with("rejected: insufficient"));
    }

    #[test]
    fn bridge_transfers_credit_core_spot() {
        let mut s = state();
        let tkn: Address = "0x00000000000000000000000000000000000000aa".parse().unwrap();
        let other: Address = "0x00000000000000000000000000000000000000bb".parse().unwrap();
        let events = [
            Logged { block: 11, log_index: 0, event: Event::HypeIn { user: BOB, amount: U256::from(2u64) * U256::from(10u64).pow(U256::from(18u64)) } },
            Logged { block: 11, log_index: 1, event: Event::TokenIn { contract: tkn, from: BOB, to: system_address(200), value: U256::from(3u64) * U256::from(10u64).pow(U256::from(18u64)) } },
            // A Transfer from a contract that is not the linked one does nothing.
            Logged { block: 11, log_index: 2, event: Event::TokenIn { contract: other, from: BOB, to: system_address(200), value: U256::from(1u64) } },
        ];
        let a = s.apply(&events, (11, "0xb".into()));
        assert_eq!(a.outcomes.len(), 2);
        assert_eq!(s.accounts[&BOB].spot[&HYPE_TOKEN].0, 200_000_000);
        assert_eq!(s.accounts[&BOB].spot[&200].0, 3_000_000);
        assert_eq!(system_token(system_address(200)), Some(200));
        assert_eq!(system_address(0x1234).to_string().to_lowercase(), "0x2000000000000000000000000000000000001234");
    }

    #[test]
    fn re_applying_a_range_is_a_no_op() {
        let mut s = state();
        let ev = [act(11, ALICE, limit_order(0, true, x8(101.0), x8(0.5), false, 3, 0))];
        s.apply(&ev, (11, "0xb".into()));
        let once = s.clone();
        let again = s.apply(&ev, (11, "0xb".into()));
        assert!(again.outcomes.is_empty());
        assert_eq!(s, once);
        assert_eq!(s.result_of((11, 0)), Some("filled 0.5 at 101.0"));
    }

    #[test]
    fn logs_become_ordered_events() {
        let call = hypercore::send_raw_action(&limit_order(0, true, 1, 1, false, 3, 0));
        let raw = json!({ "address": hypercore::CORE_WRITER, "topics": [hypercore::RAW_ACTION_TOPIC, format!("0x{:0>64}", "a1".repeat(20))], "data": format!("0x{}", &call[10..]), "blockNumber": "0xb", "logIndex": "0x1" });
        let recv = json!({ "address": "0x2222222222222222222222222222222222222222", "topics": [received_topic()], "data": format!("0x{:0>64}{:064x}", "b0".repeat(20), 5), "blockNumber": "0xb", "logIndex": "0x0" });
        let xfer = json!({ "address": "0x00000000000000000000000000000000000000aa", "topics": [TRANSFER_TOPIC, format!("0x{:0>64}", "b0".repeat(20)), format!("0x{:0>64}", "20000000000000000000000000000000000000c8")], "data": format!("0x{:064x}", 9), "blockNumber": "0xa", "logIndex": "0x3" });
        let plain = json!({ "address": "0x00000000000000000000000000000000000000aa", "topics": [TRANSFER_TOPIC, format!("0x{:0>64}", "b0".repeat(20)), format!("0x{:0>64}", "a1".repeat(20))], "data": "0x", "blockNumber": "0xa", "logIndex": "0x4" });
        let ev = events_from_logs(&[raw, recv, xfer, plain]);
        assert_eq!(ev.iter().map(|e| (e.block, e.log_index)).collect::<Vec<_>>(), [(10, 3), (11, 0), (11, 1)]);
        assert_eq!(ev[1].event, Event::HypeIn { user: BOB, amount: U256::from(5u64) });
        assert!(matches!(&ev[2].event, Event::Action { user, .. } if *user == ALICE));
        assert!(matches!(&ev[0].event, Event::TokenIn { to, .. } if system_token(*to) == Some(200)));
    }

    #[test]
    fn state_encodes_to_precompile_answers() {
        let mut s = state();
        run(&mut s, 11, ALICE, limit_order(0, true, x8(101.0), x8(1.0), false, 3, 0));
        s.account(ALICE).spot.insert(HYPE_TOKEN, Spot(42, 0, 0));
        let answers = s.answers(&BTreeSet::from([ALICE]));
        let find = |p: u16| answers.iter().find(|e| e.precompile == p).unwrap();
        // Position: 1 BTC is 100000 in szDecimals 5, entry 101 USD.
        let pos = find(hypercore::POSITION);
        assert_eq!(pos.calldata, [word_addr(ALICE), word_u(0)].concat());
        assert_eq!(pos.result, [word_i(100_000), word_u(101_000_000), word_i(0), word_u(20), word_u(0)].concat());
        assert_eq!(find(hypercore::SPOT_BALANCE).result, [word_u(42), word_u(0), word_u(0)].concat());
        // Mark 100: value 100, uPnL -1, margin 5, account value 9999, withdrawable 9994.
        assert_eq!(find(hypercore::WITHDRAWABLE).result, word_u(9_994_000_000).to_vec());
        let ams = find(hypercore::ACCOUNT_MARGIN_SUMMARY);
        assert_eq!(ams.calldata, [word_u(0), word_addr(ALICE)].concat());
        assert_eq!(ams.result, [word_i(9_999_000_000), word_u(5_000_000), word_u(100_000_000), word_i(9_899_000_000)].concat());
        let info = s.info(ALICE, 7);
        let p = &info["clearinghouseState"]["assetPositions"][0]["position"];
        assert_eq!((p["coin"].as_str(), p["szi"].as_str(), p["entryPx"].as_str(), p["unrealizedPnl"].as_str()), (Some("BTC"), Some("1.0"), Some("101.0"), Some("-1.0")));
        assert_eq!(info["clearinghouseState"]["withdrawable"], "9994.0");
        assert_eq!(info["spotClearinghouseState"]["balances"][0]["total"], "0.00000042");
    }

    #[test]
    fn mirror_reads_the_info_api_shapes() {
        let mut s = state();
        let ch = json!({
            "marginSummary": { "accountValue": "1000.5" },
            "assetPositions": [
                { "type": "oneWay", "position": { "coin": "BTC", "szi": "-0.12016", "entryPx": "98.0", "leverage": { "type": "cross", "value": 10 } } },
                { "type": "oneWay", "position": { "coin": "DOGE", "szi": "5", "entryPx": "1", "leverage": { "type": "cross", "value": 3 } } },
            ],
        });
        assert_eq!(s.mirror_perps(BOB, &ch), ["DOGE"]);
        let p = s.accounts[&BOB].positions[&0];
        assert_eq!((p.szi_x8, p.entry_ntl, p.leverage), (-12_016_000, 11_775_680, 10));
        assert_eq!(s.margin(BOB).account_value, 1_000_500_000);
        let sch = json!({ "balances": [{ "coin": "HYPE", "token": 150, "total": "12.5", "hold": "0.0", "entryNtl": "300.0" }, { "coin": "X", "token": 9, "total": "1" }] });
        assert_eq!(s.mirror_spot(BOB, &sch), ["X"]);
        assert_eq!(s.accounts[&BOB].spot[&HYPE_TOKEN], Spot(1_250_000_000, 0, 300_000_000));
        let l2 = json!({ "time": 5, "levels": [[{ "px": "78352.0", "sz": "3.34071", "n": 9 }], [{ "px": "78353.0", "sz": "0.1", "n": 1 }]] });
        let b = State::book_from_info(&l2).unwrap();
        assert_eq!((b.bids[0], b.asks[0], b.time), ((7_835_200_000_000, 334_071_000), (7_835_300_000_000, 10_000_000), 5));
    }

    #[test]
    fn decimals_parse_and_format_like_the_info_api() {
        assert_eq!(parse_dec("78352.0", 8), Some(7_835_200_000_000));
        assert_eq!(parse_dec("-0.12016", 8), Some(-12_016_000));
        assert_eq!(parse_dec("1.123456789", 6), Some(1_123_456));
        assert_eq!(parse_dec("x", 6), None);
        assert_eq!(fmt_dec(7_835_200_000_000, 8), "78352.0");
        assert_eq!(fmt_dec(-12_016_000, 8), "-0.12016");
        assert_eq!(fmt_dec(0, 6), "0.0");
    }

    #[test]
    fn state_round_trips_through_bytes() {
        let mut s = state();
        run(&mut s, 11, ALICE, limit_order(0, true, x8(90.0), x8(1.0), false, 2, u128::MAX));
        assert_eq!(State::from_bytes(&s.to_bytes()), Some(s));
    }
}
