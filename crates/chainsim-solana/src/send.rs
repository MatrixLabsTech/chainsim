//! `send --as`: a System transfer, or a transaction built by any SDK, sent as
//! an address without its key. The engine skips the signature check for that
//! one request and keeps checking every other client's transactions.

use std::io::Read;
use std::time::{Duration, Instant};

use chainsim_core::fork::SendRequest;
use chainsim_core::profile::Profile;
use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

use crate::fund;
use crate::rpc::{processed, Rpc};
use crate::units::{base58_decode, base58_encode, format_amount, parse_address, parse_amount};
use crate::SolanaAdapter;

pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
/// How long `send` waits for the engine to report the transaction's status.
const STATUS_TIMEOUT: Duration = Duration::from_secs(15);
const FORMS: &str = "chainsim send <name> --as <address> --to <address> --value <SOL>, or chainsim send <name> --as <address> --data <base64 transaction>";

/// The compact-u16 length prefix of the transaction format.
fn shortvec_encode(mut n: usize, out: &mut Vec<u8>) {
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A compact-u16 at the start of `bytes`: the value and how many bytes it took.
fn shortvec_decode(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut n = 0usize;
    for (i, b) in bytes.iter().take(3).enumerate() {
        n |= ((b & 0x7f) as usize) << (7 * i);
        if b & 0x80 == 0 {
            return Some((n, i + 1));
        }
    }
    None
}

/// A legacy transaction with one empty signature slot and one System
/// transfer of `lamports` from `from` to `to`.
pub fn transfer_transaction(from: &[u8], to: &[u8], blockhash: &[u8], lamports: u64) -> Vec<u8> {
    let mut tx = vec![1u8];
    tx.extend([0u8; 64]);
    // Header: one signer, no read-only signer, one read-only account (the program).
    tx.extend([1, 0, 1]);
    shortvec_encode(3, &mut tx);
    tx.extend(from);
    tx.extend(to);
    tx.extend([0u8; 32]);
    tx.extend(blockhash);
    shortvec_encode(1, &mut tx);
    // Program index 2, accounts [0, 1], data: instruction 2 (transfer) and the amount.
    tx.extend([2, 2, 0, 1, 12, 2, 0, 0, 0]);
    tx.extend(lamports.to_le_bytes());
    tx
}

/// Where the parts `send` rewrites sit in a serialized transaction.
#[derive(Debug, PartialEq, Eq)]
pub struct Layout {
    /// The number of signature slots, which start at `signatures`.
    pub slots: usize,
    pub signatures: usize,
    /// The required signers, in order. The first pays the fee.
    pub signers: Vec<String>,
    pub blockhash: usize,
}

/// Reads the layout of a legacy or version 0 transaction.
pub fn layout(tx: &[u8]) -> Result<Layout> {
    let bad = |what: &str| Error::new(ErrorCode::Usage, format!("--data is not a serialized Solana transaction: {what}")).next(FORMS);
    let (slots, signatures) = shortvec_decode(tx).ok_or_else(|| bad("no signature count"))?;
    let mut at = signatures + slots * 64;
    // A version 0 message starts with a byte that has the high bit set.
    if tx.get(at).is_some_and(|b| b & 0x80 != 0) {
        at += 1;
    }
    let header = tx.get(at..at + 3).ok_or_else(|| bad("it ends before the message header"))?;
    let required = header[0] as usize;
    at += 3;
    let (keys, n) = shortvec_decode(tx.get(at..).unwrap_or_default()).ok_or_else(|| bad("no account count"))?;
    at += n;
    if required == 0 || required != slots || required > keys {
        return Err(bad("the signature slots do not match the required signers"));
    }
    let blockhash = at + keys * 32;
    if tx.len() < blockhash + 32 {
        return Err(bad("it ends before the blockhash"));
    }
    let signers = (0..required).map(|i| base58_encode(&tx[at + i * 32..at + (i + 1) * 32])).collect();
    Ok(Layout { slots, signatures, signers, blockhash })
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Standard base64, with or without padding. None when a character is outside the alphabet.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u8);
    for c in s.trim().trim_end_matches('=').bytes() {
        acc = acc << 6 | B64.iter().position(|a| *a == c)? as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Random signature bytes, so two sends never share a signature id.
fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).map_err(|e| Error::new(ErrorCode::Internal, format!("read /dev/urandom: {e}")))?;
    Ok(buf)
}

/// What one send did, as `send` returns it.
pub struct Sent {
    pub signature: String,
    pub slot: u64,
    pub err: Value,
    pub logs: Value,
}

/// Sets the fork's blockhash and random signatures on `tx`, sends it with the
/// signature check skipped for this request, and waits for its status.
pub async fn submit(rpc: &Rpc<'_>, name: &str, mut tx: Vec<u8>, at: &Layout) -> Result<Sent> {
    let payer = &at.signers[0];
    match rpc.account(payer).await? {
        None => {
            return Err(Error::new(ErrorCode::TxReverted, format!("the fee payer {payer} holds no SOL on the copy")).next(format!("chainsim set {name} balance {payer} 1")));
        }
        Some(a) if a["owner"] != SYSTEM_PROGRAM => {
            return Err(Error::unsupported(
                &format!("a fee payer owned by program {}", a["owner"].as_str().unwrap_or("?")),
                "build the transaction with a wallet as its fee payer and pass it with --data: only an account of the System program can pay the fee",
            ));
        }
        Some(_) => {}
    }
    let latest = rpc.call("getLatestBlockhash", json!([processed()])).await?;
    let blockhash = latest["value"]["blockhash"].as_str().and_then(base58_decode).filter(|b| b.len() == 32).ok_or_else(|| Error::new(ErrorCode::RpcError, format!("getLatestBlockhash answered {latest}")))?;
    tx[at.blockhash..at.blockhash + 32].copy_from_slice(&blockhash);
    let signatures = random_bytes(at.slots * 64)?;
    tx[at.signatures..at.signatures + signatures.len()].copy_from_slice(&signatures);

    let sent = rpc.call("sendTransaction", json!([base64_encode(&tx), { "encoding": "base64", "skipSigVerify": true, "preflightCommitment": "processed" }])).await;
    let signature = match sent {
        Ok(v) => v.as_str().map(String::from).ok_or_else(|| Error::new(ErrorCode::RpcError, format!("sendTransaction answered {v}")))?,
        // The engine simulates first, and a transaction that fails there is not sent.
        Err(e) if e.code == ErrorCode::RpcError && e.data.as_ref().is_some_and(|d| !d["rpcData"]["err"].is_null()) => {
            let data = e.data.unwrap_or_default();
            return Err(Error::new(ErrorCode::TxReverted, format!("the transaction failed in simulation and was not sent: {}", data["rpcData"]["err"]))
                .next("data.logs holds the program log")
                .data(json!({ "err": data["rpcData"]["err"], "logs": data["rpcData"]["logs"] })));
        }
        Err(e) => return Err(e),
    };

    let started = Instant::now();
    let status = loop {
        let v = rpc.call("getSignatureStatuses", json!([[signature]])).await?;
        if let Some(s) = v["value"].get(0).filter(|s| !s.is_null()) {
            break s.clone();
        }
        if started.elapsed() > STATUS_TIMEOUT {
            return Err(Error::new(ErrorCode::EngineFailed, format!("the engine took transaction {signature} and reported no status within {} s", STATUS_TIMEOUT.as_secs())));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    let full = rpc.call("getTransaction", json!([signature, { "encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed" }])).await.unwrap_or(Value::Null);
    Ok(Sent { signature, slot: status["slot"].as_u64().unwrap_or(0), err: status["err"].clone(), logs: full["meta"]["logMessages"].clone() })
}

pub async fn send(adapter: &SolanaAdapter, name: &str, req: &SendRequest) -> Result<Value> {
    let (handle, profile) = adapter.open(name).await?;
    let rpc = adapter.rpc(&handle, &profile);
    if req.sig.is_some() || !req.args.is_empty() {
        return Err(Error::new(ErrorCode::Usage, "on Solana, send takes a transfer or a whole transaction; --sig and arguments are not used").next(FORMS));
    }
    let from = parse_address(&req.from)?;
    let decimals = profile.native.as_ref().map_or(9, |n| n.decimals);
    let (tx, mut out) = match (&req.data, req.to.trim(), &req.value) {
        (Some(data), "", None) => {
            let tx = base64_decode(data).ok_or_else(|| Error::new(ErrorCode::Usage, "--data is not base64").next(FORMS))?;
            (tx, json!({ "from": from }))
        }
        (Some(_), _, _) => return Err(Error::new(ErrorCode::Usage, "--data carries the whole transaction, so --to and --value are not used with it").next(FORMS)),
        (None, to, Some(value)) if !to.is_empty() => {
            let to = parse_address(to)?;
            if to == from || to == SYSTEM_PROGRAM || from == SYSTEM_PROGRAM {
                return Err(Error::new(ErrorCode::Usage, "a transfer needs two different wallet addresses"));
            }
            let lamports = parse_amount(value, decimals)?;
            let key = |a: &str| base58_decode(a).unwrap_or_default();
            (transfer_transaction(&key(&from), &key(&to), &[0u8; 32], lamports), json!({ "from": from, "to": to, "value": format_amount(lamports, decimals) }))
        }
        _ => return Err(Error::new(ErrorCode::Usage, "send needs --to and --value for a transfer, or --data for a whole transaction").next(FORMS)),
    };
    let at = layout(&tx)?;
    if !at.signers.contains(&from) {
        return Err(Error::new(ErrorCode::Usage, format!("{from} does not sign this transaction; its required signers are {}", at.signers.join(", "))).next(FORMS));
    }

    let sent = submit(&rpc, name, tx, &at).await?;
    let status = if sent.err.is_null() { "success" } else { "failed" };
    out["signature"] = json!(sent.signature);
    out["slot"] = json!(sent.slot);
    out["status"] = json!(status);
    out["err"] = sent.err.clone();
    out["logs"] = sent.logs;
    out["signers"] = json!(at.signers);
    adapter.record(name, json!({ "kind": "send-as", "from": from, "to": out["to"], "value": out["value"], "signature": out["signature"], "status": status, "signatureCheckSkipped": true })).await?;
    out["note"] = json!(chainsim_core::mods::NOTE);
    if status == "failed" {
        return Err(Error::new(ErrorCode::TxReverted, format!("the transaction failed: {}", sent.err)).next("data.logs holds the program log; the failed transaction still paid its fee").data(out));
    }
    Ok(out)
}

/// The selftest's send: 1 SOL set on `sender`, then 0.001 SOL moved to
/// `receiver` with no key. True when the receiver's balance grew by exactly that.
pub async fn selftest_transfer(adapter: &SolanaAdapter, name: &str, rpc: &Rpc<'_>, profile: &Profile, sender: &str, receiver: &str) -> bool {
    const LAMPORTS: u64 = 1_000_000;
    let run = async {
        fund::set_native(rpc, profile, sender, 1_000_000_000).await?;
        adapter.record(name, json!({ "kind": "selftest-fund", "address": sender, "asset": "native", "amount": "1" })).await?;
        let before = rpc.balance(receiver).await?;
        let key = |a: &str| base58_decode(a).unwrap_or_default();
        let tx = transfer_transaction(&key(sender), &key(receiver), &[0u8; 32], LAMPORTS);
        let at = layout(&tx)?;
        let sent = submit(rpc, name, tx, &at).await?;
        adapter.record(name, json!({ "kind": "selftest-send-as", "address": sender, "to": receiver, "amount": "0.001", "signature": sent.signature })).await?;
        Ok::<bool, Error>(sent.err.is_null() && rpc.balance(receiver).await? == before + LAMPORTS)
    };
    run.await.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROM: &str = "5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9";
    const TO: &str = "US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELGZ";
    /// The unsigned transfer of 1 SOL between those two that the engine took
    /// in the design probe, blockhash included.
    const PROBE: &str = "AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAAEDSMAbUFkAVFXZ3LDGvOzctPtbLqvBqagrVzkrqqQPBOYHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHKgAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABoaFnTjICpchSBhBaRfgv4/KFX5GT5jM/Q+LYPxyrxIBAgIAAQwCAAAAAMqaOwAAAAA=";

    #[test]
    fn base64_round_trips() {
        for (bytes, text) in [(&b""[..], ""), (b"f", "Zg=="), (b"fo", "Zm8="), (b"foo", "Zm9v"), (b"foob", "Zm9vYg=="), (&[0xfb, 0xff][..], "+/8=")] {
            assert_eq!(base64_encode(bytes), text);
            assert_eq!(base64_decode(text).unwrap(), bytes);
        }
        assert_eq!(base64_decode("Zm8").unwrap(), b"fo");
        assert_eq!(base64_decode("Zm9v!"), None);
        assert_eq!(base64_encode(&base64_decode(PROBE).unwrap()), PROBE);
    }

    #[test]
    fn shortvec_round_trips() {
        for n in [0usize, 1, 127, 128, 300, 16383, 16384, 65535] {
            let mut out = Vec::new();
            shortvec_encode(n, &mut out);
            assert_eq!(shortvec_decode(&out), Some((n, out.len())), "{n}");
        }
        assert_eq!(shortvec_decode(&[0x80, 0x80, 0x80, 0x01]), None);
    }

    #[test]
    fn the_transfer_has_the_layout_the_engine_took() {
        let probe = base64_decode(PROBE).unwrap();
        let at = layout(&probe).unwrap();
        assert_eq!(at, Layout { slots: 1, signatures: 1, signers: vec![FROM.to_string()], blockhash: 1 + 64 + 3 + 1 + 96 });
        let built = transfer_transaction(&base58_decode(FROM).unwrap(), &base58_decode(TO).unwrap(), &probe[at.blockhash..at.blockhash + 32], 1_000_000_000);
        assert_eq!(built, probe);
    }

    #[test]
    fn a_version_0_message_and_a_second_signer_are_read() {
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let mut tx = vec![2u8];
        tx.extend([0u8; 128]);
        tx.push(0x80);
        tx.extend([2, 0, 1, 3]);
        tx.extend(a);
        tx.extend(b);
        tx.extend([0u8; 32]);
        tx.extend([9u8; 32]);
        tx.extend([0, 0]);
        let at = layout(&tx).unwrap();
        assert_eq!((at.slots, at.signers.clone(), at.blockhash), (2, vec![base58_encode(&a), base58_encode(&b)], 1 + 128 + 1 + 3 + 1 + 96));
        assert_eq!(tx[at.blockhash..at.blockhash + 32], [9u8; 32]);
    }

    #[test]
    fn a_broken_transaction_is_a_usage_error() {
        let good = base64_decode(PROBE).unwrap();
        let mut mismatch = good.clone();
        mismatch[65] = 2;
        for bad in [&b""[..], &good[..80], &good[..150], &mismatch] {
            assert_eq!(layout(bad).unwrap_err().code, ErrorCode::Usage);
        }
    }
}
