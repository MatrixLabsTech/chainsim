//! The Sui adapter (experimental): Mysten's sui-fork, driven from outside
//! through Docker and the fork's gRPC API (design sections 3 and 5). Every
//! call runs as grpcurl in the fork's control sidecar.

pub mod grpc;
pub mod object;
pub mod ptb;
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

use grpc::{upstream_next, Grpc};
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
/// The gas budget for `send` and `set`, paid by the funder (0.5 SUI; unused gas is refunded).
const SEND_BUDGET: u64 = 500_000_000;
const EXECUTE: &str = "sui.rpc.v2.TransactionExecutionService/ExecuteTransaction";

/// The engine's data root: the fork data directory, snapshots and the supervisor's files.
const DATA: &str = "/data";

/// A throwaway receiver for the selftest transfer, never active on mainnet.
const ST_RECEIVER: &str = "0x000000000000000000000000000000000000000000000000000000000000c5e2";

pub const OPERATIONS: &[&str] = &["describe", "up", "down", "ls", "status", "fund", "send", "set", "snapshot", "revert", "call", "selftest"];

/// One fork's control path, profile and handle, for the transaction helpers.
struct On<'a> {
    g: &'a Grpc<'a>,
    profile: &'a Profile,
    fork: &'a ForkHandle,
}

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
/// A trap clears `hold` on any exit, so a failed copy never leaves sui-fork stopped.
const STOP: &str = "D=/data; trap 'rm -f $D/hold' EXIT; touch $D/hold; pid=$(cat $D/pid); kill $pid 2>/dev/null; i=0; \
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

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// An object as a PTB input, from a `GetObject` reply: owned and immutable
/// objects by reference, shared ones by their initial shared version.
pub fn object_input(object: &Value, mutable: bool) -> Result<Value> {
    let id = &object["objectId"];
    match object["owner"]["kind"].as_str() {
        Some("SHARED" | "CONSENSUS_ADDRESS") => Ok(json!({ "kind": "SHARED", "objectId": id, "version": object["owner"]["version"], "mutable": mutable })),
        Some("ADDRESS" | "OBJECT" | "IMMUTABLE") => Ok(json!({ "kind": "IMMUTABLE_OR_OWNED", "objectId": id, "version": object["version"], "digest": object["digest"] })),
        other => Err(Error::new(ErrorCode::Internal, format!("object {id} has an unknown owner kind {other:?}"))),
    }
}

/// Refuses an owned input the sender does not own, before any gas is spent:
/// the fork would reject it at execution. Shared and immutable inputs pass.
pub fn check_owner(object: &Value, sender: &str, fork: &str) -> Result<()> {
    let id = object["objectId"].as_str().unwrap_or("?");
    match object["owner"]["kind"].as_str() {
        Some("ADDRESS") => match object["owner"]["address"].as_str() {
            Some(owner) if owner != sender => Err(Error::new(ErrorCode::Usage, format!("object {id} is owned by {owner}, not by the sender {sender}"))
                .next(format!("rerun with --as {owner}, or chainsim set {fork} owner {id} {sender} first"))),
            _ => Ok(()),
        },
        Some("OBJECT") => Err(Error::new(ErrorCode::Usage, format!("object {id} is owned by another object ({}), so it cannot be a transaction input", owner_text(&object["owner"])))
            .next("pass its parent object, and let the parent's module reach it")),
        _ => Ok(()),
    }
}

/// The owner of an object for messages: an address, or the kind.
fn owner_text(owner: &Value) -> String {
    owner["address"].as_str().map(str::to_string).unwrap_or_else(|| owner["kind"].as_str().unwrap_or("unknown").to_lowercase())
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
        Grpc { docker: &self.docker, container: handle.control_container.clone(), target: format!("{}:{}", handle.alias, profile.ports.rpc), secret, upstream_env: profile.upstream.env.clone() }
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
            Error::new(ErrorCode::RateLimited, format!("the upstream rate-limited the engine at startup: {tail}")).next(upstream_next(&g.upstream_env))
        } else if lower.contains("graphql") || lower.contains("error sending request") || lower.contains("connect") {
            Error::new(ErrorCode::UpstreamUnavailable, format!("the engine could not fork from the upstream: {tail}")).next(upstream_next(&g.upstream_env))
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

    // The engine spec sets every field today; `..Default::default()` keeps it compiling when core adds one.
    #[allow(clippy::needless_update)]
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
        let probe = Grpc { docker: &self.docker, container: docker::control_container(name), target: format!("{alias}:{}", profile.ports.rpc), secret: secret.then(|| upstream.clone()), upstream_env: profile.upstream.env.clone() };

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
        // The fork volume is created owned by the engine user, so sui-fork runs as non-root.
        if let Some(owner) = &engine.user {
            self.docker.ensure_volume_owned(&self.lock, &data_volume(name), name, owner).await?;
        }
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
                ..Default::default()
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

    async fn get_object(&self, g: &Grpc<'_>, id: &str) -> Result<Value> {
        let body = json!({ "objectId": id, "readMask": { "paths": ["object_id", "version", "digest", "owner", "object_type"] } });
        Ok(g.call("sui.rpc.v2.LedgerService/GetObject", &body, CALL_TIMEOUT).await?["object"].clone())
    }

    /// True when a Move struct type has the store ability, which `TransferObjects` needs.
    async fn has_store(&self, g: &Grpc<'_>, ty: &str) -> Result<bool> {
        let base = ty.split('<').next().unwrap_or(ty);
        let parts: Vec<&str> = base.split("::").collect();
        let [package, module, name] = parts[..] else { return Err(Error::new(ErrorCode::Internal, format!("{ty} is not a struct type"))) };
        let d = g.call("sui.rpc.v2.MovePackageService/GetDatatype", &json!({ "packageId": package, "moduleName": module, "name": name }), CALL_TIMEOUT).await?;
        Ok(d["datatype"]["abilities"].as_array().is_some_and(|a| a.iter().any(|x| x == "STORE")))
    }

    /// True when `who` owns any object on the upstream (Sui Mainnet).
    async fn owns_mainnet_objects(&self, g: &Grpc<'_>, who: &str) -> Result<bool> {
        let query = format!("{{ address(address: \"{who}\") {{ objects(first: 1) {{ nodes {{ address }} }} }} }}");
        let data = g.upstream_graphql(&query, Duration::from_secs(30)).await?;
        Ok(data["address"]["objects"]["nodes"].as_array().is_some_and(|n| !n.is_empty()))
    }

    async fn gas_price(&self, g: &Grpc<'_>) -> Result<u64> {
        let epoch = g.call("sui.rpc.v2.LedgerService/GetEpoch", &json!({ "readMask": { "paths": ["reference_gas_price"] } }), CALL_TIMEOUT).await?;
        u64_field(&epoch["epoch"]["referenceGasPrice"]).ok_or_else(|| Error::new(ErrorCode::EngineFailed, "the fork did not return a reference gas price"))
    }

    /// Runs a programmable transaction as `sender` with no signature, its gas
    /// paid from the funder's largest coin. Returns the executed transaction
    /// and the gas coin's ID.
    async fn run_as(&self, on: &On<'_>, sender: &str, inputs: Vec<Value>, commands: Vec<Value>) -> Result<(Value, String)> {
        let (g, profile, fork) = (on.g, on.profile, on.fork);
        let funder = profile.funder.as_deref().ok_or_else(|| Error::new(ErrorCode::Unsupported, format!("{} names no funder to pay gas", profile.key)))?;
        let (coin, _) = self.funder_coin(g, funder, fork).await?;
        let gas_coin = coin["objectId"].as_str().unwrap_or_default().to_string();
        let price = self.gas_price(g).await?;
        let body = json!({
            "transaction": {
                "version": 1,
                "sender": sender,
                "kind": { "kind": "PROGRAMMABLE_TRANSACTION", "programmableTransaction": { "inputs": inputs, "commands": commands } },
                "gasPayment": { "objects": [coin], "owner": funder, "price": price.to_string(), "budget": SEND_BUDGET.to_string() },
                "expiration": { "kind": "NONE" },
            },
            "signatures": [],
            "readMask": { "paths": ["digest", "checkpoint", "effects.status", "effects.gas_used", "effects.changed_objects"] },
        });
        let out = g.call(EXECUTE, &body, CALL_TIMEOUT).await?;
        Ok((out["transaction"].clone(), gas_coin))
    }

    /// The inputs and the one MoveCall for `send --to package::module::function<T> args...`.
    async fn move_call(&self, g: &Grpc<'_>, fork: &str, sender: &str, to: &str, args: &[String]) -> Result<(Vec<Value>, Vec<Value>)> {
        let target = ptb::parse_target(to)?;
        let f = g
            .call("sui.rpc.v2.MovePackageService/GetFunction", &json!({ "packageId": target.package, "moduleName": target.module, "name": target.function }), CALL_TIMEOUT)
            .await?;
        let params: Vec<&Value> = f["function"]["parameters"].as_array().into_iter().flatten().filter(|p| !ptb::is_tx_context(p)).collect();
        if params.len() != args.len() {
            let types: Vec<String> = params.iter().map(|p| p["body"]["typeName"].as_str().map(|t| t.rsplit("::").next().unwrap_or(t).to_string()).unwrap_or_else(|| p["body"]["type"].as_str().unwrap_or("?").to_lowercase())).collect();
            return Err(Error::new(ErrorCode::Usage, format!("{}::{} takes {} arguments ({}), {} given", target.module, target.function, params.len(), types.join(", "), args.len())));
        }
        let mut inputs = Vec::new();
        for (p, arg) in params.iter().zip(args) {
            inputs.push(match ptb::param(p, arg, &target.type_args)? {
                ptb::Param::Pure(bytes) => ptb::pure_input(&bytes),
                ptb::Param::Object { id, mutable } => {
                    let o = self.get_object(g, &id).await?;
                    check_owner(&o, sender, fork)?;
                    object_input(&o, mutable)?
                }
                ptb::Param::Receiving { id } => {
                    let o = self.get_object(g, &id).await?;
                    json!({ "kind": "RECEIVING", "objectId": o["objectId"], "version": o["version"], "digest": o["digest"] })
                }
            });
        }
        let call = json!({ "moveCall": {
            "package": target.package, "module": target.module, "function": target.function,
            "typeArguments": target.type_args, "arguments": (0..inputs.len()).map(ptb::input_arg).collect::<Vec<_>>(),
        } });
        Ok((inputs, vec![call]))
    }

    /// The inputs and commands of a raw PTB (`send --data`). Object inputs given
    /// by ID alone get their version and digest, or initial shared version, from the fork.
    async fn raw_ptb(&self, g: &Grpc<'_>, fork: &str, sender: &str, data: &str) -> Result<(Vec<Value>, Vec<Value>)> {
        let usage = || Error::new(ErrorCode::Usage, "--data must be a programmable transaction as protobuf JSON: {\"inputs\": [...], \"commands\": [...]}");
        let ptb: Value = serde_json::from_str(data).map_err(|_| usage())?;
        let (Some(inputs), Some(commands)) = (ptb["inputs"].as_array(), ptb["commands"].as_array()) else { return Err(usage()) };
        let mut resolved = Vec::new();
        for i in inputs {
            let kind = i["kind"].as_str().unwrap_or("");
            let resolved_input = match (kind, i["objectId"].as_str(), i.get("version")) {
                ("IMMUTABLE_OR_OWNED" | "SHARED", Some(id), _) => {
                    let o = self.get_object(g, &parse_address(id)?).await?;
                    check_owner(&o, sender, fork)?;
                    if i.get("version").is_some() { i.clone() } else { object_input(&o, i["mutable"].as_bool().unwrap_or(true))? }
                }
                ("RECEIVING", Some(id), None) => {
                    let o = self.get_object(g, &parse_address(id)?).await?;
                    json!({ "kind": "RECEIVING", "objectId": o["objectId"], "version": o["version"], "digest": o["digest"] })
                }
                _ => i.clone(),
            };
            resolved.push(resolved_input);
        }
        Ok((resolved, commands.clone()))
    }

    /// The coins of `coin_type` the fork indexes for `owner`, as owned inputs with their balances.
    async fn fork_coins(&self, g: &Grpc<'_>, owner: &str, coin_type: &str) -> Result<Vec<(Value, u64)>> {
        let body = json!({ "owner": owner, "objectType": format!("0x2::coin::Coin<{coin_type}>"), "pageSize": 50, "readMask": { "paths": ["object_id", "version", "digest", "balance"] } });
        Ok(g.call("sui.rpc.v2.StateService/ListOwnedObjects", &body, CALL_TIMEOUT).await?["objects"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|o| (json!({ "kind": "IMMUTABLE_OR_OWNED", "objectId": o["objectId"], "version": o["version"], "digest": o["digest"] }), u64_field(&o["balance"]).unwrap_or(0)))
            .collect())
    }

    /// Moves `amount` of `coins` (owned by `sender`, as inputs) to `to` as `sender`: merge, split, transfer.
    async fn move_coins(&self, on: &On<'_>, sender: &str, coins: Vec<Value>, amount: u64, to: &str) -> Result<()> {
        if coins.is_empty() {
            return Err(Error::new(ErrorCode::ReadbackMismatch, format!("{sender} holds no coins this fork can list, so nothing can be moved from it")));
        }
        let n = coins.len();
        let mut inputs = coins;
        inputs.push(ptb::pure_input(&amount.to_le_bytes()));
        inputs.push(ptb::pure_input(&address_bytes(to)));
        let mut commands = Vec::new();
        if n > 1 {
            commands.push(json!({ "mergeCoins": { "coin": ptb::input_arg(0), "coinsToMerge": (1..n).map(ptb::input_arg).collect::<Vec<_>>() } }));
        }
        let split = commands.len();
        commands.push(json!({ "splitCoins": { "coin": ptb::input_arg(0), "amounts": [ptb::input_arg(n)] } }));
        commands.push(json!({ "transferObjects": { "objects": [{ "kind": "RESULT", "result": split }], "address": ptb::input_arg(n + 1) } }));
        let (tx, _) = self.run_as(on, sender, inputs, commands).await?;
        let status = &tx["effects"]["status"];
        if status["success"] != json!(true) {
            return Err(Error::new(ErrorCode::RpcError, format!("moving coins from {sender} failed on the fork: {}", status["error"]["description"])));
        }
        Ok(())
    }

    /// Lowers `who`'s balance of `coin_type` by `excess`, sending it to the funder as `who`.
    async fn lower_balance(&self, on: &On<'_>, who: &str, coin_type: &str, excess: u64) -> Result<()> {
        let coins = self.fork_coins(on.g, who, coin_type).await?.into_iter().map(|(c, _)| c).collect();
        self.move_coins(on, who, coins, excess, on.profile.funder.as_deref().unwrap_or_default()).await
    }

    /// The balance of any coin type the fork indexes for `owner`.
    async fn coin_balance(&self, g: &Grpc<'_>, owner: &str, coin_type: &str) -> Result<u64> {
        let v = g.call("sui.rpc.v2.StateService/GetBalance", &json!({ "owner": owner, "coinType": coin_type }), CALL_TIMEOUT).await?;
        Ok(u64_field(&v["balance"]["balance"]).unwrap_or(0))
    }

    /// Adds `amount` of `coin_type` to `who`: by minting as the TreasuryCap's
    /// owner, or with `from`, by moving coins out of that holder. Returns how.
    async fn add_token(&self, on: &On<'_>, who: &str, coin_type: &str, amount: u64, from: Option<&str>) -> Result<Value> {
        let g = on.g;
        if let Some(holder) = from {
            let holder = parse_address(holder)?;
            let mut coins = self.fork_coins(g, &holder, coin_type).await?;
            // Coins the holder had before the fork point are not indexed on the fork; find them upstream.
            let query = format!("{{ address(address: \"{holder}\") {{ objects(first: 50, filter: {{ type: \"0x2::coin::Coin<{coin_type}>\" }}) {{ nodes {{ address }} }} }} }}");
            let upstream = g.upstream_graphql(&query, Duration::from_secs(30)).await?;
            for id in upstream["address"]["objects"]["nodes"].as_array().into_iter().flatten().filter_map(|n| n["address"].as_str()) {
                if coins.iter().any(|(c, _)| c["objectId"].as_str() == Some(id)) {
                    continue;
                }
                let body = json!({ "objectId": id, "readMask": { "paths": ["object_id", "version", "digest", "owner", "balance"] } });
                // A coin created after the fork point does not exist on the fork.
                let Ok(o) = g.call("sui.rpc.v2.LedgerService/GetObject", &body, CALL_TIMEOUT).await.map(|v| v["object"].clone()) else { continue };
                if o["owner"]["address"].as_str() == Some(holder.as_str()) {
                    coins.push((json!({ "kind": "IMMUTABLE_OR_OWNED", "objectId": o["objectId"], "version": o["version"], "digest": o["digest"] }), u64_field(&o["balance"]).unwrap_or(0)));
                }
            }
            let held: u64 = coins.iter().map(|(_, b)| b).sum();
            if held < amount {
                return Err(Error::new(ErrorCode::Usage, format!("{holder} holds {held} units of {coin_type} on the fork, less than the {amount} needed")).next("name a larger holder with --from, or leave --from out to mint"));
            }
            self.move_coins(on, &holder, coins.into_iter().map(|(c, _)| c).collect(), amount, who).await?;
            return Ok(json!({ "from": holder }));
        }
        let info = g.call("sui.rpc.v2.StateService/GetCoinInfo", &json!({ "coinType": coin_type }), CALL_TIMEOUT).await?;
        let no_cap = |why: String| {
            Error::new(ErrorCode::Unsupported, format!("cannot mint {coin_type}: {why}")).next("chainsim set <name> token <address> <coin type> <amount> --from <holder> moves it from an address that holds it")
        };
        let cap_id = info["treasury"]["id"].as_str().ok_or_else(|| no_cap("its TreasuryCap was not found".into()))?;
        let cap = self.get_object(g, cap_id).await?;
        let Some(cap_owner) = cap["owner"]["address"].as_str().filter(|_| cap["owner"]["kind"] == json!("ADDRESS")) else {
            return Err(no_cap(format!("its TreasuryCap {cap_id} is {}, not held by an address", owner_text(&cap["owner"]))));
        };
        let inputs = vec![object_input(&cap, true)?, ptb::pure_input(&amount.to_le_bytes()), ptb::pure_input(&address_bytes(who))];
        let call = json!({ "moveCall": { "package": "0x2", "module": "coin", "function": "mint_and_transfer", "typeArguments": [coin_type], "arguments": [ptb::input_arg(0), ptb::input_arg(1), ptb::input_arg(2)] } });
        let (tx, _) = self.run_as(on, cap_owner, inputs, vec![call]).await?;
        let status = &tx["effects"]["status"];
        if status["success"] != json!(true) {
            return Err(Error::new(ErrorCode::RpcError, format!("minting {coin_type} as the TreasuryCap owner failed on the fork: {}", status["error"]["description"])));
        }
        Ok(json!({ "minted": { "treasuryCap": cap_id, "capOwner": cap_owner } }))
    }

    /// The funder's largest SUI coin on the fork, as a gas object reference.
    async fn funder_coin(&self, g: &Grpc<'_>, funder: &str, fork: &ForkHandle) -> Result<(Value, u64)> {
        let body = json!({ "owner": funder, "objectType": "0x2::coin::Coin<0x2::sui::SUI>", "pageSize": 50, "readMask": { "paths": ["object_id", "version", "digest", "balance"] } });
        let v = g.call("sui.rpc.v2.StateService/ListOwnedObjects", &body, CALL_TIMEOUT).await?;
        v["objects"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|o| Some((json!({ "objectId": o["objectId"], "version": o["version"], "digest": o["digest"] }), u64_field(&o["balance"])?)))
            .max_by_key(|(_, b)| *b)
            .ok_or_else(|| too_old(&fork.name, fork.fork_point.number))
    }

    /// Sends `amount` MIST from the profile's funder to `to` by impersonation:
    /// a transaction with no signatures, which sui-fork executes as its sender.
    async fn send_from_funder(&self, g: &Grpc<'_>, profile: &Profile, fork: &ForkHandle, to: &str, amount: u64) -> Result<()> {
        let funder = profile.funder.as_deref().ok_or_else(|| Error::new(ErrorCode::Unsupported, format!("{} names no funder address", profile.key)).next("chainsim describe sui-mainnet"))?;
        let (coin, coin_balance) = self.funder_coin(g, funder, fork).await?;
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

    /// An object with its BCS contents, for set object.
    async fn object_contents(&self, g: &Grpc<'_>, id: &str) -> Result<Value> {
        let body = json!({ "objectId": id, "readMask": { "paths": ["object_id", "version", "digest", "owner", "object_type", "contents"] } });
        Ok(g.call("sui.rpc.v2.LedgerService/GetObject", &body, CALL_TIMEOUT).await?["object"].clone())
    }

    /// Fetches the layout of every datatype `ty` reaches.
    async fn layouts(&self, g: &Grpc<'_>, ty: &object::Ty) -> Result<object::Layouts> {
        let mut layouts = object::Layouts::new();
        loop {
            let missing = object::missing(ty, &layouts)?;
            if missing.is_empty() {
                return Ok(layouts);
            }
            for name in missing {
                let parts: Vec<&str> = name.split("::").collect();
                let d = g.call("sui.rpc.v2.MovePackageService/GetDatatype", &json!({ "packageId": parts[0], "moduleName": parts[1], "name": parts[2] }), CALL_TIMEOUT).await?;
                layouts.insert(name, object::layout_of(&d["datatype"]));
            }
        }
    }

    /// set object: replaces an object's contents through ForkingService/SetObject,
    /// from `--field` changes to its decoded fields or from `--bcs`, and reads them back.
    async fn set_object(&self, g: &Grpc<'_>, fork: &str, id: &str, fields: &[String], bcs: Option<&str>) -> Result<(Value, Value)> {
        let id = parse_address(id)?;
        let current = self.object_contents(g, &id).await?;
        let ty_text = current["objectType"].as_str().unwrap_or("").to_string();
        let refuse = |why: String, next: String| Err(Error::new(ErrorCode::Unsupported, format!("object {id} cannot be set: {why}")).next(next));
        let Some(contents) = current["contents"]["value"].as_str().filter(|_| ty_text != "package") else {
            return refuse("it is a package, and a package's code cannot change".into(), format!("not supported: call its functions with chainsim send {fork} --as <address> --to '<package>::<module>::<function>'"));
        };
        let ty = object::parse_type(&ty_text)?;
        if let object::Ty::Datatype { name, args } = &ty {
            if *name == format!("0x{:0>64}::coin::Coin", "2") {
                let coin_type = args.first().map(object::Ty::to_string).unwrap_or_default();
                let owner = current["owner"]["address"].as_str().unwrap_or("<owner>");
                return refuse("it is a coin, and the fork's balance index changes only through transactions".into(), format!("chainsim set {fork} token {owner} {coin_type} <amount>"));
            }
        }
        let layouts = self.layouts(g, &ty).await?;
        let mut node = object::decode(&ty, &units::unbase64(contents)?, &layouts)?;
        let (new_bytes, changed) = match bcs {
            Some(hex) => {
                let bytes = ptb::hex(hex.trim_start_matches("0x")).ok_or_else(|| Error::new(ErrorCode::Usage, "--bcs must be hex bytes, such as 0x0a1b..."))?;
                if bytes.get(..32) != Some(&address_bytes(&id)[..]) {
                    return Err(Error::new(ErrorCode::Usage, format!("--bcs must start with the object's own id {id}")).next(format!("chainsim call {fork} LedgerService/GetObject '{{\"objectId\": \"{id}\", \"readMask\": {{\"paths\": [\"contents\"]}}}}' shows the current contents, which start with it")));
                }
                // Refuse bytes that do not decode as the object's type before the engine takes them.
                object::decode(&ty, &bytes, &layouts).map_err(|e| Error::new(ErrorCode::Usage, format!("--bcs does not decode as {ty}: {}", e.message)))?;
                (bytes, json!("bcs"))
            }
            None => {
                let mut paths = Vec::new();
                for f in fields {
                    let (path, value) = object::split_field(f)?;
                    object::set_path(&mut node, path, value, &layouts)?;
                    paths.push(json!(path));
                }
                (object::encode(&node), Value::Array(paths))
            }
        };
        let set = g
            .call("sui.forking.v1alpha.ForkingService/SetObject", &json!({ "objectId": id, "contents": base64(&new_bytes) }), CALL_TIMEOUT)
            .await
            .map_err(|e| if e.message.contains("SetObject") && e.message.to_lowercase().contains("not") { Error::new(ErrorCode::Unsupported, format!("this fork's engine has no ForkingService/SetObject: {}", e.message)).next(format!("chainsim down {fork}, then chainsim up sui-mainnet, which runs chainsim's patched engine")) } else { e })?;
        let read = self.object_contents(g, &id).await?;
        let read_bytes = units::unbase64(read["contents"]["value"].as_str().unwrap_or(""))?;
        if read_bytes != new_bytes {
            return Err(Error::new(ErrorCode::ReadbackMismatch, format!("object {id} reads back other contents than were set")));
        }
        let fields_json = object::to_json(&object::decode(&ty, &read_bytes, &layouts)?);
        let view = json!({
            "kind": "object", "object": id, "type": ty_text, "owner": read["owner"], "changed": changed,
            "version": read["version"], "digest": read["digest"], "checkpoint": set["checkpointSequenceNumber"], "fields": fields_json,
        });
        Ok((view, json!({ "kind": "set-object", "object": id, "type": ty_text, "changed": changed })))
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
        let sent = self.send_from_funder(g, profile, handle, ST_RECEIVER, unit).await.is_ok();
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

/// The funder holds no coins on the fork: its coins were not seeded, because
/// the upstream keeps owned objects only for about the last hour of checkpoints.
pub fn too_old(name: &str, fork_point: u64) -> Error {
    Error::new(
        ErrorCode::Unsupported,
        format!("fund needs a fork point within about an hour of the upstream head, and checkpoint {fork_point} is older: the upstream GraphQL keeps owned objects only for about the last hour, so the funder's coins were not seeded at up"),
    )
    .next(format!("chainsim down {name}, then chainsim up sui-mainnet --name {name} without --block, which forks from the upstream head"))
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
            return Err(Error::new(ErrorCode::Unsupported, format!("{} funds SUI only", profile.key)).next(format!("chainsim set {name} token {who} <coin type> <amount> sets a token balance")));
        }
        let Some(native) = &req.native else {
            return Err(Error::new(ErrorCode::Usage, "nothing to fund: pass --native <amount>"));
        };
        let decimals = profile.native.as_ref().map_or(9, |n| n.decimals);
        let target = u64::try_from(parse_amount(native, decimals)?).map_err(|_| Error::new(ErrorCode::Usage, format!("{native} SUI is more than a u64 of MIST")))?;

        // An address with any owned object on mainnet may hold real assets.
        if self.owns_mainnet_objects(&g, &who).await? {
            return Err(Error::new(ErrorCode::MainnetAddress, format!("{who} owns objects on Sui Mainnet; fund only addresses of test keys that hold no mainnet assets")).next("use a fresh test address"));
        }

        let current = self.balance(&g, &who).await?;
        if current > target {
            return Err(Error::new(ErrorCode::Unsupported, format!("{who} holds {} SUI, more than the {native} SUI asked; on Sui, fund only raises a balance", format_amount(current as u128, decimals)))
                .next(format!("chainsim revert {name} <snapshot>, or fund a fresh address")));
        }
        if current < target {
            self.send_from_funder(&g, &profile, &handle, &who, target - current).await?;
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

    async fn send(&self, name: &str, req: &SendRequest) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        let g = self.grpc(&handle, &profile);
        let sender = parse_address(&req.from)?;
        let move_form = "chainsim send <name> --as <address> --to 'package::module::function<T...>' <arg>..., or --data '<PTB JSON>'";
        if req.sig.is_some() || req.value.is_some() {
            return Err(Error::new(ErrorCode::Usage, "on Sui, --to names a Move function and the arguments follow it; --sig and --value are EVM-only").next(move_form));
        }
        let (inputs, commands, to) = match &req.data {
            Some(_) if !req.args.is_empty() => return Err(Error::new(ErrorCode::Usage, "pass a Move function with its arguments, or --data alone").next(move_form)),
            Some(data) => {
                let (i, c) = self.raw_ptb(&g, name, &sender, data).await?;
                (i, c, "ptb".to_string())
            }
            None => {
                let (i, c) = self.move_call(&g, name, &sender, &req.to, &req.args).await?;
                (i, c, req.to.trim().to_string())
            }
        };
        let on = On { g: &g, profile: &profile, fork: &handle };
        let (tx, gas_coin) = self.run_as(&on, &sender, inputs, commands).await?;
        let mut out = ptb::summarize(&tx, &gas_coin);
        let ok = out["status"] == json!(1);
        let entry = json!({ "kind": "send-as", "from": sender, "to": to, "digest": out["digest"], "status": out["status"], "gasPaidBy": profile.funder, "at": now_secs() });
        chainsim_core::mods::record(&self.docker, name, &entry).await?;
        out["from"] = json!(sender);
        out["to"] = json!(to);
        out["gasPaidBy"] = json!(profile.funder);
        out["note"] = json!(chainsim_core::mods::NOTE);
        if !ok {
            let e = &out["error"];
            let message = match e.get("abort") {
                Some(a) => format!("the transaction aborted in {}::{} with code {}", a["module"].as_str().unwrap_or("?"), a["function"].as_str().unwrap_or("?"), a["code"].as_str().unwrap_or("?")),
                None => format!("the transaction failed: {}", e["description"].as_str().unwrap_or("unknown error")),
            };
            return Err(Error::new(ErrorCode::TxReverted, message).next("data.error holds the Move abort location and code; the failed transaction still used gas").data(out));
        }
        Ok(out)
    }

    async fn set(&self, name: &str, req: &SetRequest) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        let g = self.grpc(&handle, &profile);
        let on = On { g: &g, profile: &profile, fork: &handle };
        let (mut out, mut entry) = match &req.kind {
            SetKind::Balance => {
                let who = parse_address(&req.address)?;
                if profile.funder.as_deref().map(parse_address).transpose()?.as_deref() == Some(who.as_str()) {
                    return Err(Error::new(ErrorCode::Usage, format!("{who} is the funder, which pays for every send and set on this fork")));
                }
                let decimals = profile.native.as_ref().map_or(9, |n| n.decimals);
                let target = u64::try_from(parse_amount(&req.value, decimals)?).map_err(|_| Error::new(ErrorCode::Usage, format!("{} SUI is more than a u64 of MIST", req.value)))?;
                let current = self.balance(&g, &who).await?;
                if current < target {
                    self.send_from_funder(&g, &profile, &handle, &who, target - current).await?;
                } else if current > target {
                    self.lower_balance(&on, &who, "0x2::sui::SUI", current - target).await?;
                }
                let read = self.balance(&g, &who).await?;
                if read != target {
                    return Err(Error::new(ErrorCode::ReadbackMismatch, format!("{who} reads {} SUI after setting {}", format_amount(read as u128, decimals), req.value)));
                }
                let amount = format_amount(read as u128, decimals);
                let mut view = json!({ "kind": "balance", "address": who, "symbol": "SUI", "amount": amount, "raw": read.to_string() });
                if self.owns_mainnet_objects(&g, &who).await? {
                    view["warning"] = json!("this address owns objects on Sui Mainnet; the fork counts only coins it has seen, so coins the address held before the fork point are not in this balance but can still be spent");
                }
                (
                    view,
                    json!({ "kind": "set-balance", "address": who, "symbol": "SUI", "amount": amount }),
                )
            }
            SetKind::Token { coin_type, from } => {
                let who = parse_address(&req.address)?;
                let info = g.call("sui.rpc.v2.StateService/GetCoinInfo", &json!({ "coinType": coin_type }), CALL_TIMEOUT).await?;
                let decimals = info["metadata"]["decimals"].as_u64().map_or(0, |d| d as u8);
                let target = u64::try_from(parse_amount(&req.value, decimals)?).map_err(|_| Error::new(ErrorCode::Usage, format!("{} is more than a u64 of {coin_type}", req.value)))?;
                let current = self.coin_balance(&g, &who, coin_type).await?;
                let source = if current < target {
                    self.add_token(&on, &who, coin_type, target - current, from.as_deref()).await?
                } else if current > target {
                    self.lower_balance(&on, &who, coin_type, current - target).await?;
                    json!({ "returnedTo": profile.funder })
                } else {
                    Value::Null
                };
                let read = self.coin_balance(&g, &who, coin_type).await?;
                if read != target {
                    return Err(Error::new(ErrorCode::ReadbackMismatch, format!("{who} reads {} of {coin_type} after setting {}", format_amount(read as u128, decimals), req.value)));
                }
                let amount = format_amount(read as u128, decimals);
                let mut view = json!({ "kind": "token", "address": who, "coinType": coin_type, "decimals": decimals, "amount": amount, "raw": read.to_string(), "source": source });
                if self.owns_mainnet_objects(&g, &who).await? {
                    view["warning"] = json!("this address owns objects on Sui Mainnet; the fork counts only coins it has seen, so coins the address held before the fork point are not in this balance but can still be spent");
                }
                (view, json!({ "kind": "set-token", "address": who, "coinType": coin_type, "amount": amount, "source": source }))
            }
            SetKind::Owner => {
                let id = parse_address(&req.address)?;
                let to = parse_address(&req.value)?;
                let object = self.get_object(&g, &id).await?;
                let ty = object["objectType"].as_str().unwrap_or("").to_string();
                let cannot = |why: &str, next: &str| Err(Error::new(ErrorCode::Unsupported, format!("object {id} cannot change owner: {why}")).next(next.to_string()));
                let from = match object["owner"]["kind"].as_str() {
                    Some("ADDRESS") => object["owner"]["address"].as_str().unwrap_or_default().to_string(),
                    Some("SHARED" | "CONSENSUS_ADDRESS") => return cannot("it is shared, so it has no owner", "change it through its own module with chainsim send --as <an address its module allows>"),
                    Some("IMMUTABLE") => return cannot("it is immutable", "an immutable object never changes"),
                    _ => return cannot(&format!("it is owned by another object ({})", owner_text(&object["owner"])), "move the parent object, or change it through its module with chainsim send --as"),
                };
                if !ty.is_empty() && !self.has_store(&g, &ty).await? {
                    return cannot(&format!("{ty} has key but not store, so only its own module can transfer it"), &format!("chainsim send {name} --as {from} --to '<package>::<module>::<its transfer function>' {id} {to}"));
                }
                if from != to {
                    let inputs = vec![object_input(&object, true)?, ptb::pure_input(&address_bytes(&to))];
                    let cmd = json!({ "transferObjects": { "objects": [ptb::input_arg(0)], "address": ptb::input_arg(1) } });
                    let (tx, _) = self.run_as(&on, &from, inputs, vec![cmd]).await?;
                    let status = &tx["effects"]["status"];
                    if status["success"] != json!(true) {
                        return Err(Error::new(ErrorCode::RpcError, format!("transferring {id} as {from} failed on the fork: {}", status["error"]["description"])));
                    }
                }
                let read = self.get_object(&g, &id).await?;
                let owner = read["owner"]["address"].as_str().unwrap_or_default();
                if owner != to {
                    return Err(Error::new(ErrorCode::ReadbackMismatch, format!("object {id} reads owner {} after setting {to}", owner_text(&read["owner"]))));
                }
                (
                    json!({ "kind": "owner", "object": id, "type": ty, "previousOwner": from, "owner": owner, "version": read["version"] }),
                    json!({ "kind": "set-owner", "object": id, "type": ty, "previousOwner": from, "owner": owner }),
                )
            }
            SetKind::Object { fields, bcs } => self.set_object(&g, name, &req.address, fields, bcs.as_deref()).await?,
            SetKind::Storage { .. } | SetKind::Code => {
                return Err(Error::unsupported(
                    "set storage and set code on Sui",
                    "Sui has no storage slots or contract code: chainsim set <name> object <id> --field <name>=<value> changes an object's fields, and chainsim send --as calls its module as its owner or admin",
                ));
            }
        };
        entry["at"] = json!(now_secs());
        chainsim_core::mods::record(&self.docker, name, &entry).await?;
        out["note"] = json!(chainsim_core::mods::NOTE);
        Ok(out)
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
            return Err(Error::unsupported("call --from", "chainsim call <name> TransactionExecutionService/ExecuteTransaction '<request>' with an empty signatures list acts as the transaction's sender"));
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
    fn vendored_proto_lists_four_rpcs() {
        assert_eq!(grpc::rpc_names(FORKING_PROTO), vec!["AdvanceCheckpoint", "AdvanceClock", "GetStatus", "SetObject"]);
        assert!(FORKING_PROTO.contains("SPDX-License-Identifier: Apache-2.0") && FORKING_PROTO.contains("tag mainnet-v1.81.1"));
    }

    #[test]
    fn profile_names_the_engine_source_from_the_lock() {
        let p = profile::get("sui-mainnet").unwrap();
        let source = EngineLock::embedded().unwrap().engine(&p.engine).unwrap().build.as_ref().unwrap().source.clone().unwrap();
        assert!(FORKING_PROTO.contains(&source.commit[..12]) && FORKING_PROTO.contains(&source.branch));
        let first = &p.limits[0];
        for part in [&source.repo, &source.branch, &source.commit, &source.upstream] {
            assert!(first.contains(part.as_str()), "the first limit does not name {part}");
        }
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
        assert!(snapshot_script("s1").contains("trap 'rm -f $D/hold' EXIT"));
    }

    #[test]
    fn owned_inputs_must_belong_to_the_sender() {
        let cap = json!({ "objectId": "0xef", "owner": { "kind": "ADDRESS", "address": "0xcf" } });
        assert!(check_owner(&cap, "0xcf", "f").is_ok());
        let e = check_owner(&cap, "0x5e", "f").unwrap_err();
        assert_eq!(e.code, ErrorCode::Usage);
        assert_eq!(e.next.as_deref(), Some("rerun with --as 0xcf, or chainsim set f owner 0xef 0x5e first"));
        assert!(check_owner(&json!({ "objectId": "0x6", "owner": { "kind": "SHARED", "version": "1" } }), "0x5e", "f").is_ok());
        assert!(check_owner(&json!({ "objectId": "0x2", "owner": { "kind": "IMMUTABLE" } }), "0x5e", "f").is_ok());
        assert_eq!(check_owner(&json!({ "objectId": "0xc", "owner": { "kind": "OBJECT", "address": "0xp" } }), "0x5e", "f").unwrap_err().code, ErrorCode::Usage);
    }

    #[test]
    fn object_inputs_resolve_by_owner_kind() {
        let owned = json!({ "objectId": "0xa", "version": "7", "digest": "D", "owner": { "kind": "ADDRESS", "address": "0xb" } });
        assert_eq!(object_input(&owned, true).unwrap(), json!({ "kind": "IMMUTABLE_OR_OWNED", "objectId": "0xa", "version": "7", "digest": "D" }));
        let frozen = json!({ "objectId": "0x2", "version": "1", "digest": "P", "owner": { "kind": "IMMUTABLE" } });
        assert_eq!(object_input(&frozen, false).unwrap()["kind"], "IMMUTABLE_OR_OWNED");
        // A shared object goes by its initial shared version, not its current one.
        let shared = json!({ "objectId": "0xc", "version": "1043929494", "digest": "S", "owner": { "kind": "SHARED", "version": "1574190" } });
        assert_eq!(object_input(&shared, true).unwrap(), json!({ "kind": "SHARED", "objectId": "0xc", "version": "1574190", "mutable": true }));
        assert_eq!(object_input(&shared, false).unwrap()["mutable"], false);
        assert!(object_input(&json!({ "objectId": "0xd", "owner": {} }), true).is_err());
        assert_eq!(owner_text(&json!({ "kind": "SHARED" })), "shared");
        assert_eq!(owner_text(&json!({ "kind": "ADDRESS", "address": "0xb" })), "0xb");
    }

    #[test]
    fn too_old_names_the_fork_point_and_the_fix() {
        let e = too_old("f", 42);
        assert_eq!(e.code, ErrorCode::Unsupported);
        assert!(e.message.contains("checkpoint 42") && e.message.contains("about an hour"), "{}", e.message);
        assert!(e.next.as_deref().is_some_and(|n| n.starts_with("chainsim down f,") && n.contains("without --block")));
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
