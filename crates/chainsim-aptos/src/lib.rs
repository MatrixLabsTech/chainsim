//! The Aptos adapter (experimental): Aptos CLI transaction simulation
//! sessions (`aptos move sim`), driven from outside through Docker exec
//! (design addendum 2026-11-01). The engine container idles; every operation
//! is one CLI call on the session directory in the fork volume, serialized
//! with `flock`, because each call loads and rewrites the session files. The
//! session serves no RPC, so the fork object's `rpcUrl` is null.

pub mod state;
pub mod units;

use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chainsim_core::adapter::selftest_result;
use chainsim_core::control::{redact_urls, Control};
use chainsim_core::docker::{self, ContainerSpec, Docker, ExecOutput, ForkHandle, UPSTREAM_ENV};
use chainsim_core::error::{archive_next, upstream_next};
use chainsim_core::fork::*;
use chainsim_core::lock::EngineLock;
use chainsim_core::profile::{self, Profile, Token};
use chainsim_core::{Adapter, Error, ErrorCode, Result};
use serde_json::{json, Value};

use state::*;
use units::*;

pub const OPERATIONS: &[&str] = &["describe", "up", "down", "ls", "status", "fund", "send", "set", "snapshot", "revert", "time", "mine", "call", "selftest"];

/// The variable that may hold an API key for the upstream fullnode. It is
/// passed to the CLI as `--api-key` and never printed.
pub const API_KEY_ENV: &str = "CHAINSIM_APTOS_API_KEY";
/// The same key inside the engine and sidecar environments.
const KEY_ENV: &str = "CHAINSIM_API_KEY";

/// The fork volume: the session, its snapshots, the lock and the control key.
const DATA: &str = "/data";
const SESSION: &str = "/data/session";
const SNAP: &str = "/data/snap";
const LOCK: &str = "/data/lock";
const PATCH: &str = "/data/patch";
/// The fork's control key: a fresh test key, written to the fork volume at `up`.
const KEY: &str = "/data/key";

const APT_DECIMALS: u8 = 8;
const OCTAS: u64 = 100_000_000;
/// `send --as` tops the sender up with 1 APT for gas when it holds less than 0.1 APT.
const GAS_FLOOR: u64 = OCTAS / 10;
const GAS_TOP_UP: u64 = OCTAS;
const MAX_MINE: u64 = 100;
const ACCOUNT: &str = "0x1::account::Account";
const FA_METADATA: &str = "0x1::fungible_asset::Metadata";
const OBJECT_GROUP: &str = "0x1::object::ObjectGroup";

/// A throwaway receiver for the selftest transfer, never active on mainnet.
const ST_RECEIVER: &str = "0xc5e2";

pub struct AptosAdapter {
    docker: Docker,
    lock: EngineLock,
}

/// One fork's engine container, scrubbing and upstream variable.
struct Session<'a> {
    docker: &'a Docker,
    engine: String,
    control: Control<'a>,
    /// The API key, only to scrub it from messages.
    api_key: Option<String>,
    env: String,
}

/// A transaction the CLI ran on the session.
#[derive(Debug, Clone)]
pub struct TxResult {
    pub hash: String,
    pub success: bool,
    pub vm_status: String,
    pub gas_used: u64,
    pub gas_unit_price: u64,
    pub sequence_number: u64,
}

impl TxResult {
    pub fn from_cli(v: &Value) -> Result<Self> {
        let num = |k: &str| v[k].as_u64().or_else(|| v[k].as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
        let hash = v["transaction_hash"].as_str().ok_or_else(|| Error::new(ErrorCode::EngineFailed, "the CLI returned no transaction hash"))?.to_string();
        Ok(Self {
            hash,
            success: v["success"].as_bool().unwrap_or(false),
            vm_status: v["vm_status"].as_str().unwrap_or("").to_string(),
            gas_used: num("gas_used"),
            gas_unit_price: num("gas_unit_price"),
            sequence_number: num("sequence_number"),
        })
    }

    pub fn summary(&self) -> Value {
        json!({
            "hash": self.hash,
            "status": if self.success { "success" } else { "failure" },
            "vmStatus": self.vm_status,
            "gasUsed": self.gas_used,
            "gasUnitPrice": self.gas_unit_price,
            "sequenceNumber": self.sequence_number,
        })
    }
}

/// Maps a CLI `Error` text to the contract's codes, from the measured texts.
pub fn classify(message: &str, env: &str) -> Error {
    let lower = message.to_lowercase();
    if lower.contains("pruned") {
        return Error::new(ErrorCode::UpstreamUnavailable, format!("the upstream no longer keeps state at the fork version: {message}")).next(archive_next(env));
    }
    if lower.contains("429") || lower.contains("rate limit") || lower.contains("too many requests") {
        return Error::new(ErrorCode::RateLimited, message.to_string()).next(upstream_next(Some(env), true));
    }
    if ["error sending request", "dns error", "error trying to connect", "connection", "timed out", "timeout"].iter().any(|w| lower.contains(w)) {
        return Error::new(ErrorCode::UpstreamUnavailable, message.to_string()).next(upstream_next(Some(env), false));
    }
    Error::new(ErrorCode::RpcError, message.to_string())
}

/// A Move function id with optional type arguments, such as
/// `0x1::primary_fungible_store::transfer<0x1::fungible_asset::Metadata>`.
pub fn parse_function(s: &str) -> Result<(String, Vec<String>)> {
    let bad = || Error::new(ErrorCode::Usage, format!("{s} is not a Move function such as 0x1::aptos_account::transfer or 0x1::coin::transfer<0x1::aptos_coin::AptosCoin>"));
    let s = s.trim();
    let (id, args) = match s.split_once('<') {
        Some((id, rest)) => (id, rest.strip_suffix('>').ok_or_else(bad)?),
        None => (s, ""),
    };
    let parts: Vec<&str> = id.split("::").collect();
    if parts.len() != 3 || parse_address(parts[0]).is_err() || parts[1..].iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')) {
        return Err(bad());
    }
    let mut type_args = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    for (i, c) in args.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                type_args.push(args[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
        if depth < 0 {
            return Err(bad());
        }
    }
    if depth != 0 {
        return Err(bad());
    }
    if !args.trim().is_empty() {
        type_args.push(args[start..].trim().to_string());
    }
    if type_args.iter().any(String::is_empty) {
        return Err(bad());
    }
    Ok((id.to_string(), type_args))
}

/// The engine's environment: the upstream URL and the API key, expanded
/// inside the container so neither is ever in an exec argument.
pub fn engine_env(upstream: &str, api_key: Option<&str>) -> Vec<String> {
    let mut env = vec!["HOME=/tmp".to_string(), "APTOS_DISABLE_TELEMETRY=true".to_string(), format!("{UPSTREAM_ENV}={upstream}")];
    if let Some(k) = api_key {
        env.push(format!("{KEY_ENV}={k}"));
    }
    env
}

/// `sim init` at a version, with the URL and key read from the environment.
pub fn init_script() -> String {
    format!("exec 9>{LOCK}; flock 9; exec aptos move sim init --path {SESSION} --network \"${UPSTREAM_ENV}\" --network-version \"$1\" ${{{KEY_ENV}:+--api-key \"${KEY_ENV}\"}}")
}

/// One GET to the upstream REST API from the control sidecar, with the API
/// key as a bearer token when set. The URL and key never leave the sidecar.
fn upstream_get_script() -> String {
    format!("exec curl -sS --max-time 30 -w '\\n@@chainsim-http-status@@%{{http_code}}' ${{{KEY_ENV}:+-H \"Authorization: Bearer ${KEY_ENV}\"}} \"${{{UPSTREAM_ENV}%/}}$1\"")
}

pub fn snapshot_script(id: &str) -> String {
    format!("set -e; exec 9>{LOCK}; flock 9; mkdir -p {SNAP}; rm -rf {SNAP}/{id}; cp -a {SESSION} {SNAP}/{id}")
}

/// Restores snapshot `old` and keeps it as the fresh snapshot `new`. Exits 4 when `old` does not exist.
pub fn revert_script(old: &str, new: &str) -> String {
    format!("exec 9>{LOCK}; flock 9; [ -d {SNAP}/{old} ] || exit 4; set -e; rm -rf {SESSION}; cp -a {SNAP}/{old} {SESSION}; mv {SNAP}/{old} {SNAP}/{new}")
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

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn ms(t: Instant) -> u64 {
    t.elapsed().as_millis() as u64
}

fn u64_value(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// The chain id number of a CAIP-2 id such as `aptos:1`.
fn chain_number(id: &str) -> Option<u64> {
    id.rsplit_once(':').and_then(|(_, n)| n.parse().ok())
}

impl Session<'_> {
    /// Removes the upstream URL, its host, the API key and every other URL.
    fn scrub(&self, s: &str) -> String {
        let s = match self.api_key.as_deref().filter(|k| !k.is_empty()) {
            Some(k) => s.replace(k, "<api key>"),
            None => s.to_string(),
        };
        self.control.scrub(&s)
    }

    /// Parses the CLI's `{"Result": ...}` or `{"Error": "..."}` output.
    fn parse(&self, out: &ExecOutput, what: &str) -> Result<Value> {
        let v: Option<Value> = serde_json::from_str(out.stdout.trim()).ok();
        match v {
            Some(v) if v.get("Result").is_some() && out.exit_code == 0 => Ok(v["Result"].clone()),
            Some(v) if v.get("Error").is_some() => {
                let message = self.scrub(v["Error"].as_str().unwrap_or("error"));
                let mut e = classify(&message, &self.env);
                e.message = format!("{what}: {}", e.message);
                Err(e)
            }
            _ => {
                let tail: String = self.scrub(out.stderr.trim()).lines().rev().take(4).collect::<Vec<_>>().join(" | ");
                Err(Error::new(ErrorCode::EngineFailed, format!("{what}: the CLI exited {} without a result: {tail}", out.exit_code)))
            }
        }
    }

    async fn exec(&self, argv: Vec<String>) -> Result<ExecOutput> {
        self.docker.exec(&self.engine, &argv).await
    }

    /// One CLI call on the session, under the fork lock.
    async fn cli(&self, what: &str, args: &[&str]) -> Result<Value> {
        let mut argv: Vec<String> = vec!["flock".into(), LOCK.into(), "aptos".into()];
        argv.extend(args.iter().map(|a| a.to_string()));
        let out = self.exec(argv).await?;
        self.parse(&out, what)
    }

    async fn view(&self, function: &str, type_args: &[String], args: &[String]) -> Result<Value> {
        let mut a: Vec<&str> = vec!["move", "view", "--session", SESSION, "--function-id", function];
        if !type_args.is_empty() {
            a.push("--type-args");
            a.extend(type_args.iter().map(String::as_str));
        }
        if !args.is_empty() {
            a.push("--args");
            a.extend(args.iter().map(String::as_str));
        }
        self.cli(&format!("view {function}"), &a).await
    }

    async fn resource(&self, address: &str, tag: &str) -> Result<Value> {
        self.cli(&format!("view-resource {tag}"), &["move", "sim", "view-resource", "--session", SESSION, "--account", address, "--resource", tag]).await
    }

    async fn apt_balance(&self, who: &str) -> Result<u64> {
        let v = self.view("0x1::coin::balance", &["0x1::aptos_coin::AptosCoin".into()], &[format!("address:{who}")]).await?;
        u64_value(&v[0]).ok_or_else(|| Error::new(ErrorCode::RpcError, "coin::balance returned no amount"))
    }

    async fn fa_balance(&self, who: &str, metadata: &str) -> Result<u64> {
        let v = self.view("0x1::primary_fungible_store::balance", &[FA_METADATA.into()], &[format!("address:{who}"), format!("address:{metadata}")]).await?;
        u64_value(&v[0]).ok_or_else(|| Error::new(ErrorCode::RpcError, "primary_fungible_store::balance returned no amount"))
    }

    /// Adds octas to an address's APT with `sim fund` (a direct store write).
    async fn add_apt(&self, who: &str, octas: u64) -> Result<()> {
        self.cli("sim fund", &["move", "sim", "fund", "--session", SESSION, "--account", who, "--amount", &octas.to_string()]).await.map(|_| ())
    }

    async fn timestamp_usecs(&self) -> Result<u64> {
        let v = self.resource("0x1", "0x1::timestamp::CurrentTimeMicroseconds").await?;
        u64_value(&v["microseconds"]).ok_or_else(|| Error::new(ErrorCode::RpcError, "the copy returned no timestamp"))
    }

    async fn block_height(&self) -> Result<u64> {
        let v = self.resource("0x1", "0x1::block::BlockResource").await?;
        u64_value(&v["height"]).ok_or_else(|| Error::new(ErrorCode::RpcError, "the copy returned no block height"))
    }

    async fn new_block(&self, flag: Option<(&str, u64)>) -> Result<()> {
        let value = flag.map(|(_, v)| v.to_string()).unwrap_or_default();
        let mut a = vec!["move", "sim", "new-block", "--session", SESSION];
        if let Some((f, _)) = flag {
            a.extend([f, value.as_str()]);
        }
        self.cli("sim new-block", &a).await.map(|_| ())
    }

    /// The control key's address, from its public key on the fork volume.
    async fn control_address(&self) -> Result<String> {
        let out = self.exec(vec!["cat".into(), format!("{KEY}.pub")]).await?;
        let public = from_hex(out.stdout.trim(), "the control public key").map_err(|_| Error::new(ErrorCode::EngineFailed, "the fork has no control key").next("chainsim down <name> then chainsim up <chain>"))?;
        Ok(format!("0x{}", to_hex(&ed25519_auth_key(&public))))
    }

    /// Writes delta entries into the session under the fork lock.
    async fn write_delta(&self, entries: &[(String, String)]) -> Result<()> {
        let out = self.exec(vec!["sh".into(), "-c".into(), merge_script(SESSION, LOCK, PATCH), "sh".into(), patch_lines(entries)]).await?;
        if out.exit_code != 0 {
            return Err(Error::new(ErrorCode::EngineFailed, format!("writing the session delta failed: {}", self.scrub(out.stderr.trim()))));
        }
        Ok(())
    }

    /// Lets the control key sign for `sender` on the copy: the sender's
    /// Account resource gets the control key's authentication key, and an
    /// address with no Account resource gets a new one.
    async fn impersonate(&self, sender: &str, control: &str) -> Result<()> {
        if sender == control {
            return Ok(());
        }
        let key = address_bytes(control)?;
        let account = self.resource(sender, ACCOUNT).await?;
        let bytes = if account.is_null() { new_account_bytes(sender, &key)? } else { account_bytes(&account, &key)? };
        self.write_delta(&[(resource_key(sender, ACCOUNT)?, state_value_hex(&bytes))]).await
    }

    /// Runs an entry function as `sender`, signed with the control key after
    /// `impersonate`. Tops the sender up for gas first when it is low, and
    /// returns the top-up in octas with the result.
    async fn run_as(&self, sender: &str, function: &str, type_args: &[String], args: &[String]) -> Result<(TxResult, u64)> {
        let control = self.control_address().await?;
        self.impersonate(sender, &control).await?;
        let top_up = if self.apt_balance(sender).await? < GAS_FLOOR { GAS_TOP_UP } else { 0 };
        if top_up > 0 {
            self.add_apt(sender, top_up).await?;
        }
        let mut a: Vec<&str> = vec!["move", "run", "--session", SESSION, "--private-key-file", KEY, "--sender-account", sender, "--function-id", function, "--assume-yes"];
        if !type_args.is_empty() {
            a.push("--type-args");
            a.extend(type_args.iter().map(String::as_str));
        }
        if !args.is_empty() {
            a.push("--args");
            a.extend(args.iter().map(String::as_str));
        }
        let v = self.cli(&format!("run {function}"), &a).await?;
        Ok((TxResult::from_cli(&v)?, top_up))
    }

    /// Moves a fungible asset from `from` to `to` as `from`.
    async fn transfer_fa(&self, from: &str, metadata: &str, to: &str, amount: u64) -> Result<TxResult> {
        let args = [format!("address:{metadata}"), format!("address:{to}"), format!("u64:{amount}")];
        let (tx, _) = self.run_as(from, "0x1::primary_fungible_store::transfer", &[FA_METADATA.into()], &args).await?;
        if !tx.success {
            return Err(Error::new(ErrorCode::TxReverted, format!("the transfer from {from} failed on the copy: {}", tx.vm_status)).data(tx.summary()));
        }
        Ok(tx)
    }

    /// One GET to the upstream REST API through the control sidecar: the HTTP status and body.
    async fn upstream_get(&self, path: &str) -> Result<(u16, Value)> {
        const MARK: &str = "\n@@chainsim-http-status@@";
        let argv = vec!["sh".into(), "-c".into(), upstream_get_script(), "sh".into(), path.to_string()];
        let out = self.docker.exec(&self.control.container, &argv).await?;
        if out.exit_code != 0 {
            return Err(Error::new(ErrorCode::UpstreamUnavailable, format!("the upstream did not answer (curl exit {}): {}", out.exit_code, self.scrub(out.stderr.trim()))).next(upstream_next(Some(&self.env), false)));
        }
        let (body, status) = out.stdout.rsplit_once(MARK).ok_or_else(|| Error::new(ErrorCode::Internal, "the control reply has no status"))?;
        let status: u16 = status.trim().parse().unwrap_or(0);
        if status == 429 {
            return Err(Error::new(ErrorCode::RateLimited, "the upstream rate-limited chainsim (HTTP 429)").next(upstream_next(Some(&self.env), true)));
        }
        let v = serde_json::from_str(body).unwrap_or(Value::Null);
        if status >= 500 || status == 0 {
            return Err(Error::new(ErrorCode::UpstreamUnavailable, format!("the upstream answered HTTP {status}")).next(upstream_next(Some(&self.env), false)));
        }
        Ok((status, v))
    }
}

impl AptosAdapter {
    pub async fn connect() -> Result<Self> {
        Ok(Self { docker: Docker::connect().await?, lock: EngineLock::embedded()? })
    }

    fn api_key() -> Option<String> {
        std::env::var(API_KEY_ENV).ok().filter(|v| !v.trim().is_empty())
    }

    fn session<'a>(&'a self, name: &str, profile: &Profile) -> Session<'a> {
        let secret = profile.upstream_is_secret().then(|| profile.upstream_url());
        Session {
            docker: &self.docker,
            engine: docker::engine_container(name),
            control: Control::new(&self.docker, docker::control_container(name), secret).with_env(&profile.upstream.env),
            api_key: Self::api_key(),
            env: profile.upstream.env.clone(),
        }
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

    /// A profile token by symbol or metadata address, or `unknown_token` (exit 4).
    fn token<'p>(profile: &'p Profile, name: &str, fork: &str) -> Result<&'p Token> {
        let by_address = parse_address(name).ok();
        profile
            .tokens
            .iter()
            .find(|t| t.symbol.eq_ignore_ascii_case(name) || by_address.as_deref().is_some_and(|a| parse_address(&t.address).is_ok_and(|ta| ta == a)))
            .ok_or_else(|| Error::new(ErrorCode::UnknownToken, format!("{} has no token {name}", profile.key)).next(format!("chainsim describe {} lists the tokens of {fork}: {}", profile.key, profile.tokens.iter().map(|t| t.symbol.as_str()).collect::<Vec<_>>().join(", "))))
    }

    fn holder(profile: &Profile) -> Result<String> {
        parse_address(profile.funder.as_deref().ok_or_else(|| Error::new(ErrorCode::Unsupported, format!("{} names no token holder", profile.key)))?)
    }

    async fn start(&self, profile: &Profile, req: &UpRequest) -> Result<Fork> {
        let name = &req.name;
        let upstream = profile.upstream_url();
        let api_key = Self::api_key();
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

        // The control sidecar: curl with the upstream URL and API key in its
        // environment only. It reads the ledger head and keeps the modification log.
        let control_engine = self.lock.engine(profile.control_engine())?;
        let control_image = self.docker.ensure_engine(profile.control_engine(), control_engine).await?;
        self.docker
            .run(&ContainerSpec {
                name: docker::control_container(name),
                image: control_image,
                user: control_engine.user.clone(),
                entrypoint: Some(vec!["sleep".into()]),
                cmd: vec!["infinity".into()],
                env: engine_env(&upstream, api_key.as_deref()),
                labels: HashMap::from([(docker::LABEL_FORK.to_string(), name.clone()), (docker::LABEL_ROLE.to_string(), docker::ROLE_CONTROL.to_string())]),
                network: network.clone(),
                tmpfs: vec![("/tmp".into(), "rw,noexec,nosuid,size=16m".into())],
                memory: 64 << 20,
                ..Default::default()
            })
            .await?;
        let s = self.session(name, profile);

        // The fork point: the asked version, or the upstream head now, so the
        // label records the exact version the session reads.
        let (_, ledger) = s.upstream_get("/").await?;
        let head = u64_value(&ledger["ledger_version"]).ok_or_else(|| Error::new(ErrorCode::UpstreamUnavailable, "the upstream did not return its ledger version").next(upstream_next(Some(&s.env), false)))?;
        let oldest = u64_value(&ledger["oldest_ledger_version"]).unwrap_or(0);
        if let Some(want) = chain_number(&profile.id) {
            if u64_value(&ledger["chain_id"]).or(ledger["chain_id"].as_u64()) != Some(want) {
                return Err(Error::new(ErrorCode::UpstreamUnavailable, format!("the upstream is not chain {want} ({})", profile.name)).next(format!("point {} at a {} fullnode REST URL", profile.upstream.env, profile.name)));
            }
        }
        let version = req.block.unwrap_or(head);
        if version > head {
            return Err(Error::new(ErrorCode::Usage, format!("version {version} is ahead of the upstream head {head}")).next("drop --block to fork at the head"));
        }
        // The session reads state as of the version before the fork version.
        if version <= oldest {
            return Err(Error::new(ErrorCode::UpstreamUnavailable, format!("the upstream keeps versions from {oldest} only, so version {version} is pruned")).next(archive_next(&profile.upstream.env)));
        }

        let engine = self.lock.engine(&profile.engine)?;
        let image = self.docker.ensure_engine(&profile.engine, engine).await?;
        let owner = engine.user.clone().ok_or_else(|| Error::new(ErrorCode::Internal, format!("{} names no uid:gid user in the lock", profile.engine)))?;
        self.docker.ensure_volume_owned(&self.lock, &data_volume(name), name, &owner).await?;
        let labels = HashMap::from([
            (docker::LABEL_FORK.to_string(), name.clone()),
            (docker::LABEL_CHAIN.to_string(), profile.key.clone()),
            (docker::LABEL_ENGINE.to_string(), format!("{}@{}", profile.engine, engine.version)),
            (docker::LABEL_FORK_POINT.to_string(), format!("version:{version}")),
            (docker::LABEL_ROLE.to_string(), docker::ROLE_ENGINE.to_string()),
            (docker::LABEL_NETWORK.to_string(), network.clone()),
            (docker::LABEL_ALIAS.to_string(), alias.clone()),
            (docker::LABEL_NETWORK_CREATED.to_string(), created.to_string()),
        ]);
        self.docker
            .run(&ContainerSpec {
                name: docker::engine_container(name),
                image,
                user: Some(owner),
                entrypoint: Some(vec!["sleep".into()]),
                cmd: vec!["infinity".into()],
                env: engine_env(&upstream, api_key.as_deref()),
                labels,
                network: network.clone(),
                aliases: vec![alias.clone()],
                tmpfs: vec![("/tmp".into(), "rw,nosuid,size=64m".into())],
                memory: docker::ENGINE_MEMORY,
                publish: None,
                mounts: vec![(data_volume(name), DATA.into())],
            })
            .await?;

        let keygen = s.exec(["aptos", "key", "generate", "--key-type", "ed25519", "--output-file", KEY, "--assume-yes"].map(String::from).to_vec()).await?;
        s.parse(&keygen, "generate the control key")?;
        let init = s.exec(vec!["sh".into(), "-c".into(), init_script(), "sh".into(), version.to_string()]).await?;
        s.parse(&init, "sim init")?;
        // The first read through the session: a pruned version fails here.
        let chain = s.resource("0x1", "0x1::chain_id::ChainId").await?;
        if u64_value(&chain["id"]) != chain_number(&profile.id) {
            return Err(Error::new(ErrorCode::EngineFailed, format!("the session reads chain id {} instead of {}", chain["id"], profile.id)));
        }
        let handle = self.docker.find(name).await?.ok_or_else(|| Error::new(ErrorCode::EngineFailed, "the engine container vanished after start"))?;
        Ok(handle.fork(profile, None))
    }

    async fn checks(&self, handle: &ForkHandle, profile: &Profile, s: &Session<'_>) -> Vec<Check> {
        let mut parts = Vec::new();

        let t = Instant::now();
        let chain = s.resource("0x1", "0x1::chain_id::ChainId").await;
        let ok = chain.as_ref().is_ok_and(|c| u64_value(&c["id"]) == chain_number(&profile.id));
        parts.push(Check { part: "chain-id", ok, reason: if ok { "0x1::chain_id::ChainId reads the profile's chain id at the fork version" } else { "0x1::chain_id::ChainId did not read the profile's chain id" }, ms: ms(t) });
        let _ = handle;

        let control = s.control_address().await.ok();
        let t = Instant::now();
        let ok = match &control {
            Some(c) => match s.apt_balance(c).await {
                Ok(before) => s.add_apt(c, OCTAS).await.is_ok() && s.apt_balance(c).await.ok() == Some(before + OCTAS),
                Err(_) => false,
            },
            None => false,
        };
        parts.push(Check { part: "fund", ok, reason: if ok { "sim fund gave the control key 1 APT and it read back" } else { "sim fund of 1 APT to the control key did not read back" }, ms: ms(t) });

        let t = Instant::now();
        let receiver = parse_address(ST_RECEIVER).expect("constant");
        let ok = match &control {
            Some(c) => {
                let before = s.apt_balance(&receiver).await.ok();
                let sent = s.run_as(c, "0x1::aptos_account::transfer", &[], &[format!("address:{receiver}"), format!("u64:{}", OCTAS / 10)]).await;
                let after = s.apt_balance(&receiver).await.ok();
                sent.is_ok_and(|(tx, _)| tx.success) && matches!((before, after), (Some(b), Some(a)) if a == b + OCTAS / 10)
            }
            None => false,
        };
        parts.push(Check { part: "transfer", ok, reason: if ok { "a 0.1 APT transfer from the control key executed and moved the receiver's balance" } else { "a 0.1 APT transfer from the control key failed or did not move the balance" }, ms: ms(t) });

        let t = Instant::now();
        let before = s.timestamp_usecs().await.ok();
        let moved = s.new_block(Some(("--offset-usecs", 1_000_000))).await.is_ok();
        let after = s.timestamp_usecs().await.ok();
        let ok = moved && matches!((before, after), (Some(b), Some(a)) if a > b);
        parts.push(Check { part: "new-block", ok, reason: if ok { "sim new-block moved the on-chain timestamp forward" } else { "sim new-block did not move the on-chain timestamp" }, ms: ms(t) });

        if let Some(symbol) = &profile.selftest.token {
            let t = Instant::now();
            let ok = match (Self::token(profile, symbol, &handle.name), Self::holder(profile), &control) {
                (Ok(token), Ok(holder), Some(c)) => {
                    let unit = 10u64.pow(token.decimals as u32);
                    let before = s.fa_balance(c, &token.address).await.ok();
                    let sent = s.transfer_fa(&holder, &token.address, c, unit).await.is_ok();
                    let after = s.fa_balance(c, &token.address).await.ok();
                    sent && matches!((before, after), (Some(b), Some(a)) if a == b + unit)
                }
                _ => false,
            };
            parts.push(Check { part: "token-fund", ok, reason: if ok { "1 unit of the profile token moved from the holder to the control key by impersonation and read back" } else { "moving 1 unit of the profile token from the holder by impersonation failed or did not read back" }, ms: ms(t) });
        }
        parts
    }
}

#[async_trait]
impl Adapter for AptosAdapter {
    fn family(&self) -> &'static str {
        "aptos"
    }

    fn operations(&self) -> &'static [&'static str] {
        OPERATIONS
    }

    async fn up(&self, profile: &Profile, req: &UpRequest) -> Result<Fork> {
        docker::check_name(&req.name)?;
        if req.publish {
            return Err(Error::unsupported("up --publish on Aptos", "the session engine serves no RPC to publish: read the copy with chainsim call <name> view|resource|ledger"));
        }
        if let Some(h) = self.docker.find(&req.name).await? {
            let taken = |why: String| Error::new(ErrorCode::NameConflict, format!("fork {} already runs {why}", req.name)).next(format!("chainsim down {} first, or pick another --name", req.name));
            if h.chain != profile.key {
                return Err(taken(format!("chain {}", h.chain)));
            }
            if let Some(b) = req.block.filter(|b| *b != h.fork_point.number) {
                return Err(taken(format!("at version {} (asked {b})", h.fork_point.number)));
            }
            if req.network.as_ref().is_some_and(|n| *n != h.network) || req.alias.as_ref().is_some_and(|a| *a != h.alias) {
                return Err(taken(format!("on network {} as {}", h.network, h.alias)));
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
        let head = self.session(name, &profile).block_height().await?;
        Ok(Status { fork: handle.fork(&profile, None), head, uptime_secs: handle.uptime_secs })
    }

    async fn fund(&self, name: &str, req: &FundRequest) -> Result<Funded> {
        let (handle, profile) = self.open(name).await?;
        let s = self.session(name, &profile);
        let who = parse_address(&req.address)?;
        if req.native.is_none() && req.tokens.is_empty() {
            return Err(Error::new(ErrorCode::Usage, "nothing to fund: pass --native <amount> or --token <symbol>:<amount>"));
        }
        let holder = Self::holder(&profile)?;
        if who == holder {
            return Err(Error::new(ErrorCode::Usage, format!("{who} is the token holder that fund draws from")));
        }
        // An account that has sent a transaction on mainnet belongs to a real key.
        let (status, account) = s.upstream_get(&format!("/accounts/{who}?ledger_version={}", handle.fork_point.number)).await?;
        if status == 200 && u64_value(&account["sequence_number"]).is_some_and(|n| n > 0) {
            return Err(Error::new(ErrorCode::MainnetAddress, format!("{who} has sent transactions on Aptos Mainnet; fund only addresses of test keys that hold no mainnet assets")).next("use a fresh test address"));
        }
        let mut balances = Vec::new();
        if let Some(native) = &req.native {
            let target = parse_u64_amount(native, APT_DECIMALS, "APT")?;
            let current = s.apt_balance(&who).await?;
            if current > target {
                return Err(Error::new(ErrorCode::Unsupported, format!("{who} holds {} APT, more than the {native} APT asked; on Aptos, fund only raises a balance", format_amount(current as u128, APT_DECIMALS))).next(format!("chainsim revert {name} <snapshot>, or fund a fresh address")));
            }
            if current < target {
                s.add_apt(&who, target - current).await?;
            }
            let read = s.apt_balance(&who).await?;
            if read != target {
                return Err(Error::new(ErrorCode::ReadbackMismatch, format!("{who} reads back {} APT after funding {native} APT", format_amount(read as u128, APT_DECIMALS))));
            }
            balances.push(Balance { asset: "native".into(), address: None, decimals: APT_DECIMALS, amount: format_amount(read as u128, APT_DECIMALS), raw: read.to_string() });
        }
        for t in &req.tokens {
            let token = Self::token(&profile, &t.token, name)?;
            let target = parse_u64_amount(&t.amount, token.decimals, &token.symbol)?;
            let current = s.fa_balance(&who, &token.address).await?;
            if current > target {
                return Err(Error::new(ErrorCode::Unsupported, format!("{who} holds {} {}, more than the {} asked; on Aptos, fund only raises a balance", format_amount(current as u128, token.decimals), token.symbol, t.amount)).next(format!("chainsim revert {name} <snapshot>, or fund a fresh address")));
            }
            if current < target {
                s.transfer_fa(&holder, &token.address, &who, target - current).await?;
            }
            let read = s.fa_balance(&who, &token.address).await?;
            if read != target {
                return Err(Error::new(ErrorCode::ReadbackMismatch, format!("{who} reads back {} {} after funding {}", format_amount(read as u128, token.decimals), token.symbol, t.amount)));
            }
            balances.push(Balance { asset: token.symbol.clone(), address: Some(token.address.clone()), decimals: token.decimals, amount: format_amount(read as u128, token.decimals), raw: read.to_string() });
        }
        Ok(Funded { address: who, balances })
    }

    async fn send(&self, name: &str, req: &SendRequest) -> Result<Value> {
        let (_, profile) = self.open(name).await?;
        let s = self.session(name, &profile);
        let form = "chainsim send <name> --as <address> --to '<address>::<module>::<function><T...>' <type>:<value>..., such as address:0x1 u64:100";
        if req.sig.is_some() || req.value.is_some() || req.data.is_some() {
            return Err(Error::new(ErrorCode::Usage, "on Aptos, --to names a Move entry function and its arguments follow it; --sig, --data and --value are not used").next(form));
        }
        if req.args.iter().any(|a| !a.contains(':')) {
            return Err(Error::new(ErrorCode::Usage, "each argument is <type>:<value>, as the Aptos CLI takes it").next(form));
        }
        let sender = parse_address(&req.from)?;
        let (function, type_args) = parse_function(&req.to)?;
        let (tx, top_up) = s.run_as(&sender, &function, &type_args, &req.args).await?;
        let mut out = tx.summary();
        out["from"] = json!(sender);
        out["to"] = json!(req.to.trim());
        if top_up > 0 {
            out["gasTopUp"] = json!(format_amount(top_up as u128, APT_DECIMALS));
        }
        let entry = json!({ "kind": "send-as", "from": sender, "to": req.to.trim(), "hash": tx.hash, "status": out["status"], "authKeyReplaced": true, "gasTopUp": out["gasTopUp"], "at": now_secs() });
        chainsim_core::mods::record(&self.docker, name, &entry).await?;
        out["note"] = json!(chainsim_core::mods::NOTE);
        if !tx.success {
            return Err(Error::new(ErrorCode::TxReverted, format!("the transaction failed: {}", tx.vm_status)).next("data.vmStatus names the abort; the failed transaction still used gas").data(out));
        }
        Ok(out)
    }

    async fn set(&self, name: &str, req: &SetRequest) -> Result<Value> {
        let (_, profile) = self.open(name).await?;
        let s = self.session(name, &profile);
        let raise_only = |what: &str| Error::unsupported(&format!("lowering {what} on Aptos"), format!("chainsim revert {name} <snapshot>, or use a fresh address"));
        let (mut out, mut entry) = match &req.kind {
            SetKind::Balance => {
                let who = parse_address(&req.address)?;
                let target = parse_u64_amount(&req.value, APT_DECIMALS, "APT")?;
                let current = s.apt_balance(&who).await?;
                if current > target {
                    return Err(raise_only("an APT balance"));
                }
                if current < target {
                    s.add_apt(&who, target - current).await?;
                }
                let read = s.apt_balance(&who).await?;
                if read != target {
                    return Err(Error::new(ErrorCode::ReadbackMismatch, format!("{who} reads {} APT after setting {}", format_amount(read as u128, APT_DECIMALS), req.value)));
                }
                let amount = format_amount(read as u128, APT_DECIMALS);
                (json!({ "kind": "balance", "address": who, "symbol": "APT", "amount": amount, "raw": read.to_string() }), json!({ "kind": "set-balance", "address": who, "symbol": "APT", "amount": amount }))
            }
            SetKind::Token { coin_type, from } => {
                let who = parse_address(&req.address)?;
                let token = Self::token(&profile, coin_type, name)?;
                let source = match from {
                    Some(f) => parse_address(f)?,
                    None => Self::holder(&profile)?,
                };
                let target = parse_u64_amount(&req.value, token.decimals, &token.symbol)?;
                let current = s.fa_balance(&who, &token.address).await?;
                if current > target {
                    return Err(raise_only("a token balance"));
                }
                if current < target {
                    s.transfer_fa(&source, &token.address, &who, target - current).await?;
                }
                let read = s.fa_balance(&who, &token.address).await?;
                if read != target {
                    return Err(Error::new(ErrorCode::ReadbackMismatch, format!("{who} reads {} {} after setting {}", format_amount(read as u128, token.decimals), token.symbol, req.value)));
                }
                let amount = format_amount(read as u128, token.decimals);
                (
                    json!({ "kind": "token", "address": who, "token": token.symbol, "metadata": token.address, "decimals": token.decimals, "amount": amount, "raw": read.to_string(), "from": source }),
                    json!({ "kind": "set-token", "address": who, "token": token.symbol, "amount": amount, "from": source }),
                )
            }
            SetKind::Resource { struct_tag, bcs } => {
                let who = parse_address(&req.address)?;
                let key = resource_key(&who, struct_tag)?;
                let bytes = from_hex(bcs, "--bcs")?;
                if bytes.is_empty() {
                    return Err(Error::new(ErrorCode::Usage, "--bcs is empty"));
                }
                let tag = canonical_struct_tag(struct_tag)?;
                // A member of a resource group lives inside its group's state value, so a
                // plain write would read back on the session but never reach the VM.
                let group = s.cli("view-resource-group 0x1::object::ObjectGroup", &["move", "sim", "view-resource-group", "--session", SESSION, "--account", &who, "--resource-group", OBJECT_GROUP]).await?;
                if group.as_object().is_some_and(|g| g.contains_key(&tag)) {
                    return Err(Error::unsupported(
                        &format!("set resource on {tag}, a member of {OBJECT_GROUP} at {who},"),
                        format!("chainsim send {name} --as <address> --to '<its module's function>' changes it through its module"),
                    ));
                }
                // Keep the delta, so bytes that do not decode as the resource leave the copy as it was.
                let backup = s.exec(vec!["sh".into(), "-c".into(), format!("exec 9>{LOCK}; flock 9; cp {SESSION}/delta.json {DATA}/delta.bak")]).await?;
                if backup.exit_code != 0 {
                    return Err(Error::new(ErrorCode::EngineFailed, "could not keep a copy of the session delta"));
                }
                s.write_delta(&[(key, state_value_hex(&bytes))]).await?;
                let read = match s.resource(&who, &tag).await {
                    Ok(v) if !v.is_null() => v,
                    other => {
                        s.exec(vec!["sh".into(), "-c".into(), format!("exec 9>{LOCK}; flock 9; mv {DATA}/delta.bak {SESSION}/delta.json")]).await?;
                        let why = other.err().map(|e| e.message).unwrap_or_else(|| "it reads back empty".into());
                        return Err(Error::new(ErrorCode::Usage, format!("--bcs is not a valid {tag}, so the copy was left as it was: {why}")));
                    }
                };
                (
                    json!({ "kind": "resource", "address": who, "structTag": tag, "value": read }),
                    json!({ "kind": "set-resource", "address": who, "structTag": tag, "bytes": bytes.len() }),
                )
            }
            SetKind::Storage { .. } | SetKind::Code | SetKind::Owner | SetKind::Object { .. } => {
                return Err(Error::unsupported(
                    "set storage, code, owner and object on Aptos",
                    "Aptos keeps state in Move resources: chainsim set <name> resource <address> <struct tag> --bcs <hex> replaces one, and chainsim send --as calls its module as any address",
                ));
            }
        };
        entry["at"] = json!(now_secs());
        chainsim_core::mods::record(&self.docker, name, &entry).await?;
        out["note"] = json!(chainsim_core::mods::NOTE);
        Ok(out)
    }

    async fn snapshot(&self, name: &str) -> Result<Snapshot> {
        let (_, profile) = self.open(name).await?;
        let s = self.session(name, &profile);
        let id = snapshot_id();
        let out = s.exec(vec!["sh".into(), "-c".into(), snapshot_script(&id)]).await?;
        match out.exit_code {
            0 => Ok(Snapshot { id }),
            c => Err(Error::new(ErrorCode::EngineFailed, format!("snapshot failed: the copy step exited {c}: {}", s.scrub(out.stderr.trim())))),
        }
    }

    async fn revert(&self, name: &str, id: &str) -> Result<Reverted> {
        let (_, profile) = self.open(name).await?;
        let missing = || Error::new(ErrorCode::SnapshotNotFound, format!("no snapshot {id} on {name}: it was never taken or was already used")).next(format!("chainsim snapshot {name}"));
        if !valid_snapshot_id(id) {
            return Err(missing());
        }
        let s = self.session(name, &profile);
        let fresh = snapshot_id();
        let out = s.exec(vec!["sh".into(), "-c".into(), revert_script(id, &fresh)]).await?;
        match out.exit_code {
            0 => Ok(Reverted { reverted: id.to_string(), snapshot: fresh }),
            4 => Err(missing()),
            c => Err(Error::new(ErrorCode::EngineFailed, format!("revert failed: the copy step exited {c}: {}", s.scrub(out.stderr.trim())))),
        }
    }

    async fn time(&self, name: &str, req: &TimeRequest) -> Result<Value> {
        let (_, profile) = self.open(name).await?;
        let s = self.session(name, &profile);
        let before = s.timestamp_usecs().await?;
        let flag = match req {
            TimeRequest::Advance(secs) => ("--offset-usecs", secs.checked_mul(1_000_000).ok_or_else(|| Error::new(ErrorCode::Usage, "the offset is too large"))?),
            TimeRequest::To(t) => {
                let usecs = t.checked_mul(1_000_000).ok_or_else(|| Error::new(ErrorCode::Usage, "the timestamp is too large"))?;
                if usecs <= before {
                    return Err(Error::new(ErrorCode::Usage, format!("the copy is already at {} s; time only moves forward", before / 1_000_000)));
                }
                ("--timestamp-usecs", usecs)
            }
        };
        s.new_block(Some(flag)).await?;
        let after = s.timestamp_usecs().await?;
        Ok(json!({ "timestamp": after / 1_000_000, "timestampUsecs": after.to_string(), "previous": before / 1_000_000, "height": s.block_height().await? }))
    }

    async fn mine(&self, name: &str, blocks: Option<u64>) -> Result<Value> {
        let (_, profile) = self.open(name).await?;
        let s = self.session(name, &profile);
        let n = blocks.unwrap_or(1);
        if n == 0 || n > MAX_MINE {
            return Err(Error::new(ErrorCode::Usage, format!("mine takes 1 to {MAX_MINE} blocks; each runs one block metadata transaction through the CLI")));
        }
        for _ in 0..n {
            s.new_block(None).await?;
        }
        let ts = s.timestamp_usecs().await?;
        Ok(json!({ "mined": n, "height": s.block_height().await?, "timestamp": ts / 1_000_000, "timestampUsecs": ts.to_string() }))
    }

    async fn call(&self, name: &str, method: &str, params: Value, from: Option<&str>) -> Result<Value> {
        let (handle, profile) = self.open(name).await?;
        if from.is_some() {
            return Err(Error::unsupported("call --from on Aptos", "chainsim send <name> --as <address> --to <function> <args>... runs an entry function as any address"));
        }
        let s = self.session(name, &profile);
        let usage = |m: &str| Error::new(ErrorCode::Usage, m.to_string()).next("chainsim call <name> view '[\"0x1::coin::balance<0x1::aptos_coin::AptosCoin>\", [\"address:0x1\"]]' | resource '[\"0x1\", \"0x1::chain_id::ChainId\"]' | ledger");
        let strings = |v: &Value| -> Option<Vec<String>> { v.as_array().map(|a| a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect()) };
        let out = match method {
            "view" => {
                // [function, args] or {"function", "typeArgs", "args"}; type args may also follow the function in <...>.
                let (f, mut targs, args) = match &params {
                    Value::Array(a) if !a.is_empty() => (a[0].as_str().unwrap_or("").to_string(), Vec::new(), a.get(1).and_then(strings).unwrap_or_default()),
                    Value::Object(o) => (o.get("function").and_then(Value::as_str).unwrap_or("").to_string(), o.get("typeArgs").and_then(strings).unwrap_or_default(), o.get("args").and_then(strings).unwrap_or_default()),
                    _ => return Err(usage("view takes [\"<function>\", [\"<type>:<value>\", ...]]")),
                };
                let (function, inline) = parse_function(&f)?;
                targs.splice(0..0, inline);
                s.view(&function, &targs, &args).await?
            }
            "resource" => {
                let (addr, tag) = match &params {
                    Value::Array(a) if a.len() == 2 => (a[0].as_str().unwrap_or(""), a[1].as_str().unwrap_or("")),
                    _ => return Err(usage("resource takes [\"<address>\", \"<struct tag>\"]")),
                };
                s.resource(&parse_address(addr)?, &canonical_struct_tag(tag)?).await?
            }
            "ledger" => json!({
                "chainId": s.resource("0x1", "0x1::chain_id::ChainId").await?["id"],
                "forkVersion": handle.fork_point.number,
                "blockHeight": s.block_height().await?,
                "timestampUsecs": s.timestamp_usecs().await?.to_string(),
            }),
            other => return Err(usage(&format!("{other} is not a method on Aptos; methods: view, resource, ledger"))),
        };
        let text = s.scrub(&out.to_string());
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(redact_urls(&text))))
    }

    async fn selftest(&self, name: &str) -> Result<SelftestReport> {
        let (handle, profile) = self.open(name).await?;
        let s = self.session(name, &profile);
        let parts = self.checks(&handle, &profile, &s).await;
        let report = SelftestReport { ok: parts.iter().all(|c| c.ok), parts };
        // Every CLI call reads the upstream, so a rate limit fails checks that
        // say nothing about the copy: report it as the upstream failure it is.
        if !report.ok {
            if let Err(e) = s.upstream_get("/").await {
                if e.code == ErrorCode::RateLimited {
                    let next = format!("{}, such as CHAINSIM_APTOS_API_KEY=<API key>", e.next.unwrap_or_default());
                    return Err(Error::new(ErrorCode::RateLimited, "selftest checks failed while the upstream rate-limits this host").next(next).data(serde_json::to_value(&report).expect("report serializes")));
                }
            }
        }
        selftest_result(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn functions_and_type_args() {
        assert_eq!(parse_function("0x1::aptos_account::transfer").unwrap(), ("0x1::aptos_account::transfer".to_string(), vec![]));
        let (f, t) = parse_function("0x1::primary_fungible_store::transfer<0x1::fungible_asset::Metadata>").unwrap();
        assert_eq!((f.as_str(), t), ("0x1::primary_fungible_store::transfer", vec!["0x1::fungible_asset::Metadata".to_string()]));
        let (_, t) = parse_function("0x1::m::f<0x1::a::B<u8, u64>, bool>").unwrap();
        assert_eq!(t, ["0x1::a::B<u8, u64>", "bool"]);
        for bad in ["transfer", "0x1::m", "0x1::m::f<", "0x1::m::f<u8>>", "0x1::m::f<,u8>", "zz::m::f"] {
            assert!(parse_function(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cli_errors_map_to_contract_codes() {
        let env = "CHAINSIM_APTOS_MAINNET_REST";
        let pruned = classify("Unexpected error: API error Error(VersionPruned): Ledger version(999) has been pruned", env);
        assert_eq!((pruned.code, pruned.code.exit_code()), (ErrorCode::UpstreamUnavailable, 5));
        assert!(pruned.next.as_deref().unwrap().contains("archive"));
        let dns = classify("API error: Unknown error error sending request for url (<url>): error trying to connect: dns error", env);
        assert_eq!(dns.code, ErrorCode::UpstreamUnavailable);
        assert!(dns.next.as_deref().unwrap().contains(env));
        assert_eq!(classify("HTTP 429 Too Many Requests", env).code, ErrorCode::RateLimited);
        assert_eq!(classify("Linker Error: Module 0x1::nope doesn't exist", env).code, ErrorCode::RpcError);
    }

    #[test]
    fn transactions_summarize_the_cli_result() {
        let v = json!({ "transaction_hash": "0xb9", "gas_used": 10336, "gas_unit_price": 100, "sender": "1e35", "sequence_number": 0, "success": true, "vm_status": "Executed successfully" });
        let tx = TxResult::from_cli(&v).unwrap();
        assert!(tx.success);
        assert_eq!(tx.summary()["status"], "success");
        assert_eq!(tx.summary()["gasUsed"], 10336);
        let failed = json!({ "transaction_hash": "0x7c", "success": false, "vm_status": "Move abort in 0x1::fungible_asset: EINSUFFICIENT_BALANCE(0x10004)" });
        assert_eq!(TxResult::from_cli(&failed).unwrap().summary()["status"], "failure");
        assert!(TxResult::from_cli(&json!({})).is_err());
    }

    #[test]
    fn secrets_stay_in_the_environment() {
        let env = engine_env("https://key.example/v1", Some("k-123"));
        assert!(env.contains(&format!("{UPSTREAM_ENV}=https://key.example/v1")) && env.contains(&format!("{KEY_ENV}=k-123")));
        assert!(!engine_env("https://x/v1", None).iter().any(|e| e.starts_with(KEY_ENV)));
        // The scripts name the variables, never their values.
        for script in [init_script(), upstream_get_script()] {
            assert!(script.contains(&format!("${UPSTREAM_ENV}")) || script.contains(&format!("${{{UPSTREAM_ENV}")), "{script}");
            assert!(script.contains(&format!("${{{KEY_ENV}:+")), "{script}");
            assert!(!script.contains("https://"));
        }
    }

    #[test]
    fn snapshot_ids_are_safe_in_scripts() {
        let id = snapshot_id();
        assert!(valid_snapshot_id(&id));
        for bad in ["", "s", "x12", "s1;rm", "s../a"] {
            assert!(!valid_snapshot_id(bad), "{bad}");
        }
        assert!(revert_script("s1", "s2").contains("exit 4"));
        assert!(snapshot_script("s1").contains(&format!("cp -a {SESSION} {SNAP}/s1")));
    }

    #[test]
    fn the_profile_matches_the_lock_and_the_design() {
        let p = profile::get("aptos-mainnet").unwrap();
        assert_eq!((p.id.as_str(), p.family.as_str(), p.engine.as_str()), ("aptos:1", "aptos", "aptos-cli"));
        assert!(p.ports.rpc.is_none());
        let lock = EngineLock::embedded().unwrap();
        let e = lock.engine("aptos-cli").unwrap();
        assert_eq!(e.version, "9.6.0");
        let user = e.user.as_deref().unwrap();
        assert!(user.split_once(':').is_some_and(|(u, g)| u.parse::<u32>().is_ok() && g.parse::<u32>().is_ok()), "the volume owner needs uid:gid");
        let usdc = p.token("USDC").unwrap();
        assert_eq!((usdc.address.as_str(), usdc.decimals), ("0xbae207659db88bea0cbead6da0ed00aac12edcdda169e591cd41c94180b46f3b", 6));
        assert_eq!(p.selftest.token.as_deref(), Some("USDC"));
        parse_address(p.funder.as_deref().unwrap()).unwrap();
        for words in ["experimental", "REST API", "rpcUrl", "one at a time", "authentication key", "chain id 1", "compute units", "150 million", "first read", "config.json", "Innovation-Enabling Source Code License"] {
            assert!(p.limits.iter().any(|l| l.to_lowercase().contains(&words.to_lowercase())), "no limit mentions {words}");
        }
    }
}
