<p align="center"><img src="../../assets/chains/hyperliquid.svg" height="96" alt="Hyperliquid"></p>

# HyperEVM mainnet and testnet

chainsim runs a private copy of HyperEVM Mainnet, the EVM side of Hyperliquid, in Docker as the
chain `hyperevm-mainnet`, and of HyperEVM Testnet as `hyperevm-testnet` (see
[HyperEVM testnet](#hyperevm-testnet)). The commands and their JSON output are the same as on every chain, and
the [README](../../README.md) covers the shared workflow. This page covers what is specific to
HyperEVM. Every output below comes from a live run against HyperEVM Mainnet, on 2026-11-01 for
the precompile answers and on 2026-11-02 for the local HyperCore, market scripting, the proxy,
builder-deployed perps, borrow and lend, and testnet.

## What chainsim gives on HyperEVM

- A HyperEVM fork where contracts that read HyperCore run. HyperCore's prices, balances and
  positions reach HyperEVM contracts through read precompiles at `0x800` to `0x813`. A plain
  anvil fork leaves those addresses empty, so every contract that decodes their answer reverts.
  chainsim puts a mock at each of them.
- A local HyperCore beside the fork. The precompiles answer from the copy's own Core state, for
  any account, and the actions contracts send through CoreWriter change it.
- Limit orders that fill against the mainnet order book, with its depth: full and partial fills,
  resting orders, cancels, and IOC and ALO rules.
- Bridging both ways: HYPE and linked tokens sent to their system addresses credit Core spot,
  and a spot send back credits the EVM balance.
- `set hypercore mirror`, which copies a mainnet account's positions, margin and balances onto the
  copy, and `call hypercore_state`, which reads an account's Core state in the info API's shapes.
- Market scripting: `set hypercore price` reprices positions, fills the resting orders a move
  crosses and liquidates accounts below maintenance margin, `time --replay` plays a past stretch
  of mainnet candles as the price path, funding accrues every hour of the fork's clock, and every
  fill pays maker or taker fees.
- A replay steps one candle at a time, so other clients are served while it runs, and a
  transaction one of them sends between two candles is applied before the next candle.
- Builder-deployed (HIP-3) perps, such as xyz:XYZ100, with their own dex balance and margin,
  isolated margin where the perp is isolated only, and their mainnet books.
- Borrow and lend: supply and withdraw through CoreWriter, implicit borrowing for portfolio margin
  and unified accounts, hourly interest, health factor and liquidation, read through `0x811` and
  `0x812`.
- An RPC proxy in front of the fork, so any client, such as forge, cast, a frontend or an SDK,
  gets HyperCore applied on every block with no chainsim command in between.
- Small and big blocks as on mainnet, per sender, with `eth_usingBigBlocks` and
  `eth_bigBlockGasPrice`.
- The Hyperliquid info API for the copy at `/info`, so a frontend built on the Hyperliquid SDK can
  point at it.
- Every action a contract sends through CoreWriter, decoded with what it did by `call
  corewriter_actions`.
- `fund` for HYPE and five ERC-20 tokens, and `send --as` for any account.
- `snapshot` and `revert`, which restore Core state with the rest of the fork.
- `selftest`, which checks the chain id, the mock and CoreWriter before a test relies on them.
- The same JSON contract and exit codes as every other chain, so a script or an agent drives
  every chain the same way.

## Engine

- The engine is Foundry anvil v1.8.5, the same image as Ethereum mainnet, run with chain id 999,
  the Cancun hardfork and a 30,000,000 gas limit, so large deployments fit.
- The fork has no dev accounts. `fund` gives a test wallet its balances.
- Each fork is an engine container, a control sidecar and chainsim's HyperEVM proxy on the
  fork's network. The proxy takes the fork's alias and port, and anvil sits behind it at
  `<alias>-engine`. All three run as non-root users with a read-only root filesystem, every
  Linux capability dropped and `no-new-privileges`, and publish no port unless `up --publish`
  asks for one, which publishes the proxy's.
- The proxy is a small Rust binary chainsim builds in its release workflow and pins in its engine
  lock by version and sha256, like the other engines. `up` downloads it once and checks it.

## Upstream

- The fork reads HyperEVM Mainnet from Hyperliquid's public RPC, `https://rpc.hyperliquid.xyz/evm`.
- `CHAINSIM_HYPEREVM_MAINNET_RPC` replaces it with a provider URL. chainsim keeps the URL in the
  environment and removes it from every message.
- The public RPC answers every state read at its latest block, whatever block the read names. So
  `up --block` with the default upstream exits 7 and creates nothing:

```json
{
  "ok": false,
  "error": {
    "code": "latest_state_only",
    "message": "--block is refused on hyperevm-mainnet: the default upstream answers state reads at the latest block whatever block they name, so an older fork point would pair an old header with current state",
    "next": "export CHAINSIM_HYPEREVM_MAINNET_RPC=<archive provider URL> to fork at an older block, or drop --block to fork at the head"
  }
}
```

With a provider URL set, `--block` works as on the other EVM chains.

## Start a fork

```sh
chainsim up hyperevm-mainnet
chainsim selftest hyperevm-mainnet
```

`up` forks at the upstream head, puts the HyperCore mock at `0x800` to `0x813` and seeds it. The
fork object, with its `limits` and `devAccounts` left out here:

```json
{
  "name": "hyperevm-mainnet",
  "chain": "hyperevm-mainnet",
  "chainId": "eip155:999",
  "maturity": "experimental",
  "forkPoint": { "kind": "block", "number": 50151644 },
  "network": "chainsim-hyperevm-mainnet",
  "alias": "hyperevm-mainnet",
  "rpcUrl": "http://hyperevm-mainnet:8545",
  "wsUrl": "ws://hyperevm-mainnet-engine:8545",
  "hostRpcUrl": null,
  "upstream": { "default": "https://rpc.hyperliquid.xyz/evm", "env": "CHAINSIM_HYPEREVM_MAINNET_RPC", "set": false },
  "hypercore": {
    "precompiles": "0x0000000000000000000000000000000000000800 to 0x0000000000000000000000000000000000000813",
    "seeded": 17,
    "l1BlockNumber": 1205881826,
    "perps": [
      { "index": 0, "coin": "BTC", "markPx": "784570", "oraclePx": "784889" },
      { "index": 1, "coin": "ETH", "markPx": "229380", "oraclePx": "229476" },
      { "index": 5, "coin": "SOL", "markPx": "950890", "oraclePx": "951100" },
      { "index": 159, "coin": "HYPE", "markPx": "877320", "oraclePx": "876635" }
    ],
    "tokens": [
      { "index": 0, "name": "USDC" },
      { "index": 150, "name": "HYPE" }
    ],
    "spots": [
      { "index": 107, "name": "@107", "spotPx": "87641000" }
    ]
  }
}
```

The `hypercore` field appears only on HyperEVM forks. Prices are in the precompiles' raw units.
`up` also takes one order book per seeded perp from the Hyperliquid info API and lists them under
`hypercore.core`. From the 2026-11-02 run, shortened to BTC:

```json
"core": {
  "books": [
    { "coin": "BTC", "perp": 0, "bestBid": "78257.0", "bestAsk": "78258.0", "bidLevels": 20, "askLevels": 20, "time": 1793600049698 }
  ],
  "infoCalls": 6,
  "state": "0x9999999999999999999999999999999999999999"
}
```

The copy keeps its Core state at `0x9999…9999`, so a snapshot holds it with everything else.

`selftest` checks nine parts, the six every EVM chain has and three for HyperEVM:

- `chain-id`: `eth_chainId` returns 999.
- `hypercore`: the L1 block number precompile at `0x809` and a seeded oracle price at `0x807`
  answer non-zero values.
- `corewriter`: a limit order sent through CoreWriter emits `RawAction`, and it decodes.

```json
{
  "ok": true,
  "parts": [
    { "part": "block", "ok": true, "ms": 40 },
    { "part": "block-time", "ok": true, "ms": 0 },
    { "part": "native-transfer", "ok": true, "ms": 389 },
    { "part": "token-transfer", "ok": true, "ms": 1897 },
    { "part": "snapshot-revert", "ok": true, "ms": 236 },
    { "part": "upstream-read", "ok": true, "ms": 142 },
    { "part": "chain-id", "ok": true, "ms": 35 },
    { "part": "hypercore", "ok": true, "ms": 38 },
    { "part": "corewriter", "ok": true, "ms": 216 }
  ]
}
```

Each part also carries a `reason` sentence.

## HyperCore answers

A contract reads HyperCore with a `staticcall` to a precompile, passing the ABI-encoded
arguments with no selector. On the fork, the BTC oracle price (perp 0) reads back as seeded,
784889:

```sh
chainsim call hyperevm-mainnet eth_call '[{"to": "0x0000000000000000000000000000000000000807", "data": "0x0000000000000000000000000000000000000000000000000000000000000000"}, "latest"]'
```

```json
"0x00000000000000000000000000000000000000000000000000000000000bf9f9"
```

`set hypercore` writes the common answers without hand-encoding them. `price` sets a perp's mark
price at `0x806` and oracle price at `0x807`:

```sh
chainsim set hyperevm-mainnet hypercore price 0 650000
```

```json
{
  "kind": "hypercore",
  "helper": "price",
  "perp": 0,
  "px": 650000,
  "sets": [
    { "kind": "precompile", "address": "0x0000000000000000000000000000000000000806", "calldata": "0x0000000000000000000000000000000000000000000000000000000000000000", "result": "0x000000000000000000000000000000000000000000000000000000000009eb10", "source": "given" },
    { "kind": "precompile", "address": "0x0000000000000000000000000000000000000807", "calldata": "0x0000000000000000000000000000000000000000000000000000000000000000", "result": "0x000000000000000000000000000000000000000000000000000000000009eb10", "source": "given" }
  ],
  "note": "This result depends on a chainsim override that does not exist on mainnet (impersonation or a state change). chainsim status <name> lists every modification until down."
}
```

The same `eth_call` then returns `0x…09eb10`, which is 650000. The new mark also reprices every
position on the copy, so margin and withdrawable answers follow it.

- `set <name> hypercore spot-balance <user> <token> <total> [--hold <n>] [--entry-ntl <n>]`
  sets a user's spot balance in the copy's Core state, in the token's raw units on HyperCore.
  USDC is token 0, with 8 decimals there, so this gives `0x0000…c0ffee` 5 USDC:

  ```sh
  chainsim set hyperevm-mainnet hypercore spot-balance 0x0000000000000000000000000000000000c0ffee 0 500000000
  ```

- `set <name> hypercore position <user> <perp> <szi> <entryNtl> [--leverage <n>]
  [--isolated-raw-usd <n>] [--isolated]` sets a user's perp position in the copy's Core state.
  A negative `szi` is a short. Give the account perp USD first, with `spot-balance` and a USD
  class transfer, or the next price move or funding payment liquidates it.
- `set <name> hypercore price <spot pair> <px> --spot` sets a spot pair's price (`0x808`), in its
  raw units (8 minus the base token's szDecimals decimals). Borrow and lend collateral priced by
  that pair follows it.
- `set <name> hypercore isolated-margin <user> <perp> <+usd|-usd>` adds or removes USD margin on an
  isolated position (see [HIP-3](#builder-deployed-perps-hip-3)).
- `set <name> precompile <address> <calldata> --result <hex>` stores any other answer, for any
  precompile from `0x800` to `0x813`.

Spot balance, position, withdrawable and margin summary reads answer for any user, with zeros
for an account the copy has no Core state for. A price or info query nobody seeded reverts with a
reason that names the fix. The oracle price of perp 2:

```json
{
  "ok": false,
  "error": {
    "code": "rpc_error",
    "message": "eth_call: execution reverted: chainsim: unseeded HyperCore query, run chainsim set <name> precompile <address> <calldata> --live (3)",
    "next": null
  }
}
```

`set precompile --live` copies the upstream's answer for that query:

```sh
chainsim set hyperevm-mainnet precompile 0x807 0x0000000000000000000000000000000000000000000000000000000000000002 --live
```

```json
{
  "kind": "precompile",
  "address": "0x0000000000000000000000000000000000000807",
  "calldata": "0x0000000000000000000000000000000000000000000000000000000000000002",
  "result": "0x000000000000000000000000000000000000000000000000000000000000395d",
  "source": "live",
  "note": "This result depends on a chainsim override that does not exist on mainnet (impersonation or a state change). chainsim status <name> lists every modification until down."
}
```

Every `set precompile` and `set hypercore` is listed under `status` modifications.

## Local HyperCore

chainsim keeps HyperCore state for the copy: spot balances, perp positions with cross margin, and
open orders, per account. The proxy applies each CoreWriter action and bridge transfer in block
order after every block, whoever sent it, and an action takes effect from the block after its
own, as on mainnet.

### Mirror a mainnet account

`set hypercore mirror <user>` copies an account's perp positions, margin and spot balances from
the info API, and its HYPE and token balances on the EVM side. This mirrors one of HLP's strategy
vaults:

```sh
chainsim set hyperevm-mainnet hypercore mirror 0x010461c14e146ac35fe42271bdc1134ee31c703a
```

```json
{
  "kind": "hypercore",
  "helper": "mirror",
  "user": "0x010461C14e146ac35Fe42271BDC1134EE31C703a",
  "positions": 177,
  "spotBalances": 0,
  "accountValue": "3043939.222041",
  "evm": [
    { "asset": "HYPE", "amount": "0.00185249" },
    { "asset": "USDC", "amount": "0" }
  ],
  "skipped": [],
  "infoCalls": 3,
  "upstreamCalls": 6,
  "note": "This result depends on a chainsim override that does not exist on mainnet (impersonation or a state change). chainsim status <name> lists every modification until down."
}
```

The `evm` list is shortened here. The vault's BTC position then reads back through `0x800` from
a contract, and `send --as` the vault address acts as it.

### Orders

A limit order sent through CoreWriter matches against the order book snapshot, best price first,
and against orders resting on the copy. Each fill takes depth off the book, so a second order
fills deeper. A GTC remainder rests, an IOC remainder is dropped, and an ALO order that would
cross is rejected. A cancel by order id or client order id removes a resting order. Orders check
the account's withdrawable margin at the order's leverage, 20 or the perp's maximum if lower.

This run gave `0x0000…c0ffee` 1,000 USDC on spot, moved 500 USD to perp with a USD class
transfer, bought 0.001 BTC with an IOC order above the ask, and left a GTC bid at 70,000.
`call corewriter_actions` shows each action with its `result`:

```json
[
  { "block": 50165643, "action": "usdClassTransfer", "fields": { "ntl": "500000000", "toPerp": true }, "result": "moved 500.0 USD to perp" },
  { "block": 50165644, "action": "limitOrder", "fields": { "asset": "0", "isBuy": true, "limitPx": "7904058000000", "sz": "100000", "reduceOnly": false, "encodedTif": "3", "cloid": "0" }, "result": "filled 0.001 at 78258.0" },
  { "block": 50165645, "action": "limitOrder", "fields": { "asset": "0", "isBuy": true, "limitPx": "7000000000000", "sz": "100000", "reduceOnly": false, "encodedTif": "2", "cloid": "77" }, "result": "0.001 resting as oid 1" }
]
```

Each entry also carries `txHash`, `logIndex`, `user`, `version` and `actionId`. The IOC filled at
78,258, the best ask in the book `up` took.

### Core state

`call <name> hypercore_state <user>` returns an account's Core state in the info API's own
shapes, `clearinghouseState` and `spotClearinghouseState`, plus `openOrders`:

```sh
chainsim call hyperevm-mainnet hypercore_state 0x0000000000000000000000000000000000c0ffee
```

```json
{
  "user": "0x0000000000000000000000000000000000C0FFEE",
  "clearinghouseState": {
    "marginSummary": { "accountValue": "486.742", "totalNtlPos": "65.0", "totalRawUsd": "421.742", "totalMarginUsed": "3.25" },
    "withdrawable": "479.992",
    "assetPositions": [
      {
        "type": "oneWay",
        "position": {
          "coin": "BTC", "szi": "0.001", "leverage": { "type": "cross", "value": 20 }, "entryPx": "78258.0",
          "positionValue": "65.0", "unrealizedPnl": "-13.258", "returnOnEquity": "-3.3882797924", "liquidationPx": null,
          "marginUsed": "3.25", "maxLeverage": 40, "cumFunding": { "allTime": "0.0", "sinceOpen": "0.0", "sinceChange": "0.0" }
        }
      }
    ],
    "time": 1793600069000
  },
  "spotClearinghouseState": {
    "balances": [
      { "coin": "USDC", "token": 0, "total": "500.0", "hold": "0.0", "entryNtl": "0.0" },
      { "coin": "HYPE", "token": 150, "total": "2.0", "hold": "0.0", "entryNtl": "0.0" }
    ]
  },
  "openOrders": [
    { "coin": "BTC", "asset": 0, "side": "B", "limitPx": "70000.0", "sz": "0.001", "oid": 1, "cloid": "0x0000000000000000000000000000004d", "tif": "Gtc" }
  ]
}
```

`crossMarginSummary` and `crossMaintenanceMarginUsed` are left out here. The BTC mark had been
set to 65,000 earlier in the run with `set hypercore price`, so the position shows a loss against
its 78,258 entry. The same figures answer the `0x800`, `0x801`, `0x803` and `0x80f` precompiles.

`set hypercore book <coin> --live` takes a fresh book for one perp from the info API, by name or
index, and replaces the one fills have consumed.

### Bridging

- HYPE sent to `0x2222222222222222222222222222222222222222` credits the sender's Core spot HYPE.
  In the run above, `send --as 0x0000…c0ffee --to 0x2222…2222 --value 2` gave the 2.0 HYPE in
  the balances.
- An ERC-20 linked to a spot token, sent to that token's system address (`0x20` followed by
  zeros and the token index), credits the sender's Core spot balance of it.
- A spot send to a system address debits Core and credits the sender's EVM balance. A spot send
  of 1 HYPE to `0x2222…2222` left 1.0 HYPE on Core and raised the EVM balance by 1 HYPE, less the
  gas of the transaction that sent the action.

## Market scripting

A test moves the market on the copy and watches what follows. Each move runs the same steps:

1. The perp's mark and oracle prices take the new value, and every position reprices: unrealized
   PnL, margin used, withdrawable and the margin summary follow it.
2. Resting orders the move crosses fill at their own limit price, as makers.
3. Borrow and lend collateral that follows the perp takes its new price.
4. Accounts whose account value falls below their maintenance margin are liquidated, and borrow
   and lend accounts past the portfolio margin ratio are liquidated in steps. The same check runs
   after each funding payment and on every replayed candle.

The rules follow Hyperliquid's documentation on
[margining](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/margining),
[liquidations](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/liquidations),
[funding](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding) and
[fees](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees), simplified where the
profile limits say:

- Margin follows the perp's margin table from the info API meta. A position's tier is the one
  whose lower bound is the highest at or below its notional. Maintenance margin is the notional
  over twice the tier's max leverage, less the tier's maintenance deduction, so 1/80 of the
  position's value for BTC at 40x up to 150,000,000 USD and 1/40 less 1,875,000 USD above it.
  A table id below 50 is one tier at that leverage, as the info API lists only the others.
- A liquidation closes every position of the account at the mark price and cancels its open
  orders. Collateral that remains stays with the account. A balance below zero is reset to zero,
  the loss the backstop liquidator takes on mainnet.
- Funding is paid every hour of the fork's clock: size times oracle price times the perp's hourly
  rate. Longs pay shorts when the rate is positive, and shorts pay longs when it is negative.
  `up` takes each seeded perp's last settled rate from the info API's `fundingHistory`.
- Fills pay the base-tier fees: 0.045% of the notional for the taker and 0.015% for the maker.

### Move the price

This run opened a 0.1 BTC long for one wallet with 500 USD of margin, and a resting bid 8% under
the book for another. The IOC paid the taker fee:

```json
{ "block": 50169604, "action": "limitOrder", "result": "filled 0.1 at 78090.0, fee 3.51405 USD" }
```

Then `set hypercore price` dropped BTC 10%, to 70,281:

```sh
chainsim set hyperevm-mainnet hypercore price 0 702810
```

```json
{
  "kind": "hypercore",
  "helper": "price",
  "perp": 0,
  "px": 702810,
  "filled": 1,
  "liquidated": ["0x00000000000000000000000000000000000A11cE"]
}
```

The `sets` list, the two price answers written, is left out here.

The bid filled on the way down and the long was liquidated, so its `0x800` position reads zero.
`hypercore_state` lists what happened under `history`, newest last. For the long:

```json
"history": [
  { "time": 1793603950000, "event": "liquidated: account value -284.41405 USD below maintenance margin 87.85125 USD, closed 0.1 BTC at 70281.0, shortfall 284.41405 USD absorbed" }
]
```

### Funding

`set hypercore funding <perp> <rate>` sets a perp's hourly rate. A negative rate makes shorts
pay. After a rate of 0.0001 and `time --advance 3600`, the bidder's history reads:

```json
"history": [
  { "time": 1793603950000, "event": "oid 1 bought 0.01 BTC at 71841.0 as the price moved to 70281.0, fee 0.107761 USD" },
  { "time": 1793606400000, "event": "funding on BTC at rate 0.0001: paid 0.070281 USD" }
]
```

The position's `cumFunding` carries the total paid since it opened, `"0.070281"` here.

### Replay a past day

`time <name> --replay <perp> --from <time> --to <time>` takes the perp's hourly candles from the
info API's `candleSnapshot`, both ends included, and plays their closes as the price path. Each
candle mines one block an hour after the last, pays funding, then moves the price with the steps
above. The times take Unix seconds, a date, or a date and time such as `2026-10-31T12:00Z`.

```sh
chainsim time hyperevm-mainnet --replay BTC --from 2026-10-31 --to 2026-11-01
```

```json
{
  "coin": "BTC",
  "perp": 0,
  "interval": "1h",
  "candles": 25,
  "from": 1793404800,
  "to": 1793491200,
  "firstClose": "77536.0",
  "lastClose": "77638.0",
  "lowClose": "77120.0",
  "highClose": "77671.0",
  "markPx": 776380,
  "height": 50169631,
  "filled": 0,
  "liquidated": [],
  "infoCalls": 1
}
```

The replay took one info API request. After it, `0x806` reads 776380, the last close in the
precompile's units, and the bidder had paid 25 more hours of funding, 1.999287 USD in all.
`snapshot` and `revert` restore prices, positions, margin and funding with the rest of the fork.

## RPC proxy

The fork's `rpcUrl` is chainsim's HyperEVM proxy. It forwards every JSON-RPC request to anvil,
brings HyperCore up to date after each block, and answers what anvil cannot. The fork object
from the 2026-11-02 run:

```json
{ "rpcUrl": "http://hyperevm-mainnet:8545", "wsUrl": "ws://hyperevm-mainnet-engine:8545", "hostRpcUrl": null }
```

`wsUrl` is the proxy as well (`ws://hyperevm-mainnet:8545`). Over a WebSocket, every request takes
the same path as over HTTP, so a transaction sent there is mined into its own block and HyperCore
catches up after it. `eth_subscribe` and `eth_unsubscribe` go to a socket of the client's own on
anvil, so `newHeads`, `logs` and `newPendingTransactions` behave as on anvil. In the 2026-11-02 run,
a `newHeads` subscription saw three blocks mined over the same socket, a CoreWriter IOC sent with
`cast send --rpc-url ws://…` read back through `0x800` and `/info` at once, and `eth_chainId` took
292 µs through the proxy against 133 µs on anvil's own socket.

### Any client

This run gave `0x7099…79C8` 1,000 USDC with `set hypercore spot-balance`, then used plain `cast`
on the fork's network for a USD class transfer and an IOC buy of 0.001 BTC through CoreWriter:

```sh
cast send --rpc-url http://hyperevm-mainnet:8545 --private-key <test key> \
  0x3333333333333333333333333333333333333333 "sendRawAction(bytes)" <IOC action>
```

With no chainsim command after it, the next `eth_call` to the position precompile reads the
position, szi 100 (0.001 BTC in szDecimals 5):

```json
"0x0000000000000000000000000000000000000000000000000000000000000064000000000000000000000000000000000000000000000000000000000004a7b238…"
```

The proxy's overhead was about 0.1 to 0.2 ms per request in this run.

### Block types

HyperEVM has small blocks (about 1 s, 3,000,000 gas) and big blocks (about 1 min, 30,000,000
gas), and a user opts into big blocks with the `evmUserModify` action. On the copy the proxy
mines one block per transaction with the gas limit of its sender's block type, and
`set hypercore big-blocks <user> on|off` stands in for that action:

```sh
chainsim set hyperevm-mainnet hypercore big-blocks 0x70997970C51812dc3A010C7d01b50e0d17dc79C8 on
```

```json
{ "kind": "hypercore", "helper": "big-blocks", "user": "0x70997970C51812dc3A010C7d01b50e0d17dc79C8", "usingBigBlocks": true, "blockGasLimit": 30000000 }
```

After it, `eth_usingBigBlocks` answers `true` for that address, its next transaction lands in a
block with `gasLimit` `0x1c9c380` (30,000,000), and a small-block sender's lands in one with
`0x2dc6c0` (3,000,000). A big block comes at least 60 s after the last big block.
`eth_bigBlockGasPrice` answers `0x5f5e100`, as measured on mainnet. A transaction too large for
its sender's block type is refused:

```text
error code -32000: exceeds block gas limit: the transaction asks 5000000 gas and a small block holds 3000000
```

### Info API

`POST /info` on the proxy answers the Hyperliquid info API's request types `clearinghouseState`,
`spotClearinghouseState`, `openOrders`, `userFills`, `meta`, `metaAndAssetCtxs`, `l2Book` and
`allMids` from the copy, in the info API's own shapes, with CORS headers for a browser. After the
IOC and a GTC bid at 70,000:

```sh
curl -X POST -H 'content-type: application/json' \
  --data '{"type": "openOrders", "user": "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"}' \
  http://hyperevm-mainnet:8545/info
```

```json
[{ "coin": "BTC", "side": "B", "limitPx": "70000.0", "sz": "0.001", "oid": 2, "timestamp": 1793606127000, "origSz": "0.001", "cloid": "0x0000000000000000000000000000004d" }]
```

`userFills` for the same user:

```json
[{ "coin": "BTC", "px": "78099.0", "sz": "0.001", "side": "B", "time": 1793606127000, "startPosition": "0.0", "dir": "Open Long", "closedPnl": "0.0", "hash": "0x0000…0001", "oid": 1, "crossed": true, "fee": "0.035144", "tid": 1, "feeToken": "USDC" }]
```

`clearinghouseState` matches `call hypercore_state`, whose output also carries the user's fills
under `userFills`. `meta` and `metaAndAssetCtxs` cover the whole perp universe from the snapshot
`up` took, with the copy's own mark, oracle, funding and mid for the perps it tracks.

## Builder-deployed perps (HIP-3)

Builder-deployed dexes list their perps as `dex:coin`, such as `xyz:XYZ100`. Orders and CoreWriter
use asset `100000 + dex * 10000 + index`, and the read precompiles take the perp index
`dex * 10000 + index` (measured on the mainnet RPC: `0x806` with 10000 answers xyz:XYZ100). xyz is
dex 1, so xyz:XYZ100 is perp 10000 and asset 110000.

- `set hypercore book xyz:XYZ100 --live` adds the perp with its mainnet book, mark and oracle
  price, and the dex's meta for `/info`. `set hypercore price 10000 <px>` and `set hypercore
  mirror` cover HIP-3 perps the same way. None are seeded at `up`.
- Each dex has its own USD balance and cross margin. `sendAsset` (CoreWriter action 13) moves USDC
  between the main dex (0), a builder dex and spot (dex 4294967295). Only dexes with USDC
  collateral trade on the copy.
- Perps meta marks `onlyIsolated` hold isolated margin per position: the order's leverage sets
  the margin at open, liquidation checks the position against its own margin, and a loss past it
  stays off the dex balance.
- `set hypercore isolated-margin <user> <perp> <+usd|-usd>` adds or removes margin on an isolated
  position, as the exchange's `updateIsolatedMargin`. Adding takes the amount from the dex's
  withdrawable balance. Removing must leave at least the larger of the initial margin and 10% of
  the position's value.
- Funding uses the HIP-3 premium: the mid of the impact bid and ask on the copy's book over the
  oracle price, less 1. The hourly rate is the dex's funding multiplier times the premium plus
  `clamp(interest rate - premium, -0.0005, 0.0005)`, over 8, capped at 4%. `perpDexs` gives each
  perp's multiplier, interest rate (0.0001 unless set) and clamp at the time the perp is added.
  Measured on mainnet: xyz:TSLA with premium -0.0001887542 and multiplier 0.5 pays 0.00000625.
- Margin tiers come from the dex's own meta, as on the main dex.
- `0x800` (uint16 perp) and `0x813` (uint32 perp) answer positions, `0x80f` takes the dex, and
  `/info` takes `"dex": "xyz"` for meta, metaAndAssetCtxs, clearinghouseState, openOrders and
  allMids. `call hypercore_state` lists builder dexes under `dexes`.

In the 2026-11-02 run, a wallet sent 600 USD to xyz with sendAsset, bought 0.01 xyz:XYZ100 with an
IOC at 28,610.0 (the book's best ask), read szi 100 back through `0x800` and `0x813`, and opened
an isolated xyz:HOOD long and a cross BTC long. A 30% HOOD drop liquidated the HOOD position
alone. The loss past the position's own margin was absorbed, not taken from the xyz balance:

```text
liquidated: isolated xyz:HOOD equity -199.999776 USD below maintenance margin 34.99996 USD, closed 11.542 xyz:HOOD at 60.648, shortfall 199.999776 USD absorbed
```

XYZ100 on xyz and BTC on the main dex kept their positions.

## Borrow and lend

Borrow and lend follows Hyperliquid's
[portfolio margin](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/portfolio-margin) and
[manual borrows](https://hyperliquid.gitbook.io/hyperliquid-docs/support/faq/manual-borrows)
pages.

- `up` takes every reserve from the info API's `allBorrowLendReserveStates`: LTV (0.65 for HYPE,
  0.5 for UBTC, 0 for the stablecoins), oracle price, total supplied and borrowed.
- CoreWriter action 15 supplies (0) or withdraws (1) a token, `wei` 0 meaning all of it. Action
  16 sets the account abstraction: 2 unified account, 3 portfolio margin.
- Borrowing is implicit, as on mainnet: for a portfolio margin or unified account, a spot send,
  USD class transfer or sendAsset beyond the spot balance borrows the stablecoin it lacks against
  supplied collateral, while the health factor (LTV-weighted collateral over borrowed value)
  stays at or above 1. A later credit of that token repays first.
- Stablecoins accrue `0.05 + 4.75 * max(0, utilization - 0.8)` a year, indexed every hour of the
  fork's clock, and suppliers earn it times utilization, less 10%.
- A collateral token's price is the median of its spot price against USDC, the mark and the
  oracle price of the perp on the same asset (UBTC follows BTC). `set hypercore price 159 <px>`
  on the HYPE perp and `set hypercore price 107 <px> --spot` on HYPE/USDC both move HYPE
  collateral. Measured on mainnet: the UBTC reserve price followed the UBTC/USDC spot mark, the
  middle of the three.
- The portfolio margin ratio for a borrowed token is (20 USDC + the cross maintenance margin of
  every dex + the borrowed value) over (the token's balance, with the account value of the dexes
  it margins, + the supplied collateral times its price times `0.5 + 0.5 * LTV`). Past 0.95 the
  backstop at `0xbbbb…bbbb` takes 20% of each collateral at a time, at its price, against the
  borrows, until the ratio is back at or below 0.95.
- `0x811` answers a user's borrow and supply (basis and value, token wei) and `0x812` a reserve
  (rates and utilization in bps, oracle price, LTV, totals). `/info` answers borrowLendUserState,
  borrowLendReserveState and allBorrowLendReserveStates, and `set hypercore mirror` copies a
  user's borrow and lend positions and abstraction.

In the 2026-11-02 run, a wallet supplied 10 HYPE, switched to portfolio margin and sent 100 USDC
it did not hold. `0x811` read a USDC borrow of 10,000,000,000 wei and 10 HYPE supplied, `0x812`
showed USDC's total borrowed up by the same amount, and after `time --advance 7200` the borrow read
10,000,114,154 wei, two hours at 5% a year. A mirror of a mainnet portfolio margin account copied
its 3 borrow and lend tokens.

In another 2026-11-02 run, a wallet supplied 10 HYPE at 86.128 and borrowed 473 USDC. HYPE at 85%
of that left it healthy. At 65% the backstop took 6 HYPE in three steps, worth 335.8992 USD, and
`0x811` and borrowLendUserState read 4 HYPE supplied and 137.1008 USDC borrowed. On xyz:HOOD, an
isolated long took 50 USD of added margin, `0x813` read the margin back, and a CoreWriter close
returned it to the xyz balance.

## HyperEVM testnet

`hyperevm-testnet` forks HyperEVM Testnet (chain id 998) from
`https://rpc.hyperliquid-testnet.xyz/evm`, under `CHAINSIM_HYPEREVM_TESTNET_RPC`, with the same
proxy and HyperCore features. Its info API is `https://api.hyperliquid-testnet.xyz/info`, under
`CHAINSIM_HYPERLIQUID_TESTNET_INFO`.

- `up` seeds the BTC (3), ETH (4), SOL (0) and HYPE (135) perps, USDC and HYPE, which is token
  1105 on testnet, and HYPE/USDC spot 1035. HYPE sent to `0x2222…2222` credits token 1105.
- `fund` covers WHYPE (`0x5555…5555`) and PURR (`0xa9056c15938f9aff34cd497c722ce33db0c2fd57`,
  linked to Core token 1) by setting balances directly, so no holder is needed. Testnet's USDC
  entry is not an ERC-20 on HyperEVM.
- `selftest` checks nine parts as on mainnet, with PURR for the token transfer.

In the 2026-11-02 run, `up` took 18.6 s, `selftest` passed 9 of 9, `fund` gave 5 HYPE and 100 PURR,
and a CoreWriter IOC on BTC filled against the testnet book:

```text
filled 0.00001 at 79352.0, fee 0.000357 USD
```

## CoreWriter actions

CoreWriter at `0x3333333333333333333333333333333333333333` keeps its mainnet code and emits
`RawAction` for each action. `call <name> corewriter_actions` reads those logs since the fork
block, or since `[{"fromBlock": <n>}]`, and decodes them: the sender, the version, the action id
and name, and the action's fields, with integers as decimal strings. An action id chainsim does
not know comes back as `raw` hex.

This sends a limit order for perp 0 as `0x0000…c0ffee`, then reads it back. The output below is
from the 2026-11-01 run, before the local HyperCore. Today each entry also carries a `result`,
as in [Orders](#orders):

```sh
chainsim send hyperevm-mainnet --as 0x0000000000000000000000000000000000c0ffee \
  --to 0x3333333333333333333333333333333333333333 --sig "sendRawAction(bytes)" \
  0x01000001000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000010000000000000000000000000000000000000000000000000000065dd083700000000000000000000000000000000000000000000000000000000000000186a0000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000000
chainsim call hyperevm-mainnet corewriter_actions
```

```json
[
  {
    "block": 50151647,
    "txHash": "0xfb1331fcc1f818d06bd6b4e37e4bd467df60a258c1dce85638897df655f8a3b2",
    "logIndex": "0x0",
    "user": "0x0000000000000000000000000000000000C0FFEE",
    "version": 1,
    "actionId": 1,
    "action": "limitOrder",
    "fields": { "asset": "0", "isBuy": true, "limitPx": "7000000000000", "sz": "100000", "reduceOnly": false, "encodedTif": "2", "cloid": "0" }
  }
]
```

chainsim decodes action ids 1 to 13, 15 and 16: limit order, vault transfer, token delegate,
staking deposit and withdraw, spot send, USD class transfer, finalize EVM contract, add API
wallet, cancel by order id and by client order id, approve builder fee, send asset, borrow and
lend, and set abstraction. Limit orders, cancels, spot sends and USD class transfers change Core
state, and the others are recorded only. `status` counts the captured actions as
`hypercore.coreWriterActions`.

## Fund

```sh
chainsim fund hyperevm-mainnet 0x0000000000000000000000000000000000c0ffee --native 10 --token USDC:1000
```

```json
{
  "address": "0x0000000000000000000000000000000000C0FFEE",
  "balances": [
    { "asset": "HYPE", "address": null, "decimals": 18, "amount": "10", "raw": "10000000000000000000" },
    { "asset": "USDC", "address": "0xb88339CB7199b77E23DB6E890353E22632Ba630f", "decimals": 6, "amount": "1000", "raw": "1000000000" }
  ]
}
```

- `--native` sets the HYPE balance.
- `--token` takes a symbol or an ERC-20 address. The profile tokens:
  - USDC `0xb88339CB7199b77E23DB6E890353E22632Ba630f`, 6 decimals
  - USD₮0 `0xB8CE59FC3717ada4C02eaDF9682A9e934F625ebb` as `USDT0`, 6 decimals
  - WHYPE `0x5555555555555555555555555555555555555555`, 18 decimals
  - UBTC `0x9FDBdA0A5e284c32744D2f17Ee5c74B284993463`, 8 decimals
  - UETH `0xBe6727B535545C67d5cAa73dEa54865B92CF7907`, 18 decimals
- Every balance is read back before `fund` returns. An address that holds HYPE or has sent a
  transaction on HyperEVM Mainnet exits 7.

## Send as any address

`send --as` sends a transaction as any address, without its key. This sends 250 USDC from the
CoreDepositWallet, which holds USDC on HyperEVM, to `0x0000…c0ffee`:

```sh
chainsim send hyperevm-mainnet --as 0x6B9E773128f453f5c2C60935Ee2DE2CBc5390A24 \
  --to 0xb88339CB7199b77E23DB6E890353E22632Ba630f --sig "transfer(address,uint256)" \
  0x0000000000000000000000000000000000c0ffee 250000000
```

```json
{
  "txHash": "0x194343a099423025cebb42f8912d9dd9a6430b0ed2ecb36429dad41b9e1b3c12",
  "status": 1,
  "gasUsed": "44867",
  "blockNumber": 50151646,
  "from": "0x6B9E773128f453f5c2C60935Ee2DE2CBc5390A24",
  "to": "0xb88339CB7199b77E23DB6E890353E22632Ba630f",
  "topUp": { "amount": "0.000097644019945992", "symbol": "HYPE" },
  "note": "This result depends on a chainsim override that does not exist on mainnet (impersonation or a state change). chainsim status <name> lists every modification until down."
}
```

A sender without HYPE for gas gets a top-up first, reported as `topUp`.

## Set

Besides `set precompile` and `set hypercore`, `set storage`, `set code`, `set balance` and
`set token` work as on the other EVM chains, and so do `time` and `mine`. `set owner`,
`set object` and `set resource` exit 3.

## Snapshot and revert

```sh
chainsim snapshot hyperevm-mainnet
chainsim revert hyperevm-mainnet 0x2
```

```json
{ "id": "0x2" }
{ "reverted": "0x2", "snapshot": "0x3" }
```

The HyperCore answers live in the mock contracts' storage and Core state at `0x9999…9999`, so a
snapshot includes them. In the 2026-11-01 run, the BTC oracle price was set to 1 after the
snapshot, and `revert` brought it back to 650000. In the 2026-11-02 run, an IOC buy after the
snapshot moved the position, and after `revert` `hypercore_state` matched what it was before. As on every chain, `revert` consumes its id and returns a fresh snapshot of the restored
state.

## Call

`call` sends any JSON-RPC method to the fork, as on the other EVM chains, and adds
`corewriter_actions` and `hypercore_state`. A precompile answers `eth_call` like any contract, since the mock is
ordinary code on the fork.

## Status and clean up

```sh
chainsim status hyperevm-mainnet
chainsim down hyperevm-mainnet
```

`status` adds the head block, the captured CoreWriter action count, a summary of Core state and
the `modifications` list, every `send --as` and `set` since `up`. From the 2026-11-02 run,
shortened here:

```json
{
  "head": 50165647,
  "uptimeSecs": 36,
  "hypercore": {
    "seeded": 17,
    "coreWriterActions": 5,
    "coreState": { "accounts": 2, "openOrders": 0, "books": ["BTC", "ETH", "SOL", "HYPE"], "appliedThrough": 50165647 }
  },
  "modifications": [
    { "kind": "set-hypercore", "helper": "price", "perp": 0, "px": 650000, "at": 1793600051 },
    { "kind": "set-hypercore", "helper": "mirror", "user": "0x010461C14e146ac35Fe42271BDC1134EE31C703a", "positions": 177, "at": 1793600054 }
  ]
}
```

`down` removes the engine, the sidecar and the network chainsim made.
