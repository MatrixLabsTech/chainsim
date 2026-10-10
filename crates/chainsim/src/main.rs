//! chainsim: local mainnet forks in Docker, with a JSON interface for agents.
//!
//! stdout carries exactly one JSON envelope per command, even on error; logs
//! go to stderr. The exit code says what happened (design section 9).

use std::io::IsTerminal;

use chainsim_core::docker::Docker;
use chainsim_core::fork::{FundRequest, TokenAmount, UpRequest};
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
    /// Chain-level checks; exit 6 when any fails, with every check in data.
    Selftest { name: String },
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
        "maturity": p.maturity,
        "operations": match p.family.as_str() { "evm" => chainsim_evm::OPERATIONS, "sui" => chainsim_sui::OPERATIONS, _ => &[] },
        "upstream": { "default": p.upstream.default, "env": p.upstream.env, "fromEnv": p.upstream_is_secret() },
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
            "selftest <name>",
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
        Command::Status { name } => to_value(adapter_of(&name).await?.status(&name).await),
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
    }
}

fn print(value: &Value, pretty: bool) {
    if pretty {
        println!("{}", serde_json::to_string_pretty(value).expect("json"));
    } else {
        println!("{value}");
    }
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
