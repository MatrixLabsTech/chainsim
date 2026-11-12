//! `time --replay`: a range of a feed's prices played onto the fork. Each
//! step moves the fork's clock forward by the time between two prices and
//! writes the price with a publish time equal to the Clock. The clock only
//! moves forward, so a past range plays from the fork's current time on.
//!
//! The prices come from Hyperliquid's 1-minute perp candles, or from a CSV
//! file of the user's own data.

use std::time::Duration;

use chainsim_core::control::Target;
use chainsim_core::error::upstream_next;
use chainsim_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

use crate::price::{self, from_raw, to_raw};
use crate::time::clock;
use crate::SolanaAdapter;

/// Where the fetched prices come from, as results and `status` name it.
pub const SOURCE: &str = "Hyperliquid 1-minute perp candles, close price";
/// How results and `status` name prices read from a file.
pub const FILE_SOURCE: &str = "a CSV file of unix seconds and price";
const INFO: &str = "https://api.hyperliquid.xyz/info";
/// One candle a minute.
const STEP: u64 = 60;
/// The longest fetched range one replay takes: a day of minutes.
const MAX_STEPS: u64 = 1440;
/// The most rows one file replay takes.
const MAX_ROWS: usize = 10_000;

/// The source's perp for a feed symbol, or None when it has no such market.
pub fn coin(symbol: &str) -> Option<&'static str> {
    match symbol {
        "SOL/USD" => Some("SOL"),
        "BTC/USD" => Some("BTC"),
        "ETH/USD" => Some("ETH"),
        _ => None,
    }
}

/// The (time, close) pairs of a candle answer, `[{"t": <ms>, "c": "<close>", ...}, ...]`, oldest first.
pub fn candles(body: &str) -> Result<Vec<(u64, String)>> {
    let bad = |what: String| Error::new(ErrorCode::UpstreamUnavailable, format!("the price source answered {what}")).next("retry later");
    let v: Value = serde_json::from_str(body).map_err(|_| bad("with a body that is not JSON".into()))?;
    let rows = v.as_array().ok_or_else(|| bad("with no candle list".into()))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        match (row["t"].as_u64(), row["c"].as_str()) {
            (Some(ms), Some(close)) => out.push((ms / 1000, close.to_string())),
            _ => return Err(bad(format!("with a candle that reads {row}"))),
        }
    }
    out.sort();
    Ok(out)
}

/// The rows of a price file: `<unix seconds>,<price>` a line, oldest first.
/// Blank lines and one header line are skipped.
pub fn parse_csv(text: &str) -> Result<Vec<(u64, String)>> {
    let bad = |n: usize, line: &str| Error::new(ErrorCode::Usage, format!("line {n} of the price file reads {line:?}: use <unix seconds>,<price>, such as 1794404160,84.04"));
    let mut out: Vec<(u64, String)> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row = line.split_once(',').map(|(t, p)| (t.trim().parse::<u64>(), p.trim()));
        match row {
            Some((Ok(time), price)) if !price.is_empty() && price.chars().all(|c| c.is_ascii_digit() || c == '.') => {
                if out.last().is_some_and(|(last, _)| *last >= time) {
                    return Err(Error::new(ErrorCode::Usage, format!("line {} of the price file is not later than the line before it: sort the rows by time", i + 1)));
                }
                out.push((time, price.to_string()));
            }
            // A header, such as time,price.
            _ if out.is_empty() && !line.starts_with(|c: char| c.is_ascii_digit()) => {}
            _ => return Err(bad(i + 1, line)),
        }
    }
    if out.is_empty() {
        return Err(Error::new(ErrorCode::Usage, "the price file has no rows: use <unix seconds>,<price> a line"));
    }
    if out.len() > MAX_ROWS {
        return Err(Error::new(ErrorCode::Usage, format!("the price file has {} rows, and a replay takes at most {MAX_ROWS}", out.len())));
    }
    Ok(out)
}

/// Replays `[from, to)` of a feed from the price source, one step a minute.
pub async fn replay(adapter: &SolanaAdapter, name: &str, feed: &str, from: u64, to: u64) -> Result<Value> {
    let (feed, account) = price::resolve(feed)?;
    let coin = coin(&feed).ok_or_else(|| Error::unsupported(&format!("time --replay for {feed} without --file"), format!("the price source has SOL/USD, BTC/USD and ETH/USD. chainsim time {name} --replay {feed} --file <csv> plays your own prices")))?;
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
    // Both ends of the range are included by the source.
    let body = json!({ "type": "candleSnapshot", "req": { "coin": coin, "interval": "1m", "startTime": from * 1000, "endTime": (to - STEP) * 1000 } });
    let reply = rpc.control.post(&Target::Http(INFO.to_string()), &body.to_string(), Duration::from_secs(30)).await.map_err(|e| e.next(upstream_next(None, false)))?;
    if reply.status == 429 {
        return Err(Error::new(ErrorCode::RateLimited, "the price source rate-limited the request (HTTP 429)").next("retry later"));
    }
    let prices: Vec<(u64, String)> = candles(&reply.body)?.into_iter().filter(|(t, _)| (from..to).contains(t)).collect();
    if prices.is_empty() {
        return Err(Error::new(ErrorCode::Usage, format!("the price source has no {feed} prices between {from} and {to}: it keeps about the latest 5,000 minutes"))
            .next(format!("chainsim time {name} --replay {feed} --file <csv> plays any range from your own prices")));
    }
    play(adapter, name, &feed, &account, &prices, &format!("{SOURCE}, {coin}"), SOURCE).await
}

/// Replays the rows of a price file: any feed, any times.
pub async fn replay_file(adapter: &SolanaAdapter, name: &str, feed: &str, csv: &str) -> Result<Value> {
    let (feed, account) = price::resolve(feed)?;
    let prices = parse_csv(csv)?;
    play(adapter, name, &feed, &account, &prices, FILE_SOURCE, FILE_SOURCE).await
}

async fn play(adapter: &SolanaAdapter, name: &str, feed: &str, account: &str, prices: &[(u64, String)], source: &str, listed: &str) -> Result<Value> {
    let (handle, profile) = adapter.open(name).await?;
    let rpc = adapter.rpc(&handle, &profile);
    let mut data = price::read(&rpc, account).await?;
    let old = price::parse(&data).expect("read checked it");
    // Every price is checked before the fork changes.
    let mut raws = Vec::with_capacity(prices.len());
    for (_, close) in prices {
        raws.push(i64::try_from(to_raw(close, old.exponent)?).map_err(|_| Error::new(ErrorCode::Usage, format!("the price {close} is too large")))?);
    }
    let (from, to) = (prices[0].0, prices[prices.len() - 1].0);

    let (_, started) = clock(&rpc).await?;
    let mut applied = Vec::with_capacity(prices.len());
    let mut slot = 0;
    let mut failed = None;
    for ((time, _), raw) in prices.iter().zip(raws) {
        let step = async {
            if *time > from {
                rpc.call("surfnet_timeTravel", json!([{ "absoluteTimestamp": (started + time - from) * 1000 }])).await?;
            }
            let (at_slot, now) = clock(&rpc).await?;
            price::write(&mut data, raw, old.conf, now as i64, at_slot);
            let read = price::store(&rpc, account, &data).await?;
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
    adapter.record(name, json!({ "kind": "replay", "feed": feed, "account": account, "from": from, "to": to, "steps": applied.len(), "of": prices.len(), "source": listed })).await?;
    if let Some(e) = failed {
        return Err(Error::new(e.code, format!("the replay stopped after {} of {} steps: {}", applied.len(), prices.len(), e.message)).next(format!("chainsim status {name} shows the fork's state")));
    }
    let (_, ended) = clock(&rpc).await?;
    Ok(json!({
        "kind": "replay", "feed": feed, "account": account, "source": source,
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
    fn candles_give_the_close_oldest_first() {
        let body = r#"[{"t":1794403380000,"T":1794403439999,"s":"SOL","i":"1m","o":"84.363","c":"84.451","h":"84.577","l":"84.363","v":"4939.83","n":343},{"t":1794403320000,"T":1794403379999,"s":"SOL","i":"1m","o":"84.319","c":"84.31","h":"84.353","l":"84.293","v":"461.52","n":51}]"#;
        assert_eq!(candles(body).unwrap(), [(1794403320, "84.31".to_string()), (1794403380, "84.451".to_string())]);
        assert_eq!(to_raw(&candles(body).unwrap()[0].1, -8).unwrap(), 8_431_000_000);
        assert_eq!(candles("[]").unwrap(), []);
        for bad in ["{\"error\":\"x\"}", "<html>", "[[1,2]]", "[{\"t\":1,\"c\":84.3}]"] {
            assert_eq!(candles(bad).unwrap_err().code, ErrorCode::UpstreamUnavailable, "{bad}");
        }
    }

    #[test]
    fn only_perps_the_source_lists_replay_without_a_file() {
        assert_eq!((coin("SOL/USD"), coin("BTC/USD"), coin("ETH/USD")), (Some("SOL"), Some("BTC"), Some("ETH")));
        assert_eq!(coin("USDC/USD"), None);
        assert_eq!(coin("7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE"), None);
    }

    #[test]
    fn a_price_file_is_rows_of_time_and_price() {
        let rows = parse_csv("time,price\n1794404160,84.04\n\n1794404220, 84\n1794404400,83.9\n").unwrap();
        assert_eq!(rows, [(1794404160, "84.04".to_string()), (1794404220, "84".to_string()), (1794404400, "83.9".to_string())]);
        assert_eq!(parse_csv("1,1.5").unwrap(), [(1, "1.5".to_string())]);
        for bad in ["", "time,price\n", "1794404160;84.04", "1794404160,abc", "1794404160,", "10,1\n10,2", "10,1\n9,2", "10,1\nlater,2", "10,-1"] {
            assert_eq!(parse_csv(bad).unwrap_err().code, ErrorCode::Usage, "{bad:?}");
        }
        let long: String = (0..=MAX_ROWS).map(|i| format!("{i},1\n")).collect();
        assert_eq!(parse_csv(&long).unwrap_err().code, ErrorCode::Usage);
    }
}
