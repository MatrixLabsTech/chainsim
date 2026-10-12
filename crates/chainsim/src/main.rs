//! chainsim: local mainnet forks in Docker, with a JSON interface for agents.
//!
//! stdout carries exactly one JSON envelope per command, even on error; logs
//! go to stderr. The exit code says what happened (design section 9).

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

mod install;

use chainsim_core::docker::Docker;
use chainsim_core::fork::{FundRequest, SendRequest, SetKind, SetRequest, TokenAmount, UpRequest};
use chainsim_core::lock::EngineLock;
use chainsim_core::profile::{self, Profile};
use chainsim_core::{envelope, Adapter, Error, ErrorCode, Result};
use chainsim_evm::EvmAdapter;
use chainsim_sui::SuiAdapter;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "chainsim", version, about = "Local mainnet forks in Docker. Every command prints one JSON object.")]
struct Cli {
    /// Print compact JSON (the default when stdout is not a terminal).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the chains, or one chain's engine, maturity, operations, tokens and limits.
    Describe { chain: Option<String> },
    /// Start a fork and print the fork object. Returns a running fork of the same name and chain as is.
    Up {
        chain: String,
        /// The fork name (default: the chain).
        #[arg(long)]
        name: Option<String>,
        /// The fork block (default: the upstream head now).
        #[arg(long)]
        block: Option<u64>,
        /// Join this existing Docker network instead of chainsim-<name>.
        #[arg(long, requires = "alias")]
        network: Option<String>,
        /// The engine's host name on that network.
        #[arg(long)]
        alias: Option<String>,
        /// Bind a free 127.0.0.1 port and report it as hostRpcUrl.
        #[arg(long)]
        publish: bool,
    },
    /// List the forks.
    Ls,
    /// The fork object plus head and uptime.
    Status { name: String },
    /// Remove a fork: engine, sidecar, volume and the network chainsim made. A missing fork is exit 0.
    Down {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        name: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Set balances to the asked amounts, then read them back.
    Fund {
        name: String,
        address: String,
        /// Native balance in whole units.
        #[arg(long)]
        native: Option<String>,
        /// <symbol|address>:<amount>, repeatable.
        #[arg(long = "token", value_name = "TOKEN:AMOUNT")]
        tokens: Vec<String>,
    },
    /// Snapshot the fork state.
    Snapshot { name: String },
    /// Revert to a snapshot (consumed) and return a fresh snapshot of the restored state.
    Revert { name: String, id: String },
    /// One raw RPC call through the fork's control sidecar.
    Call {
        name: String,
        method: String,
        /// A JSON array of params (default: []).
        params: Option<String>,
        /// Send an eth_sendTransaction as this address by impersonation (EVM).
        #[arg(long)]
        from: Option<String>,
    },
    /// Send a transaction as any address, without its key, on the fork only.
    Send {
        name: String,
        /// The sender to act as (impersonated; a mainnet-active address works).
        #[arg(long = "as", value_name = "ADDRESS")]
        from: String,
        /// The contract address (EVM), or the Move function package::module::function<T...> (Sui).
        #[arg(long)]
        to: String,
        /// The function, such as "pause()" or "transfer(address,uint256)", followed by its arguments.
        #[arg(long, conflicts_with = "data")]
        sig: Option<String>,
        /// Raw calldata in hex, instead of --sig.
        #[arg(long)]
        data: Option<String>,
        /// Native value to send, in whole units of the native symbol.
        #[arg(long)]
        value: Option<String>,
        /// The arguments for --sig. Put a negative number after `--`.
        args: Vec<String>,
    },
    /// Override state on the fork and read it back.
    Set {
        name: String,
        #[command(subcommand)]
        what: SetWhat,
    },
    /// Chain-level checks; exit 6 when any fails, with every check in data.
    Selftest { name: String },
    /// Pull or build engines from the lock ahead of time (default: every engine).
    Pull { engine: Option<String> },
    /// List the lock's engines, whether each is on this host, and the chains that use it.
    Engines,
    /// Check the setup: Docker, engines, the plugin in Claude Code and Codex, and each chain's upstream variable.
    Doctor,
    /// Remove chainsim engine images no profile runs and fork volumes no fork owns.
    Prune,
    /// Write the chainsim plugin to a local folder and add it to Claude Code and Codex.
    Install {
        /// The folder (default: $CHAINSIM_HOME/marketplace, else ~/.chainsim/marketplace).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SetWhat {
    /// One storage slot to a 32-byte value (decimal or 0x hex).
    Storage { address: String, slot: String, value: String },
    /// A contract's code, from hex or a file holding hex.
    Code {
        address: String,
        #[arg(required_unless_present = "file", conflicts_with = "file")]
        hex: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// The native balance, in whole units of the native symbol.
    Balance { address: String, amount: String },
    /// A token balance, in whole units of the token, minted or taken from a holder.
    Token {
        address: String,
        /// The token: a Sui coin type such as 0x...::usdc::USDC.
        coin_type: String,
        amount: String,
        /// Take the amount from this holder instead of minting.
        #[arg(long)]
        from: Option<String>,
    },
    /// The owner of an object, moved as its current owner.
    Owner { object: String, owner: String },
    /// The contents of an object: one or more fields, or the whole object as BCS.
    Object {
        object: String,
        /// A field to change, as name=value; a nested field by dotted path, such as a.b=1. Repeat for more.
        #[arg(long = "field", value_name = "NAME=VALUE", required_unless_present = "bcs", conflicts_with = "bcs")]
        fields: Vec<String>,
        /// The whole new contents as BCS hex, starting with the object's own id.
        #[arg(long)]
        bcs: Option<String>,
    },
}

/// The adapter for a profile family.
async fn adapter(family: &str) -> Result<Box<dyn Adapter>> {
    match family {
        "evm" => Ok(Box::new(EvmAdapter::connect().await?)),
        "sui" => Ok(Box::new(SuiAdapter::connect().await?)),
        other => Err(Error::unsupported(&format!("the {other} family"), "chainsim describe")),
    }
}

/// The adapter of a running fork, from its labels.
async fn adapter_of(name: &str) -> Result<Box<dyn Adapter>> {
    let docker = Docker::connect().await?;
    let handle = docker.find(name).await?.ok_or_else(|| Error::fork_not_found(name))?;
    let profile = profile::get(&handle.chain)?;
    adapter(&profile.family).await
}

fn describe_profile(p: &Profile, lock: &EngineLock) -> Value {
    let engine = lock.engines.get(&p.engine);
    json!({
        "chain": p.key,
        "chainId": p.id,
        "name": p.name,
        "family": p.family,
        "engine": format!("{}@{}", p.engine, engine.map_or("?", |e| e.version.as_str())),
        // A source-built engine: the repo, branch and commit it compiles.
        "engineSource": engine.and_then(|e| e.build.as_ref()).and_then(|b| b.source.as_ref()),
        "maturity": p.maturity,
        "operations": match p.family.as_str() { "evm" => chainsim_evm::OPERATIONS, "sui" => chainsim_sui::OPERATIONS, _ => &[] },
        "upstream": p.upstream_info(),
        "native": p.native,
        "tokens": p.tokens,
        "overrides": p.overrides.iter().map(|o| json!({ "kind": o.kind, "target": o.target, "note": o.note })).collect::<Vec<_>>(),
        "limits": p.limits,
    })
}

fn describe(chain: Option<&str>) -> Result<Value> {
    let lock = EngineLock::embedded()?;
    let errors: Vec<Value> = ErrorCode::ALL.iter().map(|c| json!({ "code": c.as_str(), "exit": c.exit_code() })).collect();
    if let Some(chain) = chain {
        let mut out = describe_profile(&profile::get(chain)?, &lock);
        out["errors"] = json!(errors);
        return Ok(out);
    }
    let chains: Vec<Value> = profile::all()?.iter().map(|p| describe_profile(p, &lock)).collect();
    Ok(json!({
        "chains": chains,
        "commands": [
            "describe [<chain>]",
            "up <chain> [--name <n>] [--block <n>] [--network <net> --alias <host>] [--publish]",
            "ls",
            "status <name>",
            "down <name> | --all",
            "fund <name> <address> [--native <amount>] [--token <symbol|address>:<amount>]...",
            "snapshot <name>",
            "revert <name> <id>",
            "call <name> <method> [<params-json>] [--from <address>]",
            "send <name> --as <address> --to <address> (--sig \"<fn(types)>\" [<arg>...] | --data <hex>) [--value <amount>]",
            "set <name> storage <address> <slot> <value> | code <address> (<hex> | --file <path>) | balance <address> <amount> | token <address> <coin type> <amount> [--from <holder>] | owner <object> <new owner>",
            "selftest <name>",
            "pull [<engine>]",
            "engines",
            "prune",
            "doctor",
            "install [--dir <folder>]",
        ],
        "exitCodes": { "0": "ok", "1": "error", "2": "usage", "3": "unsupported on this engine", "4": "not found", "5": "upstream failure", "6": "selftest failed", "7": "refused for safety", "8": "conflict" },
        "errors": errors,
    }))
}

async fn ls() -> Result<Value> {
    let docker = Docker::connect().await?;
    let mut out = Vec::new();
    for h in docker.forks().await? {
        let mut v = match profile::get(&h.chain) {
            Ok(p) => serde_json::to_value(h.fork(&p, None)).unwrap_or(Value::Null),
            Err(_) => json!({ "name": h.name, "chain": h.chain }),
        };
        v["running"] = json!(h.running);
        v["uptimeSecs"] = json!(h.uptime_secs);
        out.push(v);
    }
    Ok(json!(out))
}

async fn pull(only: Option<&str>) -> Result<Value> {
    let lock = EngineLock::embedded()?;
    if let Some(id) = only.filter(|id| !lock.engines.contains_key(*id)) {
        return Err(Error::new(ErrorCode::Usage, format!("no engine {id} in the lock")).next("chainsim engines"));
    }
    let docker = Docker::connect().await?;
    let mut done = Vec::new();
    let mut failed = Vec::new();
    for (id, engine) in lock.engines.iter().filter(|(id, _)| only.is_none_or(|o| o == id.as_str())) {
        match docker.pull_engine(id, engine).await {
            Ok((image, status)) => done.push(json!({ "engine": id, "version": engine.version, "image": image, "status": status })),
            Err(e) => failed.push(json!({ "engine": id, "code": e.code.as_str(), "message": e.message })),
        }
    }
    let data = json!({ "engines": done, "failed": failed });
    if !failed.is_empty() {
        return Err(Error::new(ErrorCode::EngineFailed, format!("{} engine(s) did not pull or build", failed.len())).next("chainsim pull <engine> to retry one").data(data));
    }
    Ok(data)
}

async fn engines() -> Result<Value> {
    let lock = EngineLock::embedded()?;
    let profiles = profile::all()?;
    let docker = Docker::connect().await?;
    let mut present = std::collections::HashSet::new();
    for e in lock.engines.values() {
        if docker.image_exists(&e.reference()).await? {
            present.insert(e.reference());
        }
    }
    Ok(json!(chainsim_core::engines::rows(&lock, &profiles, |image| present.contains(image))))
}

/// The setup in one object. Exit 1 (`docker_unavailable`) when Docker is not
/// reachable, with the rest of the report in `data`.
async fn doctor() -> Result<Value> {
    let lock = EngineLock::embedded()?;
    let profiles = profile::all()?;
    let chains: Vec<Value> = profiles.iter().map(|p| json!({ "chain": p.key, "maturity": p.maturity, "upstream": p.upstream_info() })).collect();
    let chainsim = json!({ "version": env!("CARGO_PKG_VERSION"), "path": std::env::current_exe().ok() });
    let plugin = install::plugin_status();
    let docker = match Docker::connect().await {
        Ok(d) => d,
        Err(e) => return Err(e.data(json!({ "docker": { "reachable": false }, "chainsim": chainsim, "plugin": plugin, "chains": chains }))),
    };
    let version = docker.api().version().await.ok().and_then(|v| v.version);
    let mut present = std::collections::HashSet::new();
    for e in lock.engines.values() {
        if docker.image_exists(&e.reference()).await? {
            present.insert(e.reference());
        }
    }
    let engines = chainsim_core::engines::rows(&lock, &profiles, |image| present.contains(image));
    Ok(json!({ "docker": { "reachable": true, "version": version }, "chainsim": chainsim, "engines": engines, "plugin": plugin, "chains": chains }))
}

async fn prune() -> Result<Value> {
    let lock = EngineLock::embedded()?;
    let profiles = profile::all()?;
    let docker = Docker::connect().await?;
    let mut present = std::collections::HashSet::new();
    for e in lock.engines.values() {
        if docker.image_exists(&e.reference()).await? {
            present.insert(e.reference());
        }
    }
    let plan = chainsim_core::engines::prune_plan(&lock, &profiles, &docker.owned_tags().await?, |r| present.contains(r), &docker.fork_volumes().await?, &docker.live_forks().await?);
    let (mut images, mut volumes, mut skipped) = (Vec::new(), Vec::new(), Vec::new());
    for image in plan.images {
        match docker.remove_image(&image).await {
            Ok(()) => images.push(image),
            Err(reason) => skipped.push(json!({ "image": image, "reason": reason })),
        }
    }
    for volume in plan.volumes {
        match docker.remove_volume(&volume).await {
            Ok(()) => volumes.push(volume),
            Err(reason) => skipped.push(json!({ "volume": volume, "reason": reason })),
        }
    }
    Ok(json!({ "removedImages": images, "removedVolumes": volumes, "skipped": skipped }))
}

async fn down_all() -> Result<Value> {
    let docker = Docker::connect().await?;
    let mut removed = Vec::new();
    for h in docker.forks().await? {
        docker.remove_fork(&h.name).await?;
        removed.push(h.name);
    }
    Ok(json!({ "removed": removed }))
}

fn parse_tokens(raw: &[String]) -> Result<Vec<TokenAmount>> {
    raw.iter()
        .map(|t| {
            let (token, amount) = t.rsplit_once(':').filter(|(a, b)| !a.is_empty() && !b.is_empty()).ok_or_else(|| Error::new(ErrorCode::Usage, format!("bad --token {t:?}: use <symbol|address>:<amount>")))?;
            Ok(TokenAmount { token: token.to_string(), amount: amount.to_string() })
        })
        .collect()
}

fn to_value<T: serde::Serialize>(r: Result<T>) -> Result<Value> {
    r.map(|v| serde_json::to_value(v).expect("result serializes"))
}

async fn run(command: Command) -> Result<Value> {
    match command {
        Command::Describe { chain } => describe(chain.as_deref()),
        Command::Up { chain, name, block, network, alias, publish } => {
            let profile = profile::get(&chain)?;
            let req = UpRequest { name: name.unwrap_or_else(|| profile.key.clone()), block, network, alias, publish };
            to_value(adapter(&profile.family).await?.up(&profile, &req).await)
        }
        Command::Ls => ls().await,
        Command::Status { name } => {
            let mut status = to_value(adapter_of(&name).await?.status(&name).await)?;
            // Every send --as and set since up, so a report can name what depends on them.
            status["modifications"] = json!(chainsim_core::mods::list(&Docker::connect().await?, &name).await?);
            Ok(status)
        }
        Command::Send { name, from, to, sig, data, value, args } => {
            let req = SendRequest { from, to, sig, args, data, value };
            adapter_of(&name).await?.send(&name, &req).await
        }
        Command::Set { name, what } => {
            let req = match what {
                SetWhat::Storage { address, slot, value } => SetRequest { kind: SetKind::Storage { slot }, address, value },
                SetWhat::Code { address, hex, file } => {
                    let value = match (hex, file) {
                        (Some(h), _) => h,
                        (None, Some(f)) => std::fs::read_to_string(&f).map_err(|e| Error::new(ErrorCode::Usage, format!("read {}: {e}", f.display())))?,
                        (None, None) => unreachable!("clap requires one"),
                    };
                    SetRequest { kind: SetKind::Code, address, value }
                }
                SetWhat::Balance { address, amount } => SetRequest { kind: SetKind::Balance, address, value: amount },
                SetWhat::Token { address, coin_type, amount, from } => SetRequest { kind: SetKind::Token { coin_type, from }, address, value: amount },
                SetWhat::Owner { object, owner } => SetRequest { kind: SetKind::Owner, address: object, value: owner },
                SetWhat::Object { object, fields, bcs } => SetRequest { kind: SetKind::Object { fields, bcs }, address: object, value: String::new() },
            };
            adapter_of(&name).await?.set(&name, &req).await
        }
        Command::Down { all: true, .. } => down_all().await,
        Command::Down { name, .. } => {
            let name = name.unwrap_or_default();
            chainsim_core::docker::check_name(&name)?;
            // Down works without a profile: it removes whatever carries the fork's labels.
            to_value(EvmAdapter::connect().await?.down(&name).await)
        }
        Command::Fund { name, address, native, tokens } => {
            let req = FundRequest { address, native, tokens: parse_tokens(&tokens)? };
            to_value(adapter_of(&name).await?.fund(&name, &req).await)
        }
        Command::Snapshot { name } => to_value(adapter_of(&name).await?.snapshot(&name).await),
        Command::Revert { name, id } => to_value(adapter_of(&name).await?.revert(&name, &id).await),
        Command::Call { name, method, params, from } => {
            let params: Value = match params {
                Some(p) => serde_json::from_str(&p).map_err(|e| Error::new(ErrorCode::Usage, format!("params are not JSON: {e}")))?,
                None => json!([]),
            };
            adapter_of(&name).await?.call(&name, &method, params, from.as_deref()).await
        }
        Command::Selftest { name } => to_value(adapter_of(&name).await?.selftest(&name).await),
        Command::Pull { engine } => pull(engine.as_deref()).await,
        Command::Engines => engines().await,
        Command::Doctor => doctor().await,
        Command::Prune => prune().await,
        Command::Install { dir } => install::install(dir),
    }
}

/// Writes the envelope. A closed stdout, such as `chainsim describe | head`,
/// is not an error: the reader has what it wanted.
fn print(value: &Value, pretty: bool) {
    let text = if pretty { serde_json::to_string_pretty(value).expect("json") } else { value.to_string() };
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{text}").and_then(|()| out.flush());
}

#[tokio::main]
async fn main() {
    let pretty_default = std::io::stdout().is_terminal();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            use clap::error::ErrorKind;
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand) && pretty_default {
                e.exit();
            }
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                let text = e.to_string();
                print(&envelope::ok(json!({ "help": text })), false);
                std::process::exit(0);
            }
            let message = e.render().to_string();
            let message = message.lines().next().unwrap_or("bad arguments").trim_start_matches("error: ").to_string();
            let err = Error::new(ErrorCode::Usage, message).next("chainsim --help");
            print(&envelope::err(&err), false);
            std::process::exit(2);
        }
    };
    let pretty = pretty_default && !cli.json;
    let result = run(cli.command).await;
    let (out, code) = envelope::render(&result);
    print(&out, pretty);
    std::process::exit(code);
}
