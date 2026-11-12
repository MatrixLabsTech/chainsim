//! Solana acceptance test for `set mirror kamino`: a Kamino Lend position of
//! Solana Mainnet copied onto a test wallet, which then deposits, borrows and
//! repays on it with its own key. It starts a real Surfpool fork of Solana
//! Mainnet in Docker against the public upstream, so it is ignored by default:
//!
//!     cargo test -p chainsim --test solana_kamino -- --ignored --test-threads=1 --nocapture

use std::io::Read;
use std::process::Command;

use chainsim_solana::mirror::{changed_ranges, farm_user_address, find_program_address, key, Key, FARMS, KLEND};
use chainsim_solana::send::{base64_decode, base64_encode};
use chainsim_solana::units::base58_encode;
use curve25519_dalek::{EdwardsPoint, Scalar};
use serde_json::{json, Value};
use sha2::{Digest, Sha256, Sha512};

/// An obligation on Solana Mainnet with one deposit and three borrows when
/// this test was written (2026-11-12). The test asserts it still has a deposit
/// and a borrow. When its owner has closed it, put another obligation here.
const SOURCE: &str = "9Z7nH6krHvtEqDDMV5bNj2enPb99qVpu3acB3KeasCxW";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const INSTRUCTIONS_SYSVAR: &str = "Sysvar1nstructions1111111111111111111111111";

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

fn call(name: &str, method: &str, params: Value) -> Value {
    ok(&["call", name, method, &params.to_string()])
}

fn k(address: &str) -> Key {
    key(address).unwrap_or_else(|| panic!("{address} is not an address"))
}

fn b58(key: &Key) -> String {
    base58_encode(key)
}

fn key_at(data: &[u8], offset: usize) -> Key {
    data[offset..offset + 32].try_into().unwrap()
}

fn u64_at(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

/// An account's bytes on the fork, read through a raw call.
fn account(name: &str, address: &Key) -> Vec<u8> {
    let v = call(name, "getAccountInfo", json!([b58(address), { "encoding": "base64", "commitment": "processed" }]));
    base64_decode(v["value"]["data"][0].as_str().unwrap_or_else(|| panic!("no account {}: {v}", b58(address)))).unwrap()
}

/// A token account's balance in base units.
fn token_balance(name: &str, token_account: &str) -> u64 {
    let v = call(name, "getTokenAccountBalance", json!([token_account, { "commitment": "processed" }]));
    v["value"]["amount"].as_str().unwrap_or_else(|| panic!("no balance: {v}")).parse().unwrap()
}

/// An Ed25519 key made here, so the engine checks a real signature.
struct Wallet {
    scalar: Scalar,
    prefix: [u8; 32],
    public: Key,
}

impl Wallet {
    fn new() -> Self {
        let mut seed = [0u8; 32];
        std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut seed)).expect("random bytes");
        let hash: [u8; 64] = Sha512::digest(seed).into();
        let mut low: [u8; 32] = hash[..32].try_into().unwrap();
        low[0] &= 248;
        low[31] &= 127;
        low[31] |= 64;
        let scalar = Scalar::from_bytes_mod_order(low);
        Self { scalar, prefix: hash[32..].try_into().unwrap(), public: EdwardsPoint::mul_base(&scalar).compress().to_bytes() }
    }

    fn address(&self) -> String {
        b58(&self.public)
    }

    fn sign(&self, message: &[u8]) -> [u8; 64] {
        let r = Scalar::from_bytes_mod_order_wide(&Sha512::new().chain_update(self.prefix).chain_update(message).finalize().into());
        let point = EdwardsPoint::mul_base(&r).compress().to_bytes();
        let h = Scalar::from_bytes_mod_order_wide(&Sha512::new().chain_update(point).chain_update(self.public).chain_update(message).finalize().into());
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&point);
        out[32..].copy_from_slice(&(r + h * self.scalar).to_bytes());
        out
    }
}

#[derive(Clone)]
struct Meta {
    key: Key,
    signer: bool,
    writable: bool,
}

fn writable(key: Key) -> Meta {
    Meta { key, signer: false, writable: true }
}

fn readonly(key: Key) -> Meta {
    Meta { key, signer: false, writable: false }
}

fn signer(key: Key) -> Meta {
    Meta { key, signer: true, writable: true }
}

struct Instruction {
    program: Key,
    accounts: Vec<Meta>,
    data: Vec<u8>,
}

fn shortvec(mut n: usize, out: &mut Vec<u8>) {
    while n >= 0x80 {
        out.push((n & 0x7f) as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// A legacy transaction with one signer, built byte by byte and signed by `payer`.
fn transaction(payer: &Wallet, instructions: &[Instruction], blockhash: &Key) -> Vec<u8> {
    let mut metas = vec![signer(payer.public)];
    for ix in instructions {
        for meta in ix.accounts.iter().cloned().chain([readonly(ix.program)]) {
            match metas.iter_mut().find(|m| m.key == meta.key) {
                Some(m) => {
                    m.signer |= meta.signer;
                    m.writable |= meta.writable;
                }
                None => metas.push(meta),
            }
        }
    }
    metas.sort_by_key(|m| (!m.signer, !m.writable));
    assert_eq!(metas.iter().filter(|m| m.signer).count(), 1, "one signer, the payer");
    let index = |key: &Key| metas.iter().position(|m| &m.key == key).unwrap() as u8;

    let mut message = vec![1, 0, metas.iter().filter(|m| !m.signer && !m.writable).count() as u8];
    shortvec(metas.len(), &mut message);
    metas.iter().for_each(|m| message.extend(m.key));
    message.extend(blockhash);
    shortvec(instructions.len(), &mut message);
    for ix in instructions {
        message.push(index(&ix.program));
        shortvec(ix.accounts.len(), &mut message);
        message.extend(ix.accounts.iter().map(|m| index(&m.key)));
        shortvec(ix.data.len(), &mut message);
        message.extend(&ix.data);
    }
    let mut tx = vec![1];
    tx.extend(payer.sign(&message));
    tx.extend(message);
    tx
}

/// Sends a signed transaction through plain `sendTransaction`, so the engine
/// checks the signature, and returns its error (null on success) and its logs.
fn send(name: &str, payer: &Wallet, instructions: &[Instruction]) -> (Value, Vec<String>) {
    let blockhash = call(name, "getLatestBlockhash", json!([{ "commitment": "processed" }]))["value"]["blockhash"].as_str().map(k).expect("a blockhash");
    let tx = transaction(payer, instructions, &blockhash);
    assert!(tx.len() <= 1232, "the transaction is {} bytes", tx.len());
    let signature = call(name, "sendTransaction", json!([base64_encode(&tx), { "encoding": "base64", "skipPreflight": true }]));
    for _ in 0..80 {
        let status = call(name, "getSignatureStatuses", json!([[signature]]))["value"][0].clone();
        if !status.is_null() {
            let full = call(name, "getTransaction", json!([signature, { "encoding": "json", "maxSupportedTransactionVersion": 0, "commitment": "confirmed" }]));
            let logs = full["meta"]["logMessages"].as_array().map(|l| l.iter().map(|x| x.as_str().unwrap_or_default().to_string()).collect()).unwrap_or_default();
            return (status["err"].clone(), logs);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    panic!("no status for {signature} after 20 s");
}

/// The 8 bytes that name an instruction of an Anchor program.
fn instruction_data(name: &str, amount: Option<u64>) -> Vec<u8> {
    let mut data = Sha256::digest(format!("global:{name}"))[..8].to_vec();
    data.extend(amount.iter().flat_map(|a| a.to_le_bytes()));
    data
}

/// The fields of a Kamino Lend reserve that its instructions name.
struct Reserve {
    address: Key,
    market: Key,
    farm_collateral: Key,
    farm_debt: Key,
    mint: Key,
    decimals: u64,
    supply_vault: Key,
    fee_vault: Key,
    token_program: Key,
    collateral_mint: Key,
    collateral_supply: Key,
    /// Pyth price, Switchboard price, Switchboard TWAP, Scope prices.
    oracles: [Key; 4],
}

fn reserve(name: &str, address: &Key) -> Reserve {
    let d = account(name, address);
    assert_eq!(d.len(), 8624, "reserve {}", b58(address));
    Reserve {
        address: *address,
        market: key_at(&d, 32),
        farm_collateral: key_at(&d, 64),
        farm_debt: key_at(&d, 96),
        mint: key_at(&d, 128),
        decimals: u64_at(&d, 272),
        supply_vault: key_at(&d, 160),
        fee_vault: key_at(&d, 192),
        token_program: key_at(&d, 408),
        collateral_mint: key_at(&d, 2560),
        collateral_supply: key_at(&d, 2600),
        oracles: [key_at(&d, 5224), key_at(&d, 5160), key_at(&d, 5192), key_at(&d, 5112)],
    }
}

/// An optional account: the program itself stands for none.
fn optional(key: &Key, write: bool) -> Meta {
    if key == &[0; 32] || key == &k("11111111111111111111111111111111") {
        readonly(k(KLEND))
    } else if write {
        writable(*key)
    } else {
        readonly(*key)
    }
}

fn refresh_reserve(r: &Reserve) -> Instruction {
    let mut accounts = vec![writable(r.address), readonly(r.market)];
    accounts.extend(r.oracles.iter().map(|o| optional(o, false)));
    Instruction { program: k(KLEND), accounts, data: instruction_data("refresh_reserve", None) }
}

/// Kamino's own valuation of an obligation, from the reserves it holds now.
fn refresh_obligation(market: &Key, obligation: &Key, reserves: &[&Reserve]) -> Instruction {
    let mut accounts = vec![readonly(*market), writable(*obligation)];
    accounts.extend(reserves.iter().map(|r| writable(r.address)));
    Instruction { program: k(KLEND), accounts, data: instruction_data("refresh_obligation", None) }
}

/// The obligation's farm user state and the farm, or none for a reserve with no farm.
fn farm_accounts(farm: &Key, obligation: &Key) -> [Meta; 2] {
    if farm == &[0; 32] {
        return [readonly(k(KLEND)), readonly(k(KLEND))];
    }
    [writable(farm_user_address(farm, obligation).unwrap()), writable(*farm)]
}

fn market_authority(market: &Key) -> Key {
    find_program_address(&[b"lma", market], &k(KLEND)).unwrap().0
}

fn deposit(owner: &Key, obligation: &Key, r: &Reserve, source: &Key, amount: u64) -> Instruction {
    let mut accounts = vec![
        signer(*owner),
        writable(*obligation),
        readonly(r.market),
        readonly(market_authority(&r.market)),
        writable(r.address),
        readonly(r.mint),
        writable(r.supply_vault),
        writable(r.collateral_mint),
        writable(r.collateral_supply),
        writable(*source),
        readonly(k(KLEND)),
        readonly(k(TOKEN_PROGRAM)),
        readonly(r.token_program),
        readonly(k(INSTRUCTIONS_SYSVAR)),
    ];
    accounts.extend(farm_accounts(&r.farm_collateral, obligation));
    accounts.push(readonly(k(FARMS)));
    Instruction { program: k(KLEND), accounts, data: instruction_data("deposit_reserve_liquidity_and_obligation_collateral_v2", Some(amount)) }
}

fn borrow(owner: &Key, obligation: &Key, r: &Reserve, destination: &Key, amount: u64) -> Instruction {
    let mut accounts = vec![
        signer(*owner),
        writable(*obligation),
        readonly(r.market),
        readonly(market_authority(&r.market)),
        writable(r.address),
        readonly(r.mint),
        writable(r.supply_vault),
        writable(r.fee_vault),
        writable(*destination),
        readonly(k(KLEND)),
        readonly(r.token_program),
        readonly(k(INSTRUCTIONS_SYSVAR)),
    ];
    accounts.extend(farm_accounts(&r.farm_debt, obligation));
    accounts.push(readonly(k(FARMS)));
    Instruction { program: k(KLEND), accounts, data: instruction_data("borrow_obligation_liquidity_v2", Some(amount)) }
}

fn repay(owner: &Key, obligation: &Key, r: &Reserve, source: &Key, amount: u64) -> Instruction {
    let mut accounts = vec![
        signer(*owner),
        writable(*obligation),
        readonly(r.market),
        writable(r.address),
        readonly(r.mint),
        writable(r.supply_vault),
        writable(*source),
        readonly(r.token_program),
        readonly(k(INSTRUCTIONS_SYSVAR)),
    ];
    accounts.extend(farm_accounts(&r.farm_debt, obligation));
    accounts.extend([readonly(market_authority(&r.market)), readonly(k(FARMS))]);
    Instruction { program: k(KLEND), accounts, data: instruction_data("repay_obligation_liquidity_v2", Some(amount)) }
}

/// Kamino's log lines that value a position, such as "Deposit: JLP amount: 1 value: 4".
fn valuation(logs: &[String]) -> Vec<&str> {
    logs.iter().filter_map(|l| l.strip_prefix("Program log: ")).filter(|l| l.starts_with("Deposit: ") || l.starts_with("Borrow: ")).collect()
}

fn kamino_logs(logs: &[String]) -> Vec<&String> {
    logs.iter().filter(|l| l.contains("Instruction:") || l.contains("Error") || l.contains("failed")).collect()
}

#[test]
#[ignore]
fn solana_mirror_kamino() {
    let name = "acc-sol-kamino";
    chainsim(&["down", name]);
    let d = ok(&["describe", "solana-mainnet"]);
    assert!(d["limits"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().contains("set mirror kamino")), "describe states no limit of the mirror");
    ok(&["up", "solana-mainnet", "--name", name]);

    let wallet = Wallet::new();
    let stranger = Wallet::new();
    ok(&["fund", name, &wallet.address(), "--native", "10"]);
    ok(&["fund", name, &stranger.address(), "--native", "10"]);

    // What the command refuses, before it writes anything.
    fails(&["set", name, "mirror", "kamino", &wallet.address(), "--to", &wallet.address()], 2, "usage");
    fails(&["set", name, "mirror", "kamino", "not an address", "--to", &wallet.address()], 2, "usage");

    // 1. The command mirrors a live obligation with a deposit and a borrow onto the new wallet.
    let m = ok(&["set", name, "mirror", "kamino", SOURCE, "--to", &wallet.address()]);
    eprintln!("mirror: {m}");
    let source = k(SOURCE);
    let copy = k(m["obligation"].as_str().unwrap());
    let market = k(m["market"].as_str().unwrap());
    let keys = |v: &Value| v.as_array().unwrap().iter().map(|x| k(x.as_str().unwrap())).collect::<Vec<Key>>();
    let (deposits, borrows) = (keys(&m["reserves"]["deposits"]), keys(&m["reserves"]["borrows"]));
    assert!(!deposits.is_empty() && !borrows.is_empty(), "{SOURCE} no longer has a deposit and a borrow: put another obligation in SOURCE");
    assert_eq!((m["source"].as_str(), m["owner"].as_str()), (Some(SOURCE), Some(wallet.address().as_str())));
    assert_eq!(copy, find_program_address(&[&[0], &[0], &wallet.public, &market, &[0; 32], &[0; 32]], &k(KLEND)).unwrap().0);
    assert_eq!(m["seeds"], json!({ "tag": 0, "id": 0, "seed1": "11111111111111111111111111111111", "seed2": "11111111111111111111111111111111" }));
    // Every byte of the copy equals the source, except the owner.
    let (source_bytes, copy_bytes) = (account(name, &source), account(name, &copy));
    assert_eq!(changed_ranges(&source_bytes, &copy_bytes), vec![[64, 96]]);
    assert_eq!(m["rewritten"], json!([[64, 96]]));
    assert_eq!(key_at(&copy_bytes, 64), wallet.public);
    let info = |address: &Key| call(name, "getAccountInfo", json!([b58(address), { "encoding": "base64", "dataSlice": { "offset": 0, "length": 0 }, "commitment": "processed" }]))["value"].clone();
    assert_eq!((info(&copy)["owner"].as_str(), info(&copy)["lamports"].clone()), (Some(KLEND), info(&source)["lamports"].clone()));
    // Each farm user state: the same bytes, except its owner and the obligation it stands for.
    let states = m["farmUserStates"].as_array().unwrap();
    assert!(!states.is_empty(), "{SOURCE} has no farm user state: put an obligation with a farmed reserve in SOURCE");
    for state in states {
        let (from, to) = (k(state["source"].as_str().unwrap()), k(state["copy"].as_str().unwrap()));
        assert_eq!(to, farm_user_address(&k(state["farm"].as_str().unwrap()), &copy).unwrap());
        let (a, b) = (account(name, &from), account(name, &to));
        assert_eq!(changed_ranges(&a, &b), vec![[48, 80], [480, 512]], "{state}");
        assert_eq!(state["rewritten"], json!([[48, 80], [480, 512]]));
        assert_eq!((key_at(&b, 48), key_at(&b, 480)), (wallet.public, copy));
        assert_eq!((info(&to)["owner"].as_str(), info(&to)["lamports"].clone()), (Some(FARMS), info(&from)["lamports"].clone()));
    }
    let status = ok(&["status", name]);
    let entry = status["modifications"].as_array().unwrap().iter().find(|e| e["kind"] == "mirror-kamino").expect("a mirror-kamino entry in status");
    assert_eq!((entry["source"].as_str(), entry["obligation"].as_str(), entry["owner"].as_str()), (Some(SOURCE), Some(b58(&copy).as_str()), Some(wallet.address().as_str())));

    let deposit_reserves: Vec<Reserve> = deposits.iter().map(|r| reserve(name, r)).collect();
    let borrow_reserves: Vec<Reserve> = borrows.iter().map(|r| reserve(name, r)).collect();
    let held: Vec<&Reserve> = deposit_reserves.iter().chain(&borrow_reserves).collect();
    // The refreshes a Kamino instruction on `reserve` needs before it: every reserve the obligation holds, that one last, then the obligation.
    let refreshed = |obligation: &Key, reserve: &Reserve| {
        let mut ixs: Vec<Instruction> = held.iter().filter(|r| r.address != reserve.address).map(|r| refresh_reserve(r)).collect();
        ixs.push(refresh_reserve(reserve));
        ixs.push(refresh_obligation(&market, obligation, &held));
        ixs
    };

    // 3. Kamino's own refresh values the copy as it values the source: one transaction refreshes both at the same prices.
    let mut ixs: Vec<Instruction> = held.iter().map(|r| refresh_reserve(r)).collect();
    ixs.push(refresh_obligation(&market, &source, &held));
    ixs.push(refresh_obligation(&market, &copy, &held));
    let (err, logs) = send(name, &wallet, &ixs);
    assert!(err.is_null(), "refresh failed: {err} {:?}", kamino_logs(&logs));
    let values = valuation(&logs);
    eprintln!("refresh of the source, then of the copy: {values:#?}");
    assert!(values.len() == 2 * held.len() && values[..held.len()] == values[held.len()..], "{values:?}");
    let (source_after, copy_after) = (account(name, &source), account(name, &copy));
    assert_eq!(changed_ranges(&source_after, &copy_after), vec![[64, 96]], "the refreshed copy differs from the refreshed source");
    assert_ne!(source_after, source_bytes, "the refresh changed nothing");
    assert!(u64_at(&copy_after, 16) > m["slot"].as_u64().unwrap(), "the copy's last update is not this refresh");

    // 2. The wallet deposits, borrows and repays on the copy with its own key.
    let d = &deposit_reserves[0];
    // A borrow of a Token program mint, from a reserve with a debt farm when the obligation has one.
    let b = borrow_reserves.iter().filter(|r| r.token_program == k(TOKEN_PROGRAM)).max_by_key(|r| r.farm_debt != [0; 32]).expect("a borrow of a Token program mint");
    let (d_unit, b_unit) = (10u64.pow(d.decimals as u32), 10u64.pow(b.decimals as u32));
    let token_account = |who: &Wallet, r: &Reserve, amount: &str| k(ok(&["set", name, "token", &who.address(), &b58(&r.mint), amount])["tokenAccount"].as_str().unwrap());
    let wallet_d = token_account(&wallet, d, "10");
    let wallet_b = token_account(&wallet, b, "10");
    let stranger_b = token_account(&stranger, b, "10");

    let deposited = u64_at(&copy_after, 128);
    let mut ixs = refreshed(&copy, d);
    ixs.push(deposit(&wallet.public, &copy, d, &wallet_d, d_unit));
    let (err, logs) = send(name, &wallet, &ixs);
    eprintln!("deposit: {err} {:#?}", kamino_logs(&logs));
    assert!(err.is_null(), "deposit failed: {err} {logs:#?}");
    assert_eq!(token_balance(name, &b58(&wallet_d)), 9 * d_unit);
    assert!(u64_at(&account(name, &copy), 128) > deposited, "the copy's first deposit did not grow");

    // A wallet that is not the owner is refused, on the copy and on the source.
    for obligation in [&copy, &source] {
        let mut ixs = refreshed(obligation, b);
        ixs.push(borrow(&stranger.public, obligation, b, &stranger_b, 2 * b_unit));
        let (err, logs) = send(name, &stranger, &ixs);
        eprintln!("borrow by a stranger: {err} {:#?}", kamino_logs(&logs));
        assert!(err["InstructionError"][0] == json!(ixs.len() - 1), "the stranger's borrow was not refused by Kamino: {err} {logs:#?}");
        assert_eq!(token_balance(name, &b58(&stranger_b)), 10 * b_unit);
    }
    // The wallet may not act on the source either.
    let mut ixs = refreshed(&source, b);
    ixs.push(borrow(&wallet.public, &source, b, &wallet_b, 2 * b_unit));
    let (err, _) = send(name, &wallet, &ixs);
    assert!(err["InstructionError"][0] == json!(ixs.len() - 1), "the wallet borrowed on the source: {err}");

    let mut ixs = refreshed(&copy, b);
    ixs.push(borrow(&wallet.public, &copy, b, &wallet_b, 2 * b_unit));
    let (err, logs) = send(name, &wallet, &ixs);
    eprintln!("borrow: {err} {:#?}", kamino_logs(&logs));
    assert!(err.is_null(), "borrow failed: {err} {logs:#?}");
    assert_eq!(token_balance(name, &b58(&wallet_b)), 12 * b_unit);

    let mut ixs = refreshed(&copy, b);
    ixs.push(repay(&wallet.public, &copy, b, &wallet_b, 5 * b_unit));
    let (err, logs) = send(name, &wallet, &ixs);
    eprintln!("repay: {err} {:#?}", kamino_logs(&logs));
    assert!(err.is_null(), "repay failed: {err} {logs:#?}");
    assert_eq!(token_balance(name, &b58(&wallet_b)), 7 * b_unit);

    // The source obligation never moved its owner, and only refreshes touched it.
    assert_eq!(key_at(&account(name, &source), 64), key_at(&source_bytes, 64));

    assert_eq!(ok(&["down", name])["removed"], true);
    assert_gone(name);
}
