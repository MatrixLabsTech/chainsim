//! The Sui adapter (experimental): Mysten's sui-fork, driven from outside
//! through Docker and the fork's gRPC API (design sections 3 and 5). Every
//! call runs as grpcurl in the fork's control sidecar.

pub mod grpc;
pub mod units;

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chainsim_core::adapter::selftest_result;
use chainsim_core::docker::{self, ContainerSpec, Docker, ForkHandle, UPSTREAM_ENV};
use chainsim_core::fork::*;
use chainsim_core::lock::EngineLock;
use chainsim_core::profile::{self, Profile};
use chainsim_core::{Adapter, Error, ErrorCode, Result};
use serde_json::{json, Value};

use grpc::Grpc;
use units::*;

/// How long `up` waits for gRPC health. A cold fork measured 4 to 22 s.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a restart for snapshot or revert may take.
const RESTART_TIMEOUT: Duration = Duration::from_secs(60);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);
/// The UnitFlow target: a transaction confirms within 10 s.
const TX_TARGET: Duration = Duration::from_secs(10);
/// sui-fork has no gas estimation, so every transaction names its budget (0.01 SUI).
const GAS_BUDGET: u64 = 10_000_000;

/// The engine's data root: the fork data directory, snapshots and the supervisor's files.
const DATA: &str = "/data";

/// A throwaway receiver for the selftest transfer, never active on mainnet.
const ST_RECEIVER: &str = "0x000000000000000000000000000000000000000000000000000000000000c5e2";

pub const OPERATIONS: &[&str] = &["describe", "up", "down", "ls", "status", "fund", "snapshot", "revert", "call", "selftest"];

pub struct SuiAdapter {
    docker: Docker,
    lock: EngineLock,
}

/// A Sui address as 0x plus 64 lowercase hex digits.
pub fn parse_address(s: &str) -> Result<String> {
    let hex = s.strip_prefix("0x").unwrap_or("");
    if hex.is_empty() || hex.len() > 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::new(ErrorCode::Usage, format!("{s} is not a Sui address (0x and up to 64 hex digits)")));
    }
    Ok(format!("0x{:0>64}", hex.to_lowercase()))
}

/// A gRPC method as `package.Service/Method`. A bare `Service/Method` gets
/// its package: `sui.forking.v1alpha` for ForkingService, else `sui.rpc.v2`.
pub fn full_method(method: &str) -> Result<String> {
    let Some((service, rpc)) = method.split_once('/') else {
        return Err(Error::new(ErrorCode::Usage, format!("{method} is not a gRPC method; use Service/Method, such as LedgerService/GetObject")));
    };
    if service.contains('.') {
        return Ok(method.to_string());
    }
    let package = if service == "ForkingService" { "sui.forking.v1alpha" } else { "sui.rpc.v2" };
    Ok(format!("{package}.{service}/{rpc}"))
}

/// The request body for `call`: a JSON object, `[]` or `[object]`.
pub fn call_body(params: Value) -> Result<Value> {
    match params {
        Value::Object(_) => Ok(params),
        Value::Null => Ok(json!({})),
        Value::Array(a) if a.is_empty() => Ok(json!({})),
        Value::Array(mut a) if a.len() == 1 && a[0].is_object() => Ok(a.remove(0)),
        _ => Err(Error::new(ErrorCode::Usage, "params must be one protobuf-JSON request object, such as '{\"objectId\": \"0x6\"}'")),
    }
}

/// The engine's entrypoint: a supervisor loop around sui-fork. Snapshot and
/// revert stop sui-fork while `hold` exists, copy the data directory, and let
/// the loop start it again. Seed flags go only to the first start: sui-fork
/// refuses them once `seed_manifest.json` exists.
pub fn engine_script(checkpoint: u64, funder: Option<&str>, port: u16) -> String {
    // The funder is checked to be 0x and hex before it gets here, so it needs no quoting.
    let seed = funder.map(|f| format!("--address {f}")).unwrap_or_default();
    format!(
        "D={DATA}; mkdir -p $D/snap; \
         while :; do \
           while [ -e $D/hold ]; do sleep 0.2; done; \
           if [ -e $D/fork/seed_manifest.json ]; then seed=''; else seed='{seed}'; fi; \
           sui-fork start --network \"${{{UPSTREAM_ENV}:-mainnet}}\" --checkpoint {checkpoint} $seed --rpc-addr 0.0.0.0:{port} --data-dir $D/fork & \
           echo $! > $D/pid; wait $!; \
           [ -e $D/hold ] || exit 1; \
         done"
    )
}

/// Stops sui-fork for a copy. Exits 3 when it does not stop within 30 s.
const STOP: &str = "D=/data; touch $D/hold; pid=$(cat $D/pid); kill $pid 2>/dev/null; i=0; \
    while kill -0 $pid 2>/dev/null; do i=$((i+1)); [ $i -gt 300 ] && kill -9 $pid; [ $i -gt 350 ] && exit 3; sleep 0.1; done";

fn snapshot_script(id: &str) -> String {
    format!("set -e; {STOP}; rm -rf $D/snap/{id}; cp -a $D/fork $D/snap/{id}; rm -f $D/hold")
}

/// Restores snapshot `old` and keeps the same files as the fresh snapshot
/// `new` of the restored state. Exits 4 when `old` does not exist.
fn revert_script(old: &str, new: &str) -> String {
    format!("D=/data; [ -d $D/snap/{old} ] || exit 4; set -e; {STOP}; rm -rf $D/fork; cp -a $D/snap/{old} $D/fork; mv $D/snap/{old} $D/snap/{new}; rm -f $D/hold")
}

/// The fork's data volume, labeled with the fork so `down` removes it.
pub fn data_volume(name: &str) -> String {
    format!("chainsim-{name}-data")
}

fn snapshot_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("s{nanos:x}")
}

fn valid_snapshot_id(id: &str) -> bool {
    id.len() > 1 && id.starts_with('s') && id[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn ms(t: Instant) -> u64 {
    t.elapsed().as_millis() as u64
}

fn u64_field(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

impl SuiAdapter {
    pub async fn connect() -> Result<Self> {
        Ok(Self { docker: Docker::connect().await?, lock: EngineLock::embedded()? })
    }

    pub fn docker(&self) -> &Docker {
        &self.docker
    }

    /// The control path of a fork.
    pub fn grpc<'a>(&'a self, handle: &ForkHandle, profile: &Profile) -> Grpc<'a> {
        let secret = profile.upstream_is_secret().then(|| profile.upstream_url());
        Grpc { docker: &self.docker, container: handle.control_container.clone(), target: format!("{}:{}", handle.alias, profile.ports.rpc), secret }
    }

    async fn open(&self, name: &str) -> Result<(ForkHandle, Profile)> {
        docker::check_name(name)?;
        let handle = self.docker.find(name).await?.ok_or_else(|| Error::fork_not_found(name))?;
        let profile = profile::get(&handle.chain)?;
        if !handle.running || !self.docker.is_running(&handle.control_container).await {
            return Err(Error::new(ErrorCode::EngineFailed, format!("fork {name} is not running")).next(format!("chainsim down {name} && chainsim up {} --name {name}", profile.key)));
        }
        Ok((handle, profile))
    }

    /// Why the engine stopped, from its log, with every URL removed.
    async fn engine_failure(&self, name: &str, g: &Grpc<'_>) -> Error {
        let log = g.scrub(&self.docker.logs_tail(&docker::engine_container(name), 20).await);
        let lower = log.to_lowercase();
        let tail: String = log.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
        if lower.contains("429") || lower.contains("rate limit") || lower.contains("too many requests") {
            Error::new(ErrorCode::RateLimited, format!("the upstream rate-limited the engine at startup: {tail}")).next("retry later, or set the profile's upstream env variable to a provider URL")
        } else if lower.contains("graphql") || lower.contains("error sending request") || lower.contains("connect") {
            Error::new(ErrorCode::UpstreamUnavailable, format!("the engine could not fork from the upstream: {tail}")).next("check the upstream, or set the profile's upstream env variable to a provider URL (chainsim describe <chain>)")
        } else {
            Error::new(ErrorCode::EngineFailed, format!("the engine did not start: {tail}"))
        }
    }

    /// Waits for gRPC health: sui-fork prints no ready line.
    async fn wait_healthy(&self, name: &str, g: &Grpc<'_>, timeout: Duration) -> Result<()> {
        let started = Instant::now();
        loop {
            if !self.docker.is_running(&docker::engine_container(name)).await {
                return Err(self.engine_failure(name, g).await);
            }
            if g.healthy().await {
                return Ok(());
            }
            if started.elapsed() > timeout {
                let e = self.engine_failure(name, g).await;
                return Err(Error::new(e.code, format!("the engine did not report healthy within {} s. {}", timeout.as_secs(), e.message)));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn start(&self, profile: &Profile, req: &UpRequest) -> Result<Fork> {
        let name = &req.name;
        let upstream = profile.upstream_url();
        let secret = profile.upstream_is_secret();
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

        // The control sidecar: grpcurl (the profile's control engine), with the upstream URL in its environment only.
        self.docker.run_control(&self.lock, profile.control_engine(), name, &network, &upstream).await?;
        let probe = Grpc { docker: &self.docker, container: docker::control_container(name), target: format!("{alias}:{}", profile.ports.rpc), secret: secret.then(|| upstream.clone()) };

        // The fork point: the asked checkpoint, or the upstream head now, so
        // the label records the exact checkpoint sui-fork starts from.
        let checkpoint = match req.block {
            Some(c) => c,
            None => {
                let data = probe.upstream_graphql("{ checkpoint { sequenceNumber } }", Duration::from_secs(30)).await?;
                u64_field(&data["checkpoint"]["sequenceNumber"]).ok_or_else(|| Error::new(ErrorCode::UpstreamUnavailable, "the upstream did not return its latest checkpoint").next("retry"))?
            }
        };

        let funder = profile.funder.as_deref().map(parse_address).transpose()?;
        let engine = self.lock.engine(&profile.engine)?;
        let image = self.docker.ensure_engine(&profile.engine, engine).await?;
        let labels = HashMap::from([
            (docker::LABEL_FORK.to_string(), name.clone()),
            (docker::LABEL_CHAIN.to_string(), profile.key.clone()),
            (docker::LABEL_ENGINE.to_string(), format!("{}@{}", profile.engine, engine.version)),
            (docker::LABEL_FORK_POINT.to_string(), format!("checkpoint:{checkpoint}")),
            (docker::LABEL_ROLE.to_string(), docker::ROLE_ENGINE.to_string()),
            (docker::LABEL_NETWORK.to_string(), network.clone()),
            (docker::LABEL_ALIAS.to_string(), alias.clone()),
            (docker::LABEL_NETWORK_CREATED.to_string(), created.to_string()),
        ]);
        // sui-fork takes "mainnet" or a GraphQL URL; a provider URL replaces the keyword.
        let mut env = vec![format!("HOME={DATA}")];
        if secret {
            env.push(format!("{UPSTREAM_ENV}={upstream}"));
        }
        self.docker
            .run(&ContainerSpec {
                name: docker::engine_container(name),
                image,
                user: engine.user.clone(),
                entrypoint: Some(vec!["/bin/sh".into(), "-c".into()]),
                cmd: vec![engine_script(checkpoint, funder.as_deref(), profile.ports.rpc)],
                env,
                labels,
                network: network.clone(),
                aliases: vec![alias.clone()],
                tmpfs: vec![("/tmp".into(), "rw,nosuid,size=64m".into())],
                memory: docker::ENGINE_MEMORY,
                publish: req.publish.then_some(profile.ports.rpc),
                // The data directory and snapshots outlive a sui-fork restart; down removes the volume.
                mounts: vec![(data_volume(name), DATA.into())],
            })
            .await?;

        let handle = self.docker.find(name).await?.ok_or_else(|| Error::new(ErrorCode::EngineFailed, "the engine container vanished after start"))?;
        let g = self.grpc(&handle, profile);
        self.wait_healthy(name, &g, READY_TIMEOUT).await?;
        Ok(handle.fork(profile, None))
    }

    /// Runs a snapshot or revert script in the engine, then waits for sui-fork to serve again.
    async fn restart_with(&self, name: &str, g: &Grpc<'_>, script: String) -> Result<i64> {
        let out = self.docker.exec(&docker::engine_container(name), &["sh".into(), "-c".into(), script]).await?;
        if out.exit_code == 0 {
            self.wait_healthy(name, g, RESTART_TIMEOUT).await?;
        }
        Ok(out.exit_code)
    }

    async fn balance(&self, g: &Grpc<'_>, owner: &str) -> Result<u64> {
        let v = g.call("sui.rpc.v2.StateService/GetBalance", &json!({ "owner": owner, "coinType": "0x2::sui::SUI" }), CALL_TIMEOUT).await?;
        Ok(u64_field(&v["balance"]["balance"]).unwrap_or(0))
    }

    /// The funder's largest SUI coin on the fork, as a gas object reference.
    async fn funder_coin(&self, g: &Grpc<'_>, funder: &str) -> Result<(Value, u64)> {
        let body = json!({ "owner": funder, "objectType": "0x2::coin::Coin<0x2::sui::SUI>", "pageSize": 50, "readMask": { "paths": ["object_id", "version", "digest", "balance"] } });
        let v = g.call("sui.rpc.v2.StateService/ListOwnedObjects", &body, CALL_TIMEOUT).await?;
        v["objects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| Some((json!({ "objectId": o["objectId"], "version": o["version"], "digest": o["digest"] }), u64_field(&o["balance"])?)))
            .max_by_key(|(_, b)| *b)
            .ok_or_else(|| {
                Error::new(ErrorCode::EngineFailed, format!("the funder {funder} has no SUI coins on this fork: seeding works only for fork points within about an hour of the upstream head"))
                    .next("chainsim down <name>, then chainsim up sui-mainnet without --block")
            })
    }

    /// Sends `amount` MIST from the profile's funder to `to` by impersonation:
    /// a transaction with no signatures, which sui-fork executes as its sender.
    async fn send_from_funder(&self, g: &Grpc<'_>, profile: &Profile, to: &str, amount: u64) -> Result<()> {
        let funder = profile.funder.as_deref().ok_or_else(|| Error::new(ErrorCode::Unsupported, format!("{} names no funder address", profile.key)).next("chainsim describe sui-mainnet"))?;
        let (coin, coin_balance) = self.funder_coin(g, funder).await?;
        if coin_balance < amount.saturating_add(GAS_BUDGET) {
            return Err(Error::new(ErrorCode::EngineFailed, format!("the funder's largest coin holds {} SUI, less than the {} SUI asked plus gas", format_amount(coin_balance as u128, 9), format_amount(amount as u128, 9))));
        }
        let epoch = g.call("sui.rpc.v2.LedgerService/GetEpoch", &json!({ "readMask": { "paths": ["reference_gas_price"] } }), CALL_TIMEOUT).await?;
        let price = u64_field(&epoch["epoch"]["referenceGasPrice"]).ok_or_else(|| Error::new(ErrorCode::EngineFailed, "the fork did not return a reference gas price"))?;
        let body = transfer_request(funder, to, amount, coin, price);
        let out = g.call("sui.rpc.v2.TransactionExecutionService/ExecuteTransaction", &body, CALL_TIMEOUT).await?;
        let status = &out["transaction"]["effects"]["status"];
        if status["success"] != json!(true) {
            return Err(Error::new(ErrorCode::RpcError, format!("the transfer from the funder failed on the fork: {}", status["error"])));
        }
        Ok(())
    }

    /// The selftest checks. Never returns early: every part reports.
    async fn checks(&self, handle: &ForkHandle, profile: &Profile, g: &Grpc<'_>) -> Vec<Check> {
        let mut parts = Vec::new();

        let t = Instant::now();
        let ok = g.healthy().await;
        parts.push(Check { part: "grpc-health", ok, reason: if ok { "grpc.health.v1 reports SERVING" } else { "grpc.health.v1 does not report SERVING" }, ms: ms(t) });

        let t = Instant::now();
        let status = g.call("sui.forking.v1alpha.ForkingService/GetStatus", &json!({}), CALL_TIMEOUT).await;
        let forked_at = status.as_ref().ok().and_then(|s| u64_field(&s["forkedAtCheckpoint"]));
        let ok = forked_at == Some(handle.fork_point.number);
        parts.push(Check {
            part: "status",
            ok,
            reason: match (status.is_ok(), ok) {
                (_, true) => "ForkingService/GetStatus answers with the labeled fork checkpoint",
                (true, false) => "ForkingService/GetStatus answers with another fork checkpoint than the label",
                _ => "ForkingService/GetStatus did not answer",
            },
            ms: ms(t),
        });

        let t = Instant::now();
        let clock = g.call("sui.rpc.v2.LedgerService/GetObject", &json!({ "objectId": "0x6" }), CALL_TIMEOUT).await;
        let ok = clock.is_ok_and(|c| c["object"]["objectId"].as_str().is_some_and(|id| id.ends_with("06")));
        parts.push(Check { part: "clock", ok, reason: if ok { "LedgerService/GetObject returns the Clock 0x6" } else { "LedgerService/GetObject did not return the Clock 0x6" }, ms: ms(t) });

        // funded-transfer: 1 SUI from the funder by impersonation, within 10 s.
        let t = Instant::now();
        let unit = 10u64.pow(9);
        let before = self.balance(g, ST_RECEIVER).await.ok();
        let sent = self.send_from_funder(g, profile, ST_RECEIVER, unit).await.is_ok();
        let after = self.balance(g, ST_RECEIVER).await.ok();
        let moved = matches!((before, after), (Some(b), Some(a)) if a == b + unit);
        let elapsed = t.elapsed();
        parts.push(Check {
            part: "funded-transfer",
            ok: sent && moved && elapsed <= TX_TARGET,
            reason: match (sent && moved, elapsed <= TX_TARGET) {
                (true, true) => "a 1 SUI transfer from the funder by impersonation executed within 10 s and moved the balance",
                (true, false) => "a 1 SUI transfer from the funder executed but took longer than 10 s",
                _ => "a 1 SUI transfer from the funder by impersonation failed or did not move the balance",
            },
            ms: ms(t),
        });
        parts
    }
}

/// An ExecuteTransaction request with no signatures: SplitCoins from the gas
/// coin, then TransferObjects to `to`.
pub fn transfer_request(sender: &str, to: &str, amount: u64, gas_coin: Value, gas_price: u64) -> Value {
    json!({
        "transaction": {
            "version": 1,
            "sender": sender,
            "kind": {
                "kind": "PROGRAMMABLE_TRANSACTION",
                "programmableTransaction": {
                    "inputs": [
                        { "kind": "PURE", "pure": base64(&amount.to_le_bytes()) },
                        { "kind": "PURE", "pure": base64(&address_bytes(to)) },
                    ],
                    "commands": [
                        { "splitCoins": { "coin": { "kind": "GAS" }, "amounts": [{ "kind": "INPUT", "input": 0 }] } },
                        { "transferObjects": { "objects": [{ "kind": "RESULT", "result": 0 }], "address": { "kind": "INPUT", "input": 1 } } },
                    ],
                },
            },
            "gasPayment": { "objects": [gas_coin], "owner": sender, "price": gas_price.to_string(), "budget": GAS_BUDGET.to_string() },
            "expiration": { "kind": "NONE" },
        },
        "signatures": [],
        "readMask": { "paths": ["effects.status", "digest"] },
    })
}

#[async_trait]
impl Adapter for SuiAdapter {
    fn family(&self) -> &'static str {
        "sui"
    }

    fn operations(&self) -> &'static [&'static str] {
        OPERATIONS
    }

    async fn up(&self, profile: &Profile, req: &UpRequest) -> Result<Fork> {
        docker::check_name(&req.name)?;
        if let Some(h) = self.docker.find(&req.name).await? {
            let taken = |why: String| {
                Error::new(ErrorCode::NameConflict, format!("fork {} already runs {why}", req.name)).next(format!("chainsim down {} first, or pick another --name", req.name))
            };
            if h.chain != profile.key {
                return Err(taken(format!("chain {}", h.chain)));
            }
            if let Some(b) = req.block.filter(|b| *b != h.fork_point.number) {
                return Err(taken(format!("at checkpoint {} (asked {b})", h.fork_point.number)));
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
        let g = self.grpc(&handle, &profile);
        let s = g.call("sui.forking.v1alpha.ForkingService/GetStatus", &json!({}), CALL_TIMEOUT).await?;
        let head = u64_field(&s["checkpointSequenceNumber"]).ok_or_else(|| Error::new(ErrorCode::EngineFailed, "GetStatus returned no checkpoint"))?;
        Ok(Status { fork: handle.fork(&profile, None), head, uptime_secs: handle.uptime_secs })
    }

    async fn fund(&self, name: &str, req: &FundRequest) -> Result<Funded> {
        let (handle, profile) = self.open(name).await?;
        let g = self.grpc(&handle, &profile);
        let who = parse_address(&req.address)?;
        if !req.tokens.is_empty() {
            return Err(Error::new(ErrorCode::Unsupported, format!("{} funds SUI only", profile.key)).next(format!("chainsim fund {name} {who} --native <amount>")));
        }
        let Some(native) = &req.native else {
            return Err(Error::new(ErrorCode::Usage, "nothing to fund: pass --native <amount>"));
        };
        let decimals = profile.native.as_ref().map_or(9, |n| n.decimals);
        let target = u64::try_from(parse_amount(native, decimals)?).map_err(|_| Error::new(ErrorCode::Usage, format!("{native} SUI is more than a u64 of MIST")))?;

        // An address with any owned object on mainnet may hold real assets.
        let query = format!("{{ address(address: \"{who}\") {{ objects(first: 1) {{ nodes {{ address }} }} }} }}");
        let data = g.upstream_graphql(&query, Duration::from_secs(30)).await?;
        if data["address"]["objects"]["nodes"].as_array().is_some_and(|n| !n.is_empty()) {
            return Err(Error::new(ErrorCode::MainnetAddress, format!("{who} owns objects on Sui Mainnet; fund only addresses of test keys that hold no mainnet assets")).next("use a fresh test address"));
        }

        let current = self.balance(&g, &who).await?;
        if current > target {
            return Err(Error::new(ErrorCode::Unsupported, format!("{who} holds {} SUI, more than the {native} SUI asked; on Sui, fund only raises a balance", format_amount(current as u128, decimals)))
                .next(format!("chainsim revert {name} <snapshot>, or fund a fresh address")));
        }
        if current < target {
            self.send_from_funder(&g, &profile, &who, target - current).await?;
        }
        let read = self.balance(&g, &who).await?;
        if read != target {
            return Err(Error::new(ErrorCode::ReadbackMismatch, format!("{who} reads back {} SUI after funding {native} SUI", format_amount(read as u128, decimals))));
        }
        Ok(Funded {
            address: who,
            balances: vec![Balance { asset: "native".into(), address: None, decimals, amount: format_amount(read as u128, decimals), raw: read.to_string() }],
        })
    }

    async fn snapshot(&self, name: &str) -> Result<Snapshot> {
        let (handle, profile) = self.open(name).await?;
        let g = self.grpc(&handle, &profile);
        let id = snapshot_id();
        match self.restart_with(name, &g, snapshot_script(&id)).await? {
            0 => Ok(Snapshot { id }),
            c => Err(Error::new(ErrorCode::EngineFailed, format!("snapshot failed: the engine copy step exited {c}"))),
        }
    }

    async fn revert(&self, name: &str, id: &str) -> Result<Reverted> {
        let (handle, profile) = self.open(name).await?;
        if !valid_snapshot_id(id) {
            return Err(Error::new(ErrorCode::SnapshotNotFound, format!("no snapshot {id} on {name}")).next(format!("chainsim snapshot {name}")));
        }
        let g = self.grpc(&handle, &profile);
        let fresh = snapshot_id();
        match self.restart_with(name, &g, revert_script(id, &fresh)).await? {
            0 => Ok(Reverted { reverted: id.to_string(), snapshot: fresh }),
            4 => Err(Error::new(ErrorCode::SnapshotNotFound, format!("no snapshot {id} on {name}: it was never taken or was already used")).next(format!("chainsim snapshot {name}"))),
            c => Err(Error::new(ErrorCode::EngineFailed, format!("revert failed: the engine copy step exited {c}"))),
        }
    }

    async fn call(&self, name: &str, method: &str, params: Value, from: Option<&str>) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        if from.is_some() {
            return Err(Error::new(ErrorCode::Usage, "--from is EVM only; on Sui, send TransactionExecutionService/ExecuteTransaction with an empty signatures list to act as the sender"));
        }
        let g = self.grpc(&handle, &profile);
        let out = g.call(&full_method(method)?, &call_body(params)?, CALL_TIMEOUT).await?;
        let text = g.scrub(&out.to_string());
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    async fn selftest(&self, name: &str) -> Result<SelftestReport> {
        let (handle, profile) = self.open(name).await?;
        let g = self.grpc(&handle, &profile);
        let parts = self.checks(&handle, &profile, &g).await;
        selftest_result(SelftestReport { ok: parts.iter().all(|c| c.ok), parts })
    }
}

/// The vendored ForkingService proto (mainnet-v1.81.1), for the drift test.
pub const FORKING_PROTO: &str = include_str!("../proto/sui/forking/v1alpha/forking_service.proto");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(parse_address("0x6").unwrap(), format!("0x{}6", "0".repeat(63)));
        assert_eq!(parse_address(&format!("0x{}", "A".repeat(64))).unwrap(), format!("0x{}", "a".repeat(64)));
        assert!(parse_address("6").is_err());
        assert!(parse_address(&format!("0x{}", "1".repeat(65))).is_err());
        assert!(parse_address("0xzz").is_err());
    }

    #[test]
    fn methods_and_bodies() {
        assert_eq!(full_method("LedgerService/GetObject").unwrap(), "sui.rpc.v2.LedgerService/GetObject");
        assert_eq!(full_method("ForkingService/GetStatus").unwrap(), "sui.forking.v1alpha.ForkingService/GetStatus");
        assert_eq!(full_method("grpc.health.v1.Health/Check").unwrap(), "grpc.health.v1.Health/Check");
        assert_eq!(full_method("GetObject").unwrap_err().code, ErrorCode::Usage);
        assert_eq!(call_body(json!([])).unwrap(), json!({}));
        assert_eq!(call_body(json!([{ "objectId": "0x6" }])).unwrap(), json!({ "objectId": "0x6" }));
        assert_eq!(call_body(json!(["0x6"])).unwrap_err().code, ErrorCode::Usage);
    }

    #[test]
    fn vendored_proto_lists_three_rpcs() {
        assert_eq!(grpc::rpc_names(FORKING_PROTO), vec!["AdvanceCheckpoint", "AdvanceClock", "GetStatus"]);
        assert!(FORKING_PROTO.contains("SPDX-License-Identifier: Apache-2.0") && FORKING_PROTO.contains("tag mainnet-v1.81.1"));
    }

    #[test]
    fn engine_script_seeds_only_the_first_start() {
        let s = engine_script(42, Some("0xabc"), 9000);
        assert!(s.contains("--checkpoint 42") && s.contains("--rpc-addr 0.0.0.0:9000") && s.contains("--data-dir $D/fork"));
        assert!(s.contains("seed_manifest.json ]; then seed=''; else seed='--address 0xabc'; fi"), "{s}");
        assert!(s.contains("${CHAINSIM_UPSTREAM:-mainnet}"));
        assert!(!engine_script(42, None, 9000).contains("--address"));
    }

    #[test]
    fn snapshot_ids_are_safe_in_scripts() {
        let id = snapshot_id();
        assert!(valid_snapshot_id(&id), "{id}");
        assert!(!valid_snapshot_id("s1; rm -rf /"));
        assert!(!valid_snapshot_id("../x"));
        assert!(revert_script("s1", "s2").starts_with("D=/data; [ -d $D/snap/s1 ] || exit 4;"));
    }

    #[test]
    fn transfer_request_has_no_signatures() {
        let to = format!("0x{}c5", "0".repeat(62));
        let r = transfer_request("0xf", &to, 1_000_000_000, json!({ "objectId": "0x1" }), 100);
        assert_eq!(r["signatures"], json!([]));
        let inputs = &r["transaction"]["kind"]["programmableTransaction"]["inputs"];
        assert_eq!(inputs[0]["pure"], "AMqaOwAAAAA=");
        assert_eq!(inputs[1]["pure"], "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAMU=");
        assert_eq!(r["transaction"]["gasPayment"]["price"], "100");
    }
}
