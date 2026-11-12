//! `time` and `mine`: the fork's clock and slot moved forward with
//! `surfnet_timeTravel`. Time never moves back, `revert` does that.

use chainsim_core::fork::TimeRequest;
use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

use crate::rpc::Rpc;
use crate::SolanaAdapter;

pub const CLOCK_SYSVAR: &str = "SysvarC1ock11111111111111111111111111111111";
/// What `time` and `mine` say about reads right after a jump.
pub const FINALIZED_NOTE: &str = "slot is read at processed commitment. Reads at the default (finalized) commitment show the old slot for about 15 s after a jump, so pass {\"commitment\": \"processed\"}.";

/// The Clock sysvar: its slot and its Unix time in seconds.
pub async fn clock(rpc: &Rpc<'_>) -> Result<(u64, u64)> {
    let account = rpc.account(CLOCK_SYSVAR).await?.unwrap_or(Value::Null);
    let info = &account["data"]["parsed"]["info"];
    match (info["slot"].as_u64(), info["unixTimestamp"].as_u64()) {
        (Some(slot), Some(time)) => Ok((slot, time)),
        _ => Err(Error::new(ErrorCode::RpcError, format!("the Clock sysvar reads {account}"))),
    }
}

/// True for the engine's refusal to move to a slot or time it has passed.
fn is_past(e: &Error) -> bool {
    e.data.as_ref().is_some_and(|d| d["rpcData"].as_str().is_some_and(|s| s.contains("Cannot travel to past")))
}

pub async fn time(adapter: &SolanaAdapter, name: &str, req: &TimeRequest) -> Result<Value> {
    let (handle, profile) = adapter.open(name).await?;
    let rpc = adapter.rpc(&handle, &profile);
    let (slot_before, before) = clock(&rpc).await?;
    let past = |target: u64| Error::unsupported(&format!("moving the clock back from {before} s to {target} s"), format!("chainsim revert {name} <snapshot> goes back to an earlier state. time moves forward only"));
    let target = match req {
        TimeRequest::Advance(0) => return Err(Error::new(ErrorCode::Usage, "--advance takes one second or more")),
        TimeRequest::Advance(secs) => before.checked_add(*secs).ok_or_else(|| Error::new(ErrorCode::Usage, "the offset is too large"))?,
        TimeRequest::To(t) if *t <= before => return Err(past(*t)),
        TimeRequest::To(t) => *t,
    };
    let millis = target.checked_mul(1000).ok_or_else(|| Error::new(ErrorCode::Usage, "the timestamp is too large"))?;
    rpc.call("surfnet_timeTravel", json!([{ "absoluteTimestamp": millis }])).await.map_err(|e| if is_past(&e) { past(target) } else { e })?;
    let (_, after) = clock(&rpc).await?;
    let slot = rpc.slot().await?;
    adapter.record(name, json!({ "kind": "time", "from": before, "to": after, "slot": slot })).await?;
    Ok(json!({ "timestamp": after, "previous": before, "slot": slot, "previousSlot": slot_before, "note": FINALIZED_NOTE }))
}

pub async fn mine(adapter: &SolanaAdapter, name: &str, blocks: Option<u64>) -> Result<Value> {
    let (handle, profile) = adapter.open(name).await?;
    let rpc = adapter.rpc(&handle, &profile);
    let n = blocks.unwrap_or(1);
    if n == 0 {
        return Err(Error::new(ErrorCode::Usage, "mine takes one slot or more"));
    }
    // The engine makes a slot about every 400 ms on its own, so a target one
    // slot ahead can already be behind when the call lands: read and try again.
    let mut tries = 0;
    let before = loop {
        let before = rpc.slot().await?;
        let target = before.checked_add(n).ok_or_else(|| Error::new(ErrorCode::Usage, "the slot count is too large"))?;
        match rpc.call("surfnet_timeTravel", json!([{ "absoluteSlot": target }])).await {
            Ok(_) => break before,
            Err(e) if is_past(&e) && tries < 3 => tries += 1,
            Err(e) => return Err(e),
        }
    };
    let slot = rpc.slot().await?;
    let (_, timestamp) = clock(&rpc).await?;
    adapter.record(name, json!({ "kind": "mine", "slots": n, "slot": slot })).await?;
    Ok(json!({ "mined": n, "slot": slot, "previousSlot": before, "timestamp": timestamp, "note": FINALIZED_NOTE }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_engines_past_refusal_is_a_past_target() {
        let refusal = |text: &str| Error::new(ErrorCode::RpcError, "surfnet_timeTravel: Internal error (-32603)").data(json!({ "rpcData": text }));
        assert!(is_past(&refusal("Cannot travel to past slot: target=1, current=2")));
        assert!(is_past(&refusal("Cannot travel to past timestamp: target=1, current=2")));
        assert!(!is_past(&refusal("something else")));
        assert!(!is_past(&Error::new(ErrorCode::RpcError, "no data")));
    }
}
