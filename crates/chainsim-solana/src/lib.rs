//! The Solana adapter: mainnet forks on Surfpool, driven through JSON-RPC from
//! the fork's control sidecar.

pub mod fund;
pub mod rpc;
pub mod send;
pub mod snapshot;
pub mod time;
pub mod units;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chainsim_core::adapter::selftest_result;
use chainsim_core::control::{Control, Target};
use chainsim_core::docker::{self, ContainerSpec, Docker, ForkHandle};
use chainsim_core::error::upstream_next;
use chainsim_core::fork::*;
use chainsim_core::lock::EngineLock;
use chainsim_core::profile::{self, Profile};
use chainsim_core::{Adapter, Error, ErrorCode, Result};
use serde_json::{json, Value};

use rpc::Rpc;
use units::{parse_address, parse_amount};

/// How long `up` waits for the engine to answer `getHealth`.
const READY_TIMEOUT: Duration = Duration::from_secs(90);
/// The variable Surfpool reads its datasource URL from, so the URL is never an argument.
const DATASOURCE_ENV: &str = "SURFPOOL_DATASOURCE_RPC_URL";
/// The address selftest funds. No key for it is known.
const ST_RECEIVER: &str = "US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELGZ";
/// The address selftest sends from: the SHA-256 of "chainsim selftest sender". No key for it is known.
const ST_SENDER: &str = "9JK5Y9jJJkJNbi52uCbxTtNKfmBhSsbindebh2noAWRc";

pub const OPERATIONS: &[&str] = &["describe", "up", "down", "ls", "status", "fund", "send", "set", "snapshot", "revert", "time", "mine", "call", "selftest"];

/// Whether `set <kind>` works on Solana. Raw account data and owner changes
/// go through `call <name> surfnet_setAccount`.
pub fn supports_set(_profile: &Profile, kind: &SetKind) -> bool {
    match kind {
        SetKind::Balance | SetKind::Token { .. } => true,
        SetKind::Storage { .. } | SetKind::Code | SetKind::Owner | SetKind::Object { .. } | SetKind::Resource { .. } | SetKind::Precompile { .. } => false,
    }
}

/// The engine's arguments. The upstream URL is in the container environment.
pub fn surfpool_args(profile: &Profile) -> Vec<String> {
    let mut args: Vec<String> = ["start", "--no-tui", "--no-studio", "--ci", "--host", "0.0.0.0", "--port"].map(String::from).to_vec();
    args.push(profile.rpc_port().to_string());
    if let Some(ws) = profile.ports.ws {
        args.extend(["--ws-port".to_string(), ws.to_string()]);
    }
    args.extend(profile.fork.flags.iter().cloned());
    args
}

pub struct SolanaAdapter {
    pub(crate) docker: Docker,
    lock: EngineLock,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn ms(t: Instant) -> u64 {
    t.elapsed().as_millis() as u64
}

impl SolanaAdapter {
    pub async fn connect() -> Result<Self> {
        Ok(Self { docker: Docker::connect().await?, lock: EngineLock::embedded()? })
    }

    pub(crate) fn rpc<'a>(&'a self, handle: &ForkHandle, profile: &Profile) -> Rpc<'a> {
        let secret = profile.upstream_is_secret().then(|| profile.upstream_url());
        Rpc { control: Control::new(&self.docker, handle.control_container.clone(), secret).with_env(&profile.upstream.env), url: Target::Engine(handle.rpc_url(profile)) }
    }

    /// The fork, its profile, and a running engine, or an error an agent can act on.
    pub(crate) async fn open(&self, name: &str) -> Result<(ForkHandle, Profile)> {
        let handle = self.docker.find(name).await?.ok_or_else(|| Error::fork_not_found(name))?;
        let profile = profile::get(&handle.chain)?;
        if !handle.running || !self.docker.is_running(&handle.control_container).await {
            return Err(Error::new(ErrorCode::EngineFailed, format!("fork {name} is not running")).next(format!("chainsim down {name} && chainsim up {} --name {name}", profile.key)));
        }
        Ok((handle, profile))
    }

    /// Why the engine stopped or stayed silent, from its log, with every URL removed.
    async fn engine_failure(&self, name: &str, control: &Control<'_>) -> Error {
        let log = control.scrub(&self.docker.logs_tail(&docker::engine_container(name), 20).await);
        let lower = log.to_lowercase();
        let tail: String = log.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
        if lower.contains("429") || lower.contains("rate limit") || lower.contains("too many requests") {
            Error::new(ErrorCode::RateLimited, format!("the upstream rate-limited the engine at startup: {tail}")).next(upstream_next(control.env.as_deref(), true))
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

        self.docker.run_control(&self.lock, profile.control_engine(), name, &network, &upstream).await?;
        let control = Control::new(&self.docker, docker::control_container(name), secret).with_env(&profile.upstream.env);

        // The fork point: the upstream's slot just before the engine starts.
        // The engine starts at the upstream's slot at that moment, a few slots
        // later, and container labels cannot change after creation.
        let slot = control.rpc(&Target::Upstream, "getSlot", json!([rpc::processed()]), Duration::from_secs(30)).await?;
        let slot = slot.as_u64().ok_or_else(|| Error::new(ErrorCode::UpstreamUnavailable, format!("the upstream answered getSlot with {slot}")).next(upstream_next(Some(&profile.upstream.env), false)))?;
        let genesis = control.rpc(&Target::Upstream, "getGenesisHash", json!([]), Duration::from_secs(30)).await?;

        let engine = self.lock.engine(&profile.engine)?;
        let image = self.docker.ensure_engine(&profile.engine, engine).await?;
        let labels = HashMap::from([
            (docker::LABEL_FORK.to_string(), name.clone()),
            (docker::LABEL_CHAIN.to_string(), profile.key.clone()),
            (docker::LABEL_ENGINE.to_string(), format!("{}@{}", profile.engine, engine.version)),
            (docker::LABEL_FORK_POINT.to_string(), format!("slot:{slot}")),
            (docker::LABEL_ROLE.to_string(), docker::ROLE_ENGINE.to_string()),
            (docker::LABEL_NETWORK.to_string(), network.clone()),
            (docker::LABEL_ALIAS.to_string(), alias.clone()),
            (docker::LABEL_NETWORK_CREATED.to_string(), created.to_string()),
        ]);
        // The fork volume holds the snapshots, and is created owned by the engine user.
        if let Some(owner) = &engine.user {
            self.docker.ensure_volume_owned(&self.lock, &snapshot::data_volume(name), name, owner).await?;
        }
        let spec = ContainerSpec {
            name: docker::engine_container(name),
            image,
            user: engine.user.clone(),
            // A shell loop runs surfpool with these arguments, so revert can start it again from a snapshot.
            entrypoint: Some(vec!["/bin/sh".into(), "-c".into()]),
            cmd: vec![snapshot::engine_script(&surfpool_args(profile))],
            env: vec![format!("{DATASOURCE_ENV}={upstream}"), "HOME=/tmp".to_string()],
            labels,
            network,
            aliases: vec![alias],
            // The image's working directory, where surfpool looks for a project.
            tmpfs: vec![("/tmp".to_string(), "rw,nosuid,size=256m".to_string()), ("/workspace".to_string(), "rw,nosuid,size=64m,mode=1777".to_string())],
            memory: docker::ENGINE_MEMORY,
            publish: req.publish.then_some(profile.rpc_port()),
            mounts: vec![(snapshot::data_volume(name), snapshot::DATA.into())],
        };
        self.docker.run(&spec).await?;

        let handle = self.docker.find(name).await?.ok_or_else(|| Error::new(ErrorCode::EngineFailed, "the engine container vanished after start"))?;
        let rpc = self.rpc(&handle, profile);
        let started = Instant::now();
        loop {
            if !self.docker.is_running(&handle.engine_container).await {
                return Err(self.engine_failure(name, &rpc.control).await);
            }
            if rpc.call_timeout("getHealth", json!([]), Duration::from_secs(5)).await.is_ok_and(|v| v == "ok") {
                break;
            }
            if started.elapsed() > READY_TIMEOUT {
                let e = self.engine_failure(name, &rpc.control).await;
                return Err(Error::new(e.code, format!("the engine did not answer within {} s. {}", READY_TIMEOUT.as_secs(), e.message)));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        // The engine must report the upstream's chain, or clients would take the copy for another cluster.
        let local = rpc.call("getGenesisHash", json!([])).await?;
        if local != genesis {
            return Err(Error::new(ErrorCode::EngineFailed, format!("the engine reports genesis hash {local}, the upstream {genesis}")));
        }
        Ok(handle.fork(profile, None))
    }

    /// The selftest checks, in order. They leave 1 SOL and 1 of the profile's
    /// selftest token on the throwaway address, and 1 SOL less the transfer and its fee on the sender.
    async fn checks(&self, name: &str, rpc: &Rpc<'_>, profile: &Profile) -> Vec<Check> {
        let mut out = Vec::new();

        let t = Instant::now();
        let same = match (rpc.call("getGenesisHash", json!([])).await, rpc.upstream("getGenesisHash", json!([])).await) {
            (Ok(a), Ok(b)) => a == b && a.is_string(),
            _ => false,
        };
        out.push(Check { part: "genesis-hash", ok: same, reason: if same { "the fork reports the upstream's genesis hash" } else { "the fork's genesis hash is not the upstream's, or a read failed" }, ms: ms(t) });

        let t = Instant::now();
        let before = rpc.slot().await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let moved = matches!((before, rpc.slot().await), (Ok(a), Ok(b)) if b > a);
        out.push(Check { part: "slot-advances", ok: moved, reason: if moved { "the slot moved forward within 1.5 s" } else { "the slot did not move within 1.5 s, or a read failed" }, ms: ms(t) });

        let token = profile.selftest.token.as_deref().and_then(|t| profile.token(t));
        if let Some(token) = token {
            let t = Instant::now();
            let params = json!([token.address, { "encoding": "jsonParsed" }]);
            let (ok, reason) = match (rpc.call("getAccountInfo", params.clone()).await, rpc.upstream("getAccountInfo", params).await) {
                (Ok(a), Ok(b)) if !a["value"].is_null() && a["value"] == b["value"] => (true, "the token mint on the fork equals the upstream's"),
                (Ok(a), Ok(b)) if !a["value"].is_null() && without_supply(&a["value"]) == without_supply(&b["value"]) => (true, "the token mint on the fork equals the upstream's, apart from the supply, which moved on the upstream after the fork read it"),
                (Ok(_), Ok(_)) => (false, "the token mint on the fork differs from the upstream's"),
                _ => (false, "the token mint could not be read on the fork or the upstream"),
            };
            out.push(Check { part: "mainnet-state", ok, reason, ms: ms(t) });
        }

        let t = Instant::now();
        let decimals = profile.native.as_ref().map_or(9, |n| n.decimals);
        let funded = match parse_amount("1", decimals) {
            Ok(one) => fund::set_native(rpc, profile, ST_RECEIVER, one).await.is_ok(),
            Err(_) => false,
        };
        if funded {
            let _ = self.record(name, json!({ "kind": "selftest-fund", "address": ST_RECEIVER, "asset": "native", "amount": "1" })).await;
        }
        out.push(Check { part: "fund-native", ok: funded, reason: if funded { "a native balance set on the fork read back exactly" } else { "a native balance set on the fork did not read back" }, ms: ms(t) });

        if let Some(token) = token {
            let t = Instant::now();
            let funded = match fund::resolve_mint(rpc, profile, &token.symbol).await {
                Ok(mint) => match parse_amount("1", mint.decimals) {
                    Ok(one) => fund::set_token(rpc, ST_RECEIVER, &mint, one).await.is_ok(),
                    Err(_) => false,
                },
                Err(_) => false,
            };
            if funded {
                let _ = self.record(name, json!({ "kind": "selftest-fund", "address": ST_RECEIVER, "asset": token.symbol, "amount": "1" })).await;
            }
            out.push(Check { part: "fund-token", ok: funded, reason: if funded { "a token balance set on the fork read back exactly" } else { "a token balance set on the fork did not read back" }, ms: ms(t) });
        }

        let t = Instant::now();
        let sent = send::selftest_transfer(self, name, rpc, profile, ST_SENDER, ST_RECEIVER).await;
        out.push(Check { part: "send-as", ok: sent, reason: if sent { "a transfer sent as an address without its key moved the amount" } else { "a transfer sent as an address without its key did not land" }, ms: ms(t) });
        out
    }

    /// Adds one change to the fork's list, which `status` shows.
    pub(crate) async fn record(&self, name: &str, mut entry: Value) -> Result<()> {
        entry["at"] = json!(now_secs());
        chainsim_core::mods::record(&self.docker, name, &entry).await
    }
}

/// A parsed mint account without its supply, the one field that moves on mainnet.
fn without_supply(account: &Value) -> Value {
    let mut a = account.clone();
    if let Some(info) = a.pointer_mut("/data/parsed/info").and_then(Value::as_object_mut) {
        info.remove("supply");
    }
    a
}

/// True for an engine method that changes the fork: every `surfnet_` method
/// except the reads.
fn changes_state(method: &str) -> bool {
    method.strip_prefix("surfnet_").is_some_and(|m| !m.starts_with("get") && !m.starts_with("export") && !m.starts_with("profile"))
}

#[async_trait]
impl Adapter for SolanaAdapter {
    fn family(&self) -> &'static str {
        "solana"
    }

    fn operations(&self) -> &'static [&'static str] {
        OPERATIONS
    }

    async fn up(&self, profile: &Profile, req: &UpRequest) -> Result<Fork> {
        docker::check_name(&req.name)?;
        if req.block.is_some() {
            return Err(Error::unsupported(&format!("up --block on {}", profile.key), format!("chainsim up {} forks at the current slot: the engine reads each account from the upstream when it is first touched, so there is no pinned fork point", profile.key)));
        }
        if let Some(h) = self.docker.find(&req.name).await? {
            let taken = |why: String| {
                Error::new(ErrorCode::NameConflict, format!("fork {} already runs {why}", req.name)).next(format!("chainsim down {} first, or pick another --name", req.name))
            };
            if h.chain != profile.key {
                return Err(taken(format!("chain {}", h.chain)));
            }
            if req.network.as_ref().is_some_and(|n| *n != h.network) || req.alias.as_ref().is_some_and(|a| *a != h.alias) {
                return Err(taken(format!("on network {} as {}", h.network, h.alias)));
            }
            if req.publish && h.host_port.is_none() {
                return Err(taken("without --publish".into()));
            }
            if h.running && self.docker.is_running(&h.control_container).await {
                return Ok(h.fork(profile, None));
            }
            // A stopped fork of the same name and chain: start it fresh.
            self.docker.remove_fork(&req.name).await?;
        }
        match self.start(profile, req).await {
            Ok(fork) => Ok(fork),
            Err(e) => {
                let _ = self.docker.remove_fork(&req.name).await;
                Err(e)
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
        let head = self.rpc(&handle, &profile).slot().await?;
        Ok(Status { fork: handle.fork(&profile, None), head, uptime_secs: handle.uptime_secs })
    }

    async fn fund(&self, name: &str, req: &FundRequest) -> Result<Funded> {
        let (handle, profile) = self.open(name).await?;
        let rpc = self.rpc(&handle, &profile);
        let who = parse_address(&req.address)?;
        if req.native.is_none() && req.tokens.is_empty() {
            return Err(Error::new(ErrorCode::Usage, "nothing to fund: pass --native <amount> or --token <symbol|mint>:<amount>"));
        }
        let native = match &req.native {
            Some(a) => Some(parse_amount(a, profile.native.as_ref().map_or(9, |n| n.decimals))?),
            None => None,
        };
        let mut tokens = Vec::new();
        for t in &req.tokens {
            let mint = fund::resolve_mint(&rpc, &profile, &t.token).await?;
            let raw = parse_amount(&t.amount, mint.decimals)?;
            tokens.push((mint, raw));
        }

        // Safety: refuse an address in use on mainnet, checked once on the upstream.
        let held = rpc.upstream("getBalance", json!([who])).await?;
        if held["value"].as_u64() != Some(0) {
            return Err(Error::new(ErrorCode::MainnetAddress, format!("{who} holds SOL on {}: fund refuses it, because a transaction signed for the fork can also be valid on the real chain", profile.name))
                .next("fund a fresh test wallet that holds no mainnet assets"));
        }

        let mut balances = Vec::new();
        if let Some(lamports) = native {
            let b = fund::set_native(&rpc, &profile, &who, lamports).await?;
            self.record(name, json!({ "kind": "fund", "address": who, "asset": "native", "symbol": b.asset, "amount": b.amount })).await?;
            balances.push(b);
        }
        for (mint, raw) in &tokens {
            let (b, account) = fund::set_token(&rpc, &who, mint, *raw).await?;
            self.record(name, json!({ "kind": "fund", "address": who, "asset": mint.address, "symbol": b.asset, "amount": b.amount, "tokenAccount": account, "tokenProgram": mint.program })).await?;
            balances.push(b);
        }
        Ok(Funded { address: who, balances })
    }

    async fn set(&self, name: &str, req: &SetRequest) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        let rpc = self.rpc(&handle, &profile);
        let raw_write = format!("chainsim call {name} surfnet_setAccount '[\"<address>\", {{\"data\": \"<hex>\", \"owner\": \"<program>\"}}]' writes any account");
        let (mut out, entry) = match &req.kind {
            SetKind::Balance => {
                let who = parse_address(&req.address)?;
                let lamports = parse_amount(&req.value, profile.native.as_ref().map_or(9, |n| n.decimals))?;
                let b = fund::set_native(&rpc, &profile, &who, lamports).await?;
                (
                    json!({ "kind": "balance", "address": who, "symbol": b.asset, "amount": b.amount, "raw": b.raw }),
                    json!({ "kind": "set-balance", "address": who, "symbol": b.asset, "amount": b.amount }),
                )
            }
            SetKind::Token { from: Some(_), .. } => {
                return Err(Error::unsupported("set token --from on Solana", format!("chainsim set {name} token <address> <symbol|mint> <amount> sets the balance directly")));
            }
            SetKind::Token { coin_type, from: None } => {
                let who = parse_address(&req.address)?;
                let mint = fund::resolve_mint(&rpc, &profile, coin_type).await?;
                let raw = parse_amount(&req.value, mint.decimals)?;
                let (b, account) = fund::set_token(&rpc, &who, &mint, raw).await?;
                (
                    json!({ "kind": "token", "address": who, "mint": mint.address, "symbol": b.asset, "amount": b.amount, "raw": b.raw, "tokenAccount": account, "tokenProgram": mint.program }),
                    json!({ "kind": "set-token", "address": who, "mint": mint.address, "symbol": b.asset, "amount": b.amount, "tokenAccount": account }),
                )
            }
            other => return Err(Error::unsupported(&format!("set {} on Solana", other.name()), raw_write)),
        };
        self.record(name, entry).await?;
        out["note"] = json!(chainsim_core::mods::NOTE);
        Ok(out)
    }

    async fn snapshot(&self, name: &str) -> Result<Snapshot> {
        snapshot::snapshot(self, name).await
    }

    async fn revert(&self, name: &str, id: &str) -> Result<Reverted> {
        snapshot::revert(self, name, id).await
    }

    async fn send(&self, name: &str, req: &SendRequest) -> Result<Value> {
        send::send(self, name, req).await
    }

    async fn time(&self, name: &str, req: &TimeRequest) -> Result<Value> {
        time::time(self, name, req).await
    }

    async fn mine(&self, name: &str, blocks: Option<u64>) -> Result<Value> {
        time::mine(self, name, blocks).await
    }

    async fn call(&self, name: &str, method: &str, params: Value, from: Option<&str>) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        let rpc = self.rpc(&handle, &profile);
        if !params.is_array() {
            return Err(Error::new(ErrorCode::Usage, "params must be a JSON array, such as '[\"<address>\", {\"commitment\": \"processed\"}]'"));
        }
        if from.is_some() {
            return Err(Error::new(ErrorCode::Usage, "--from works only on EVM forks"));
        }
        let result = rpc.call(method, params.clone()).await?;
        if changes_state(method) {
            // A long account write is listed by its size, since the list is read back whole.
            let text = params.to_string();
            let shown = if text.len() <= 2048 { params } else { json!(format!("{} bytes of JSON", text.len())) };
            self.record(name, json!({ "kind": "call", "method": method, "params": shown })).await?;
        }
        // Remove a provider URL an engine method may echo.
        let text = rpc.control.scrub_secret(&result.to_string());
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    async fn selftest(&self, name: &str) -> Result<SelftestReport> {
        let (handle, profile) = self.open(name).await?;
        let rpc = self.rpc(&handle, &profile);
        let parts = self.checks(name, &rpc, &profile).await;
        selftest_result(SelftestReport { ok: parts.iter().all(|c| c.ok), parts })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_command_keeps_the_upstream_in_the_environment() {
        let p = profile::get("solana-mainnet").unwrap();
        let args = surfpool_args(&p);
        assert_eq!(args.join(" "), "start --no-tui --no-studio --ci --host 0.0.0.0 --port 8899 --ws-port 8900");
        assert!(!args.iter().any(|a| a.contains("://") || a == "--rpc-url"));
    }

    #[test]
    fn the_profile_matches_the_adapter() {
        let p = profile::get("solana-mainnet").unwrap();
        assert_eq!((p.family.as_str(), p.id.as_str(), p.engine.as_str()), ("solana", "solana:mainnet", "surfpool"));
        let lock = EngineLock::embedded().unwrap();
        let engine = lock.engine("surfpool").unwrap();
        assert!(engine.user.as_deref().is_some_and(|u| u != "0:0" && u != "root"), "the engine runs as a non-root user");
        let usdc = p.token(p.selftest.token.as_deref().unwrap()).unwrap();
        assert!(parse_address(&usdc.address).is_ok());
        for kind in SetKind::examples() {
            assert_eq!(supports_set(&p, &kind), matches!(kind, SetKind::Balance | SetKind::Token { .. }), "{}", kind.name());
        }
    }

    #[test]
    fn only_surfnet_writes_are_listed_as_changes() {
        for m in ["surfnet_setAccount", "surfnet_setTokenAccount", "surfnet_timeTravel", "surfnet_resetAccount", "surfnet_pauseClock"] {
            assert!(changes_state(m), "{m}");
        }
        for m in ["getBalance", "sendTransaction", "surfnet_getSurfnetInfo", "surfnet_exportSnapshot", "surfnet_getLocalSignatures", "surfnet_profileTransaction"] {
            assert!(!changes_state(m), "{m}");
        }
    }

    #[test]
    fn a_mint_compares_without_its_supply() {
        let a = json!({ "owner": "T", "data": { "parsed": { "info": { "decimals": 6, "supply": "1" }, "type": "mint" } } });
        let b = json!({ "owner": "T", "data": { "parsed": { "info": { "decimals": 6, "supply": "2" }, "type": "mint" } } });
        let c = json!({ "owner": "T", "data": { "parsed": { "info": { "decimals": 9, "supply": "1" }, "type": "mint" } } });
        assert_eq!(without_supply(&a), without_supply(&b));
        assert_ne!(without_supply(&a), without_supply(&c));
    }
}
