//! The control path. The host cannot route to a Docker bridge network, so
//! every engine call runs as a `docker exec` of curl in the fork's control
//! sidecar (design section 4, owner ask Q5). Only this module reaches the
//! engine or the upstream: replacing the sidecar with a CLI-only loopback port
//! means replacing `post` alone.

use std::time::Duration;

use serde_json::{json, Value};

use crate::docker::{Docker, UPSTREAM_ENV};
use crate::error::{upstream_next, Error, ErrorCode, Result};

/// Where a call goes.
#[derive(Debug, Clone)]
pub enum Target {
    /// The engine, by its in-network URL.
    Engine(String),
    /// The upstream RPC, whose URL lives only in the sidecar environment.
    Upstream,
}

/// The control path of one fork.
pub struct Control<'a> {
    pub docker: &'a Docker,
    /// The control sidecar container.
    pub container: String,
    /// The upstream URL, only to scrub it from messages.
    pub secret: Option<String>,
    /// The profile's upstream variable, named in `error.next` on exit 5.
    pub env: Option<String>,
}

/// A raw HTTP answer from the control path.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl<'a> Control<'a> {
    pub fn new(docker: &'a Docker, container: impl Into<String>, secret: Option<String>) -> Self {
        Self { docker, container: container.into(), secret, env: None }
    }

    /// Names the profile's upstream variable in exit-5 advice.
    pub fn with_env(mut self, env: impl Into<String>) -> Self {
        self.env = Some(env.into());
        self
    }

    /// POSTs a JSON body to the target and returns the HTTP status and body.
    /// The swap point for owner ask Q5.
    pub async fn post(&self, target: &Target, body: &str, timeout: Duration) -> Result<Reply> {
        const MARK: &str = "\n@@chainsim-http-status@@";
        let secs = timeout.as_secs().max(1).to_string();
        let mut argv: Vec<String> = vec!["curl".into(), "-sS".into(), "--max-time".into(), secs, "-X".into(), "POST".into(), "-H".into(), "content-type: application/json".into(), "-w".into(), format!("{MARK}%{{http_code}}")];
        let argv = match target {
            Target::Engine(url) => {
                argv.extend(["--data-raw".into(), body.to_string(), url.clone()]);
                argv
            }
            Target::Upstream => {
                // The URL is expanded inside the sidecar, so it is never in an argument.
                let script = format!("exec {} --data-raw \"$1\" \"${UPSTREAM_ENV}\"", argv.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" "));
                vec!["sh".into(), "-c".into(), script, "sh".into(), body.to_string()]
            }
        };
        let out = self.docker.exec(&self.container, &argv).await?;
        let upstream = matches!(target, Target::Upstream);
        if out.exit_code != 0 {
            let msg = self.scrub(out.stderr.trim());
            let code = match out.exit_code {
                // 28 timeout, 6 cannot resolve, 7 cannot connect, 35 and 60 TLS, 52 and 56 empty or reset.
                28 | 6 | 7 | 35 | 52 | 56 | 60 if upstream => ErrorCode::UpstreamUnavailable,
                28 => ErrorCode::UpstreamUnavailable,
                _ => ErrorCode::EngineFailed,
            };
            let what = if upstream { "upstream" } else { "engine" };
            let mut e = Error::new(code, format!("{what} call failed (curl exit {}): {msg}", out.exit_code));
            if code == ErrorCode::UpstreamUnavailable {
                e = e.next(upstream_next(self.env.as_deref(), false));
            }
            return Err(e);
        }
        let (body, status) = out.stdout.rsplit_once(MARK).ok_or_else(|| Error::new(ErrorCode::Internal, "control reply has no status"))?;
        Ok(Reply { status: status.trim().parse().unwrap_or(0), body: body.to_string() })
    }

    /// One JSON-RPC call. Returns `result`, or maps the RPC error to an error code.
    pub async fn rpc(&self, target: &Target, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string();
        let reply = self.post(target, &body, timeout).await?;
        let upstream = matches!(target, Target::Upstream);
        if reply.status == 429 {
            return Err(Error::new(ErrorCode::RateLimited, format!("{method}: rate limited (HTTP 429)")).next(upstream_next(self.env.as_deref(), true)));
        }
        let v: Value = serde_json::from_str(&reply.body).map_err(|_| {
            let code = if upstream || reply.status >= 500 { ErrorCode::UpstreamUnavailable } else { ErrorCode::EngineFailed };
            let e = Error::new(code, format!("{method}: HTTP {} with a body that is not JSON-RPC", reply.status));
            if code == ErrorCode::UpstreamUnavailable {
                e.next(upstream_next(self.env.as_deref(), false))
            } else {
                e
            }
        })?;
        if let Some(err) = v.get("error") {
            let message = self.scrub(err.get("message").and_then(Value::as_str).unwrap_or("error"));
            return Err(classify_rpc_error(method, &message, err.get("code").and_then(Value::as_i64), self.env.as_deref()));
        }
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Removes the provider URL and its host, and nothing else, from engine
    /// output. The host goes too: some providers put the key in a subdomain,
    /// and curl names the host on its own.
    pub fn scrub_secret(&self, s: &str) -> String {
        let Some(secret) = self.secret.as_deref().filter(|x| !x.is_empty()) else { return s.to_string() };
        let s = s.replace(secret, "<upstream>");
        match url_host(secret) {
            Some(host) => s.replace(host, "<upstream host>"),
            None => s,
        }
    }

    /// Removes the upstream URL, its host, and any other http(s) URL from a message.
    pub fn scrub(&self, s: &str) -> String {
        redact_urls(&self.scrub_secret(s))
    }
}

/// Maps an engine's JSON-RPC error to the contract's codes. Upstream trouble
/// surfaces through the engine as text, so it is matched on the message.
pub fn classify_rpc_error(method: &str, message: &str, code: Option<i64>, env: Option<&str>) -> Error {
    let lower = message.to_lowercase();
    let rpc_code = code.map(|c| format!(" ({c})")).unwrap_or_default();
    if lower.contains("429") || lower.contains("rate limit") || lower.contains("too many requests") || code == Some(-32005) {
        return Error::new(ErrorCode::RateLimited, format!("{method}: {message}{rpc_code}")).next(upstream_next(env, true));
    }
    let upstream_words = ["error sending request", "failed to get", "timed out", "timeout", "connection", "fork", "upstream", "-32014"];
    if upstream_words.iter().any(|w| lower.contains(w)) && !lower.contains("revert") {
        return Error::new(ErrorCode::UpstreamUnavailable, format!("{method}: {message}{rpc_code}")).next(upstream_next(env, false));
    }
    Error::new(ErrorCode::RpcError, format!("{method}: {message}{rpc_code}"))
}

/// Replaces every `http(s)://` and `ws(s)://` URL with `<url>`: an upstream
/// URL may carry a provider key in its path.
pub fn redact_urls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let next = ["https://", "http://", "wss://", "ws://"].iter().filter_map(|p| rest.find(p)).min();
        match next {
            None => {
                out.push_str(rest);
                return out;
            }
            Some(i) => {
                out.push_str(&rest[..i]);
                let tail = &rest[i..];
                let end = tail.find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ')' || c == ',').unwrap_or(tail.len());
                out.push_str("<url>");
                rest = &tail[end..];
            }
        }
    }
}

/// The host of a URL, without user info or port.
fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

/// Quotes one argument for `sh -c`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_urls() {
        assert_eq!(redact_urls("failed https://x.io/v2/KEY: boom"), "failed <url> boom");
        assert_eq!(redact_urls("a (http://h:1) b"), "a (<url>) b");
        assert_eq!(redact_urls("plain"), "plain");
    }

    #[test]
    fn classifies() {
        assert_eq!(classify_rpc_error("m", "HTTP 429 Too Many Requests", None, None).code, ErrorCode::RateLimited);
        assert_eq!(classify_rpc_error("m", "error sending request for url", None, None).code, ErrorCode::UpstreamUnavailable);
        assert_eq!(classify_rpc_error("m", "execution reverted", Some(3), None).code, ErrorCode::RpcError);
    }

    #[test]
    fn exit_5_next_names_the_profile_variable() {
        let env = Some("CHAINSIM_ARC_MAINNET_RPC");
        let rate = classify_rpc_error("m", "HTTP 429 Too Many Requests", None, env);
        assert_eq!(rate.next.as_deref(), Some("retry later, or export CHAINSIM_ARC_MAINNET_RPC=<provider URL> for a higher rate limit"));
        let down = classify_rpc_error("m", "error sending request for url", None, env);
        assert_eq!(down.next.as_deref(), Some("retry, or export CHAINSIM_ARC_MAINNET_RPC=<provider URL>"));
        assert_eq!(down.code.exit_code(), 5);
    }

    #[test]
    fn hosts() {
        assert_eq!(url_host("https://abc123.quiknode.pro/KEY/"), Some("abc123.quiknode.pro"));
        assert_eq!(url_host("http://u:p@127.0.0.1:9/v2/K"), Some("127.0.0.1"));
        assert_eq!(url_host("nope"), None);
    }

    #[test]
    fn quotes() {
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
    }
}
