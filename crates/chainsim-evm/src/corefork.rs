//! The local HyperCore on a running HyperEVM fork: where its state lives,
//! the catch-up every command runs, and the `set hypercore` and
//! `call hypercore_state` helpers.
//!
//! The driver itself (state on the fork, catch-up, credits) is in
//! [`crate::coresync`], shared with the HyperEVM proxy. The HyperCore lock
//! lives in the proxy: every command takes it there before it changes Core
//! state, so the proxy's own per-block apply never races a command.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use alloy_primitives::U256;
use chainsim_core::control::Target;
use chainsim_core::docker::ForkHandle;
use chainsim_core::profile::Profile;
use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

use crate::core::{self as hc, State};
use crate::coresync::{self, block_ref, block_time, CoreRpc, STATE_ADDRESS};
use crate::hypercore::{self, Entry};
use crate::units::*;
use crate::{now_secs, Engine, EvmAdapter, CALL_TIMEOUT};

/// The default Hyperliquid info API, replaced by `CHAINSIM_HYPERLIQUID_INFO`.
pub const INFO_DEFAULT: &str = "https://api.hyperliquid.xyz/info";
pub const INFO_ENV: &str = "CHAINSIM_HYPERLIQUID_INFO";
/// Requests per batch, below the control path's argument size limit.
const BATCH: usize = 150;

/// A `set hypercore` helper.
#[derive(Debug, Clone)]
pub enum CoreOp {
    Price { perp: u32, px: u64 },
    SpotPrice { pair: u32, px: u64 },
    SpotBalance { user: String, token: u64, total: u64, hold: u64, entry_ntl: u64 },
    Position { user: String, perp: u16, szi: i64, entry_ntl: u64, leverage: u32, isolated_raw_usd: i64, isolated: bool },
    Book { coin: String },
    Mirror { user: String },
    Funding { perp: u32, rate: String },
    BigBlocks { user: String, on: bool },
}

/// The info API URL and the variable that replaces it, for a HyperEVM profile.
pub(crate) fn info_source(profile: &Profile) -> Option<(String, String)> {
    let h = profile.hypercore.as_ref()?;
    let (default, env) = match &h.info {
        Some(i) => (i.default.clone(), i.env.clone()),
        None => (INFO_DEFAULT.to_string(), INFO_ENV.to_string()),
    };
    let url = std::env::var(&env).ok().filter(|v| !v.trim().is_empty()).unwrap_or(default);
    Some((url, env))
}

#[async_trait::async_trait]
impl CoreRpc for Engine<'_> {
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.rpc(method, params).await
    }

    async fn batch(&self, calls: &[(&str, Value)]) -> Result<Vec<Value>> {
        self.rpc_batch(calls).await
    }
}

impl Engine<'_> {
    /// Several JSON-RPC calls in one request per batch. Any error fails the whole call.
    pub(crate) async fn rpc_batch(&self, calls: &[(&str, Value)]) -> Result<Vec<Value>> {
        let mut out = Vec::with_capacity(calls.len());
        for chunk in calls.chunks(BATCH) {
            let body: Vec<Value> = chunk.iter().enumerate().map(|(i, (m, p))| json!({ "jsonrpc": "2.0", "id": i, "method": m, "params": p })).collect();
            let reply = self.control.post(&self.url, &Value::Array(body).to_string(), CALL_TIMEOUT).await?;
            let v: Value = serde_json::from_str(&reply.body).map_err(|_| Error::new(ErrorCode::EngineFailed, format!("batch of {}: HTTP {} with a body that is not JSON-RPC", chunk.len(), reply.status)))?;
            let mut replies = v.as_array().cloned().ok_or_else(|| Error::new(ErrorCode::EngineFailed, format!("batch of {}: the engine did not answer a list", chunk.len())))?;
            replies.sort_by_key(|r| r["id"].as_u64().unwrap_or(u64::MAX));
            if replies.len() != chunk.len() {
                return Err(Error::new(ErrorCode::EngineFailed, format!("batch of {}: {} answers", chunk.len(), replies.len())));
            }
            for (r, (m, _)) in replies.into_iter().zip(chunk) {
                if let Some(err) = r.get("error") {
                    let message = self.control.scrub(err.get("message").and_then(Value::as_str).unwrap_or("error"));
                    return Err(chainsim_core::control::classify_rpc_error(m, &message, err.get("code").and_then(Value::as_i64), self.control.env.as_deref()));
                }
                out.push(r.get("result").cloned().unwrap_or(Value::Null));
            }
        }
        Ok(out)
    }

    /// One info API request.
    async fn info(&self, body: Value, calls: &mut u32) -> Result<Value> {
        *calls += 1;
        let (url, env) = self.info.clone().unwrap_or_else(|| (INFO_DEFAULT.to_string(), INFO_ENV.to_string()));
        let info_next = || format!("retry in a minute, or export {env}=<info API URL>");
        let reply = self.control.post(&Target::Http(url), &body.to_string(), Duration::from_secs(30)).await.map_err(|e| e.next(info_next()))?;
        if reply.status == 429 {
            return Err(Error::new(ErrorCode::RateLimited, format!("info API {}: rate limited (HTTP 429)", body["type"])).next(info_next()));
        }
        if reply.status != 200 {
            return Err(Error::new(ErrorCode::UpstreamUnavailable, format!("info API {}: HTTP {}", body["type"], reply.status)).next(info_next()));
        }
        serde_json::from_str(&reply.body).map_err(|_| Error::new(ErrorCode::UpstreamUnavailable, format!("info API {}: the answer is not JSON", body["type"])).next(info_next()))
    }

}

/// Precompile prices are decimal strings with up to 6 - szDecimals decimals.
fn raw_px(px: &str, sz_decimals: u8) -> Option<u64> {
    let d = 6u32.checked_sub(sz_decimals as u32)?;
    hc::parse_dec(px, d).and_then(|v| u64::try_from(v).ok())
}

impl EvmAdapter {
    /// Takes the HyperCore lock in the fork's proxy, waiting while the proxy
    /// applies a block or another command holds it. Returns the lease token.
    async fn core_lock(&self, eng: &Engine<'_>) -> Result<String> {
        let token = eng.rpc_timeout("chainsim_coreLock", json!([]), Duration::from_secs(150)).await?;
        token.as_str().map(str::to_string).ok_or_else(|| Error::new(ErrorCode::EngineFailed, "the HyperEVM proxy did not grant the HyperCore lock").next("retry the command"))
    }

    async fn core_unlock(&self, eng: &Engine<'_>, token: &str) {
        let _ = eng.rpc("chainsim_coreUnlock", json!([token])).await;
    }

    /// Applies every Core event since the cursor, on a HyperEVM fork. A
    /// no-op elsewhere and on a fork without Core state.
    pub(crate) async fn core_sync(&self, _handle: &ForkHandle, profile: &Profile, eng: &Engine<'_>) -> Result<()> {
        if profile.hypercore.is_none() {
            return Ok(());
        }
        let lease = self.core_lock(eng).await?;
        let r = coresync::sync_locked(eng).await.map(|_| ());
        self.core_unlock(eng, &lease).await;
        r
    }

    /// `up`: the Core state from the info API, with the seeded marks, and the
    /// zero answers for users with no Core state. Returns the summary part.
    pub(crate) async fn core_init(&self, eng: &Engine<'_>, profile: &Profile, seeded: &Value) -> Result<Value> {
        let settings = profile.hypercore.as_ref().expect("hypercore profile");
        let code = eng.rpc("eth_getCode", json!([STATE_ADDRESS, "latest"])).await?;
        if code.as_str().is_some_and(|c| c != "0x") {
            return Err(Error::new(ErrorCode::Internal, format!("{STATE_ADDRESS} has code on this fork, so chainsim cannot keep the HyperCore state there")));
        }
        let mut calls = 0u32;
        let meta = eng.info(json!({ "type": "metaAndAssetCtxs" }), &mut calls).await?;
        let spot = eng.info(json!({ "type": "spotMeta" }), &mut calls).await?;
        let mut state = State::new();
        state.hype_token = settings.hype_token.unwrap_or(hc::HYPE_TOKEN);
        let universe = meta[0]["universe"].as_array().cloned().unwrap_or_default();
        for &p in &settings.perps {
            let u = universe.get(p as usize).ok_or_else(|| Error::new(ErrorCode::UpstreamUnavailable, format!("the info API meta has no perp {p}")))?;
            let d = u["szDecimals"].as_u64().unwrap_or(0) as u8;
            let mark_raw = seeded["perps"].as_array().and_then(|ps| ps.iter().find(|x| x["index"] == json!(p))).and_then(|x| x["markPx"].as_str()).and_then(|m| m.parse::<u64>().ok());
            let mark = mark_raw.map(|r| r * 10u64.pow(2 + d as u32)).or_else(|| meta[1][p as usize]["markPx"].as_str().and_then(|s| hc::parse_dec(s, 8)).map(|v| v as u64)).unwrap_or(0);
            let oracle_raw = seeded["perps"].as_array().and_then(|ps| ps.iter().find(|x| x["index"] == json!(p))).and_then(|x| x["oraclePx"].as_str()).and_then(|m| m.parse::<u64>().ok());
            let oracle = oracle_raw.map_or(mark, |r| r * 10u64.pow(2 + d as u32));
            let name = u["name"].as_str().unwrap_or_default().to_string();
            // The last settled hourly rate.
            let start = now_secs().saturating_sub(3 * hc::FUNDING_INTERVAL) * 1000;
            let history = eng.info(json!({ "type": "fundingHistory", "coin": name, "startTime": start }), &mut calls).await?;
            let funding = history.as_array().and_then(|h| h.last()).and_then(|f| f["fundingRate"].as_str()).and_then(|r| hc::parse_dec(r, 12)).unwrap_or(0) as i64;
            let tiers = hc::tiers_from_meta(u, &meta[0]["marginTables"]);
            state.perps.insert(p, hc::Perp { name, sz_decimals: d, max_leverage: u["maxLeverage"].as_u64().unwrap_or(1) as u32, mark_x8: mark, oracle_x8: oracle, funding_e12: funding, tiers, ..Default::default() });
        }
        // Borrow and lend reserves, with their tokens' decimals.
        let reserves = eng.info(json!({ "type": "allBorrowLendReserveStates" }), &mut calls).await?;
        let reserve_tokens: Vec<u64> = reserves.as_array().map_or(vec![], |r| r.iter().filter_map(|p| p[0].as_u64()).collect());
        add_tokens(&mut state, &spot, settings.tokens.iter().map(|t| *t as u64).chain(reserve_tokens));
        state.reserves_from_info(&reserves);
        // Collateral prices follow their spot pair and the perp on the same asset.
        for p in state.link_reserves(&spot, &meta[0]) {
            if let (false, Some(u)) = (state.perps.contains_key(&p), universe.get(p as usize)) {
                state.perps.insert(p, perp_from_ctx(u, &meta[1][p as usize], &meta[0]["marginTables"]));
            }
        }
        state.reprice_reserves();
        let mut books = Vec::new();
        for &p in &settings.perps {
            let coin = state.perps[&p].name.clone();
            let l2 = eng.info(json!({ "type": "l2Book", "coin": coin }), &mut calls).await?;
            let book = State::book_from_info(&l2).ok_or_else(|| Error::new(ErrorCode::UpstreamUnavailable, format!("the info API l2Book for {coin} does not parse")))?;
            books.push(book_summary(&coin, p, &book));
            state.books.insert(p, book);
        }
        let latest = eng.rpc("eth_getBlockByNumber", json!(["latest", false])).await?;
        state.cursor = Some(block_ref(&latest)?);
        state.funding_time = block_time(&latest)? / hc::FUNDING_INTERVAL * hc::FUNDING_INTERVAL;
        let mut writes: Vec<(&str, Value)> = Vec::new();
        for (p, zeros) in hypercore::zero_defaults() {
            for (slot, value) in hypercore::default_writes(&zeros) {
                writes.push(("anvil_setStorageAt", json!([hypercore::address(p), slot, value])));
            }
        }
        eng.rpc_batch(&writes).await?;
        let mut snapshot = hc::Meta::from_info(&meta);
        if !settings.dexes.is_empty() {
            let dexes = self.perp_dexes(eng, &mut calls).await?;
            for name in &settings.dexes {
                let index = dexes.iter().find(|(_, n)| n == name).map(|(i, _)| *i).ok_or_else(|| Error::new(ErrorCode::Internal, format!("the profile names perp dex {name}, which Hyperliquid does not list")))?;
                let raw = eng.info(json!({ "type": "metaAndAssetCtxs", "dex": name }), &mut calls).await?;
                snapshot.dexes.insert(name.clone(), hc::DexMeta { index, collateral: raw[0]["collateralToken"].as_u64().unwrap_or(0), meta: Box::new(hc::Meta::from_info(&raw)) });
            }
        }
        coresync::store_meta(eng, &snapshot).await?;
        let reserve_answers = state.reserve_answers();
        state.take_reserves_dirty();
        coresync::store(eng, &state, &reserve_answers).await?;
        chainsim_core::log(format_args!("HyperCore state: {calls} info API calls"));
        let funding: Vec<Value> = state.perps.values().map(|p| json!({ "coin": p.name, "rate": hc::fmt_dec(p.funding_e12 as i128, 12) })).collect();
        Ok(json!({ "books": books, "funding": funding, "reserves": state.reserves.len(), "infoCalls": calls, "state": STATE_ADDRESS }))
    }

    /// A `set hypercore` helper, applied to the Core state under the lock.
    pub async fn hypercore(&self, name: &str, op: CoreOp) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        if profile.hypercore.is_none() {
            return Err(Error::unsupported(&format!("set hypercore on {}", profile.key), "only HyperEVM has HyperCore"));
        }
        let eng = self.engine(&handle, &profile);
        let lease = self.core_lock(&eng).await?;
        let r = self.hypercore_locked(&handle, &profile, &eng, op).await;
        self.core_unlock(&eng, &lease).await;
        let (mut out, mut entry) = r?;
        entry["at"] = json!(now_secs());
        chainsim_core::mods::record(&self.docker, name, &entry).await?;
        out["note"] = json!(chainsim_core::mods::NOTE);
        Ok(out)
    }

    async fn hypercore_locked(&self, handle: &ForkHandle, profile: &Profile, eng: &Engine<'_>, op: CoreOp) -> Result<(Value, Value)> {
        let mut state = coresync::sync_locked(eng).await?.ok_or_else(|| Error::new(ErrorCode::Internal, "this fork has no HyperCore state").next(format!("chainsim down {0} && chainsim up {1} --name {0}", handle.name, handle.chain)))?;
        let mut calls = 0u32;
        match op {
            CoreOp::Price { perp, px } => {
                let entries = hypercore::price(perp, px);
                let (_, time) = eng.latest_block().await?;
                // A perp the state does not track yet comes from the meta snapshot, so it gets a mark.
                if !state.perps.contains_key(&perp) {
                    if perp < hc::DEX_SIZE {
                        state.add_perp_from_meta(perp, &coresync::load_meta(eng).await?);
                    } else {
                        self.resolve_perp(eng, &mut state, &perp.to_string(), &mut calls).await?;
                    }
                }
                let applied = match state.px_from_raw(perp, px) {
                    Some(x8) => state.set_price(perp, x8, time),
                    None => hc::Applied::default(),
                };
                let mut answers = state.answers_for(&applied);
                answers.extend(entries.iter().cloned());
                coresync::store(eng, &state, &answers).await?;
                coresync::settle_credits(eng, &mut state).await?;
                let sets = sets_json(&entries);
                let liquidated: Vec<String> = applied.liquidated.iter().map(|a| a.to_string()).collect();
                Ok((
                    json!({ "kind": "hypercore", "helper": "price", "perp": perp, "px": px, "filled": applied.fills, "liquidated": liquidated, "sets": sets }),
                    json!({ "kind": "set-hypercore", "helper": "price", "perp": perp, "px": px }),
                ))
            }
            CoreOp::SpotPrice { pair, px } => {
                let entry = hypercore::spot_price(pair, px);
                let (_, time) = eng.latest_block().await?;
                // Spot raw prices carry 8 minus the base token's szDecimals decimals.
                let base = state.reserves.iter().find(|(_, r)| r.spot == Some(pair)).map(|(t, _)| *t);
                let applied = match base.and_then(|t| state.tokens.get(&t)) {
                    Some(t) => state.set_spot_price(pair, px.saturating_mul(10u64.pow(t.sz_decimals.min(8) as u32)), time),
                    None => hc::Applied::default(),
                };
                let mut answers = state.answers_for(&applied);
                answers.push(entry.clone());
                coresync::store(eng, &state, &answers).await?;
                let liquidated: Vec<String> = applied.liquidated.iter().map(|a| a.to_string()).collect();
                let collateral: Vec<Value> = state.reserves.iter().filter(|(_, r)| r.spot == Some(pair)).map(|(t, r)| json!({ "token": t, "oraclePx": hc::fmt_dec(r.oracle_x8 as i128, 8) })).collect();
                Ok((
                    json!({ "kind": "hypercore", "helper": "price", "spot": pair, "px": px, "collateral": collateral, "liquidated": liquidated, "sets": sets_json(&[entry]) }),
                    json!({ "kind": "set-hypercore", "helper": "price", "spot": pair, "px": px }),
                ))
            }
            CoreOp::Funding { perp, rate } => {
                let e12 = hc::parse_dec(&rate, 12).and_then(|v| i64::try_from(v).ok()).ok_or_else(|| Error::new(ErrorCode::Usage, format!("{rate:?} is not a decimal funding rate, such as 0.0000125")))?;
                if !state.perps.contains_key(&perp) {
                    self.add_perps(eng, &mut state, &[perp], &mut calls).await?;
                }
                state.perps.get_mut(&perp).expect("perp").funding_e12 = e12;
                coresync::store(eng, &state, &[]).await?;
                let coin = state.perps[&perp].name.clone();
                let rate = hc::fmt_dec(e12 as i128, 12);
                Ok((
                    json!({ "kind": "hypercore", "helper": "funding", "perp": perp, "coin": coin, "rate": rate, "interval": hc::FUNDING_INTERVAL }),
                    json!({ "kind": "set-hypercore", "helper": "funding", "perp": perp, "rate": rate }),
                ))
            }
            CoreOp::SpotBalance { user, token, total, hold, entry_ntl } => {
                let who = parse_address(&user)?;
                state.accounts.entry(who).or_default().spot.insert(token, hc::Spot(total, hold, entry_ntl));
                let answers = state.answers(&BTreeSet::from([who]));
                coresync::store(eng, &state, &answers).await?;
                let e = hypercore::spot_balance(who, token, total, hold, entry_ntl);
                Ok((
                    json!({ "kind": "hypercore", "helper": "spot-balance", "user": who.to_string(), "token": token, "total": total, "sets": sets_json(&[e]) }),
                    json!({ "kind": "set-hypercore", "helper": "spot-balance", "user": who.to_string(), "token": token, "total": total }),
                ))
            }
            CoreOp::Position { user, perp, szi, entry_ntl, leverage, isolated_raw_usd, isolated } => {
                let who = parse_address(&user)?;
                let asset = perp as u32;
                if !state.perps.contains_key(&asset) {
                    self.add_perps(eng, &mut state, &[asset], &mut calls).await?;
                }
                let szi_x8 = state.sz_from_raw(asset, szi).ok_or_else(|| Error::new(ErrorCode::Usage, format!("szi {szi} is out of range for perp {perp}")))?;
                state.accounts.entry(who).or_default().positions.insert(asset, hc::Pos { szi_x8, entry_ntl, leverage, isolated, isolated_raw_usd, funding_paid: 0 });
                let answers = state.answers(&BTreeSet::from([who]));
                coresync::store(eng, &state, &answers).await?;
                let e = hypercore::position(who, perp, hypercore::Position { szi, entry_ntl, isolated_raw_usd, leverage, is_isolated: isolated });
                Ok((
                    json!({ "kind": "hypercore", "helper": "position", "user": who.to_string(), "perp": perp, "szi": szi, "sets": sets_json(&[e]) }),
                    json!({ "kind": "set-hypercore", "helper": "position", "user": who.to_string(), "perp": perp, "szi": szi }),
                ))
            }
            CoreOp::Book { coin } => {
                let asset = self.resolve_perp(eng, &mut state, &coin, &mut calls).await?;
                let name = state.perps[&asset].name.clone();
                let l2 = eng.info(json!({ "type": "l2Book", "coin": name }), &mut calls).await?;
                let book = State::book_from_info(&l2).ok_or_else(|| Error::new(ErrorCode::UpstreamUnavailable, format!("the info API l2Book for {name} does not parse")))?;
                let summary = book_summary(&name, asset, &book);
                let (_, time) = eng.latest_block().await?;
                // Resting orders the new book crosses fill, as on a price move.
                let applied = state.set_book(asset, book, time);
                coresync::store(eng, &state, &state.answers_for(&applied)).await?;
                Ok((
                    json!({ "kind": "hypercore", "helper": "book", "book": summary, "filled": applied.fills, "infoCalls": calls }),
                    json!({ "kind": "set-hypercore", "helper": "book", "coin": name, "time": summary["time"] }),
                ))
            }
            CoreOp::Mirror { user } => self.mirror(eng, profile, &mut state, &user).await,
            CoreOp::BigBlocks { user, on } => {
                let who = parse_address(&user)?;
                state.set_big_blocks(who, on);
                coresync::store(eng, &state, &[]).await?;
                let addr = who.to_string();
                Ok((
                    json!({ "kind": "hypercore", "helper": "big-blocks", "user": addr, "usingBigBlocks": on, "blockGasLimit": hc::block_gas_limit(on) }),
                    json!({ "kind": "set-hypercore", "helper": "big-blocks", "user": addr, "usingBigBlocks": on }),
                ))
            }
        }
    }

    /// The builder-deployed perp dexes: (index, name), from perpDexs.
    async fn perp_dexes(&self, eng: &Engine<'_>, calls: &mut u32) -> Result<Vec<(u32, String)>> {
        let list = eng.info(json!({ "type": "perpDexs" }), calls).await?;
        Ok(list.as_array().map_or(vec![], |l| l.iter().enumerate().filter_map(|(i, d)| d["name"].as_str().map(|n| (i as u32, n.to_string()))).collect()))
    }

    /// A builder-deployed dex's meta and asset contexts, recorded in the
    /// snapshot so /info can answer with the dex argument.
    async fn dex_meta(&self, eng: &Engine<'_>, index: u32, name: &str, calls: &mut u32) -> Result<Value> {
        let raw = eng.info(json!({ "type": "metaAndAssetCtxs", "dex": name }), calls).await?;
        let mut meta = coresync::load_meta(eng).await?;
        let collateral = raw[0]["collateralToken"].as_u64().unwrap_or(0);
        meta.dexes.insert(name.to_string(), hc::DexMeta { index, collateral, meta: Box::new(hc::Meta::from_info(&raw)) });
        coresync::store_meta(eng, &meta).await?;
        Ok(raw)
    }

    /// The perp index of a coin ("BTC", "xyz:XYZ100") or of an index, adding
    /// the perp to the state (and its dex to the snapshot) when the copy does
    /// not track it yet. Its mark and oracle answers are written then.
    async fn resolve_perp(&self, eng: &Engine<'_>, state: &mut State, coin: &str, calls: &mut u32) -> Result<u32> {
        if let Some((a, _)) = state.perps.iter().find(|(_, p)| p.name.eq_ignore_ascii_case(coin)) {
            return Ok(*a);
        }
        let index = coin.parse::<u32>().ok();
        if let Some(i) = index.filter(|i| state.perps.contains_key(i)) {
            return Ok(i);
        }
        let dex_name = match (index, coin.split_once(':')) {
            (Some(i), _) if i < hc::DEX_SIZE => None,
            (Some(i), _) => {
                let dexes = self.perp_dexes(eng, calls).await?;
                Some(dexes.into_iter().find(|(d, _)| *d == hc::dex_of(i)).map(|(_, n)| n).ok_or_else(|| Error::new(ErrorCode::Usage, format!("no perp dex {} on Hyperliquid", hc::dex_of(i))))?)
            }
            (None, Some((dex, _))) => Some(dex.to_string()),
            (None, None) => None,
        };
        let Some(dex_name) = dex_name else {
            // The main dex.
            let i = match index {
                Some(i) => i,
                None => {
                    let meta = eng.info(json!({ "type": "metaAndAssetCtxs" }), calls).await?;
                    let universe = meta[0]["universe"].as_array().cloned().unwrap_or_default();
                    universe.iter().position(|u| u["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(coin))).ok_or_else(|| Error::new(ErrorCode::Usage, format!("no perp {coin} on Hyperliquid")))? as u32
                }
            };
            self.add_perps(eng, state, &[i], calls).await?;
            return Ok(i);
        };
        let dex = match state.perps.keys().find(|a| state.dex_name(hc::dex_of(**a)) == dex_name) {
            Some(a) => hc::dex_of(*a),
            None => self.perp_dexes(eng, calls).await?.into_iter().find(|(_, n)| *n == dex_name).map(|(d, _)| d).ok_or_else(|| Error::new(ErrorCode::Usage, format!("no perp dex {dex_name} on Hyperliquid")))?,
        };
        let raw = self.dex_meta(eng, dex, &dex_name, calls).await?;
        let universe = raw[0]["universe"].as_array().cloned().unwrap_or_default();
        let local = match index {
            Some(i) => (i - dex * hc::DEX_SIZE) as usize,
            None => universe.iter().position(|u| u["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(coin))).ok_or_else(|| Error::new(ErrorCode::Usage, format!("no perp {coin} on Hyperliquid")))?,
        };
        let entries = insert_perp(state, dex * hc::DEX_SIZE + local as u32, &raw, local).ok_or_else(|| Error::new(ErrorCode::Usage, format!("no perp {coin} on Hyperliquid")))?;
        coresync::store(eng, state, &entries).await?;
        Ok(dex * hc::DEX_SIZE + local as u32)
    }

    /// Adds perps to the state from the info API, and writes their mark and oracle answers.
    async fn add_perps(&self, eng: &Engine<'_>, state: &mut State, assets: &[u32], calls: &mut u32) -> Result<()> {
        let meta = eng.info(json!({ "type": "metaAndAssetCtxs" }), calls).await?;
        let universe = meta[0]["universe"].as_array().cloned().unwrap_or_default();
        let mut entries = Vec::new();
        for &a in assets {
            let u = universe.get(a as usize).ok_or_else(|| Error::new(ErrorCode::Usage, format!("no perp {a} on Hyperliquid")))?;
            let ctx = &meta[1][a as usize];
            let d = u["szDecimals"].as_u64().unwrap_or(0) as u8;
            state.perps.insert(a, perp_from_ctx(u, ctx, &meta[0]["marginTables"]));
            if let (Some(m), Some(o)) = (ctx["markPx"].as_str().and_then(|s| raw_px(s, d)), ctx["oraclePx"].as_str().and_then(|s| raw_px(s, d))) {
                entries.push(Entry { precompile: hypercore::MARK_PX, calldata: hypercore::word_u(a as u128).to_vec(), result: hypercore::word_u(m as u128).to_vec() });
                entries.push(Entry { precompile: hypercore::ORACLE_PX, calldata: hypercore::word_u(a as u128).to_vec(), result: hypercore::word_u(o as u128).to_vec() });
            }
        }
        coresync::store(eng, state, &entries).await
    }

    /// `set hypercore mirror <user>`: the user's Core state from the info API,
    /// and its native HYPE and profile token balances from the upstream.
    async fn mirror(&self, eng: &Engine<'_>, profile: &Profile, state: &mut State, user: &str) -> Result<(Value, Value)> {
        let who = parse_address(user)?;
        let started = Instant::now();
        let mut calls = 0u32;
        let ch = eng.info(json!({ "type": "clearinghouseState", "user": who.to_string().to_lowercase() }), &mut calls).await?;
        let sch = eng.info(json!({ "type": "spotClearinghouseState", "user": who.to_string().to_lowercase() }), &mut calls).await?;
        let by_name: BTreeSet<String> = state.perps.values().map(|p| p.name.clone()).collect();
        let wanted: Vec<String> = ch["assetPositions"].as_array().map_or(vec![], |a| a.iter().filter_map(|p| p["position"]["coin"].as_str().map(str::to_string)).filter(|c| !by_name.contains(c)).collect());
        if !wanted.is_empty() {
            let meta = eng.info(json!({ "type": "metaAndAssetCtxs" }), &mut calls).await?;
            let universe = meta[0]["universe"].as_array().cloned().unwrap_or_default();
            let assets: Vec<u32> = wanted.iter().filter_map(|c| universe.iter().position(|u| u["name"] == json!(c)).map(|i| i as u32)).collect();
            // Uses the same meta: one more call would only repeat it.
            let mut entries = Vec::new();
            for a in assets {
                let (u, ctx) = (&universe[a as usize], &meta[1][a as usize]);
                let d = u["szDecimals"].as_u64().unwrap_or(0) as u8;
                state.perps.insert(a, perp_from_ctx(u, ctx, &meta[0]["marginTables"]));
                if let (Some(m), Some(o)) = (ctx["markPx"].as_str().and_then(|s| raw_px(s, d)), ctx["oraclePx"].as_str().and_then(|s| raw_px(s, d))) {
                    entries.push(Entry { precompile: hypercore::MARK_PX, calldata: hypercore::word_u(a as u128).to_vec(), result: hypercore::word_u(m as u128).to_vec() });
                    entries.push(Entry { precompile: hypercore::ORACLE_PX, calldata: hypercore::word_u(a as u128).to_vec(), result: hypercore::word_u(o as u128).to_vec() });
                }
            }
            coresync::store(eng, state, &entries).await?;
        }
        let unknown: Vec<u64> = sch["balances"].as_array().map_or(vec![], |b| b.iter().filter_map(|x| x["token"].as_u64()).filter(|t| !state.tokens.contains_key(t)).collect());
        if !unknown.is_empty() {
            let spot = eng.info(json!({ "type": "spotMeta" }), &mut calls).await?;
            add_tokens(state, &spot, unknown.into_iter());
        }
        let mut skipped = state.mirror_perps(who, &ch);
        skipped.extend(state.mirror_spot(who, &sch));

        // Builder-deployed dexes the user has value on, each with its own balance and positions.
        let mut dexes = Vec::new();
        for (index, name) in self.perp_dexes(eng, &mut calls).await? {
            let chd = eng.info(json!({ "type": "clearinghouseState", "user": who.to_string().to_lowercase(), "dex": name }), &mut calls).await?;
            let positions: Vec<String> = chd["assetPositions"].as_array().map_or(vec![], |a| a.iter().filter_map(|p| p["position"]["coin"].as_str().map(str::to_string)).collect());
            let value = chd["marginSummary"]["accountValue"].as_str().and_then(|v| hc::parse_dec(v, 6)).unwrap_or(0);
            if positions.is_empty() && value == 0 {
                continue;
            }
            let raw = self.dex_meta(eng, index, &name, &mut calls).await?;
            let universe = raw[0]["universe"].as_array().cloned().unwrap_or_default();
            let mut entries = Vec::new();
            for coin in &positions {
                if let Some(local) = universe.iter().position(|u| u["name"] == json!(coin)) {
                    let perp = index * hc::DEX_SIZE + local as u32;
                    if !state.perps.contains_key(&perp) {
                        entries.extend(insert_perp(state, perp, &raw, local).unwrap_or_default());
                    }
                }
            }
            coresync::store(eng, state, &entries).await?;
            skipped.extend(state.mirror_dex(who, index, &chd));
            dexes.push(name);
        }

        // Borrow and lend positions and the account abstraction.
        let lend = eng.info(json!({ "type": "borrowLendUserState", "user": who.to_string().to_lowercase() }), &mut calls).await?;
        let abstraction = eng.info(json!({ "type": "userAbstraction", "user": who.to_string().to_lowercase() }), &mut calls).await?;
        skipped.extend(state.mirror_lend(who, &lend, &abstraction).into_iter().map(|t| format!("borrow and lend token {t}")));
        let answers = state.answers(&BTreeSet::from([who]));
        coresync::store(eng, state, &answers).await?;

        // The EVM side: native HYPE and the profile tokens, read from the upstream.
        let addr = who.to_string();
        let mut rpc = 0u32;
        let mut evm = Vec::new();
        let native = parse_qty(&eng.upstream("eth_getBalance", json!([addr, "latest"])).await?)?;
        rpc += 1;
        eng.set_balance(&addr, native).await?;
        let (sym, dec) = profile.native.as_ref().map_or(("HYPE".to_string(), 18), |n| (n.symbol.clone(), n.decimals));
        evm.push(json!({ "asset": sym, "amount": format_amount(native, dec) }));
        for t in profile.tokens.iter().filter(|t| !t.native_view) {
            let out = eng.upstream("eth_call", json!([{ "to": t.address, "data": balance_of(who) }, "latest"])).await?;
            rpc += 1;
            let bal = parse_qty(&out)?;
            // A zero balance needs no write: the copy reads untouched accounts from the upstream.
            if bal > U256::ZERO {
                eng.deal(&t.address, who, bal).await?;
            }
            evm.push(json!({ "asset": t.symbol, "amount": format_amount(bal, t.decimals) }));
        }
        let info = state.info(who, 0);
        let positions = info["clearinghouseState"]["assetPositions"].as_array().map_or(0, Vec::len);
        let balances = info["spotClearinghouseState"]["balances"].as_array().map_or(0, |b| b.iter().filter(|x| x["total"] != "0.0").count());
        chainsim_core::log(format_args!("mirrored {addr}: {calls} info API calls, {rpc} upstream RPC calls, {} ms", started.elapsed().as_millis()));
        Ok((
            json!({
                "kind": "hypercore", "helper": "mirror", "user": addr, "positions": positions, "spotBalances": balances,
                "accountValue": info["clearinghouseState"]["marginSummary"]["accountValue"], "dexes": dexes,
                "borrowLend": info["borrowLendUserState"]["tokenToState"].as_array().map_or(0, Vec::len), "abstraction": abstraction,
                "evm": evm, "skipped": skipped,
                "infoCalls": calls, "upstreamCalls": rpc,
            }),
            json!({ "kind": "set-hypercore", "helper": "mirror", "user": addr, "positions": positions }),
        ))
    }

    /// `call hypercore_state [<user>]`: one user's Core state in the info API's shapes.
    pub(crate) async fn hypercore_state(&self, eng: &Engine<'_>, params: &Value) -> Result<Value> {
        let user = params.get(0).and_then(Value::as_str).ok_or_else(|| Error::new(ErrorCode::Usage, "hypercore_state takes a user address: chainsim call <name> hypercore_state <address>"))?;
        let who = parse_address(user)?;
        let lease = self.core_lock(eng).await?;
        let r = coresync::sync_locked(eng).await;
        self.core_unlock(eng, &lease).await;
        let state = r?.ok_or_else(|| Error::new(ErrorCode::Internal, "this fork has no HyperCore state"))?;
        let (_, ts) = eng.latest_block().await?;
        Ok(state.info(who, ts * 1000))
    }

    /// The applied result of each CoreWriter action, by block and log index.
    pub(crate) async fn action_results(&self, eng: &Engine<'_>, decoded: &mut [Value]) -> Result<()> {
        let Some(state) = coresync::load_state(eng).await? else { return Ok(()) };
        for d in decoded {
            let at = (d["block"].as_u64().unwrap_or(0), d["logIndex"].as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(0));
            d["result"] = json!(state.result_of(at));
        }
        Ok(())
    }

    /// `time --replay <coin> --from <t> --to <t>`: the coin's hourly mainnet
    /// candles become its price path. Each candle mines one block an hour
    /// after the last, pays funding, then moves mark and oracle to the
    /// candle's close with the same fills and liquidations as `set hypercore price`.
    pub async fn replay(&self, name: &str, coin: &str, from: u64, to: u64) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        if profile.hypercore.is_none() {
            return Err(Error::unsupported(&format!("time --replay on {}", profile.key), "chainsim time <name> --advance <seconds> moves the clock"));
        }
        if to <= from {
            return Err(Error::new(ErrorCode::Usage, "--to must be later than --from"));
        }
        if hc::replay_candles(from, to) > hc::MAX_REPLAY_CANDLES {
            return Err(Error::new(ErrorCode::Usage, format!("a replay covers at most {} hourly candles (31 days), and this range has {}", hc::MAX_REPLAY_CANDLES, hc::replay_candles(from, to)))
                .next("replay a shorter range, or several ranges one after another"));
        }
        let eng = self.engine(&handle, &profile);
        let lease = self.core_lock(&eng).await?;
        let r = self.replay_locked(&handle, &eng, coin, from, to).await;
        self.core_unlock(&eng, &lease).await;
        let (out, mut entry) = r?;
        entry["at"] = json!(now_secs());
        chainsim_core::mods::record(&self.docker, name, &entry).await?;
        Ok(out)
    }

    async fn replay_locked(&self, handle: &ForkHandle, eng: &Engine<'_>, coin: &str, from: u64, to: u64) -> Result<(Value, Value)> {
        let mut state = coresync::sync_locked(eng).await?.ok_or_else(|| Error::new(ErrorCode::Internal, "this fork has no HyperCore state").next(format!("chainsim down {0} && chainsim up {1} --name {0}", handle.name, handle.chain)))?;
        let mut calls = 0u32;
        let asset = match state.perps.iter().find(|(_, p)| p.name.eq_ignore_ascii_case(coin)) {
            Some((a, _)) => *a,
            None => {
                let meta = eng.info(json!({ "type": "metaAndAssetCtxs" }), &mut calls).await?;
                let universe = meta[0]["universe"].as_array().cloned().unwrap_or_default();
                let i = universe.iter().position(|u| u["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(coin))).ok_or_else(|| Error::new(ErrorCode::Usage, format!("no perp {coin} on Hyperliquid")))? as u32;
                state.perps.insert(i, perp_from_ctx(&universe[i as usize], &meta[1][i as usize], &meta[0]["marginTables"]));
                i
            }
        };
        let name = state.perps[&asset].name.clone();
        let candles = eng.info(json!({ "type": "candleSnapshot", "req": { "coin": name, "interval": "1h", "startTime": from * 1000, "endTime": to * 1000 } }), &mut calls).await?;
        let path = candle_path(&candles);
        if path.is_empty() {
            return Err(Error::new(ErrorCode::Usage, format!("the info API has no {name} candles between {from} and {to}")).next("pick a range in the past, such as the previous day"));
        }
        let started = Instant::now();
        let (mut fills, mut liquidated, mut height) = (0usize, Vec::new(), 0u64);
        let (mut low, mut high) = (u64::MAX, 0u64);
        for (_, close) in &path {
            let (_, now) = eng.latest_block().await?;
            eng.rpc("evm_setNextBlockTimestamp", json!([now + hc::FUNDING_INTERVAL])).await?;
            eng.rpc("evm_mine", json!([])).await?;
            let b = eng.rpc("eth_getBlockByNumber", json!(["latest", false])).await?;
            let (head, time) = (block_ref(&b)?, block_time(&b)?);
            height = head.0;
            // Blocks another client mined since the cursor are applied too, never skipped.
            let from = state.cursor.as_ref().map_or(head.0, |c| c.0 + 1);
            let events = coresync::core_events(eng, &state, from, head.0).await?;
            let applied = state.candle_step(&events, asset, *close, head, time);
            fills += applied.fills;
            liquidated.extend(applied.liquidated.iter().map(|a| a.to_string()));
            let mut answers = state.answers_for(&applied);
            if let Some(raw) = state.px_to_raw(asset, *close) {
                answers.extend(hypercore::price(asset, raw));
            }
            coresync::store(eng, &state, &answers).await?;
            low = low.min(*close);
            high = high.max(*close);
        }
        coresync::settle_credits(eng, &mut state).await?;
        let px = |v: u64| hc::fmt_dec(v as i128, 8);
        let last = path.last().expect("not empty").1;
        chainsim_core::log(format_args!("replayed {} {name} candles in {} ms, {calls} info API calls", path.len(), started.elapsed().as_millis()));
        Ok((
            json!({
                "coin": name, "perp": asset, "interval": "1h", "candles": path.len(),
                "from": path[0].0 / 1000, "to": path.last().expect("not empty").0 / 1000,
                "firstClose": px(path[0].1), "lastClose": px(last), "lowClose": px(low), "highClose": px(high),
                "markPx": state.px_to_raw(asset, last), "height": height, "filled": fills, "liquidated": liquidated, "infoCalls": calls,
            }),
            json!({ "kind": "time-replay", "coin": name, "candles": path.len(), "from": from, "to": to }),
        ))
    }

    /// What `status` adds: accounts, open orders and the cursor.
    pub(crate) async fn core_status(&self, eng: &Engine<'_>) -> Result<Value> {
        Ok(match coresync::load_state(eng).await? {
            Some(s) => json!({
                "accounts": s.accounts.len(), "openOrders": s.orders.len(), "books": s.books.keys().filter_map(|a| s.perps.get(a).map(|p| p.name.clone())).collect::<Vec<_>>(),
                "appliedThrough": s.cursor.map(|c| c.0),
            }),
            None => Value::Null,
        })
    }
}

/// Inserts the perp at `local` in a metaAndAssetCtxs answer as perp index
/// `perp`, with its dex's collateral token, and returns its mark and oracle
/// answers. None when the meta has no such perp.
fn insert_perp(state: &mut State, perp: u32, raw: &Value, local: usize) -> Option<Vec<Entry>> {
    let u = raw[0]["universe"].as_array()?.get(local)?.clone();
    let ctx = raw[1].as_array()?.get(local)?.clone();
    let d = u["szDecimals"].as_u64().unwrap_or(0) as u8;
    let mut p = perp_from_ctx(&u, &ctx, &raw[0]["marginTables"]);
    p.collateral = raw[0]["collateralToken"].as_u64().unwrap_or(0);
    state.perps.insert(perp, p);
    let mut entries = Vec::new();
    if let (Some(m), Some(o)) = (ctx["markPx"].as_str().and_then(|s| raw_px(s, d)), ctx["oraclePx"].as_str().and_then(|s| raw_px(s, d))) {
        entries.push(Entry { precompile: hypercore::MARK_PX, calldata: hypercore::word_u(perp as u128).to_vec(), result: hypercore::word_u(m as u128).to_vec() });
        entries.push(Entry { precompile: hypercore::ORACLE_PX, calldata: hypercore::word_u(perp as u128).to_vec(), result: hypercore::word_u(o as u128).to_vec() });
    }
    Some(entries)
}

/// A perp from the info API's meta and asset context: the current mark,
/// oracle and funding rate.
fn perp_from_ctx(u: &Value, ctx: &Value, tables: &Value) -> hc::Perp {
    let px = |k: &str| ctx[k].as_str().and_then(|s| hc::parse_dec(s, 8)).unwrap_or(0) as u64;
    hc::Perp {
        name: u["name"].as_str().unwrap_or_default().to_string(),
        sz_decimals: u["szDecimals"].as_u64().unwrap_or(0) as u8,
        max_leverage: u["maxLeverage"].as_u64().unwrap_or(1) as u32,
        mark_x8: px("markPx"),
        oracle_x8: px("oraclePx"),
        funding_e12: ctx["funding"].as_str().and_then(|s| hc::parse_dec(s, 12)).unwrap_or(0) as i64,
        // HIP-3 perps marked onlyIsolated or strictIsolated hold isolated margin.
        isolated_only: u["onlyIsolated"] == json!(true) || u["marginMode"].as_str().is_some_and(|m| m.contains("solated")),
        collateral: 0,
        tiers: hc::tiers_from_meta(u, tables),
        hip3_funding: None,
    }
}

fn add_tokens(state: &mut State, spot: &Value, tokens: impl Iterator<Item = u64>) {
    let list = spot["tokens"].as_array().cloned().unwrap_or_default();
    for t in tokens {
        let Some(m) = list.iter().find(|x| x["index"].as_u64() == Some(t)) else { continue };
        // USDC's evmContract is the CoreDepositWallet, not an ERC-20: its path is not modeled.
        let evm = m["evmContract"]["address"].as_str().filter(|_| t != 0).map(|a| (a.to_string(), m["evmContract"]["evm_extra_wei_decimals"].as_i64().unwrap_or(0) as i8));
        state.tokens.insert(t, hc::TokenMeta { name: m["name"].as_str().unwrap_or_default().to_string(), wei_decimals: m["weiDecimals"].as_u64().unwrap_or(8) as u8, evm, sz_decimals: m["szDecimals"].as_u64().unwrap_or(0) as u8 });
    }
}

/// The candles' open times (ms) and closes (x8), oldest first.
fn candle_path(candles: &Value) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = candles.as_array().map_or(vec![], |c| c.iter().filter_map(|k| Some((k["t"].as_u64()?, hc::parse_dec(k["c"].as_str()?, 8)? as u64))).collect());
    out.sort();
    out
}

/// A time for `--from` and `--to`: Unix seconds, a date (UTC midnight), or
/// a date and time such as 2026-11-01T12:00Z.
pub fn parse_time(s: &str) -> Result<u64> {
    let bad = || Error::new(ErrorCode::Usage, format!("{s:?} is not a time: use Unix seconds, YYYY-MM-DD or YYYY-MM-DDTHH:MM[:SS]Z"));
    let t = s.trim();
    if let Ok(n) = t.parse::<u64>() {
        return Ok(n);
    }
    let (date, clock) = t.trim_end_matches('Z').split_once('T').unwrap_or((t, "00:00"));
    let d: Vec<i64> = date.split('-').map(|x| x.parse().map_err(|_| bad())).collect::<Result<_>>()?;
    let c: Vec<i64> = clock.split(':').map(|x| x.parse().map_err(|_| bad())).collect::<Result<_>>()?;
    let ([y, m, day], true) = (<[i64; 3]>::try_from(d).map_err(|_| bad())?, (2..=3).contains(&c.len())) else { return Err(bad()) };
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || c[0] > 23 || c[1] > 59 || c.get(2).is_some_and(|x| *x > 59) {
        return Err(bad());
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + c[0] * 3600 + c[1] * 60 + c.get(2).copied().unwrap_or(0);
    u64::try_from(secs).map_err(|_| bad())
}

fn book_summary(coin: &str, asset: u32, b: &hc::Book) -> Value {
    let px = |l: Option<&(u64, u64)>| l.map(|(p, _)| hc::fmt_dec(*p as i128, 8));
    json!({ "coin": coin, "perp": asset, "bestBid": px(b.bids.first()), "bestAsk": px(b.asks.first()), "bidLevels": b.bids.len(), "askLevels": b.asks.len(), "time": b.time })
}

fn sets_json(entries: &[Entry]) -> Vec<Value> {
    entries
        .iter()
        .map(|e| json!({ "kind": "precompile", "address": hypercore::address(e.precompile), "calldata": hypercore::to_hex(&e.calldata), "result": hypercore::to_hex(&e.result), "source": "given" }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usdc_has_no_evm_side_and_linked_tokens_do() {
        let spot = json!({ "tokens": [
            { "name": "USDC", "index": 0, "weiDecimals": 8, "evmContract": { "address": "0x6b9e773128f453f5c2c60935ee2de2cbc5390a24", "evm_extra_wei_decimals": -2 } },
            { "name": "TKN", "index": 200, "weiDecimals": 6, "evmContract": { "address": "0x00000000000000000000000000000000000000aa", "evm_extra_wei_decimals": 12 } },
            { "name": "HYPE", "index": 150, "weiDecimals": 8, "evmContract": null },
        ] });
        let mut s = State::new();
        add_tokens(&mut s, &spot, [0, 150, 200, 9].into_iter());
        assert_eq!((s.tokens[&0].evm.clone(), s.tokens[&150].evm.clone()), (None, None));
        assert_eq!(s.tokens[&200].evm, Some(("0x00000000000000000000000000000000000000aa".to_string(), 12)));
        assert!(!s.tokens.contains_key(&9));
    }

    #[test]
    fn times_parse_as_seconds_dates_and_date_times() {
        assert_eq!(parse_time("1793600000").unwrap(), 1_793_600_000);
        assert_eq!(parse_time("1970-01-01").unwrap(), 0);
        assert_eq!(parse_time("2026-11-01").unwrap(), 1_793_491_200);
        assert_eq!(parse_time("2026-11-01T12:30Z").unwrap(), 1_793_491_200 + 45_000);
        assert_eq!(parse_time("2024-02-29T00:00:01Z").unwrap(), 1_709_164_801);
        assert!(parse_time("2026-13-01").is_err() && parse_time("yesterday").is_err() && parse_time("2026-11").is_err());
    }

    #[test]
    fn candles_become_a_sorted_close_path() {
        let c = json!([{ "t": 2000, "c": "78230.0" }, { "t": 1000, "c": "77932.5" }, { "t": 3000 }]);
        assert_eq!(candle_path(&c), [(1000, 7_793_250_000_000), (2000, 7_823_000_000_000)]);
    }

    #[test]
    fn info_prices_become_precompile_units() {
        // BTC has szDecimals 5: one decimal place; HYPE has 2: four.
        assert_eq!(raw_px("78349.0", 5), Some(783_490));
        assert_eq!(raw_px("41.2345", 2), Some(412_345));
        assert_eq!(raw_px("1", 7), None);
    }
}
