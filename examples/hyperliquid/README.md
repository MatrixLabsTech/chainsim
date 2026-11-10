# HyperEVM example: a stop-loss vault tested on a mainnet fork

A small HyperEVM project that shows how chainsim fits into the unit and end-to-end tests a team
already has. `StopLossVault` holds a perp long on HyperCore. It reads the mark price and its own
position through the read precompiles and closes the long through CoreWriter when the mark falls
to its stop.

- `src/HyperCore.sol`: precompile readers (`0x800`, `0x806`, `0x807`) and CoreWriter action
  encoders (limit order, USD class transfer).
- `src/StopLossVault.sol`: the vault.
- `test/StopLossVault.t.sol`: Foundry unit tests, run with `forge test --fork-url` against the
  fork's RPC. They read HyperCore through the precompiles and check the exact CoreWriter action
  each call emits. A fuzz test runs 1,000 mark prices, stops and position sizes through the
  keeper call: nothing is sent above the stop, and at or under it the whole long closes with a
  limit under the stop.
- `e2e/stop-loss.e2e.mjs`: a viem script that sends real transactions through the fork's RPC
  proxy. It deploys the vault, moves USD to perp, opens a 0.01 BTC long that fills against the
  mainnet order book, moves BTC down on the fork and checks that the keeper call closes the long
  under the stop, at the price the book moved to.
- `e2e/replay-day.e2e.mjs`: a real day against the vault. The fork replays BTC's hourly mainnet
  candles of 2026-11-05, when BTC fell 6% within the day. The vault opens a long at the day's
  first prices with a stop 3% under its entry, a keeper calls it after every two candles, and
  the script checks the hour of the close, its price and the loss.
- `scripts/test.sh`: the whole run: `chainsim up`, `selftest`, the unit and fuzz tests, the e2e
  scripts and `chainsim down`.
- `scripts/docker.sh`: runs any of these scripts in one container that has chainsim built from
  this repository, Foundry and Node.js, so the host needs only Docker.
- `.github/workflows/chainsim.yml`: a GitHub Actions job that runs the same script. Copy it into
  your own repository's `.github/workflows/`.

## Run it

With Docker alone, from a clone of the chainsim repository:

```sh
examples/hyperliquid/scripts/docker.sh scripts/test.sh
```

Or with chainsim, Docker, Foundry and Node.js 22 with yarn on the host:

```sh
CHAINSIM_PUBLISH=1 scripts/test.sh
```

`CHAINSIM_PUBLISH=1` publishes the fork's RPC on `127.0.0.1` for tools on the host. Inside a
container that has the Docker socket, leave it unset and the script joins the fork's network.

Unit tests read HyperCore from the fork, so `forge test` runs locally and its CoreWriter writes
stay in Foundry. The e2e script sends real transactions, and the proxy applies their CoreWriter
actions to the fork's HyperCore after each block.

## Output

From a run on 2026-11-10:

```text
{"fork":"hyperevm-mainnet","block":50898839,"rpc":"http://hyperevm-mainnet:8545"}
{"selftest":true,"parts":9}
{"seeded":"position","user":"0x0000000000000000000000000000000000005a1E"}
Ran 6 tests for test/StopLossVault.t.sol:StopLossVaultTest
[PASS] testFuzz_ClosesTheWholeLongOnlyAtOrUnderTheStop(uint64,uint64,int64) (runs: 1000, μ: 110828, ~: 77454)
[PASS] test_ClosesTheWholeLongAtTheStop() (gas: 163371)
[PASS] test_HoldsWhileTheMarkIsAboveTheStop() (gas: 118691)
[PASS] test_OnlyTheOwnerOpens() (gas: 31710)
[PASS] test_OpenLongSendsAnIocBuyThroughCoreWriter() (gas: 70514)
[PASS] test_ReadsMainnetPricesThroughThePrecompiles() (gas: 19550)
Suite result: ok. 6 passed; 0 failed; 0 skipped; finished in 1.48s (2.67s CPU time)
Ran 1 test suite in 1.49s (1.48s CPU time): 6 tests passed, 0 failed, 0 skipped (6 total tests)
PASS  deployed StopLossVault at 0x6971a599325f73ab18663786e15e1de72c962adc
PASS  perp balance 500 USD (/info clearinghouseState withdrawable 500.0)
PASS  position 0.01 BTC read back through the 0x800 precompile (best ask 74056)
PASS  /info userFills: Open Long 0.01 at 74056.0
PASS  keeper holds at mark 739790, stop 724994
PASS  keeper closed at the stop (tx 0xba600007)
PASS  position reads 0 through the 0x800 precompile
PASS  /info userFills: Close Long 0.01 BTC in 1 fill, first at 71831.0, under the stop 72499.4
PASS  corewriter_actions: moved 500.0 USD to perp, filled 0.01 at 74056.0, filled 0.01 at 71831.0
all passed
PASS  replayed 2026-11-05 00:00 and 01:00, BTC at 80647.0
PASS  opened 0.01 BTC long, best ask 80731
PASS  entry 80731 within 0.5% of the 01:00 close, stop 78309.1
PASS  keeper closed after the 2026-11-05T15:00Z candle, BTC at 76044
PASS  position reads 0 through the 0x800 precompile at the end of the day
PASS  close filled at 76120, under the stop and within 0.5% of the candle's close
PASS  closed PnL -46.11 USD on 0.01 BTC, day low 75703
all passed
{"down":true}
```
