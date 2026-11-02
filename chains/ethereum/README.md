<p align="center"><img src="../../assets/chains/ethereum.svg" height="96" alt="Ethereum"></p>

# Ethereum mainnet

`ethereum-mainnet` forks Ethereum mainnet with stock Foundry anvil. It is the reference EVM
chain: what this page says about `fund`, `send`, `set` and `selftest` also holds for Arc and
Robinhood Chain unless their pages say otherwise.

[`profiles/ethereum-mainnet.json`](../../profiles/ethereum-mainnet.json) is the source of truth for everything below. Run
`chainsim describe ethereum-mainnet` to read it from the binary.

## What chainsim solves here

- Funding an ERC-20 on a fork means writing the token's balance storage. `fund` tries anvil's
  `anvil_dealERC20`, falls back to finding the balance mapping slot (Solidity and Vyper
  layouts), and reads every balance back.
- Calling an admin function as its real role holder takes impersonation plus gas for an
  account that may hold none. `send --as` does both and returns the receipt, or the revert
  reason without sending when the call would fail.
- Forking an old block from a public endpoint fails with an error about pruned history.
  chainsim exits 5 and names `CHAINSIM_ETHEREUM_MAINNET_RPC` and the need for an archive node.

## Chain facts

- Chain id: `eip155:1`. The fork keeps chain id 1.
- Engine: `ghcr.io/foundry-rs/foundry:v1.8.5` (anvil), pinned by digest in `engines.lock.json`.
- Native token: ETH, 18 decimals.
- Upstream: `https://ethereum-rpc.publicnode.com` by default, or the URL in
  `CHAINSIM_ETHEREUM_MAINNET_RPC`.
- Engine flags: `--retries 20 --fork-retry-backoff 1500 --compute-units-per-second 1000`, so a
  burst of cold reads backs off instead of failing.

## Tokens and fund

- `USDC` at `0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48`, 6 decimals
- `WETH` at `0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2`, 18 decimals

`fund` sets the native balance with `anvil_setBalance` and an ERC-20 balance with
`anvil_dealERC20`. When the deal does not read back, it searches the token's balance mapping
slot (Solidity and Vyper layouts) and writes that. Any ERC-20 works by address, not only the
symbols above:

```sh
chainsim up ethereum-mainnet
chainsim fund ethereum-mainnet 0xd8d244aF69B839999883F9f32a68C7F184012e8a \
  --native 10 --token USDC:1000 --token 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2:2
```

Every amount is in whole units of the token. `fund` reads each balance back and fails with
`readback_mismatch` if one differs.

## send and set

- `send --as <address>` impersonates any sender, with `--sig "<fn(types)>" <args>...`, raw
  `--data <hex>`, and `--value <amount>` in ETH. When the sender cannot pay for gas, chainsim
  tops it up and reports `topUp`. A mined revert exits 1 (`tx_reverted`) with the decoded
  reason. `--sig` covers flat types (address, bool, uintN, intN, bytesN, bytes, string). Encode
  arrays and tuples elsewhere, for example with `cast calldata`, and pass `--data`.
- `set storage <address> <slot> <value>`, `set code <address> <hex or file>` and
  `set balance <address> <amount>` write state directly and read it back.
- `set token <address> <symbol or token address> <amount>` sets an ERC-20 balance the way
  `fund --token` does, on any address, and reads it back. `--from` exits 3.
- `set owner` exits 3. Use `set storage` for the slot that records an owner.

```sh
chainsim set ethereum-mainnet code 0xd8d244aF69B839999883F9f32a68C7F184012e8a 0x600160005260206000f3
chainsim set ethereum-mainnet storage 0xd8d244aF69B839999883F9f32a68C7F184012e8a 0 42
chainsim status ethereum-mainnet
```

```sh
chainsim set ethereum-mainnet token 0x0000000000000000000000000000000000c0ffee USDC 1234.5
```

```json
{ "kind": "token", "address": "0x0000000000000000000000000000000000C0FFEE", "symbol": "USDC", "token": "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", "decimals": 6, "amount": "1234.5", "raw": "1234500000" }
```

`status` lists every `send` and `set` under `modifications`. The list lives in the fork's
sidecar, so it is gone after `down`, and a `revert` does not shorten it.

## Time and blocks

```sh
chainsim time ethereum-mainnet --advance 3600
chainsim mine ethereum-mainnet --blocks 3
```

```json
{ "previous": 1793592650, "timestamp": 1793596250, "timestampUsecs": "1793596250000000", "height": 26324617 }
{ "mined": 3, "height": 26324620, "timestamp": 1793596251, "timestampUsecs": "1793596251000000" }
```

`time` sets the next block's timestamp with `evm_setNextBlockTimestamp` and mines that block.
`time --to <unix seconds>` moves to a given time. `mine` produces up to 10,000 blocks per call
with `anvil_mine`. A `revert` to a snapshot taken before both returned the fork to block
26324616 at timestamp 1793592650.

## Old blocks need an archive upstream

`--block` forks at a past block instead of the head. The public default keeps only recent
state, so a fork far behind the head fails at `up`:

```sh
chainsim up ethereum-mainnet --name eth-old --block 1000000
```

This exits 5 (`upstream_unavailable`), and `error.next` says to set
`CHAINSIM_ETHEREUM_MAINNET_RPC` to an archive provider URL. With an archive URL in that
variable, the same command starts the fork at block 1000000.

## selftest

`chainsim selftest ethereum-mainnet` runs these checks inside one snapshot and reverts it, so it
leaves no trace:

- `block`: the fork returns its latest block
- `block-time`: the latest block is within 5 minutes of the host clock
- `native-transfer`: an ETH transfer confirms within 10 s and moves the balance
- `token-transfer`: a USDC approve and transfer confirm and move the balance
- `snapshot-revert`: `evm_revert` undoes a transfer made after `evm_snapshot`
- `upstream-read`: a cold account read from the upstream answers within 10 s

```sh
chainsim selftest ethereum-mainnet
chainsim down ethereum-mainnet
```
