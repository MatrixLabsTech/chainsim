# Benches

This run: 2026-11-10, chainsim 0.1.11, proxy 0.1.11-839de116a72f, HyperEVM mainnet through the public RPC, Apple M5 Max, Docker with 18 CPUs and 7.7 GiB, arm64.

Reproduce it from a clone of the repository, with Docker as the only requirement:

```sh
examples/hyperliquid/scripts/docker.sh bench/run.sh
```

Every number here comes from [bench.json](bench.json), which the run writes.

## Start

- `up`, a cold fork with HyperCore, over 5 runs: 17.5 s at the median, 17.8 s at the slowest (17.7, 17.8, 16.9, 17.0, 17.5).
- First transaction after `up`: 153 ms at the median.
- Selftest: 9 of 9 checks.

## Orders

200 CoreWriter IOC buys of 0.001 BTC, each a signed transaction sent through the proxy from one viem client. 2,200 of 2,200 orders in this run filled.

| Milliseconds | p50 | p95 | p99 | max |
| --- | --- | --- | --- | --- |
| Send to receipt | 3.05 | 4.31 | 6.91 | 13.03 |
| Send to the fill read back through the `0x800` precompile | 3.32 | 4.65 | 7.35 | 13.73 |

Sustained: 2,000 orders of 0.0002 BTC sent back to back were all filled in 4.87 s, 410 orders a second.

## RPC proxy

2,000 `eth_chainId` requests each way. The overhead is the difference of the medians.

| Milliseconds, p50 / p95 / p99 | Through the proxy | Straight to the engine | Overhead at p50 |
| --- | --- | --- | --- |
| HTTP | 0.26 / 0.34 / 0.66 | 0.11 / 0.15 / 0.22 | 0.15 |
| WebSocket | 0.16 / 0.18 / 0.23 | 0.05 / 0.07 / 0.1 | 0.11 |

Requests a second through the proxy, `eth_call markPx (0x806)` for 5 s:

| Clients | Requests a second | Latency ms, p50 / p95 / p99 |
| --- | --- | --- |
| 1 | 3,749 | 0.25 / 0.31 / 0.52 |
| 4 | 5,931 | 0.63 / 0.84 / 1.65 |
| 16 | 5,726 | 2.68 / 3.62 / 4.47 |

## Mirror and replay

Each run starts from the same snapshot. The last column is the longest another client waited for `eth_blockNumber` while the command ran, polling every 50 ms.

| Mirror of a mainnet account | Seconds | Info API calls | Upstream calls | Longest client wait, ms |
| --- | --- | --- | --- | --- |
| 5 positions | 6.0 | 16 | 6 | 164 |
| 23 positions | 6.4 | 18 | 6 | 261 |
| 177 positions | 6.6 | 16 | 6 | 327 |

| Replay of hourly BTC candles | Seconds | Milliseconds per candle | Longest client wait, ms |
| --- | --- | --- | --- |
| 25 candles | 4.7 | 189.1 | 161 |
| 73 candles | 13.0 | 177.9 | 227 |
| 169 candles | 30.3 | 179.2 | 250 |

## Footprint

- Peak memory during the run: proxy 5.2 MiB, engine 647.6 MiB, control 4.4 MiB.
- Images: proxy 29.7 MB, engine 246.2 MB, control 13.7 MB.
