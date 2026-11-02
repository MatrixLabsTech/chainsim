<p align="center"><img src="../../assets/chains/hyperliquid.svg" height="96" alt="Hyperliquid"></p>

# HyperEVM mainnet

chainsim runs a private copy of HyperEVM Mainnet, the EVM side of Hyperliquid, in Docker as the
chain `hyperevm-mainnet`. The commands and their JSON output are the same as on every chain, and
the [README](../../README.md) covers the shared workflow. This page covers what is specific to
HyperEVM. Every output below comes from a live run against HyperEVM Mainnet, on 2026-11-01 for
the precompile answers and on 2026-11-02 for the local HyperCore.

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
- Each fork is an engine container and a control sidecar on the fork's network. Both run as
  non-root users with a read-only root filesystem, every Linux capability dropped and
  `no-new-privileges`, and publish no port unless `up --publish` asks for one.

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
  "wsUrl": "ws://hyperevm-mainnet:8545",
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
  A negative `szi` is a short.
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
open orders, per account. It applies each CoreWriter action and bridge transfer in block order
when a chainsim command runs on the fork, and an action takes effect from the block after its
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
