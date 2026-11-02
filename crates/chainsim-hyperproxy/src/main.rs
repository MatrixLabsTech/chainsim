//! The HyperEVM proxy. It runs in its own container in front of a HyperEVM
//! fork's anvil and takes the fork's RPC address, so every client (forge,
//! cast, a frontend, an SDK) goes through it:
//!
//! - After each request that mines or moves the chain, and before a read
//!   when the chain is past the cursor, it applies the local HyperCore with
//!   the same driver the CLI uses ([`chainsim_evm::coresync`]).
//! - It mines one block per transaction, with the gas limit of the sender's
//!   block type (small or big), and answers `eth_usingBigBlocks` and
//!   `eth_bigBlockGasPrice`.
//! - It answers `POST /info` from the copy's Core state.
//! - It holds the HyperCore lock: a chainsim command takes a lease with
//!   `chainsim_coreLock` before it changes Core state, and the proxy applies
//!   nothing while another holder has it.
//!
//! Environment: `CHAINSIM_PROXY_ENGINE` (anvil's in-network URL) and
//! `CHAINSIM_PROXY_LISTEN` (default `0.0.0.0:8545`).

mod lease;

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chainsim_core::{Error, ErrorCode, Result};
use chainsim_evm::core::{self as hc, Meta};
use chainsim_evm::coresync::{self, CoreRpc};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use lease::Leases;

/// Methods that send a transaction: the proxy mines each into its own block.
const SENDS: &[&str] = &["eth_sendTransaction", "eth_sendRawTransaction"];
/// Methods that mine or move the chain: Core catches up after them.
const MINING: &[&str] = &["evm_mine", "anvil_mine", "evm_revert", "anvil_revert", "anvil_reset", "anvil_rollback"];

/// anvil over HTTP.
struct Engine {
    client: Client<HttpConnector, Full<Bytes>>,
    url: hyper::Uri,
}

impl Engine {
    async fn post(&self, body: String) -> Result<Value> {
        let req = Request::post(self.url.clone()).header("content-type", "application/json").body(Full::new(Bytes::from(body))).map_err(|e| Error::new(ErrorCode::Internal, e.to_string()))?;
        let res = self.client.request(req).await.map_err(|e| Error::new(ErrorCode::EngineFailed, format!("engine request failed: {e}")))?;
        let bytes = res.into_body().collect().await.map_err(|e| Error::new(ErrorCode::EngineFailed, format!("engine reply failed: {e}")))?.to_bytes();
        serde_json::from_slice(&bytes).map_err(|_| Error::new(ErrorCode::EngineFailed, "the engine answered something that is not JSON"))
    }

    /// Forwards a JSON-RPC request or batch as is.
    async fn forward(&self, req: &Value) -> Result<Value> {
        self.post(req.to_string()).await
    }
}

fn rpc_error(method: &str, err: &Value) -> Error {
    let message = err.get("message").and_then(Value::as_str).unwrap_or("error");
    chainsim_core::control::classify_rpc_error(method, message, err.get("code").and_then(Value::as_i64), None)
}

#[async_trait]
impl CoreRpc for Engine {
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let v = self.post(json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string()).await?;
        match v.get("error") {
            Some(err) => Err(rpc_error(method, err)),
            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
        }
    }

    async fn batch(&self, calls: &[(&str, Value)]) -> Result<Vec<Value>> {
        let mut out = Vec::with_capacity(calls.len());
        for chunk in calls.chunks(500) {
            let body: Vec<Value> = chunk.iter().enumerate().map(|(i, (m, p))| json!({ "jsonrpc": "2.0", "id": i, "method": m, "params": p })).collect();
            let v = self.post(Value::Array(body).to_string()).await?;
            let mut replies = v.as_array().cloned().ok_or_else(|| Error::new(ErrorCode::EngineFailed, "the engine did not answer the batch with a list"))?;
            replies.sort_by_key(|r| r["id"].as_u64().unwrap_or(u64::MAX));
            for (r, (m, _)) in replies.into_iter().zip(chunk) {
                if let Some(err) = r.get("error") {
                    return Err(rpc_error(m, err));
                }
                out.push(r.get("result").cloned().unwrap_or(Value::Null));
            }
        }
        Ok(out)
    }
}

struct Proxy {
    engine: Engine,
    /// Held while the proxy applies Core or mines, so neither interleaves.
    apply: Mutex<()>,
    leases: Leases,
    /// The head the proxy last brought Core up to.
    head: AtomicU64,
    meta: Mutex<Option<Meta>>,
}

fn reply(id: &Value, result: Result<Value>) -> Value {
    match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": e.message } }),
    }
}

fn head_of(v: &Value) -> u64 {
    v.as_str().and_then(|h| u64::from_str_radix(h.trim_start_matches("0x"), 16).ok()).unwrap_or(0)
}

impl Proxy {
    /// Takes `apply` for a request from `peer`, after any lease held by
    /// someone else ends. Leases are taken under `apply` too, so checking
    /// again once it is held closes the gap between the wait and the lock.
    /// Returns the guard and whether `peer` holds the lease.
    async fn enter(&self, peer: IpAddr) -> (tokio::sync::MutexGuard<'_, ()>, bool) {
        loop {
            if !self.leases.holds(peer) {
                self.leases.wait(peer).await;
            }
            let g = self.apply.lock().await;
            if self.leases.free_for(peer) {
                return (g, self.leases.holds(peer));
            }
        }
    }

    /// Brings Core up to the head when the chain moved since the last apply.
    /// The caller holds `apply`.
    async fn sync_if_behind(&self) -> Result<()> {
        let head = head_of(&self.engine.call("eth_blockNumber", json!([])).await?);
        if head != self.head.load(Ordering::SeqCst) {
            self.sync().await?;
            self.head.store(head, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn sync(&self) -> Result<()> {
        coresync::sync_locked(&self.engine).await?;
        let head = head_of(&self.engine.call("eth_blockNumber", json!([])).await?);
        self.head.store(head, Ordering::SeqCst);
        Ok(())
    }

    /// Mines a sent transaction into a block of its sender's type. With
    /// `apply`, Core catches up after it.
    async fn send(&self, req: &Value, apply: bool) -> Result<Value> {
        let sent = self.engine.forward(req).await?;
        let Some(hash) = sent.get("result").and_then(Value::as_str).map(str::to_string) else { return Ok(sent) };
        let tx = self.engine.call("eth_getTransactionByHash", json!([hash])).await?;
        let from: alloy_primitives::Address = tx["from"].as_str().and_then(|f| f.parse().ok()).unwrap_or_default();
        let gas = head_of(&tx["gas"]);
        let state = coresync::load_state(&self.engine).await?;
        let big = state.as_ref().is_some_and(|s| s.uses_big_blocks(from));
        if let Some(why) = hc::block_refusal(gas, big) {
            let _ = self.engine.call("anvil_dropTransaction", json!([hash])).await;
            return Ok(json!({ "jsonrpc": "2.0", "id": req["id"], "error": { "code": -32000, "message": why } }));
        }
        self.engine.call("evm_setBlockGasLimit", json!([format!("0x{:x}", hc::block_gas_limit(big))])).await?;
        if big {
            let latest = self.engine.call("eth_getBlockByNumber", json!(["latest", false])).await?;
            let last_big = state.as_ref().map_or(0, |s| s.last_big_block);
            if let Some(t) = hc::big_block_time(coresync::block_time(&latest)? + 1, last_big) {
                self.engine.call("evm_setNextBlockTimestamp", json!([t])).await?;
            }
        }
        self.engine.call("evm_mine", json!([])).await?;
        // Estimates and calls see the big-block limit, as on mainnet where a
        // big-block user can deploy what a small block cannot hold.
        self.engine.call("evm_setBlockGasLimit", json!([format!("0x{:x}", hc::BIG_BLOCK_GAS)])).await?;
        if big {
            let mined = self.engine.call("eth_getBlockByNumber", json!(["latest", false])).await?;
            if let Some(mut s) = coresync::load_state(&self.engine).await? {
                s.last_big_block = coresync::block_time(&mined)?;
                coresync::store(&self.engine, &s, &[]).await?;
            }
        }
        if apply {
            self.sync().await?;
        }
        Ok(sent)
    }

    /// One JSON-RPC request from `peer`.
    async fn one(&self, req: &Value, peer: IpAddr) -> Value {
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req["method"].as_str().unwrap_or_default().to_string();
        let params = req.get("params").cloned().unwrap_or(json!([]));
        match method.as_str() {
            "chainsim_coreLock" => {
                // Wait for any apply in flight and for another holder, without
                // holding `apply` while waiting, so the holder can finish.
                let until = std::time::Instant::now() + lease::TTL;
                loop {
                    {
                        let _g = self.apply.lock().await;
                        if let Some(token) = self.leases.try_take(peer) {
                            return reply(&id, Ok(json!(token)));
                        }
                    }
                    if std::time::Instant::now() > until {
                        return reply(&id, Err(Error::new(ErrorCode::EngineFailed, "another command held the HyperCore lock for over two minutes")));
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            "chainsim_coreUnlock" => {
                let token = params.get(0).and_then(Value::as_str).unwrap_or_default();
                return reply(&id, Ok(json!(self.leases.give_back(token))));
            }
            "eth_bigBlockGasPrice" => return reply(&id, Ok(json!(format!("0x{:x}", hc::BIG_BLOCK_GAS_PRICE)))),
            _ => {}
        }
        let (_g, holder) = self.enter(peer).await;
        let result: Result<Value> = async {
            if method == "eth_usingBigBlocks" {
                let who: alloy_primitives::Address = params.get(0).and_then(Value::as_str).and_then(|a| a.parse().ok()).ok_or_else(|| Error::new(ErrorCode::Usage, "eth_usingBigBlocks takes an address"))?;
                return Ok(json!({ "jsonrpc": "2.0", "id": id, "result": coresync::load_state(&self.engine).await?.is_some_and(|s| s.uses_big_blocks(who)) }));
            }
            if SENDS.contains(&method.as_str()) {
                return self.send(req, !holder).await;
            }
            if !holder {
                self.sync_if_behind().await?;
            }
            let out = self.engine.forward(req).await?;
            if !holder && MINING.contains(&method.as_str()) {
                self.sync().await?;
            }
            Ok(out)
        }
        .await;
        result.unwrap_or_else(|e| reply(&id, Err(e)))
    }

    /// A JSON-RPC body: one request or a batch.
    async fn rpc(&self, body: &Value, peer: IpAddr) -> Value {
        match body.as_array() {
            Some(batch) => {
                let mut out = Vec::with_capacity(batch.len());
                for req in batch {
                    out.push(Box::pin(self.one(req, peer)).await);
                }
                Value::Array(out)
            }
            None => self.one(body, peer).await,
        }
    }

    /// `POST /info`: the copy's Core state in the info API's shapes.
    async fn info(&self, body: &Value, peer: IpAddr) -> std::result::Result<Value, (StatusCode, String)> {
        let _g = self.enter(peer).await.0;
        let internal = |e: Error| (StatusCode::INTERNAL_SERVER_ERROR, e.message);
        self.sync_if_behind().await.map_err(internal)?;
        let state = coresync::load_state(&self.engine).await.map_err(internal)?.ok_or((StatusCode::SERVICE_UNAVAILABLE, "the copy has no HyperCore state yet".to_string()))?;
        let meta = {
            let mut cached = self.meta.lock().await;
            if cached.as_ref().is_none_or(|m| m.universe.is_empty()) {
                *cached = Some(coresync::load_meta(&self.engine).await.map_err(internal)?);
            }
            cached.clone().unwrap_or_default()
        };
        let latest = self.engine.call("eth_getBlockByNumber", json!(["latest", false])).await.map_err(internal)?;
        let time_ms = coresync::block_time(&latest).map_err(internal)? * 1000;
        state.info_request(&meta, body, time_ms).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))
    }
}

fn respond(status: StatusCode, body: Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("access-control-allow-origin", "*")
        .header("access-control-allow-methods", "POST, OPTIONS")
        .header("access-control-allow-headers", "content-type")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("a valid response")
}

async fn serve(proxy: Arc<Proxy>, req: Request<Incoming>, peer: IpAddr) -> std::result::Result<Response<Full<Bytes>>, Infallible> {
    if req.method() == Method::OPTIONS {
        return Ok(respond(StatusCode::NO_CONTENT, Value::Null));
    }
    if req.method() != Method::POST {
        return Ok(respond(StatusCode::METHOD_NOT_ALLOWED, json!({ "error": "POST a JSON-RPC request, or an info request to /info" })));
    }
    let info = req.uri().path().trim_end_matches('/') == "/info";
    let bytes = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return Ok(respond(StatusCode::BAD_REQUEST, json!({ "error": e.to_string() }))),
    };
    let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(respond(StatusCode::BAD_REQUEST, json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } })));
    };
    if info {
        return Ok(match proxy.info(&body, peer).await {
            Ok(v) => respond(StatusCode::OK, v),
            Err((status, message)) => respond(status, json!({ "error": message })),
        });
    }
    Ok(respond(StatusCode::OK, proxy.rpc(&body, peer).await))
}

#[tokio::main]
async fn main() {
    let engine_url = std::env::var("CHAINSIM_PROXY_ENGINE").unwrap_or_else(|_| "http://127.0.0.1:8546".into());
    let listen: SocketAddr = std::env::var("CHAINSIM_PROXY_LISTEN").ok().and_then(|l| l.parse().ok()).unwrap_or_else(|| "0.0.0.0:8545".parse().expect("address"));
    let engine = Engine { client: Client::builder(TokioExecutor::new()).build_http(), url: engine_url.parse().expect("CHAINSIM_PROXY_ENGINE is a URL") };
    // The engine first: the proxy takes over mining once it answers.
    let mut ready = false;
    for _ in 0..360 {
        if engine.call("eth_blockNumber", json!([])).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    if !ready || engine.call("evm_setAutomine", json!([false])).await.is_err() {
        eprintln!("chainsim-hyperproxy: the engine at {engine_url} did not answer");
        std::process::exit(1);
    }
    let proxy = Arc::new(Proxy { engine, apply: Mutex::new(()), leases: Leases::default(), head: AtomicU64::new(0), meta: Mutex::new(None) });
    let listener = TcpListener::bind(listen).await.expect("bind the proxy port");
    eprintln!("chainsim-hyperproxy: serving {listen} in front of the engine");
    loop {
        let Ok((stream, addr)) = listener.accept().await else { continue };
        let proxy = proxy.clone();
        tokio::spawn(async move {
            let service = hyper::service::service_fn(move |req| serve(proxy.clone(), req, addr.ip()));
            let _ = hyper::server::conn::http1::Builder::new().keep_alive(true).serve_connection(TokioIo::new(stream), service).await;
        });
    }
}
