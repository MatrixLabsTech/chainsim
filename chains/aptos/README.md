<p align="center"><img src="../../assets/chains/aptos.svg" height="96" alt="Aptos"></p>

# Aptos mainnet

chainsim runs a private copy of Aptos Mainnet in Docker as the chain `aptos-mainnet`. The commands
and their JSON output are the same as on every chain, and the [README](../../README.md) covers the
shared workflow. This page covers what is specific to Aptos. Every output below comes from a live
run against Aptos Mainnet on 2026-11-01.

## What chainsim gives on Aptos

- A mainnet fork of Aptos in one command. The Aptos CLI runs transactions on forked mainnet state
  in simulation sessions, and chainsim adds the fork lifecycle around them: `up`, `status`, `ls`
  and `down`, with the session in its own Docker volume.
- `send --as` runs an entry function as any account, without its key, including a protocol's
  admin or a large token holder.
- `fund` gives a test wallet APT and Circle's native USDC in one command and reads both balances
  back.
- `set resource` replaces a Move resource under an account with the bytes you give, for state no
  transaction can reach.
- `snapshot` and `revert` reset the fork between scenarios.
- `time` and `mine` move the on-chain clock and produce blocks.
- `selftest` checks the fork before a test relies on it.
- The same JSON contract and exit codes as Ethereum, Arc, Robinhood Chain and Sui, so a script or
  an agent drives every chain the same way.

## Engine

- The engine is the Aptos CLI 9.6.0, the official release of aptos-core, which is under the
  [Innovation-Enabling Source Code License](https://github.com/aptos-labs/aptos-core/blob/aptos-cli-v9.6.0/LICENSE).
- The first `up` builds the image `chainsim/aptos-cli:9.6.0` from `engines/aptos-cli/Dockerfile`,
  and `chainsim pull aptos-cli` builds it ahead of time. The build downloads the Linux zip for the
  host (x86_64 or aarch64) from the `aptos-cli-v9.6.0` release and checks it against the sha256
  in `engines.lock.json`.
- A first `up` that built the image was ready in 18 s on arm64, and a later `up` in 2 s
  (measured on 2026-11-01).
- Each fork is an engine container that holds the simulation session in its volume, and a
  control sidecar on the fork's network. chainsim runs every operation as one CLI call on the
  session.
- Both containers run as non-root users with a read-only root filesystem, every Linux capability
  dropped and `no-new-privileges`, and publish no port.

## Upstream

- The fork reads Aptos Mainnet from the Aptos Labs REST API, `https://api.mainnet.aptoslabs.com/v1`.
- `CHAINSIM_APTOS_API_KEY` passes an API key from Aptos Labs' developer portal,
  https://geomi.dev. Set it before `up`.
- `CHAINSIM_APTOS_MAINNET_REST` replaces the endpoint with a provider REST URL.
- chainsim keeps the URL and key in the environment, and removes them from every message.

## Start a fork

```sh
export CHAINSIM_APTOS_API_KEY=<your API key>
chainsim up aptos-mainnet
chainsim selftest aptos-mainnet
```

`up` forks at the upstream's latest ledger version, and `--block <version>` forks at an older one.
The fork object, with its `limits` left out here:

```json
{
  "name": "aptos-mainnet",
  "chain": "aptos-mainnet",
  "chainId": "aptos:1",
  "maturity": "experimental",
  "forkPoint": { "kind": "version", "number": 7861083469 },
  "network": "chainsim-aptos-mainnet",
  "alias": "aptos-mainnet",
  "rpcUrl": null,
  "upstream": { "default": "https://api.mainnet.aptoslabs.com/v1", "env": "CHAINSIM_APTOS_MAINNET_REST", "set": false }
}
```

`selftest` checks five parts:

- `chain-id`: `0x1::chain_id::ChainId` reads 1 at the fork version.
- `fund`: the fork's control key gets 1 APT, and it reads back.
- `transfer`: 0.1 APT from the control key to
  `0x000000000000000000000000000000000000000000000000000000000000c5e2` moves the receiver's
  balance.
- `new-block`: a block moves the on-chain timestamp forward.
- `token-fund`: 1 USDC moves from the holder to the control key and reads back.

```json
{
  "ok": true,
  "parts": [
    { "part": "chain-id", "ok": true, "ms": 440 },
    { "part": "fund", "ok": true, "ms": 4162 },
    { "part": "transfer", "ok": true, "ms": 8949 },
    { "part": "new-block", "ok": true, "ms": 3775 },
    { "part": "token-fund", "ok": true, "ms": 10992 }
  ]
}
```

Each part also carries a `reason` sentence.

## Fund

```sh
chainsim fund aptos-mainnet 0xc0ffee --native 10 --token USDC:1000
```

```json
{
  "address": "0x0000000000000000000000000000000000000000000000000000000000c0ffee",
  "balances": [
    { "asset": "native", "address": null, "decimals": 8, "amount": "10", "raw": "1000000000" },
    { "asset": "USDC", "address": "0xbae207659db88bea0cbead6da0ed00aac12edcdda169e591cd41c94180b46f3b", "decimals": 6, "amount": "1000", "raw": "1000000000" }
  ]
}
```

- `--native` raises the APT balance with the CLI's direct store write.
- `--token USDC:<amount>` raises the balance of Circle's native USDC,
  `0xbae207659db88bea0cbead6da0ed00aac12edcdda169e591cd41c94180b46f3b`, with a transfer on the
  fork from `0xbe13ea18241ed83ada91e8f5ca37b5507b397cb66ae1e52151cf32a96547600b`, the largest
  primary USDC store in the Aptos indexer on 2026-11-01.
- Both balances are read back before `fund` returns.
- `fund` raises balances. An address that already holds more than the amount asked exits 3, and
  an address that has sent transactions on Aptos Mainnet exits 7.

## Send as any address

`send --as` runs a Move entry function as any address. `--to` names the function, with its type
arguments in angle brackets, and the arguments follow in the Aptos CLI's `<type>:<value>` form.
This sends 250 USDC from the USDC holder to `0xc0ffee`:

```sh
chainsim send aptos-mainnet --as 0xbe13ea18241ed83ada91e8f5ca37b5507b397cb66ae1e52151cf32a96547600b \
  --to '0x1::primary_fungible_store::transfer<0x1::fungible_asset::Metadata>' \
  address:0xbae207659db88bea0cbead6da0ed00aac12edcdda169e591cd41c94180b46f3b \
  address:0xc0ffee u64:250000000
```

```json
{
  "status": "success",
  "vmStatus": "Executed successfully",
  "hash": "0xecfe893fe76a403270de7b4fcebd3621de906bf88c3e217ecacb3887d40d6737",
  "gasUsed": 149,
  "gasUnitPrice": 100,
  "sequenceNumber": 9,
  "from": "0xbe13ea18241ed83ada91e8f5ca37b5507b397cb66ae1e52151cf32a96547600b",
  "to": "0x1::primary_fungible_store::transfer<0x1::fungible_asset::Metadata>",
  "note": "This result depends on a chainsim override that does not exist on mainnet (impersonation or a state change). chainsim status <name> lists every modification until down."
}
```

- chainsim signs for the sender with the fork's control key, so any address can send, including
  one that has never sent a transaction.
- A sender with less than 0.1 APT gets 1 APT for gas first, reported as `gasTopUp`.
- A transaction that aborts exits 1 with `tx_reverted`, and the Move abort is in `data.vmStatus`.

## Set

`set resource` replaces a resource under an account with its new value as BCS hex, and reads it
back. This shortens Aptos governance voting from 3 days to 1 hour on the fork, by setting
`voting_duration_secs` in `0x1::aptos_governance::GovernanceConfig`:

```sh
chainsim set aptos-mainnet resource 0x1 0x1::aptos_governance::GovernanceConfig \
  --bcs 0x0000434fd7946a00000000000000000000407a10f35a0000100e000000000000
```

```json
{
  "kind": "resource",
  "address": "0x0000000000000000000000000000000000000000000000000000000000000001",
  "structTag": "0x1::aptos_governance::GovernanceConfig",
  "value": {
    "min_voting_threshold": "30000000000000000",
    "required_proposer_stake": "100000000000000",
    "voting_duration_secs": "3600"
  }
}
```

- `set balance <address> <APT>` and `set token <address> USDC <amount>` raise a balance, as
  `fund` does. `set token --from <holder>` names the holder to take USDC from.
- Resources that live in an object group, such as fungible store and object resources, exit 3.
  `send --as` changes them through their own module.
- `set storage`, `set code`, `set owner` and `set object` exit 3 on Aptos.

## Snapshot and revert

```sh
chainsim snapshot aptos-mainnet
chainsim revert aptos-mainnet s18e412a05bfe9b03
```

```json
{ "id": "s18e412a05bfe9b03" }
{ "reverted": "s18e412a05bfe9b03", "snapshot": "s18e412a50ae45bd1" }
```

A snapshot copies the session inside the fork volume, and `revert` copies it back. In the run
above, the USDC transfer between the two raised `0xc0ffee` to 1250 USDC, and `revert` brought it
back to 1000. As on every chain, `revert` consumes its id and returns a fresh snapshot of the
restored state.

## Time and blocks

```sh
chainsim time aptos-mainnet --advance 86400
chainsim mine aptos-mainnet --blocks 3
```

```json
{ "previous": 1793578971, "timestamp": 1793665371, "timestampUsecs": "1793665371068303", "height": 1155026020 }
{ "mined": 3, "height": 1155026023, "timestamp": 1793665371, "timestampUsecs": "1793665371068306" }
```

Both run block metadata transactions through the VM, so `0x1::timestamp` and the block height
move as on the chain. `time --to <unix seconds>` moves to a given time, and `mine` produces up to
100 blocks per call.

## Call

`call` reads the fork with three methods:

```sh
chainsim call aptos-mainnet view '["0x1::primary_fungible_store::balance<0x1::fungible_asset::Metadata>", ["address:0xc0ffee", "address:0xbae207659db88bea0cbead6da0ed00aac12edcdda169e591cd41c94180b46f3b"]]'
chainsim call aptos-mainnet resource '["0x1", "0x1::aptos_governance::GovernanceConfig"]'
chainsim call aptos-mainnet ledger
```

```json
["1000000000"]
{ "min_voting_threshold": "30000000000000000", "required_proposer_stake": "100000000000000", "voting_duration_secs": "259200" }
{ "chainId": 1, "forkVersion": 7861083469, "blockHeight": 1155026023, "timestampUsecs": "1793665371068306" }
```

- `view` runs a Move view function: the function with its type arguments, then its arguments in
  `<type>:<value>` form.
- `resource` reads a resource under an account as JSON.
- `ledger` returns the chain id, fork version, block height and timestamp.

## Status and clean up

```sh
chainsim status aptos-mainnet
chainsim down aptos-mainnet
```

`status` adds the head block height and the `modifications` list, every `send --as` and `set`
since `up`, so a report can name each result that depends on impersonation or an override:

```json
{
  "head": 1155026023,
  "uptimeSecs": 122,
  "modifications": [
    { "kind": "set-resource", "address": "0x0000000000000000000000000000000000000000000000000000000000000001", "structTag": "0x1::aptos_governance::GovernanceConfig", "bytes": 32, "at": 1793579031 },
    { "kind": "send-as", "from": "0xbe13ea18241ed83ada91e8f5ca37b5507b397cb66ae1e52151cf32a96547600b", "to": "0x1::primary_fungible_store::transfer<0x1::fungible_asset::Metadata>", "hash": "0xecfe893fe76a403270de7b4fcebd3621de906bf88c3e217ecacb3887d40d6737", "status": "success", "authKeyReplaced": true, "gasTopUp": null, "at": 1793579049 }
  ]
}
```

`down` removes the engine, the sidecar, the fork volume and the network chainsim made.
