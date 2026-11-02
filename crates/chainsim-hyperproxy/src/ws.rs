//! WebSocket clients. The proxy accepts the upgrade on its RPC port and
//! answers each JSON-RPC message the way it answers HTTP, so a transaction
//! sent over a socket is mined into its own block and Core catches up after
//! it. `eth_subscribe` and `eth_unsubscribe` go to a socket of the client's
//! own on the engine, and the engine's notifications come back as they are.
//! Requests are answered in the order they arrive, so a client's
//! transactions keep their nonce order.

use std::net::IpAddr;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, UPGRADE};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::Proxy;

/// Methods the engine's own socket answers: subscriptions live there.
const SUBSCRIPTIONS: &[&str] = &["eth_subscribe", "eth_unsubscribe"];

/// Whether a request asks for a WebSocket upgrade.
pub fn is_upgrade(req: &Request<Incoming>) -> bool {
    req.headers().get(UPGRADE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Answers the upgrade and serves the socket once hyper hands it over.
pub fn accept(proxy: Arc<Proxy>, mut req: Request<Incoming>, peer: IpAddr) -> Response<Full<Bytes>> {
    let Some(accept) = req.headers().get(SEC_WEBSOCKET_KEY).map(|k| derive_accept_key(k.as_bytes())) else {
        return Response::builder().status(StatusCode::BAD_REQUEST).body(Full::new(Bytes::from_static(b"a WebSocket upgrade needs Sec-WebSocket-Key"))).expect("a valid response");
    };
    let upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        match upgrade.await {
            Ok(io) => session(proxy, WebSocketStream::from_raw_socket(TokioIo::new(io), Role::Server, None).await, peer).await,
            Err(e) => eprintln!("chainsim-hyperproxy: WebSocket upgrade failed: {e}"),
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(UPGRADE, "websocket")
        .header(CONNECTION, "Upgrade")
        .header(SEC_WEBSOCKET_ACCEPT, accept)
        .body(Full::new(Bytes::new()))
        .expect("a valid response")
}

fn error(id: &Value, code: i64, message: &str) -> Message {
    Message::text(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string())
}

async fn session<S>(proxy: Arc<Proxy>, socket: WebSocketStream<S>, peer: IpAddr)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut to_client, mut from_client) = socket.split();
    let (out, mut outbox) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(m) = outbox.recv().await {
            if to_client.send(m).await.is_err() {
                break;
            }
        }
    });
    let (mut to_engine, relay) = match tokio_tungstenite::connect_async(proxy.engine.ws.as_str()).await {
        Ok((engine, _)) => {
            let (to_engine, mut from_engine) = engine.split();
            let notify = out.clone();
            let relay = tokio::spawn(async move {
                while let Some(Ok(m)) = from_engine.next().await {
                    if matches!(m, Message::Text(_)) && notify.send(m).is_err() {
                        break;
                    }
                }
            });
            (Some(to_engine), Some(relay))
        }
        Err(e) => {
            eprintln!("chainsim-hyperproxy: the engine's WebSocket did not open: {e}");
            (None, None)
        }
    };
    while let Some(Ok(msg)) = from_client.next().await {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(body) = serde_json::from_str::<Value>(&text) else {
            let _ = out.send(error(&Value::Null, -32700, "parse error"));
            continue;
        };
        if body["method"].as_str().is_some_and(|m| SUBSCRIPTIONS.contains(&m)) {
            let sent = match to_engine.as_mut() {
                Some(engine) => engine.send(Message::text(text)).await.is_ok(),
                None => false,
            };
            if !sent {
                let _ = out.send(error(&body["id"], -32000, "the engine's WebSocket is not available for subscriptions"));
            }
            continue;
        }
        let answer = proxy.rpc(&body, peer).await;
        if out.send(Message::text(answer.to_string())).is_err() {
            break;
        }
    }
    if let Some(r) = relay {
        r.abort();
    }
    if let Some(mut engine) = to_engine {
        let _ = engine.close().await;
    }
    drop(out);
    let _ = writer.await;
}
