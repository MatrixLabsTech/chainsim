//! The Sui control path: grpcurl in the fork's control sidecar, run through
//! `docker exec`. The host cannot route to the fork network (design section
//! 4), so every gRPC call and every upstream GraphQL query runs there.

use std::time::Duration;

use chainsim_core::control::redact_urls;
use chainsim_core::docker::{Docker, UPSTREAM_ENV};
use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

pub struct Grpc<'a> {
    pub docker: &'a Docker,
    /// The control sidecar container.
    pub container: String,
    /// The engine as `host:port` on the fork network.
    pub target: String,
    /// A provider URL from the environment, only to scrub it from messages.
    pub secret: Option<String>,
    /// The profile's upstream variable, named in `next` on upstream failures.
    pub upstream_env: String,
}

/// The `next` for an upstream failure (exit 5): the variable to set, never its value.
pub fn upstream_next(upstream_env: &str) -> String {
    format!("retry, or set {upstream_env} to a provider GraphQL URL; the public endpoint rate-limits heavy runs (chainsim describe sui-mainnet)")
}

impl<'a> Grpc<'a> {
    /// One unary gRPC call with a protobuf-JSON body. Returns the response as JSON.
    pub async fn call(&self, method: &str, body: &Value, timeout: Duration) -> Result<Value> {
        let argv: Vec<String> = vec![
            "grpcurl".into(),
            "-plaintext".into(),
            "-max-time".into(),
            timeout.as_secs().max(1).to_string(),
            "-d".into(),
            body.to_string(),
            self.target.clone(),
            method.into(),
        ];
        let out = self.docker.exec(&self.container, &argv).await?;
        if out.exit_code == 0 {
            let text = out.stdout.trim();
            if text.is_empty() {
                return Ok(json!({}));
            }
            return serde_json::from_str(text).map_err(|_| Error::new(ErrorCode::EngineFailed, format!("{method}: the reply is not JSON")));
        }
        Err(classify(method, out.exit_code, &self.scrub(&out.stderr), &self.upstream_env))
    }

    /// True when `grpc.health.v1.Health/Check` answers SERVING.
    pub async fn healthy(&self) -> bool {
        self.call("grpc.health.v1.Health/Check", &json!({}), Duration::from_secs(3)).await.is_ok_and(|v| v["status"] == "SERVING")
    }

    /// The RPC names a service lists through the live fork's reflection.
    pub async fn describe_rpcs(&self, service: &str) -> Result<Vec<String>> {
        let argv: Vec<String> = vec!["grpcurl".into(), "-plaintext".into(), self.target.clone(), "describe".into(), service.into()];
        let out = self.docker.exec(&self.container, &argv).await?;
        if out.exit_code != 0 {
            return Err(Error::new(ErrorCode::EngineFailed, format!("describe {service}: {}", self.scrub(out.stderr.trim()))));
        }
        Ok(rpc_names(&out.stdout))
    }

    /// One query to the upstream GraphQL service. Its URL lives only in the
    /// sidecar environment, so it is never in an argument.
    pub async fn upstream_graphql(&self, query: &str, timeout: Duration) -> Result<Value> {
        let body = json!({ "query": query }).to_string();
        let script = format!("exec wget -q -O - -T {} --header 'content-type: application/json' --post-data \"$1\" \"${UPSTREAM_ENV}\"", timeout.as_secs().max(1));
        let argv: Vec<String> = vec!["sh".into(), "-c".into(), script, "sh".into(), body];
        let out = self.docker.exec(&self.container, &argv).await?;
        let next = upstream_next(&self.upstream_env);
        if out.exit_code != 0 {
            let msg = self.scrub(out.stderr.trim());
            let code = if msg.contains("429") { ErrorCode::RateLimited } else { ErrorCode::UpstreamUnavailable };
            return Err(Error::new(code, format!("upstream GraphQL failed (wget exit {}): {msg}", out.exit_code)).next(next.clone()));
        }
        let v: Value = serde_json::from_str(out.stdout.trim()).map_err(|_| Error::new(ErrorCode::UpstreamUnavailable, "upstream GraphQL reply is not JSON").next(next.clone()))?;
        if let Some(errors) = v.get("errors").filter(|e| e.as_array().is_some_and(|a| !a.is_empty())) {
            return Err(Error::new(ErrorCode::UpstreamUnavailable, format!("upstream GraphQL error: {}", self.scrub(&errors.to_string()))).next(next));
        }
        Ok(v.get("data").cloned().unwrap_or(Value::Null))
    }

    pub fn scrub(&self, s: &str) -> String {
        scrub(s, self.secret.as_deref())
    }
}

/// Removes a provider URL and its host from a message, then every other URL.
pub fn scrub(s: &str, secret: Option<&str>) -> String {
    let mut s = s.to_string();
    if let Some(secret) = secret.filter(|x| !x.is_empty()) {
        s = s.replace(secret, "<upstream>");
        // Tools such as wget name only the host, and a provider may put its key there.
        if let Some(host) = url_host(secret) {
            s = s.replace(host, "<upstream-host>");
        }
    }
    redact_urls(&s)
}

/// The host of a URL, without scheme, credentials, port or path.
fn url_host(url: &str) -> Option<&str> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

/// Maps a failed grpcurl run to the contract's codes. grpcurl exits with
/// 64 plus the gRPC status code when the server answered with an error, and
/// with 1 when it could not reach the server or build the request.
pub fn classify(method: &str, exit_code: i64, stderr: &str, upstream_env: &str) -> Error {
    let message = stderr.lines().find_map(|l| l.trim().strip_prefix("Message: ")).unwrap_or(stderr.trim());
    match exit_code {
        // ResourceExhausted
        72 => Error::new(ErrorCode::RateLimited, format!("{method}: {message}")).next(upstream_next(upstream_env)),
        // Unavailable, DeadlineExceeded
        78 | 68 => Error::new(ErrorCode::EngineFailed, format!("{method}: {message}")).next("chainsim status <name>; if the engine stopped, chainsim down <name> then chainsim up <chain>"),
        c if c > 64 => {
            let lower = message.to_lowercase();
            if lower.contains("error sending request") || lower.contains("graphql") || lower.contains("upstream") {
                Error::new(ErrorCode::UpstreamUnavailable, format!("{method}: {message}")).next(upstream_next(upstream_env))
            } else {
                Error::new(ErrorCode::RpcError, format!("{method}: {message}"))
            }
        }
        _ if message.contains("does not include a method") || message.contains("not expose service") || message.contains("bad input") || message.contains("error getting request data") => {
            Error::new(ErrorCode::Usage, format!("{method}: {message}")).next("chainsim call <name> list, or chainsim call <name> describe <service>")
        }
        _ => Error::new(ErrorCode::EngineFailed, format!("{method}: {message}")).next("chainsim status <name>; if the engine stopped, chainsim down <name> then chainsim up <chain>"),
    }
}

/// The RPC names in proto or `grpcurl describe` text: `rpc Name (` or `rpc Name(`.
pub fn rpc_names(text: &str) -> Vec<String> {
    let mut names: Vec<String> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("rpc "))
        .filter_map(|rest| rest.split(|c: char| c == '(' || c.is_whitespace()).next())
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_grpcurl_exits() {
        let not_found = "ERROR:\n  Code: NotFound\n  Message: Object 0x1234 not found\n";
        let e = classify("m", 69, not_found, "ENV");
        assert_eq!(e.code, ErrorCode::RpcError);
        assert_eq!(e.message, "m: Object 0x1234 not found");
        let limited = classify("m", 72, "ERROR:\n  Code: ResourceExhausted\n  Message: slow down", "ENV");
        assert_eq!(limited.code, ErrorCode::RateLimited);
        assert!(limited.next.as_deref().is_some_and(|n| n.contains("set ENV to a provider")), "{limited:?}");
        let upstream = classify("m", 77, "ERROR:\n  Code: Internal\n  Message: error sending request for url", "ENV");
        assert_eq!(upstream.code, ErrorCode::UpstreamUnavailable);
        assert!(upstream.next.as_deref().is_some_and(|n| n.contains("ENV")));
        assert_eq!(classify("m", 78, "", "ENV").code, ErrorCode::EngineFailed);
        assert_eq!(classify("m", 1, "Failed to dial target host \"sui:9001\": connection refused", "ENV").code, ErrorCode::EngineFailed);
        assert_eq!(classify("m", 1, "service \"x\" does not include a method named \"NoSuch\"", "ENV").code, ErrorCode::Usage);
    }

    #[test]
    fn scrub_hides_the_provider_host() {
        assert_eq!(url_host("https://k3y.sui.example.com:443/graphql?x=1"), Some("k3y.sui.example.com"));
        assert_eq!(url_host("https://u:p@h.example/x"), Some("h.example"));
        assert_eq!(scrub("wget: bad address 'k3y.sui.example.com'", Some("https://k3y.sui.example.com/graphql")), "wget: bad address '<upstream-host>'");
    }

    #[test]
    fn reads_rpc_names_from_proto_and_describe() {
        let describe = "service ForkingService {\n  rpc AdvanceCheckpoint ( .sui.forking.v1alpha.AdvanceCheckpointRequest ) returns ( .x );\n  rpc GetStatus ( .a ) returns ( .b );\n}";
        assert_eq!(rpc_names(describe), vec!["AdvanceCheckpoint", "GetStatus"]);
        assert_eq!(rpc_names("  rpc AdvanceClock(AdvanceClockRequest) returns (X);"), vec!["AdvanceClock"]);
    }
}
