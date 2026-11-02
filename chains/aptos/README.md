# Aptos

chainsim runs a private copy of Aptos Mainnet in Docker as the chain `aptos-mainnet`. The commands
and their JSON output are the same as on every chain, and the [README](../../README.md) covers the
shared workflow. This page covers what is specific to Aptos.

## What chainsim gives on Aptos

- A mainnet fork of Aptos in one command. The Aptos CLI can run transactions on forked mainnet
  state in a simulation session, and chainsim adds the fork lifecycle around it: `up`, `status`,
  `ls` and `down`, with the session in its own Docker volume.
- `send --as` runs an entry function as any account, without its key, including a protocol's
  admin or a large token holder.
- `fund` gives a test wallet APT and Circle's native USDC in one command and reads both balances
  back.
- `set resource` replaces a Move resource under an account with the bytes you give, for state
  no transaction can reach.
- `snapshot` and `revert` reset the fork between scenarios.
- `time` and `mine` move the on-chain clock and produce blocks.
- `selftest` checks the fork before a test relies on it.
- The same JSON contract and exit codes as Ethereum, Arc, Robinhood Chain and Sui, so a script or
  an agent drives every chain the same way.

## Quick start

```sh
export CHAINSIM_APTOS_API_KEY=<your Aptos Labs API key>
chainsim up aptos-mainnet
chainsim selftest aptos-mainnet
chainsim fund aptos-mainnet <test address> --native 10 --token USDC:1000
chainsim send aptos-mainnet --as 0xbe13ea18241ed83ada91e8f5ca37b5507b397cb66ae1e52151cf32a96547600b \
  --to '0x1::primary_fungible_store::transfer<0x1::fungible_asset::Metadata>' \
  address:0xbae207659db88bea0cbead6da0ed00aac12edcdda169e591cd41c94180b46f3b \
  address:<test address> u64:1000000
chainsim call aptos-mainnet ledger
chainsim down aptos-mainnet
```

An API key comes from Aptos Labs' developer portal at https://geomi.dev.

## Commands

| Command | On Aptos |
| --- | --- |
| `up [--block <version>]` | Forks Aptos Mainnet at a ledger version, or at the head. |
| `fund --native <APT> --token USDC:<amount>` | Raises APT and USDC balances to the amounts asked and reads them back. |
| `send --as <address> --to <function> <args>...` | Runs an entry function as any address. |
| `set balance` and `set token` | Raise an APT or USDC balance, as `fund` does. `set token --from <holder>` names the holder to take USDC from. |
| `set resource <address> <struct tag> --bcs <hex>` | Replaces a resource under an account and reads it back. |
| `snapshot` and `revert` | Save the fork state and go back to it. |
| `time --advance <s>` or `--to <unix s>` | Moves the on-chain clock forward. |
| `mine [--blocks <n>]` | Produces up to 100 blocks. |
| `call <name> view\|resource\|ledger` | Reads the fork. |
| `selftest` | Checks the chain id, funding, a transfer, a block and a USDC move. |

### send --as

`--to` names a Move entry function, with its type arguments in angle brackets. The arguments
follow it in the Aptos CLI's `<type>:<value>` form, such as `address:0x1` or `u64:100`.

- chainsim signs for the sender on the fork, so any address can send, including one that has
  never sent a transaction.
- A sender with less than 0.1 APT gets 1 APT for gas first, reported as `gasTopUp`.
- The result has `hash`, `status` (`success` or `failure`), `vmStatus`, `gasUsed`,
  `gasUnitPrice` and `sequenceNumber`. A failed transaction exits 1 with `tx_reverted`, and the
  Move abort is in `data.vmStatus`.
- `chainsim status` lists every `send --as` and `set` under `modifications`.

### set resource

`--bcs` is the whole new value of the resource as BCS hex. chainsim writes it into the fork and
returns the resource as JSON.

### call

- `view` runs a Move view function:
  `'["0x1::coin::balance<0x1::aptos_coin::AptosCoin>", ["address:0x1"]]'`.
- `resource` reads a resource under an account: `'["0x1", "0x1::chain_id::ChainId"]'`.
- `ledger` returns the chain id, fork version, block height and timestamp.

## Tokens

| Symbol | Metadata | Decimals |
| --- | --- | --- |
| USDC | `0xbae207659db88bea0cbead6da0ed00aac12edcdda169e591cd41c94180b46f3b` | 6 |

USDC is Circle's native fungible asset on Aptos. `fund` takes it from
`0xbe13ea18241ed83ada91e8f5ca37b5507b397cb66ae1e52151cf32a96547600b`, the largest primary USDC
store in the Aptos indexer on 2026-11-01.

## Selftest

- `chain-id`: `0x1::chain_id::ChainId` reads 1 at the fork version.
- `fund`: the fork's control key gets 1 APT, and it reads back.
- `transfer`: 0.1 APT from the control key to the receiver
  `0x000000000000000000000000000000000000000000000000000000000000c5e2` moves the receiver's
  balance.
- `new-block`: a block moves the on-chain timestamp forward.
- `token-fund`: 1 USDC moves from the holder to the control key and reads back.

## Engine

- The engine is the Aptos CLI 9.6.0, the official release of aptos-core, which is under the
  [Innovation-Enabling Source Code License](https://github.com/aptos-labs/aptos-core/blob/aptos-cli-v9.6.0/LICENSE).
- The first `up` builds the image `chainsim/aptos-cli:9.6.0`, and `chainsim pull aptos-cli`
  builds it ahead of time. The build downloads the Linux zip for the host (x86_64 or aarch64)
  from the `aptos-cli-v9.6.0` release and checks it against the sha256 in `engines.lock.json`.
- A first `up` that built the image was ready in 18 s on arm64, and a later `up` in 2 s
  (measured on 2026-11-01).
- The engine runs as a non-root user with a read-only root filesystem, every Linux capability
  dropped and `no-new-privileges`, and publishes no port.

## Upstream

The fork reads Aptos Mainnet from the Aptos Labs REST API `https://api.mainnet.aptoslabs.com/v1`.
`CHAINSIM_APTOS_API_KEY` passes an API key, and `CHAINSIM_APTOS_MAINNET_REST` replaces the
endpoint with a provider REST URL.
