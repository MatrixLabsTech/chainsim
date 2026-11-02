//! The local HyperCore on a running HyperEVM fork: where its state lives,
//! the catch-up every command runs, and the `set hypercore` and
//! `call hypercore_state` helpers.
//!
//! All Core state is fork state, so snapshot and revert carry it: the state
//! is the code of [`STATE_ADDRESS`] (a STOP byte, a marker, then the state as
//! JSON), and the answers it produces are in the precompile mock's storage.
//! Catch-up runs under a lock in the control sidecar, and the cursor in the
//! state makes a second pass over the same blocks a no-op.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use alloy_primitives::{Address, U256};
use chainsim_core::control::Target;
use chainsim_core::docker::ForkHandle;
use chainsim_core::profile::Profile;
use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

use crate::core::{self as hc, State};
use crate::hypercore::{self, Entry};
use crate::units::*;
use crate::{now_secs, Engine, EvmAdapter, CALL_TIMEOUT};

/// Where the Core state lives on the fork. It has no code and no balance on
/// HyperEVM Mainnet, and `up` checks that before using it.
pub const STATE_ADDRESS: &str = "0x9999999999999999999999999999999999999999";
/// The state code starts with STOP, so a call to the address does nothing.
const MARKER: &[u8] = b"\x00chainsim-core\x00";
/// The largest state the control path can write in one call.
const MAX_STATE: usize = 60_000;
/// The default Hyperliquid info API, replaced by `CHAINSIM_HYPERLIQUID_INFO`.
pub const INFO_DEFAULT: &str = "https://api.hyperliquid.xyz/info";
pub const INFO_ENV: &str = "CHAINSIM_HYPERLIQUID_INFO";
/// Requests per batch, below the control path's argument size limit.
const BATCH: usize = 150;
/// The catch-up lock in the control sidecar's tmpfs, and when a lock counts as left behind.
const LOCK_DIR: &str = "/tmp/chainsim-core.lock";
const LOCK_STALE_SECS: u64 = 120;

/// A `set hypercore` helper.
#[derive(Debug, Clone)]
pub enum CoreOp {
    Price { perp: u32, px: u64 },
    SpotBalance { user: String, token: u64, total: u64, hold: u64, entry_ntl: u64 },
    Position { user: String, perp: u16, szi: i64, entry_ntl: u64, leverage: u32, isolated_raw_usd: i64, isolated: bool },
    Book { coin: String },
    Mirror { user: String },
}

fn info_url() -> String {
    std::env::var(INFO_ENV).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| INFO_DEFAULT.to_string())
}

fn info_next() -> String {
    format!("retry in a minute, or export {INFO_ENV}=<info API URL>")
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
        let reply = self.control.post(&Target::Http(info_url()), &body.to_string(), Duration::from_secs(30)).await.map_err(|e| e.next(info_next()))?;
        if reply.status == 429 {
            return Err(Error::new(ErrorCode::RateLimited, format!("info API {}: rate limited (HTTP 429)", body["type"])).next(info_next()));
        }
        if reply.status != 200 {
            return Err(Error::new(ErrorCode::UpstreamUnavailable, format!("info API {}: HTTP {}", body["type"], reply.status)).next(info_next()));
        }
        serde_json::from_str(&reply.body).map_err(|_| Error::new(ErrorCode::UpstreamUnavailable, format!("info API {}: the answer is not JSON", body["type"])).next(info_next()))
    }

    /// The Core state on the fork, or None before `up` wrote it.
    async fn load_state(&self) -> Result<Option<State>> {
        let code = self.rpc("eth_getCode", json!([STATE_ADDRESS, "latest"])).await?;
        decode_state(&code)
    }

    /// Writes the state and the answers it changed, in as few requests as fit.
    async fn store(&self, state: &State, answers: &[Entry]) -> Result<()> {
        let mut blob = MARKER.to_vec();
        blob.extend(state.to_bytes());
        if blob.len() > MAX_STATE {
            return Err(Error::new(ErrorCode::Internal, format!("the HyperCore state is {} bytes, over the {MAX_STATE} the fork can hold", blob.len()))
                .next("mirror an account with fewer positions, or restart the fork with chainsim down and up"));
        }
        let mut calls: Vec<(&str, Value)> = Vec::new();
        for e in answers {
            let target = hypercore::address(e.precompile);
            for (slot, value) in hypercore::storage_writes(&e.calldata, &e.result) {
                calls.push(("anvil_setStorageAt", json!([target, slot, value])));
            }
        }
        calls.push(("anvil_setCode", json!([STATE_ADDRESS, hypercore::to_hex(&blob)])));
        self.rpc_batch(&calls).await.map(|_| ())
    }

    /// Credits Core owes the EVM side: native HYPE or a linked ERC-20.
    async fn credit(&self, credits: &[hc::Credit]) -> Result<()> {
        for c in credits {
            match c.token {
                None => {
                    let have = self.balance(&c.to.to_string()).await?;
                    self.set_balance(&c.to.to_string(), have + c.amount).await?;
                }
                Some(token) => {
                    let have = self.token_balance(&token.to_string(), c.to).await?;
                    self.deal(&token.to_string(), c.to, have + c.amount).await?;
                }
            }
        }
        Ok(())
    }
}

fn decode_state(code: &Value) -> Result<Option<State>> {
    let bytes = code.as_str().and_then(|c| alloy_primitives::hex::decode(c).ok()).unwrap_or_default();
    if bytes.is_empty() {
        return Ok(None);
    }
    let body = bytes.strip_prefix(MARKER).ok_or_else(|| Error::new(ErrorCode::Internal, format!("{STATE_ADDRESS} holds code that is not chainsim's HyperCore state")))?;
    State::from_bytes(body).map(Some).ok_or_else(|| Error::new(ErrorCode::Internal, "the HyperCore state on the fork does not decode").next("chainsim down <name> && chainsim up hyperevm-mainnet"))
}

fn block_ref(b: &Value) -> Result<(u64, String)> {
    Ok((parse_qty(&b["number"])?.to::<u64>(), b["hash"].as_str().unwrap_or_default().to_string()))
}

/// Precompile prices are decimal strings with up to 6 - szDecimals decimals.
fn raw_px(px: &str, sz_decimals: u8) -> Option<u64> {
    let d = 6u32.checked_sub(sz_decimals as u32)?;
    hc::parse_dec(px, d).and_then(|v| u64::try_from(v).ok())
}

impl EvmAdapter {
    async fn core_lock(&self, handle: &ForkHandle) -> Result<()> {
        let script = format!(
            "i=0; while ! mkdir {LOCK_DIR} 2>/dev/null; do now=$(date +%s); t=$(cat {LOCK_DIR}/t 2>/dev/null || echo $now); \
             if [ $((now - t)) -gt {LOCK_STALE_SECS} ]; then rm -rf {LOCK_DIR}; continue; fi; i=$((i+1)); [ $i -gt 1200 ] && exit 9; sleep 0.1; done; date +%s > {LOCK_DIR}/t"
        );
        let out = self.docker.exec(&handle.control_container, &["sh".into(), "-c".into(), script]).await?;
        if out.exit_code != 0 {
            return Err(Error::new(ErrorCode::EngineFailed, "another chainsim command held the HyperCore lock for over two minutes").next("retry the command"));
        }
        Ok(())
    }

    async fn core_unlock(&self, handle: &ForkHandle) {
        let _ = self.docker.exec(&handle.control_container, &["rm".into(), "-rf".into(), LOCK_DIR.into()]).await;
    }

    /// Applies every Core event since the cursor, on a HyperEVM fork. A
    /// no-op elsewhere and on a fork without Core state.
    pub(crate) async fn core_sync(&self, handle: &ForkHandle, profile: &Profile, eng: &Engine<'_>) -> Result<()> {
        if profile.hypercore.is_none() {
            return Ok(());
        }
        self.core_lock(handle).await?;
        let r = self.core_sync_locked(eng).await.map(|_| ());
        self.core_unlock(handle).await;
        r
    }

    /// The catch-up itself, under the lock. Returns the state after it.
    async fn core_sync_locked(&self, eng: &Engine<'_>) -> Result<Option<State>> {
        let got = eng.rpc_batch(&[("eth_getCode", json!([STATE_ADDRESS, "latest"])), ("eth_getBlockByNumber", json!(["latest", false]))]).await?;
        let Some(mut state) = decode_state(&got[0])? else { return Ok(None) };
        let head = block_ref(&got[1])?;
        let Some((at, hash)) = state.cursor.clone() else { return Ok(Some(state)) };
        if head.0 <= at {
            if head.0 < at || head.1 != hash {
                // The chain no longer holds the cursor block: take the chain
                // as it is and apply nothing twice.
                chainsim_core::log(format_args!("HyperCore cursor {at} moved to {} without applying", head.0));
                state.cursor = Some(head);
                eng.store(&state, &[]).await?;
            }
            return Ok(Some(state));
        }
        let mut addresses = vec![json!(hypercore::CORE_WRITER), json!(hc::HYPE_SYSTEM.to_string())];
        addresses.extend(state.tokens.values().filter_map(|t| t.evm.as_ref()).map(|(a, _)| json!(a)));
        let filter = json!({
            "fromBlock": format!("0x{:x}", at + 1), "toBlock": format!("0x{:x}", head.0), "address": addresses,
            "topics": [[hypercore::RAW_ACTION_TOPIC, hc::received_topic(), hc::TRANSFER_TOPIC]],
        });
        let logs = eng.rpc("eth_getLogs", json!([filter])).await?;
        let events = hc::events_from_logs(logs.as_array().map_or(&[][..], Vec::as_slice));
        let applied = state.apply(&events, head);
        let answers = state.answers(&applied.users);
        eng.store(&state, &answers).await?;
        eng.credit(&applied.credits).await?;
        if !events.is_empty() {
            chainsim_core::log(format_args!("HyperCore applied {} events, {} answers", events.len(), answers.len()));
        }
        Ok(Some(state))
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
        let universe = meta[0]["universe"].as_array().cloned().unwrap_or_default();
        for &p in &settings.perps {
            let u = universe.get(p as usize).ok_or_else(|| Error::new(ErrorCode::UpstreamUnavailable, format!("the info API meta has no perp {p}")))?;
            let d = u["szDecimals"].as_u64().unwrap_or(0) as u8;
            let mark_raw = seeded["perps"].as_array().and_then(|ps| ps.iter().find(|x| x["index"] == json!(p))).and_then(|x| x["markPx"].as_str()).and_then(|m| m.parse::<u64>().ok());
            let mark = mark_raw.map(|r| r * 10u64.pow(2 + d as u32)).or_else(|| meta[1][p as usize]["markPx"].as_str().and_then(|s| hc::parse_dec(s, 8)).map(|v| v as u64)).unwrap_or(0);
            state.perps.insert(p, hc::Perp { name: u["name"].as_str().unwrap_or_default().to_string(), sz_decimals: d, max_leverage: u["maxLeverage"].as_u64().unwrap_or(1) as u32, mark_x8: mark });
        }
        add_tokens(&mut state, &spot, settings.tokens.iter().map(|t| *t as u64));
        let mut books = Vec::new();
        for &p in &settings.perps {
            let coin = state.perps[&p].name.clone();
            let l2 = eng.info(json!({ "type": "l2Book", "coin": coin }), &mut calls).await?;
            let book = State::book_from_info(&l2).ok_or_else(|| Error::new(ErrorCode::UpstreamUnavailable, format!("the info API l2Book for {coin} does not parse")))?;
            books.push(book_summary(&coin, p, &book));
            state.books.insert(p, book);
        }
        state.cursor = Some(block_ref(&eng.rpc("eth_getBlockByNumber", json!(["latest", false])).await?)?);
        let mut writes: Vec<(&str, Value)> = Vec::new();
        for (p, zeros) in hypercore::zero_defaults() {
            for (slot, value) in hypercore::default_writes(&zeros) {
                writes.push(("anvil_setStorageAt", json!([hypercore::address(p), slot, value])));
            }
        }
        eng.rpc_batch(&writes).await?;
        eng.store(&state, &[]).await?;
        chainsim_core::log(format_args!("HyperCore state: {calls} info API calls"));
        Ok(json!({ "books": books, "infoCalls": calls, "state": STATE_ADDRESS }))
    }

    /// A `set hypercore` helper, applied to the Core state under the lock.
    pub async fn hypercore(&self, name: &str, op: CoreOp) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        if profile.hypercore.is_none() {
            return Err(Error::unsupported(&format!("set hypercore on {}", profile.key), "only HyperEVM has HyperCore"));
        }
        let eng = self.engine(&handle, &profile);
        self.core_lock(&handle).await?;
        let r = self.hypercore_locked(&handle, &profile, &eng, op).await;
        self.core_unlock(&handle).await;
        let (mut out, mut entry) = r?;
        entry["at"] = json!(now_secs());
        chainsim_core::mods::record(&self.docker, name, &entry).await?;
        out["note"] = json!(chainsim_core::mods::NOTE);
        Ok(out)
    }

    async fn hypercore_locked(&self, handle: &ForkHandle, profile: &Profile, eng: &Engine<'_>, op: CoreOp) -> Result<(Value, Value)> {
        let mut state = self.core_sync_locked(eng).await?.ok_or_else(|| Error::new(ErrorCode::Internal, "this fork has no HyperCore state").next(format!("chainsim down {0} && chainsim up hyperevm-mainnet --name {0}", handle.name)))?;
        let mut calls = 0u32;
        match op {
            CoreOp::Price { perp, px } => {
                let entries = hypercore::price(perp, px);
                if let Some(x8) = state.px_from_raw(perp, px) {
                    state.perps.get_mut(&perp).expect("perp").mark_x8 = x8;
                }
                // Margin figures of every account move with the mark.
                let users: BTreeSet<Address> = state.accounts.keys().copied().collect();
                let mut answers = state.answers(&users);
                answers.extend(entries.iter().cloned());
                eng.store(&state, &answers).await?;
                let sets = sets_json(&entries);
                Ok((json!({ "kind": "hypercore", "helper": "price", "perp": perp, "px": px, "sets": sets }), json!({ "kind": "set-hypercore", "helper": "price", "perp": perp, "px": px })))
            }
            CoreOp::SpotBalance { user, token, total, hold, entry_ntl } => {
                let who = parse_address(&user)?;
                state.accounts.entry(who).or_default().spot.insert(token, hc::Spot(total, hold, entry_ntl));
                let answers = state.answers(&BTreeSet::from([who]));
                eng.store(&state, &answers).await?;
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
                state.accounts.entry(who).or_default().positions.insert(asset, hc::Pos { szi_x8, entry_ntl, leverage, isolated, isolated_raw_usd });
                let answers = state.answers(&BTreeSet::from([who]));
                eng.store(&state, &answers).await?;
                let e = hypercore::position(who, perp, hypercore::Position { szi, entry_ntl, isolated_raw_usd, leverage, is_isolated: isolated });
                Ok((
                    json!({ "kind": "hypercore", "helper": "position", "user": who.to_string(), "perp": perp, "szi": szi, "sets": sets_json(&[e]) }),
                    json!({ "kind": "set-hypercore", "helper": "position", "user": who.to_string(), "perp": perp, "szi": szi }),
                ))
            }
            CoreOp::Book { coin } => {
                let asset = match coin.parse::<u32>() {
                    Ok(a) => a,
                    Err(_) => match state.perps.iter().find(|(_, p)| p.name.eq_ignore_ascii_case(&coin)) {
                        Some((a, _)) => *a,
                        None => {
                            let meta = eng.info(json!({ "type": "metaAndAssetCtxs" }), &mut calls).await?;
                            let universe = meta[0]["universe"].as_array().cloned().unwrap_or_default();
                            let i = universe.iter().position(|u| u["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(&coin))).ok_or_else(|| Error::new(ErrorCode::Usage, format!("no perp {coin} on Hyperliquid")))?;
                            i as u32
                        }
                    },
                };
                if !state.perps.contains_key(&asset) {
                    self.add_perps(eng, &mut state, &[asset], &mut calls).await?;
                }
                let name = state.perps[&asset].name.clone();
                let l2 = eng.info(json!({ "type": "l2Book", "coin": name }), &mut calls).await?;
                let book = State::book_from_info(&l2).ok_or_else(|| Error::new(ErrorCode::UpstreamUnavailable, format!("the info API l2Book for {name} does not parse")))?;
                let summary = book_summary(&name, asset, &book);
                state.books.insert(asset, book);
                eng.store(&state, &[]).await?;
                Ok((json!({ "kind": "hypercore", "helper": "book", "book": summary, "infoCalls": calls }), json!({ "kind": "set-hypercore", "helper": "book", "coin": name, "time": summary["time"] })))
            }
            CoreOp::Mirror { user } => self.mirror(eng, profile, &mut state, &user).await,
        }
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
            let mark = ctx["markPx"].as_str().and_then(|s| hc::parse_dec(s, 8)).unwrap_or(0) as u64;
            state.perps.insert(a, hc::Perp { name: u["name"].as_str().unwrap_or_default().to_string(), sz_decimals: d, max_leverage: u["maxLeverage"].as_u64().unwrap_or(1) as u32, mark_x8: mark });
            if let (Some(m), Some(o)) = (ctx["markPx"].as_str().and_then(|s| raw_px(s, d)), ctx["oraclePx"].as_str().and_then(|s| raw_px(s, d))) {
                entries.push(Entry { precompile: hypercore::MARK_PX, calldata: hypercore::word_u(a as u128).to_vec(), result: hypercore::word_u(m as u128).to_vec() });
                entries.push(Entry { precompile: hypercore::ORACLE_PX, calldata: hypercore::word_u(a as u128).to_vec(), result: hypercore::word_u(o as u128).to_vec() });
            }
        }
        eng.store(state, &entries).await
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
                state.perps.insert(a, hc::Perp { name: u["name"].as_str().unwrap_or_default().to_string(), sz_decimals: d, max_leverage: u["maxLeverage"].as_u64().unwrap_or(1) as u32, mark_x8: ctx["markPx"].as_str().and_then(|s| hc::parse_dec(s, 8)).unwrap_or(0) as u64 });
                if let (Some(m), Some(o)) = (ctx["markPx"].as_str().and_then(|s| raw_px(s, d)), ctx["oraclePx"].as_str().and_then(|s| raw_px(s, d))) {
                    entries.push(Entry { precompile: hypercore::MARK_PX, calldata: hypercore::word_u(a as u128).to_vec(), result: hypercore::word_u(m as u128).to_vec() });
                    entries.push(Entry { precompile: hypercore::ORACLE_PX, calldata: hypercore::word_u(a as u128).to_vec(), result: hypercore::word_u(o as u128).to_vec() });
                }
            }
            eng.store(state, &entries).await?;
        }
        let unknown: Vec<u64> = sch["balances"].as_array().map_or(vec![], |b| b.iter().filter_map(|x| x["token"].as_u64()).filter(|t| !state.tokens.contains_key(t)).collect());
        if !unknown.is_empty() {
            let spot = eng.info(json!({ "type": "spotMeta" }), &mut calls).await?;
            add_tokens(state, &spot, unknown.into_iter());
        }
        let mut skipped = state.mirror_perps(who, &ch);
        skipped.extend(state.mirror_spot(who, &sch));
        let answers = state.answers(&BTreeSet::from([who]));
        eng.store(state, &answers).await?;

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
                "accountValue": info["clearinghouseState"]["marginSummary"]["accountValue"], "evm": evm, "skipped": skipped,
                "infoCalls": calls, "upstreamCalls": rpc,
            }),
            json!({ "kind": "set-hypercore", "helper": "mirror", "user": addr, "positions": positions }),
        ))
    }

    /// `call hypercore_state [<user>]`: one user's Core state in the info API's shapes.
    pub(crate) async fn hypercore_state(&self, eng: &Engine<'_>, handle: &ForkHandle, params: &Value) -> Result<Value> {
        let user = params.get(0).and_then(Value::as_str).ok_or_else(|| Error::new(ErrorCode::Usage, "hypercore_state takes a user address: chainsim call <name> hypercore_state <address>"))?;
        let who = parse_address(user)?;
        self.core_lock(handle).await?;
        let r = self.core_sync_locked(eng).await;
        self.core_unlock(handle).await;
        let state = r?.ok_or_else(|| Error::new(ErrorCode::Internal, "this fork has no HyperCore state"))?;
        let (_, ts) = eng.latest_block().await?;
        Ok(state.info(who, ts * 1000))
    }

    /// The applied result of each CoreWriter action, by block and log index.
    pub(crate) async fn action_results(&self, eng: &Engine<'_>, decoded: &mut [Value]) -> Result<()> {
        let Some(state) = eng.load_state().await? else { return Ok(()) };
        for d in decoded {
            let at = (d["block"].as_u64().unwrap_or(0), d["logIndex"].as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(0));
            d["result"] = json!(state.result_of(at));
        }
        Ok(())
    }

    /// What `status` adds: accounts, open orders and the cursor.
    pub(crate) async fn core_status(&self, eng: &Engine<'_>) -> Result<Value> {
        Ok(match eng.load_state().await? {
            Some(s) => json!({
                "accounts": s.accounts.len(), "openOrders": s.orders.len(), "books": s.books.keys().filter_map(|a| s.perps.get(a).map(|p| p.name.clone())).collect::<Vec<_>>(),
                "appliedThrough": s.cursor.map(|c| c.0),
            }),
            None => Value::Null,
        })
    }
}

fn add_tokens(state: &mut State, spot: &Value, tokens: impl Iterator<Item = u64>) {
    let list = spot["tokens"].as_array().cloned().unwrap_or_default();
    for t in tokens {
        let Some(m) = list.iter().find(|x| x["index"].as_u64() == Some(t)) else { continue };
        // USDC's evmContract is the CoreDepositWallet, not an ERC-20: its path is not modeled.
        let evm = m["evmContract"]["address"].as_str().filter(|_| t != 0).map(|a| (a.to_string(), m["evmContract"]["evm_extra_wei_decimals"].as_i64().unwrap_or(0) as i8));
        state.tokens.insert(t, hc::TokenMeta { name: m["name"].as_str().unwrap_or_default().to_string(), wei_decimals: m["weiDecimals"].as_u64().unwrap_or(8) as u8, evm });
    }
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
    fn state_code_round_trips_and_foreign_code_is_refused() {
        let mut s = State::new();
        s.cursor = Some((5, "0x05".into()));
        let mut blob = MARKER.to_vec();
        blob.extend(s.to_bytes());
        assert_eq!(decode_state(&json!(hypercore::to_hex(&blob))).unwrap(), Some(s));
        assert_eq!(decode_state(&json!("0x")).unwrap(), None);
        assert!(decode_state(&json!("0x6080")).is_err());
        // A call into the state address stops at once.
        assert_eq!(MARKER[0], 0x00);
    }

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
    fn info_prices_become_precompile_units() {
        // BTC has szDecimals 5: one decimal place; HYPE has 2: four.
        assert_eq!(raw_px("78349.0", 5), Some(783_490));
        assert_eq!(raw_px("41.2345", 2), Some(412_345));
        assert_eq!(raw_px("1", 7), None);
    }
}
