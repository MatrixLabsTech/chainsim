//! The REST endpoint of an Aptos fork: a small HTTP server that runs inside
//! the engine container, beside the session, and answers the Aptos fullnode
//! REST paths an SDK needs by running the Aptos CLI on the session under the
//! fork lock, as the adapter does through Docker exec.
//!
//! - Reads (`/v1`, accounts, resources, `view`) are CLI reads on the session.
//!   A read's answer is kept until the session's `delta.json` changes, since
//!   that file is the copy's whole state and reads never write it.
//! - Module ABIs and the base of an account's resource list come from the
//!   upstream at the version the session reads, through curl in the container.
//! - A submitted transaction is decoded from its BCS and its entry function
//!   runs as the sender with the fork's control key, the way `send --as` does.
//!   The signature is not checked.
//!
//! Environment: `CHAINSIM_REST_LISTEN` (default `0.0.0.0:8080`), and the
//! engine's upstream URL and API key variables.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chainsim_core::control::redact_urls;
use chainsim_core::docker::UPSTREAM_ENV;
use serde_json::{json, Value};
use sha3::{Digest, Sha3_256};

use crate::state::*;
use crate::{upstream_get_script, ACCOUNT, KEY, KEY_ENV, LOCK, OBJECT_GROUP, PATCH, SESSION};

/// The port the endpoint listens on in the engine container.
pub const LISTEN_ENV: &str = "CHAINSIM_REST_LISTEN";
/// A private copy of the session, for simulations.
const SIM: &str = "/data/sim";
const MAX_HEAD: usize = 64 << 10;
const MAX_BODY: usize = 2 << 20;
const BCS_SIGNED: &str = "application/x.aptos.signed_transaction+bcs";
const BCS_VIEW: &str = "application/x.aptos.view_function+bcs";
const STATUS_MARK: &str = "\n@@chainsim-http-status@@";
const EVENTS_MARK: &str = "\n@@chainsim-events@@";
/// The gas prices the endpoint reports: the network minimum.
const GAS_PRICE: u64 = 100;

/// An error in the fullnode's shape: `{message, error_code, vm_error_code}`.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub vm: Option<u64>,
}

impl ApiError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), vm: None }
    }

    fn bad(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_input", message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(500, "internal_error", message)
    }

    fn vm(message: impl Into<String>, code: u64) -> Self {
        Self { status: 400, code: "vm_error", message: message.into(), vm: Some(code) }
    }

    fn body(&self) -> Value {
        json!({ "message": self.message, "error_code": self.code, "vm_error_code": self.vm })
    }
}

type Api<T> = std::result::Result<T, ApiError>;

/// A BCS reader over a byte slice.
pub struct Bcs<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Bcs<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, n: usize) -> Api<&'a [u8]> {
        let end = self.at.checked_add(n).filter(|e| *e <= self.bytes.len()).ok_or_else(|| ApiError::bad("the BCS body ends early"))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u8(&mut self) -> Api<u8> {
        Ok(self.take(1)?[0])
    }

    fn u64(&mut self) -> Api<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    fn uleb(&mut self) -> Api<usize> {
        let mut out = 0u64;
        for shift in (0..35).step_by(7) {
            let b = self.u8()?;
            out |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(out as usize);
            }
        }
        Err(ApiError::bad("a BCS length is too long"))
    }

    fn bytes(&mut self) -> Api<&'a [u8]> {
        let n = self.uleb()?;
        self.take(n)
    }

    fn identifier(&mut self) -> Api<String> {
        let s = std::str::from_utf8(self.bytes()?).map_err(|_| ApiError::bad("a name in the BCS body is not UTF-8"))?;
        if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(ApiError::bad(format!("{s:?} is not a Move name")));
        }
        Ok(s.to_string())
    }

    fn address(&mut self) -> Api<String> {
        Ok(format!("0x{}", to_hex(self.take(32)?)))
    }

    /// A TypeTag as the CLI's `--type-args` takes it.
    fn type_tag(&mut self, depth: u8) -> Api<String> {
        if depth > 8 {
            return Err(ApiError::bad("a type argument nests too deep"));
        }
        Ok(match self.uleb()? {
            0 => "bool".into(),
            1 => "u8".into(),
            2 => "u64".into(),
            3 => "u128".into(),
            4 => "address".into(),
            5 => "signer".into(),
            6 => format!("vector<{}>", self.type_tag(depth + 1)?),
            7 => {
                let address = short_address(&self.address()?);
                let (module, name) = (self.identifier()?, self.identifier()?);
                let n = self.uleb()?;
                let args = (0..n).map(|_| self.type_tag(depth + 1)).collect::<Api<Vec<_>>>()?;
                let generics = if args.is_empty() { String::new() } else { format!("<{}>", args.join(", ")) };
                format!("{address}::{module}::{name}{generics}")
            }
            8 => "u16".into(),
            9 => "u32".into(),
            10 => "u256".into(),
            other => return Err(ApiError::bad(format!("type tag variant {other} is not supported"))),
        })
    }

    /// An entry function call: module, function, type arguments and BCS arguments.
    fn entry(&mut self) -> Api<EntryCall> {
        let address = short_address(&self.address()?);
        let (module, function) = (self.identifier()?, self.identifier()?);
        let n = self.uleb()?;
        let type_args = (0..n).map(|_| self.type_tag(0)).collect::<Api<Vec<_>>>()?;
        let n = self.uleb()?;
        let args = (0..n).map(|_| self.bytes().map(<[u8]>::to_vec)).collect::<Api<Vec<_>>>()?;
        Ok(EntryCall { function: format!("{address}::{module}::{function}"), type_args, args })
    }
}

/// An address as the fullnode prints it: 0x and no leading zeros.
fn short_address(long: &str) -> String {
    let digits = long.trim_start_matches("0x").trim_start_matches('0');
    format!("0x{}", if digits.is_empty() { "0" } else { digits })
}

#[derive(Debug, Clone, PartialEq)]
pub struct EntryCall {
    pub function: String,
    pub type_args: Vec<String>,
    /// Each argument's BCS bytes.
    pub args: Vec<Vec<u8>>,
}

impl EntryCall {
    /// The CLI arguments that name this call: each argument goes as `raw:<hex>`.
    fn cli_args(&self) -> Vec<String> {
        let mut out = vec!["--function-id".to_string(), self.function.clone()];
        if !self.type_args.is_empty() {
            out.push("--type-args".into());
            out.extend(self.type_args.iter().cloned());
        }
        if !self.args.is_empty() {
            out.push("--args".into());
            out.extend(self.args.iter().map(|a| format!("raw:0x{}", to_hex(a))));
        }
        out
    }

    fn payload(&self) -> Value {
        json!({
            "type": "entry_function_payload",
            "function": self.function,
            "type_arguments": self.type_args,
            "arguments": self.args.iter().map(|a| format!("0x{}", to_hex(a))).collect::<Vec<_>>(),
        })
    }
}

/// The body of a BCS view request.
pub fn decode_view(body: &[u8]) -> Api<EntryCall> {
    Bcs::new(body).entry()
}

/// A signed transaction, as far as the endpoint reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedTx {
    pub sender: String,
    pub sequence_number: u64,
    pub call: EntryCall,
    pub max_gas: u64,
    pub gas_unit_price: u64,
    pub expiration_secs: u64,
    pub chain_id: u8,
    /// The transaction hash a fullnode gives these bytes.
    pub hash: String,
}

/// Decodes a BCS `SignedTransaction` with an entry function payload. The
/// authenticator after the raw transaction is not read.
pub fn decode_signed(body: &[u8]) -> Api<SignedTx> {
    let mut b = Bcs::new(body);
    let sender = b.address()?;
    let sequence_number = b.u64()?;
    let call = match b.uleb()? {
        2 => b.entry()?,
        other => return Err(ApiError::bad(format!("the endpoint runs entry function transactions only (payload variant 2), not payload variant {other}"))),
    };
    let (max_gas, gas_unit_price, expiration_secs, chain_id) = (b.u64()?, b.u64()?, b.u64()?, b.u8()?);
    Ok(SignedTx { sender, sequence_number, call, max_gas, gas_unit_price, expiration_secs, chain_id, hash: transaction_hash(body) })
}

/// The hash of a user transaction: sha3-256 over the `APTOS::Transaction`
/// prefix hash, the UserTransaction variant byte and the signed bytes.
pub fn transaction_hash(signed: &[u8]) -> String {
    let prefix = Sha3_256::digest(b"APTOS::Transaction");
    let mut h = Sha3_256::new();
    h.update(prefix);
    h.update([0u8]);
    h.update(signed);
    format!("0x{}", to_hex(&h.finalize()))
}

/// One argument of a JSON view request as the CLI takes it, from the
/// parameter type in the module's ABI.
pub fn json_arg(param: &str, value: &Value) -> Api<String> {
    let text = |v: &Value| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    let kind = |t: &str| -> Option<&'static str> {
        Some(match t {
            "bool" => "bool",
            "u8" => "u8",
            "u16" => "u16",
            "u32" => "u32",
            "u64" => "u64",
            "u128" => "u128",
            "u256" => "u256",
            "address" => "address",
            "0x1::string::String" => "string",
            t if t.starts_with("0x1::object::Object<") => "address",
            _ => return None,
        })
    };
    if let Some(k) = kind(param) {
        return Ok(format!("{k}:{}", text(value)));
    }
    if param == "vector<u8>" && value.is_string() {
        return Ok(format!("hex:{}", text(value)));
    }
    if let Some(k) = param.strip_prefix("vector<").and_then(|t| t.strip_suffix('>')).and_then(kind) {
        if value.is_array() {
            return Ok(format!("{k}:{value}"));
        }
    }
    Err(ApiError::bad(format!("a JSON view cannot pass an argument of type {param}: send the view as BCS, as the SDKs do")))
}

/// The session's record of a transaction's events in the fullnode's event shape.
/// The data stays as the session wrote it.
pub fn events(recorded: &Value) -> Vec<Value> {
    let Some(list) = recorded.as_array() else { return vec![] };
    list.iter()
        .filter_map(|e| {
            let (version, body) = e.as_object()?.iter().next()?;
            let text = |v: &Value| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
            let (creation, account, sequence) = match version.as_str() {
                "V1" => (text(&body["key"]["creation_number"]), short_address(&text(&body["key"]["account_address"])), text(&body["sequence_number"])),
                _ => ("0".into(), "0x0".into(), "0".into()),
            };
            Some(json!({
                "guid": { "creation_number": creation, "account_address": account },
                "sequence_number": sequence,
                "type": body["type_tag"],
                "data": body["event_data"],
            }))
        })
        .collect()
}

/// A request, as far as the endpoint reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: String,
    /// The path without the query and without the optional `/v1` prefix,
    /// split at `/` and percent-decoded.
    pub path: Vec<String>,
    pub content_type: String,
    pub body: Vec<u8>,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%').then(|| b.get(i + 1..i + 3)).flatten().and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The path segments of a request target, without `/v1` and the query.
pub fn route(target: &str) -> Vec<String> {
    let path = target.split(['?', '#']).next().unwrap_or("");
    let mut parts: Vec<String> = path.split('/').filter(|p| !p.is_empty()).map(percent_decode).collect();
    if parts.first().is_some_and(|p| p == "v1") {
        parts.remove(0);
    }
    parts
}

/// Reads one HTTP/1.1 request. `Ok(None)` is a connection closed before a request.
pub fn read_request(stream: &mut impl Read) -> std::result::Result<Option<Request>, ApiError> {
    let mut reader = BufReader::new(stream);
    let mut head = Vec::new();
    loop {
        let before = head.len();
        let n = reader.by_ref().take((MAX_HEAD - before) as u64 + 1).read_until(b'\n', &mut head).map_err(|e| ApiError::bad(e.to_string()))?;
        if n == 0 {
            if head.is_empty() {
                return Ok(None);
            }
            return Err(ApiError::bad("the request ends inside its headers"));
        }
        if head.len() > MAX_HEAD {
            return Err(ApiError::new(431, "web_framework_error", "the request headers are too large"));
        }
        if &head[before..] == b"\r\n" || &head[before..] == b"\n" {
            break;
        }
    }
    let head = String::from_utf8_lossy(&head);
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or(""));
    let mut length = 0usize;
    let mut content_type = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().map_err(|_| ApiError::bad("content-length is not a number"))?,
            "content-type" => content_type = value.split(';').next().unwrap_or("").trim().to_ascii_lowercase(),
            "transfer-encoding" if !value.eq_ignore_ascii_case("identity") => {
                return Err(ApiError::new(411, "web_framework_error", "send the body with a content-length"));
            }
            _ => {}
        }
    }
    if length > MAX_BODY {
        return Err(ApiError::new(413, "web_framework_error", "the request body is too large"));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).map_err(|_| ApiError::bad("the request body ends early"))?;
    Ok(Some(Request { method, path: route(target), content_type, body }))
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    }
}

/// A whole HTTP/1.1 response. Every answer closes the connection.
pub fn response(status: u16, body: &str, chain_id: Option<u64>) -> Vec<u8> {
    let chain = chain_id.map(|c| format!("x-aptos-chain-id: {c}\r\n")).unwrap_or_default();
    format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{chain}access-control-allow-origin: *\r\naccess-control-allow-methods: GET, POST, OPTIONS\r\naccess-control-allow-headers: *\r\nconnection: close\r\n\r\n{body}",
        reason(status),
        body.len()
    )
    .into_bytes()
}

/// What a read is kept under: the session state file's modification time and size.
type Stamp = (std::time::SystemTime, u64);

#[derive(Default)]
struct Reads {
    stamp: Option<Stamp>,
    kept: HashMap<String, Value>,
}

/// The endpoint's state: answers kept for the current session state, module
/// ABIs (fixed at the fork version) and the transactions it ran.
#[derive(Default)]
pub struct Facade {
    reads: Mutex<Reads>,
    modules: Mutex<HashMap<String, String>>,
    transactions: Mutex<HashMap<String, Value>>,
    /// Held while a transaction or a simulation runs, so two never interleave.
    submit: Mutex<()>,
    /// The copy's chain id, which never changes.
    chain: OnceLock<u64>,
}

fn u64_of(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// Removes each secret, the host of a secret that is a URL, and every other URL from a message.
pub fn scrub_with(s: &str, secrets: &[String]) -> String {
    let mut out = s.to_string();
    for secret in secrets.iter().filter(|v| !v.is_empty()) {
        out = out.replace(secret.as_str(), "<redacted>");
        // curl names the host on its own, and a provider may put its key in a subdomain.
        if let Some(host) = secret.split_once("://").and_then(|(_, rest)| rest.split(['/', '?', ':']).next()).filter(|h| !h.is_empty()) {
            out = out.replace(host, "<upstream host>");
        }
    }
    redact_urls(&out)
}

/// Removes the API key, the upstream URL and every other URL from a message.
fn scrub(s: &str) -> String {
    let secrets: Vec<String> = [KEY_ENV, UPSTREAM_ENV].iter().filter_map(|v| std::env::var(v).ok()).collect();
    scrub_with(s, &secrets)
}

/// The read script: one CLI call on a session under the fork lock. A read
/// leaves a `[n] view ...` folder in the session, which the script removes.
fn read_script(session: &str) -> String {
    format!(
        "exec 9>{LOCK}; flock 9; n=$(sed -n 's/.*\"ops\": *\\([0-9]*\\).*/\\1/p' {session}/config.json); aptos \"$@\"; rc=$?; rm -rf \"{session}/[$n] view\"*; exit $rc"
    )
}

/// The run script: one transaction on a session under the fork lock, then the
/// events the session recorded for it.
fn run_script(session: &str) -> String {
    format!(
        "exec 9>{LOCK}; flock 9; n=$(sed -n 's/.*\"ops\": *\\([0-9]*\\).*/\\1/p' {session}/config.json); aptos \"$@\"; rc=$?; printf '{}'; cat \"{session}/[$n] execute\"*/events.json 2>/dev/null; exit $rc",
        EVENTS_MARK.replace('\n', "\\n")
    )
}

fn sh(script: &str, args: &[String]) -> Api<(i32, String, String)> {
    let out = Command::new("sh").arg("-c").arg(script).arg("sh").args(args).output().map_err(|e| ApiError::internal(format!("could not run the CLI: {e}")))?;
    Ok((out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned()))
}

/// The CLI's `{"Result": ...}`, or its `{"Error": "..."}` as an API error.
fn cli_result(stdout: &str, stderr: &str, code: i32) -> Api<Value> {
    let v: Option<Value> = serde_json::from_str(stdout.trim()).ok();
    match v {
        Some(v) if v.get("Result").is_some() && code == 0 => Ok(v["Result"].clone()),
        Some(v) if v.get("Error").is_some() => {
            let message = scrub(v["Error"].as_str().unwrap_or("error"));
            let e = crate::classify(&message, "");
            Err(match e.code {
                chainsim_core::ErrorCode::RateLimited => ApiError::new(429, "rate_limited", "the upstream rate-limits the fork"),
                chainsim_core::ErrorCode::UpstreamUnavailable => ApiError::new(503, "internal_error", message),
                _ => ApiError::bad(message),
            })
        }
        _ => {
            let tail: String = scrub(stderr.trim()).lines().rev().take(3).collect::<Vec<_>>().join(" | ");
            Err(ApiError::internal(format!("the CLI exited {code} without a result: {tail}")))
        }
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

impl Facade {
    fn stamp() -> Option<Stamp> {
        let m = std::fs::metadata(format!("{SESSION}/delta.json")).ok()?;
        Some((m.modified().ok()?, m.len()))
    }

    /// One CLI read on the session, kept until the session state changes.
    fn read(&self, args: &[String]) -> Api<Value> {
        let key = args.join("\u{1f}");
        let stamp = Self::stamp();
        {
            let mut reads = self.reads.lock().expect("reads");
            if reads.stamp != stamp {
                reads.stamp = stamp;
                reads.kept.clear();
            }
            if let Some(v) = reads.kept.get(&key) {
                return Ok(v.clone());
            }
        }
        let (code, stdout, stderr) = sh(&read_script(SESSION), args)?;
        let value = cli_result(&stdout, &stderr, code)?;
        // Keep it only when the state did not move while the read ran.
        if stamp.is_some() && Self::stamp() == stamp {
            let mut reads = self.reads.lock().expect("reads");
            if reads.stamp == stamp {
                reads.kept.insert(key, value.clone());
            }
        }
        Ok(value)
    }

    fn resource_in(&self, session: &str, address: &str, tag: &str) -> Api<Value> {
        let args = strings(&["move", "sim", "view-resource", "--session", session, "--account", address, "--resource", tag]);
        if session == SESSION {
            return self.read(&args);
        }
        let (code, stdout, stderr) = sh(&read_script(session), &args)?;
        cli_result(&stdout, &stderr, code)
    }

    fn group(&self, address: &str) -> Api<Value> {
        self.read(&strings(&["move", "sim", "view-resource-group", "--session", SESSION, "--account", address, "--resource-group", OBJECT_GROUP]))
    }

    /// A resource under an account, also when it is a member of the object group. Null when absent.
    fn resource(&self, address: &str, tag: &str) -> Api<Value> {
        let v = self.resource_in(SESSION, address, tag)?;
        if !v.is_null() {
            return Ok(v);
        }
        Ok(self.group(address)?.get(tag).cloned().unwrap_or(Value::Null))
    }

    fn view(&self, call: &EntryCall) -> Api<Value> {
        let mut args = strings(&["move", "view", "--session", SESSION]);
        args.extend(call.cli_args());
        self.read(&args)
    }

    /// The session's fork version and operation count, from its config.
    fn config(&self) -> Api<(u64, u64)> {
        let text = std::fs::read_to_string(format!("{SESSION}/config.json")).map_err(|_| ApiError::new(503, "internal_error", "the fork has no session yet"))?;
        let v: Value = serde_json::from_str(&text).map_err(|_| ApiError::internal("the session config is not JSON"))?;
        let version = u64_of(&v["base"]["Remote"]["network_version"]).ok_or_else(|| ApiError::internal("the session config names no fork version"))?;
        Ok((version, u64_of(&v["ops"]).unwrap_or(0)))
    }

    fn chain_id(&self) -> Api<u64> {
        if let Some(id) = self.chain.get() {
            return Ok(*id);
        }
        let id = u64_of(&self.resource_in(SESSION, "0x1", "0x1::chain_id::ChainId")?["id"]).ok_or_else(|| ApiError::internal("the copy returned no chain id"))?;
        Ok(*self.chain.get_or_init(|| id))
    }

    fn timestamp_usecs(&self) -> Api<u64> {
        u64_of(&self.resource_in(SESSION, "0x1", "0x1::timestamp::CurrentTimeMicroseconds")?["microseconds"]).ok_or_else(|| ApiError::internal("the copy returned no timestamp"))
    }

    fn ledger(&self) -> Api<Value> {
        let (version, ops) = self.config()?;
        let height = u64_of(&self.resource_in(SESSION, "0x1", "0x1::block::BlockResource")?["height"]).unwrap_or(0);
        let epoch = u64_of(&self.resource_in(SESSION, "0x1", "0x1::reconfiguration::Configuration")?["epoch"]).unwrap_or(0);
        Ok(json!({
            "chain_id": self.chain_id()?,
            "epoch": epoch.to_string(),
            "ledger_version": (version + ops).to_string(),
            "oldest_ledger_version": version.to_string(),
            "ledger_timestamp": self.timestamp_usecs()?.to_string(),
            "node_role": "full_node",
            "oldest_block_height": height.to_string(),
            "block_height": height.to_string(),
            "git_hash": "",
        }))
    }

    fn account(&self, address: &str) -> Api<Value> {
        let account = self.resource_in(SESSION, address, ACCOUNT)?;
        if account.is_null() {
            // An address with no Account resource is an account at sequence number 0.
            return Ok(json!({ "sequence_number": "0", "authentication_key": address }));
        }
        Ok(json!({ "sequence_number": account["sequence_number"], "authentication_key": account["authentication_key"] }))
    }

    /// One GET to the upstream at the version the session reads: the HTTP status and body.
    fn upstream(&self, path: &str) -> Api<(u16, String)> {
        // A session at fork version V reads state as of V - 1.
        let version = self.config()?.0.saturating_sub(1);
        let sep = if path.contains('?') { '&' } else { '?' };
        let (code, stdout, stderr) = sh(&upstream_get_script(), &[format!("{path}{sep}ledger_version={version}")])?;
        let Some((body, status)) = stdout.rsplit_once(STATUS_MARK).filter(|_| code == 0) else {
            return Err(ApiError::new(503, "internal_error", format!("the upstream did not answer: {}", scrub(stderr.trim()))));
        };
        let status: u16 = status.trim().parse().unwrap_or(0);
        match status {
            429 => Err(ApiError::new(429, "rate_limited", "the upstream rate-limits the fork")),
            0 | 500.. => Err(ApiError::new(503, "internal_error", format!("the upstream answered HTTP {status}"))),
            _ => Ok((status, body.to_string())),
        }
    }

    /// A module with its ABI, from the upstream at the fork version.
    fn module(&self, address: &str, name: &str) -> Api<(u16, String)> {
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(ApiError::bad(format!("{name:?} is not a module name")));
        }
        let key = format!("{address}::{name}");
        if let Some(body) = self.modules.lock().expect("modules").get(&key) {
            return Ok((200, body.clone()));
        }
        let (status, body) = self.upstream(&format!("/accounts/{address}/module/{name}"))?;
        if status == 200 {
            self.modules.lock().expect("modules").insert(key, body.clone());
        }
        Ok((status, body))
    }

    /// An account's resources: the upstream's list at the fork version, with
    /// every resource the copy changed read through the session on top.
    fn resources(&self, address: &str) -> Api<Value> {
        let (status, body) = self.upstream(&format!("/accounts/{address}/resources?limit=9999"))?;
        let mut list: Vec<Value> = if status == 200 { serde_json::from_str(&body).unwrap_or_default() } else { vec![] };
        let delta: serde_json::Map<String, Value> = std::fs::read_to_string(format!("{SESSION}/delta.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let standard = standard_address(address).map_err(|e| ApiError::bad(e.message))?;
        let mut put = |tag: &str, data: Value| {
            list.retain(|r| r["type"] != tag);
            if !data.is_null() {
                list.push(json!({ "type": tag, "data": data }));
            }
        };
        for key in delta.keys() {
            if let Some(tag) = key.strip_prefix(&format!("resource::{standard}::")) {
                put(tag, self.resource_in(SESSION, address, tag)?);
            } else if key == &format!("resource_group::{standard}::{OBJECT_GROUP}") {
                for (tag, data) in self.group(address)?.as_object().cloned().unwrap_or_default() {
                    put(&tag, data);
                }
            }
        }
        Ok(Value::Array(list))
    }

    fn view_request(&self, req: &Request) -> Api<Value> {
        if req.content_type == BCS_VIEW {
            return self.view(&decode_view(&req.body)?);
        }
        let body: Value = serde_json::from_slice(&req.body).map_err(|_| ApiError::bad("the view body is not JSON"))?;
        let function = body["function"].as_str().unwrap_or("");
        let parts: Vec<&str> = function.split("::").collect();
        if parts.len() != 3 {
            return Err(ApiError::bad(format!("{function:?} is not a function such as 0x1::coin::balance")));
        }
        let address = parse_address(parts[0]).map_err(|e| ApiError::bad(e.message))?;
        let (status, module) = self.module(&address, parts[1])?;
        let module: Value = serde_json::from_str(&module).unwrap_or(Value::Null);
        let params = module["abi"]["exposed_functions"].as_array().and_then(|fs| fs.iter().find(|f| f["name"] == parts[2])).map(|f| f["params"].clone());
        let Some(params) = params.filter(|_| status == 200) else {
            return Err(ApiError::bad(format!("the fork point has no function {function}")));
        };
        let params: Vec<String> = params.as_array().map(|p| p.iter().filter_map(|t| t.as_str().map(str::to_string)).collect()).unwrap_or_default();
        let given = body["arguments"].as_array().cloned().unwrap_or_default();
        if given.len() != params.len() {
            return Err(ApiError::bad(format!("{function} takes {} arguments, got {}", params.len(), given.len())));
        }
        let mut args = strings(&["move", "view", "--session", SESSION, "--function-id", function]);
        let type_args: Vec<String> = body["type_arguments"].as_array().map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect()).unwrap_or_default();
        if let Some(bad) = type_args.iter().find(|t| t.starts_with('-') || t.is_empty()) {
            return Err(ApiError::bad(format!("{bad:?} is not a type argument")));
        }
        if !type_args.is_empty() {
            args.push("--type-args".into());
            args.extend(type_args);
        }
        if !params.is_empty() {
            args.push("--args".into());
            for (param, value) in params.iter().zip(&given) {
                args.push(json_arg(param, value)?);
            }
        }
        self.read(&args)
    }

    /// Lets the control key sign for the sender on a session, as `send --as` does.
    fn impersonate(&self, session: &str, sender: &str) -> Api<()> {
        let public = std::fs::read_to_string(format!("{KEY}.pub")).map_err(|_| ApiError::internal("the fork has no control key"))?;
        let public = from_hex(public.trim(), "the control public key").map_err(|e| ApiError::internal(e.message))?;
        let key = ed25519_auth_key(&public);
        if sender == format!("0x{}", to_hex(&key)) {
            return Ok(());
        }
        let account = self.resource_in(session, sender, ACCOUNT)?;
        let bytes = if account.is_null() { new_account_bytes(sender, &key) } else { account_bytes(&account, &key) }.map_err(|e| ApiError::internal(e.message))?;
        let entry = (resource_key(sender, ACCOUNT).map_err(|e| ApiError::internal(e.message))?, state_value_hex(&bytes));
        let (code, _, stderr) = sh(&merge_script(session, LOCK, PATCH), &[patch_lines(&[entry])])?;
        if code != 0 {
            return Err(ApiError::internal(format!("writing the session delta failed: {}", scrub(stderr.trim()))));
        }
        Ok(())
    }

    /// Runs a decoded transaction on a session as its sender and returns it in
    /// the fullnode's `user_transaction` shape. A transaction the VM discards
    /// is an error.
    fn run(&self, session: &str, tx: &SignedTx, check_sequence: bool) -> Api<Value> {
        let chain = self.chain_id()?;
        if u64::from(tx.chain_id) != chain {
            return Err(ApiError::vm(format!("Invalid transaction: Type: Validation Code: BAD_CHAIN_ID (the fork is chain {chain})"), 4));
        }
        let account = self.resource_in(session, &tx.sender, ACCOUNT)?;
        let sequence = u64_of(&account["sequence_number"]).unwrap_or(0);
        if check_sequence && tx.sequence_number != sequence {
            let (name, code) = if tx.sequence_number < sequence { ("SEQUENCE_NUMBER_TOO_OLD", 3) } else { ("SEQUENCE_NUMBER_TOO_NEW", 4_000_003) };
            return Err(ApiError::vm(format!("Invalid transaction: Type: Validation Code: {name} (the account is at sequence number {sequence})"), code));
        }
        self.impersonate(session, &tx.sender)?;
        let mut args = strings(&["move", "run", "--session", session, "--private-key-file", KEY, "--sender-account", &tx.sender]);
        args.extend(tx.call.cli_args());
        args.extend(strings(&["--max-gas", &tx.max_gas.to_string(), "--gas-unit-price", &tx.gas_unit_price.to_string(), "--assume-yes"]));
        let (code, stdout, stderr) = sh(&run_script(session), &args)?;
        let (result, recorded) = stdout.split_once(EVENTS_MARK).unwrap_or((&stdout, ""));
        let result = cli_result(result, &stderr, code)?;
        let status = result["vm_status"].as_str().unwrap_or("").to_string();
        let Some(success) = result["success"].as_bool() else {
            return Err(ApiError::vm(format!("Invalid transaction: {status}"), 1));
        };
        let (version, ops) = self.config().unwrap_or((0, 0));
        let zero = format!("0x{}", "0".repeat(64));
        Ok(json!({
            "type": "user_transaction",
            "version": (version + ops).to_string(),
            "hash": tx.hash,
            "state_change_hash": zero,
            "event_root_hash": zero,
            "state_checkpoint_hash": null,
            "gas_used": u64_of(&result["gas_used"]).unwrap_or(0).to_string(),
            "success": success,
            "vm_status": status,
            "accumulator_root_hash": zero,
            "changes": [],
            "sender": tx.sender,
            "sequence_number": tx.sequence_number.to_string(),
            "max_gas_amount": tx.max_gas.to_string(),
            "gas_unit_price": tx.gas_unit_price.to_string(),
            "expiration_timestamp_secs": tx.expiration_secs.to_string(),
            "payload": tx.call.payload(),
            "events": events(&serde_json::from_str(recorded.trim()).unwrap_or(Value::Null)),
            "timestamp": if session == SESSION { self.timestamp_usecs().unwrap_or(0) } else { 0 }.to_string(),
        }))
    }

    fn signed(req: &Request) -> Api<SignedTx> {
        if req.content_type != BCS_SIGNED {
            return Err(ApiError::new(415, "web_framework_error", format!("send the signed transaction as {BCS_SIGNED}, as the SDKs do")));
        }
        decode_signed(&req.body)
    }

    fn submit(&self, req: &Request) -> Api<Value> {
        let tx = Self::signed(req)?;
        let _one = self.submit.lock().expect("submit");
        let done = self.run(SESSION, &tx, true)?;
        let pending = json!({
            "hash": done["hash"],
            "sender": done["sender"],
            "sequence_number": done["sequence_number"],
            "max_gas_amount": done["max_gas_amount"],
            "gas_unit_price": done["gas_unit_price"],
            "expiration_timestamp_secs": done["expiration_timestamp_secs"],
            "payload": done["payload"],
        });
        self.transactions.lock().expect("transactions").insert(tx.hash.clone(), done);
        Ok(pending)
    }

    /// Runs the transaction on a private copy of the session and drops the copy.
    fn simulate(&self, req: &Request) -> Api<Value> {
        let tx = Self::signed(req)?;
        let _one = self.submit.lock().expect("submit");
        let (code, _, stderr) = sh(&format!("exec 9>{LOCK}; flock 9; rm -rf {SIM} && cp -a {SESSION} {SIM}"), &[])?;
        if code != 0 {
            return Err(ApiError::internal(format!("could not copy the session: {}", scrub(stderr.trim()))));
        }
        let out = match self.run(SIM, &tx, false) {
            Ok(done) => Ok(json!([done])),
            // A discarded transaction is a simulation result, not a refused request.
            Err(e) if e.code == "vm_error" => Ok(json!([{ "type": "user_transaction", "hash": tx.hash, "success": false, "vm_status": e.message, "gas_used": "0", "sender": tx.sender, "sequence_number": tx.sequence_number.to_string(), "max_gas_amount": tx.max_gas.to_string(), "gas_unit_price": tx.gas_unit_price.to_string(), "expiration_timestamp_secs": tx.expiration_secs.to_string(), "payload": tx.call.payload(), "events": [], "changes": [] }])),
            Err(e) => Err(e),
        };
        let _ = std::fs::remove_dir_all(SIM);
        out
    }

    fn transaction(&self, hash: &str) -> Api<Value> {
        let key = hash.to_ascii_lowercase();
        self.transactions.lock().expect("transactions").get(&key).cloned().ok_or_else(|| ApiError::new(404, "transaction_not_found", format!("Transaction not found by Transaction hash({hash})")))
    }

    /// The status and JSON body for one request.
    pub fn handle(&self, req: &Request) -> (u16, String) {
        let path: Vec<&str> = req.path.iter().map(String::as_str).collect();
        let address = |a: &str| parse_address(a).map_err(|e| ApiError::bad(e.message));
        let tag = |t: &str| canonical_struct_tag(t).map_err(|e| ApiError::bad(e.message));
        let out: Api<(u16, String)> = (|| {
            let ok = |v: Value| Ok((200, v.to_string()));
            match (req.method.as_str(), path.as_slice()) {
                ("OPTIONS", _) => Ok((204, String::new())),
                ("GET", []) => ok(self.ledger()?),
                ("GET", ["-", "healthy"]) => ok(json!({ "message": "chainsim aptos-rest:ok" })),
                ("GET", ["estimate_gas_price"]) => ok(json!({ "deprioritized_gas_estimate": GAS_PRICE, "gas_estimate": GAS_PRICE, "prioritized_gas_estimate": GAS_PRICE * 3 / 2 })),
                ("GET", ["accounts", a]) => ok(self.account(&address(a)?)?),
                ("GET", ["accounts", a, "resources"]) => ok(self.resources(&address(a)?)?),
                ("GET", ["accounts", a, "resource", t]) => {
                    let (a, t) = (address(a)?, tag(t)?);
                    match self.resource(&a, &t)? {
                        Value::Null => Err(ApiError::new(404, "resource_not_found", format!("Resource not found by Address({a}), Struct tag({t}) and Ledger version({})", self.config()?.0))),
                        data => ok(json!({ "type": t, "data": data })),
                    }
                }
                ("GET", ["accounts", a, "module", m]) => self.module(&address(a)?, m),
                ("POST", ["view"]) => ok(self.view_request(req)?),
                ("POST", ["transactions"]) => Ok((202, self.submit(req)?.to_string())),
                ("POST", ["transactions", "simulate"]) => ok(self.simulate(req)?),
                ("GET", ["transactions", "by_hash", h]) | ("GET", ["transactions", "wait_by_hash", h]) => ok(self.transaction(h)?),
                _ => Err(ApiError::new(404, "web_framework_error", format!("the fork's REST endpoint serves no {} /v1/{}", req.method, path.join("/")))),
            }
        })();
        match out {
            Ok(done) => done,
            Err(e) => (e.status, e.body().to_string()),
        }
    }
}

fn connection(facade: &Facade, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let (status, body) = match read_request(&mut stream) {
        Ok(Some(req)) => facade.handle(&req),
        Ok(None) => return,
        Err(e) => (e.status, e.body().to_string()),
    };
    let _ = stream.write_all(&response(status, &body, facade.chain.get().copied()));
    let _ = stream.flush();
}

/// Serves the endpoint until the process ends: one thread per connection.
pub fn serve() -> std::io::Result<()> {
    let listen = std::env::var(LISTEN_ENV).unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = TcpListener::bind(&listen)?;
    eprintln!("chainsim-aptos-rest: serving {listen}");
    let facade = Arc::new(Facade::default());
    for stream in listener.incoming().flatten() {
        let facade = facade.clone();
        std::thread::spawn(move || connection(&facade, stream));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body the Aptos TypeScript SDK 7.5.0 posted for
    /// `0x1::aptos_account::transfer(0xc5e2, 10000000)` (recorded 2026-11-12),
    /// up to the end of the raw transaction, with a zeroed Ed25519 authenticator.
    fn signed() -> Vec<u8> {
        let mut b = from_hex("5d75bf443ff1576b243eeab6b4c8b8512b445a36f226261933957b02e184cd88", "sender").unwrap();
        b.extend(1u64.to_le_bytes());
        b.push(2);
        b.extend(address_bytes("0x1").unwrap());
        b.extend([13]);
        b.extend(b"aptos_account");
        b.extend([8]);
        b.extend(b"transfer");
        b.extend([0, 2, 32]);
        b.extend(address_bytes("0xc5e2").unwrap());
        b.push(8);
        b.extend(10_000_000u64.to_le_bytes());
        b.extend(200_000u64.to_le_bytes());
        b.extend(100u64.to_le_bytes());
        b.extend(1_794_446_000u64.to_le_bytes());
        b.push(1);
        b.push(0);
        b.push(32);
        b.extend([0u8; 32]);
        b.push(64);
        b.extend([0u8; 64]);
        b
    }

    #[test]
    fn a_signed_transfer_decodes_to_a_cli_call() {
        let bytes = signed();
        let tx = decode_signed(&bytes).unwrap();
        assert_eq!(tx.sender, "0x5d75bf443ff1576b243eeab6b4c8b8512b445a36f226261933957b02e184cd88");
        assert_eq!((tx.sequence_number, tx.max_gas, tx.gas_unit_price, tx.expiration_secs, tx.chain_id), (1, 200_000, 100, 1_794_446_000, 1));
        assert_eq!(tx.call.function, "0x1::aptos_account::transfer");
        let cli = tx.call.cli_args();
        assert_eq!(cli[..3], ["--function-id", "0x1::aptos_account::transfer", "--args"]);
        assert_eq!(cli[3], format!("raw:0x{}c5e2", "0".repeat(60)));
        assert_eq!(cli[4], "raw:0x8096980000000000");
        assert_eq!(tx.hash, transaction_hash(&bytes));
        assert_eq!(tx.hash.len(), 66);
        assert!(decode_signed(&bytes[..60]).is_err());
        // A script payload (variant 0) is refused by name.
        let mut script = bytes.clone();
        script[40] = 0;
        assert!(decode_signed(&script).unwrap_err().message.contains("entry function"));
    }

    #[test]
    fn view_bodies_carry_type_tags() {
        // 0x1::coin::balance<0x1::aptos_coin::AptosCoin>(0x1), as the SDK encodes a view.
        let mut b = address_bytes("0x1").unwrap().to_vec();
        b.extend([4]);
        b.extend(b"coin");
        b.extend([7]);
        b.extend(b"balance");
        b.extend([1, 7]);
        b.extend(address_bytes("0x1").unwrap());
        b.extend([10]);
        b.extend(b"aptos_coin");
        b.extend([9]);
        b.extend(b"AptosCoin");
        b.extend([0, 1, 32]);
        b.extend(address_bytes("0x1").unwrap());
        assert_eq!(b.len(), 135, "the length the SDK posted");
        let call = decode_view(&b).unwrap();
        assert_eq!(call.function, "0x1::coin::balance");
        assert_eq!(call.type_args, ["0x1::aptos_coin::AptosCoin"]);
        assert_eq!(call.cli_args().last().unwrap(), &format!("raw:0x{}1", "0".repeat(63)));
        // Nested and primitive tags.
        let mut t = Bcs::new(&[6, 6, 2]);
        assert_eq!(t.type_tag(0).unwrap(), "vector<vector<u64>>");
        assert!(Bcs::new(&[99]).type_tag(0).is_err());
    }

    #[test]
    fn json_view_arguments_follow_the_abi() {
        assert_eq!(json_arg("address", &json!("0x1")).unwrap(), "address:0x1");
        assert_eq!(json_arg("u64", &json!("100")).unwrap(), "u64:100");
        assert_eq!(json_arg("u8", &json!(7)).unwrap(), "u8:7");
        assert_eq!(json_arg("bool", &json!(true)).unwrap(), "bool:true");
        assert_eq!(json_arg("0x1::string::String", &json!("hi")).unwrap(), "string:hi");
        assert_eq!(json_arg("0x1::object::Object<0x1::fungible_asset::Metadata>", &json!("0xa")).unwrap(), "address:0xa");
        assert_eq!(json_arg("vector<u8>", &json!("0x0102")).unwrap(), "hex:0x0102");
        assert_eq!(json_arg("vector<address>", &json!(["0x1", "0x2"])).unwrap(), "address:[\"0x1\",\"0x2\"]");
        assert!(json_arg("0x1::m::T", &json!({})).unwrap_err().message.contains("BCS"));
    }

    #[test]
    fn requests_parse_and_route() {
        let raw = b"POST /v1/view?x=1 HTTP/1.1\r\nHost: a\r\nContent-Type: application/x.aptos.view_function+bcs\r\nContent-Length: 3\r\n\r\nabcEXTRA";
        let req = read_request(&mut &raw[..]).unwrap().unwrap();
        assert_eq!((req.method.as_str(), req.path.clone(), req.content_type.as_str(), req.body.as_slice()), ("POST", vec!["view".to_string()], BCS_VIEW, &b"abc"[..]));
        // The prefix is optional and struct tags arrive percent-encoded.
        assert_eq!(route("/accounts/0x1/resource/0x1::coin::CoinInfo%3C0x1::aptos_coin::AptosCoin%3E"), ["accounts", "0x1", "resource", "0x1::coin::CoinInfo<0x1::aptos_coin::AptosCoin>"]);
        assert_eq!(route("/v1"), Vec::<String>::new());
        assert_eq!(route("/v1/"), Vec::<String>::new());
        assert!(read_request(&mut &b""[..]).unwrap().is_none());
        assert_eq!(read_request(&mut &b"POST /v1/view HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"[..]).unwrap_err().status, 411);
        assert_eq!(read_request(&mut &b"POST /v1/view HTTP/1.1\r\nContent-Length: 9\r\n\r\nabc"[..]).unwrap_err().status, 400);
        let big = format!("POST /v1/view HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert_eq!(read_request(&mut big.as_bytes()).unwrap_err().status, 413);
    }

    #[test]
    fn responses_close_and_allow_browsers() {
        let text = String::from_utf8(response(202, "{}", Some(1))).unwrap();
        assert!(text.starts_with("HTTP/1.1 202 Accepted\r\n") && text.ends_with("\r\n\r\n{}"));
        for header in ["content-length: 2\r\n", "x-aptos-chain-id: 1\r\n", "access-control-allow-origin: *\r\n", "connection: close\r\n"] {
            assert!(text.contains(header), "{header}");
        }
    }

    #[test]
    fn recorded_events_take_the_fullnode_shape() {
        let recorded = json!([
            { "V2": { "type_tag": "0x1::fungible_asset::Deposit", "event_data": { "store": "db68", "amount": 100 } } },
            { "V1": { "key": { "creation_number": 2, "account_address": "00c5e2" }, "sequence_number": 5, "type_tag": "0x1::coin::DepositEvent", "event_data": { "amount": 1 } } }
        ]);
        let out = events(&recorded);
        assert_eq!(out[0], json!({ "guid": { "creation_number": "0", "account_address": "0x0" }, "sequence_number": "0", "type": "0x1::fungible_asset::Deposit", "data": { "store": "db68", "amount": 100 } }));
        assert_eq!(out[1]["guid"], json!({ "creation_number": "2", "account_address": "0xc5e2" }));
        assert_eq!(out[1]["sequence_number"], "5");
        assert!(events(&Value::Null).is_empty());
    }

    #[test]
    fn secrets_leave_messages() {
        let secrets = ["https://key123.provider.invalid/v1/SECRETPATH".to_string(), "aptos-key-SECRET-456".to_string(), String::new()];
        let said = "error sending request for url (https://key123.provider.invalid/v1/SECRETPATH/accounts/0x1) with aptos-key-SECRET-456; curl: (6) Could not resolve host: key123.provider.invalid; see https://other.example/x";
        let out = scrub_with(said, &secrets);
        for secret in ["key123", "SECRETPATH", "SECRET-456", "provider.invalid", "other.example"] {
            assert!(!out.contains(secret), "{secret} left in {out}");
        }
        assert!(out.contains("Could not resolve host: <upstream host>"), "{out}");
        assert_eq!(scrub_with("plain", &secrets), "plain");
    }

    #[test]
    fn errors_and_scripts() {
        assert_eq!(ApiError::vm("x", 3).body(), json!({ "message": "x", "error_code": "vm_error", "vm_error_code": 3 }));
        assert_eq!(short_address(&format!("0x{}", "0".repeat(64))), "0x0");
        for script in [read_script(SESSION), run_script(SESSION)] {
            assert!(script.contains("flock 9") && script.contains("aptos \"$@\"") && !script.contains("https://"), "{script}");
        }
        assert!(read_script(SESSION).contains("rm -rf \"/data/session/[$n] view\"*"));
        assert!(run_script(SIM).contains("/data/sim/[$n] execute"));
        let e = cli_result("{\"Error\": \"Linker Error: Module 0x1::nope doesn't exist\"}", "", 1).unwrap_err();
        assert_eq!((e.status, e.code), (400, "invalid_input"));
        assert_eq!(cli_result("{\"Result\": [\"1\"]}", "", 0).unwrap(), json!(["1"]));
        assert_eq!(cli_result("", "boom", 2).unwrap_err().status, 500);
    }
}
