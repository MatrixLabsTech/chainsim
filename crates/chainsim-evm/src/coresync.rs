//! The local HyperCore's driver, shared by the CLI and the HyperEVM proxy:
//! where Core state and the meta snapshot live on the fork, the catch-up that
//! applies each block's events, and the credits Core owes the EVM side. It
//! reaches the engine through [`CoreRpc`], so each side brings its own
//! transport (the control sidecar for the CLI, HTTP for the proxy).
//!
//! All Core state is fork state, so snapshot and revert carry it: the state
//! is the code of [`STATE_ADDRESS`] (a STOP byte, a marker, then the state as
//! JSON), and the answers it produces are in the precompile mock's storage.
//! The cursor in the state makes a second pass over the same blocks a no-op.

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

use crate::core::{self as hc, Meta, State};
use crate::hypercore::{self, Entry};
use crate::units::*;

/// Where the Core state lives on the fork. It has no code and no balance on
/// HyperEVM Mainnet, and `up` checks that before using it.
pub const STATE_ADDRESS: &str = "0x9999999999999999999999999999999999999999";
/// Where the perp meta snapshot from `up` lives, read by the info answers.
pub const META_ADDRESS: &str = "0x9999999999999999999999999999999999999998";
/// The state code starts with STOP, so a call to the address does nothing.
const MARKER: &[u8] = b"\x00chainsim-core\x00";
/// The largest state the control path can write in one call.
pub const MAX_STATE: usize = 60_000;

/// JSON-RPC to the engine.
#[async_trait]
pub trait CoreRpc: Send + Sync {
    async fn call(&self, method: &str, params: Value) -> Result<Value>;
    /// Several calls in as few requests as fit. Any error fails the whole call.
    async fn batch(&self, calls: &[(&str, Value)]) -> Result<Vec<Value>>;
}

fn blob(body: Vec<u8>, what: &str) -> Result<String> {
    let mut b = MARKER.to_vec();
    b.extend(body);
    if b.len() > MAX_STATE {
        return Err(Error::new(ErrorCode::Internal, format!("the HyperCore {what} is {} bytes, over the {MAX_STATE} the fork can hold", b.len()))
            .next("mirror an account with fewer positions, or restart the fork with chainsim down and up"));
    }
    Ok(hypercore::to_hex(&b))
}

fn unblob(code: &Value, address: &str) -> Result<Option<Vec<u8>>> {
    let bytes = code.as_str().and_then(|c| alloy_primitives::hex::decode(c).ok()).unwrap_or_default();
    if bytes.is_empty() {
        return Ok(None);
    }
    bytes
        .strip_prefix(MARKER)
        .map(|b| Some(b.to_vec()))
        .ok_or_else(|| Error::new(ErrorCode::Internal, format!("{address} holds code that is not chainsim's HyperCore state")))
}

/// The state from its code, or None before `up` wrote it.
pub fn decode_state(code: &Value) -> Result<Option<State>> {
    match unblob(code, STATE_ADDRESS)? {
        None => Ok(None),
        Some(b) => State::from_bytes(&b).map(Some).ok_or_else(|| Error::new(ErrorCode::Internal, "the HyperCore state on the fork does not decode").next("chainsim down <name>, then chainsim up <chain> --name <name>")),
    }
}

pub async fn load_state(rpc: &dyn CoreRpc) -> Result<Option<State>> {
    decode_state(&rpc.call("eth_getCode", json!([STATE_ADDRESS, "latest"])).await?)
}

/// The meta snapshot, empty before `up` wrote it.
pub async fn load_meta(rpc: &dyn CoreRpc) -> Result<Meta> {
    let code = rpc.call("eth_getCode", json!([META_ADDRESS, "latest"])).await?;
    Ok(unblob(&code, META_ADDRESS)?.and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default())
}

pub async fn store_meta(rpc: &dyn CoreRpc, meta: &Meta) -> Result<()> {
    let code = blob(serde_json::to_vec(meta).expect("meta serializes"), "meta snapshot")?;
    rpc.call("anvil_setCode", json!([META_ADDRESS, code])).await.map(|_| ())
}

/// Writes the state and the answers it changed, in as few requests as fit.
pub async fn store(rpc: &dyn CoreRpc, state: &State, answers: &[Entry]) -> Result<()> {
    let code = blob(state.to_bytes(), "state")?;
    let mut calls: Vec<(&str, Value)> = Vec::new();
    for e in answers {
        let target = hypercore::address(e.precompile);
        for (slot, value) in hypercore::storage_writes(&e.calldata, &e.result) {
            calls.push(("anvil_setStorageAt", json!([target, slot, value])));
        }
    }
    calls.push(("anvil_setCode", json!([STATE_ADDRESS, code])));
    rpc.batch(&calls).await.map(|_| ())
}

pub fn block_ref(b: &Value) -> Result<(u64, String)> {
    Ok((parse_qty(&b["number"])?.to::<u64>(), b["hash"].as_str().unwrap_or_default().to_string()))
}

pub fn block_time(b: &Value) -> Result<u64> {
    Ok(parse_qty(&b["timestamp"])?.to::<u64>())
}

/// The Core events in blocks `from..=to`: RawAction, HYPE to its system
/// address, and Transfers of linked tokens.
pub async fn core_events(rpc: &dyn CoreRpc, state: &State, from: u64, to: u64) -> Result<Vec<hc::Logged>> {
    let mut addresses = vec![json!(hypercore::CORE_WRITER), json!(hc::HYPE_SYSTEM.to_string())];
    addresses.extend(state.tokens.values().filter_map(|t| t.evm.as_ref()).map(|(a, _)| json!(a)));
    let filter = json!({
        "fromBlock": format!("0x{from:x}"), "toBlock": format!("0x{to:x}"), "address": addresses,
        "topics": [[hypercore::RAW_ACTION_TOPIC, hc::received_topic(), hc::TRANSFER_TOPIC]],
    });
    let logs = rpc.call("eth_getLogs", json!([filter])).await?;
    Ok(hc::events_from_logs(logs.as_array().map_or(&[][..], Vec::as_slice)))
}

/// The catch-up, under the caller's lock: applies every Core event since the
/// cursor, writes the answers it changed, and lands pending credits. Returns
/// the state after it, or None on a fork without Core state.
pub async fn sync_locked(rpc: &dyn CoreRpc) -> Result<Option<State>> {
    let got = rpc.batch(&[("eth_getCode", json!([STATE_ADDRESS, "latest"])), ("eth_getBlockByNumber", json!(["latest", false]))]).await?;
    let Some(mut state) = decode_state(&got[0])? else { return Ok(None) };
    let head = block_ref(&got[1])?;
    let time = block_time(&got[1])?;
    let Some((at, hash)) = state.cursor.clone() else { return Ok(Some(state)) };
    if head.0 <= at {
        if head.0 < at || head.1 != hash {
            // The chain no longer holds the cursor block: take the chain as
            // it is and apply nothing twice.
            chainsim_core::log(format_args!("HyperCore cursor {at} moved to {} without applying", head.0));
            state.cursor = Some(head);
            store(rpc, &state, &[]).await?;
        }
        settle_credits(rpc, &mut state).await?;
        return Ok(Some(state));
    }
    let events = core_events(rpc, &state, at + 1, head.0).await?;
    let applied = state.apply(&events, head, time);
    let answers = state.answers_for(&applied);
    store(rpc, &state, &answers).await?;
    settle_credits(rpc, &mut state).await?;
    if !events.is_empty() {
        chainsim_core::log(format_args!("HyperCore applied {} events, {} answers", events.len(), answers.len()));
    }
    Ok(Some(state))
}

async fn native_balance(rpc: &dyn CoreRpc, who: Address) -> Result<U256> {
    parse_qty(&rpc.call("eth_getBalance", json!([who.to_string(), "latest"])).await?)
}

pub async fn token_balance(rpc: &dyn CoreRpc, token: &str, who: Address) -> Result<U256> {
    parse_qty(&rpc.call("eth_call", json!([{ "to": token, "data": balance_of(who) }, "latest"])).await?)
}

/// Sets an ERC-20 balance: `anvil_dealERC20`, then a balance slot search when
/// the deal does not read back.
pub async fn deal(rpc: &dyn CoreRpc, token: &str, who: Address, amount: U256) -> Result<()> {
    let dealt = rpc.call("anvil_dealERC20", json!([who.to_string(), token, qty(amount)])).await;
    if dealt.is_ok() && token_balance(rpc, token, who).await? == amount {
        return Ok(());
    }
    for vyper in [false, true] {
        for slot in 0..=50u64 {
            let key = mapping_slot(who, slot, vyper);
            let before = rpc.call("eth_getStorageAt", json!([token, key, "latest"])).await?;
            rpc.call("anvil_setStorageAt", json!([token, key, format!("0x{:064x}", amount)])).await?;
            if token_balance(rpc, token, who).await.ok() == Some(amount) {
                return Ok(());
            }
            let restore = format!("0x{:0>64}", before.as_str().unwrap_or("0x0").trim_start_matches("0x"));
            rpc.call("anvil_setStorageAt", json!([token, key, restore])).await?;
        }
    }
    Err(Error::new(ErrorCode::ReadbackMismatch, format!("could not set the balance of {token}: anvil_dealERC20 and the slot search both failed")))
}

/// Lands the credits Core owes the EVM side (native HYPE or a linked ERC-20).
/// Each one first records the balance it starts from in the state, then sets
/// `base + amount` and leaves the queue. A failed one stays queued and is
/// retried at the next catch-up, and a retry sets the same target, so a
/// credit is never applied twice.
pub async fn settle_credits(rpc: &dyn CoreRpc, state: &mut State) -> Result<()> {
    if state.pending.is_empty() {
        return Ok(());
    }
    let unbased: Vec<hc::Credit> = state.pending.iter().filter(|c| c.base.is_none()).cloned().collect();
    for c in &unbased {
        let have = match c.token {
            None => native_balance(rpc, c.to).await?,
            Some(token) => token_balance(rpc, &token.to_string(), c.to).await?,
        };
        state.credit_base(c.id, have);
    }
    if !unbased.is_empty() {
        store(rpc, state, &[]).await?;
    }
    for c in state.pending.clone() {
        let Some(target) = c.target() else { continue };
        let landed = match c.token {
            None => rpc.call("anvil_setBalance", json!([c.to.to_string(), qty(target)])).await.map(|_| ()),
            Some(token) => deal(rpc, &token.to_string(), c.to, target).await,
        };
        match landed {
            Ok(()) => state.credit_landed(c.id),
            Err(e) => chainsim_core::log(format_args!("HyperCore credit {} to {} kept for a retry: {}", c.id, c.to, e.message)),
        }
    }
    store(rpc, state, &[]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_code_round_trips_and_foreign_code_is_refused() {
        let mut s = State::new();
        s.cursor = Some((5, "0x05".into()));
        let code = blob(s.to_bytes(), "state").unwrap();
        assert_eq!(decode_state(&json!(code)).unwrap(), Some(s));
        assert_eq!(decode_state(&json!("0x")).unwrap(), None);
        assert!(decode_state(&json!("0x6080")).is_err());
        // A call into the state address stops at once.
        assert_eq!(MARKER[0], 0x00);
        assert!(blob(vec![0u8; MAX_STATE], "state").is_err());
    }
}
