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
  each call emits.
- `e2e/stop-loss.e2e.mjs`: a viem script that sends real transactions through the fork's RPC
  proxy. It deploys the vault, moves USD to perp, opens a 0.01 BTC long that fills against the
  mainnet order book, moves BTC down on the fork and checks that the keeper call closes the long
  under the stop, at the price the book moved to.
- `scripts/test.sh`: the whole run: `chainsim up`, `selftest`, the unit tests, the e2e script and
  `chainsim down`.
- `.github/workflows/chainsim.yml`: a GitHub Actions job that runs the same script. Copy it into
  your own repository's `.github/workflows/`.

## Run it

You need chainsim, Docker, Foundry and Node.js 22 with yarn.

```sh
forge install foundry-rs/forge-std --no-git
(cd e2e && yarn install)
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
{"fork":"hyperevm-mainnet","block":50895082,"rpc":"http://hyperevm-mainnet:8545"}
{"selftest":true,"parts":9}
{"seeded":"position","user":"0x0000000000000000000000000000000000005a1E"}
Ran 5 tests for test/StopLossVault.t.sol:StopLossVaultTest
[PASS] test_ClosesTheWholeLongAtTheStop() (gas: 163371)
[PASS] test_HoldsWhileTheMarkIsAboveTheStop() (gas: 118691)
[PASS] test_OnlyTheOwnerOpens() (gas: 31710)
[PASS] test_OpenLongSendsAnIocBuyThroughCoreWriter() (gas: 70514)
[PASS] test_ReadsMainnetPricesThroughThePrecompiles() (gas: 19550)
Suite result: ok. 5 passed; 0 failed; 0 skipped; finished in 1.57s (2.40s CPU time)
Ran 1 test suite in 1.58s (1.57s CPU time): 5 tests passed, 0 failed, 0 skipped (5 total tests)
PASS  deployed StopLossVault at 0x6971a599325f73ab18663786e15e1de72c962adc
PASS  perp balance 500 USD (/info clearinghouseState withdrawable 500.0)
PASS  position 0.01 BTC read back through the 0x800 precompile (best ask 73791)
PASS  /info userFills: Open Long 0.01 at 73791.0
PASS  keeper holds at mark 737250, stop 722505
PASS  keeper closed at the stop (tx 0x58b8202e)
PASS  position reads 0 through the 0x800 precompile
PASS  /info userFills: Close Long 0.01 BTC in 1 fill, first at 71574.0, under the stop 72250.5
PASS  corewriter_actions: moved 500.0 USD to perp, filled 0.01 at 73791.0, filled 0.01 at 71574.0
all passed
{"down":true}
```
