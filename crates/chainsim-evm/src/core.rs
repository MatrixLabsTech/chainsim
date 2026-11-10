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
/// Orders on builder-deployed (HIP-3) perps use asset 100000 + perp index, where
/// the perp index is dex * 10000 + the index in the dex's meta. The read
/// precompiles take the perp index.
pub const HIP3_ASSET_BASE: u32 = 100_000;
pub const DEX_SIZE: u32 = 10_000;
/// The spot side in sendAsset's dex fields.
pub const SPOT_DEX: u32 = u32::MAX;

/// The perp dex of a perp index: 0 for the main dex.
pub fn dex_of(perp: u32) -> u32 {
    perp / DEX_SIZE
}
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
    system_address_for(token, HYPE_TOKEN)
}

/// The system address of a spot token on a network whose HYPE token index is `hype`.
pub fn system_address_for(token: u64, hype: u64) -> Address {
    if token == hype {
        return HYPE_SYSTEM;
    }
    let mut b = [0u8; 20];
    b[0] = 0x20;
    b[12..].copy_from_slice(&token.to_be_bytes());
    Address::from(b)
}

/// The token a system address stands for, if it is one.
pub fn system_token(a: Address) -> Option<u64> {
    system_token_for(a, HYPE_TOKEN)
}

/// The token a system address stands for on a network whose HYPE token index is `hype`.
pub fn system_token_for(a: Address, hype: u64) -> Option<u64> {
    if a == HYPE_SYSTEM {
        return Some(hype);
    }
    let b = a.as_slice();
    (b[0] == 0x20 && b[1..12].iter().all(|x| *x == 0)).then(|| u64::from_be_bytes(b[12..].try_into().expect("8 bytes")))
}

/// A perp's metadata and current mark price.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Perp {
    #[serde(rename = "n")]
    pub name: String,
    #[serde(rename = "d")]
    pub sz_decimals: u8,
    #[serde(rename = "l")]
    pub max_leverage: u32,
    #[serde(rename = "m")]
    pub mark_x8: u64,
    /// Oracle price x8; zero means the mark.
    #[serde(rename = "o", default, skip_serializing_if = "is_zero_u")]
    pub oracle_x8: u64,
    /// Hourly funding rate, 12 decimals.
    #[serde(rename = "f", default, skip_serializing_if = "is_zero")]
    pub funding_e12: i64,
    /// Isolated margin only (meta's onlyIsolated or strictIsolated).
    #[serde(rename = "is", default, skip_serializing_if = "is_false")]
    pub isolated_only: bool,
    /// The collateral token of the perp's dex (0 is USDC).
    #[serde(rename = "co", default, skip_serializing_if = "is_zero_u")]
    pub collateral: u64,
    /// Margin tiers: (lower bound of the position notional x6, max leverage),
    /// lowest first. Empty means one tier at `max_leverage`.
    #[serde(rename = "mt", default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<(u64, u32)>,
    /// HIP-3 funding settings from perpDexs, when the perp's dex sets them.
    #[serde(rename = "hf", default, skip_serializing_if = "Option::is_none")]
    pub hip3_funding: Option<Hip3Funding>,
}

/// A HIP-3 perp's funding settings, 12 decimals: the deployer's multiplier,
/// the 8-hour interest rate and the clamp on rate minus premium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hip3Funding {
    #[serde(rename = "m")]
    pub multiplier_e12: i64,
    #[serde(rename = "r")]
    pub rate_e12: i64,
    #[serde(rename = "c")]
    pub clamp_e12: i64,
}

/// The default 8-hour interest rate (0.01%) and clamp (0.05%) of the funding formula.
pub const FUNDING_RATE_E12: i64 = 100_000_000;
pub const FUNDING_CLAMP_E12: i64 = 500_000_000;
/// Funding is capped at 4% an hour.
pub const FUNDING_CAP_E12: i64 = 40_000_000_000;

impl Hip3Funding {
    /// A perp's settings from its dex's perpDexs entry: assetToFundingMultiplier,
    /// assetToFundingInterestRate and assetToFundingClamp, keyed by the perp's
    /// name. A perp the lists leave out takes multiplier 1 and the defaults.
    pub fn from_dex(dex: &Value, name: &str) -> Self {
        let find = |key: &str| dex[key].as_array().and_then(|l| l.iter().find(|e| e[0].as_str() == Some(name))).and_then(|e| e[1].as_str()).and_then(|v| parse_dec(v, 12)).map(|v| v as i64);
        Self { multiplier_e12: find("assetToFundingMultiplier").unwrap_or(1_000_000_000_000), rate_e12: find("assetToFundingInterestRate").unwrap_or(FUNDING_RATE_E12), clamp_e12: find("assetToFundingClamp").unwrap_or(FUNDING_CLAMP_E12) }
    }

    /// The hourly funding rate (12 decimals) from the premium, also 12 decimals:
    /// multiplier x (premium + clamp(rate - premium, -clamp, clamp)) / 8, capped at 4% an hour.
    pub fn hourly_e12(&self, premium_e12: i64) -> i64 {
        let p = premium_e12 as i128;
        let c = self.clamp_e12 as i128;
        let eight_hour = p + (self.rate_e12 as i128 - p).clamp(-c, c);
        (self.multiplier_e12 as i128 * eight_hour / 1_000_000_000_000 / 8).clamp(-(FUNDING_CAP_E12 as i128), FUNDING_CAP_E12 as i128) as i64
    }
}

/// The notional of an impact price (20,000 USD for BTC and ETH, 6,000 for others), x6.
pub fn impact_notional(name: &str) -> u64 {
    if matches!(name, "BTC" | "ETH") {
        20_000_000_000
    } else {
        6_000_000_000
    }
}

/// A perp price (x8) on Hyperliquid's tick: at most 5 significant figures
/// and 6 minus szDecimals decimals, with integer prices always valid. An ask
/// rounds up and a bid rounds down.
pub fn tick_px(px_x8: u64, sz_decimals: u8, ask: bool) -> u64 {
    let decimals = 10u64.pow(2 + (sz_decimals as u32).min(6));
    let digits = px_x8.checked_ilog10().map_or(0, |d| d + 1);
    let figures = 10u64.pow(digits.saturating_sub(5)).min(100_000_000);
    let step = decimals.max(figures);
    if ask {
        px_x8.div_ceil(step) * step
    } else {
        px_x8 / step * step
    }
}

/// The average price of taking `ntl` (x6) from one side of a book, x8, or
/// the last level's price when the book is thinner than that.
pub fn impact_px(levels: &Levels, ntl: u64) -> Option<u64> {
    const EXTRA: u128 = 100_000_000;
    let (mut got, mut size) = (0u128, 0u128);
    for &(px, sz) in levels {
        let level = notional(sz, px) as u128;
        let take = level.min(ntl as u128 - got);
        // Sizes carry 8 more decimals here, so the average does not drift up.
        size += if take == level { sz as u128 * EXTRA } else { take * X16_TO_X6 * EXTRA / px as u128 };
        got += take;
        if got >= ntl as u128 {
            break;
        }
    }
    (size > 0).then(|| (got * X16_TO_X6 * EXTRA / size) as u64)
}

/// The HIP-3 premium, 12 decimals: the impact mid over the oracle price, less 1.
pub fn hip3_premium_e12(book: &Book, oracle_x8: u64, ntl: u64) -> Option<i64> {
    let (bid, ask) = (impact_px(&book.bids, ntl)?, impact_px(&book.asks, ntl)?);
    let mid = (bid as i128 + ask as i128) / 2;
    (oracle_x8 > 0).then(|| ((mid - oracle_x8 as i128) * 1_000_000_000_000 / oracle_x8 as i128) as i64)
}

impl Perp {
    fn tier_list(&self) -> Vec<(u64, u32)> {
        if self.tiers.is_empty() {
            vec![(0, self.max_leverage.max(1))]
        } else {
            self.tiers.clone()
        }
    }

    /// The tier a position of this notional (x6) falls in: the highest lower bound at or below it.
    fn tier_at(&self, ntl: u64) -> usize {
        self.tier_list().iter().rposition(|(lb, _)| *lb <= ntl).unwrap_or(0)
    }

    /// The max leverage at a position notional, x6.
    pub fn max_leverage_at(&self, ntl: u64) -> u32 {
        self.tier_list()[self.tier_at(ntl)].1.max(1)
    }

    /// Each tier as (lower bound x6, max leverage, maintenance deduction x6).
    fn tier_terms(&self) -> Vec<(u64, u32, i128)> {
        let tiers = self.tier_list();
        let mut deduction = 0i128;
        let mut out = Vec::with_capacity(tiers.len());
        for (n, (lb, lev)) in tiers.iter().enumerate() {
            if n > 0 {
                let (lev, prev) = ((*lev).max(1) as i128, tiers[n - 1].1.max(1) as i128);
                deduction += *lb as i128 * (prev - lev) / (2 * lev * prev);
            }
            out.push((*lb, (*lev).max(1), deduction));
        }
        out
    }

    /// The maintenance margin of a position notional, x6:
    /// notional / (2 x tier max leverage) - the tier's maintenance deduction,
    /// where each tier adds lower bound x the step in maintenance rate.
    pub fn maintenance_at(&self, ntl: u64) -> u64 {
        let tiers = self.tier_list();
        let at = self.tier_at(ntl);
        let mut deduction = 0i128;
        for n in 1..=at {
            let (lb, lev) = (tiers[n].0 as i128, tiers[n].1.max(1) as i128);
            let prev = tiers[n - 1].1.max(1) as i128;
            deduction += lb * (prev - lev) / (2 * lev * prev);
        }
        (ntl as i128 / (2 * tiers[at].1.max(1) as i128) - deduction).max(0) as u64
    }
}

/// A perp's margin tiers from the info API meta (`marginTableId` and the
/// meta's `marginTables`). A table id below 50 is not listed and means one
/// tier at the perp's max leverage, which is also what an empty result means.
pub fn tiers_from_meta(u: &Value, tables: &Value) -> Vec<(u64, u32)> {
    let Some(id) = u["marginTableId"].as_u64().filter(|id| *id >= 50) else { return vec![] };
    let tiers = tables.as_array().and_then(|t| t.iter().find(|e| e[0].as_u64() == Some(id))).and_then(|e| e[1]["marginTiers"].as_array()).cloned().unwrap_or_default();
    let list: Vec<(u64, u32)> = tiers.iter().filter_map(|t| Some((parse_dec(t["lowerBound"].as_str()?, 6)? as u64, t["maxLeverage"].as_u64()? as u32))).collect();
    if list.len() > 1 {
        list
    } else {
        vec![]
    }
}

fn is_zero_u(v: &u64) -> bool {
    *v == 0
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
    #[serde(rename = "z", default)]
    pub sz_decimals: u8,
}

/// One side of a book: (price x8, size x8), best first.
pub type Levels = Vec<(u64, u64)>;

/// A perp's order book: the levels the info API gave, moved with every
/// price scripted since.
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

impl Book {
    /// Moves every level's price by `to / from`, the ratio of a mark price
    /// move, and keeps the sizes. Bids round down and asks round up, so the
    /// sides never meet. Levels that land on one price merge.
    pub fn scale(&mut self, to: u64, from: u64) {
        if from == 0 || to == from {
            return;
        }
        let moved = |levels: &Levels, up: bool| -> Levels {
            let mut out: Levels = Vec::with_capacity(levels.len());
            for &(px, sz) in levels {
                let n = px as u128 * to as u128;
                let px = (if up { n.div_ceil(from as u128) } else { n / from as u128 }) as u64;
                match out.last_mut() {
                    Some(last) if last.0 == px => last.1 += sz,
                    _ if px > 0 => out.push((px, sz)),
                    _ => {}
                }
            }
            out
        };
        self.bids = moved(&self.bids, false);
        self.asks = moved(&self.asks, true);
    }
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
    /// Funding paid since the position opened, x6 (negative when received).
    #[serde(rename = "f", default, skip_serializing_if = "is_zero")]
    pub funding_paid: i64,
    /// The position's own max leverage when a mirror found one that differs
    /// from the perp's (an account with its own margin terms), else 0.
    #[serde(rename = "m", default, skip_serializing_if = "is_zero_u32")]
    pub max_leverage: u32,
    /// Funding paid before the position opened, x6: all-time funding is this plus `funding_paid`.
    #[serde(rename = "fa", default, skip_serializing_if = "is_zero")]
    pub funding_before: i64,
    /// `funding_paid` at the last change of the position's size, x6.
    #[serde(rename = "fc", default, skip_serializing_if = "is_zero")]
    pub funding_at_change: i64,
}

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
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
    /// Recent fills from price moves, liquidations, funding and fees: (time, text).
    #[serde(rename = "h", default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<(u64, String)>,
    /// Recent fills, newest last.
    #[serde(rename = "fl", default, skip_serializing_if = "Vec::is_empty")]
    pub fills: Vec<Fill>,
    /// Whether this address's EVM transactions go into big blocks.
    #[serde(rename = "bb", default, skip_serializing_if = "is_false")]
    pub big_blocks: bool,
    /// Perp USD balances of builder-deployed dexes, x6 (dex 0 is `usd`).
    #[serde(rename = "du", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dex_usd: BTreeMap<u32, i64>,
    /// Borrow and lend positions per token.
    #[serde(rename = "ld", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lend: BTreeMap<u64, crate::lend::Lend>,
    /// Account abstraction (action 16): 0 default, 1 disabled, 2 unified account, 3 portfolio margin.
    #[serde(rename = "ab", default, skip_serializing_if = "is_zero_u8")]
    pub abstraction: u8,
}

fn is_zero_u8(v: &u8) -> bool {
    *v == 0
}

impl Account {
    /// The perp USD balance of a dex, x6.
    pub fn usd_of(&self, dex: u32) -> i64 {
        if dex == 0 {
            self.usd
        } else {
            self.dex_usd.get(&dex).copied().unwrap_or(0)
        }
    }

    pub fn usd_mut(&mut self, dex: u32) -> &mut i64 {
        if dex == 0 {
            &mut self.usd
        } else {
            self.dex_usd.entry(dex).or_default()
        }
    }
}

/// One fill, as the info API's userFills reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    #[serde(rename = "a")]
    pub asset: u32,
    #[serde(rename = "p")]
    pub px_x8: u64,
    #[serde(rename = "s")]
    pub sz_x8: u64,
    #[serde(rename = "b")]
    pub is_buy: bool,
    /// Block time, ms.
    #[serde(rename = "t")]
    pub time_ms: u64,
    /// The signed size before the fill, x8.
    #[serde(rename = "sp")]
    pub start_x8: i64,
    /// Realized PnL, x6.
    #[serde(rename = "cp")]
    pub closed_pnl: i64,
    #[serde(rename = "o")]
    pub oid: u64,
    /// Taker (true) or maker.
    #[serde(rename = "x")]
    pub crossed: bool,
    /// Fee, x6.
    #[serde(rename = "f")]
    pub fee: u64,
    #[serde(rename = "id")]
    pub tid: u64,
    #[serde(rename = "l", default, skip_serializing_if = "is_false")]
    pub liquidation: bool,
}

/// How many fills an account keeps.
const MAX_FILLS: usize = 50;

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
    /// When it was placed, ms, and its size then (x8).
    #[serde(rename = "ts", default)]
    pub time_ms: u64,
    #[serde(rename = "os", default)]
    pub orig_sz_x8: u64,
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
    /// The last funding time applied, in seconds, on the hour.
    #[serde(rename = "ft", default)]
    pub funding_time: u64,
    /// Credits owed to the EVM side that have not landed yet.
    #[serde(rename = "pc", default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<Credit>,
    #[serde(rename = "nc", default)]
    pub next_credit: u64,
    #[serde(rename = "nt", default)]
    pub next_tid: u64,
    /// The block time of the last big block, seconds.
    #[serde(rename = "lb", default)]
    pub last_big_block: u64,
    /// The HYPE token index (150 on mainnet, 1105 on testnet). Zero means 150.
    #[serde(rename = "ht", default)]
    pub hype_token: u64,
    /// Borrow and lend reserves per token.
    #[serde(rename = "rv", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reserves: BTreeMap<u64, crate::lend::Reserve>,
    /// The block time of the step being applied, for history lines.
    #[serde(skip)]
    now: u64,
    /// Whether a reserve changed since the answers were last written.
    #[serde(skip)]
    pub reserves_dirty: bool,
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

/// A credit Core owes the EVM side: native when `token` is None, else the
/// ERC-20. `base` is the EVM balance read before the first attempt, so a retry
/// sets `base + amount` again instead of adding twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credit {
    #[serde(rename = "i")]
    pub id: u64,
    #[serde(rename = "u")]
    pub to: Address,
    #[serde(rename = "k", default, skip_serializing_if = "Option::is_none")]
    pub token: Option<Address>,
    #[serde(rename = "a")]
    pub amount: U256,
    #[serde(rename = "b", default, skip_serializing_if = "Option::is_none")]
    pub base: Option<U256>,
}

impl Credit {
    /// The EVM balance this credit leaves, once its base is known.
    pub fn target(&self) -> Option<U256> {
        self.base.map(|b| b + self.amount)
    }
}

/// Base-tier perp fees, in millionths of notional: 0.045% taker, 0.015% maker.
pub const TAKER_FEE_E6: u64 = 450;
pub const MAKER_FEE_E6: u64 = 150;
/// Funding is paid every hour.
pub const FUNDING_INTERVAL: u64 = 3600;
/// How many history lines an account keeps.
const MAX_HISTORY: usize = 16;

/// The fee on a fill's notional (x6).
pub fn fee(ntl: u64, maker: bool) -> u64 {
    ntl * if maker { MAKER_FEE_E6 } else { TAKER_FEE_E6 } / 1_000_000
}

/// What one apply did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// Users whose every answer is rewritten.
    pub users: BTreeSet<Address>,
    /// Users whose margin answers (withdrawable, margin summary) changed.
    pub margin_users: BTreeSet<Address>,
    pub credits: Vec<Credit>,
    pub outcomes: Vec<Outcome>,
    /// Resting orders filled by a price move.
    pub fills: usize,
    pub liquidated: Vec<Address>,
    /// Whether any borrow and lend reserve changed, so 0x812 is rewritten.
    pub reserves: bool,
}

/// The perp universe and asset contexts from the info API at `up`, kept for
/// `meta`, `metaAndAssetCtxs` and `allMids` on the copy. Universe entries are
/// (name, szDecimals, maxLeverage, isDelisted). Context entries are, in order:
/// markPx, oraclePx, funding, openInterest, prevDayPx, dayNtlVlm, premium,
/// midPx, dayBaseVlm.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    #[serde(rename = "u")]
    pub universe: Vec<(String, u8, u32, bool)>,
    #[serde(rename = "c")]
    pub ctxs: Vec<Vec<Option<String>>>,
    /// Builder-deployed dexes the copy has fetched, by name.
    #[serde(rename = "d", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dexes: BTreeMap<String, DexMeta>,
    /// Margin tiers of the universe entries with more than one tier, by index.
    #[serde(rename = "t", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tiers: BTreeMap<u32, Vec<(u64, u32)>>,
}

/// A builder-deployed dex's meta: its index, collateral token and universe.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DexMeta {
    #[serde(rename = "i")]
    pub index: u32,
    #[serde(rename = "k")]
    pub collateral: u64,
    #[serde(rename = "m")]
    pub meta: Box<Meta>,
}

/// The context fields a [`Meta`] keeps, in order.
pub const META_CTX_FIELDS: [&str; 9] = ["markPx", "oraclePx", "funding", "openInterest", "prevDayPx", "dayNtlVlm", "premium", "midPx", "dayBaseVlm"];

impl Meta {
    /// From the info API's `metaAndAssetCtxs` answer.
    pub fn from_info(v: &Value) -> Self {
        let universe = v[0]["universe"].as_array().map_or(vec![], |u| {
            u.iter()
                .map(|x| (x["name"].as_str().unwrap_or_default().to_string(), x["szDecimals"].as_u64().unwrap_or(0) as u8, x["maxLeverage"].as_u64().unwrap_or(1) as u32, x["isDelisted"] == json!(true)))
                .collect()
        });
        let ctxs = v[1].as_array().map_or(vec![], |c| c.iter().map(|x| META_CTX_FIELDS.iter().map(|k| x[*k].as_str().map(str::to_string)).collect()).collect());
        let tiers = v[0]["universe"].as_array().map_or(BTreeMap::new(), |u| {
            u.iter().enumerate().map(|(i, x)| (i as u32, tiers_from_meta(x, &v[0]["marginTables"]))).filter(|(_, t)| !t.is_empty()).collect()
        });
        Self { universe, ctxs, dexes: BTreeMap::new(), tiers }
    }
}

/// HyperEVM block types: small blocks carry 3,000,000 gas, big blocks 30,000,000.
pub const SMALL_BLOCK_GAS: u64 = 3_000_000;
pub const BIG_BLOCK_GAS: u64 = 30_000_000;
/// Big blocks come about a minute apart.
pub const BIG_BLOCK_SECS: u64 = 60;
/// The big-block gas price measured on the mainnet RPC (eth_bigBlockGasPrice), 0.1 gwei.
pub const BIG_BLOCK_GAS_PRICE: u64 = 100_000_000;

/// The gas limit of the block a transaction goes into.
pub fn block_gas_limit(big: bool) -> u64 {
    if big {
        BIG_BLOCK_GAS
    } else {
        SMALL_BLOCK_GAS
    }
}

/// The timestamp a big block needs, when the next block time `next` would
/// come less than a minute after the last big block. None means `next` is fine.
pub fn big_block_time(next: u64, last_big: u64) -> Option<u64> {
    (last_big > 0 && next < last_big + BIG_BLOCK_SECS).then_some(last_big + BIG_BLOCK_SECS)
}

/// Why a transaction cannot go into its block type, or None.
pub fn block_refusal(gas: u64, big: bool) -> Option<String> {
    let limit = block_gas_limit(big);
    (gas > limit).then(|| format!("exceeds block gas limit: the transaction asks {gas} gas and a {} block holds {limit}", if big { "big" } else { "small" }))
}

/// The longest replay: a month of hourly candles.
pub const MAX_REPLAY_CANDLES: u64 = 744;

/// How many hourly candles a replay from `from` to `to` (seconds, both ends included) covers.
pub fn replay_candles(from: u64, to: u64) -> u64 {
    to.saturating_sub(from) / FUNDING_INTERVAL + 1
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
    SendAsset { destination: Address, source_dex: u32, destination_dex: u32, token: u64, wei: u64 },
    BorrowLend { op: u8, token: u64, wei: u64 },
    SetAbstraction { user: Address, abstraction: u8 },
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
        13 => (|| {
            Some(Action::SendAsset {
                destination: address_word(b, 0)?,
                source_dex: small_word(b, 2, 32)? as u32,
                destination_dex: small_word(b, 3, 32)? as u32,
                token: small_word(b, 4, 64)? as u64,
                wei: small_word(b, 5, 64)? as u64,
            })
        })(),
        15 => (|| Some(Action::BorrowLend { op: small_word(b, 0, 8)? as u8, token: small_word(b, 1, 64)? as u64, wei: small_word(b, 2, 64)? as u64 }))(),
        16 => (|| Some(Action::SetAbstraction { user: address_word(b, 0)?, abstraction: small_word(b, 1, 8)? as u8 }))(),
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
    /// What a transfer out may take: account value less the larger of the
    /// margin used and 10% of the notional, less the margin of open orders.
    pub withdrawable: u64,
    /// What a new order may use: account value less margin used and order margin.
    pub free: u64,
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

    /// The HYPE token index of this network.
    pub fn hype(&self) -> u64 {
        if self.hype_token == 0 {
            HYPE_TOKEN
        } else {
            self.hype_token
        }
    }

    fn mark_of(&self, perp: u32, p: &Pos) -> u64 {
        let size = p.szi_x8.unsigned_abs().max(1);
        self.perps.get(&perp).map_or_else(|| (p.entry_ntl as u128 * X16_TO_X6 / size as u128) as u64, |x| x.mark_x8)
    }

    /// The max leverage of a position of notional `value` (x6): its own when
    /// a mirror stored one, else the perp's at that notional.
    fn max_leverage_of(&self, perp: u32, p: &Pos, value: u64) -> u32 {
        if p.max_leverage > 0 {
            return p.max_leverage;
        }
        self.perps.get(&perp).map_or(u32::MAX, |x| x.max_leverage_at(value)).max(1)
    }

    /// The maintenance margin of a position of notional `value` (x6): half
    /// the initial margin at its max leverage, by the perp's tiers unless the
    /// position carries its own max leverage.
    fn maintenance_of(&self, perp: u32, p: &Pos, value: u64) -> u64 {
        if p.max_leverage > 0 {
            return value / (2 * p.max_leverage as u64);
        }
        self.perps.get(&perp).map_or(value / 2, |x| x.maintenance_at(value))
    }

    /// The liquidation price of a position (x8), or None when no positive
    /// price liquidates it: the price at which `equity` (the account value,
    /// or an isolated position's margin with its PnL, x6) falls to the
    /// maintenance margin, `maintenance` being what is required now. Within
    /// one margin tier this is Hyperliquid's formula, price - side x margin
    /// available / size / (1 - l x side) with l = 1 / (2 x max leverage). A
    /// position that reaches a higher tier on the way takes that tier's rate
    /// and deduction.
    fn liquidation_px(&self, perp: u32, p: &Pos, equity: i128, maintenance: i128) -> Option<u64> {
        let size = p.szi_x8.unsigned_abs() as i128;
        if size == 0 {
            return None;
        }
        let side: i128 = if p.szi_x8 > 0 { 1 } else { -1 };
        let value = notional(size as u64, self.mark_of(perp, p));
        let others = maintenance - self.maintenance_of(perp, p, value) as i128;
        let tiers = match (p.max_leverage, self.perps.get(&perp)) {
            (0, Some(x)) => x.tier_terms(),
            (0, None) => vec![(0, 1, 0)],
            (own, _) => vec![(0, own, 0)],
        };
        for (n, (lower, max, deduction)) in tiers.iter().enumerate() {
            // equity + side x (v - value) = others + v / (2 max) - deduction, solved for the notional v.
            let m2 = 2 * *max as i128;
            let (num, den) = ((side * value as i128 - (equity - others + deduction)) * m2, m2 * side - 1);
            let px = num * 10_000_000_000 / (den * size);
            let v = num / den;
            let below_next = tiers.get(n + 1).is_none_or(|next| v < next.0 as i128);
            if px > 0 && v >= *lower as i128 && below_next {
                return Some(px as u64);
            }
        }
        None
    }

    /// A user's main-dex margin figures at the current marks.
    pub fn margin(&self, user: Address) -> Margin {
        self.margin_dex(user, 0)
    }

    /// A user's cross margin figures on one perp dex, x6. Isolated positions
    /// hold their own margin and stay out of these figures.
    pub fn margin_dex(&self, user: Address, dex: u32) -> Margin {
        let Some(acct) = self.accounts.get(&user) else { return Margin::default() };
        let mut m = Margin::default();
        let (mut upnl, mut signed_entry) = (0i128, 0i128);
        for (perp, p) in acct.positions.iter().filter(|(a, p)| dex_of(**a) == dex && !p.isolated && p.szi_x8 != 0) {
            let value = notional(p.szi_x8.unsigned_abs(), self.mark_of(*perp, p));
            let sign: i128 = if p.szi_x8 > 0 { 1 } else { -1 };
            m.ntl_pos += value;
            m.margin_used += value / p.leverage.clamp(1, self.max_leverage_of(*perp, p, value)) as u64;
            upnl += sign * (value as i128 - p.entry_ntl as i128);
            signed_entry += sign * p.entry_ntl as i128;
        }
        let order_margin: u64 = self.orders.iter().filter(|o| o.user == user && dex_of(o.asset) == dex).map(|o| notional(o.sz_x8, o.px_x8) / self.leverage_of(user, o.asset) as u64).sum();
        let usd = acct.usd_of(dex);
        m.account_value = (usd as i128 + upnl) as i64;
        m.raw_usd = (usd as i128 - signed_entry) as i64;
        m.free = (m.account_value as i128 - m.margin_used as i128 - order_margin as i128).max(0) as u64;
        // transfer_margin_required = max(initial_margin_required, 0.1 x total_position_value).
        m.withdrawable = (m.account_value as i128 - m.margin_used.max(m.ntl_pos / 10) as i128 - order_margin as i128).max(0) as u64;
        m
    }

    /// An isolated position's equity (its margin plus unrealized PnL) and
    /// maintenance margin, x6.
    pub fn isolated_health(&self, perp: u32, p: &Pos) -> (i64, u64) {
        let value = notional(p.szi_x8.unsigned_abs(), self.mark_of(perp, p));
        let sign: i128 = if p.szi_x8 > 0 { 1 } else { -1 };
        let upnl = sign * (value as i128 - p.entry_ntl as i128);
        ((p.isolated_raw_usd as i128 + upnl) as i64, self.maintenance_of(perp, p, value))
    }

    fn leverage_of(&self, user: Address, asset: u32) -> u32 {
        match self.accounts.get(&user).and_then(|a| a.positions.get(&asset)).filter(|p| p.leverage > 0) {
            Some(p) => p.leverage,
            None => self.perps.get(&asset).map_or(DEFAULT_LEVERAGE, |p| DEFAULT_LEVERAGE.min(p.max_leverage.max(1))),
        }
    }

    /// One fill for a user: the position moves, the fee is charged, and the
    /// fill is recorded for userFills.
    #[allow(clippy::too_many_arguments)]
    fn fill(&mut self, user: Address, asset: u32, is_buy: bool, q: u64, px: u64, oid: u64, crossed: bool, fee: u64, liquidation: bool) {
        let start = self.accounts.get(&user).and_then(|a| a.positions.get(&asset)).map_or(0, |p| p.szi_x8);
        let closed_pnl = self.fill_raw(user, asset, is_buy, q, px);
        self.charge(user, asset, fee);
        let tid = self.next_tid.max(1);
        self.next_tid = tid + 1;
        let time_ms = self.now * 1000;
        let fills = &mut self.account(user).fills;
        fills.push(Fill { asset, px_x8: px, sz_x8: q, is_buy, time_ms, start_x8: start, closed_pnl, oid, crossed, fee, tid, liquidation });
        let drop = fills.len().saturating_sub(MAX_FILLS);
        fills.drain(..drop);
    }

    /// Applies one fill to a user's position: size and price x8. Returns the
    /// realized PnL, x6. The dex's USD balance takes the PnL. An isolated
    /// position takes margin from it as it grows and gives back its share as
    /// it shrinks.
    fn fill_raw(&mut self, user: Address, asset: u32, is_buy: bool, q: u64, px: u64) -> i64 {
        let lev = self.leverage_of(user, asset);
        let isolated = self.perps.get(&asset).is_some_and(|p| p.isolated_only);
        let dex = dex_of(asset);
        let acct = self.account(user);
        let pos = acct.positions.entry(asset).or_insert(Pos { leverage: lev, ..Default::default() });
        if pos.leverage == 0 {
            pos.leverage = lev;
        }
        if pos.szi_x8 == 0 {
            pos.isolated = isolated;
        }
        let dq: i64 = if is_buy { q as i64 } else { -(q as i64) };
        pos.funding_at_change = pos.funding_paid;
        if pos.szi_x8 == 0 || (pos.szi_x8 > 0) == is_buy {
            pos.szi_x8 += dq;
            pos.entry_ntl += notional(q, px);
            if pos.isolated {
                let add = (notional(q, px) / pos.leverage.max(1) as u64) as i64;
                pos.isolated_raw_usd += add;
                *acct.usd_mut(dex) -= add;
            }
            return 0;
        }
        let size = pos.szi_x8.unsigned_abs();
        let closing = q.min(size);
        let closed_entry = (pos.entry_ntl as u128 * closing as u128 / size as u128) as u64;
        let exit = notional(closing, px) as i64;
        let realized = if pos.szi_x8 > 0 { exit - closed_entry as i64 } else { closed_entry as i64 - exit };
        let released = if pos.isolated { (pos.isolated_raw_usd as i128 * closing as i128 / size as i128) as i64 } else { 0 };
        pos.isolated_raw_usd -= released;
        pos.entry_ntl -= closed_entry;
        pos.szi_x8 += if is_buy { closing as i64 } else { -(closing as i64) };
        let left = q - closing;
        let mut reopen = 0i64;
        if left > 0 {
            pos.szi_x8 = if is_buy { left as i64 } else { -(left as i64) };
            pos.entry_ntl = notional(left, px);
            if pos.isolated {
                reopen = (notional(left, px) / pos.leverage.max(1) as u64) as i64;
                pos.isolated_raw_usd += reopen;
            }
        }
        if left > 0 || pos.szi_x8 == 0 {
            // The position closed (and may have reopened the other way): funding since open starts over.
            pos.funding_before += pos.funding_paid;
            pos.funding_paid = 0;
            pos.funding_at_change = 0;
            pos.max_leverage = 0;
        }
        if pos.szi_x8 == 0 {
            pos.entry_ntl = 0;
            pos.isolated_raw_usd = 0;
        }
        *acct.usd_mut(dex) += realized + released - reopen;
        realized
    }

    fn fmt_sz(&self, sz_x8: u64) -> String {
        fmt_dec(sz_x8 as i128, 8)
    }

    /// A limit order: matched against the book and other users' resting
    /// orders, then the remainder rests (GTC, ALO) or is dropped (IOC).
    #[allow(clippy::too_many_arguments)]
    fn limit(&mut self, user: Address, order_asset: u32, is_buy: bool, px: u64, mut sz: u64, reduce_only: bool, tif: u8, cloid: u128, applied: &mut Applied) -> String {
        let asset = match order_asset {
            a if a >= HIP3_ASSET_BASE => a - HIP3_ASSET_BASE,
            a if a >= SPOT_ASSET_BASE => return "rejected: spot orders are not modeled".into(),
            a => a,
        };
        if !self.perps.contains_key(&asset) || !self.books.contains_key(&asset) {
            return format!("rejected: no book for perp {asset}, run chainsim set <name> hypercore book {} --live", self.coin(asset));
        }
        if let Some(t) = self.perps.get(&asset).map(|p| p.collateral).filter(|t| *t != 0) {
            return format!("rejected: the dex of {} uses token {t} as collateral, and only USDC dexes are modeled", self.coin(asset));
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
        // A level trades at its price on the exchange's tick and is found by its stored price.
        let book = &self.books[&asset];
        let side = if is_buy { &book.asks } else { &book.bids };
        let crosses = |p: u64| if is_buy { p <= px } else { p >= px };
        let mut makers: Vec<(u64, Option<u64>, u64)> = side.iter().map(|(p, _)| (self.level_px(asset, *p, is_buy), None, *p)).filter(|m| crosses(m.0)).collect();
        makers.extend(self.orders.iter().filter(|o| o.asset == asset && o.is_buy != is_buy && o.user != user && crosses(o.px_x8)).map(|o| (o.px_x8, Some(o.oid), o.px_x8)));
        let best_first = |a: u64, b: u64| if is_buy { a.cmp(&b) } else { b.cmp(&a) };
        makers.sort_by(|a, b| best_first(a.0, b.0).then(a.1.is_some().cmp(&b.1.is_some())).then(best_first(a.2, b.2)));
        makers.dedup();
        if tif == 1 && !makers.is_empty() {
            return "rejected: ALO order would cross the book".into();
        }
        // Margin for the part that adds exposure, at the limit price.
        let reducing = if pos != 0 && (pos > 0) != is_buy { sz.min(pos.unsigned_abs()) } else { 0 };
        let need = notional(sz - reducing, px) / self.leverage_of(user, asset) as u64;
        if need > self.margin_dex(user, dex_of(asset)).free {
            return format!("rejected: insufficient margin, needs {} USD", fmt_dec(need as i128, 6));
        }
        let order_oid = self.next_oid.max(1);
        self.next_oid = order_oid + 1;
        let mut left = sz;
        let (mut filled, mut cost, mut taker_fee) = (0u64, 0u128, 0u64);
        for (mpx, oid, stored) in makers {
            if left == 0 {
                break;
            }
            let q = match oid {
                None => {
                    let book = self.books.get_mut(&asset).expect("book");
                    let side = if is_buy { &mut book.asks } else { &mut book.bids };
                    let Some(i) = side.iter().position(|(p, _)| *p == stored) else { continue };
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
                    let f = fee(notional(q, mpx), true);
                    self.fill(maker, asset, !is_buy, q, mpx, oid, false, f, false);
                    let coin = self.coin(asset);
                    self.note(maker, format!("{} {} {coin} at {} as maker, fee {} USD", if is_buy { "sold" } else { "bought" }, self.fmt_sz(q), fmt_dec(mpx as i128, 8), fmt_dec(f as i128, 6)));
                    applied.users.insert(maker);
                    q
                }
            };
            let f = fee(notional(q, mpx), false);
            self.fill(user, asset, is_buy, q, mpx, order_oid, true, f, false);
            taker_fee += f;
            left -= q;
            filled += q;
            cost += q as u128 * mpx as u128;
        }
        let mut parts = Vec::new();
        if filled > 0 {
            parts.push(format!("filled {} at {}, fee {} USD", self.fmt_sz(filled), fmt_dec((cost / filled as u128) as i128, 8), fmt_dec(taker_fee as i128, 6)));
        }
        if left > 0 {
            if tif == 3 {
                parts.push(format!("{} dropped (IOC)", self.fmt_sz(left)));
            } else {
                let time_ms = self.now * 1000;
                self.orders.push(Order { oid: order_oid, cloid, user, asset, is_buy, px_x8: px, sz_x8: left, tif, time_ms, orig_sz_x8: sz });
                parts.push(format!("{} resting as oid {order_oid}", self.fmt_sz(left)));
            }
        }
        parts.join(", ")
    }

    fn cancel(&mut self, user: Address, order_asset: u32, matches: impl Fn(&Order) -> bool) -> String {
        let asset = if order_asset >= HIP3_ASSET_BASE { order_asset - HIP3_ASSET_BASE } else { order_asset };
        match self.orders.iter().position(|o| o.user == user && o.asset == asset && matches(o)) {
            Some(i) => format!("canceled oid {}", self.orders.remove(i).oid),
            None => "rejected: no such open order".into(),
        }
    }

    /// The EVM amount for a Core amount of `token`, or None when the token has no EVM side.
    fn evm_amount(&self, token: u64, wei: u64) -> Option<(Option<Address>, U256)> {
        if token == self.hype() {
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
        let v = if token == self.hype() {
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
        let to_evm = system_token_for(destination, self.hype()) == Some(token);
        let evm = if to_evm {
            match self.evm_amount(token, wei) {
                Some(e) => Some(e),
                None => return format!("rejected: token {token} has no EVM side"),
            }
        } else {
            None
        };
        if let Err(why) = self.debit_spot(user, token, wei) {
            return why;
        }
        if let Some((evm, amount)) = evm {
            self.queue_credit(user, evm, amount, applied);
            return format!("sent {wei} wei of token {token} to the EVM");
        }
        self.credit_spot(destination, token, wei);
        applied.users.insert(destination);
        format!("sent {wei} wei of token {token} to {destination}")
    }

    fn usd_class(&mut self, user: Address, ntl: u64, to_perp: bool) -> String {
        // Spot USDC (token 0) has 8 wei decimals, perp USD 6.
        let wei = ntl * 100;
        if to_perp {
            if let Err(why) = self.debit_spot(user, 0, wei) {
                return why.replace("spot balance of token 0", "spot USDC");
            }
            self.account(user).usd += ntl as i64;
        } else {
            if ntl > self.margin(user).withdrawable {
                return "rejected: insufficient withdrawable perp USD".into();
            }
            self.account(user).usd -= ntl as i64;
            self.credit_spot(user, 0, wei);
        }
        format!("moved {} USD to {}", fmt_dec(ntl as i128, 6), if to_perp { "perp" } else { "spot" })
    }

    /// sendAsset (action 13): USDC between perp dexes and spot (dex
    /// [`SPOT_DEX`]), to the sender or another user. Perp dex balances are USD
    /// x6, spot USDC is wei with 8 decimals.
    #[allow(clippy::too_many_arguments)]
    fn send_asset(&mut self, user: Address, destination: Address, source_dex: u32, destination_dex: u32, token: u64, wei: u64, applied: &mut Applied) -> String {
        if token != 0 {
            return format!("rejected: sendAsset of token {token} is not modeled, only USDC");
        }
        let ntl = wei / 100;
        if source_dex == SPOT_DEX {
            if let Err(why) = self.debit_spot(user, 0, wei) {
                return why;
            }
        } else {
            if ntl > self.margin_dex(user, source_dex).withdrawable {
                return format!("rejected: insufficient withdrawable USD on perp dex {source_dex}");
            }
            *self.account(user).usd_mut(source_dex) -= ntl as i64;
        }
        if destination_dex == SPOT_DEX {
            self.credit_spot(destination, 0, ntl * 100);
        } else {
            *self.account(destination).usd_mut(destination_dex) += ntl as i64;
        }
        applied.users.insert(destination);
        let side = |d: u32| if d == SPOT_DEX { "spot".to_string() } else { format!("perp dex {d}") };
        format!("moved {} USD from {} to {} of {destination}", fmt_dec(ntl as i128, 6), side(source_dex), side(destination_dex))
    }

    fn coin(&self, asset: u32) -> String {
        self.perps.get(&asset).map_or_else(|| asset.to_string(), |p| p.name.clone())
    }

    /// Adds a history line to an account, keeping the newest ones.
    fn note(&mut self, user: Address, text: String) {
        let now = self.now;
        let h = &mut self.account(user).history;
        h.push((now, text));
        let drop = h.len().saturating_sub(MAX_HISTORY);
        h.drain(..drop);
    }

    /// Takes a fee from the USD balance of a perp's dex.
    fn charge(&mut self, user: Address, perp: u32, usd: u64) {
        *self.account(user).usd_mut(dex_of(perp)) -= usd as i64;
    }

    fn queue_credit(&mut self, to: Address, token: Option<Address>, amount: U256, applied: &mut Applied) {
        let id = self.next_credit.max(1);
        self.next_credit = id + 1;
        let c = Credit { id, to, token, amount, base: None };
        self.pending.push(c.clone());
        applied.credits.push(c);
    }

    /// Records the EVM balance read before a credit's first attempt.
    pub fn credit_base(&mut self, id: u64, base: U256) {
        if let Some(c) = self.pending.iter_mut().find(|c| c.id == id && c.base.is_none()) {
            c.base = Some(base);
        }
    }

    /// A credit landed on the EVM side: it leaves the queue, so it is never applied again.
    pub fn credit_landed(&mut self, id: u64) {
        self.pending.retain(|c| c.id != id);
    }

    /// The maintenance margin of a user's positions at the current marks, x6:
    /// half the initial margin at the max leverage of the position's margin
    /// tier, less the tier's maintenance deduction.
    pub fn maintenance(&self, user: Address) -> u64 {
        self.maintenance_dex(user, 0)
    }

    /// The cross maintenance margin of a user's positions on one dex.
    pub fn maintenance_dex(&self, user: Address, dex: u32) -> u64 {
        let Some(acct) = self.accounts.get(&user) else { return 0 };
        acct.positions
            .iter()
            .filter(|(a, p)| p.szi_x8 != 0 && !p.isolated && dex_of(**a) == dex)
            .map(|(a, p)| {
                self.maintenance_of(*a, p, notional(p.szi_x8.unsigned_abs(), self.mark_of(*a, p)))
            })
            .sum()
    }

    /// Moves a perp's mark and oracle to `px` at block time `time`. The
    /// perp's book levels move by the same ratio with their sizes kept (the
    /// stored prices stay exact and leave the model on the tick), then
    /// the resting orders the price or the moved book crosses fill (as
    /// makers, at their own price), positions reprice at the new mark and
    /// accounts below maintenance margin are liquidated.
    pub fn set_price(&mut self, asset: u32, px: u64, time: u64) -> Applied {
        self.now = time;
        let mut applied = Applied::default();
        if let Some(p) = self.perps.get_mut(&asset) {
            if let Some(book) = self.books.get_mut(&asset) {
                book.scale(px, p.mark_x8);
            }
            p.mark_x8 = px;
            p.oracle_x8 = px;
        }
        let (bid, ask) = self.books.get(&asset).map_or((None, None), |b| (b.bids.first().map(|l| self.level_px(asset, l.0, false)), b.asks.first().map(|l| self.level_px(asset, l.0, true))));
        let crossed: Vec<Order> = self
            .orders
            .iter()
            .filter(|o| o.asset == asset && if o.is_buy { o.px_x8 >= px || ask.is_some_and(|a| o.px_x8 >= a) } else { o.px_x8 <= px || bid.is_some_and(|b| o.px_x8 <= b) })
            .cloned()
            .collect();
        for o in crossed {
            self.orders.retain(|x| x.oid != o.oid);
            let f = fee(notional(o.sz_x8, o.px_x8), true);
            self.fill(o.user, asset, o.is_buy, o.sz_x8, o.px_x8, o.oid, false, f, false);
            let coin = self.coin(asset);
            self.note(o.user, format!("oid {} {} {} {coin} at {} as the price moved to {}, fee {} USD", o.oid, if o.is_buy { "bought" } else { "sold" }, self.fmt_sz(o.sz_x8), fmt_dec(o.px_x8 as i128, 8), fmt_dec(px as i128, 8), fmt_dec(f as i128, 6)));
            applied.users.insert(o.user);
            applied.fills += 1;
        }
        self.liquidate(&mut applied);
        // Everyone holding this perp sees new margin figures.
        applied.margin_users.extend(self.accounts.iter().filter(|(_, a)| a.positions.get(&asset).is_some_and(|p| p.szi_x8 != 0)).map(|(u, _)| *u));
        applied.reserves = self.take_reserves_dirty();
        applied
    }

    /// Replaces a perp's book (a refresh from the info API) at block time
    /// `time`. Resting orders the new book crosses (a bid at or above its best
    /// ask, an ask at or below its best bid) fill at their own price as
    /// makers, as on a price move, and liquidation follows.
    pub fn set_book(&mut self, asset: u32, book: Book, time: u64) -> Applied {
        self.now = time;
        let mut applied = Applied::default();
        let (best_bid, best_ask) = (book.bids.first().map(|l| l.0), book.asks.first().map(|l| l.0));
        self.books.insert(asset, book);
        let crossed: Vec<Order> = self
            .orders
            .iter()
            .filter(|o| o.asset == asset && if o.is_buy { best_ask.is_some_and(|a| o.px_x8 >= a) } else { best_bid.is_some_and(|b| o.px_x8 <= b) })
            .cloned()
            .collect();
        for o in crossed {
            self.orders.retain(|x| x.oid != o.oid);
            let f = fee(notional(o.sz_x8, o.px_x8), true);
            self.fill(o.user, asset, o.is_buy, o.sz_x8, o.px_x8, o.oid, false, f, false);
            let coin = self.coin(asset);
            self.note(o.user, format!("oid {} {} {} {coin} at {} as the book was refreshed, fee {} USD", o.oid, if o.is_buy { "bought" } else { "sold" }, self.fmt_sz(o.sz_x8), fmt_dec(o.px_x8 as i128, 8), fmt_dec(f as i128, 6)));
            applied.users.insert(o.user);
            applied.fills += 1;
        }
        if applied.fills > 0 {
            self.liquidate(&mut applied);
        }
        applied
    }

    /// Adds a perp the state does not track yet from the meta snapshot `up`
    /// took, so a price set on it has a mark to move. Returns false when the
    /// snapshot has no such perp.
    pub fn add_perp_from_meta(&mut self, asset: u32, meta: &Meta) -> bool {
        if self.perps.contains_key(&asset) {
            return true;
        }
        let Some(u) = meta.universe.get(asset as usize) else { return false };
        let ctx = meta.ctxs.get(asset as usize);
        let px = |i: usize| ctx.and_then(|c| c.get(i)).and_then(|v| v.as_deref()).and_then(|s| parse_dec(s, 8)).unwrap_or(0) as u64;
        let funding = ctx.and_then(|c| c.get(2)).and_then(|v| v.as_deref()).and_then(|s| parse_dec(s, 12)).unwrap_or(0) as i64;
        let tiers = meta.tiers.get(&asset).cloned().unwrap_or_default();
        self.perps.insert(asset, Perp { name: u.0.clone(), sz_decimals: u.1, max_leverage: u.2, mark_x8: px(0), oracle_x8: px(1), funding_e12: funding, tiers, ..Default::default() });
        true
    }

    /// updateIsolatedMargin: moves `ntli` (USD x6, positive to add) between
    /// the dex's USD balance and an isolated position's margin. Adding needs
    /// that much withdrawable on the dex. Removing must leave the position at
    /// least max(its initial margin, 10% of its value).
    pub fn update_isolated_margin(&mut self, user: Address, perp: u32, ntli: i64) -> std::result::Result<String, String> {
        let coin = self.coin(perp);
        let dex = dex_of(perp);
        let p = self.accounts.get(&user).and_then(|a| a.positions.get(&perp)).filter(|p| p.szi_x8 != 0).copied().ok_or_else(|| format!("{user} has no open {coin} position"))?;
        if !p.isolated {
            return Err(format!("the {coin} position uses cross margin, and only an isolated position takes margin"));
        }
        if ntli == 0 {
            return Err("the margin change is zero".into());
        }
        if ntli > 0 {
            let free = self.margin_dex(user, dex).withdrawable;
            if ntli as u64 > free {
                return Err(format!("adding {} USD needs that much withdrawable on the dex, which has {} USD", fmt_dec(ntli as i128, 6), fmt_dec(free as i128, 6)));
            }
        } else {
            let value = notional(p.szi_x8.unsigned_abs(), self.mark_of(perp, &p));
            let required = (value / p.leverage.max(1) as u64).max(value / 10);
            let (equity, _) = self.isolated_health(perp, &p);
            if equity + ntli < required as i64 {
                return Err(format!("removing {} USD would leave {} USD of margin, below the {} USD the position needs (its initial margin or 10% of its value)", fmt_dec(-ntli as i128, 6), fmt_dec((equity + ntli) as i128, 6), fmt_dec(required as i128, 6)));
            }
        }
        let acct = self.account(user);
        acct.positions.get_mut(&perp).expect("position").isolated_raw_usd += ntli;
        *acct.usd_mut(dex) -= ntli;
        let margin = self.isolated_health(perp, &self.accounts[&user].positions[&perp]).0;
        let text = format!("{} {} USD of isolated margin on {coin}, margin now {} USD", if ntli > 0 { "added" } else { "removed" }, fmt_dec(ntli.unsigned_abs() as i128, 6), fmt_dec(margin as i128, 6));
        self.note(user, text.clone());
        Ok(text)
    }

    /// Sets whether an address's EVM transactions go into big blocks.
    pub fn set_big_blocks(&mut self, user: Address, on: bool) {
        self.account(user).big_blocks = on;
    }

    pub fn uses_big_blocks(&self, user: Address) -> bool {
        self.accounts.get(&user).is_some_and(|a| a.big_blocks)
    }

    /// One replayed candle: the block that carries it (funding up to its
    /// time, then liquidations), then the move to the candle's close.
    pub fn candle_step(&mut self, events: &[Logged], asset: u32, close: u64, head: (u64, String), time: u64) -> Applied {
        let mut applied = self.apply(events, head, time);
        let step = self.set_price(asset, close, time);
        applied.users.extend(step.users);
        applied.margin_users.extend(step.margin_users);
        applied.fills += step.fills;
        applied.liquidated.extend(step.liquidated);
        applied.reserves |= step.reserves;
        applied
    }

    /// Liquidates every account whose account value is below its maintenance
    /// margin: all positions close at the mark price, its open orders are
    /// canceled, and the remaining collateral stays with the account. A
    /// negative balance is reset to zero, the loss the backstop liquidator takes.
    fn liquidate(&mut self, applied: &mut Applied) {
        let users: Vec<Address> = self.accounts.iter().filter(|(_, a)| a.positions.values().any(|p| p.szi_x8 != 0)).map(|(u, _)| *u).collect();
        for user in users {
            // Isolated positions, each against its own margin.
            let isolated: Vec<(u32, Pos)> = self.accounts[&user].positions.iter().filter(|(_, p)| p.szi_x8 != 0 && p.isolated).map(|(a, p)| (*a, *p)).collect();
            for (asset, p) in isolated {
                let (equity, mm) = self.isolated_health(asset, &p);
                if equity >= mm as i64 {
                    continue;
                }
                let mark = self.mark_of(asset, &p);
                self.fill(user, asset, p.szi_x8 < 0, p.szi_x8.unsigned_abs(), mark, 0, true, 0, true);
                self.orders.retain(|o| !(o.user == user && o.asset == asset));
                // A loss past the position's own margin is the backstop's, not the dex balance's.
                let shortfall = (-equity).max(0);
                *self.account(user).usd_mut(dex_of(asset)) += shortfall;
                let absorbed = if shortfall > 0 { format!(", shortfall {} USD absorbed", fmt_dec(shortfall as i128, 6)) } else { String::new() };
                let coin = self.coin(asset);
                self.note(user, format!("liquidated: isolated {coin} equity {} USD below maintenance margin {} USD, closed {} {coin} at {}{absorbed}", fmt_dec(equity as i128, 6), fmt_dec(mm as i128, 6), fmt_dec(p.szi_x8 as i128, 8), fmt_dec(mark as i128, 8)));
                applied.users.insert(user);
                if !applied.liquidated.contains(&user) {
                    applied.liquidated.push(user);
                }
            }
            // Cross positions, per dex.
            let dexes: BTreeSet<u32> = self.accounts[&user].positions.iter().filter(|(_, p)| p.szi_x8 != 0 && !p.isolated).map(|(a, _)| dex_of(*a)).collect();
            for dex in dexes {
                let (value, mm) = (self.margin_dex(user, dex).account_value, self.maintenance_dex(user, dex));
                if value >= mm as i64 {
                    continue;
                }
                let open: Vec<(u32, i64)> = self.accounts[&user].positions.iter().filter(|(a, p)| p.szi_x8 != 0 && !p.isolated && dex_of(**a) == dex).map(|(a, p)| (*a, p.szi_x8)).collect();
                let mut closed = Vec::new();
                for (asset, szi) in open {
                    let mark = self.perps.get(&asset).map_or(0, |p| p.mark_x8);
                    self.fill(user, asset, szi < 0, szi.unsigned_abs(), mark, 0, true, 0, true);
                    closed.push(format!("{} {} at {}", fmt_dec(szi as i128, 8), self.coin(asset), fmt_dec(mark as i128, 8)));
                }
                self.orders.retain(|o| !(o.user == user && dex_of(o.asset) == dex));
                // A negative balance is the backstop liquidator's loss, not the account's.
                let acct = self.account(user);
                let shortfall = (-acct.usd_of(dex)).max(0);
                *acct.usd_mut(dex) += shortfall;
                let absorbed = if shortfall > 0 { format!(", shortfall {} USD absorbed", fmt_dec(shortfall as i128, 6)) } else { String::new() };
                self.note(user, format!("liquidated: account value {} USD below maintenance margin {} USD, closed {}{absorbed}", fmt_dec(value as i128, 6), fmt_dec(mm as i128, 6), closed.join(", ")));
                applied.users.insert(user);
                if !applied.liquidated.contains(&user) {
                    applied.liquidated.push(user);
                }
            }
        }
        self.lend_liquidate(applied);
    }

    /// Pays funding for every interval boundary up to `time`: each position
    /// pays size x oracle price x the perp's hourly rate, longs paying shorts
    /// when the rate is positive. Returns whether anything was paid.
    fn fund_until(&mut self, time: u64, applied: &mut Applied) -> bool {
        let mut paid = false;
        if self.funding_time == 0 {
            self.funding_time = time / FUNDING_INTERVAL * FUNDING_INTERVAL;
            return false;
        }
        while self.funding_time + FUNDING_INTERVAL <= time {
            self.funding_time += FUNDING_INTERVAL;
            self.now = self.funding_time;
            // Borrow and lend interest is indexed on the same hour.
            if self.accrue_interest(applied) {
                paid = true;
            }
            // HIP-3 perps set their rate from the book's premium at the hour.
            let hip3: Vec<(u32, i64)> = self
                .perps
                .iter()
                .filter_map(|(a, p)| {
                    let f = p.hip3_funding?;
                    let oracle = if p.oracle_x8 > 0 { p.oracle_x8 } else { p.mark_x8 };
                    let premium = hip3_premium_e12(self.books.get(a)?, oracle, impact_notional(&p.name))?;
                    Some((*a, f.hourly_e12(premium)))
                })
                .collect();
            for (a, rate) in hip3 {
                self.perps.get_mut(&a).expect("perp").funding_e12 = rate;
            }
            let users: Vec<Address> = self.accounts.keys().copied().collect();
            for user in users {
                let open: Vec<(u32, i64)> = self.accounts[&user].positions.iter().filter(|(_, p)| p.szi_x8 != 0).map(|(a, p)| (*a, p.szi_x8)).collect();
                for (asset, szi) in open {
                    let Some(perp) = self.perps.get(&asset) else { continue };
                    if perp.funding_e12 == 0 {
                        continue;
                    }
                    let px = if perp.oracle_x8 > 0 { perp.oracle_x8 } else { perp.mark_x8 };
                    let pay = (szi as i128 * px as i128 / X16_TO_X6 as i128 * perp.funding_e12 as i128 / 1_000_000_000_000) as i64;
                    let rate = fmt_dec(perp.funding_e12 as i128, 12);
                    let acct = self.account(user);
                    let isolated = acct.positions.get(&asset).is_some_and(|p| p.isolated);
                    if isolated {
                        // An isolated position pays from its own margin.
                        if let Some(p) = acct.positions.get_mut(&asset) {
                            p.isolated_raw_usd -= pay;
                        }
                    } else {
                        *acct.usd_mut(dex_of(asset)) -= pay;
                    }
                    if let Some(p) = acct.positions.get_mut(&asset) {
                        p.funding_paid += pay;
                    }
                    let coin = self.coin(asset);
                    self.note(user, format!("funding on {coin} at rate {rate}: {} {} USD", if pay >= 0 { "paid" } else { "received" }, fmt_dec(pay.unsigned_abs() as i128, 6)));
                    applied.margin_users.insert(user);
                    paid = true;
                }
            }
        }
        paid
    }

    /// Applies events in order and moves the cursor to `head` (number, hash),
    /// then pays funding up to the head's block time, with a liquidation check
    /// when funding was paid. Events
    /// at or below the cursor are skipped, so re-applying a range is a no-op.
    pub fn apply(&mut self, events: &[Logged], head: (u64, String), time: u64) -> Applied {
        let mut applied = Applied::default();
        self.now = time;
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
                        Action::SendAsset { destination, source_dex, destination_dex, token, wei } => self.send_asset(*user, destination, source_dex, destination_dex, token, wei, &mut applied),
                        Action::BorrowLend { op, token, wei } => self.borrow_lend(*user, op, token, wei),
                        Action::SetAbstraction { user: target, abstraction } => self.set_abstraction(*user, target, abstraction),
                        Action::Other(id) => format!("recorded: action {id} is not modeled"),
                    }
                }
                Event::HypeIn { user, amount } => match self.core_amount(self.hype(), *amount) {
                    Some(wei) => {
                        self.credit_spot(*user, self.hype(), wei);
                        applied.users.insert(*user);
                        format!("credited {wei} wei of HYPE on Core")
                    }
                    None => "rejected: amount too large".into(),
                },
                Event::TokenIn { contract, from, to, value } => {
                    let Some(token) = system_token_for(*to, self.hype()) else { continue };
                    let linked = self.tokens.get(&token).and_then(|t| t.evm.as_ref()).and_then(|(a, _)| a.parse::<Address>().ok()) == Some(*contract);
                    match self.core_amount(token, *value).filter(|_| linked) {
                        Some(wei) => {
                            self.credit_spot(*from, token, wei);
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
        // Liquidation follows what moves margin outside an order: funding
        // here, price moves in `set_price`.
        if self.fund_until(time, &mut applied) {
            self.now = time;
            self.liquidate(&mut applied);
        }
        applied.reserves = self.take_reserves_dirty();
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
                let pos = hypercore::Position { szi: self.sz_to_raw(*asset, p.szi_x8), entry_ntl: p.entry_ntl, isolated_raw_usd: p.isolated_raw_usd, leverage: p.leverage, is_isolated: p.isolated };
                // 0x800 takes a uint16 perp index, 0x813 a uint32 one.
                if let Ok(perp) = u16::try_from(*asset) {
                    out.push(hypercore::position(*user, perp, pos));
                }
                out.push(hypercore::position2(*user, *asset, pos));
            }
            out.extend(self.margin_answers(*user));
            out.extend(self.lend_user_answers(*user));
        }
        out
    }

    /// Withdrawable (0x803, main dex) and the margin summary (0x80f) of every dex the user has value on.
    fn margin_answers(&self, user: Address) -> Vec<Entry> {
        let mut dexes: BTreeSet<u32> = BTreeSet::from([0]);
        if let Some(a) = self.accounts.get(&user) {
            dexes.extend(a.dex_usd.keys());
            dexes.extend(a.positions.keys().map(|p| dex_of(*p)));
        }
        let mut out = Vec::new();
        for dex in dexes {
            let m = self.margin_dex(user, dex);
            if dex == 0 {
                out.push(Entry { precompile: hypercore::WITHDRAWABLE, calldata: word_addr(user).to_vec(), result: word_u(m.withdrawable as u128).to_vec() });
            }
            out.push(Entry {
                precompile: hypercore::ACCOUNT_MARGIN_SUMMARY,
                calldata: [word_u(dex as u128), word_addr(user)].concat(),
                result: [word_i(m.account_value), word_u(m.margin_used as u128), word_u(m.ntl_pos as u128), word_i(m.raw_usd)].concat(),
            });
        }
        out
    }

    /// The answers an apply or a price step changed: every answer for its
    /// users, the margin answers for its margin users, and the reserves when
    /// any moved.
    pub fn answers_for(&self, applied: &Applied) -> Vec<Entry> {
        let mut out = self.answers(&applied.users);
        for user in applied.margin_users.difference(&applied.users) {
            out.extend(self.margin_answers(*user));
        }
        if applied.reserves {
            out.extend(self.reserve_answers());
        }
        out
    }

    /// Whether reserves changed since the last call, for [`Applied::reserves`].
    /// Sets the block time of the step being applied.
    pub fn set_now(&mut self, time: u64) {
        self.now = time;
    }

    pub fn take_reserves_dirty(&mut self) -> bool {
        std::mem::take(&mut self.reserves_dirty)
    }

    pub fn now_secs(&self) -> u64 {
        self.now
    }

    /// The status lines of applied actions at these (block, log index) places.
    pub fn result_of(&self, at: (u64, u64)) -> Option<&str> {
        self.results.iter().rev().find(|o| o.at == at).map(|o| o.status.as_str())
    }

    /// A user's Core state in the info API's shapes: `clearinghouseState`,
    /// `spotClearinghouseState`, and the open orders.
    pub fn info(&self, user: Address, time_ms: u64) -> Value {
        let acct = self.accounts.get(&user).cloned().unwrap_or_default();
        let usd = |v: i128| fmt_dec(v, 6);
        let balances: Vec<Value> = acct
            .spot
            .iter()
            .map(|(token, s)| {
                let meta = self.tokens.get(token);
                let w = meta.map_or(8, |t| t.wei_decimals as u32);
                let mut b = json!({
                    "coin": meta.map_or_else(|| token.to_string(), |t| t.name.clone()), "token": token,
                    "total": fmt_dec(s.0 as i128, w), "hold": fmt_dec(s.1 as i128, w), "entryNtl": usd(s.2 as i128),
                });
                if let Some(l) = acct.lend.get(token) {
                    b["supplied"] = json!(fmt_dec(l.supply_value as i128, w));
                    b["borrowed"] = json!(fmt_dec(l.borrow_value as i128, w));
                }
                b
            })
            .collect();
        let orders: Vec<Value> = self
            .orders
            .iter()
            .filter(|o| o.user == user)
            .map(|o| {
                json!({
                    "coin": self.coin(o.asset), "asset": o.asset,
                    "side": if o.is_buy { "B" } else { "A" }, "limitPx": fmt_dec(o.px_x8 as i128, 8), "sz": fmt_dec(o.sz_x8 as i128, 8),
                    "oid": o.oid, "cloid": (o.cloid != 0).then(|| format!("0x{:032x}", o.cloid)),
                    "tif": match o.tif { 1 => "Alo", 2 => "Gtc", _ => "Ioc" },
                })
            })
            .collect();
        let mut dexes = serde_json::Map::new();
        let other: BTreeSet<u32> = acct.dex_usd.keys().copied().chain(acct.positions.keys().map(|p| dex_of(*p))).filter(|d| *d != 0).collect();
        for dex in other {
            dexes.insert(self.dex_name(dex), self.clearinghouse(user, dex, time_ms));
        }
        let mut out = json!({
            "user": user.to_string(),
            "clearinghouseState": self.clearinghouse(user, 0, time_ms),
            "spotClearinghouseState": { "balances": balances, "portfolioMarginEnabled": acct.abstraction == 3 },
            "openOrders": orders,
            "history": acct.history.iter().map(|(t, e)| json!({ "time": t * 1000, "event": e })).collect::<Vec<_>>(),
            "userFills": acct.fills.iter().rev().map(|f| self.fill_json(f)).collect::<Vec<_>>(),
        });
        if !dexes.is_empty() {
            out["dexes"] = Value::Object(dexes);
        }
        if !acct.lend.is_empty() {
            out["borrowLendUserState"] = self.lend_info(&json!({ "type": "borrowLendUserState", "user": user.to_string() })).and_then(Result::ok).unwrap_or(Value::Null);
        }
        out
    }

    /// The name of a builder-deployed dex, from its perps' "dex:coin" names.
    pub fn dex_name(&self, dex: u32) -> String {
        self.perps.iter().find(|(a, _)| dex_of(**a) == dex).and_then(|(_, p)| p.name.split_once(':').map(|(d, _)| d.to_string())).unwrap_or_else(|| dex.to_string())
    }

    /// The dex index of a dex name: "" is the main dex.
    pub fn dex_index(&self, name: &str, meta: &Meta) -> Option<u32> {
        if name.is_empty() {
            return Some(0);
        }
        let prefix = format!("{name}:");
        self.perps.iter().find(|(_, p)| p.name.starts_with(&prefix)).map(|(a, _)| dex_of(*a)).or_else(|| meta.dexes.get(name).map(|d| d.index))
    }

    /// One dex's clearinghouseState for a user, in the info API's shape.
    pub fn clearinghouse(&self, user: Address, dex: u32, time_ms: u64) -> Value {
        let m = self.margin_dex(user, dex);
        let usd = |v: i128| fmt_dec(v, 6);
        let acct = self.accounts.get(&user).cloned().unwrap_or_default();
        let maintenance = self.maintenance_dex(user, dex);
        let mut positions = Vec::new();
        let (mut iso_value, mut iso_margin, mut iso_ntl) = (0i128, 0i128, 0i128);
        for (asset, p) in acct.positions.iter().filter(|(a, p)| p.szi_x8 != 0 && dex_of(**a) == dex) {
            let perp = self.perps.get(asset);
            let size = p.szi_x8.unsigned_abs();
            let value = notional(size, self.mark_of(*asset, p)) as i128;
            let sign: i128 = if p.szi_x8 > 0 { 1 } else { -1 };
            let upnl = sign * (value - p.entry_ntl as i128);
            let margin = if p.isolated { p.isolated_raw_usd as i128 + upnl } else { value / p.leverage.max(1) as i128 };
            if p.isolated {
                iso_value += p.isolated_raw_usd as i128 + upnl;
                iso_margin += margin;
                iso_ntl += value;
            }
            // The info API gives the entry price with 6 minus szDecimals decimals, cut off.
            let cut = 10i128.pow(2 + perp.map_or(0, |x| (x.sz_decimals as u32).min(6)));
            let entry_px = (p.entry_ntl as u128 * X16_TO_X6 / size as u128) as i128 / cut * cut;
            let liquidation_px = if p.isolated { self.liquidation_px(*asset, p, p.isolated_raw_usd as i128 + upnl, self.maintenance_of(*asset, p, value as u64) as i128) } else { self.liquidation_px(*asset, p, m.account_value as i128, maintenance as i128) };
            let liquidation_px = liquidation_px.map(|px| fmt_dec(px as i128, 8));
            // Ten decimals, rounded half away from zero as the info API does.
            let roe = if p.entry_ntl > 0 {
                let (n, d) = (upnl * p.leverage as i128 * 10_000_000_000, p.entry_ntl as i128);
                (2 * n + n.signum() * d) / (2 * d)
            } else {
                0
            };
            let mut lev = json!({ "type": if p.isolated { "isolated" } else { "cross" }, "value": p.leverage });
            if p.isolated {
                lev["rawUsd"] = json!(usd((p.isolated_raw_usd as i128) - sign * p.entry_ntl as i128));
            }
            positions.push(json!({
                "type": "oneWay",
                "position": {
                    "coin": perp.map_or_else(|| asset.to_string(), |x| x.name.clone()),
                    "szi": fmt_dec(p.szi_x8 as i128, 8),
                    "leverage": lev,
                    "entryPx": fmt_dec(entry_px, 8),
                    "positionValue": usd(value),
                    "unrealizedPnl": usd(upnl),
                    "returnOnEquity": fmt_dec(roe, 10),
                    "liquidationPx": liquidation_px,
                    "marginUsed": usd(margin),
                    "maxLeverage": if p.max_leverage > 0 { p.max_leverage } else { perp.map_or(0, |x| x.max_leverage) },
                    "cumFunding": { "allTime": usd(p.funding_before as i128 + p.funding_paid as i128), "sinceOpen": usd(p.funding_paid as i128), "sinceChange": usd(p.funding_paid as i128 - p.funding_at_change as i128) },
                },
            }));
        }
        let cross = json!({
            "accountValue": usd(m.account_value as i128), "totalNtlPos": usd(m.ntl_pos as i128),
            "totalRawUsd": usd(m.raw_usd as i128), "totalMarginUsed": usd(m.margin_used as i128),
        });
        let total = json!({
            "accountValue": usd(m.account_value as i128 + iso_value), "totalNtlPos": usd(m.ntl_pos as i128 + iso_ntl),
            "totalRawUsd": usd(m.raw_usd as i128), "totalMarginUsed": usd(m.margin_used as i128 + iso_margin),
        });
        json!({
            "marginSummary": total, "crossMarginSummary": cross,
            "crossMaintenanceMarginUsed": usd(maintenance as i128),
            "withdrawable": usd(m.withdrawable as i128), "assetPositions": positions, "time": time_ms,
        })
    }

    /// The price a book level shows and trades at: its stored price on the
    /// perp's tick, so a level moved by an exact ratio stays a valid price.
    fn level_px(&self, asset: u32, px: u64, ask: bool) -> u64 {
        self.perps.get(&asset).map_or(px, |p| tick_px(px, p.sz_decimals, ask))
    }

    /// The best bid and ask of a perp's book on the copy, with its own resting orders.
    fn best(&self, asset: u32) -> (Option<u64>, Option<u64>) {
        let book = self.books.get(&asset);
        let bid = book.and_then(|b| b.bids.first().map(|l| self.level_px(asset, l.0, false))).into_iter().chain(self.orders.iter().filter(|o| o.asset == asset && o.is_buy).map(|o| o.px_x8)).max();
        let ask = book.and_then(|b| b.asks.first().map(|l| self.level_px(asset, l.0, true))).into_iter().chain(self.orders.iter().filter(|o| o.asset == asset && !o.is_buy).map(|o| o.px_x8)).min();
        (bid, ask)
    }

    /// The copy's book in the info API's `l2Book` shape: book levels at
    /// their tick price and resting orders, merged by price, best first.
    fn l2_book(&self, asset: u32, time_ms: u64) -> Value {
        let book = self.books.get(&asset).cloned().unwrap_or_default();
        let side = |levels: &Levels, buy: bool| -> Vec<Value> {
            let mut merged: BTreeMap<u64, (u64, u64)> = BTreeMap::new();
            for (p, sz) in levels {
                let e = merged.entry(self.level_px(asset, *p, !buy)).or_default();
                e.0 += sz;
                e.1 += 1;
            }
            for o in self.orders.iter().filter(|o| o.asset == asset && o.is_buy == buy) {
                let e = merged.entry(o.px_x8).or_default();
                e.0 += o.sz_x8;
                e.1 += 1;
            }
            let mut out: Vec<(u64, (u64, u64))> = merged.into_iter().collect();
            if buy {
                out.reverse();
            }
            out.into_iter().map(|(p, (sz, n))| json!({ "px": fmt_dec(p as i128, 8), "sz": fmt_dec(sz as i128, 8), "n": n })).collect()
        };
        json!({ "coin": self.coin(asset), "time": time_ms, "levels": [side(&book.bids, true), side(&book.asks, false)] })
    }

    /// A perp's asset context: the snapshot from `up`, with the copy's own
    /// mark, oracle, funding and mid where the copy tracks the perp.
    fn asset_ctx(&self, asset: u32, index: u32, meta: &Meta) -> Value {
        let mut ctx = serde_json::Map::new();
        if let Some(c) = meta.ctxs.get(index as usize) {
            for (k, v) in META_CTX_FIELDS.iter().zip(c) {
                ctx.insert(k.to_string(), json!(v));
            }
        }
        if let Some(p) = self.perps.get(&asset) {
            let oracle = if p.oracle_x8 > 0 { p.oracle_x8 } else { p.mark_x8 };
            ctx.insert("markPx".into(), json!(fmt_dec(p.mark_x8 as i128, 8)));
            ctx.insert("oraclePx".into(), json!(fmt_dec(oracle as i128, 8)));
            ctx.insert("funding".into(), json!(fmt_dec(p.funding_e12 as i128, 12)));
            if let (Some(b), Some(a)) = self.best(asset) {
                ctx.insert("midPx".into(), json!(fmt_dec(((b as u128 + a as u128) / 2) as i128, 8)));
            }
        }
        Value::Object(ctx)
    }

    fn universe(&self, meta: &Meta) -> Vec<Value> {
        meta.universe
            .iter()
            .map(|(name, d, lev, delisted)| {
                let mut u = json!({ "name": name, "szDecimals": d, "maxLeverage": lev });
                if *delisted {
                    u["isDelisted"] = json!(true);
                }
                u
            })
            .collect()
    }

    /// Answers one info API request from the copy's state, in the info API's
    /// own shapes: clearinghouseState, spotClearinghouseState, openOrders,
    /// userFills, meta, metaAndAssetCtxs, l2Book and allMids. Any other type,
    /// or a malformed request, is an error message.
    pub fn info_request(&self, meta: &Meta, req: &Value, time_ms: u64) -> std::result::Result<Value, String> {
        let kind = req["type"].as_str().ok_or("the request has no type")?;
        if let Some(answer) = self.lend_info(req) {
            return answer;
        }
        // Builder-deployed dexes by name, "" being the main dex.
        let dex_name = req["dex"].as_str().unwrap_or("");
        let dex = self.dex_index(dex_name, meta).ok_or_else(|| format!("the copy has no perp dex {dex_name}: run chainsim set <name> hypercore book {dex_name}:<coin> --live"))?;
        let empty = Meta::default();
        let dmeta = if dex == 0 { meta } else { meta.dexes.get(dex_name).map_or(&empty, |d| &*d.meta) };
        let user = || -> std::result::Result<Address, String> { req["user"].as_str().and_then(|u| u.parse().ok()).ok_or_else(|| format!("{kind} needs a user address")) };
        let asset_of = |coin: &str| -> Option<u32> {
            self.perps.iter().find(|(_, p)| p.name == coin).map(|(a, _)| *a).or_else(|| meta.universe.iter().position(|u| u.0 == coin).map(|i| i as u32))
        };
        Ok(match kind {
            "clearinghouseState" => self.clearinghouse(user()?, dex, time_ms),
            "spotClearinghouseState" => self.info(user()?, time_ms)["spotClearinghouseState"].clone(),
            "openOrders" => {
                let who = user()?;
                let mut orders: Vec<&Order> = self.orders.iter().filter(|o| o.user == who && dex_of(o.asset) == dex).collect();
                orders.sort_by(|a, b| b.oid.cmp(&a.oid));
                json!(orders
                    .iter()
                    .map(|o| {
                        let mut v = json!({
                            "coin": self.coin(o.asset), "side": if o.is_buy { "B" } else { "A" }, "limitPx": fmt_dec(o.px_x8 as i128, 8),
                            "sz": fmt_dec(o.sz_x8 as i128, 8), "oid": o.oid, "timestamp": o.time_ms, "origSz": fmt_dec(o.orig_sz_x8.max(o.sz_x8) as i128, 8),
                        });
                        if o.cloid != 0 {
                            v["cloid"] = json!(format!("0x{:032x}", o.cloid));
                        }
                        v
                    })
                    .collect::<Vec<_>>())
            }
            "userFills" => {
                let who = user()?;
                let fills = self.accounts.get(&who).map_or(&[][..], |a| a.fills.as_slice());
                json!(fills.iter().rev().map(|f| self.fill_json(f)).collect::<Vec<_>>())
            }
            "meta" => json!({ "universe": self.universe(dmeta) }),
            "metaAndAssetCtxs" => {
                let base = dex * DEX_SIZE;
                let tracked = self.perps.keys().filter(|a| dex_of(**a) == dex).max().map_or(0, |m| (*m - base) as usize + 1);
                let n = dmeta.universe.len().max(tracked);
                json!([{ "universe": self.universe(dmeta) }, (0..n as u32).map(|i| self.asset_ctx(base + i, i, dmeta)).collect::<Vec<_>>()])
            }
            "l2Book" => {
                let coin = req["coin"].as_str().ok_or("l2Book needs a coin")?;
                let asset = asset_of(coin).filter(|a| self.books.contains_key(a)).ok_or_else(|| format!("the copy has no book for {coin}: run chainsim set <name> hypercore book {coin} --live"))?;
                self.l2_book(asset, time_ms)
            }
            "allMids" => {
                let mut mids = serde_json::Map::new();
                for (i, u) in dmeta.universe.iter().enumerate() {
                    if let Some(m) = dmeta.ctxs.get(i).and_then(|c| c.get(7).cloned().flatten().or_else(|| c.first().cloned().flatten())) {
                        mids.insert(u.0.clone(), json!(m));
                    }
                }
                for (a, p) in self.perps.iter().filter(|(a, _)| dex_of(**a) == dex) {
                    let mid = match self.best(*a) {
                        (Some(b), Some(x)) => (b as u128 + x as u128) / 2,
                        _ => p.mark_x8 as u128,
                    };
                    mids.insert(p.name.clone(), json!(fmt_dec(mid as i128, 8)));
                }
                Value::Object(mids)
            }
            other => return Err(format!("the copy does not serve the info request type {other}")),
        })
    }

    fn fill_json(&self, f: &Fill) -> Value {
        let after = f.start_x8 + if f.is_buy { f.sz_x8 as i64 } else { -(f.sz_x8 as i64) };
        let dir = match (f.start_x8.signum(), after.signum()) {
            _ if f.liquidation => if f.start_x8 > 0 { "Close Long" } else { "Close Short" },
            (0, 1) | (1, 1) => if f.is_buy { "Open Long" } else { "Close Long" },
            (0, -1) | (-1, -1) => if f.is_buy { "Close Short" } else { "Open Short" },
            (1, 0) => "Close Long",
            (-1, 0) => "Close Short",
            (1, -1) => "Long > Short",
            (-1, 1) => "Short > Long",
            _ => "Open Long",
        };
        let mut v = json!({
            "coin": self.coin(f.asset), "px": fmt_dec(f.px_x8 as i128, 8), "sz": fmt_dec(f.sz_x8 as i128, 8), "side": if f.is_buy { "B" } else { "A" },
            "time": f.time_ms, "startPosition": fmt_dec(f.start_x8 as i128, 8), "dir": dir, "closedPnl": fmt_dec(f.closed_pnl as i128, 6),
            "hash": format!("0x{:064x}", f.tid), "oid": f.oid, "crossed": f.crossed, "fee": fmt_dec(f.fee as i128, 6), "tid": f.tid, "feeToken": "USDC",
        });
        if f.liquidation {
            v["liquidation"] = json!({ "method": "market", "markPx": fmt_dec(f.px_x8 as i128, 8) });
        }
        v
    }

    /// Sets a user's perp state from the info API's `clearinghouseState`.
    /// Perps must already be in `perps`; positions in other perps are skipped
    /// and returned by name.
    pub fn mirror_perps(&mut self, user: Address, ch: &Value) -> Vec<String> {
        self.mirror_dex(user, 0, ch)
    }

    /// Sets a user's positions and USD balance on one perp dex from that
    /// dex's `clearinghouseState`. Isolated positions keep their margin
    /// (marginUsed less unrealized PnL), and the dex balance is set so the
    /// cross account value matches at the copy's marks.
    pub fn mirror_dex(&mut self, user: Address, dex: u32, ch: &Value) -> Vec<String> {
        let by_name: BTreeMap<String, u32> = self.perps.iter().filter(|(a, _)| dex_of(**a) == dex).map(|(a, p)| (p.name.clone(), *a)).collect();
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
            let dec6 = |k: &str| p[k].as_str().and_then(|s| parse_dec(s, 6)).unwrap_or(0) as i64;
            let isolated_raw_usd = if isolated { dec6("marginUsed") - dec6("unrealizedPnl") } else { 0 };
            // The exact entry notional is the position value less the unrealized PnL. The entry price is cut off.
            let sign = if szi > 0 { 1 } else { -1 };
            let entry_ntl = match (p["positionValue"].as_str().and_then(|s| parse_dec(s, 6)), p["unrealizedPnl"].as_str().and_then(|s| parse_dec(s, 6))) {
                (Some(value), Some(upnl)) if value - sign * upnl > 0 => (value - sign * upnl) as u64,
                _ => notional(szi.unsigned_abs() as u64, entry_px as u64),
            };
            // An account with its own margin terms (an HLP vault at 50x on every perp) keeps them.
            let own_max = p["maxLeverage"].as_u64().unwrap_or(0) as u32;
            let max_leverage = if self.perps.get(&asset).is_some_and(|x| x.max_leverage_at(dec6("positionValue").unsigned_abs()) != own_max) { own_max } else { 0 };
            let funding = |k: &str| p["cumFunding"][k].as_str().and_then(|s| parse_dec(s, 6)).unwrap_or(0) as i64;
            let funding_paid = funding("sinceOpen");
            positions.insert(asset, Pos { szi_x8: szi as i64, entry_ntl, leverage, isolated, isolated_raw_usd, funding_paid, max_leverage, funding_before: funding("allTime") - funding_paid, funding_at_change: funding_paid - funding("sinceChange") });
        }
        let summary = if ch["crossMarginSummary"].is_object() { &ch["crossMarginSummary"] } else { &ch["marginSummary"] };
        let account_value = summary["accountValue"].as_str().and_then(|s| parse_dec(s, 6)).unwrap_or(0);
        let acct = self.account(user);
        // Old positions on this dex go to zero, so their answers are rewritten.
        for (a, p) in acct.positions.iter_mut() {
            if dex_of(*a) == dex {
                *p = Pos { leverage: p.leverage, funding_before: p.funding_before + p.funding_paid, ..Default::default() };
            }
        }
        acct.positions.extend(positions);
        *acct.usd_mut(dex) = 0;
        // Pick the USD balance that gives the same cross account value at our marks.
        let upnl = self.margin_dex(user, dex).account_value as i128;
        *self.account(user).usd_mut(dex) = (account_value - upnl) as i64;
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
        s.perps.insert(0, Perp { name: "BTC".into(), sz_decimals: 5, max_leverage: 40, mark_x8: 100 * ONE_X8 as u64, ..Default::default() });
        let x = |v: u64| v * ONE_X8 as u64;
        s.books.insert(0, Book { bids: vec![(x(99), x(1)), (x(98), x(3))], asks: vec![(x(101), x(1)), (x(102), x(2))], time: 1 });
        s.tokens.insert(0, TokenMeta { name: "USDC".into(), wei_decimals: 8, evm: None, sz_decimals: 8 });
        s.tokens.insert(HYPE_TOKEN, TokenMeta { name: "HYPE".into(), wei_decimals: 8, evm: None, sz_decimals: 2 });
        s.tokens.insert(200, TokenMeta { name: "TKN".into(), wei_decimals: 6, evm: Some(("0x00000000000000000000000000000000000000aa".into(), 12)), sz_decimals: 2 });
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
        let a = s.apply(&[act(block, user, payload)], (block, format!("0x{block:x}")), 0);
        a.outcomes.last().map(|o| o.status.clone()).unwrap_or_default()
    }

    #[test]
    fn a_buy_fills_fully_at_the_best_ask() {
        let mut s = state();
        let r = run(&mut s, 11, ALICE, limit_order(0, true, x8(101.0), x8(0.5), false, 3, 0));
        assert_eq!(r, "filled 0.5 at 101.0, fee 0.022725 USD");
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
        assert_eq!(r, "filled 3.0 at 101.66666666, fee 0.13725 USD, 1.0 resting as oid 1");
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
        assert_eq!(r, "filled 1.0 at 101.0, fee 0.04545 USD, 0.5 dropped (IOC)");
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
        let a = s.apply(&[act(12, ALICE, limit_order(0, true, x8(101.0), x8(0.3), false, 3, 0))], (12, "0xc".into()), 0);
        assert_eq!(a.outcomes[0].status, "filled 0.3 at 100.66666666, fee 0.01359 USD");
        assert!(a.users.contains(&BOB));
        assert_eq!(s.accounts[&BOB].positions[&0].szi_x8, -(x8(0.2) as i64));
        assert!(s.orders.is_empty());
    }

    #[test]
    fn closing_realizes_pnl_and_margin_limits_size() {
        let mut s = state();
        run(&mut s, 11, ALICE, limit_order(0, true, x8(101.0), x8(1.0), false, 3, 0));
        // Sell 1 at the best bid 99: 2 USD loss.
        assert_eq!(run(&mut s, 12, ALICE, limit_order(0, false, x8(99.0), x8(1.0), true, 3, 0)), "filled 1.0 at 99.0, fee 0.04455 USD");
        assert_eq!(s.accounts[&ALICE].usd, 9_998_000_000 - 45_450 - 44_550);
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
        let a = s.apply(&[act(11, ALICE, encode_action(6, &[word_addr(BOB), word_u(HYPE_TOKEN as u128), word_u(100_000_000)].concat()))], (11, "0xb".into()), 0);
        assert!(a.users.contains(&BOB) && a.credits.is_empty());
        assert_eq!((s.accounts[&ALICE].spot[&HYPE_TOKEN].0, s.accounts[&BOB].spot[&HYPE_TOKEN].0), (400_000_000, 100_000_000));
        // To 0x2222…2222: native HYPE on the EVM, 18 decimals.
        let a = s.apply(&[act(12, ALICE, encode_action(6, &[word_addr(HYPE_SYSTEM), word_u(HYPE_TOKEN as u128), word_u(150_000_000)].concat()))], (12, "0xc".into()), 0);
        assert_eq!(a.credits, vec![Credit { id: 1, to: ALICE, token: None, amount: U256::from(15u64) * U256::from(10u64).pow(U256::from(17u64)), base: None }]);
        assert_eq!(s.pending, a.credits);
        assert_eq!(s.accounts[&ALICE].spot[&HYPE_TOKEN].0, 250_000_000);
        // A linked ERC-20 with 12 extra decimals.
        let a = s.apply(&[act(13, ALICE, encode_action(6, &[word_addr(system_address(200)), word_u(200), word_u(2_000_000)].concat()))], (13, "0xd".into()), 0);
        assert_eq!(a.credits[0].token, Some("0x00000000000000000000000000000000000000aa".parse().unwrap()));
        assert_eq!(a.credits[0].amount, U256::from(2_000_000u64) * U256::from(10u64).pow(U256::from(12u64)));
        assert!(run(&mut s, 14, ALICE, encode_action(6, &[word_addr(BOB), word_u(0), word_u(1)].concat())).starts_with("rejected: insufficient"));
        // USDC to its system address: no EVM side, rejected before any debit.
        s.account(ALICE).spot.insert(0, Spot(9, 0, 0));
        assert_eq!(run(&mut s, 15, ALICE, encode_action(6, &[word_addr(system_address(0)), word_u(0), word_u(5)].concat())), "rejected: token 0 has no EVM side");
        assert_eq!(s.accounts[&ALICE].spot[&0].0, 9);
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
        let a = s.apply(&events, (11, "0xb".into()), 0);
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
        s.apply(&ev, (11, "0xb".into()), 0);
        let once = s.clone();
        let again = s.apply(&ev, (11, "0xb".into()), 0);
        assert!(again.outcomes.is_empty());
        assert_eq!(s, once);
        assert_eq!(s.result_of((11, 0)), Some("filled 0.5 at 101.0, fee 0.022725 USD"));
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
        // Mark 100: value 100, uPnL -1, margin 5, fee 0.04545, account value 9998.95455.
        // A transfer must leave the larger of the margin used and 10% of the notional, 10 here.
        assert_eq!(find(hypercore::WITHDRAWABLE).result, word_u(9_989_000_000 - 45_450).to_vec());
        let ams = find(hypercore::ACCOUNT_MARGIN_SUMMARY);
        assert_eq!(ams.calldata, [word_u(0), word_addr(ALICE)].concat());
        assert_eq!(ams.result, [word_i(9_999_000_000 - 45_450), word_u(5_000_000), word_u(100_000_000), word_i(9_899_000_000 - 45_450)].concat());
        let info = s.info(ALICE, 7);
        let p = &info["clearinghouseState"]["assetPositions"][0]["position"];
        assert_eq!((p["coin"].as_str(), p["szi"].as_str(), p["entryPx"].as_str(), p["unrealizedPnl"].as_str()), (Some("BTC"), Some("1.0"), Some("101.0"), Some("-1.0")));
        assert_eq!(info["clearinghouseState"]["withdrawable"], "9988.95455");
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

    fn usd(v: f64) -> i64 {
        (v * 1e6).round() as i64
    }

    #[test]
    fn a_price_move_reprices_positions_and_margin() {
        let mut s = state();
        run(&mut s, 11, ALICE, limit_order(0, true, x8(102.0), x8(2.0), false, 3, 0));
        // Long 1 at 101 and 1 at 102, fee 0.045% of 203.
        let fee = usd(0.09135);
        assert_eq!(s.accounts[&ALICE].usd, usd(10_000.0) - fee);
        let a = s.set_price(0, x8(110.0), 100);
        assert!(a.margin_users.contains(&ALICE) && a.liquidated.is_empty());
        let m = s.margin(ALICE);
        // Value 220, entry 203: uPnL 17. Margin 220 / 20 = 11.
        assert_eq!((m.ntl_pos, m.margin_used, m.account_value), (220_000_000, 11_000_000, usd(10_017.0) - fee));
        assert_eq!(s.perps[&0].oracle_x8, x8(110.0));
        let info = s.info(ALICE, 0);
        assert_eq!(info["clearinghouseState"]["assetPositions"][0]["position"]["unrealizedPnl"], "17.0");
        // The answers carry the new margin figures without a fill.
        let answers = s.answers_for(&a);
        assert!(answers.iter().any(|e| e.precompile == hypercore::WITHDRAWABLE && e.result == word_u(m.withdrawable as u128).to_vec()));
        assert!(!answers.iter().any(|e| e.precompile == hypercore::POSITION));
    }

    #[test]
    fn a_price_move_fills_resting_orders_it_crosses_as_maker() {
        let mut s = state();
        run(&mut s, 11, ALICE, limit_order(0, true, x8(95.0), x8(1.0), false, 2, 0));
        run(&mut s, 12, ALICE, limit_order(0, false, x8(130.0), x8(1.0), false, 2, 0));
        // 96 does not reach the bid at 95.
        assert_eq!(s.set_price(0, x8(96.0), 100).fills, 0);
        let a = s.set_price(0, x8(94.0), 200);
        assert_eq!(a.fills, 1);
        assert!(a.users.contains(&ALICE));
        let p = s.accounts[&ALICE].positions[&0];
        assert_eq!((p.szi_x8, p.entry_ntl), (x8(1.0) as i64, 95_000_000));
        // The maker fee, 0.015% of 95.
        assert_eq!(s.accounts[&ALICE].usd, usd(10_000.0) - usd(0.01425));
        assert_eq!(s.orders.len(), 1);
        assert!(s.accounts[&ALICE].history.last().unwrap().1.starts_with("oid 1 bought 1.0 BTC at 95.0 as the price moved to 94.0, fee 0.01425 USD"));
    }

    #[test]
    fn a_price_move_shifts_the_book_levels_and_keeps_resting_orders() {
        let mut s = state();
        run(&mut s, 11, ALICE, limit_order(0, true, x8(95.0), x8(1.0), false, 2, 0));
        // 3% down: every level moves 3% with its size, the bid at 95 stays where it was placed.
        assert_eq!(s.set_price(0, x8(97.0), 100).fills, 0);
        let b = &s.books[&0];
        assert_eq!(b.bids, vec![(x8(96.03), x8(1.0)), (x8(95.06), x8(3.0))]);
        assert_eq!(b.asks, vec![(x8(97.97), x8(1.0)), (x8(98.94), x8(2.0))]);
        assert_eq!((s.orders.len(), s.orders[0].px_x8), (1, x8(95.0)));
        // A further move through 95 fills it at its own price.
        let a = s.set_price(0, x8(94.0), 200);
        assert_eq!(a.fills, 1);
        assert_eq!(s.accounts[&ALICE].positions[&0].entry_ntl, 95_000_000);
        assert_eq!(s.books[&0].bids[0], (x8(93.06), x8(1.0)));
    }

    #[test]
    fn an_ioc_sell_after_a_price_move_fills_at_the_moved_best_bid() {
        let mut s = state();
        run(&mut s, 11, ALICE, limit_order(0, true, x8(101.0), x8(1.0), false, 3, 0));
        s.set_price(0, x8(97.0), 100);
        // The moved best bid, 96.03, trades on the 0.1 tick of a perp with szDecimals 5.
        assert!(run(&mut s, 12, ALICE, limit_order(0, false, x8(90.0), x8(1.0), true, 3, 0)).starts_with("filled 1.0 at 96.0,"));
        assert_eq!(s.books[&0].bids, vec![(x8(95.06), x8(3.0))]);
    }

    #[test]
    fn a_moved_book_that_reaches_a_resting_order_fills_it() {
        let mut s = state();
        // A book refreshed under the mark: asks from 95 while the mark is 100.
        s.books.insert(0, Book { bids: vec![(x8(94.0), x8(1.0))], asks: vec![(x8(95.0), x8(1.0))], time: 2 });
        run(&mut s, 11, ALICE, limit_order(0, true, x8(93.0), x8(1.0), false, 2, 0));
        // Mark 97, best ask 92.15 (92.2 on the tick): the bid at 93 is under the mark and inside the moved book.
        assert_eq!(s.set_price(0, x8(97.0), 100).fills, 1);
        assert!(s.orders.is_empty());
    }

    #[test]
    fn moved_levels_show_on_the_tick_and_merge_while_an_accounts_order_keeps_its_price() {
        let mut s = state();
        s.books.insert(0, Book { bids: vec![(x8(99.1), x8(1.0)), (x8(99.0), x8(2.0))], asks: vec![(x8(101.0), x8(1.0)), (x8(102.0), x8(2.0))], time: 1 });
        run(&mut s, 11, ALICE, limit_order(0, true, x8(45.3), x8(1.0), false, 2, 0));
        s.set_price(0, x8(50.0), 100);
        // Stored levels keep the exact ratio.
        assert_eq!(s.books[&0].bids, vec![(x8(49.55), x8(1.0)), (x8(49.5), x8(2.0))]);
        let m = meta();
        let book = s.info_request(&m, &json!({ "type": "l2Book", "coin": "BTC" }), 0).unwrap();
        // 49.55 and 49.5 are one level at 49.5, and Alice's bid stays at 45.3.
        assert_eq!(book["levels"][0], json!([{ "px": "49.5", "sz": "3.0", "n": 2 }, { "px": "45.3", "sz": "1.0", "n": 1 }]));
        assert_eq!(book["levels"][1], json!([{ "px": "50.5", "sz": "1.0", "n": 1 }, { "px": "51.0", "sz": "2.0", "n": 1 }]));
        assert_eq!(s.orders[0].px_x8, x8(45.3));
        // A sell through both merged levels fills 3 at 49.5.
        s.account(BOB).usd = usd(10_000.0);
        assert!(run(&mut s, 12, BOB, limit_order(0, false, x8(49.0), x8(3.0), false, 3, 0)).starts_with("filled 3.0 at 49.5,"));
        assert!(s.books[&0].bids.is_empty());
    }

    #[test]
    fn tick_prices_follow_the_exchange_rule() {
        // 5 significant figures, 6 minus szDecimals decimals, integers always valid.
        assert_eq!((tick_px(x8(72160.17299918), 5, false), tick_px(x8(72160.17299918), 5, true)), (x8(72160.0), x8(72161.0)));
        assert_eq!((tick_px(x8(1234.56), 2, false), tick_px(x8(1234.56), 2, true)), (x8(1234.5), x8(1234.6)));
        assert_eq!(tick_px(x8(0.0012345), 0, false), x8(0.001234));
        assert_eq!(tick_px(x8(0.012345), 1, false), x8(0.01234));
        assert_eq!(tick_px(x8(123456.0), 0, true), x8(123456.0));
        assert_eq!(tick_px(x8(123456.7), 0, true), x8(123457.0));
        for valid in [78258.0, 1234.5, 0.001234, 402.61, 99.0] {
            assert_eq!((tick_px(x8(valid), 0, false), tick_px(x8(valid), 0, true)), (x8(valid), x8(valid)));
        }
    }

    #[test]
    fn scaled_levels_round_apart_and_merge_on_one_price() {
        let mut b = Book { bids: vec![(3, 1), (2, 1)], asks: vec![(4, 1), (5, 1), (6, 1)], time: 0 };
        b.scale(1, 2);
        assert_eq!((b.bids, b.asks), (vec![(1, 2)], vec![(2, 1), (3, 2)]));
    }

    /// Bob on mainnet: short 1000 ALT (a 3x perp) held at 20x under his own 50x terms, and long 1 BTC.
    fn mirrored() -> State {
        let mut s = state();
        s.perps.insert(1, Perp { name: "ALT".into(), sz_decimals: 1, max_leverage: 3, mark_x8: x8(2.0), ..Default::default() });
        let ch = json!({
            "assetPositions": [
                { "position": { "coin": "ALT", "szi": "-1000.0", "entryPx": "2.10004", "positionValue": "2000.0", "unrealizedPnl": "100.049", "marginUsed": "100.0", "maxLeverage": 50,
                    "leverage": { "type": "cross", "value": 20 }, "cumFunding": { "allTime": "-50.5", "sinceOpen": "-1.5", "sinceChange": "-0.5" } } },
                { "position": { "coin": "BTC", "szi": "1.0", "entryPx": "101.0", "positionValue": "100.0", "unrealizedPnl": "-1.0", "marginUsed": "5.0", "maxLeverage": 40,
                    "leverage": { "type": "cross", "value": 20 }, "cumFunding": { "allTime": "0.0", "sinceOpen": "0.0", "sinceChange": "0.0" } } },
            ],
            "marginSummary": { "accountValue": "10000.0" },
        });
        assert!(s.mirror_perps(BOB, &ch).is_empty());
        s
    }

    #[test]
    fn a_mirrored_account_keeps_its_own_max_leverage_in_the_summary() {
        let s = mirrored();
        let ch = s.clearinghouse(BOB, 0, 0);
        // Margin at the position's leverage of 20, not the perp's 3: 2000 / 20 + 100 / 20.
        assert_eq!(ch["marginSummary"]["totalMarginUsed"], "105.0");
        let listed: i128 = ch["assetPositions"].as_array().unwrap().iter().map(|p| parse_dec(p["position"]["marginUsed"].as_str().unwrap(), 6).unwrap()).sum();
        assert_eq!(listed, 105_000_000);
        // Maintenance is half the initial margin at max leverage: 2000 / 100 at Bob's 50x, 100 / 80 at BTC's 40x.
        assert_eq!(ch["crossMaintenanceMarginUsed"], "21.25");
        // A transfer leaves max(105, 10% of 2100).
        assert_eq!((&ch["marginSummary"]["accountValue"], &ch["withdrawable"]), (&json!("10000.0"), &json!("9790.0")));
        let alt = &ch["assetPositions"][1]["position"];
        assert_eq!((&alt["coin"], &alt["maxLeverage"], &ch["assetPositions"][0]["position"]["maxLeverage"]), (&json!("ALT"), &json!(50), &json!(40)));
        assert_eq!(alt["cumFunding"], json!({ "allTime": "-50.5", "sinceOpen": "-1.5", "sinceChange": "-0.5" }));
        // Only the position whose terms differ from the perp's stores them.
        assert_eq!((s.accounts[&BOB].positions[&1].max_leverage, s.accounts[&BOB].positions[&0].max_leverage), (50, 0));
    }

    #[test]
    fn a_mirrored_position_keeps_its_unrealized_pnl_at_the_mirror_price() {
        let s = mirrored();
        let alt = &s.clearinghouse(BOB, 0, 0)["assetPositions"][1]["position"];
        // Entry notional 2100.049 from value and PnL. The cut-off entry price alone would give 100.04.
        assert_eq!((&alt["unrealizedPnl"], &alt["entryPx"], &alt["positionValue"]), (&json!("100.049"), &json!("2.10004"), &json!("2000.0")));
    }

    #[test]
    fn return_on_equity_rounds_to_ten_decimals() {
        let s = mirrored();
        let ch = s.clearinghouse(BOB, 0, 0);
        // ALT: 100.049 x 20 / 2100.049 = 0.95282538645..., and BTC: -1 x 20 / 101 = -0.19801980198...
        assert_eq!(ch["assetPositions"][1]["position"]["returnOnEquity"], "0.9528253865");
        assert_eq!(ch["assetPositions"][0]["position"]["returnOnEquity"], "-0.198019802");
    }

    #[test]
    fn liquidation_price_follows_the_documented_formula() {
        let s = mirrored();
        let ch = s.clearinghouse(BOB, 0, 0);
        // Short ALT: 2 + (10000 - 21.25) / 1000 / (1 + 1 / 100) = 11.87995049.
        assert_eq!(ch["assetPositions"][1]["position"]["liquidationPx"], "11.87995049");
        // Long BTC: 100 - 9978.75 / (1 - 1 / 80) is below zero, so no price liquidates it.
        assert_eq!(ch["assetPositions"][0]["position"]["liquidationPx"], Value::Null);
        // A long on thin margin: 1 BTC at 100 with 3 USD, 100 - (2 - 1.25) / (1 - 1 / 80) = 99.24050632.
        let mut s = state();
        s.account(BOB).usd = usd(3.0);
        s.account(BOB).positions.insert(0, Pos { szi_x8: x8(1.0) as i64, entry_ntl: 101_000_000, leverage: 20, ..Default::default() });
        assert_eq!(s.clearinghouse(BOB, 0, 0)["assetPositions"][0]["position"]["liquidationPx"], "99.24050632");
    }

    #[test]
    fn liquidation_price_takes_the_tier_it_ends_in() {
        // A perp at 10x up to 3,000 USD and 5x above, which deducts 3000 x (1 / 10 - 1 / 20) = 150.
        let mut s = state();
        s.perps.insert(1, Perp { name: "ALT".into(), sz_decimals: 1, max_leverage: 10, mark_x8: x8(2.0), tiers: vec![(0, 10), (3_000_000_000, 5)], ..Default::default() });
        s.account(BOB).usd = usd(10_000.0);
        s.account(BOB).positions.insert(1, Pos { szi_x8: -(x8(1000.0) as i64), entry_ntl: 2_000_000_000, leverage: 10, ..Default::default() });
        // Short 1000 at 2 with 10,000 USD of account value. The first tier would give a notional of
        // 11,428, outside it. In the second, v = (2000 + 10000 + 150) x 10 / 11 = 11045.45, so 11.04545454:
        // there the account value, 10000 - 9045.45, equals the maintenance margin, 11045.45 / 10 - 150.
        assert_eq!(s.clearinghouse(BOB, 0, 0)["assetPositions"][0]["position"]["liquidationPx"], "11.04545454");
    }

    #[test]
    fn an_account_opened_on_the_fork_keeps_its_summary() {
        let mut s = state();
        run(&mut s, 11, ALICE, limit_order(0, true, x8(101.0), x8(1.0), false, 3, 0));
        let ch = s.clearinghouse(ALICE, 0, 0);
        assert_eq!(ch["marginSummary"], json!({ "accountValue": "9998.95455", "totalNtlPos": "100.0", "totalRawUsd": "9898.95455", "totalMarginUsed": "5.0" }));
        assert_eq!(ch["crossMaintenanceMarginUsed"], "1.25");
        let p = &ch["assetPositions"][0]["position"];
        assert_eq!((&p["entryPx"], &p["maxLeverage"], &p["marginUsed"], &p["unrealizedPnl"]), (&json!("101.0"), &json!(40), &json!("5.0"), &json!("-1.0")));
        assert_eq!(s.accounts[&ALICE].positions[&0].max_leverage, 0);
        // An order may still use everything above the margin in use.
        assert_eq!(s.margin(ALICE).free, 9_993_954_550);
    }

    #[test]
    fn liquidation_at_the_maintenance_boundary() {
        // BTC max leverage 40: maintenance is 1/80 of notional.
        let mut s = state();
        s.account(BOB).usd = 0;
        s.account(BOB).positions.insert(0, Pos { szi_x8: x8(1.0) as i64, entry_ntl: 100_000_000, leverage: 20, ..Default::default() });
        s.account(ALICE).positions.insert(0, Pos { szi_x8: x8(1.0) as i64, entry_ntl: 100_000_000, leverage: 20, ..Default::default() });
        // At price p Bob's value is 1.25 + p - 100 and his maintenance p / 80.
        // At 100 both are 1.25: equal is not below, so he survives.
        s.account(BOB).usd = usd(1.25);
        let a = s.set_price(0, x8(100.0), 10);
        assert!(a.liquidated.is_empty(), "equal to maintenance survives");
        let a = s.set_price(0, x8(99.99), 20);
        // value 1.24, maintenance 99.99 / 80 = 1.249875: below, liquidated.
        assert_eq!(a.liquidated, vec![BOB]);
        let bob = &s.accounts[&BOB];
        assert_eq!(bob.positions[&0].szi_x8, 0);
        // Closed at the mark: the loss is realized, the rest stays.
        assert_eq!(bob.usd, usd(1.24));
        assert!(bob.history.last().unwrap().1.starts_with("liquidated: account value 1.24 USD below maintenance margin 1.249875 USD"));
        // Alice has 10,000 USD behind the same position and survives.
        assert_eq!(s.accounts[&ALICE].positions[&0].szi_x8, x8(1.0) as i64);
        // The 0x800 answer for Bob reads zero.
        let pos = s.answers(&BTreeSet::from([BOB])).into_iter().find(|e| e.precompile == hypercore::POSITION).unwrap();
        assert_eq!(pos.result, [word_i(0), word_u(0), word_i(0), word_u(20), word_u(0)].concat());
    }

    #[test]
    fn a_plain_block_does_not_liquidate() {
        let mut s = state();
        s.account(BOB).positions.insert(0, Pos { szi_x8: x8(1.0) as i64, entry_ntl: 100_000_000, leverage: 20, ..Default::default() });
        let t0 = 1_793_599_200;
        s.apply(&[], (11, "0xb".into()), t0 + 10);
        let a = s.apply(&[], (12, "0xc".into()), t0 + 20);
        assert!(a.liquidated.is_empty());
        assert_eq!(s.accounts[&BOB].positions[&0].szi_x8, x8(1.0) as i64);
        // A funding payment is a margin move: it runs the check.
        s.perps.get_mut(&0).unwrap().funding_e12 = 1;
        let a = s.apply(&[], (13, "0xd".into()), t0 + 3600);
        assert_eq!(a.liquidated, vec![BOB]);
    }

    #[test]
    fn liquidation_cancels_open_orders() {
        let mut s = state();
        s.account(BOB).usd = usd(1.0);
        s.account(BOB).positions.insert(0, Pos { szi_x8: -(x8(1.0) as i64), entry_ntl: 100_000_000, leverage: 20, ..Default::default() });
        s.orders.push(Order { oid: 9, cloid: 0, user: BOB, asset: 0, is_buy: false, px_x8: x8(200.0), sz_x8: 1, tif: 2, time_ms: 0, orig_sz_x8: 1 });
        let a = s.set_price(0, x8(101.0), 10);
        assert_eq!(a.liquidated, vec![BOB]);
        assert!(s.orders.is_empty());
        assert_eq!(s.accounts[&BOB].usd, 0);
        // Value 1 - 1 = 0 at 101: no shortfall. A gap through zero leaves none on the account.
        s.account(BOB).usd = usd(1.0);
        s.account(BOB).positions.insert(0, Pos { szi_x8: x8(1.0) as i64, entry_ntl: 101_000_000, leverage: 20, ..Default::default() });
        s.set_price(0, x8(90.0), 20);
        assert_eq!(s.accounts[&BOB].usd, 0);
        assert!(s.accounts[&BOB].history.last().unwrap().1.ends_with("shortfall 10.0 USD absorbed"), "{:?}", s.accounts[&BOB].history);
    }

    #[test]
    fn funding_over_two_intervals_with_both_signs() {
        let mut s = state();
        s.perps.get_mut(&0).unwrap().funding_e12 = 100_000_000; // 0.0001 per hour
        s.account(BOB).usd = usd(1_000.0);
        s.account(BOB).positions.insert(0, Pos { szi_x8: x8(2.0) as i64, entry_ntl: 200_000_000, leverage: 20, ..Default::default() });
        s.account(ALICE).positions.insert(0, Pos { szi_x8: -(x8(1.0) as i64), entry_ntl: 100_000_000, leverage: 20, ..Default::default() });
        let t0 = 1_793_599_200; // on the hour
        s.apply(&[], (11, "0xb".into()), t0 + 10);
        // The first interval: the long pays 2 x 100 x 0.0001 = 0.02, the short receives 0.01.
        s.apply(&[], (12, "0xc".into()), t0 + 3600);
        assert_eq!(s.accounts[&BOB].usd, usd(1_000.0) - usd(0.02));
        assert_eq!(s.accounts[&ALICE].usd, usd(10_000.0) + usd(0.01));
        // A negative rate: shorts pay, longs receive.
        s.perps.get_mut(&0).unwrap().funding_e12 = -50_000_000;
        let a = s.apply(&[], (13, "0xd".into()), t0 + 7200 + 5);
        assert!(a.margin_users.contains(&BOB) && a.margin_users.contains(&ALICE));
        assert_eq!(s.accounts[&BOB].usd, usd(1_000.0) - usd(0.02) + usd(0.01));
        assert_eq!(s.accounts[&BOB].positions[&0].funding_paid, usd(0.01));
        assert_eq!(s.accounts[&ALICE].positions[&0].funding_paid, -usd(0.01) + usd(0.005));
        assert_eq!(s.info(BOB, 0)["clearinghouseState"]["assetPositions"][0]["position"]["cumFunding"]["sinceOpen"], "0.01");
        // No new boundary: nothing more is paid.
        s.apply(&[], (14, "0xe".into()), t0 + 7200 + 3000);
        assert_eq!(s.accounts[&BOB].positions[&0].funding_paid, usd(0.01));
        assert_eq!(s.funding_time, t0 + 7200);
    }

    #[test]
    fn fees_on_taker_and_maker_fills() {
        let mut s = state();
        s.account(BOB).usd = usd(1_000.0);
        run(&mut s, 11, BOB, limit_order(0, false, x8(100.5), x8(2.0), false, 2, 0));
        assert_eq!(run(&mut s, 12, ALICE, limit_order(0, true, x8(100.5), x8(2.0), false, 3, 0)), "filled 2.0 at 100.5, fee 0.09045 USD");
        // Taker: 0.045% of 201. Maker: 0.015% of 201.
        assert_eq!(s.accounts[&ALICE].usd, usd(10_000.0) - usd(0.09045));
        assert_eq!(s.accounts[&BOB].usd, usd(1_000.0) - usd(0.03015));
        assert_eq!((fee(1_000_000, false), fee(1_000_000, true)), (450, 150));
    }

    #[test]
    fn candle_replay_walks_a_fixed_path() {
        let mut s = state();
        s.perps.get_mut(&0).unwrap().funding_e12 = 100_000_000;
        s.account(BOB).usd = usd(3.0);
        s.account(BOB).positions.insert(0, Pos { szi_x8: x8(1.0) as i64, entry_ntl: 100_000_000, leverage: 20, ..Default::default() });
        run(&mut s, 11, ALICE, limit_order(0, true, x8(97.0), x8(1.0), false, 2, 0));
        let t0 = 1_793_599_200;
        s.apply(&[], (12, "0xc".into()), t0);
        let path = [100.0, 98.5, 96.0, 99.0];
        let mut fills = 0;
        let mut liquidated = Vec::new();
        for (i, close) in path.iter().enumerate() {
            let a = s.candle_step(&[], 0, x8(*close), (13 + i as u64, format!("0x{i}")), t0 + 3600 * (i as u64 + 1));
            fills += a.fills;
            liquidated.extend(a.liquidated);
        }
        assert_eq!(s.perps[&0].mark_x8, x8(99.0));
        // Alice's bid at 97 filled on the way down to 96.
        assert_eq!(fills, 1);
        assert_eq!(s.accounts[&ALICE].positions[&0].szi_x8, x8(1.0) as i64);
        // Bob paid funding each hour and fell below maintenance at 96.
        assert_eq!(liquidated, vec![BOB]);
        assert_eq!(s.accounts[&BOB].positions[&0].szi_x8, 0);
        assert_eq!(s.funding_time, t0 + 4 * 3600);
        assert_eq!(s.cursor.as_ref().unwrap().0, 16);
    }

    #[test]
    fn a_credit_is_retried_after_a_failure_and_applied_once() {
        let mut s = state();
        s.account(ALICE).spot.insert(HYPE_TOKEN, Spot(300_000_000, 0, 0));
        let send = encode_action(6, &[word_addr(HYPE_SYSTEM), word_u(HYPE_TOKEN as u128), word_u(100_000_000)].concat());
        s.apply(&[act(11, ALICE, send.clone())], (11, "0xb".into()), 0);
        assert_eq!(s.pending.len(), 1);
        let one = U256::from(10u64).pow(U256::from(18u64));
        // First attempt: the base is read and kept, then the write fails.
        let id = s.pending[0].id;
        s.credit_base(id, U256::from(5u64));
        let target = s.pending[0].target().unwrap();
        assert_eq!(target, U256::from(5u64) + one);
        // A later base read does not move the target, so the retry sets the same balance.
        s.credit_base(id, target);
        assert_eq!(s.pending[0].target(), Some(target));
        // The state survives a round trip with the credit pending.
        let mut s = State::from_bytes(&s.to_bytes()).unwrap();
        s.credit_landed(id);
        assert!(s.pending.is_empty());
        // Re-applying the same block queues nothing.
        s.apply(&[act(11, ALICE, send)], (11, "0xb".into()), 0);
        assert!(s.pending.is_empty());
        assert_eq!(s.accounts[&ALICE].spot[&HYPE_TOKEN].0, 200_000_000);
    }

    fn meta() -> Meta {
        let some = |v: &str| Some(v.to_string());
        Meta {
            universe: vec![("BTC".into(), 5, 40, false), ("ETH".into(), 4, 25, false), ("OLD".into(), 2, 3, true)],
            ctxs: vec![
                vec![some("100.0"), some("100.5"), some("0.0000125"), some("5.0"), some("99.0"), some("1000.0"), some("0.0"), some("100.0"), some("10.0")],
                vec![some("2000.0"), some("2001.0"), some("0.00001"), some("1.0"), some("1990.0"), some("10.0"), some("0.0"), some("2000.5"), some("1.0")],
                vec![some("1.0"), some("1.0"), some("0.0"), None, None, None, None, None, None],
            ],
            dexes: BTreeMap::new(),
            tiers: BTreeMap::new(),
        }
    }

    #[test]
    fn block_type_rules() {
        assert_eq!((block_gas_limit(false), block_gas_limit(true)), (3_000_000, 30_000_000));
        assert_eq!(block_refusal(3_000_000, false), None);
        assert_eq!(block_refusal(3_000_001, false).unwrap(), "exceeds block gas limit: the transaction asks 3000001 gas and a small block holds 3000000");
        assert_eq!(block_refusal(29_000_000, true), None);
        // A big block comes at least a minute after the last one, and the first is free.
        assert_eq!(big_block_time(1_000, 0), None);
        assert_eq!(big_block_time(1_030, 1_000), Some(1_060));
        assert_eq!(big_block_time(1_060, 1_000), None);
        let mut s = state();
        assert!(!s.uses_big_blocks(ALICE) && !s.uses_big_blocks(BOB));
        s.set_big_blocks(BOB, true);
        assert!(s.uses_big_blocks(BOB));
        let back = State::from_bytes(&s.to_bytes()).unwrap();
        assert!(back.uses_big_blocks(BOB));
        s.set_big_blocks(BOB, false);
        assert!(!s.uses_big_blocks(BOB));
        assert_eq!(BIG_BLOCK_GAS_PRICE, 0x5f5e100);
    }

    #[test]
    fn info_requests_answer_from_the_state() {
        let mut s = state();
        s.now = 1_793_600_000;
        s.account(BOB).usd = usd(1_000.0);
        run(&mut s, 11, BOB, limit_order(0, false, x8(100.5), x8(0.2), false, 2, 0x77));
        s.now = 1_793_600_010;
        run(&mut s, 12, ALICE, limit_order(0, true, x8(101.0), x8(0.5), false, 3, 0));
        s.apply(&[act(13, ALICE, limit_order(0, true, x8(90.0), x8(1.0), false, 2, 0))], (13, "0xd".into()), 1_793_600_010);
        let m = meta();
        let t = 1_793_600_020_000;
        let ask = |r: Value| s.info_request(&m, &r, t).unwrap();
        let alice = ALICE.to_string();
        // clearinghouseState and spotClearinghouseState are the hypercore_state parts.
        let ch = ask(json!({ "type": "clearinghouseState", "user": alice }));
        assert_eq!(ch, s.info(ALICE, t)["clearinghouseState"]);
        assert_eq!(ch["assetPositions"][0]["position"]["szi"], "0.5");
        assert_eq!(ask(json!({ "type": "spotClearinghouseState", "user": alice })), s.info(ALICE, t)["spotClearinghouseState"]);
        // openOrders: the GTC at 90, newest first.
        let oo = ask(json!({ "type": "openOrders", "user": alice }));
        assert_eq!(oo, json!([{ "coin": "BTC", "side": "B", "limitPx": "90.0", "sz": "1.0", "oid": 3, "timestamp": 1_793_600_010_000u64, "origSz": "1.0" }]));
        let bob_orders = ask(json!({ "type": "openOrders", "user": BOB.to_string() }));
        assert_eq!(bob_orders, json!([]));
        // userFills: Alice took 0.2 from Bob at 100.5 and 0.3 from the book at 101, newest first.
        let uf = ask(json!({ "type": "userFills", "user": alice }));
        assert_eq!(uf.as_array().unwrap().len(), 2);
        assert_eq!((uf[0]["px"].as_str(), uf[0]["sz"].as_str(), uf[0]["side"].as_str(), uf[0]["dir"].as_str()), (Some("101.0"), Some("0.3"), Some("B"), Some("Open Long")));
        assert_eq!((uf[1]["px"].as_str(), uf[1]["startPosition"].as_str(), uf[1]["crossed"].clone(), uf[1]["fee"].as_str()), (Some("100.5"), Some("0.0"), json!(true), Some("0.009045")));
        let bf = ask(json!({ "type": "userFills", "user": BOB.to_string() }));
        assert_eq!((bf[0]["dir"].as_str(), bf[0]["crossed"].clone(), bf[0]["oid"].clone(), bf[0]["fee"].as_str()), (Some("Open Short"), json!(false), json!(1), Some("0.003015")));
        // meta and metaAndAssetCtxs: the snapshot universe, with the copy's own BTC mark and mid.
        assert_eq!(ask(json!({ "type": "meta" }))["universe"][2], json!({ "name": "OLD", "szDecimals": 2, "maxLeverage": 3, "isDelisted": true }));
        let mac = ask(json!({ "type": "metaAndAssetCtxs" }));
        assert_eq!(mac[1].as_array().unwrap().len(), 3);
        assert_eq!((mac[1][0]["markPx"].as_str(), mac[1][0]["oraclePx"].as_str(), mac[1][0]["openInterest"].as_str()), (Some("100.0"), Some("100.0"), Some("5.0")));
        assert_eq!(mac[1][1]["markPx"], "2000.0");
        // Best bid 99 (book), best ask 101 (0.7 left there): mid 100.
        assert_eq!(mac[1][0]["midPx"], "100.0");
        // l2Book: the copy's book with Alice's bid at 90 merged in.
        let book = ask(json!({ "type": "l2Book", "coin": "BTC" }));
        assert_eq!(book["levels"][0][0], json!({ "px": "99.0", "sz": "1.0", "n": 1 }));
        assert_eq!(book["levels"][0].as_array().unwrap().last().unwrap(), &json!({ "px": "90.0", "sz": "1.0", "n": 1 }));
        assert_eq!(book["levels"][1][0], json!({ "px": "101.0", "sz": "0.7", "n": 1 }));
        assert!(s.info_request(&m, &json!({ "type": "l2Book", "coin": "ETH" }), t).unwrap_err().contains("hypercore book ETH --live"));
        // allMids: the snapshot's mids, and the copy's own for the perps it tracks.
        let mids = ask(json!({ "type": "allMids" }));
        assert_eq!((mids["BTC"].as_str(), mids["ETH"].as_str(), mids["OLD"].as_str()), (Some("100.0"), Some("2000.5"), Some("1.0")));
        // Unknown types and missing fields are refused.
        assert!(s.info_request(&m, &json!({ "type": "vaultDetails" }), t).unwrap_err().contains("does not serve"));
        assert!(s.info_request(&m, &json!({ "type": "openOrders" }), t).unwrap_err().contains("needs a user"));
    }

    #[test]
    fn a_book_refresh_that_crosses_resting_orders_fills_them() {
        let mut s = state();
        run(&mut s, 11, ALICE, limit_order(0, true, x8(97.0), x8(1.0), false, 2, 0));
        run(&mut s, 12, ALICE, limit_order(0, false, x8(130.0), x8(1.0), false, 2, 0));
        // A new book with its best ask at 96.5 crosses the bid at 97, not the ask at 130.
        let book = Book { bids: vec![(x8(96.0), x8(2.0))], asks: vec![(x8(96.5), x8(2.0))], time: 9 };
        let a = s.set_book(0, book.clone(), 50);
        assert_eq!(a.fills, 1);
        assert_eq!(s.books[&0], book);
        assert_eq!(s.accounts[&ALICE].positions[&0].szi_x8, x8(1.0) as i64);
        assert_eq!(s.orders.len(), 1);
        assert!(!s.accounts[&ALICE].fills.last().unwrap().crossed);
        // A refresh that crosses nothing fills nothing.
        assert_eq!(s.set_book(0, book, 60).fills, 0);
    }

    #[test]
    fn a_price_on_a_perp_not_in_state_adds_it_from_the_meta_snapshot() {
        let mut s = state();
        assert!(!s.perps.contains_key(&1));
        assert!(s.add_perp_from_meta(1, &meta()));
        assert_eq!((s.perps[&1].name.as_str(), s.perps[&1].sz_decimals, s.perps[&1].mark_x8), ("ETH", 4, x8(2000.0)));
        s.set_price(1, x8(1800.0), 10);
        assert_eq!((s.perps[&1].mark_x8, s.perps[&1].oracle_x8), (x8(1800.0), x8(1800.0)));
        // Raw precompile units convert once the perp is known: 180000 at szDecimals 4 is 1800.
        assert_eq!(s.px_from_raw(1, 180_000), Some(x8(1800.0)));
        assert!(!s.add_perp_from_meta(7, &meta()));
    }

    #[test]
    fn a_replay_is_capped_at_a_month_of_hourly_candles() {
        let day = 86_400;
        assert_eq!(replay_candles(0, day), 25);
        assert_eq!(replay_candles(0, 30 * day + 23 * 3600), 744);
        assert!(replay_candles(0, 31 * day) > MAX_REPLAY_CANDLES);
    }

    #[test]
    fn meta_reads_the_info_api_shape() {
        let v = json!([{ "universe": [{ "name": "BTC", "szDecimals": 5, "maxLeverage": 40 }, { "name": "X", "szDecimals": 0, "maxLeverage": 3, "isDelisted": true }] },
            [{ "markPx": "1.0", "oraclePx": "2.0", "funding": "0.1", "impactPxs": ["1", "2"] }, {}]]);
        let m = Meta::from_info(&v);
        assert_eq!(m.universe, vec![("BTC".into(), 5, 40, false), ("X".into(), 0, 3, true)]);
        assert_eq!(m.ctxs[0][..3], [Some("1.0".to_string()), Some("2.0".to_string()), Some("0.1".to_string())]);
        assert_eq!(m.ctxs[1], vec![None; 9]);
    }

    /// The xyz dex (1) with one cross perp and one isolated-only perp, both USDC.
    fn hip3() -> State {
        let mut s = state();
        let x = |v: u64| v * ONE_X8 as u64;
        s.perps.insert(10_000, Perp { name: "xyz:XYZ100".into(), sz_decimals: 4, max_leverage: 20, mark_x8: x(1000), ..Default::default() });
        s.perps.insert(10_001, Perp { name: "xyz:TSLA".into(), sz_decimals: 3, max_leverage: 10, mark_x8: x(400), isolated_only: true, ..Default::default() });
        s.books.insert(10_000, Book { bids: vec![(x(999), x(5))], asks: vec![(x(1001), x(5))], time: 1 });
        s.books.insert(10_001, Book { bids: vec![(x(399), x(5))], asks: vec![(x(401), x(5))], time: 1 });
        s
    }

    #[test]
    fn hip3_asset_ids_map_to_perp_indexes_with_their_own_dex_margin() {
        let mut s = hip3();
        assert_eq!((dex_of(10_001), dex_of(159), HIP3_ASSET_BASE + 10_000), (1, 0, 110_000));
        // No USD on dex 1 yet: the order is refused although the main dex has 10,000.
        assert!(run(&mut s, 11, ALICE, limit_order(110_000, true, x8(1001.0), x8(1.0), false, 3, 0)).starts_with("rejected: insufficient margin"));
        // sendAsset (13) moves 500 USD from the main dex (0) to dex 1.
        let send = encode_action(13, &[word_addr(ALICE), word_addr(Address::ZERO), word_u(0), word_u(1), word_u(0), word_u(50_000_000_000)].concat());
        assert_eq!(run(&mut s, 12, ALICE, send), format!("moved 500.0 USD from perp dex 0 to perp dex 1 of {ALICE}"));
        assert_eq!((s.accounts[&ALICE].usd, s.accounts[&ALICE].usd_of(1)), (usd(9_500.0), usd(500.0)));
        // Asset 110000 is perp 10000, filled against its own book.
        assert_eq!(run(&mut s, 13, ALICE, limit_order(110_000, true, x8(1001.0), x8(1.0), false, 3, 0)), "filled 1.0 at 1001.0, fee 0.45045 USD");
        assert_eq!(s.accounts[&ALICE].positions[&10_000].szi_x8, x8(1.0) as i64);
        // The fee came off dex 1, not the main dex.
        assert_eq!((s.accounts[&ALICE].usd, s.accounts[&ALICE].usd_of(1)), (usd(9_500.0), usd(500.0) - usd(0.45045)));
        assert_eq!(s.margin_dex(ALICE, 1).margin_used, usd(50.0) as u64);
        assert_eq!(s.margin_dex(ALICE, 0).margin_used, 0);
        // The answers: 0x800 for perp 10000 (it fits a uint16), 0x813, and 0x80f for dex 1.
        let answers = s.answers(&BTreeSet::from([ALICE]));
        assert!(answers.iter().any(|e| e.precompile == hypercore::POSITION && e.calldata == [word_addr(ALICE), word_u(10_000)].concat() && e.result[..32] == word_i(10_000)));
        assert!(answers.iter().any(|e| e.precompile == hypercore::POSITION2 && e.calldata == [word_addr(ALICE), word_u(10_000)].concat()));
        assert!(answers.iter().any(|e| e.precompile == hypercore::ACCOUNT_MARGIN_SUMMARY && e.calldata == [word_u(1), word_addr(ALICE)].concat()));
        // A dex with another collateral token is refused.
        s.perps.get_mut(&10_000).unwrap().collateral = 360;
        assert!(run(&mut s, 14, ALICE, limit_order(110_000, true, x8(1001.0), x8(1.0), false, 3, 0)).starts_with("rejected: the dex of xyz:XYZ100 uses token 360"));
        // Spot ids stay refused, and cancels map the asset the same way.
        assert_eq!(run(&mut s, 15, ALICE, limit_order(10_107, true, 1, 1, false, 3, 0)), "rejected: spot orders are not modeled");
        assert_eq!(s.dex_name(1), "xyz");
    }

    #[test]
    fn an_isolated_hip3_position_liquidates_alone_while_cross_positions_survive() {
        let mut s = hip3();
        s.account(ALICE).dex_usd.insert(1, usd(1_000.0));
        // Cross BTC on the main dex, isolated TSLA (max 10x) at 10x on dex 1: 400 notional holds 40 of margin.
        run(&mut s, 11, ALICE, limit_order(0, true, x8(101.0), x8(1.0), false, 3, 0));
        assert_eq!(run(&mut s, 12, ALICE, limit_order(110_001, true, x8(401.0), x8(1.0), false, 3, 0)), "filled 1.0 at 401.0, fee 0.18045 USD");
        let p = s.accounts[&ALICE].positions[&10_001];
        assert!(p.isolated);
        assert_eq!((p.leverage, p.isolated_raw_usd), (10, usd(40.1)));
        assert_eq!(s.accounts[&ALICE].usd_of(1), usd(1_000.0) - usd(40.1) - usd(0.18045));
        // Isolated margin stays out of the dex's cross figures.
        assert_eq!(s.margin_dex(ALICE, 1).margin_used, 0);
        // TSLA falls to 365: equity 40.1 - 36 = 4.1, maintenance 365 / 20 = 18.25. Liquidated.
        let a = s.set_price(10_001, x8(365.0), 100);
        assert_eq!(a.liquidated, vec![ALICE]);
        assert_eq!(s.accounts[&ALICE].positions[&10_001].szi_x8, 0);
        assert!(s.accounts[&ALICE].history.last().unwrap().1.starts_with("liquidated: isolated xyz:TSLA equity 4.1 USD below maintenance margin 18.25 USD"));
        // The cross BTC position and the dex balance outside the isolated margin survive.
        assert_eq!(s.accounts[&ALICE].positions[&0].szi_x8, x8(1.0) as i64);
        assert_eq!(s.accounts[&ALICE].usd_of(1), usd(1_000.0) - usd(40.1) - usd(0.18045) + usd(4.1));
        let ch = s.clearinghouse(ALICE, 0, 0);
        assert_eq!(ch["assetPositions"][0]["position"]["coin"], "BTC");
    }

    #[test]
    fn an_isolated_loss_past_its_margin_stays_off_the_dex_balance() {
        let mut s = hip3();
        s.account(ALICE).dex_usd.insert(1, usd(100.0));
        run(&mut s, 11, ALICE, limit_order(110_001, true, x8(401.0), x8(1.0), false, 3, 0));
        let before = s.accounts[&ALICE].usd_of(1);
        // A gap to 300 loses 101 on 40.1 of margin: the dex keeps what was outside it.
        s.set_price(10_001, x8(300.0), 100);
        assert_eq!(s.accounts[&ALICE].usd_of(1), before);
        assert!(s.accounts[&ALICE].history.last().unwrap().1.ends_with("shortfall 60.9 USD absorbed"));
    }

    /// USDC (borrowable) and HYPE (LTV 0.65) reserves, HYPE at 50.
    fn lend() -> State {
        let mut s = state();
        s.reserves.insert(0, crate::lend::Reserve { ltv_bps: 0, oracle_x8: ONE_X8 as u64, supplied: 1_000_000 * 100_000_000, borrowed: 500_000 * 100_000_000, rate_e12: 50_000_000_000, ..Default::default() });
        s.reserves.insert(HYPE_TOKEN, crate::lend::Reserve { ltv_bps: 6_500, oracle_x8: 50 * ONE_X8 as u64, supplied: 0, borrowed: 0, rate_e12: 50_000_000_000, ..Default::default() });
        s.account(BOB).spot.insert(HYPE_TOKEN, Spot(10 * 100_000_000, 0, 0));
        s
    }

    fn borrow_lend(op: u8, token: u64, wei: u64) -> Vec<u8> {
        encode_action(15, &[word_u(op as u128), word_u(token as u128), word_u(wei as u128)].concat())
    }

    #[test]
    fn supply_borrow_interest_and_repay() {
        let mut s = lend();
        // Supply all 10 HYPE (wei 0 means all): 500 USD of collateral, 325 at LTV 0.65.
        assert_eq!(run(&mut s, 11, BOB, borrow_lend(0, HYPE_TOKEN, 0)), "supplied 1000000000 wei of token 150");
        assert_eq!(s.reserves[&HYPE_TOKEN].supplied, 1_000_000_000);
        // A plain account cannot borrow: the spot send needs a balance.
        let send = |wei: u64| encode_action(6, &[word_addr(ALICE), word_u(0), word_u(wei as u128)].concat());
        assert_eq!(run(&mut s, 12, BOB, send(100 * 100_000_000)), "rejected: insufficient spot balance of token 0");
        // Portfolio margin (action 16, abstraction 3) borrows the 100 USDC it lacks.
        assert_eq!(run(&mut s, 13, BOB, encode_action(16, &[word_addr(BOB), word_u(3)].concat())), "abstraction set to portfolioMargin");
        assert_eq!(run(&mut s, 14, BOB, send(100 * 100_000_000)), format!("sent 10000000000 wei of token 0 to {ALICE}"));
        let l = s.accounts[&BOB].lend[&0];
        assert_eq!((l.borrow_basis, l.borrow_value), (10_000_000_000, 10_000_000_000));
        assert_eq!(s.reserves[&0].borrowed, 500_100 * 100_000_000);
        assert_eq!(s.accounts[&ALICE].spot[&0].0, 10_000_000_000);
        // Health factor 325 / 100.
        assert_eq!(s.health_factor_e12(BOB), Some(3_250_000_000_000));
        // Two hours of interest at utilization 0.5001: 0.05 a year on the borrow.
        let t0 = 1_793_599_200;
        s.apply(&[], (15, "0xf".into()), t0);
        let a = s.apply(&[], (16, "0x10".into()), t0 + 2 * 3600);
        assert!(a.reserves && a.users.contains(&BOB));
        let hourly = 10_000_000_000u64 * 5 / 100 / 8760;
        let after = s.accounts[&BOB].lend[&0].borrow_value;
        assert!(after >= 10_000_000_000 + 2 * hourly && after <= 10_000_000_000 + 2 * hourly + 2, "{after}");
        assert_eq!(s.accounts[&BOB].lend[&0].borrow_basis, 10_000_000_000);
        // A credit of USDC repays first: 50 USDC sent back goes to the borrow.
        run(&mut s, 17, ALICE, encode_action(6, &[word_addr(BOB), word_u(0), word_u(50 * 100_000_000)].concat()));
        assert_eq!(s.accounts[&BOB].lend[&0].borrow_value, after - 50 * 100_000_000);
        assert_eq!(s.accounts[&BOB].spot[&0].0, 0);
        // The precompile answers: 0x811 for (BOB, 0) and 0x812 per reserve.
        let ans = s.lend_user_answers(BOB);
        let usdc = ans.iter().find(|e| e.calldata == [word_addr(BOB), word_u(0)].concat()).unwrap();
        assert_eq!(usdc.result[32..64], word_u((after - 50 * 100_000_000) as u128));
        let reserve = s.reserve_answers().into_iter().find(|e| e.calldata == word_u(0).to_vec()).unwrap();
        // 500 bps borrow, oracle 1 in USDC's raw units (szDecimals 8), LTV 0.
        assert_eq!((reserve.result[..32].to_vec(), reserve.result[128..160].to_vec(), reserve.result[160..192].to_vec()), (word_u(500).to_vec(), word_u(1).to_vec(), word_u(0).to_vec()));
        let info = s.lend_info(&json!({ "type": "borrowLendUserState", "user": BOB.to_string() })).unwrap().unwrap();
        assert_eq!((info["health"].as_str(), info["tokenToState"][0][0].clone()), (Some("healthy"), json!(0)));
        assert_eq!(s.lend_info(&json!({ "type": "borrowLendReserveState", "token": 150 })).unwrap().unwrap()["ltv"], "0.65");
        // Withdrawing the collateral that backs the borrow is refused, a part is fine.
        assert!(run(&mut s, 18, BOB, borrow_lend(1, HYPE_TOKEN, 0)).starts_with("rejected: withdrawing"));
        assert_eq!(run(&mut s, 19, BOB, borrow_lend(1, HYPE_TOKEN, 100_000_000)), "withdrew 100000000 wei of token 150");
        assert_eq!(s.accounts[&BOB].spot[&HYPE_TOKEN].0, 100_000_000);
    }

    #[test]
    fn borrowing_stops_at_health_one_and_liquidation_takes_collateral() {
        let mut s = lend();
        run(&mut s, 11, BOB, borrow_lend(0, HYPE_TOKEN, 0));
        run(&mut s, 12, BOB, encode_action(16, &[word_addr(BOB), word_u(2)].concat()));
        let send = |wei: u64| encode_action(6, &[word_addr(ALICE), word_u(0), word_u(wei as u128)].concat());
        // 325 USD of LTV-weighted collateral: 325.000001 (the smallest USD step) is refused, 325 is fine.
        assert!(run(&mut s, 13, BOB, send(32_500_000_100)).contains("below 1"));
        assert!(run(&mut s, 14, BOB, send(32_500_000_000)).starts_with("sent"));
        assert_eq!(s.health_factor_e12(BOB), Some(1_000_000_000_000));
        // HYPE to 40: maintenance 20 + 325 = 345 against 400 x 0.825 = 330 of liquidation value.
        s.reserves.get_mut(&HYPE_TOKEN).unwrap().oracle_x8 = 40 * ONE_X8 as u64;
        let mut a = Applied::default();
        s.lend_liquidate(&mut a);
        assert_eq!(a.liquidated, vec![BOB]);
        // Each step takes 2 HYPE (80 USD) to the backstop: after two steps 165 is borrowed,
        // and 185 / (240 x 0.825 = 198) = 0.934 is healthy again.
        let l = s.accounts[&BOB].lend.clone();
        assert_eq!((l[&0].borrow_value, l[&HYPE_TOKEN].supply_value), (16_500_000_000, 600_000_000));
        assert_eq!(s.accounts[&crate::lend::BACKSTOP].spot[&HYPE_TOKEN].0, 400_000_000);
        assert!(s.accounts[&BOB].history.last().unwrap().1.contains("in 2 steps of 20%"));
        assert!(!s.lend_liquidatable(BOB));
    }

    /// BOB supplies 10 HYPE and borrows 325 USDC as portfolio margin, with
    /// HYPE's reserve price following spot pair 107 and perp 159.
    fn lend_on_prices(perp: bool) -> State {
        let mut s = lend();
        s.perps.insert(159, Perp { name: "HYPE".into(), sz_decimals: 2, max_leverage: 10, mark_x8: 50 * ONE_X8 as u64, oracle_x8: 50 * ONE_X8 as u64, ..Default::default() });
        let r = s.reserves.get_mut(&HYPE_TOKEN).unwrap();
        (r.spot, r.spot_x8, r.perp) = (Some(107), 50 * ONE_X8 as u64, perp.then_some(159));
        run(&mut s, 11, BOB, borrow_lend(0, HYPE_TOKEN, 0));
        run(&mut s, 12, BOB, encode_action(16, &[word_addr(BOB), word_u(3)].concat()));
        run(&mut s, 13, BOB, encode_action(6, &[word_addr(ALICE), word_u(0), word_u(32_500_000_000)].concat()));
        s
    }

    #[test]
    fn lend_health_at_the_threshold_after_a_price_move() {
        // Maintenance 345 against 10 x px x 0.825: the ratio crosses 0.95 between 44.02 and 44.01.
        let px = |v: u64| v * ONE_X8 as u64 / 100;
        // The perp moves: the median of spot 50, mark and oracle is the perp's price.
        let mut s = lend_on_prices(true);
        let a = s.set_price(159, px(4402), 0);
        assert!(a.liquidated.is_empty() && a.reserves);
        assert_eq!(s.reserves[&HYPE_TOKEN].oracle_x8, px(4402));
        let a = s.set_price(159, px(4401), 0);
        assert_eq!(a.liquidated, vec![BOB]);
        assert!(s.accounts[&BOB].lend[&HYPE_TOKEN].supply_value < 1_000_000_000);
        // The spot pair moves a reserve with no perp the same way.
        let mut s = lend_on_prices(false);
        assert!(s.set_spot_price(107, px(4402), 0).liquidated.is_empty());
        assert_eq!(s.set_spot_price(107, px(4401), 0).liquidated, vec![BOB]);
        // The median of spot 60 and the perp's 50 and 50 is 50.
        let mut s = lend_on_prices(true);
        s.set_spot_price(107, px(6000), 0);
        assert_eq!(s.reserves[&HYPE_TOKEN].oracle_x8, px(5000));
    }

    #[test]
    fn linking_reserves_finds_the_spot_pair_and_the_perp() {
        let mut s = lend();
        s.tokens.insert(197, TokenMeta { name: "UBTC".into(), wei_decimals: 10, evm: None, sz_decimals: 5 });
        s.reserves.insert(197, crate::lend::Reserve { ltv_bps: 5_000, oracle_x8: 77_000 * ONE_X8 as u64, ..Default::default() });
        let spot = json!({ "universe": [{ "tokens": [150, 0], "index": 107 }, { "tokens": [197, 0], "index": 142 }, { "tokens": [197, 268], "index": 300 }] });
        let meta = json!({ "universe": [{ "name": "BTC" }, { "name": "ETH" }, { "name": "HYPE" }] });
        assert_eq!(s.link_reserves(&spot, &meta), vec![2, 0]);
        let (h, b) = (&s.reserves[&HYPE_TOKEN], &s.reserves[&197]);
        assert_eq!((h.spot, h.perp, b.spot, b.perp), (Some(107), Some(2), Some(142), Some(0)));
        assert_eq!(b.spot_x8, 77_000 * ONE_X8 as u64);
        // USDC is not collateral and keeps no links.
        assert_eq!((s.reserves[&0].spot, s.reserves[&0].perp), (None, None));
    }

    #[test]
    fn the_stablecoin_rate_follows_utilization() {
        let r = |s: u64, b: u64| crate::lend::Reserve { supplied: s, borrowed: b, ..Default::default() };
        assert_eq!(r(100, 50).borrow_rate_e12(), 50_000_000_000);
        // 0.05 + 4.75 x (0.9 - 0.8) = 0.525
        assert_eq!(r(100, 90).borrow_rate_e12(), 525_000_000_000);
        // Measured on mainnet: utilization 0.7937664526 gives supply 0.0357194904.
        let u = r(1_000_000_000_000, 793_766_452_600);
        assert_eq!(u.supply_rate_e12() / 1_000_000, 35_719);
    }

    #[test]
    fn testnet_hype_index_moves_the_system_address_and_bridge() {
        let mut s = state();
        s.hype_token = 1105;
        s.tokens.insert(1105, TokenMeta { name: "HYPE".into(), wei_decimals: 8, evm: None, sz_decimals: 2 });
        assert_eq!(s.hype(), 1105);
        assert_eq!((system_address_for(1105, 1105), system_token_for(HYPE_SYSTEM, 1105)), (HYPE_SYSTEM, Some(1105)));
        // On testnet, token 150 is an ordinary token with an 0x20 system address.
        assert_eq!(system_address_for(150, 1105).to_string().to_lowercase(), "0x2000000000000000000000000000000000000096");
        let e = Logged { block: 11, log_index: 0, event: Event::HypeIn { user: BOB, amount: U256::from(10u64).pow(U256::from(18u64)) } };
        s.apply(&[e], (11, "0xb".into()), 0);
        assert_eq!(s.accounts[&BOB].spot[&1105].0, 100_000_000);
        assert!(!s.accounts[&BOB].spot.contains_key(&HYPE_TOKEN));
        let a = s.apply(&[act(12, BOB, encode_action(6, &[word_addr(HYPE_SYSTEM), word_u(1105), word_u(100_000_000)].concat()))], (12, "0xc".into()), 0);
        assert_eq!(a.credits[0].amount, U256::from(10u64).pow(U256::from(18u64)));
        // Mainnet keeps 150.
        assert_eq!(State::new().hype(), HYPE_TOKEN);
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

    #[test]
    fn margin_tiers_pick_by_notional_and_keep_maintenance_continuous() {
        let tables = json!([[50, { "marginTiers": [{ "lowerBound": "0.0", "maxLeverage": 50 }] }], [51, { "description": "tiered 10x", "marginTiers": [{ "lowerBound": "0.0", "maxLeverage": 10 }, { "lowerBound": "3000000.0", "maxLeverage": 5 }] }]]);
        // A main-dex table: 10x up to 3M notional, 5x from there.
        let tiers = tiers_from_meta(&json!({ "name": "HYPE", "maxLeverage": 10, "marginTableId": 51 }), &tables);
        assert_eq!(tiers, vec![(0, 10), (3_000_000_000_000, 5)]);
        let hype = Perp { name: "HYPE".into(), max_leverage: 10, tiers, ..Default::default() };
        let m = |usd: u64| usd * 1_000_000;
        assert_eq!((hype.max_leverage_at(m(2_999_999)), hype.max_leverage_at(m(3_000_000))), (10, 5));
        // Just below the bound: notional / 20. At and above it: notional / 10 less a 150,000 deduction.
        assert_eq!(hype.maintenance_at(m(2_000_000)), m(100_000));
        assert_eq!(hype.maintenance_at(m(3_000_000)), m(150_000));
        assert_eq!(hype.maintenance_at(m(4_000_000)), m(400_000 - 150_000));
        // A HIP-3 id below 50 (and a one-tier table) is one tier at the perp's max leverage.
        assert!(tiers_from_meta(&json!({ "name": "xyz:TSLA", "maxLeverage": 20, "marginTableId": 20 }), &tables).is_empty());
        assert!(tiers_from_meta(&json!({ "name": "xyz:SP", "maxLeverage": 50, "marginTableId": 50 }), &tables).is_empty());
        let tsla = Perp { name: "xyz:TSLA".into(), max_leverage: 20, ..Default::default() };
        assert_eq!((tsla.max_leverage_at(m(50_000_000)), tsla.maintenance_at(m(40_000))), (20, m(1_000)));
        // The meta snapshot keeps only the tiered entries.
        let meta = Meta::from_info(&json!([{ "universe": [{ "name": "A", "maxLeverage": 3, "marginTableId": 3 }, { "name": "HYPE", "maxLeverage": 10, "marginTableId": 51 }], "marginTables": tables }, []]));
        assert_eq!(meta.tiers.keys().copied().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn a_big_position_liquidates_on_its_tier_maintenance() {
        // 4M of HYPE on table 51 at 10x: maintenance is 250,000, not the one-tier 200,000.
        let mut s = State::new();
        s.perps.insert(159, Perp { name: "HYPE".into(), sz_decimals: 2, max_leverage: 10, mark_x8: 40 * ONE_X8 as u64, tiers: vec![(0, 10), (3_000_000_000_000, 5)], ..Default::default() });
        let sz = 100_000 * ONE_X8 as i64;
        let entry = 4_000_000_000_000u64;
        s.accounts.entry(ALICE).or_default().positions.insert(159, Pos { szi_x8: sz, entry_ntl: entry, leverage: 5, ..Default::default() });
        assert_eq!(s.maintenance(ALICE), 250_000_000_000);
        // Account value 240,000: below the tier's maintenance, above the one-tier figure.
        s.accounts.get_mut(&ALICE).unwrap().usd = 240_000_000_000;
        let applied = s.set_price(159, 40 * ONE_X8 as u64, 0);
        assert_eq!(applied.liquidated, vec![ALICE]);
    }

    #[test]
    fn hip3_funding_follows_the_premium_formula() {
        let xyz = json!({ "name": "xyz", "assetToFundingMultiplier": [["xyz:TSLA", "0.5"], ["xyz:XYZ100", "0.5"]], "assetToFundingInterestRate": [["xyz:EUR", "0.0"]], "assetToFundingClamp": [] });
        let tsla = Hip3Funding::from_dex(&xyz, "xyz:TSLA");
        assert_eq!(tsla, Hip3Funding { multiplier_e12: 500_000_000_000, rate_e12: FUNDING_RATE_E12, clamp_e12: FUNDING_CLAMP_E12 });
        assert_eq!(Hip3Funding::from_dex(&xyz, "xyz:EUR").rate_e12, 0);
        assert_eq!(Hip3Funding::from_dex(&xyz, "xyz:NEW").multiplier_e12, 1_000_000_000_000);
        let x8 = |v: f64| (v * 1e8).round() as u64;
        let book = |bid: f64, ask: f64| Book { bids: vec![(x8(bid), x8(1000.0))], asks: vec![(x8(ask), x8(1000.0))], time: 0 };
        // Measured on mainnet, xyz:TSLA: impact prices 402.515 and 402.613, oracle 402.64, premium
        // -0.0001887542 and funding 0.00000625 (the interest rate inside the clamp, times 0.5, over 8).
        let p = hip3_premium_e12(&book(402.515, 402.613), x8(402.64), impact_notional("xyz:TSLA")).unwrap();
        assert_eq!(p / 1000, -188_754);
        assert_eq!(tsla.hourly_e12(p), 6_250_000);
        // xyz:XYZ100: impact 28570.03 and 28576.0, oracle 28548, premium 0.000876: rate - premium clamps at -0.0005,
        // so 0.5 x 0.000376 / 8.
        let p = hip3_premium_e12(&book(28570.03, 28576.0), x8(28548.0), 6_000_000_000).unwrap();
        assert_eq!(p / 1000, 876_243);
        assert_eq!(tsla.hourly_e12(p) / 1000, 23_515);
        // The cap: 4% an hour.
        let wild = Hip3Funding { multiplier_e12: 1_000_000_000_000_000, ..tsla };
        assert_eq!(wild.hourly_e12(1_000_000_000_000), FUNDING_CAP_E12);
        // Impact prices walk the book for the impact notional.
        let deep = vec![(x8(100.0), x8(30.0)), (x8(99.0), x8(100.0))];
        // 3,000 at 100 and 3,000 at 99: 6,000 for 60.30303 units, an average of 99.497...
        assert_eq!(impact_px(&deep, 6_000_000_000).unwrap() / 1000, 9_949_748);
    }

    #[test]
    fn hip3_funding_is_paid_from_the_book_at_the_hour() {
        let mut s = hip3();
        let book = Book { bids: vec![(100_100_000_000, 10_000_000_000)], asks: vec![(100_300_000_000, 10_000_000_000)], time: 0 };
        s.books.insert(10_000, book);
        let p = s.perps.get_mut(&10_000).unwrap();
        (p.mark_x8, p.oracle_x8, p.hip3_funding) = (100_000_000_000, 100_000_000_000, Some(Hip3Funding { multiplier_e12: 500_000_000_000, rate_e12: FUNDING_RATE_E12, clamp_e12: FUNDING_CLAMP_E12 }));
        s.accounts.entry(ALICE).or_default().positions.insert(10_000, Pos { szi_x8: 10 * ONE_X8 as i64, entry_ntl: 10_000_000_000, leverage: 10, ..Default::default() });
        s.accounts.get_mut(&ALICE).unwrap().dex_usd.insert(1, 5_000_000_000);
        let t0 = 1_793_599_200;
        s.apply(&[], (20, "0x14".into()), t0);
        s.apply(&[], (21, "0x15".into()), t0 + 3600);
        // Premium 0.002 (impact mid 1002 over 1000), clamp -0.0005: 0.5 x 0.0015 / 8 = 0.00009375.
        assert_eq!(s.perps[&10_000].funding_e12, 93_750_000);
        assert_eq!(s.accounts[&ALICE].positions[&10_000].funding_paid, 937_500);
    }

    #[test]
    fn hip3_premium_holds_across_a_price_move() {
        let mut s = hip3();
        s.books.insert(10_000, Book { bids: vec![(100_100_000_000, 10_000_000_000)], asks: vec![(100_300_000_000, 10_000_000_000)], time: 0 });
        let p = s.perps.get_mut(&10_000).unwrap();
        (p.mark_x8, p.oracle_x8, p.hip3_funding) = (100_000_000_000, 100_000_000_000, Some(Hip3Funding { multiplier_e12: 500_000_000_000, rate_e12: FUNDING_RATE_E12, clamp_e12: FUNDING_CLAMP_E12 }));
        let t0 = 1_793_599_200;
        s.apply(&[], (20, "0x14".into()), t0);
        // 3% down: the book follows, so the impact mid stays 0.2% over the oracle.
        s.set_price(10_000, 97_000_000_000, t0 + 10);
        assert_eq!(s.books[&10_000].bids[0].0, 97_097_000_000);
        s.apply(&[], (21, "0x15".into()), t0 + 3600);
        assert_eq!(s.perps[&10_000].funding_e12, 93_750_000);
    }

    #[test]
    fn isolated_margin_is_added_and_removed_within_the_bound() {
        let mut s = hip3();
        let x = |v: u64| v * ONE_X8 as u64;
        // An isolated xyz:TSLA long: 10 at 400 with 10x leverage holds 400 USD of margin.
        s.accounts.entry(ALICE).or_default().dex_usd.insert(1, 1_000_000_000);
        s.accounts.get_mut(&ALICE).unwrap().positions.insert(10_001, Pos { szi_x8: x(10) as i64, entry_ntl: 4_000_000_000, leverage: 10, isolated: true, isolated_raw_usd: 400_000_000, ..Default::default() });
        assert_eq!(s.update_isolated_margin(ALICE, 10_001, 100_000_000).unwrap(), "added 100.0 USD of isolated margin on xyz:TSLA, margin now 500.0 USD");
        assert_eq!((s.accounts[&ALICE].positions[&10_001].isolated_raw_usd, s.accounts[&ALICE].usd_of(1)), (500_000_000, 900_000_000));
        // More than the dex has withdrawable is refused.
        assert!(s.update_isolated_margin(ALICE, 10_001, 950_000_000).unwrap_err().contains("withdrawable"));
        // Removing must leave max(initial margin 400, 10% of 4,000 = 400): 100 can go, 100.000001 more cannot.
        assert!(s.update_isolated_margin(ALICE, 10_001, -100_000_001).unwrap_err().contains("below the 400.0 USD"));
        assert!(s.update_isolated_margin(ALICE, 10_001, -100_000_000).unwrap().starts_with("removed 100.0 USD"));
        assert_eq!(s.accounts[&ALICE].usd_of(1), 1_000_000_000);
        // A cross position and no position are refused.
        s.accounts.get_mut(&ALICE).unwrap().positions.insert(10_000, Pos { szi_x8: x(1) as i64, entry_ntl: 1_000_000_000, leverage: 10, ..Default::default() });
        assert!(s.update_isolated_margin(ALICE, 10_000, 1_000_000).unwrap_err().contains("cross margin"));
        assert!(s.update_isolated_margin(BOB, 10_001, 1_000_000).unwrap_err().contains("no open"));
    }
}
