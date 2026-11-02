//! Borrow and lend on the local HyperCore, following Hyperliquid's
//! documentation (trading/portfolio-margin and support/faq/manual-borrows):
//!
//! - CoreWriter action 15 supplies (0) and withdraws (1) a token, wei 0
//!   meaning all of it. Action 16 sets the account abstraction.
//! - Borrowing is implicit: for a portfolio margin or unified account, a spot
//!   debit beyond the balance borrows a borrowable token (a stablecoin, LTV 0)
//!   against supplied collateral while the health factor stays at or above 1,
//!   and a later credit of that token repays first.
//! - Stablecoin borrow APY is 0.05 + 4.75 * max(0, utilization - 0.8),
//!   indexed hourly. Suppliers earn the borrow rate times utilization, less
//!   the 10% the protocol keeps.
//! - Health factor = LTV-weighted collateral / borrowed. The account is
//!   liquidated past a ratio of 0.95 between borrowed value and collateral
//!   weighted by 0.5 + 0.5 * LTV, and the backstop takes collateral at the
//!   oracle price to clear the borrows.
//!
//! Amounts are in each token's wei, prices x8, rates as yearly fractions
//! with 12 decimals.

use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::core::{fmt_dec, parse_dec, Applied, State};
use crate::hypercore::{self, word_addr, word_u, Entry};

const E12: i128 = 1_000_000_000_000;
const HOURS_PER_YEAR: i128 = 8760;

/// A token's borrow and lend reserve.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reserve {
    /// Loan-to-value, basis points. Zero for the borrowable stablecoins.
    #[serde(rename = "l")]
    pub ltv_bps: u64,
    /// Oracle price, x8.
    #[serde(rename = "p")]
    pub oracle_x8: u64,
    #[serde(rename = "s")]
    pub supplied: u64,
    #[serde(rename = "b")]
    pub borrowed: u64,
    /// The yearly borrow rate from the info API at `up`, used for tokens that
    /// are not stablecoins (12 decimals).
    #[serde(rename = "r")]
    pub rate_e12: i64,
}

/// One user's borrow and lend position in a token: basis and value, wei.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lend {
    #[serde(rename = "bb")]
    pub borrow_basis: u64,
    #[serde(rename = "bv")]
    pub borrow_value: u64,
    #[serde(rename = "sb")]
    pub supply_basis: u64,
    #[serde(rename = "sv")]
    pub supply_value: u64,
}

impl Reserve {
    pub fn borrowable(&self) -> bool {
        self.ltv_bps == 0
    }

    pub fn utilization_e12(&self) -> i128 {
        if self.supplied == 0 {
            0
        } else {
            (self.borrowed as i128 * E12 / self.supplied as i128).min(E12)
        }
    }

    /// The yearly borrow rate: the stablecoin formula, or the rate from `up`.
    pub fn borrow_rate_e12(&self) -> i128 {
        if self.borrowable() {
            let over = (self.utilization_e12() - 800_000_000_000).max(0);
            50_000_000_000 + 475 * over / 100
        } else {
            self.rate_e12 as i128
        }
    }

    /// The yearly supply rate: borrow rate x utilization x 0.9.
    pub fn supply_rate_e12(&self) -> i128 {
        self.borrow_rate_e12() * self.utilization_e12() / E12 * 9 / 10
    }
}

/// Scales a basis by how a value moved, so basis tracks the principal share.
fn rescale(basis: u64, before: u64, after: u64) -> u64 {
    if before == 0 {
        after
    } else {
        (basis as u128 * after as u128 / before as u128) as u64
    }
}

impl State {
    fn wei_decimals(&self, token: u64) -> u32 {
        self.tokens.get(&token).map_or(8, |t| t.wei_decimals as u32)
    }

    /// A token amount's value in USD, x6, at the reserve's oracle price.
    pub fn token_usd(&self, token: u64, wei: u64) -> i128 {
        let px = self.reserves.get(&token).map_or(0, |r| r.oracle_x8);
        wei as i128 * px as i128 / 10i128.pow(self.wei_decimals(token) + 2)
    }

    /// (LTV-weighted collateral, borrowed, liquidation value), USD x6.
    pub fn lend_health(&self, user: Address) -> (i128, i128, i128) {
        let Some(acct) = self.accounts.get(&user) else { return (0, 0, 0) };
        let (mut coll, mut borrowed, mut liq) = (0i128, 0i128, 0i128);
        for (token, l) in &acct.lend {
            let ltv = self.reserves.get(token).map_or(0, |r| r.ltv_bps) as i128;
            let supply = self.token_usd(*token, l.supply_value);
            coll += supply * ltv / 10_000;
            if ltv > 0 {
                liq += supply * (5_000 + ltv / 2) / 10_000;
            }
            borrowed += self.token_usd(*token, l.borrow_value);
        }
        (coll, borrowed, liq)
    }

    /// The health factor, 12 decimals, or None with nothing borrowed.
    pub fn health_factor_e12(&self, user: Address) -> Option<i128> {
        let (coll, borrowed, _) = self.lend_health(user);
        (borrowed > 0).then(|| coll * E12 / borrowed)
    }

    fn spot_available(&self, user: Address, token: u64) -> u64 {
        self.accounts.get(&user).and_then(|a| a.spot.get(&token)).map_or(0, |s| s.0.saturating_sub(s.1))
    }

    /// Takes `wei` of a token from a user's spot balance. A portfolio margin
    /// or unified account borrows a borrowable token for what it lacks, while
    /// its health factor stays at or above 1.
    pub fn debit_spot(&mut self, user: Address, token: u64, wei: u64) -> Result<(), String> {
        let have = self.spot_available(user, token);
        if wei <= have {
            self.accounts.entry(user).or_default().spot.entry(token).or_default().0 -= wei;
            return Ok(());
        }
        let insufficient = format!("rejected: insufficient spot balance of token {token}");
        let abstraction = self.accounts.get(&user).map_or(0, |a| a.abstraction);
        if !matches!(abstraction, 2 | 3) || !self.reserves.get(&token).is_some_and(Reserve::borrowable) {
            return Err(insufficient);
        }
        let short = wei - have;
        let (coll, borrowed, _) = self.lend_health(user);
        if coll < borrowed + self.token_usd(token, short) {
            return Err(format!("rejected: borrowing {short} wei of token {token} would take the health factor below 1"));
        }
        let acct = self.accounts.entry(user).or_default();
        acct.spot.entry(token).or_default().0 -= have;
        let l = acct.lend.entry(token).or_default();
        l.borrow_basis += short;
        l.borrow_value += short;
        self.reserves.get_mut(&token).expect("reserve").borrowed += short;
        self.reserves_dirty = true;
        let text = format!("borrowed {short} wei of token {token}");
        self.lend_note(user, text);
        Ok(())
    }

    /// Credits `wei` of a token to a user's spot balance, repaying a borrow of
    /// that token first.
    pub fn credit_spot(&mut self, user: Address, token: u64, wei: u64) {
        let acct = self.accounts.entry(user).or_default();
        let mut left = wei;
        if let Some(l) = acct.lend.get_mut(&token).filter(|l| l.borrow_value > 0) {
            let repay = left.min(l.borrow_value);
            let before = l.borrow_value;
            l.borrow_value -= repay;
            l.borrow_basis = rescale(l.borrow_basis, before, l.borrow_value);
            left -= repay;
            if let Some(r) = self.reserves.get_mut(&token) {
                r.borrowed = r.borrowed.saturating_sub(repay);
            }
            self.reserves_dirty = true;
            self.lend_note(user, format!("repaid {repay} wei of token {token}"));
        }
        if left > 0 {
            self.accounts.entry(user).or_default().spot.entry(token).or_default().0 += left;
        }
    }

    fn lend_note(&mut self, user: Address, text: String) {
        let now = self.now_secs();
        let h = &mut self.accounts.entry(user).or_default().history;
        h.push((now, text));
        let drop = h.len().saturating_sub(16);
        h.drain(..drop);
    }

    /// Action 15: supply (0) or withdraw (1) a token, wei 0 meaning all.
    pub fn borrow_lend(&mut self, user: Address, op: u8, token: u64, wei: u64) -> String {
        if !self.reserves.contains_key(&token) {
            return format!("rejected: token {token} has no borrow and lend reserve");
        }
        match op {
            0 => {
                let amount = if wei == 0 { self.spot_available(user, token) } else { wei };
                if amount == 0 || amount > self.spot_available(user, token) {
                    return format!("rejected: insufficient spot balance of token {token} to supply");
                }
                let acct = self.accounts.entry(user).or_default();
                acct.spot.entry(token).or_default().0 -= amount;
                let l = acct.lend.entry(token).or_default();
                l.supply_basis += amount;
                l.supply_value += amount;
                self.reserves.get_mut(&token).expect("reserve").supplied += amount;
                self.reserves_dirty = true;
                format!("supplied {amount} wei of token {token}")
            }
            1 => {
                let have = self.accounts.get(&user).and_then(|a| a.lend.get(&token)).map_or(0, |l| l.supply_value);
                let amount = if wei == 0 { have } else { wei };
                if amount == 0 || amount > have {
                    return format!("rejected: {amount} wei of token {token} is more than the {have} supplied");
                }
                let ltv = self.reserves[&token].ltv_bps as i128;
                let (coll, borrowed, _) = self.lend_health(user);
                if borrowed > 0 && coll - self.token_usd(token, amount) * ltv / 10_000 < borrowed {
                    return "rejected: withdrawing would take the health factor below 1".into();
                }
                let l = self.accounts.entry(user).or_default().lend.entry(token).or_default();
                let before = l.supply_value;
                l.supply_value -= amount;
                l.supply_basis = rescale(l.supply_basis, before, l.supply_value);
                let r = self.reserves.get_mut(&token).expect("reserve");
                r.supplied = r.supplied.saturating_sub(amount);
                self.reserves_dirty = true;
                self.credit_spot(user, token, amount);
                format!("withdrew {amount} wei of token {token}")
            }
            other => format!("rejected: borrow and lend operation {other} is not one of 0 (supply) and 1 (withdraw)"),
        }
    }

    /// Action 16: the account abstraction (1 disabled, 2 unified account, 3 portfolio margin).
    pub fn set_abstraction(&mut self, user: Address, target: Address, abstraction: u8) -> String {
        if target != user && target != Address::ZERO {
            return "rejected: an account sets only its own abstraction".into();
        }
        let name = match abstraction {
            1 => "disabled",
            2 => "unifiedAccount",
            3 => "portfolioMargin",
            other => return format!("rejected: unknown abstraction {other}"),
        };
        self.accounts.entry(user).or_default().abstraction = abstraction;
        format!("abstraction set to {name}")
    }

    /// One hour of interest on every borrow and supply. Returns whether any changed.
    pub fn accrue_interest(&mut self, applied: &mut Applied) -> bool {
        let rates: Vec<(u64, i128, i128)> = self.reserves.iter().map(|(t, r)| (*t, r.borrow_rate_e12(), r.supply_rate_e12())).collect();
        let mut changed = false;
        for (token, borrow, supply) in rates {
            let (mut borrowed, mut supplied) = (0u64, 0u64);
            for (user, acct) in self.accounts.iter_mut() {
                let Some(l) = acct.lend.get_mut(&token) else { continue };
                let b = (l.borrow_value as i128 * borrow / E12 / HOURS_PER_YEAR) as u64;
                let s = (l.supply_value as i128 * supply / E12 / HOURS_PER_YEAR) as u64;
                if b + s > 0 {
                    l.borrow_value += b;
                    l.supply_value += s;
                    borrowed += b;
                    supplied += s;
                    applied.users.insert(*user);
                }
            }
            if borrowed + supplied > 0 {
                let r = self.reserves.get_mut(&token).expect("reserve");
                r.borrowed += borrowed;
                r.supplied += supplied;
                self.reserves_dirty = true;
                changed = true;
            }
        }
        changed
    }

    /// Accounts past the liquidation ratio: the backstop takes collateral at
    /// the oracle price to clear every borrow.
    pub fn lend_liquidate(&mut self, applied: &mut Applied) {
        let users: Vec<Address> = self.accounts.iter().filter(|(_, a)| a.lend.values().any(|l| l.borrow_value > 0)).map(|(u, _)| *u).collect();
        for user in users {
            let (_, borrowed, liq) = self.lend_health(user);
            if borrowed * 100 <= liq * 95 {
                continue;
            }
            let mut need = borrowed;
            let collateral: Vec<(u64, u64)> = self.accounts[&user].lend.iter().filter(|(t, l)| l.supply_value > 0 && self.reserves.get(t).is_some_and(|r| r.ltv_bps > 0)).map(|(t, l)| (*t, l.supply_value)).collect();
            for (token, supply) in collateral {
                if need <= 0 {
                    break;
                }
                let value = self.token_usd(token, supply);
                let take = if value <= need { supply } else { (supply as i128 * need / value.max(1)) as u64 };
                need -= self.token_usd(token, take);
                let l = self.accounts.get_mut(&user).expect("user").lend.get_mut(&token).expect("lend");
                let before = l.supply_value;
                l.supply_value -= take;
                l.supply_basis = rescale(l.supply_basis, before, l.supply_value);
                if let Some(r) = self.reserves.get_mut(&token) {
                    r.supplied = r.supplied.saturating_sub(take);
                }
            }
            let borrows: Vec<(u64, u64)> = self.accounts[&user].lend.iter().filter(|(_, l)| l.borrow_value > 0).map(|(t, l)| (*t, l.borrow_value)).collect();
            for (token, value) in borrows {
                let l = self.accounts.get_mut(&user).expect("user").lend.get_mut(&token).expect("lend");
                l.borrow_value = 0;
                l.borrow_basis = 0;
                if let Some(r) = self.reserves.get_mut(&token) {
                    r.borrowed = r.borrowed.saturating_sub(value);
                }
            }
            self.reserves_dirty = true;
            self.lend_note(user, format!("liquidated: borrowed {} USD past 0.95 of the collateral's liquidation value {} USD, collateral taken by the backstop", fmt_dec(borrowed, 6), fmt_dec(liq, 6)));
            applied.users.insert(user);
            if !applied.liquidated.contains(&user) {
                applied.liquidated.push(user);
            }
        }
    }

    /// The 0x811 answers for a user, one per token with a position.
    pub fn lend_user_answers(&self, user: Address) -> Vec<Entry> {
        let Some(acct) = self.accounts.get(&user) else { return vec![] };
        acct.lend
            .iter()
            .map(|(token, l)| Entry {
                precompile: hypercore::BORROW_LEND_USER,
                calldata: [word_addr(user), word_u(*token as u128)].concat(),
                result: [word_u(l.borrow_basis as u128), word_u(l.borrow_value as u128), word_u(l.supply_basis as u128), word_u(l.supply_value as u128)].concat(),
            })
            .collect()
    }

    /// The 0x812 answers, one per reserve.
    pub fn reserve_answers(&self) -> Vec<Entry> {
        self.reserves
            .iter()
            .map(|(token, r)| {
                let sz = self.tokens.get(token).map_or(0, |t| t.sz_decimals as u32);
                let px_raw = r.oracle_x8 / 10u64.pow(sz.min(8));
                let bps = |e12: i128| (e12 / 100_000_000) as u128;
                Entry {
                    precompile: hypercore::BORROW_LEND_RESERVE,
                    calldata: word_u(*token as u128).to_vec(),
                    result: [
                        word_u(bps(r.borrow_rate_e12())),
                        word_u(bps(r.supply_rate_e12())),
                        word_u(r.supplied.saturating_sub(r.borrowed) as u128),
                        word_u(bps(r.utilization_e12())),
                        word_u(px_raw as u128),
                        word_u(r.ltv_bps as u128),
                        word_u(r.supplied as u128),
                        word_u(r.borrowed as u128),
                    ]
                    .concat(),
                }
            })
            .collect()
    }

    fn reserve_json(&self, token: u64, r: &Reserve) -> Value {
        let w = self.wei_decimals(token);
        let rate = |e12: i128| fmt_dec(e12, 12);
        json!({
            "borrowYearlyRate": rate(r.borrow_rate_e12()), "supplyYearlyRate": rate(r.supply_rate_e12()),
            "balance": fmt_dec(r.supplied.saturating_sub(r.borrowed) as i128, w), "utilization": rate(r.utilization_e12()),
            "oraclePx": fmt_dec(r.oracle_x8 as i128, 8), "ltv": fmt_dec(r.ltv_bps as i128, 4),
            "totalSupplied": fmt_dec(r.supplied as i128, w), "totalBorrowed": fmt_dec(r.borrowed as i128, w),
        })
    }

    /// The borrow and lend info request types, or None for another type.
    pub fn lend_info(&self, req: &Value) -> Option<Result<Value, String>> {
        match req["type"].as_str()? {
            "borrowLendUserState" => Some((|| {
                let user: Address = req["user"].as_str().and_then(|u| u.parse().ok()).ok_or("borrowLendUserState needs a user address")?;
                let acct = self.accounts.get(&user).cloned().unwrap_or_default();
                let states: Vec<Value> = acct
                    .lend
                    .iter()
                    .map(|(t, l)| {
                        let w = self.wei_decimals(*t);
                        let d = |v: u64| fmt_dec(v as i128, w);
                        json!([t, { "borrow": { "basis": d(l.borrow_basis), "value": d(l.borrow_value) }, "supply": { "basis": d(l.supply_basis), "value": d(l.supply_value) } }])
                    })
                    .collect();
                let hf = self.health_factor_e12(user);
                Ok(json!({
                    "tokenToState": states,
                    "health": if hf.is_some_and(|h| h < E12) { "atRisk" } else { "healthy" },
                    "healthFactor": hf.map(|h| fmt_dec(h, 12)),
                }))
            })()),
            "borrowLendReserveState" => Some((|| {
                let token = req["token"].as_u64().ok_or("borrowLendReserveState needs a token index")?;
                let r = self.reserves.get(&token).ok_or_else(|| format!("no borrow and lend reserve for token {token}"))?;
                Ok(self.reserve_json(token, r))
            })()),
            "allBorrowLendReserveStates" => Some(Ok(json!(self.reserves.iter().map(|(t, r)| json!([t, self.reserve_json(*t, r)])).collect::<Vec<_>>()))),
            _ => None,
        }
    }

    /// Reserves from the info API's `allBorrowLendReserveStates`. Tokens must
    /// be in `tokens` for their wei decimals.
    pub fn reserves_from_info(&mut self, all: &Value) {
        for pair in all.as_array().cloned().unwrap_or_default() {
            let (Some(token), r) = (pair[0].as_u64(), &pair[1]) else { continue };
            let w = self.wei_decimals(token);
            let dec = |k: &str, d: u32| r[k].as_str().and_then(|s| parse_dec(s, d)).unwrap_or(0).max(0);
            self.reserves.insert(
                token,
                Reserve {
                    ltv_bps: dec("ltv", 4) as u64,
                    oracle_x8: dec("oraclePx", 8) as u64,
                    supplied: dec("totalSupplied", w) as u64,
                    borrowed: dec("totalBorrowed", w) as u64,
                    rate_e12: dec("borrowYearlyRate", 12) as i64,
                },
            );
        }
        self.reserves_dirty = true;
    }

    /// A user's borrow and lend state from the info API's `borrowLendUserState`
    /// and `userAbstraction`. Tokens without a known reserve are skipped and returned.
    pub fn mirror_lend(&mut self, user: Address, state: &Value, abstraction: &Value) -> Vec<u64> {
        let mut skipped = Vec::new();
        let mut lend = std::collections::BTreeMap::new();
        for pair in state["tokenToState"].as_array().cloned().unwrap_or_default() {
            let Some(token) = pair[0].as_u64() else { continue };
            if !self.reserves.contains_key(&token) {
                skipped.push(token);
                continue;
            }
            let w = self.wei_decimals(token);
            let get = |a: &str, b: &str| pair[1][a][b].as_str().and_then(|s| parse_dec(s, w)).unwrap_or(0).max(0) as u64;
            lend.insert(token, Lend { borrow_basis: get("borrow", "basis"), borrow_value: get("borrow", "value"), supply_basis: get("supply", "basis"), supply_value: get("supply", "value") });
        }
        let acct = self.accounts.entry(user).or_default();
        acct.lend = lend;
        acct.abstraction = match abstraction.as_str() {
            Some("disabled") => 1,
            Some("unifiedAccount") => 2,
            Some("portfolioMargin") => 3,
            _ => 0,
        };
        skipped
    }
}
