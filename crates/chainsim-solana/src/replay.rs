//! `time --replay`: a past range of a feed's real prices played onto the
//! fork. Each step moves the fork's clock forward by the time between two
//! prices and writes the price with a publish time equal to the Clock. The
//! clock only moves forward, so the past range plays from the fork's current
//! time on.

use chainsim_core::docker;
use chainsim_core::error::upstream_next;
use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

use crate::price::{self, from_raw, to_raw};
use crate::time::clock;
use crate::SolanaAdapter;

/// Where the prices come from, as results and `status` name it.
pub const SOURCE: &str = "Coinbase Exchange 1-minute candles, close price";
const CANDLES: &str = "https://api.exchange.coinbase.com/products";
/// One candle a minute.
const STEP: u64 = 60;
/// The most candles the source returns for one request.
const PAGE: u64 = 300;
/// The longest range one replay takes: a day of minutes.
const MAX_STEPS: u64 = 1440;

/// The source's product for a feed symbol, or None when it has no such market.
pub fn product(symbol: &str) -> Option<String> {
    match symbol {
        "SOL/USD" | "BTC/USD" | "ETH/USD" => Some(symbol.replace('/', "-")),
        _ => None,
    }
}

/// The candle requests for `[from, to)`: (start, end) pairs, both ends included by the source.
pub fn pages(from: u64, to: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut start = from;
    while start < to {
        let end = (start + (PAGE - 1) * STEP).min(to - STEP);
        out.push((start, end));
        start = end + STEP;
    }
    out
}

/// The (time, close) pairs of a candle answer, `[[time, low, high, open, close, volume], ...]`, oldest first.
pub fn candles(body: &str) -> Result<Vec<(u64, String)>> {
    let bad = |what: String| Error::new(ErrorCode::UpstreamUnavailable, format!("the price source answered {what}")).next("retry later");
    let v: Value = serde_json::from_str(body).map_err(|_| bad("with a body that is not JSON".into()))?;
    let rows = v.as_array().ok_or_else(|| bad(v["message"].as_str().unwrap_or("with no candles").to_string()))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        match (row[0].as_u64(), row[4].as_f64()) {
            (Some(time), Some(_)) => out.push((time, row[4].to_string())),
            _ => return Err(bad(format!("with a candle that reads {row}"))),
        }
    }
    out.sort();
    Ok(out)
}

pub async fn replay(adapter: &SolanaAdapter, name: &str, feed: &str, from: u64, to: u64) -> Result<Value> {
    let (feed, account) = price::resolve(feed)?;
    let product = product(&feed).ok_or_else(|| Error::unsupported(&format!("time --replay for {feed}"), format!("the price source has SOL/USD, BTC/USD and ETH/USD. chainsim set {name} price <feed> <price> writes one price")))?;
    let (from, to) = (from / STEP * STEP, to / STEP * STEP);
    if to <= from {
        return Err(Error::new(ErrorCode::Usage, "--to must be at least a minute after --from"));
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if to > now {
        return Err(Error::new(ErrorCode::Usage, "a replay takes a range in the past: --to is later than now"));
    }
    if (to - from) / STEP > MAX_STEPS {
        return Err(Error::new(ErrorCode::Usage, format!("a replay takes at most {MAX_STEPS} minutes, one day: split a longer range into several replays")));
    }
    let (handle, profile) = adapter.open(name).await?;
    let rpc = adapter.rpc(&handle, &profile);
    let mut data = price::read(&rpc, &account).await?;
    let old = price::parse(&data).expect("read checked it");

    // The whole range is read before the fork changes, so a failed read leaves it as it was.
    let mut prices = Vec::new();
    for (start, end) in pages(from, to) {
        let url = format!("{CANDLES}/{product}/candles?granularity={STEP}&start={start}&end={end}");
        let argv = ["curl", "-sS", "--max-time", "30", "-H", "User-Agent: chainsim", &url].map(String::from);
        let out = adapter.docker.exec(&docker::control_container(name), &argv).await?;
        if out.exit_code != 0 {
            return Err(Error::new(ErrorCode::UpstreamUnavailable, format!("the price source did not answer (curl exit {})", out.exit_code)).next(upstream_next(None, false)));
        }
        prices.extend(candles(&out.stdout)?.into_iter().filter(|(t, _)| (from..to).contains(t)));
    }
    if prices.is_empty() {
        return Err(Error::new(ErrorCode::Usage, format!("the price source has no {feed} prices between {from} and {to}: pick a range in the past")));
    }

    let (_, started) = clock(&rpc).await?;
    let first = prices[0].0;
    let mut applied = Vec::with_capacity(prices.len());
    let mut slot = 0;
    let mut failed = None;
    for (time, close) in &prices {
        let step = async {
            if *time > first {
                rpc.call("surfnet_timeTravel", json!([{ "absoluteTimestamp": (started + time - first) * 1000 }])).await?;
            }
            let (at_slot, now) = clock(&rpc).await?;
            let raw = i64::try_from(to_raw(close, old.exponent)?).map_err(|_| Error::new(ErrorCode::Internal, format!("the source price {close} is too large")))?;
            price::write(&mut data, raw, old.conf, now as i64, at_slot);
            let read = price::store(&rpc, &account, &data).await?;
            Ok::<_, Error>((at_slot, json!({ "time": time, "price": from_raw(read.price as u64, read.exponent), "clock": read.publish_time })))
        };
        match step.await {
            Ok((at_slot, entry)) => {
                slot = at_slot;
                applied.push(entry);
            }
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    // A replay that stopped midway has still moved the clock and the price, so it is listed too.
    adapter.record(name, json!({ "kind": "replay", "feed": feed, "account": account, "from": from, "to": to, "steps": applied.len(), "of": prices.len(), "source": SOURCE })).await?;
    if let Some(e) = failed {
        return Err(Error::new(e.code, format!("the replay stopped after {} of {} steps: {}", applied.len(), prices.len(), e.message)).next(format!("chainsim status {name} shows the fork's state")));
    }
    let (_, ended) = clock(&rpc).await?;
    Ok(json!({
        "kind": "replay", "feed": feed, "account": account, "source": format!("{SOURCE}, {product}"),
        "from": from, "to": to, "steps": applied.len(),
        "clock": { "from": started, "to": ended }, "slot": slot,
        "applied": applied,
        "note": chainsim_core::mods::NOTE,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_is_read_in_pages_of_300_minutes() {
        assert_eq!(pages(0, 3600), [(0, 3540)]);
        assert_eq!(pages(0, 60), [(0, 0)]);
        assert_eq!(pages(600, 600 + 86400), [(600, 18540), (18600, 36540), (36600, 54540), (54600, 72540), (72600, 86940)]);
        let day: u64 = pages(600, 600 + 86400).iter().map(|(a, b)| (b - a) / STEP + 1).sum();
        assert_eq!(day, 1440);
    }

    #[test]
    fn candles_give_the_close_oldest_first() {
        let body = "[[1794406920,83.78,83.85,83.85,83.78,337.77636295],[1794406860,83.83,83.9,83.86,83.86,201.703616]]";
        assert_eq!(candles(body).unwrap(), [(1794406860, "83.86".to_string()), (1794406920, "83.78".to_string())]);
        assert_eq!(to_raw(&candles(body).unwrap()[0].1, -8).unwrap(), 8_386_000_000);
        assert_eq!(candles("[]").unwrap(), []);
        for bad in ["{\"message\":\"NotFound\"}", "<html>", "[[1,2]]"] {
            assert_eq!(candles(bad).unwrap_err().code, ErrorCode::UpstreamUnavailable, "{bad}");
        }
    }

    #[test]
    fn only_feeds_the_source_trades_replay() {
        assert_eq!(product("SOL/USD").as_deref(), Some("SOL-USD"));
        assert_eq!(product("BTC/USD").as_deref(), Some("BTC-USD"));
        assert_eq!(product("USDC/USD"), None);
        assert_eq!(product("7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE"), None);
    }
}
