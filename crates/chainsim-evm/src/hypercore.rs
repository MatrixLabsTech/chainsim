//! HyperEVM: a mock for the HyperCore read precompiles at 0x800 to 0x813, the
//! encoders behind `set precompile` and `set hypercore`, and the CoreWriter
//! action decoder.
//!
//! The precompile ABIs and the CoreWriter action tuples follow Hyperliquid's
//! documentation and hyper-evm-lib (MIT, https://github.com/hyperliquid-dev/hyper-evm-lib,
//! `test/utils/L1Read.sol` and `src/CoreWriterLib.sol` at 4eb7ab04). No code is
//! copied from it.
//!
//! The mock is one contract, set at each precompile address. Its storage maps
//! a query to an answer: for `k = keccak256(calldata)`, slot `k` holds the
//! answer's length plus one (zero means unseeded), and slots `k + 1 + i` hold
//! the answer's 32-byte words. An unseeded query reverts with [`UNSEEDED_REASON`].

use alloy_primitives::{keccak256, Address, U256};
use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

pub const FIRST_PRECOMPILE: u16 = 0x800;
pub const LAST_PRECOMPILE: u16 = 0x813;
pub const POSITION: u16 = 0x800;
pub const SPOT_BALANCE: u16 = 0x801;
pub const MARK_PX: u16 = 0x806;
pub const ORACLE_PX: u16 = 0x807;
pub const SPOT_PX: u16 = 0x808;
pub const L1_BLOCK_NUMBER: u16 = 0x809;
pub const PERP_ASSET_INFO: u16 = 0x80a;
pub const SPOT_INFO: u16 = 0x80b;
pub const TOKEN_INFO: u16 = 0x80c;

/// The CoreWriter system contract, and its `RawAction(address indexed user, bytes data)` event.
pub const CORE_WRITER: &str = "0x3333333333333333333333333333333333333333";
pub const RAW_ACTION_TOPIC: &str = "0x8c7f585fb295f7eb1e6aeb8fba61b23a4fe60beda405f0045073b185c74412e3";
/// `sendRawAction(bytes)`.
pub const SEND_RAW_ACTION: &str = "0x17938e13";

/// What an unseeded query reverts with.
pub const UNSEEDED_REASON: &str = "chainsim: unseeded HyperCore query, run chainsim set <name> precompile <address> <calldata> --live";

/// Bytes as 0x hex.
pub fn to_hex(b: &[u8]) -> String {
    format!("0x{}", alloy_primitives::hex::encode(b))
}

/// A precompile address as 0x plus 40 lowercase hex digits.
pub fn address(n: u16) -> String {
    format!("0x{n:040x}")
}

/// A precompile number from an address, or a usage error outside 0x800 to 0x813.
pub fn parse_precompile(s: &str) -> Result<u16> {
    let bad = || Error::new(ErrorCode::Usage, format!("{s} is not a HyperCore read precompile: use an address from 0x800 to 0x813"));
    let body = s.trim().strip_prefix("0x").ok_or_else(bad)?;
    let n = u64::from_str_radix(body, 16).map_err(|_| bad())?;
    if body.len() > 40 || !(FIRST_PRECOMPILE as u64..=LAST_PRECOMPILE as u64).contains(&n) {
        return Err(bad());
    }
    Ok(n as u16)
}

/// The ABI encoding of `Error(string)` with [`UNSEEDED_REASON`].
pub fn revert_payload() -> Vec<u8> {
    let reason = UNSEEDED_REASON.as_bytes();
    let mut out = vec![0x08, 0xc3, 0x79, 0xa0];
    out.extend(word_u(32));
    out.extend(word_u(reason.len() as u128));
    out.extend(pad_words(reason));
    out
}

/// The mock's runtime bytecode (Shanghai or later: it uses PUSH0).
pub fn mock_code() -> Vec<u8> {
    const UNSEEDED: usize = 0;
    const LOOP: usize = 1;
    const DONE: usize = 2;
    let payload = revert_payload();
    let mut code: Vec<u8> = Vec::new();
    let mut labels = [0usize; 3];
    let mut patches: Vec<(usize, usize)> = Vec::new();
    let mut push2 = |code: &mut Vec<u8>, label: usize| {
        code.push(0x61);
        patches.push((code.len(), label));
        code.extend([0, 0]);
    };
    // calldata to memory, k = keccak256(calldata), v = sload(k)
    code.extend([0x36, 0x5f, 0x5f, 0x37, 0x36, 0x5f, 0x20, 0x80, 0x54]);
    // if v == 0: unseeded
    code.extend([0x80, 0x15]);
    push2(&mut code, UNSEEDED);
    code.push(0x57);
    // len = v - 1, i = 0
    code.extend([0x60, 0x01, 0x90, 0x03, 0x5f]);
    labels[LOOP] = code.len();
    // loop: if !(i * 32 < len) goto done
    code.extend([0x5b, 0x81, 0x81, 0x60, 0x05, 0x1b, 0x10, 0x15]);
    push2(&mut code, DONE);
    code.push(0x57);
    // mstore(i * 32, sload(k + i + 1)), i += 1
    code.extend([0x80, 0x83, 0x01, 0x60, 0x01, 0x01, 0x54, 0x81, 0x60, 0x05, 0x1b, 0x52, 0x60, 0x01, 0x01]);
    push2(&mut code, LOOP);
    code.push(0x56);
    // done: return(0, len)
    labels[DONE] = code.len();
    code.extend([0x5b, 0x50, 0x5f, 0xf3]);
    // unseeded: codecopy the revert payload and revert with it
    labels[UNSEEDED] = code.len();
    let n = payload.len() as u16;
    let data_at = (code.len() + 1 + 3 + 3 + 1 + 1 + 3 + 1 + 1) as u16;
    code.push(0x5b);
    code.push(0x61);
    code.extend(n.to_be_bytes());
    code.push(0x61);
    code.extend(data_at.to_be_bytes());
    code.extend([0x5f, 0x39]);
    code.push(0x61);
    code.extend(n.to_be_bytes());
    code.extend([0x5f, 0xfd]);
    assert_eq!(code.len(), data_at as usize);
    code.extend(payload);
    for (at, label) in patches {
        code[at..at + 2].copy_from_slice(&(labels[label] as u16).to_be_bytes());
    }
    code
}

/// The mock's runtime bytecode as 0x hex, for `anvil_setCode`.
pub fn mock_code_hex() -> String {
    format!("0x{}", alloy_primitives::hex::encode(mock_code()))
}

/// The storage writes that make the mock answer `calldata` with `result`:
/// (slot, value) pairs as 0x plus 64 hex digits.
pub fn storage_writes(calldata: &[u8], result: &[u8]) -> Vec<(String, String)> {
    let k = U256::from_be_bytes(keccak256(calldata).0);
    let mut out = vec![(word_hex(k), word_hex(U256::from(result.len() + 1)))];
    for (i, chunk) in result.chunks(32).enumerate() {
        let mut w = [0u8; 32];
        w[..chunk.len()].copy_from_slice(chunk);
        out.push((word_hex(k.wrapping_add(U256::from(i + 1))), format!("0x{}", alloy_primitives::hex::encode(w))));
    }
    out
}

fn word_hex(v: U256) -> String {
    format!("0x{v:064x}")
}

pub fn word_u(v: u128) -> [u8; 32] {
    U256::from(v).to_be_bytes()
}

/// A signed integer, sign-extended to 32 bytes.
pub fn word_i(v: i64) -> [u8; 32] {
    let mut w = if v < 0 { [0xff; 32] } else { [0; 32] };
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

pub fn word_addr(a: Address) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a.as_slice());
    w
}

pub fn word_bool(b: bool) -> [u8; 32] {
    word_u(b as u128)
}

fn pad_words(bytes: &[u8]) -> Vec<u8> {
    let mut out = bytes.to_vec();
    out.resize(bytes.len().div_ceil(32) * 32, 0);
    out
}

fn concat(words: &[[u8; 32]]) -> Vec<u8> {
    words.concat()
}

/// One precompile answer to write: the precompile, the query and the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub precompile: u16,
    pub calldata: Vec<u8>,
    pub result: Vec<u8>,
}

/// `set hypercore price <perp> <px>`: the mark price (0x806) and the oracle
/// price (0x807) of a perp, in the precompile's raw units.
pub fn price(perp: u32, px: u64) -> Vec<Entry> {
    let calldata = word_u(perp as u128).to_vec();
    let result = word_u(px as u128).to_vec();
    [MARK_PX, ORACLE_PX].into_iter().map(|precompile| Entry { precompile, calldata: calldata.clone(), result: result.clone() }).collect()
}

/// `set hypercore spot-balance <user> <token> <total>`: SpotBalance(total,
/// hold, entryNtl) at 0x801, in the token's raw units on HyperCore.
pub fn spot_balance(user: Address, token: u64, total: u64, hold: u64, entry_ntl: u64) -> Entry {
    Entry {
        precompile: SPOT_BALANCE,
        calldata: concat(&[word_addr(user), word_u(token as u128)]),
        result: concat(&[word_u(total as u128), word_u(hold as u128), word_u(entry_ntl as u128)]),
    }
}

/// A perp position as the 0x800 precompile returns it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Position {
    pub szi: i64,
    pub entry_ntl: u64,
    pub isolated_raw_usd: i64,
    pub leverage: u32,
    pub is_isolated: bool,
}

/// `set hypercore position <user> <perp> <szi> <entryNtl>`: Position at 0x800.
pub fn position(user: Address, perp: u16, p: Position) -> Entry {
    Entry {
        precompile: POSITION,
        calldata: concat(&[word_addr(user), word_u(perp as u128)]),
        result: concat(&[word_i(p.szi), word_u(p.entry_ntl as u128), word_i(p.isolated_raw_usd), word_u(p.leverage as u128), word_bool(p.is_isolated)]),
    }
}

/// One query `up` seeds from the upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    pub precompile: u16,
    pub query: &'static str,
    pub index: Option<u32>,
    pub calldata: Vec<u8>,
}

/// The queries `up` seeds: the L1 block number, then mark price, oracle price
/// and asset info per perp, token info per token, spot info and spot price per
/// spot pair. Each is one upstream call.
pub fn seeds(s: &chainsim_core::profile::HyperCoreSettings) -> Vec<Seed> {
    let one = |precompile, query, index: u32| Seed { precompile, query, index: Some(index), calldata: word_u(index as u128).to_vec() };
    let mut out = vec![Seed { precompile: L1_BLOCK_NUMBER, query: "l1BlockNumber", index: None, calldata: vec![] }];
    for &p in &s.perps {
        out.extend([one(MARK_PX, "markPx", p), one(ORACLE_PX, "oraclePx", p), one(PERP_ASSET_INFO, "perpAssetInfo", p)]);
    }
    for &t in &s.tokens {
        out.push(one(TOKEN_INFO, "tokenInfo", t));
    }
    for &sp in &s.spots {
        out.extend([one(SPOT_INFO, "spotInfo", sp), one(SPOT_PX, "spotPx", sp)]);
    }
    out
}

fn word_at(data: &[u8], at: usize) -> Option<U256> {
    data.get(at..at.checked_add(32)?).map(|w| U256::from_be_slice(w))
}

fn small(v: U256) -> Option<usize> {
    (v <= U256::from(u32::MAX)).then(|| v.to::<usize>())
}

/// The first field of an ABI-encoded dynamic tuple, when it is a string
/// (the coin of PerpAssetInfo, the name of SpotInfo and TokenInfo).
pub fn tuple_name(data: &[u8]) -> Option<String> {
    let tuple = small(word_at(data, 0)?)?;
    let at = tuple.checked_add(small(word_at(data, tuple)?)?)?;
    let len = small(word_at(data, at)?)?;
    let bytes = data.get(at + 32..at + 32 + len)?;
    String::from_utf8(bytes.to_vec()).ok()
}

/// A uint64 answer (prices, the L1 block number).
pub fn uint64(data: &[u8]) -> Option<u64> {
    let v = word_at(data, 0)?;
    (data.len() == 32 && v <= U256::from(u64::MAX)).then(|| v.to::<u64>())
}

/// What `up` reports about the seeded answers: readable values per seed.
pub fn seeded_summary(seeded: &[(Seed, Vec<u8>)]) -> Value {
    let mut perps: Vec<Value> = Vec::new();
    let mut tokens: Vec<Value> = Vec::new();
    let mut spots: Vec<Value> = Vec::new();
    let mut l1 = Value::Null;
    let find = |list: &mut Vec<Value>, index: u32| -> usize {
        match list.iter().position(|v| v["index"] == json!(index)) {
            Some(i) => i,
            None => {
                list.push(json!({ "index": index }));
                list.len() - 1
            }
        }
    };
    for (seed, result) in seeded {
        let Some(index) = seed.index else {
            l1 = uint64(result).map_or(Value::Null, |v| json!(v));
            continue;
        };
        let (list, key, value) = match seed.query {
            "markPx" => (&mut perps, "markPx", uint64(result).map(|v| json!(v.to_string()))),
            "oraclePx" => (&mut perps, "oraclePx", uint64(result).map(|v| json!(v.to_string()))),
            "perpAssetInfo" => (&mut perps, "coin", tuple_name(result).map(Value::from)),
            "tokenInfo" => (&mut tokens, "name", tuple_name(result).map(Value::from)),
            "spotInfo" => (&mut spots, "name", tuple_name(result).map(Value::from)),
            "spotPx" => (&mut spots, "spotPx", uint64(result).map(|v| json!(v.to_string()))),
            _ => continue,
        };
        let i = find(list, index);
        list[i][key] = value.unwrap_or(Value::Null);
    }
    json!({
        "precompiles": format!("{} to {}", address(FIRST_PRECOMPILE), address(LAST_PRECOMPILE)),
        "seeded": seeded.len(),
        "l1BlockNumber": l1,
        "perps": perps,
        "tokens": tokens,
        "spots": spots,
    })
}

/// A field type in a CoreWriter action tuple.
#[derive(Debug, Clone, Copy)]
enum Ty {
    Uint(u16),
    Bool,
    Address,
    String,
}


/// The CoreWriter actions with a known tuple: id, name and fields.
/// An action: id, name, and its fields with their types.
type Action = (u32, &'static str, &'static [(&'static str, Ty)]);

const ACTIONS: &[Action] = &[
    (1, "limitOrder", &[("asset", Ty::Uint(32)), ("isBuy", Ty::Bool), ("limitPx", Ty::Uint(64)), ("sz", Ty::Uint(64)), ("reduceOnly", Ty::Bool), ("encodedTif", Ty::Uint(8)), ("cloid", Ty::Uint(128))]),
    (2, "vaultTransfer", &[("vault", Ty::Address), ("isDeposit", Ty::Bool), ("usd", Ty::Uint(64))]),
    (3, "tokenDelegate", &[("validator", Ty::Address), ("wei", Ty::Uint(64)), ("isUndelegate", Ty::Bool)]),
    (4, "stakingDeposit", &[("wei", Ty::Uint(64))]),
    (5, "stakingWithdraw", &[("wei", Ty::Uint(64))]),
    (6, "spotSend", &[("destination", Ty::Address), ("token", Ty::Uint(64)), ("wei", Ty::Uint(64))]),
    (7, "usdClassTransfer", &[("ntl", Ty::Uint(64)), ("toPerp", Ty::Bool)]),
    (8, "finalizeEvmContract", &[("token", Ty::Uint(64)), ("encodedVariant", Ty::Uint(8)), ("createNonce", Ty::Uint(64))]),
    (9, "addApiWallet", &[("apiWallet", Ty::Address), ("name", Ty::String)]),
    (10, "cancelOrderByOid", &[("asset", Ty::Uint(32)), ("oid", Ty::Uint(64))]),
    (11, "cancelOrderByCloid", &[("asset", Ty::Uint(32)), ("cloid", Ty::Uint(128))]),
    (12, "approveBuilderFee", &[("maxFeeRate", Ty::Uint(64)), ("builder", Ty::Address)]),
    (13, "sendAsset", &[("destination", Ty::Address), ("subAccount", Ty::Address), ("sourceDex", Ty::Uint(32)), ("destinationDex", Ty::Uint(32)), ("token", Ty::Uint(64)), ("wei", Ty::Uint(64))]),
    (15, "borrowLend", &[("encodedOperation", Ty::Uint(8)), ("token", Ty::Uint(64)), ("wei", Ty::Uint(64))]),
    (16, "setAbstraction", &[("user", Ty::Address), ("abstraction", Ty::Uint(8))]),
];

fn decode_field(body: &[u8], i: usize, ty: Ty) -> Option<Value> {
    let w = word_at(body, i * 32)?;
    let bytes = w.to_be_bytes::<32>();
    match ty {
        Ty::Uint(bits) => (bits == 256 || w >> bits as usize == U256::ZERO).then(|| json!(w.to_string())),
        Ty::Bool => match w {
            v if v == U256::ZERO => Some(json!(false)),
            v if v == U256::from(1u8) => Some(json!(true)),
            _ => None,
        },
        Ty::Address => bytes[..12].iter().all(|b| *b == 0).then(|| json!(alloy_primitives::Address::from_slice(&bytes[12..]).to_string())),
        Ty::String => {
            let at = small(w)?;
            let len = small(word_at(body, at)?)?;
            let s = body.get(at + 32..at.checked_add(32)?.checked_add(len)?)?;
            std::string::String::from_utf8(s.to_vec()).ok().map(Value::from)
        }
    }
}

/// Decodes one CoreWriter action: byte 0 the version, bytes 1 to 3 the
/// action id, the rest the ABI-encoded tuple. Integers come back as decimal
/// strings. An unknown id, another version or bytes that do not decode come
/// back as raw hex.
pub fn decode_action(data: &[u8]) -> Value {
    let raw = format!("0x{}", alloy_primitives::hex::encode(data));
    if data.len() < 4 {
        return json!({ "version": data.first(), "actionId": null, "action": null, "raw": raw });
    }
    let version = data[0];
    let id = u32::from_be_bytes([0, data[1], data[2], data[3]]);
    let body = &data[4..];
    let known = ACTIONS.iter().find(|(a, _, _)| *a == id);
    let fields = known.filter(|_| version == 1).and_then(|(_, _, fields)| {
        let mut out = serde_json::Map::new();
        for (i, (name, ty)) in fields.iter().enumerate() {
            out.insert(name.to_string(), decode_field(body, i, *ty)?);
        }
        Some(Value::Object(out))
    });
    match fields {
        Some(fields) => json!({ "version": version, "actionId": id, "action": known.map(|k| k.1), "fields": fields }),
        None => json!({ "version": version, "actionId": id, "action": known.map(|k| k.1), "raw": raw }),
    }
}

/// A CoreWriter payload: version 1, the action id in 3 bytes, then the tuple.
pub fn encode_action(id: u32, tuple: &[u8]) -> Vec<u8> {
    let mut out = vec![1u8];
    out.extend(&id.to_be_bytes()[1..]);
    out.extend(tuple);
    out
}

/// A limit order action (id 1): asset, side, raw price and size,
/// reduce-only, time in force (1 ALO, 2 GTC, 3 IOC) and client order id.
pub fn limit_order(asset: u32, is_buy: bool, limit_px: u64, sz: u64, reduce_only: bool, tif: u8, cloid: u128) -> Vec<u8> {
    encode_action(1, &concat(&[word_u(asset as u128), word_bool(is_buy), word_u(limit_px as u128), word_u(sz as u128), word_bool(reduce_only), word_u(tif as u128), word_u(cloid)]))
}

/// The calldata of `sendRawAction(bytes)` for a payload.
pub fn send_raw_action(payload: &[u8]) -> String {
    let mut out = alloy_primitives::hex::decode(SEND_RAW_ACTION).expect("selector");
    out.extend(word_u(32));
    out.extend(word_u(payload.len() as u128));
    out.extend(pad_words(payload));
    format!("0x{}", alloy_primitives::hex::encode(out))
}

/// The `data` of a RawAction log: the ABI-encoded `bytes`.
pub fn log_payload(data: &[u8]) -> Option<Vec<u8>> {
    let at = small(word_at(data, 0)?)?;
    let len = small(word_at(data, at)?)?;
    data.get(at + 32..at + 32 + len).map(<[u8]>::to_vec)
}

/// One RawAction log from `eth_getLogs`, decoded.
pub fn decode_log(log: &Value) -> Value {
    let user = log["topics"].get(1).and_then(Value::as_str).and_then(|t| alloy_primitives::hex::decode(t).ok()).filter(|b| b.len() == 32).map(|b| Address::from_slice(&b[12..]).to_string());
    let data = log["data"].as_str().and_then(|d| alloy_primitives::hex::decode(d).ok()).unwrap_or_default();
    let mut out = match log_payload(&data) {
        Some(p) => decode_action(&p),
        None => json!({ "raw": log["data"] }),
    };
    let block = log["blockNumber"].as_str().and_then(|b| u64::from_str_radix(b.trim_start_matches("0x"), 16).ok());
    let mut head = json!({ "block": block, "txHash": log["transactionHash"], "logIndex": log["logIndex"], "user": user });
    if let (Some(h), Some(o)) = (head.as_object_mut(), out.as_object_mut()) {
        h.append(o);
    }
    head
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Just enough of the EVM to run the mock: its opcodes, with storage and calldata.
    fn run(code: &[u8], calldata: &[u8], storage: &HashMap<U256, U256>) -> (bool, Vec<u8>) {
        let mut stack: Vec<U256> = Vec::new();
        let mut mem: Vec<u8> = Vec::new();
        let grow = |mem: &mut Vec<u8>, end: usize| {
            if mem.len() < end {
                mem.resize(end, 0)
            }
        };
        let mut pc = 0usize;
        for _ in 0..100_000 {
            let op = code[pc];
            pc += 1;
            macro_rules! pop {
                () => {
                    stack.pop().expect("stack underflow")
                };
            }
            match op {
                0x01 => {
                    let (a, b) = (pop!(), pop!());
                    stack.push(a.wrapping_add(b))
                }
                0x03 => {
                    let (a, b) = (pop!(), pop!());
                    stack.push(a.wrapping_sub(b))
                }
                0x10 => {
                    let (a, b) = (pop!(), pop!());
                    stack.push(U256::from((a < b) as u8))
                }
                0x15 => {
                    let a = pop!();
                    stack.push(U256::from((a == U256::ZERO) as u8))
                }
                0x1b => {
                    let (shift, v) = (pop!(), pop!());
                    stack.push(v << shift.to::<usize>())
                }
                0x20 => {
                    let (off, size) = (pop!().to::<usize>(), pop!().to::<usize>());
                    grow(&mut mem, off + size);
                    stack.push(U256::from_be_bytes(keccak256(&mem[off..off + size]).0))
                }
                0x36 => stack.push(U256::from(calldata.len())),
                0x37 => {
                    let (dest, off, size) = (pop!().to::<usize>(), pop!().to::<usize>(), pop!().to::<usize>());
                    grow(&mut mem, dest + size);
                    for i in 0..size {
                        mem[dest + i] = *calldata.get(off + i).unwrap_or(&0);
                    }
                }
                0x39 => {
                    let (dest, off, size) = (pop!().to::<usize>(), pop!().to::<usize>(), pop!().to::<usize>());
                    grow(&mut mem, dest + size);
                    for i in 0..size {
                        mem[dest + i] = *code.get(off + i).unwrap_or(&0);
                    }
                }
                0x50 => {
                    pop!();
                }
                0x52 => {
                    let (off, v) = (pop!().to::<usize>(), pop!());
                    grow(&mut mem, off + 32);
                    mem[off..off + 32].copy_from_slice(&v.to_be_bytes::<32>());
                }
                0x54 => {
                    let k = pop!();
                    stack.push(*storage.get(&k).unwrap_or(&U256::ZERO))
                }
                0x56 => {
                    pc = pop!().to::<usize>();
                    assert_eq!(code[pc], 0x5b, "jump to a non-JUMPDEST");
                }
                0x57 => {
                    let (dest, cond) = (pop!().to::<usize>(), pop!());
                    if cond != U256::ZERO {
                        pc = dest;
                        assert_eq!(code[pc], 0x5b, "jump to a non-JUMPDEST");
                    }
                }
                0x5b => {}
                0x5f => stack.push(U256::ZERO),
                0x60 | 0x61 => {
                    let n = (op - 0x5f) as usize;
                    stack.push(U256::from_be_slice(&code[pc..pc + n]));
                    pc += n;
                }
                0x80..=0x8f => {
                    let n = (op - 0x7f) as usize;
                    stack.push(stack[stack.len() - n])
                }
                0x90 => {
                    let n = stack.len();
                    stack.swap(n - 1, n - 2)
                }
                0xf3 | 0xfd => {
                    let (off, size) = (pop!().to::<usize>(), pop!().to::<usize>());
                    grow(&mut mem, off + size);
                    return (op == 0xf3, mem[off..off + size].to_vec());
                }
                other => panic!("opcode 0x{other:02x} at {} is not in the mock's set", pc - 1),
            }
        }
        panic!("the mock did not stop");
    }

    fn seed(storage: &mut HashMap<U256, U256>, calldata: &[u8], result: &[u8]) {
        for (k, v) in storage_writes(calldata, result) {
            storage.insert(k.parse().unwrap(), v.parse().unwrap());
        }
    }

    #[test]
    fn the_mock_returns_seeded_answers_and_reverts_with_the_reason_otherwise() {
        let code = mock_code();
        let mut storage = HashMap::new();
        // 0x807 oraclePx(0) as measured upstream, a 3-word struct, a 33-byte answer, an empty one.
        let oracle = word_u(0xbf5fd).to_vec();
        seed(&mut storage, &word_u(0), &oracle);
        let sb = spot_balance(Address::repeat_byte(0xc0), 0, 1_000_000, 5, 7);
        seed(&mut storage, &sb.calldata, &sb.result);
        let odd: Vec<u8> = (0..33).collect();
        seed(&mut storage, b"odd", &odd);
        seed(&mut storage, b"empty", &[]);
        assert_eq!(run(&code, &word_u(0), &storage), (true, oracle));
        assert_eq!(run(&code, &sb.calldata, &storage), (true, sb.result.clone()));
        assert_eq!(run(&code, b"odd", &storage), (true, odd));
        assert_eq!(run(&code, b"empty", &storage), (true, vec![]));
        // l1BlockNumber takes no calldata at all.
        seed(&mut storage, &[], &word_u(1_205_855_504));
        assert_eq!(run(&code, &[], &storage), (true, word_u(1_205_855_504).to_vec()));
        let (ok, out) = run(&code, &word_u(1), &storage);
        assert!(!ok);
        assert_eq!(out, revert_payload());
        assert_eq!(crate::abi::revert_reason(&format!("0x{}", alloy_primitives::hex::encode(&out))).as_deref(), Some(UNSEEDED_REASON));
    }

    #[test]
    fn the_storage_scheme_is_length_plus_one_then_words() {
        let w = storage_writes(&[], &[0xab; 40]);
        let k = U256::from_be_bytes(keccak256([]).0);
        assert_eq!(w.len(), 3);
        assert_eq!(w[0], (format!("0x{k:064x}"), format!("0x{:064x}", 41)));
        assert_eq!(w[1].0, format!("0x{:064x}", k + U256::from(1)));
        assert_eq!(w[1].1, format!("0x{}", "ab".repeat(32)));
        // The last word is padded on the right.
        assert_eq!(w[2].1, format!("0x{}{}", "ab".repeat(8), "00".repeat(24)));
        // An empty answer is one slot holding 1, so it differs from unseeded.
        assert_eq!(storage_writes(b"x", &[]), vec![(format!("0x{:064x}", U256::from_be_bytes(keccak256(b"x").0)), format!("0x{:064x}", 1))]);
    }

    #[test]
    fn helpers_encode_the_precompile_abi() {
        let p = price(0, 784_549);
        assert_eq!(p.iter().map(|e| e.precompile).collect::<Vec<_>>(), [MARK_PX, ORACLE_PX]);
        // `cast abi-encode 'f(uint32)' 0` and `cast abi-encode 'f(uint64)' 784549`
        assert_eq!(p[1].calldata, [0u8; 32]);
        assert_eq!(alloy_primitives::hex::encode(&p[1].result), format!("{:064x}", 784_549));

        let user: Address = "0x000000000000000000000000000000000000c0Fe".parse().unwrap();
        let sb = spot_balance(user, 150, 25, 1, 2);
        assert_eq!(sb.precompile, SPOT_BALANCE);
        assert_eq!(alloy_primitives::hex::encode(&sb.calldata), format!("{:064x}{:064x}", 0xc0fe, 150));
        assert_eq!(alloy_primitives::hex::encode(&sb.result), format!("{:064x}{:064x}{:064x}", 25, 1, 2));

        // A short position: szi -5 sign-extended, as `cast abi-encode` writes int64.
        let pos = position(user, 3, Position { szi: -5, entry_ntl: 900, isolated_raw_usd: 0, leverage: 10, is_isolated: false });
        assert_eq!(pos.precompile, POSITION);
        assert_eq!(alloy_primitives::hex::encode(&pos.calldata), format!("{:064x}{:064x}", 0xc0fe, 3));
        let r = alloy_primitives::hex::encode(&pos.result);
        assert_eq!(&r[..64], format!("{}fb", "f".repeat(62)));
        assert_eq!(&r[64..], format!("{:064x}{:064x}{:064x}{:064x}", 900, 0, 10, 0));
    }

    #[test]
    fn precompile_addresses_are_checked() {
        assert_eq!(parse_precompile("0x807").unwrap(), 0x807);
        assert_eq!(parse_precompile("0x0000000000000000000000000000000000000813").unwrap(), 0x813);
        assert!(parse_precompile("0x814").is_err());
        assert!(parse_precompile("0x7ff").is_err());
        assert!(parse_precompile("807").is_err());
        assert_eq!(address(0x80a), "0x000000000000000000000000000000000000080a");
    }

    #[test]
    fn seeds_cover_the_profile_lists() {
        let s = chainsim_core::profile::HyperCoreSettings { perps: vec![0, 159], tokens: vec![0, 150], spots: vec![107] };
        let q = seeds(&s);
        assert_eq!(q.len(), 1 + 3 * 2 + 2 + 2);
        assert_eq!((q[0].precompile, q[0].calldata.len()), (L1_BLOCK_NUMBER, 0));
        assert!(q.iter().any(|x| x.precompile == ORACLE_PX && x.index == Some(159) && x.calldata == word_u(159)));
        assert!(q.iter().any(|x| x.precompile == SPOT_PX && x.index == Some(107)));
    }

    #[test]
    fn seeded_answers_read_back_as_a_summary() {
        // PerpAssetInfo("BTC", 56, 5, 40, false) and SpotInfo("@107", [150, 0]) as ABI-encoded tuples.
        let mut perp = word_u(32).to_vec();
        perp.extend(concat(&[word_u(160), word_u(56), word_u(5), word_u(40), word_bool(false), word_u(3)]));
        perp.extend(pad_words(b"BTC"));
        let mut spot = word_u(32).to_vec();
        spot.extend(concat(&[word_u(96), word_u(150), word_u(0), word_u(4)]));
        spot.extend(pad_words(b"@107"));
        let s = chainsim_core::profile::HyperCoreSettings { perps: vec![0], tokens: vec![], spots: vec![107] };
        let q = seeds(&s);
        let answers = [word_u(1_205_855_504).to_vec(), word_u(784_200).to_vec(), word_u(784_549).to_vec(), perp, spot, word_u(46_000).to_vec()];
        let seeded: Vec<_> = q.into_iter().zip(answers).collect();
        let out = seeded_summary(&seeded);
        assert_eq!(out["l1BlockNumber"], 1_205_855_504u64);
        assert_eq!(out["perps"], json!([{ "index": 0, "markPx": "784200", "oraclePx": "784549", "coin": "BTC" }]));
        assert_eq!(out["spots"], json!([{ "index": 107, "name": "@107", "spotPx": "46000" }]));
        assert_eq!(out["seeded"], 6);
    }

    #[test]
    fn corewriter_actions_decode() {
        // Limit order, as Hyperliquid's docs build it: version 1, id 0x000001, abi.encode(tuple).
        let lo = limit_order(0, true, 7_000_000_000_000, 100_000, false, 2, 0);
        assert_eq!(alloy_primitives::hex::encode(&lo[..4]), "01000001");
        let d = decode_action(&lo);
        assert_eq!(d["action"], "limitOrder");
        assert_eq!(d["fields"], json!({ "asset": "0", "isBuy": true, "limitPx": "7000000000000", "sz": "100000", "reduceOnly": false, "encodedTif": "2", "cloid": "0" }));

        // Spot send (id 6), the action the research sent on the fork.
        let to: Address = "0x000000000000000000000000000000000000c5e2".parse().unwrap();
        let ss = encode_action(6, &concat(&[word_addr(to), word_u(0), word_u(1_000_000)]));
        assert_eq!(alloy_primitives::hex::encode(&ss[..4]), "01000006");
        assert_eq!(decode_action(&ss)["fields"], json!({ "destination": to.to_string(), "token": "0", "wei": "1000000" }));

        // Add API wallet (id 9) carries a string.
        let mut tuple = concat(&[word_addr(to), word_u(64), word_u(4)]);
        tuple.extend(pad_words(b"bot1"));
        assert_eq!(decode_action(&encode_action(9, &tuple))["fields"]["name"], "bot1");

        // Unknown id 17, a garbage payload and a bad bool come back raw.
        let raw = decode_action(&encode_action(17, &word_u(1)));
        assert_eq!((raw["actionId"].clone(), raw["action"].clone()), (json!(17), Value::Null));
        assert!(raw["raw"].as_str().unwrap().starts_with("0x01000011"));
        assert_eq!(decode_action(&[0xde, 0xad, 0xbe, 0xef])["raw"], "0xdeadbeef");
        let bad = encode_action(7, &concat(&[word_u(5), word_u(2)]));
        assert!(decode_action(&bad).get("fields").is_none());
        assert_eq!(decode_action(&[0x01])["actionId"], Value::Null);
    }

    #[test]
    fn raw_action_calldata_and_logs_round_trip() {
        let lo = limit_order(0, true, 1, 1, false, 3, 7);
        let call = send_raw_action(&lo);
        assert!(call.starts_with("0x17938e13"));
        // The log's data is the same ABI `bytes` as the call's argument.
        let data = format!("0x{}", &call[10..]);
        let log = json!({
            "topics": [RAW_ACTION_TOPIC, format!("0x{:064x}", 0xc5e1)],
            "data": data, "blockNumber": "0x10", "transactionHash": "0xaa", "logIndex": "0x0",
        });
        let d = decode_log(&log);
        assert_eq!(d["user"], "0x000000000000000000000000000000000000C5E1");
        assert_eq!(d["block"], 16);
        assert_eq!(d["action"], "limitOrder");
        assert_eq!(d["fields"]["cloid"], "7");
    }
}
