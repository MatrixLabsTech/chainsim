//! Solana acceptance tests for acting on a fork: send as any account, time,
//! snapshot and revert. They start a real Surfpool fork of Solana Mainnet in
//! Docker against the public upstream, so they are ignored by default:
//!
//!     cargo test -p chainsim --test solana_act -- --ignored --test-threads=1 --nocapture

use std::process::Command;

use chainsim_solana::send::base64_encode;
use chainsim_solana::units::{base58_decode, base58_encode};
use serde_json::{json, Value};

const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
/// A wallet that holds SOL on Solana Mainnet.
const MAINNET_WALLET: &str = "5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9";
/// SPL Noop, a small upgradeable program that accepts any instruction.
const NOOP: &str = "noopb9bkMVfRPU8AsbpTUg8AQkHtKwMYZiFUjNRtMmV";
const CLOCK: &str = "SysvarC1ock11111111111111111111111111111111";
const SOL: u64 = 1_000_000_000;

struct Out {
    code: i32,
    json: Value,
}

fn chainsim(args: &[&str]) -> Out {
    let out = Command::new(env!("CARGO_BIN_EXE_chainsim")).args(args).output().expect("run chainsim");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(stdout.lines().count(), 1, "stdout of {args:?} is not one line: {stdout}");
    let json: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout of {args:?} is not JSON ({e}): {stdout}"));
    assert!(chainsim_core::envelope::is_valid(&json), "not an envelope: {json}");
    Out { code: out.status.code().unwrap_or(-1), json }
}

fn ok(args: &[&str]) -> Value {
    let out = chainsim(args);
    assert_eq!(out.code, 0, "{args:?}: {}", out.json);
    out.json["data"].clone()
}

/// The command exits with `code` and `error.code`. Returns the whole envelope: `error`, and `data` when the error carries detail.
fn fails(args: &[&str], code: i32, error: &str) -> Value {
    let out = chainsim(args);
    assert_eq!((out.code, out.json["error"]["code"].as_str()), (code, Some(error)), "{args:?}: {}", out.json);
    out.json
}

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().expect("docker");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Nothing labeled with the fork is left: containers, volumes, networks.
fn assert_gone(name: &str) {
    let label = format!("label=org.chainsim.fork={name}");
    assert_eq!(docker(&["ps", "-a", "--filter", &label, "--format", "{{.Names}}"]), "", "containers left for {name}");
    assert_eq!(docker(&["volume", "ls", "--filter", &label, "--format", "{{.Name}}"]), "", "volumes left for {name}");
    assert_eq!(docker(&["network", "ls", "--filter", &label, "--format", "{{.Name}}"]), "", "networks left for {name}");
}

/// A new address nobody holds a key for: 32 bytes that end in the time.
fn fresh_address(tag: u8) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let mut bytes = [tag; 32];
    bytes[..4].copy_from_slice(&[0xc5, 0x1a, 0x7e, 0x57]);
    bytes[16..].copy_from_slice(&nanos.to_be_bytes());
    base58_encode(&bytes)
}

fn call(name: &str, method: &str, params: Value) -> Value {
    ok(&["call", name, method, &params.to_string()])
}

fn lamports(name: &str, address: &str) -> u64 {
    call(name, "getBalance", json!([address, { "commitment": "processed" }]))["value"].as_u64().expect("a balance")
}

/// The Clock sysvar: (slot, Unix seconds).
fn clock(name: &str) -> (u64, u64) {
    let v = call(name, "getAccountInfo", json!([CLOCK, { "encoding": "jsonParsed", "commitment": "processed" }]));
    let info = &v["value"]["data"]["parsed"]["info"];
    (info["slot"].as_u64().expect("a slot"), info["unixTimestamp"].as_u64().expect("a time"))
}

/// A transaction built here, byte by byte, with no chainsim code: `payer`
/// calls `program` with the data "hi" and no accounts. Its signature slot and
/// blockhash are zero. `v0` gives a version 0 message with no lookup table.
fn program_call(payer: &str, program: &str, v0: bool) -> String {
    let mut tx = vec![1u8];
    tx.extend([0u8; 64]);
    if v0 {
        tx.push(0x80);
    }
    tx.extend([1, 0, 1, 2]);
    tx.extend(base58_decode(payer).unwrap());
    tx.extend(base58_decode(program).unwrap());
    tx.extend([0u8; 32]);
    tx.extend([1, 1, 0, 2, b'h', b'i']);
    if v0 {
        tx.push(0);
    }
    base64_encode(&tx)
}

#[test]
#[ignore]
fn solana_send_time_snapshot_revert() {
    let name = "acc-sol-act";
    chainsim(&["down", name]);

    let d = ok(&["describe", "solana-mainnet"]);
    for op in ["send", "snapshot", "revert", "time", "mine"] {
        assert!(d["operations"].as_array().unwrap().contains(&json!(op)), "describe lacks {op}");
    }
    ok(&["up", "solana-mainnet", "--name", name]);
    let engine = format!("chainsim-{name}");
    let container = docker(&["inspect", &engine, "--format", "{{.Id}}"]);
    assert_eq!(docker(&["exec", &engine, "id", "-u"]), "10001");
    assert!(!docker(&["inspect", &engine, "--format", "{{json .Args}}"]).contains("://"));

    // 1. send --as a mainnet wallet, with no key: 1 SOL arrives, and two sends have two signatures.
    let w = fresh_address(1);
    let wallet_before = lamports(name, MAINNET_WALLET);
    assert!(wallet_before > 3 * SOL, "the mainnet wallet holds {wallet_before} lamports");
    let first = ok(&["send", name, "--as", MAINNET_WALLET, "--to", &w, "--value", "1"]);
    assert_eq!((first["status"].as_str(), first["from"].as_str(), first["to"].as_str(), first["value"].as_str()), (Some("success"), Some(MAINNET_WALLET), Some(w.as_str()), Some("1")));
    assert!(first["err"].is_null() && first["slot"].as_u64().unwrap() > 0, "{first}");
    assert!(first["logs"].as_array().is_some_and(|l| l.iter().any(|x| x.as_str().unwrap().contains("11111111111111111111111111111111 success"))), "{first}");
    assert!(first["note"].as_str().is_some_and(|n| n.contains("chainsim status")));
    assert_eq!(lamports(name, &w), SOL);
    let second = ok(&["send", name, "--as", MAINNET_WALLET, "--to", &w, "--value", "1"]);
    let (sig1, sig2) = (first["signature"].as_str().unwrap(), second["signature"].as_str().unwrap());
    assert!(sig1 != sig2 && sig1.len() > 80 && sig2.len() > 80, "{sig1} {sig2}");
    assert_eq!(lamports(name, &w), 2 * SOL);
    let spent = wallet_before - lamports(name, MAINNET_WALLET);
    assert!((2 * SOL + 2..2 * SOL + 100_000).contains(&spent), "the wallet spent {spent}");
    // The engine knows each signature.
    let statuses = call(name, "getSignatureStatuses", json!([[sig1, sig2]]));
    assert!(statuses["value"].as_array().unwrap().iter().all(|s| s["err"].is_null() && s["slot"].as_u64().is_some()), "{statuses}");

    // 4b, first half. A program deployed on the fork after up: SPL Noop cloned to a new address.
    let program = fresh_address(9);
    call(name, "surfnet_cloneProgramAccount", json!([NOOP, program]));
    // A transaction built outside chainsim lands through --data, and calls that program.
    let tx = program_call(MAINNET_WALLET, &program, false);
    let sent = ok(&["send", name, "--as", MAINNET_WALLET, "--data", &tx]);
    assert_eq!((sent["status"].as_str(), sent["signers"].clone()), (Some("success"), json!([MAINNET_WALLET])));
    assert!(sent["logs"].as_array().is_some_and(|l| l.iter().any(|x| x.as_str().unwrap() == format!("Program {program} success"))), "{sent}");
    assert!(sent["signature"].as_str().unwrap() != sig1);
    // The same call as a version 0 transaction.
    let v0 = ok(&["send", name, "--as", MAINNET_WALLET, "--data", &program_call(MAINNET_WALLET, &program, true)]);
    assert!(v0["logs"].as_array().is_some_and(|l| l.iter().any(|x| x.as_str().unwrap() == format!("Program {program} success"))), "{v0}");

    // What send refuses.
    fails(&["send", name, "--as", &w, "--data", &tx], 2, "usage");
    fails(&["send", name, "--as", MAINNET_WALLET, "--data", "not base64!"], 2, "usage");
    fails(&["send", name, "--as", MAINNET_WALLET, "--data", "AAAA"], 2, "usage");
    fails(&["send", name, "--as", MAINNET_WALLET, "--data", &tx, "--to", &w], 2, "usage");
    fails(&["send", name, "--as", MAINNET_WALLET, "--to", &w], 2, "usage");
    fails(&["send", name, "--as", MAINNET_WALLET, "--value", "1"], 2, "usage");
    fails(&["send", name, "--as", MAINNET_WALLET, "--to", MAINNET_WALLET, "--value", "1"], 2, "usage");
    fails(&["send", name, "--as", MAINNET_WALLET, "--to", &w, "--value", "1", "--sig", "pause()"], 2, "usage");
    // A sender with no SOL, a sender that a program owns, and a transfer of more than the balance.
    fails(&["send", name, "--as", &fresh_address(2), "--to", &w, "--value", "1"], 1, "tx_reverted");
    fails(&["send", name, "--as", USDC, "--to", &w, "--value", "1"], 3, "unsupported");
    let too_much = fails(&["send", name, "--as", &w, "--to", MAINNET_WALLET, "--value", "5"], 1, "tx_reverted");
    assert!(too_much["data"]["logs"].as_array().is_some_and(|l| !l.is_empty()), "{too_much}");
    assert_eq!(lamports(name, &w), 2 * SOL);

    // 2. Another client's unsigned transaction is still refused: the signature check is skipped per send only.
    let unsigned = chainsim_solana::send::transfer_transaction(&base58_decode(MAINNET_WALLET).unwrap(), &base58_decode(&w).unwrap(), &[0u8; 32], SOL);
    let refused = fails(&["call", name, "sendTransaction", &json!([base64_encode(&unsigned), { "encoding": "base64" }]).to_string()], 1, "rpc_error");
    assert!(refused.to_string().to_lowercase().contains("signature"), "{refused}");
    assert_eq!(lamports(name, &w), 2 * SOL);

    // 3. time --advance moves the Clock sysvar by a day, and the processed slot shows it at once.
    let (slot_before, time_before) = clock(name);
    let moved = ok(&["time", name, "--advance", "86400"]);
    let (slot_after, time_after) = clock(name);
    assert!((time_before + 86400..time_before + 86400 + 120).contains(&time_after), "clock {time_before} -> {time_after}");
    assert!(moved["previous"].as_u64().unwrap() >= time_before && moved["timestamp"].as_u64().unwrap() >= time_before + 86400, "{moved}");
    let reported = moved["slot"].as_u64().unwrap();
    assert!(reported > slot_before + 1000 && reported > moved["previousSlot"].as_u64().unwrap(), "{moved}");
    assert!(call(name, "getSlot", json!([{ "commitment": "processed" }])).as_u64().unwrap() >= reported && slot_after + 5 >= reported);
    assert!(moved["note"].as_str().unwrap().contains("processed"));
    // A target in the past exits 3 and names revert.
    let past = fails(&["time", name, "--to", &time_before.to_string()], 3, "unsupported");
    assert!(past["error"]["next"].as_str().unwrap().contains("chainsim revert"), "{past}");
    fails(&["time", name, "--to", "1"], 3, "unsupported");
    fails(&["time", name, "--advance", "0"], 2, "usage");
    let to = time_after + 3600;
    assert!(ok(&["time", name, "--to", &to.to_string()])["timestamp"].as_u64().unwrap() >= to);
    assert!(clock(name).1 >= to);
    let mined = ok(&["mine", name, "--blocks", "25"]);
    assert!(mined["slot"].as_u64().unwrap() >= mined["previousSlot"].as_u64().unwrap() + 25, "{mined}");
    fails(&["mine", name, "--blocks", "0"], 2, "usage");

    // 4. snapshot, a transfer, revert: the balances are back and the Clock is no earlier than the snapshot's.
    let wallet_at_snapshot = lamports(name, MAINNET_WALLET);
    let (slot_at_snapshot, time_at_snapshot) = clock(name);
    let id = ok(&["snapshot", name])["id"].as_str().unwrap().to_string();
    ok(&["send", name, "--as", MAINNET_WALLET, "--to", &w, "--value", "1"]);
    ok(&["set", name, "balance", &program, "7"]);
    assert_eq!(lamports(name, &w), 3 * SOL);
    let reverted = ok(&["revert", name, &id]);
    assert_eq!(reverted["reverted"], id.as_str());
    let fresh = reverted["snapshot"].as_str().unwrap().to_string();
    assert_ne!(fresh, id);
    assert_eq!(lamports(name, &w), 2 * SOL);
    assert_eq!(lamports(name, MAINNET_WALLET), wallet_at_snapshot);
    let (slot_now, time_now) = clock(name);
    assert!(slot_now >= slot_at_snapshot && time_now >= time_at_snapshot, "clock ({slot_at_snapshot}, {time_at_snapshot}) -> ({slot_now}, {time_now})");
    // The engine no longer knows a transaction sent before the revert by its signature.
    assert!(call(name, "getSignatureStatuses", json!([[sig1]]))["value"][0].is_null());
    // The same container serves the restored state, still as the engine user.
    assert_eq!(docker(&["inspect", &engine, "--format", "{{.Id}}"]), container);
    assert_eq!(docker(&["exec", &engine, "id", "-u"]), "10001");

    // 4b, second half. The program deployed before the snapshot is still callable after the revert.
    let again = ok(&["send", name, "--as", MAINNET_WALLET, "--data", &tx]);
    assert!(again["logs"].as_array().is_some_and(|l| l.iter().any(|x| x.as_str().unwrap() == format!("Program {program} success"))), "{again}");

    // A used snapshot is gone. The fresh one restores the same state again.
    fails(&["revert", name, &id], 4, "snapshot_not_found");
    fails(&["revert", name, "nope"], 4, "snapshot_not_found");
    ok(&["send", name, "--as", MAINNET_WALLET, "--to", &w, "--value", "0.5"]);
    assert_eq!(ok(&["revert", name, &fresh])["reverted"], fresh.as_str());
    assert_eq!(lamports(name, &w), 2 * SOL);
    assert!(clock(name).1 >= time_at_snapshot);

    // selftest sends as an address with no key, and status lists what was done.
    let st = ok(&["selftest", name]);
    let send_part = st["parts"].as_array().unwrap().iter().find(|p| p["part"] == "send-as").expect("a send-as part");
    assert!(st["ok"] == true && send_part["ok"] == true, "{st}");
    let status = ok(&["status", name]);
    let kinds: Vec<&str> = status["modifications"].as_array().unwrap().iter().map(|m| m["kind"].as_str().unwrap()).collect();
    for kind in ["send-as", "time", "mine", "revert", "selftest-send-as"] {
        assert!(kinds.contains(&kind), "no {kind} in {kinds:?}");
    }
    let sends: Vec<&Value> = status["modifications"].as_array().unwrap().iter().filter(|m| m["kind"] == "send-as").collect();
    assert_eq!((sends[0]["from"].as_str(), sends[0]["signature"].as_str(), sends[0]["signatureCheckSkipped"].clone()), (Some(MAINNET_WALLET), Some(sig1), json!(true)));

    assert_eq!(ok(&["down", name])["removed"], true);
    assert_gone(name);
}
