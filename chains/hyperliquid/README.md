<p align="center"><img src="../../assets/chains/hyperliquid.svg" height="96" alt="Hyperliquid"></p>

# HyperEVM mainnet

chainsim runs a private copy of HyperEVM Mainnet, the EVM side of Hyperliquid, in Docker as the
chain `hyperevm-mainnet`. The commands and their JSON output are the same as on every chain, and
the [README](../../README.md) covers the shared workflow. This page covers what is specific to
HyperEVM. Every output below comes from a live run against HyperEVM Mainnet on 2026-11-01.

## What chainsim gives on HyperEVM

- A HyperEVM fork where contracts that read HyperCore run. HyperCore's prices, balances and
  positions reach HyperEVM contracts through read precompiles at `0x800` to `0x813`. A plain
  anvil fork leaves those addresses empty, so every contract that decodes their answer reverts.
  chainsim puts a mock at each of them.
- HyperCore answers seeded from mainnet at `up`, and set per test with `set hypercore` and `set
  precompile`.
- Every action a contract sends through CoreWriter, captured and decoded by `call
  corewriter_actions`.
- `fund` for HYPE and five ERC-20 tokens, and `send --as` for any account.
- `snapshot` and `revert`, which restore the HyperCore answers with the rest of the state.
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
  "sets": [
    { "kind": "precompile", "address": "0x0000000000000000000000000000000000000806", "calldata": "0x0000000000000000000000000000000000000000000000000000000000000000", "result": "0x000000000000000000000000000000000000000000000000000000000009eb10", "source": "given" },
    { "kind": "precompile", "address": "0x0000000000000000000000000000000000000807", "calldata": "0x0000000000000000000000000000000000000000000000000000000000000000", "result": "0x000000000000000000000000000000000000000000000000000000000009eb10", "source": "given" }
  ],
  "note": "This result depends on a chainsim override that does not exist on mainnet (impersonation or a state change). chainsim status <name> lists every modification until down."
}
```

The same `eth_call` then returns `0x…09eb10`, which is 650000.

- `set <name> hypercore spot-balance <user> <token> <total> [--hold <n>] [--entry-ntl <n>]`
  sets a user's HyperCore spot balance at `0x801`, in the token's raw units on HyperCore. USDC is
  token 0, with 8 decimals there, so this gives `0x0000…c0ffee` 5 USDC:

  ```sh
  chainsim set hyperevm-mainnet hypercore spot-balance 0x0000000000000000000000000000000000c0ffee 0 500000000
  ```

- `set <name> hypercore position <user> <perp> <szi> <entryNtl> [--leverage <n>]
  [--isolated-raw-usd <n>] [--isolated]` sets a user's perp position at `0x800`. A negative `szi`
  is a short.
- `set <name> precompile <address> <calldata> --result <hex>` stores any other answer, for any
  precompile from `0x800` to `0x813`.

A query nobody seeded reverts with a reason that names the fix. The oracle price of perp 2:

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

## CoreWriter actions

CoreWriter at `0x3333333333333333333333333333333333333333` keeps its mainnet code and emits
`RawAction` for each action. `call <name> corewriter_actions` reads those logs since the fork
block, or since `[{"fromBlock": <n>}]`, and decodes them: the sender, the version, the action id
and name, and the action's fields, with integers as decimal strings. An action id chainsim does
not know comes back as `raw` hex.

This sends a limit order for perp 0 as `0x0000…c0ffee`, then reads it back:

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
lend, and set abstraction. `status` counts the captured actions as `hypercore.coreWriterActions`.

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

Besides `set precompile` and `set hypercore`, `set storage`, `set code` and `set balance` work
as on the other EVM chains. `set token`, `set owner`, `set object` and `set resource` exit 3.

## Snapshot and revert

```sh
chainsim snapshot hyperevm-mainnet
chainsim revert hyperevm-mainnet 0x2
```

```json
{ "id": "0x2" }
{ "reverted": "0x2", "snapshot": "0x3" }
```

The HyperCore answers live in the mock contracts' storage, so a snapshot includes them. In the
run above, the BTC oracle price was set to 1 after the snapshot, and `revert` brought it back to
650000. As on every chain, `revert` consumes its id and returns a fresh snapshot of the restored
state.

## Call

`call` sends any JSON-RPC method to the fork, as on the other EVM chains, and adds
`corewriter_actions`. A precompile answers `eth_call` like any contract, since the mock is
ordinary code on the fork.

## Status and clean up

```sh
chainsim status hyperevm-mainnet
chainsim down hyperevm-mainnet
```

`status` adds the head block, the captured CoreWriter action count and the `modifications`
list, every `send --as` and `set` since `up`. Shortened here:

```json
{
  "head": 50151647,
  "uptimeSecs": 21,
  "hypercore": { "coreWriterActions": 1, "seeded": 17 },
  "modifications": [
    { "kind": "set-precompile", "address": "0x0000000000000000000000000000000000000801", "calldata": "0x0000000000000000000000000000000000000000000000000000000000c0ffee0000000000000000000000000000000000000000000000000000000000000000", "result": "0x000000000000000000000000000000000000000000000000000000001dcd650000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000", "source": "given", "at": 1793586289 }
  ]
}
```

`down` removes the engine, the sidecar and the network chainsim made.
