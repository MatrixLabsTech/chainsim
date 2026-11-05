//! The EVM adapter: Foundry anvil (Ethereum) and Circle's arc-foundry (Arc),
//! driven from outside through Docker and the engine's JSON-RPC (design
//! sections 3 and 5). Every engine call goes through the fork's control path.

pub mod abi;
pub mod core;
mod corefork;
pub mod coresync;
pub mod hypercore;
pub mod lend;
pub mod units;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use alloy_primitives::{Address, U256};
use async_trait::async_trait;
use chainsim_core::adapter::selftest_result;
use chainsim_core::control::{shell_quote, Control, Target};
use chainsim_core::docker::{self, ContainerSpec, Docker, ForkHandle, UPSTREAM_ENV};
use chainsim_core::fork::*;
use chainsim_core::lock::EngineLock;
use chainsim_core::profile::{self, Profile, Token, Warmup};
use chainsim_core::error::{archive_next, upstream_next};
use chainsim_core::{Adapter, Error, ErrorCode, Result};
use serde_json::{json, Value};

use units::*;

pub use corefork::{parse_time, CoreOp, INFO_ENV};

/// How long `up` waits for the engine to answer.
const READY_TIMEOUT: Duration = Duration::from_secs(90);
/// The default time a single engine call may take (cold upstream reads included).
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// The UnitFlow target: a transaction confirms within 10 s.
const TX_TARGET: Duration = Duration::from_secs(10);

/// Throwaway accounts, funded and impersonated on the fork only.
const WARM_SENDER: &str = "0x000000000000000000000000000000000000dE01";
const ST_SENDER: &str = "0x000000000000000000000000000000000000c5e1";
const ST_RECEIVER: &str = "0x000000000000000000000000000000000000c5e2";
const ST_SPENDER: &str = "0x000000000000000000000000000000000000c5e3";
const ST_DUAL: &str = "0x000000000000000000000000000000000000c5e4";
const ST_DUAL_TO: &str = "0x000000000000000000000000000000000000c5e5";
const ARB_SYS: &str = "0x0000000000000000000000000000000000000064";
const ARB_GAS_INFO: &str = "0x000000000000000000000000000000000000006C";
/// What the ArbGasInfo mock returns for getL1BaseFeeEstimate()
/// (profiles/assets/src/ArbGasInfoMock.sol).
const MOCK_L1_BASE_FEE: u64 = 255074;

/// Engine methods that mine or move the chain, after which Core catches up.
const MINING_METHODS: &[&str] = &["eth_sendTransaction", "eth_sendRawTransaction", "evm_mine", "anvil_mine", "evm_revert", "anvil_revert"];

/// The `call` methods chainsim answers itself on HyperEVM.
pub const HYPERCORE_CALLS: &[&str] = &["corewriter_actions", "hypercore_state"];
/// The `set hypercore` helpers.
pub const HYPERCORE_SETS: &[&str] = &["price", "spot-balance", "position", "book", "mirror", "funding", "big-blocks", "isolated-margin"];
/// The info API request types the HyperEVM proxy answers at `/info`.
pub const HYPERCORE_INFO: &[&str] = &[
    "clearinghouseState", "spotClearinghouseState", "openOrders", "userFills", "meta", "metaAndAssetCtxs", "l2Book", "allMids",
    "borrowLendUserState", "borrowLendReserveState", "allBorrowLendReserveStates",
];
/// The custom JSON-RPC methods the HyperEVM proxy answers.
pub const HYPERCORE_RPC: &[&str] = &["eth_usingBigBlocks", "eth_bigBlockGasPrice"];
/// The `time` options on HyperEVM.
pub const HYPERCORE_TIME: &[&str] = &["advance", "to", "replay"];

pub const OPERATIONS: &[&str] = &["describe", "up", "down", "ls", "status", "fund", "snapshot", "revert", "call", "send", "set", "time", "mine", "selftest"];

/// The most blocks one `mine` produces.
const MAX_MINE: u64 = 10_000;

/// Whether `set <kind>` works on this profile. It mirrors the arms of
/// `EvmAdapter::set`, and the match makes a new kind a compile error here.
pub fn supports_set(profile: &Profile, kind: &SetKind) -> bool {
    match kind {
        SetKind::Storage { .. } | SetKind::Code | SetKind::Balance | SetKind::Token { .. } => true,
        SetKind::Precompile { .. } => profile.hypercore.is_some(),
        SetKind::Owner | SetKind::Object { .. } | SetKind::Resource { .. } => false,
    }
}

pub struct EvmAdapter {
    docker: Docker,
    lock: EngineLock,
}

/// One fork's control path and engine URL.
struct Engine<'a> {
    control: Control<'a>,
    url: Target,
    /// The Hyperliquid info API URL and its variable, on HyperEVM profiles.
    info: Option<(String, String)>,
    /// Info API answers read before a command took the HyperCore lock, by request body.
    prefetched: std::sync::Mutex<std::collections::BTreeMap<String, Value>>,
}

impl Engine<'_> {
    async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        self.control.rpc(&self.url, method, params, CALL_TIMEOUT).await
    }

    async fn rpc_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.control.rpc(&self.url, method, params, timeout).await
    }

    async fn upstream(&self, method: &str, params: Value) -> Result<Value> {
        self.control.rpc(&Target::Upstream, method, params, Duration::from_secs(30)).await
    }

    /// The latest block's number and timestamp in seconds.
    async fn latest_block(&self) -> Result<(u64, u64)> {
        let b = self.rpc("eth_getBlockByNumber", json!(["latest", false])).await?;
        Ok((parse_qty(&b["number"])?.to::<u64>(), parse_qty(&b["timestamp"])?.to::<u64>()))
    }

    async fn head(&self) -> Result<u64> {
        Ok(parse_qty(&self.rpc("eth_blockNumber", json!([])).await?)?.to::<u64>())
    }

    async fn balance(&self, who: &str) -> Result<U256> {
        parse_qty(&self.rpc("eth_getBalance", json!([who, "latest"])).await?)
    }

    async fn token_balance(&self, token: &str, who: Address) -> Result<U256> {
        let out = self.rpc("eth_call", json!([{ "to": token, "data": balance_of(who) }, "latest"])).await?;
        parse_qty(&out)
    }

    async fn set_balance(&self, who: &str, amount: U256) -> Result<()> {
        self.rpc("anvil_setBalance", json!([who, qty(amount)])).await.map(|_| ())
    }

    /// Sends a transaction as `from` by impersonation and waits for its
    /// receipt. Returns the receipt.
    async fn send_as(&self, from: &str, tx: Value) -> Result<Value> {
        let mut tx = tx;
        tx["from"] = json!(from);
        self.rpc("anvil_impersonateAccount", json!([from])).await?;
        let sent = self.rpc("eth_sendTransaction", json!([tx])).await;
        let _ = self.rpc("anvil_stopImpersonatingAccount", json!([from])).await;
        let hash = sent?;
        let start = Instant::now();
        loop {
            let receipt = self.rpc("eth_getTransactionReceipt", json!([hash])).await?;
            if !receipt.is_null() {
                return Ok(receipt);
            }
            if start.elapsed() > CALL_TIMEOUT {
                return Err(Error::new(ErrorCode::EngineFailed, format!("transaction {} was not mined within {} s", hash, CALL_TIMEOUT.as_secs())));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    async fn send_ok(&self, from: &str, tx: Value) -> Result<()> {
        let receipt = self.send_as(from, tx).await?;
        if receipt["status"] == json!("0x1") {
            Ok(())
        } else {
            Err(Error::new(ErrorCode::RpcError, format!("transaction {} reverted", receipt["transactionHash"])))
        }
    }

    /// Makes the HyperCore mock at `precompile` answer `calldata` with `result`.
    async fn write_answer(&self, precompile: u16, calldata: &[u8], result: &[u8]) -> Result<()> {
        let target = hypercore::address(precompile);
        for (slot, value) in hypercore::storage_writes(calldata, result) {
            self.rpc("anvil_setStorageAt", json!([target, slot, value])).await?;
        }
        Ok(())
    }

    /// One precompile call, on the fork or the upstream, as bytes.
    async fn precompile_call(&self, upstream_block: Option<u64>, precompile: u16, calldata: &[u8]) -> Result<Vec<u8>> {
        let call = json!({ "to": hypercore::address(precompile), "data": format!("0x{}", alloy_primitives::hex::encode(calldata)) });
        let out = match upstream_block {
            Some(b) => self.upstream("eth_call", json!([call, format!("0x{b:x}")])).await?,
            None => self.rpc("eth_call", json!([call, "latest"])).await?,
        };
        let text = out.as_str().ok_or_else(|| Error::new(ErrorCode::RpcError, format!("eth_call returned {out}")))?;
        alloy_primitives::hex::decode(text).map_err(|_| Error::new(ErrorCode::RpcError, format!("eth_call returned {text}")))
    }

    /// The RawAction logs CoreWriter emitted on the fork since `from`.
    async fn raw_actions(&self, from: u64) -> Result<Vec<Value>> {
        if from > self.head().await? {
            return Ok(vec![]);
        }
        let filter = json!({ "address": hypercore::CORE_WRITER, "topics": [hypercore::RAW_ACTION_TOPIC], "fromBlock": format!("0x{from:x}"), "toBlock": "latest" });
        let logs = self.rpc("eth_getLogs", json!([filter])).await?;
        Ok(logs.as_array().cloned().unwrap_or_default())
    }

    async fn decimals(&self, token: &str) -> Result<u8> {
        let out = self.rpc("eth_call", json!([{ "to": token, "data": DECIMALS }, "latest"])).await?;
        let d = parse_qty(&out)?;
        if d > U256::from(77u8) {
            return Err(Error::new(ErrorCode::RpcError, format!("{token} decimals() returned {d}")));
        }
        Ok(d.to::<u8>())
    }

    /// Sets an ERC-20 balance: `anvil_dealERC20`, then the runner's slot
    /// search when the deal does not read back.
    async fn deal(&self, token: &str, who: Address, amount: U256) -> Result<()> {
        let dealt = self.rpc("anvil_dealERC20", json!([who.to_string(), token, qty(amount)])).await;
        if dealt.is_ok() && self.token_balance(token, who).await? == amount {
            return Ok(());
        }
        for vyper in [false, true] {
            for slot in 0..=50u64 {
                let key = mapping_slot(who, slot, vyper);
                let before = self.rpc("eth_getStorageAt", json!([token, key, "latest"])).await?;
                self.rpc("anvil_setStorageAt", json!([token, key, format!("0x{:064x}", amount)])).await?;
                if self.token_balance(token, who).await.ok() == Some(amount) {
                    return Ok(());
                }
                let restore = format!("0x{:0>64}", before.as_str().unwrap_or("0x0").trim_start_matches("0x"));
                self.rpc("anvil_setStorageAt", json!([token, key, restore])).await?;
            }
        }
        Err(Error::new(ErrorCode::ReadbackMismatch, format!("could not set the balance of {token}: anvil_dealERC20 and the slot search both failed")))
    }
}

/// A token to fund: the profile entry, or one read from the chain by address.
struct FundToken {
    symbol: String,
    address: String,
    decimals: u8,
    native_view: bool,
}

/// The native amount to add before `send --as`, when `balance` cannot pay
/// `value` plus gas. The top-up leaves twice the gas cost, for a fee that
/// moves between the estimate and the send. None when the balance suffices.
pub fn top_up(balance: U256, value: U256, gas: U256, gas_price: U256) -> Option<U256> {
    let fee = gas.saturating_mul(gas_price);
    let need = fee.saturating_add(value);
    (balance < need).then(|| fee.saturating_mul(U256::from(2u8)).saturating_add(value) - balance)
}

/// A 32-byte word from a decimal or 0x-hex number, as 0x plus 64 hex digits.
pub fn word_hex(s: &str) -> Result<String> {
    let v: U256 = s.trim().parse().map_err(|_| Error::new(ErrorCode::Usage, format!("{s:?} is not a decimal or 0x-hex number of at most 32 bytes")))?;
    Ok(format!("0x{v:064x}"))
}

/// Hex bytes, checked and lowercased, with the 0x prefix.
pub fn hex_bytes(s: &str, what: &str) -> Result<String> {
    let t = s.trim();
    let body = t.strip_prefix("0x").unwrap_or(t);
    if body.len() % 2 != 0 || !body.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::new(ErrorCode::Usage, format!("{what} is not hex bytes")));
    }
    Ok(format!("0x{}", body.to_ascii_lowercase()))
}

/// A send that would revert: the decoded reason when the engine returned
/// revert data, else the engine's message.
fn reverted(e: Error) -> Error {
    if e.code != ErrorCode::RpcError {
        return e;
    }
    let data = e.data.as_ref().and_then(|d| d["rpcData"].as_str()).map(str::to_string);
    let from_message = || {
        let text = e.message.split_once("execution reverted").map_or(e.message.as_str(), |(_, r)| r);
        // The engine's JSON-RPC code rides at the end as " (3)".
        let text = match text.trim_end().rsplit_once(" (") {
            Some((head, code)) if code.trim_end_matches(')').parse::<i64>().is_ok() => head,
            _ if text.trim().strip_prefix('(').and_then(|c| c.strip_suffix(')')).is_some_and(|c| c.parse::<i64>().is_ok()) => "",
            _ => text,
        };
        text.trim_start_matches([':', ' ']).trim().to_string()
    };
    let reason = data.as_deref().and_then(abi::revert_reason).unwrap_or_else(from_message);
    let reason = if reason.is_empty() { "no reason given".to_string() } else { reason };
    Error::new(ErrorCode::TxReverted, format!("the transaction reverts on the fork: {reason}"))
        .next("check that --as holds the role the function needs, for example by reading the contract's role getter with chainsim call")
        .data(json!({ "reason": reason, "revertData": data }))
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// What `up` seeded on a HyperEVM fork, kept in the control sidecar's tmpfs
/// beside the modification log, so it goes with the fork at `down`.
const HYPERCORE_FILE: &str = "/tmp/chainsim-hypercore.json";

async fn save_hypercore(docker: &Docker, fork: &str, summary: &Value) -> Result<()> {
    let argv = ["sh", "-c", &format!("printf '%s' \"$1\" > {HYPERCORE_FILE}"), "sh", &summary.to_string()].map(String::from);
    let out = docker.exec(&docker::control_container(fork), &argv).await?;
    if out.exit_code != 0 {
        return Err(Error::new(ErrorCode::Internal, format!("could not record the seeded HyperCore answers: {}", out.stderr.trim())));
    }
    Ok(())
}

async fn load_hypercore(docker: &Docker, fork: &str) -> Value {
    let argv = ["sh", "-c", &format!("cat {HYPERCORE_FILE} 2>/dev/null || true")].map(String::from);
    match docker.exec(&docker::control_container(fork), &argv).await {
        Ok(out) => serde_json::from_str(&out.stdout).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

/// `up --block` on an upstream that answers every state read at the latest
/// block: refused, since the fork would pair an old header with current state.
pub fn latest_state_refusal(profile: &Profile, block: Option<u64>) -> Option<Error> {
    (block.is_some() && profile.fork.latest_state_only && !profile.upstream_is_secret()).then(|| {
        Error::new(
            ErrorCode::LatestStateOnly,
            format!("--block is refused on {}: the default upstream answers state reads at the latest block whatever block they name, so an older fork point would pair an old header with current state", profile.key),
        )
        .next(format!("export {}=<archive provider URL> to fork at an older block, or drop --block to fork at the head", profile.upstream.env))
    })
}

/// An `up --block` failure that reads like missing history: the upstream is
/// not an archive node. Other upstream failures (exit 5) keep their advice,
/// with the archive hint after it, since a pruned node may answer vaguely.
pub fn archive_hint(e: Error, profile: &Profile) -> Error {
    let lower = e.message.to_lowercase();
    let history = ["missing trie node", "pruned", "historical state", "header not found", "state not available", "state is not available", "archive", "unknown block", "state histories"];
    if history.iter().any(|w| lower.contains(w)) {
        return Error { code: ErrorCode::UpstreamUnavailable, next: Some(archive_next(&profile.upstream.env)), ..e };
    }
    if e.code.exit_code() == 5 {
        let next = match &e.next {
            Some(n) => format!("{n}. If the block is old: {}", archive_next(&profile.upstream.env)),
            None => archive_next(&profile.upstream.env),
        };
        return Error { next: Some(next), ..e };
    }
    e
}

impl EvmAdapter {
    pub async fn connect() -> Result<Self> {
        Ok(Self { docker: Docker::connect().await?, lock: EngineLock::embedded()? })
    }

    pub fn docker(&self) -> &Docker {
        &self.docker
    }

    /// Core catch-up after a command that mined. The command already took
    /// effect, so a failure here is logged and the next command catches up.
    async fn core_sync_after(&self, handle: &ForkHandle, profile: &Profile, eng: &Engine<'_>) {
        if let Err(e) = self.core_sync(handle, profile, eng).await {
            chainsim_core::log(format_args!("HyperCore catch-up deferred to the next command: {}", e.message));
        }
    }

    fn engine<'a>(&'a self, handle: &ForkHandle, profile: &Profile) -> Engine<'a> {
        let secret = profile.upstream_is_secret().then(|| profile.upstream_url());
        Engine { control: Control::new(&self.docker, handle.control_container.clone(), secret).with_env(&profile.upstream.env), url: Target::Engine(handle.rpc_url(profile)), info: corefork::info_source(profile), prefetched: Default::default() }
    }

    /// The fork, its profile, and a running engine, or an error an agent can act on.
    async fn open(&self, name: &str) -> Result<(ForkHandle, Profile)> {
        let handle = self.docker.find(name).await?.ok_or_else(|| Error::fork_not_found(name))?;
        let profile = profile::get(&handle.chain)?;
        if !handle.running || !self.docker.is_running(&handle.control_container).await {
            return Err(Error::new(ErrorCode::EngineFailed, format!("fork {name} is not running")).next(format!("chainsim down {name} && chainsim up {} --name {name}", profile.key)));
        }
        Ok((handle, profile))
    }

    /// Why the engine stopped, from its log, with every URL removed.
    async fn engine_failure(&self, handle_name: &str, control: &Control<'_>) -> Error {
        let log = self.docker.logs_tail(&docker::engine_container(handle_name), 20).await;
        let log = control.scrub(&log);
        let lower = log.to_lowercase();
        let tail: String = log.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
        if lower.contains("429") || lower.contains("rate limit") || lower.contains("too many requests") {
            Error::new(ErrorCode::RateLimited, format!("the upstream rate-limited the engine at startup: {tail}")).next(upstream_next(control.env.as_deref(), true))
        } else if lower.contains("fork") || lower.contains("error sending request") || lower.contains("failed to") || lower.contains("connect") {
            Error::new(ErrorCode::UpstreamUnavailable, format!("the engine could not fork from the upstream: {tail}")).next(upstream_next(control.env.as_deref(), false))
        } else {
            Error::new(ErrorCode::EngineFailed, format!("the engine did not start: {tail}"))
        }
    }

    async fn start(&self, profile: &Profile, req: &UpRequest) -> Result<Fork> {
        let name = &req.name;
        let upstream = profile.upstream_url();
        let secret = profile.upstream_is_secret().then(|| upstream.clone());
        let (network, created) = match &req.network {
            Some(net) => {
                if !self.docker.network_exists(net).await? {
                    return Err(Error::new(ErrorCode::Usage, format!("network {net} does not exist")).next(format!("docker network create {net}")));
                }
                (net.clone(), false)
            }
            None => {
                let net = docker::default_network(name);
                let created = self.docker.ensure_network(&net, name).await?;
                (net, created)
            }
        };
        let alias = req.alias.clone().unwrap_or_else(|| name.clone());
        let proxied = profile.hypercore.is_some();

        self.docker.run_control(&self.lock, profile.control_engine(), name, &network, &upstream).await?;
        let control = Control::new(&self.docker, docker::control_container(name), secret.clone()).with_env(&profile.upstream.env);

        // The fork point: the asked block, or the upstream head now, so the
        // label records the exact block.
        let block = match req.block {
            Some(b) => b,
            None => parse_qty(&control.rpc(&Target::Upstream, "eth_blockNumber", json!([]), Duration::from_secs(30)).await?)?.to::<u64>(),
        };

        let engine = self.lock.engine(&profile.engine)?;
        let image = self.docker.ensure_engine(&profile.engine, engine).await?;
        let mut tmpfs = vec![("/tmp".to_string(), "rw,nosuid,size=256m".to_string())];
        if let Some(home) = &engine.home {
            tmpfs.push((home.clone(), "rw,nosuid,size=512m,mode=1777".to_string()));
        }
        let labels = HashMap::from([
            (docker::LABEL_FORK.to_string(), name.clone()),
            (docker::LABEL_CHAIN.to_string(), profile.key.clone()),
            (docker::LABEL_ENGINE.to_string(), format!("{}@{}", profile.engine, engine.version)),
            (docker::LABEL_FORK_POINT.to_string(), format!("block:{block}")),
            (docker::LABEL_ROLE.to_string(), docker::ROLE_ENGINE.to_string()),
            (docker::LABEL_NETWORK.to_string(), network.clone()),
            (docker::LABEL_ALIAS.to_string(), alias.clone()),
            (docker::LABEL_NETWORK_CREATED.to_string(), created.to_string()),
        ]);
        let spec = ContainerSpec {
            name: docker::engine_container(name),
            image,
            user: engine.user.clone(),
            entrypoint: Some(vec!["/bin/sh".into(), "-c".into()]),
            cmd: vec![anvil_command(profile, block)],
            env: vec![format!("{UPSTREAM_ENV}={upstream}")],
            labels,
            network: network.clone(),
            // On HyperEVM the proxy takes the fork's alias and published port.
            aliases: vec![if proxied { engine_alias(&alias) } else { alias.clone() }],
            tmpfs,
            memory: docker::ENGINE_MEMORY,
            publish: (req.publish && !proxied).then_some(profile.rpc_port()),
            mounts: vec![],
        };
        self.docker.run(&spec).await?;

        let handle = self.docker.find(name).await?.ok_or_else(|| Error::new(ErrorCode::EngineFailed, "the engine container vanished after start"))?;
        let eng = self.engine(&handle, profile);
        let direct = Engine { control: Control::new(&self.docker, handle.control_container.clone(), secret.clone()).with_env(&profile.upstream.env), url: Target::Engine(engine_url(&handle, profile)), info: None, prefetched: Default::default() };
        let started = Instant::now();
        loop {
            if !self.docker.is_running(&handle.engine_container).await {
                return Err(self.engine_failure(name, &eng.control).await);
            }
            if direct.rpc_timeout("eth_blockNumber", json!([]), Duration::from_secs(5)).await.is_ok() {
                break;
            }
            if started.elapsed() > READY_TIMEOUT {
                let e = self.engine_failure(name, &eng.control).await;
                return Err(Error::new(e.code, format!("the engine did not answer within {} s. {}", READY_TIMEOUT.as_secs(), e.message)));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        if proxied {
            self.start_proxy(&handle, profile, req, &network, &alias, &eng).await?;
        }

        for o in &profile.overrides {
            match o.kind.as_str() {
                "code" => {
                    let code = profile::asset(&o.file)?.trim();
                    eng.rpc("anvil_setCode", json!([o.target, code])).await?;
                }
                other => return Err(Error::new(ErrorCode::Internal, format!("override kind {other} is not supported on EVM"))),
            }
        }

        let seeded = match &profile.hypercore {
            Some(settings) => Some(self.seed_hypercore(&eng, settings, block).await?),
            None => None,
        };

        // Warm-up: mine before handing the fork over. A cold fork's first
        // transaction took 63 s in UnitFlow round 6.
        let warm = Instant::now();
        eng.set_balance(WARM_SENDER, U256::from(10u64).pow(U256::from(21u64))).await?;
        for w in &profile.warmup {
            match w {
                Warmup::NativeTransfer => eng.send_ok(WARM_SENDER, json!({ "to": WARM_SENDER, "value": "0x0" })).await?,
                Warmup::Approve { token } => {
                    let t = profile.token(token).ok_or_else(|| Error::new(ErrorCode::Internal, format!("warm-up token {token} is not in the profile")))?;
                    let spender: Address = ST_SPENDER.parse().unwrap();
                    eng.send_ok(WARM_SENDER, json!({ "to": t.address, "data": approve(spender, U256::ZERO) })).await?;
                }
            }
        }
        chainsim_core::log(format_args!("{name} warmed up in {} ms", warm.elapsed().as_millis()));

        // The local HyperCore starts after the warm-up block, so it applies nothing from it.
        let seeded = match seeded {
            Some(mut summary) => {
                summary["core"] = self.core_init(&eng, profile, &summary).await?;
                save_hypercore(&self.docker, name, &summary).await?;
                Some(summary)
            }
            None => None,
        };

        let dev = self.dev_accounts(&eng).await;
        let mut fork = handle.fork(profile, dev);
        fork.hypercore = seeded;
        Ok(fork)
    }

    /// Sets the HyperCore mock at every read precompile, then seeds the
    /// profile's queries with the upstream's answers at the fork block.
    async fn seed_hypercore(&self, eng: &Engine<'_>, settings: &profile::HyperCoreSettings, block: u64) -> Result<Value> {
        let code = hypercore::mock_code_hex();
        for n in hypercore::FIRST_PRECOMPILE..=hypercore::LAST_PRECOMPILE {
            eng.rpc("anvil_setCode", json!([hypercore::address(n), code])).await?;
        }
        let started = Instant::now();
        let mut seeded = Vec::new();
        for seed in hypercore::seeds(settings) {
            let answer = eng.precompile_call(Some(block), seed.precompile, &seed.calldata).await?;
            eng.write_answer(seed.precompile, &seed.calldata, &answer).await?;
            seeded.push((seed, answer));
        }
        chainsim_core::log(format_args!("seeded {} HyperCore answers in {} ms", seeded.len(), started.elapsed().as_millis()));
        Ok(hypercore::seeded_summary(&seeded))
    }

    /// The fork object, with what `up` seeded on a HyperEVM fork.
    async fn fork_of(&self, handle: &ForkHandle, profile: &Profile, dev: Option<Vec<String>>) -> Fork {
        let mut fork = handle.fork(profile, dev);
        if profile.hypercore.is_some() {
            fork.hypercore = Some(load_hypercore(&self.docker, &handle.name).await);
        }
        fork
    }

    /// `set precompile`: store an answer in the HyperCore mock, given or
    /// copied from the upstream, and read it back through the precompile.
    async fn set_precompile(&self, name: &str, handle: &ForkHandle, profile: &Profile, address: &str, calldata: &str, result: Option<&str>) -> Result<(Value, Value)> {
        if profile.hypercore.is_none() {
            return Err(Error::unsupported(
                &format!("set precompile on {}", profile.key),
                format!("only HyperEVM mocks precompiles: chainsim set {name} code <address> <hex> puts a contract at any address"),
            ));
        }
        let n = hypercore::parse_precompile(address)?;
        let calldata = alloy_primitives::hex::decode(hex_bytes(calldata, "the calldata")?).unwrap_or_default();
        let eng = self.engine(handle, profile);
        let (answer, source) = match result {
            Some(r) => (alloy_primitives::hex::decode(hex_bytes(r, "--result")?).unwrap_or_default(), "given"),
            None => {
                let live = eng.precompile_call(Some(handle.fork_point.number), n, &calldata).await.map_err(|e| {
                    if e.code == ErrorCode::RpcError {
                        Error::new(ErrorCode::RpcError, format!("the upstream precompile refused the query: {}", e.message)).next("check that the calldata is the ABI encoding of the precompile's arguments, with no selector")
                    } else {
                        e
                    }
                })?;
                (live, "live")
            }
        };
        eng.write_answer(n, &calldata, &answer).await?;
        let read = eng.precompile_call(None, n, &calldata).await?;
        if read != answer {
            return Err(Error::new(ErrorCode::ReadbackMismatch, format!("{} answers {} bytes after setting {}", hypercore::address(n), read.len(), answer.len())));
        }
        let (addr, cd, res) = (hypercore::address(n), format!("0x{}", alloy_primitives::hex::encode(&calldata)), format!("0x{}", alloy_primitives::hex::encode(&answer)));
        Ok((
            json!({ "kind": "precompile", "address": addr, "calldata": cd, "result": res, "source": source }),
            json!({ "kind": "set-precompile", "address": addr, "calldata": cd, "result": res, "source": source }),
        ))
    }

    /// Starts the HyperEVM proxy on the fork's alias and waits until it answers.
    async fn start_proxy(&self, handle: &ForkHandle, profile: &Profile, req: &UpRequest, network: &str, alias: &str, eng: &Engine<'_>) -> Result<()> {
        let engine = self.lock.engine(PROXY_ENGINE)?;
        let image = self.docker.ensure_engine(PROXY_ENGINE, engine).await?;
        let labels = HashMap::from([
            (docker::LABEL_FORK.to_string(), handle.name.clone()),
            (docker::LABEL_CHAIN.to_string(), profile.key.clone()),
            (docker::LABEL_ENGINE.to_string(), format!("{PROXY_ENGINE}@{}", engine.version)),
            (docker::LABEL_ROLE.to_string(), docker::ROLE_PROXY.to_string()),
            (docker::LABEL_NETWORK.to_string(), network.to_string()),
        ]);
        let spec = ContainerSpec {
            name: docker::proxy_container(&handle.name),
            image,
            user: engine.user.clone(),
            entrypoint: None,
            cmd: vec![],
            env: vec![format!("CHAINSIM_PROXY_ENGINE={}", engine_url(handle, profile)), format!("CHAINSIM_PROXY_LISTEN=0.0.0.0:{}", profile.rpc_port())],
            labels,
            network: network.to_string(),
            aliases: vec![alias.to_string()],
            tmpfs: vec![("/tmp".to_string(), "rw,nosuid,size=16m".to_string())],
            memory: docker::PROXY_MEMORY,
            publish: req.publish.then_some(profile.rpc_port()),
            mounts: vec![],
        };
        self.docker.run(&spec).await?;
        let started = Instant::now();
        loop {
            if eng.rpc_timeout("eth_blockNumber", json!([]), Duration::from_secs(5)).await.is_ok() {
                return Ok(());
            }
            if !self.docker.is_running(&docker::proxy_container(&handle.name)).await || started.elapsed() > READY_TIMEOUT {
                let log = self.docker.logs_tail(&docker::proxy_container(&handle.name), 10).await;
                return Err(Error::new(ErrorCode::EngineFailed, format!("the HyperEVM proxy did not start: {}", eng.control.scrub(log.trim()))));
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    async fn dev_accounts(&self, eng: &Engine<'_>) -> Option<Vec<String>> {
        let accounts = eng.rpc("eth_accounts", json!([])).await.ok()?;
        serde_json::from_value(accounts).ok()
    }

    fn fund_token(&self, t: &Token) -> FundToken {
        FundToken { symbol: t.symbol.clone(), address: t.address.clone(), decimals: t.decimals, native_view: t.native_view }
    }

    async fn resolve_token(&self, eng: &Engine<'_>, profile: &Profile, token: &str) -> Result<FundToken> {
        if let Some(t) = profile.token(token) {
            return Ok(self.fund_token(t));
        }
        if token.starts_with("0x") {
            let addr = parse_address(token)?;
            let decimals = eng.decimals(&addr.to_string()).await.map_err(|_| Error::new(ErrorCode::UnknownToken, format!("{token} does not answer decimals(): not an ERC-20 on this fork")))?;
            return Ok(FundToken { symbol: addr.to_string(), address: addr.to_string(), decimals, native_view: false });
        }
        let known: Vec<_> = profile.tokens.iter().map(|t| t.symbol.as_str()).collect();
        Err(Error::new(ErrorCode::UnknownToken, format!("no token {token} on {}; known: {}", profile.key, known.join(", "))).next(format!("use a token address, or chainsim describe {}", profile.key)))
    }

    /// Runs the EVM checks. Never returns early: every part reports.
    async fn checks(&self, eng: &Engine<'_>, profile: &Profile) -> Vec<Check> {
        let mut parts = Vec::new();

        // block: the fork answers and returns its latest block.
        let t = Instant::now();
        let latest = eng.rpc("eth_getBlockByNumber", json!(["latest", false])).await;
        let ok = latest.as_ref().is_ok_and(|b| b.get("number").is_some());
        parts.push(Check { part: "block", ok, reason: if ok { "the fork returns its latest block" } else { "the fork did not return its latest block" }, ms: ms(t) });

        // block-time: the latest block is within 5 minutes of the host clock.
        let t = Instant::now();
        let ts = latest.ok().and_then(|b| parse_qty(&b["timestamp"]).ok()).map(|v| v.to::<u64>());
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let ok = ts.is_some_and(|ts| ts.abs_diff(now) <= 300);
        parts.push(Check {
            part: "block-time",
            ok,
            reason: if ok { "the latest block timestamp is within 5 minutes of the host clock" } else { "the latest block timestamp is more than 5 minutes from the host clock" },
            ms: ms(t),
        });

        let native_unit = U256::from(10u64).pow(U256::from(profile.native.as_ref().map_or(18, |n| n.decimals)));
        let funded = eng.set_balance(ST_SENDER, native_unit * U256::from(1000u64)).await.is_ok();

        // native-transfer: an impersonated native transfer confirms within 10 s.
        let t = Instant::now();
        let before = eng.balance(ST_RECEIVER).await.ok();
        let sent = funded && eng.send_ok(ST_SENDER, json!({ "to": ST_RECEIVER, "value": qty(native_unit) })).await.is_ok();
        let after = eng.balance(ST_RECEIVER).await.ok();
        let moved = matches!((before, after), (Some(b), Some(a)) if a == b + native_unit);
        let elapsed = t.elapsed();
        let ok = sent && moved && elapsed <= TX_TARGET;
        parts.push(Check {
            part: "native-transfer",
            ok,
            reason: match (sent && moved, elapsed <= TX_TARGET) {
                (true, true) => "a native transfer confirmed within 10 s and moved the balance",
                (true, false) => "a native transfer confirmed but took longer than 10 s",
                _ => "a native transfer failed or did not move the balance",
            },
            ms: ms(t),
        });

        // token-transfer: approve then transfer of the profile's main token.
        if let Some(token) = profile.selftest.token.as_deref().and_then(|s| profile.token(s)) {
            let t = Instant::now();
            let amount = U256::from(10u64).pow(U256::from(token.decimals));
            let sender: Address = ST_SENDER.parse().unwrap();
            let receiver: Address = ST_RECEIVER.parse().unwrap();
            let spender: Address = ST_SPENDER.parse().unwrap();
            let has = if token.native_view { true } else { eng.deal(&token.address, sender, amount * U256::from(10u64)).await.is_ok() };
            let before = eng.token_balance(&token.address, receiver).await.ok();
            let approved = has && eng.send_ok(ST_SENDER, json!({ "to": token.address, "data": approve(spender, amount) })).await.is_ok();
            let sent = approved && eng.send_ok(ST_SENDER, json!({ "to": token.address, "data": transfer(receiver, amount) })).await.is_ok();
            let after = eng.token_balance(&token.address, receiver).await.ok();
            let ok = sent && matches!((before, after), (Some(b), Some(a)) if a == b + amount);
            parts.push(Check {
                part: "token-transfer",
                ok,
                reason: if ok {
                    "an ERC-20 approve and transfer of the profile token confirmed and moved the balance"
                } else if !has {
                    "the profile token balance could not be set for the test account"
                } else if !approved {
                    "an ERC-20 approve of the profile token failed"
                } else {
                    "an ERC-20 transfer of the profile token failed or did not move the balance"
                },
                ms: ms(t),
            });
        }

        // snapshot-revert: a transfer after a snapshot is undone by revert.
        let t = Instant::now();
        let ok = async {
            let before = eng.balance(ST_RECEIVER).await?;
            let id = eng.rpc("evm_snapshot", json!([])).await?;
            eng.send_ok(ST_SENDER, json!({ "to": ST_RECEIVER, "value": qty(native_unit) })).await?;
            let moved = eng.balance(ST_RECEIVER).await? == before + native_unit;
            let reverted = eng.rpc("evm_revert", json!([id])).await? == json!(true);
            Ok::<bool, Error>(moved && reverted && eng.balance(ST_RECEIVER).await? == before)
        }
        .await
        .unwrap_or(false);
        parts.push(Check {
            part: "snapshot-revert",
            ok,
            reason: if ok { "evm_revert undid a transfer made after evm_snapshot" } else { "evm_snapshot and evm_revert did not restore the balance" },
            ms: ms(t),
        });

        // upstream-read: one cold read of an account the fork has never seen.
        let t = Instant::now();
        let cold = format!("0x{:040x}", now as u128 * 1_000_003 + 0xc01d);
        let ok = eng.balance(&cold).await.is_ok() && t.elapsed() <= TX_TARGET;
        parts.push(Check {
            part: "upstream-read",
            ok,
            reason: if ok { "a cold account read from the upstream answered within 10 s" } else { "a cold account read from the upstream failed or took longer than 10 s" },
            ms: ms(t),
        });

        for extra in &profile.selftest.extras {
            match extra.as_str() {
                "dual-usdc-view" => parts.push(self.dual_usdc_view(eng, profile).await),
                "arbgasinfo" => parts.push(self.arbgasinfo(eng).await),
                "chain-id" => parts.push(self.chain_id(eng, profile).await),
                "hypercore" => parts.push(self.hypercore_reads(eng, profile).await),
                "corewriter" => parts.push(self.corewriter(eng).await),
                // A typo in a profile must not pass as green.
                _ => parts.push(Check { part: "extras", ok: false, reason: "the profile names a selftest extra the EVM adapter does not have", ms: 0 }),
            }
        }
        parts
    }

    /// eth_chainId equals the profile's chain id.
    async fn chain_id(&self, eng: &Engine<'_>, profile: &Profile) -> Check {
        let t = Instant::now();
        let want = profile.id.rsplit(':').next().and_then(|n| n.parse::<u64>().ok());
        let got = eng.rpc("eth_chainId", json!([])).await.ok().and_then(|v| parse_qty(&v).ok()).map(|v| v.to::<u64>());
        let ok = want.is_some() && got == want;
        Check { part: "chain-id", ok, reason: if ok { "eth_chainId returns the profile's chain id" } else { "eth_chainId does not return the profile's chain id" }, ms: ms(t) }
    }

    /// HyperEVM: the L1 block number precompile answers a uint64, and the
    /// first seeded perp's oracle price decodes.
    async fn hypercore_reads(&self, eng: &Engine<'_>, profile: &Profile) -> Check {
        let t = Instant::now();
        let perp = profile.hypercore.as_ref().and_then(|h| h.perps.first().copied());
        let ok = async {
            let l1 = hypercore::uint64(&eng.precompile_call(None, hypercore::L1_BLOCK_NUMBER, &[]).await?);
            let px = match perp {
                Some(p) => hypercore::uint64(&eng.precompile_call(None, hypercore::ORACLE_PX, &hypercore::word_u(p as u128)).await?),
                None => None,
            };
            Ok::<bool, Error>(l1.is_some_and(|v| v > 0) && px.is_some_and(|v| v > 0))
        }
        .await
        .unwrap_or(false);
        Check {
            part: "hypercore",
            ok,
            reason: if ok { "the L1 block number precompile at 0x809 and a seeded oracle price at 0x807 answer non-zero uint64 values" } else { "the HyperCore mock did not answer the L1 block number or a seeded oracle price" },
            ms: ms(t),
        }
    }

    /// HyperEVM: a limit order sent through CoreWriter emits RawAction, and it decodes.
    async fn corewriter(&self, eng: &Engine<'_>) -> Check {
        let t = Instant::now();
        let ok = async {
            let action = hypercore::limit_order(0, true, 1, 1, false, 2, 0);
            let receipt = eng.send_as(ST_SENDER, json!({ "to": hypercore::CORE_WRITER, "data": hypercore::send_raw_action(&action) })).await?;
            let logs = receipt["logs"].as_array().cloned().unwrap_or_default();
            let decoded: Vec<Value> = logs.iter().filter(|l| l["topics"].get(0) == Some(&json!(hypercore::RAW_ACTION_TOPIC))).map(hypercore::decode_log).collect();
            let sender = ST_SENDER.parse::<Address>().map(|a| a.to_string()).unwrap_or_default();
            Ok::<bool, Error>(receipt["status"] == json!("0x1") && decoded.iter().any(|d| d["action"] == "limitOrder" && d["user"] == json!(sender)))
        }
        .await
        .unwrap_or(false);
        Check {
            part: "corewriter",
            ok,
            reason: if ok { "a limit order sent through CoreWriter emitted RawAction and decoded" } else { "a CoreWriter sendRawAction did not emit a RawAction that decodes" },
            ms: ms(t),
        }
    }

    /// Arbitrum chains: the ArbGasInfo mock at 0x6C answers, and anvil's own
    /// ArbSys at 0x64 still does, so the override did not clobber it.
    async fn arbgasinfo(&self, eng: &Engine<'_>) -> Check {
        let t = Instant::now();
        let call = |to: &'static str, sig: &str| eng.rpc("eth_call", json!([{ "to": to, "data": selector(sig) }, "latest"]));
        let ok = async {
            let fee = parse_qty(&call(ARB_GAS_INFO, "getL1BaseFeeEstimate()").await?)?;
            let block = parse_qty(&call(ARB_SYS, "arbBlockNumber()").await?)?;
            Ok::<bool, Error>(fee == U256::from(MOCK_L1_BASE_FEE) && block > U256::ZERO)
        }
        .await
        .unwrap_or(false);
        Check {
            part: "arbgasinfo",
            ok,
            reason: if ok {
                "ArbGasInfo.getL1BaseFeeEstimate() at 0x6C returned the mock value and ArbSys.arbBlockNumber() at 0x64 answered"
            } else {
                "ArbGasInfo at 0x6C did not return the mock value, or ArbSys at 0x64 did not answer"
            },
            ms: ms(t),
        }
    }

    /// Arc: one USDC balance, two views. Native 18 decimals and the ERC-20 at
    /// 0x3600…0000 with 6 decimals move together.
    async fn dual_usdc_view(&self, eng: &Engine<'_>, profile: &Profile) -> Check {
        let t = Instant::now();
        let ok = async {
            let usdc = profile.tokens.iter().find(|t| t.native_view).ok_or_else(|| Error::new(ErrorCode::Internal, "no native-view token"))?;
            let shift = U256::from(10u64).pow(U256::from(profile.native.as_ref().map_or(18, |n| n.decimals) - usdc.decimals));
            let dual: Address = ST_DUAL.parse().unwrap();
            let to: Address = ST_DUAL_TO.parse().unwrap();
            let seven = U256::from(7u64) * U256::from(10u64).pow(U256::from(usdc.decimals));
            eng.set_balance(ST_DUAL, seven * shift).await?;
            let view_ok = eng.token_balance(&usdc.address, dual).await? == seven;
            let one = U256::from(10u64).pow(U256::from(usdc.decimals));
            let native_before = eng.balance(ST_DUAL_TO).await?;
            eng.send_ok(ST_DUAL, json!({ "to": usdc.address, "data": transfer(to, one) })).await?;
            let native_ok = eng.balance(ST_DUAL_TO).await? == native_before + one * shift;
            Ok::<bool, Error>(view_ok && native_ok)
        }
        .await
        .unwrap_or(false);
        Check {
            part: "dual-usdc-view",
            ok,
            reason: if ok {
                "setting the native balance set the USDC ERC-20 view, and an ERC-20 transfer moved native balance"
            } else {
                "the native balance and the USDC ERC-20 view did not move together"
            },
            ms: ms(t),
        }
    }
}

/// The engine's `sh -c` command line. The upstream URL stays in the
/// environment, so it is never in the container's arguments.
/// The engine's alias behind a HyperEVM proxy.
pub fn engine_alias(alias: &str) -> String {
    format!("{alias}-engine")
}

/// The engine's own in-network URL: behind the proxy on HyperEVM, the fork's RPC URL elsewhere.
pub fn engine_url(handle: &ForkHandle, profile: &Profile) -> String {
    if profile.hypercore.is_some() {
        format!("http://{}:{}", engine_alias(&handle.alias), profile.rpc_port())
    } else {
        handle.rpc_url(profile)
    }
}

pub use chainsim_core::engines::PROXY_ENGINE;

pub fn anvil_command(profile: &Profile, block: u64) -> String {
    let mut script = vec![
        "exec anvil --host 0.0.0.0".to_string(),
        format!("--port {}", profile.rpc_port()),
        format!("--fork-url \"${UPSTREAM_ENV}\""),
        format!("--fork-block-number {block}"),
    ];
    script.extend(profile.fork.flags.iter().map(|f| shell_quote(f)));
    script.join(" ")
}

fn ms(t: Instant) -> u64 {
    t.elapsed().as_millis() as u64
}

#[async_trait]
impl Adapter for EvmAdapter {
    fn family(&self) -> &'static str {
        "evm"
    }

    fn operations(&self) -> &'static [&'static str] {
        OPERATIONS
    }

    async fn up(&self, profile: &Profile, req: &UpRequest) -> Result<Fork> {
        docker::check_name(&req.name)?;
        if let Some(e) = latest_state_refusal(profile, req.block) {
            return Err(e);
        }
        if let Some(h) = self.docker.find(&req.name).await? {
            let taken = |why: String| {
                Error::new(ErrorCode::NameConflict, format!("fork {} already runs {why}", req.name)).next(format!("chainsim down {} first, or pick another --name", req.name))
            };
            if h.chain != profile.key {
                return Err(taken(format!("chain {}", h.chain)));
            }
            if let Some(b) = req.block.filter(|b| *b != h.fork_point.number) {
                return Err(taken(format!("at block {} (asked {b})", h.fork_point.number)));
            }
            if req.network.as_ref().is_some_and(|n| *n != h.network) || req.alias.as_ref().is_some_and(|a| *a != h.alias) {
                return Err(taken(format!("on network {} as {}", h.network, h.alias)));
            }
            if req.publish && h.host_port.is_none() {
                return Err(taken("without --publish".into()));
            }
            if h.running && self.docker.is_running(&h.control_container).await {
                let eng = self.engine(&h, profile);
                let dev = self.dev_accounts(&eng).await;
                return Ok(self.fork_of(&h, profile, dev).await);
            }
            // A stopped fork of the same name and chain: start it fresh.
            self.docker.remove_fork(&req.name).await?;
        }
        match self.start(profile, req).await {
            Ok(fork) => Ok(fork),
            Err(e) => {
                let _ = self.docker.remove_fork(&req.name).await;
                Err(if req.block.is_some() { archive_hint(e, profile) } else { e })
            }
        }
    }

    async fn down(&self, name: &str) -> Result<Down> {
        docker::check_name(name)?;
        let removed = self.docker.remove_fork(name).await?;
        Ok(Down { name: name.to_string(), removed })
    }

    async fn status(&self, name: &str) -> Result<Status> {
        let (handle, profile) = self.open(name).await?;
        let eng = self.engine(&handle, &profile);
        self.core_sync(&handle, &profile, &eng).await?;
        let head = eng.head().await?;
        let dev = self.dev_accounts(&eng).await;
        let mut fork = self.fork_of(&handle, &profile, dev).await;
        if let Some(h) = fork.hypercore.as_mut().filter(|h| h.is_object()) {
            h["coreWriterActions"] = json!(eng.raw_actions(handle.fork_point.number + 1).await?.len());
            h["coreState"] = self.core_status(&eng).await?;
        }
        Ok(Status { fork, head, uptime_secs: handle.uptime_secs })
    }

    async fn send(&self, name: &str, req: &SendRequest) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        let eng = self.engine(&handle, &profile);
        let from = parse_address(&req.from)?.to_string();
        let to = parse_address(&req.to)?.to_string();
        let data = match (&req.sig, &req.data) {
            (Some(sig), None) => abi::encode_call(sig, &req.args)?,
            (None, Some(d)) if req.args.is_empty() => hex_bytes(d, "--data")?,
            (None, None) if req.args.is_empty() => "0x".to_string(),
            _ => return Err(Error::new(ErrorCode::Usage, "pass --sig with its arguments, or --data alone").next("chainsim send <name> --as <address> --to <address> --sig \"pause()\"")),
        };
        let decimals = profile.native.as_ref().map_or(18, |n| n.decimals);
        let symbol = profile.native.as_ref().map_or("native".to_string(), |n| n.symbol.clone());
        let value = match &req.value {
            Some(v) => parse_amount(v, decimals)?,
            None => U256::ZERO,
        };
        let tx = json!({ "from": from, "to": to, "data": data, "value": qty(value) });

        // A dry run first: a revert comes back with its reason and sends nothing.
        eng.rpc("eth_call", json!([tx, "latest"])).await.map_err(reverted)?;
        let gas = parse_qty(&eng.rpc("eth_estimateGas", json!([tx])).await.map_err(reverted)?)?;
        let price = parse_qty(&eng.rpc("eth_gasPrice", json!([])).await?)?;
        let balance = eng.balance(&from).await?;
        let added = top_up(balance, value, gas, price);
        if let Some(add) = added {
            eng.set_balance(&from, balance + add).await?;
        }
        let receipt = eng.send_as(&from, tx).await?;
        self.core_sync_after(&handle, &profile, &eng).await;
        let ok = receipt["status"] == json!("0x1");
        let hash = receipt["transactionHash"].clone();
        let top = added.map(|a| json!({ "amount": format_amount(a, decimals), "symbol": symbol }));
        let selector = data.get(..10).unwrap_or(&data).to_string();
        let entry = json!({
            "kind": "send-as", "from": from, "to": to, "sig": req.sig, "selector": selector,
            "value": format_amount(value, decimals), "txHash": hash, "status": if ok { 1 } else { 0 }, "topUp": top, "at": now_secs(),
        });
        chainsim_core::mods::record(&self.docker, name, &entry).await?;
        let gas_used = parse_qty(&receipt["gasUsed"]).map(|g| g.to_string()).unwrap_or_default();
        let block = parse_qty(&receipt["blockNumber"]).map(|b| b.to::<u64>()).unwrap_or(0);
        let out = json!({
            "txHash": hash, "status": if ok { 1 } else { 0 }, "gasUsed": gas_used, "blockNumber": block,
            "from": from, "to": to, "topUp": top, "note": chainsim_core::mods::NOTE,
        });
        if !ok {
            return Err(Error::new(ErrorCode::TxReverted, "the transaction was mined with status 0").next("chainsim call <name> eth_call '[{...}, \"latest\"]' shows the reason before sending").data(out));
        }
        Ok(out)
    }

    async fn set(&self, name: &str, req: &SetRequest) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        let eng = self.engine(&handle, &profile);
        self.core_sync(&handle, &profile, &eng).await?;
        if let SetKind::Precompile { calldata, result } = &req.kind {
            let (mut out, mut entry) = self.set_precompile(name, &handle, &profile, &req.address, calldata, result.as_deref()).await?;
            entry["at"] = json!(now_secs());
            chainsim_core::mods::record(&self.docker, name, &entry).await?;
            out["note"] = json!(chainsim_core::mods::NOTE);
            return Ok(out);
        }
        // Kinds EVM lacks answer before the address is parsed, so a Sui or
        // Aptos id gets unsupported (exit 3), not a usage error.
        match &req.kind {
            SetKind::Object { .. } => {
                return Err(Error::unsupported("set object on EVM", "EVM has no objects: chainsim set <name> storage <contract> <slot> <value> changes a contract's state"));
            }
            SetKind::Resource { .. } => {
                return Err(Error::unsupported("set resource on EVM", "EVM has no Move resources: chainsim set <name> storage <contract> <slot> <value> changes a contract's state"));
            }
            SetKind::Owner => {
                return Err(Error::unsupported("set owner on EVM", "EVM has no object owners: chainsim set <name> storage <contract> <slot> <value> changes the slot that records an owner"));
            }
            SetKind::Token { from: Some(_), .. } => {
                return Err(Error::unsupported(
                    "set token --from on EVM",
                    "set token sets the balance directly; chainsim send <name> --as <holder> --to <token> --sig \"transfer(address,uint256)\" <address> <raw amount> moves tokens from a holder",
                ));
            }
            _ => {}
        }
        let address = parse_address(&req.address)?.to_string();
        let (mut out, entry) = match &req.kind {
            SetKind::Precompile { .. } | SetKind::Object { .. } | SetKind::Resource { .. } | SetKind::Owner | SetKind::Token { from: Some(_), .. } => unreachable!("handled above"),
            SetKind::Token { coin_type, from: None } => {
                let who = parse_address(&address)?;
                let ft = self.resolve_token(&eng, &profile, coin_type).await?;
                let amount = parse_amount(&req.value, ft.decimals)?;
                if ft.native_view {
                    // Arc USDC: the token is a view of the native balance.
                    let native_decimals = profile.native.as_ref().map_or(18, |n| n.decimals);
                    eng.set_balance(&address, amount * U256::from(10u64).pow(U256::from(native_decimals - ft.decimals))).await?;
                } else {
                    eng.deal(&ft.address, who, amount).await?;
                }
                let read = eng.token_balance(&ft.address, who).await?;
                if read != amount {
                    return Err(Error::new(ErrorCode::ReadbackMismatch, format!("{address} reads {} {} after setting {}", format_amount(read, ft.decimals), ft.symbol, format_amount(amount, ft.decimals))));
                }
                let amount_s = format_amount(read, ft.decimals);
                (
                    json!({ "kind": "token", "address": address, "symbol": ft.symbol, "token": ft.address, "decimals": ft.decimals, "amount": amount_s, "raw": read.to_string() }),
                    json!({ "kind": "set-token", "address": address, "symbol": ft.symbol, "token": ft.address, "amount": amount_s }),
                )
            }
            SetKind::Storage { slot } => {
                let (slot, value) = (word_hex(slot)?, word_hex(&req.value)?);
                eng.rpc("anvil_setStorageAt", json!([address, slot, value])).await?;
                let read = parse_qty(&eng.rpc("eth_getStorageAt", json!([address, slot, "latest"])).await?)?;
                let read = format!("0x{read:064x}");
                if read != value {
                    return Err(Error::new(ErrorCode::ReadbackMismatch, format!("slot {slot} reads {read} after setting {value}")));
                }
                (json!({ "kind": "storage", "address": address, "slot": slot, "value": read }), json!({ "kind": "set-storage", "address": address, "slot": slot, "value": read }))
            }
            SetKind::Code => {
                let code = hex_bytes(&req.value, "the code")?;
                eng.rpc("anvil_setCode", json!([address, code])).await?;
                let read = eng.rpc("eth_getCode", json!([address, "latest"])).await?.as_str().unwrap_or_default().to_ascii_lowercase();
                if read != code {
                    return Err(Error::new(ErrorCode::ReadbackMismatch, format!("the code at {address} reads {} bytes after setting {}", read.len().saturating_sub(2) / 2, code.len().saturating_sub(2) / 2)));
                }
                let bytes = alloy_primitives::hex::decode(&code).unwrap_or_default();
                let hash = format!("{}", alloy_primitives::keccak256(&bytes));
                (json!({ "kind": "code", "address": address, "bytes": bytes.len(), "codeHash": hash }), json!({ "kind": "set-code", "address": address, "bytes": bytes.len(), "codeHash": hash }))
            }
            SetKind::Balance => {
                let decimals = profile.native.as_ref().map_or(18, |n| n.decimals);
                let symbol = profile.native.as_ref().map_or("native".to_string(), |n| n.symbol.clone());
                let amount = parse_amount(&req.value, decimals)?;
                eng.set_balance(&address, amount).await?;
                let read = eng.balance(&address).await?;
                if read != amount {
                    return Err(Error::new(ErrorCode::ReadbackMismatch, format!("{address} reads {} {symbol} after setting {}", format_amount(read, decimals), format_amount(amount, decimals))));
                }
                // Chains whose native balance is also an ERC-20 (Arc USDC) show both views.
                let who = parse_address(&address)?;
                let mut views = Vec::new();
                for t in profile.tokens.iter().filter(|t| t.native_view) {
                    let v = eng.token_balance(&t.address, who).await?;
                    views.push(json!({ "symbol": t.symbol, "address": t.address, "amount": format_amount(v, t.decimals), "raw": v.to_string() }));
                }
                let amount_s = format_amount(read, decimals);
                (
                    json!({ "kind": "balance", "address": address, "symbol": symbol, "amount": amount_s, "raw": read.to_string(), "views": views }),
                    json!({ "kind": "set-balance", "address": address, "symbol": symbol, "amount": amount_s }),
                )
            }
        };
        let mut entry = entry;
        entry["at"] = json!(now_secs());
        chainsim_core::mods::record(&self.docker, name, &entry).await?;
        out["note"] = json!(chainsim_core::mods::NOTE);
        Ok(out)
    }

    async fn fund(&self, name: &str, req: &FundRequest) -> Result<Funded> {
        let (handle, profile) = self.open(name).await?;
        let eng = self.engine(&handle, &profile);
        self.core_sync(&handle, &profile, &eng).await?;
        let who = parse_address(&req.address)?;
        let addr = who.to_string();
        if req.native.is_none() && req.tokens.is_empty() {
            return Err(Error::new(ErrorCode::Usage, "nothing to fund: pass --native <amount> or --token <symbol|address>:<amount>"));
        }
        let native_decimals = profile.native.as_ref().map_or(18, |n| n.decimals);
        let native_symbol = profile.native.as_ref().map_or("native".to_string(), |n| n.symbol.clone());

        let mut tokens = Vec::new();
        for t in &req.tokens {
            let ft = self.resolve_token(&eng, &profile, &t.token).await?;
            let amount = parse_amount(&t.amount, ft.decimals)?;
            tokens.push((ft, amount));
        }
        let mut native = match &req.native {
            Some(a) => Some(parse_amount(a, native_decimals)?),
            None => None,
        };
        for (ft, amount) in &tokens {
            if ft.native_view {
                if req.native.is_some() {
                    return Err(Error::new(ErrorCode::Usage, format!("{} is the native balance on {}: pass --native or --token {}, not both", ft.symbol, profile.key, ft.symbol)));
                }
                native = Some(*amount * U256::from(10u64).pow(U256::from(native_decimals - ft.decimals)));
            }
        }

        // Safety: refuse an address in use on mainnet, checked once on the upstream.
        let balance = parse_qty(&eng.upstream("eth_getBalance", json!([addr, "latest"])).await?)?;
        let nonce = parse_qty(&eng.upstream("eth_getTransactionCount", json!([addr, "latest"])).await?)?;
        if balance > U256::ZERO || nonce > U256::ZERO {
            return Err(Error::new(ErrorCode::MainnetAddress, format!("{addr} is in use on {} (balance or nonce above zero): fund refuses it, because a transaction signed for the fork is also valid on the real chain", profile.name))
                .next("fund a fresh test wallet that holds no mainnet assets"));
        }

        if let Some(n) = native {
            eng.set_balance(&addr, n).await?;
        }
        for (ft, amount) in &tokens {
            if !ft.native_view {
                eng.deal(&ft.address, who, *amount).await?;
            }
        }

        // Read every balance back.
        let mut balances = Vec::new();
        if let Some(n) = native {
            let got = eng.balance(&addr).await?;
            if got != n {
                return Err(Error::new(ErrorCode::ReadbackMismatch, format!("native balance reads {} after setting {}", format_amount(got, native_decimals), format_amount(n, native_decimals))));
            }
            balances.push(Balance { asset: native_symbol.clone(), address: None, decimals: native_decimals, amount: format_amount(got, native_decimals), raw: got.to_string() });
        }
        for (ft, amount) in &tokens {
            let got = eng.token_balance(&ft.address, who).await?;
            if got != *amount {
                return Err(Error::new(ErrorCode::ReadbackMismatch, format!("{} balance reads {} after setting {}", ft.symbol, format_amount(got, ft.decimals), format_amount(*amount, ft.decimals))));
            }
            balances.push(Balance { asset: ft.symbol.clone(), address: Some(ft.address.clone()), decimals: ft.decimals, amount: format_amount(got, ft.decimals), raw: got.to_string() });
        }
        Ok(Funded { address: addr, balances })
    }

    async fn snapshot(&self, name: &str) -> Result<Snapshot> {
        let (handle, profile) = self.open(name).await?;
        let eng = self.engine(&handle, &profile);
        // Core state is fork state: bring it up to date so the snapshot holds it.
        self.core_sync(&handle, &profile, &eng).await?;
        let id = eng.rpc("evm_snapshot", json!([])).await?;
        Ok(Snapshot { id: id.as_str().unwrap_or_default().to_string() })
    }

    async fn revert(&self, name: &str, id: &str) -> Result<Reverted> {
        let (handle, profile) = self.open(name).await?;
        let eng = self.engine(&handle, &profile);
        let ok = eng.rpc("evm_revert", json!([id])).await?;
        if ok != json!(true) {
            return Err(Error::new(ErrorCode::SnapshotNotFound, format!("no snapshot {id} on {name}: a revert consumes its snapshot")).next(format!("chainsim snapshot {name}")));
        }
        let fresh = eng.rpc("evm_snapshot", json!([])).await?;
        Ok(Reverted { reverted: id.to_string(), snapshot: fresh.as_str().unwrap_or_default().to_string() })
    }

    async fn time(&self, name: &str, req: &TimeRequest) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        let eng = self.engine(&handle, &profile);
        let (_, before) = eng.latest_block().await?;
        let target = match req {
            TimeRequest::Advance(secs) => before.checked_add(*secs).ok_or_else(|| Error::new(ErrorCode::Usage, "the offset is too large"))?,
            TimeRequest::To(t) if *t <= before => return Err(Error::new(ErrorCode::Usage, format!("the fork is already at {before} s; time only moves forward"))),
            TimeRequest::To(t) => *t,
        };
        // The next block takes the new time, and one block is mined to carry it.
        eng.rpc("evm_setNextBlockTimestamp", json!([target])).await?;
        eng.rpc("evm_mine", json!([])).await?;
        self.core_sync_after(&handle, &profile, &eng).await;
        let (height, after) = eng.latest_block().await?;
        Ok(json!({ "timestamp": after, "timestampUsecs": (after as u128 * 1_000_000).to_string(), "previous": before, "height": height }))
    }

    async fn mine(&self, name: &str, blocks: Option<u64>) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        let eng = self.engine(&handle, &profile);
        let n = blocks.unwrap_or(1);
        if n == 0 || n > MAX_MINE {
            return Err(Error::new(ErrorCode::Usage, format!("mine takes 1 to {MAX_MINE} blocks")));
        }
        eng.rpc("anvil_mine", json!([qty(U256::from(n))])).await?;
        self.core_sync_after(&handle, &profile, &eng).await;
        let (height, ts) = eng.latest_block().await?;
        Ok(json!({ "mined": n, "height": height, "timestamp": ts, "timestampUsecs": (ts as u128 * 1_000_000).to_string() }))
    }

    async fn call(&self, name: &str, method: &str, params: Value, from: Option<&str>) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        let eng = self.engine(&handle, &profile);
        if !params.is_array() {
            return Err(Error::new(ErrorCode::Usage, "params must be a JSON array, such as '[\"0x…\", \"latest\"]'"));
        }
        self.core_sync(&handle, &profile, &eng).await?;
        if method == "corewriter_actions" && profile.hypercore.is_some() {
            let first = handle.fork_point.number + 1;
            let from = match params.get(0) {
                None => first,
                Some(p) => p.get("fromBlock").and_then(Value::as_u64).filter(|_| p.is_object()).ok_or_else(|| Error::new(ErrorCode::Usage, "corewriter_actions takes [] or [{\"fromBlock\": <block number>}]"))?,
            };
            let logs = eng.raw_actions(from.max(first)).await?;
            let mut decoded: Vec<Value> = logs.iter().map(hypercore::decode_log).collect();
            self.action_results(&eng, &mut decoded).await?;
            return Ok(json!(decoded));
        }
        if method == "hypercore_state" && profile.hypercore.is_some() {
            return self.hypercore_state(&eng, &params).await;
        }
        let result = match from {
            None => eng.rpc(method, params).await?,
            Some(from) => {
                if method != "eth_sendTransaction" {
                    return Err(Error::new(ErrorCode::Usage, "--from works only with eth_sendTransaction"));
                }
                let from = parse_address(from)?.to_string();
                let tx = params.get(0).cloned().filter(Value::is_object).ok_or_else(|| Error::new(ErrorCode::Usage, "eth_sendTransaction takes one transaction object: '[{\"to\": \"0x…\", \"value\": \"0x…\"}]'"))?;
                eng.send_as(&from, tx).await?
            }
        };
        if from.is_some() || MINING_METHODS.contains(&method) {
            self.core_sync_after(&handle, &profile, &eng).await;
        }
        // Some engine methods (anvil_nodeInfo) echo the fork URL: remove a provider URL.
        let text = eng.control.scrub_secret(&result.to_string());
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    async fn selftest(&self, name: &str) -> Result<SelftestReport> {
        let (handle, profile) = self.open(name).await?;
        let eng = self.engine(&handle, &profile);
        // Leave no trace: the whole selftest runs inside one snapshot.
        let outer = eng.rpc("evm_snapshot", json!([])).await?;
        let parts = self.checks(&eng, &profile).await;
        let _ = eng.rpc("evm_revert", json!([outer])).await;
        selftest_result(SelftestReport { ok: parts.iter().all(|c| c.ok), parts })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tops_up_only_when_gas_cannot_be_paid() {
        let (gas, price) = (U256::from(50_000u64), U256::from(10u64));
        // Exactly enough: no top-up.
        assert_eq!(top_up(U256::from(500_000u64), U256::ZERO, gas, price), None);
        // Empty account: twice the fee.
        assert_eq!(top_up(U256::ZERO, U256::ZERO, gas, price), Some(U256::from(1_000_000u64)));
        // Value counts, and the top-up is the difference.
        assert_eq!(top_up(U256::from(100u64), U256::from(7u64), gas, price), Some(U256::from(1_000_007u64 - 100)));
    }

    #[test]
    fn words_and_hex() {
        assert_eq!(word_hex("1").unwrap(), format!("0x{:064x}", 1));
        assert_eq!(word_hex("0xff").unwrap(), format!("0x{:064x}", 255));
        assert!(word_hex("nope").is_err());
        assert_eq!(hex_bytes("0xAbCd", "x").unwrap(), "0xabcd");
        assert!(hex_bytes("0xabc", "x").is_err());
    }

    #[test]
    fn reverts_carry_the_decoded_reason() {
        let data = abi::encode_call("Error(string)", &["Pausable: caller is not the pauser".to_string()]).unwrap();
        let e = reverted(Error::new(ErrorCode::RpcError, "eth_call: execution reverted (3)").data(json!({ "rpcData": data })));
        assert_eq!((e.code, e.code.exit_code()), (ErrorCode::TxReverted, 1));
        assert_eq!(e.message, "the transaction reverts on the fork: Pausable: caller is not the pauser");
        let e = reverted(Error::new(ErrorCode::RpcError, "eth_call: execution reverted: Ownable: caller is not the owner"));
        assert_eq!(e.data.unwrap()["reason"], "Ownable: caller is not the owner");
        assert_eq!(reverted(Error::new(ErrorCode::RateLimited, "slow")).code, ErrorCode::RateLimited);
        // An empty revert, as anvil reports it: no reason, and not the RPC code.
        let e = reverted(Error::new(ErrorCode::RpcError, "eth_call: execution reverted (3)").data(json!({ "rpcData": "0x" })));
        assert_eq!(e.data.unwrap()["reason"], "no reason given");
        let e = reverted(Error::new(ErrorCode::RpcError, "eth_call: execution reverted: Ownable: caller is not the owner (3)"));
        assert_eq!(e.data.unwrap()["reason"], "Ownable: caller is not the owner");
    }

    #[test]
    fn old_block_failures_point_at_an_archive_upstream() {
        let p = profile::get("ethereum-mainnet").unwrap();
        let want = "an old --block needs an archive upstream: export CHAINSIM_ETHEREUM_MAINNET_RPC=<archive provider URL>, or drop --block to fork at the head";
        let e = archive_hint(Error::new(ErrorCode::RpcError, "eth_getBalance: missing trie node abc"), &p);
        assert_eq!((e.code, e.next.as_deref()), (ErrorCode::UpstreamUnavailable, Some(want)));
        let e = archive_hint(Error::new(ErrorCode::RateLimited, "slow down").next("retry later"), &p);
        assert_eq!(e.next.as_deref(), Some(format!("retry later. If the block is old: {want}").as_str()));
        let e = archive_hint(Error::new(ErrorCode::NameConflict, "taken"), &p);
        assert_eq!(e.code, ErrorCode::NameConflict);
    }

    #[test]
    fn anvil_command_keeps_the_upstream_in_the_environment() {
        let arc = profile::get("arc-mainnet").unwrap();
        assert_eq!(
            anvil_command(&arc, 25177865),
            "exec anvil --host 0.0.0.0 --port 8545 --fork-url \"$CHAINSIM_UPSTREAM\" --fork-block-number 25177865 \
             '--retries' '20' '--fork-retry-backoff' '1500' '--compute-units-per-second' '1000'"
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        );
        let eth = profile::get("ethereum-mainnet").unwrap();
        assert!(!anvil_command(&eth, 1).contains("https://"));
        assert_eq!(arc.control_engine(), "control-curl");
    }
}
