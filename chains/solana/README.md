<p align="center"><img src="../../assets/chains/solana.svg" height="96" alt="Solana"></p>

# Solana mainnet

chainsim runs a private copy of Solana Mainnet in Docker as the chain `solana-mainnet`. The
commands and their JSON output are the same as on every chain, and the [README](../../README.md)
covers the shared workflow. This page covers what is specific to Solana. Every output below comes
from one live run against Solana Mainnet on 2026-11-12.

## What chainsim gives on Solana

- A mainnet fork of Solana in one command. [Surfpool](https://github.com/solana-foundation/surfpool)
  serves Solana JSON-RPC over mainnet state that it reads account by account, and chainsim adds
  the fork lifecycle around it: `up`, `status`, `ls` and `down`, with a pinned engine image.
- `fund` gives a test wallet SOL, USDC and any SPL token in one command and reads each balance
  back. It reads the mint first, so a Token-2022 mint gets a Token-2022 token account.
- `send --as` sends a SOL transfer, or a transaction built by any SDK, as any account without
  its key, including a protocol's admin or a large holder.
- `set balance` and `set token` write an exact balance on any address.
- `snapshot` and `revert` reset the fork between scenarios, programs included.
- `time` and `mine` move the Clock sysvar and the slot forward.
- `selftest` checks the fork before a test relies on it.
- `status` lists every change made through chainsim, so a report can name what mainnet would not
  have allowed.
- The same JSON contract and exit codes as Ethereum, Arc, Robinhood Chain, HyperEVM, Sui and
  Aptos, so a script or an agent drives every chain the same way.

## Engine

- The engine is Surfpool 1.7.0, the image `surfpool/surfpool:1.7.0`, which `engines.lock.json`
  pins by digest for amd64 and arm64. The first `up` pulls it, and `chainsim pull surfpool` pulls
  it ahead of time. Nothing is built.
- `up` was ready in 1.6 to 2.7 s on arm64 with the image on the host (measured on 2026-11-12).
- Each fork is an engine container and a control sidecar on the fork's network. chainsim sends
  every engine call as JSON-RPC from the sidecar.
- The engine runs as uid 10001, not root. `up` creates the fork volume owned by that user and
  mounts it at `/data` for snapshots, and a small loop around `surfpool` lets `revert` start it
  again from a snapshot.
- Both containers run with a read-only root filesystem, every Linux capability dropped and
  `no-new-privileges`. Neither publishes a port unless you pass `--publish`, which binds the RPC
  port on `127.0.0.1` only.

## Upstream

- The fork reads Solana Mainnet from `https://api.mainnet-beta.solana.com`.
- `CHAINSIM_SOLANA_MAINNET_RPC` replaces it with a provider URL. Set it before `up`.
- chainsim keeps the URL in the container environment, never in an argument, and removes it from
  every message.
- The public endpoint throttles scans. A throttled read exits 5 and names the variable:

```sh
chainsim call solana-mainnet getTokenLargestAccounts '["EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"]'
```

```json
{"code": "rate_limited", "message": "getTokenLargestAccounts: Failed to get largest token accounts for mint EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v: HTTP status client error (429 Too Many Requests)", "next": "retry later, or export CHAINSIM_SOLANA_MAINNET_RPC=<provider URL> for a higher rate limit"}
```

## Start a fork

```sh
chainsim up solana-mainnet --publish
chainsim selftest solana-mainnet
```

`up` starts at the upstream's current slot. Solana has no pinned fork point, so `--block` exits 3:
the engine reads each account from the upstream when it is first touched. `--publish` puts the
RPC port on a free `127.0.0.1` port for tools on the host. The fork object, with its `limits`
left out here:

```json
{
  "name": "solana-mainnet",
  "chain": "solana-mainnet",
  "chainId": "solana:mainnet",
  "maturity": "experimental",
  "forkPoint": {"kind": "slot", "number": 468533539},
  "network": "chainsim-solana-mainnet",
  "alias": "solana-mainnet",
  "rpcUrl": "http://solana-mainnet:8899",
  "wsUrl": "ws://solana-mainnet:8900",
  "hostRpcUrl": "http://127.0.0.1:61504",
  "upstream": {"default": "https://api.mainnet-beta.solana.com", "env": "CHAINSIM_SOLANA_MAINNET_RPC", "set": false}
}
```

`rpcUrl` and `wsUrl` are Solana JSON-RPC and websocket endpoints for a wallet, an SDK or the
Solana CLI in a container on the fork's network. `forkPoint` is the upstream's slot read just
before the engine started.

`selftest` checks six parts:

- `genesis-hash`: the fork reports the upstream's genesis hash.
- `slot-advances`: the slot moves forward within 1.5 s.
- `mainnet-state`: the USDC mint on the fork equals the upstream's.
- `fund-native`: 1 SOL set on `US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELGZ` reads back.
- `fund-token`: 1 USDC set on the same address reads back.
- `send-as`: 0.001 SOL sent as `9JK5Y9jJJkJNbi52uCbxTtNKfmBhSsbindebh2noAWRc`, with no key, arrives.

```json
{
  "ok": true,
  "parts": [
    {"part": "genesis-hash", "ok": true, "ms": 390},
    {"part": "slot-advances", "ok": true, "ms": 1601},
    {"part": "mainnet-state", "ok": true, "ms": 510},
    {"part": "fund-native", "ok": true, "ms": 205},
    {"part": "fund-token", "ok": true, "ms": 365},
    {"part": "send-as", "ok": true, "ms": 637}
  ]
}
```

Each part also carries a `reason` sentence.

## Fund

```sh
chainsim fund solana-mainnet 8aZetj5YrYXhMT2WtqSTjYVnPW82qEVG7pxG5BcrRT6w \
  --native 5 --token USDC:1000 --token 2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo:250
```

```json
{
  "address": "8aZetj5YrYXhMT2WtqSTjYVnPW82qEVG7pxG5BcrRT6w",
  "balances": [
    {"address": null, "amount": "5", "asset": "SOL", "decimals": 9, "raw": "5000000000"},
    {"address": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "amount": "1000", "asset": "USDC", "decimals": 6, "raw": "1000000000"},
    {"address": "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo", "amount": "250", "asset": "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo", "decimals": 6, "raw": "250000000"}
  ]
}
```

- `--native` sets the SOL balance.
- `--token USDC:<amount>` sets the balance of Circle's USDC,
  `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`. Any other token is named by its mint address,
  as PayPal USD is here.
- `fund` reads the mint for its decimals and its owner program, Token or Token-2022, and writes
  the token account under that program.
- Every balance is read back before `fund` returns.
- An address that is not a Token or Token-2022 mint exits 3, and an address that holds SOL on
  Solana Mainnet exits 7.

PYUSD is a Token-2022 mint, and the wallet's one PYUSD account is owned by the Token-2022
program. This is the answer cut to the account, its owner program and its amount:

```sh
chainsim call solana-mainnet getTokenAccountsByOwner \
  '["8aZetj5YrYXhMT2WtqSTjYVnPW82qEVG7pxG5BcrRT6w", {"mint": "2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo"}, {"encoding": "jsonParsed", "commitment": "processed"}]'
```

```json
{"pubkey": "BwLBk3ZvZPzBacRYv3zJYR3RfeiEHotazA3WrPbvmUT5", "owner": "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb", "amount": "250000000"}
```

## Send as any address

`send --as` with `--to` and `--value` sends a SOL transfer as any address. This moves 1 SOL from
a mainnet wallet to the test wallet:

```sh
chainsim send solana-mainnet --as 5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9 \
  --to 8aZetj5YrYXhMT2WtqSTjYVnPW82qEVG7pxG5BcrRT6w --value 1
```

```json
{
  "status": "success",
  "signature": "2TPAuVUJ2xb3dxFWBaB4A5sjNeR17fAFPgAEXdCCKeDFqDsQDsbyVjUNBVjftMzzvH5ZDnYsSXo8PLpfC1jKsSoX",
  "slot": 468533572,
  "err": null,
  "logs": ["Program 11111111111111111111111111111111 invoke [1]", "Program 11111111111111111111111111111111 success"],
  "from": "5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9",
  "to": "8aZetj5YrYXhMT2WtqSTjYVnPW82qEVG7pxG5BcrRT6w",
  "value": "1",
  "signers": ["5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9"],
  "note": "This result depends on a chainsim override that does not exist on mainnet (impersonation or a state change). chainsim status <name> lists every modification until down."
}
```

`send --as` with `--data` takes a whole transaction as base64, built by any SDK, and needs no
signature on it. This one calls the Memo program with the memo `hi`:

```sh
chainsim send solana-mainnet --as 5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9 \
  --data AQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABAAECSMAbUFkAVFXZ3LDGvOzctPtbLqvBqagrVzkrqqQPBOYFSlNamSkhBk0k6HFg2jh8fDW13bySu4HkH6hAQQVEjQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAQEAAmhp
```

```json
{
  "status": "success",
  "signature": "4jEK3qY6tDBgCWxW7e8UhGZNFnhfcNa12EieQ64BGYCNhmjYCcDe3335RmzjBp1YQENJCVX1Z6iVQDhp91DXuqjj",
  "slot": 468533576,
  "err": null,
  "logs": [
    "Program MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr invoke [1]",
    "Program log: Memo (len 2)",
    "Program log: hi",
    "Program MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr consumed 274 of 200000 compute units",
    "Program MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr success"
  ],
  "from": "5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9",
  "signers": ["5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9"]
}
```

- The engine skips the signature check for that one request. Another client's unsigned
  transaction is still refused.
- The address must be a required signer of the transaction. chainsim sets the fork's latest
  blockhash and random signature bytes on it, so two sends never share a signature. The second
  run of the transfer above returned `2JWaEHZwkz4smfcwuQ85v4r7RFQfmbYGZLeWEbsECndbwBCDPGdJcVH8R4Texff6kLe622UwFSmU58Qq92meVoVb`.
- Legacy and version 0 transactions both work.
- A transaction that fails exits 1 with `tx_reverted`, and its logs are in `data.logs`.

## Set

```sh
chainsim set solana-mainnet balance 8aZetj5YrYXhMT2WtqSTjYVnPW82qEVG7pxG5BcrRT6w 0.25
chainsim set solana-mainnet token 8aZetj5YrYXhMT2WtqSTjYVnPW82qEVG7pxG5BcrRT6w USDC 10
```

```json
{"kind": "balance", "address": "8aZetj5YrYXhMT2WtqSTjYVnPW82qEVG7pxG5BcrRT6w", "symbol": "SOL", "amount": "0.25", "raw": "250000000"}
{"kind": "token", "address": "8aZetj5YrYXhMT2WtqSTjYVnPW82qEVG7pxG5BcrRT6w", "mint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "symbol": "USDC", "amount": "10", "raw": "10000000", "tokenAccount": "GdUB9DAWTRvt9cLgsVT4NB2B7AWME22yfQkPoHnPcyvG", "tokenProgram": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"}
```

- `set balance` and `set token` write an exact amount, higher or lower, on any address,
  including one that holds SOL on mainnet.
- Each result also carries the `note` that `send` shows.
- `set storage`, `set code`, `set owner`, `set object` and `set resource` exit 3 on Solana. Any
  account's lamports, data and owner go through Surfpool's `surfnet_setAccount`, with the data as
  hex:

```sh
chainsim call solana-mainnet surfnet_setAccount \
  '["Ep6CuSTHU4t4YLMxBxRwdmJmZbCVXsTcHaXYCVeSxbTJ", {"lamports": 1000000, "owner": "noopb9bkMVfRPU8AsbpTUg8AQkHtKwMYZiFUjNRtMmV", "data": "0102"}]'
chainsim call solana-mainnet getAccountInfo \
  '["Ep6CuSTHU4t4YLMxBxRwdmJmZbCVXsTcHaXYCVeSxbTJ", {"encoding": "base64", "commitment": "processed"}]'
```

```json
{"data": ["AQI=", "base64"], "executable": false, "lamports": 1000000, "owner": "noopb9bkMVfRPU8AsbpTUg8AQkHtKwMYZiFUjNRtMmV", "rentEpoch": 0, "space": 2}
```

## Snapshot and revert

```sh
chainsim snapshot solana-mainnet
chainsim revert solana-mainnet s18e74d9700bb735c
```

```json
{"id": "s18e74d9700bb735c"}
{"reverted": "s18e74d9700bb735c", "snapshot": "s18e74d972184d3f4"}
```

A snapshot is the engine's account export, program accounts included, kept on the fork volume.
`revert` starts the engine again from it inside the same container, which took 0.9 s here. In
the run above, a 1 SOL transfer between the two raised the test wallet to 1250000000 lamports,
and `revert` brought it back to 250000000. As on every chain, `revert` consumes its id and
returns a fresh snapshot of the restored state.

The slot and the clock after a revert are never earlier than the snapshot's. The engine no
longer knows a transaction sent before the revert by its signature.

## Time and slots

```sh
chainsim time solana-mainnet --advance 86400
chainsim mine solana-mainnet --blocks 3
```

```json
{"previous": 1794488288, "timestamp": 1794574688, "previousSlot": 468533591, "slot": 468879188}
{"mined": 3, "previousSlot": 468879188, "slot": 468879191, "timestamp": 1794574689}
```

Both move the Clock sysvar, which programs read, and the slot. After the two commands above
the Clock read slot 468879191 and Unix time 1794574689. `time --to <unix seconds>`
moves to a given time. Time moves forward only: a target in the past exits 3 and names `revert`.

Each result carries a `note`: the slot is read at processed commitment, and reads at the default
(finalized) commitment show the old slot for about 15 s after a jump.

## Programs

Surfpool reads a program from mainnet when a transaction first calls it, as the Memo call above
did. `surfnet_cloneProgramAccount` copies a program to a new address on the fork, here SPL Noop,
and a transaction then calls the copy:

```sh
chainsim call solana-mainnet surfnet_cloneProgramAccount \
  '["noopb9bkMVfRPU8AsbpTUg8AQkHtKwMYZiFUjNRtMmV", "4AftgCcLYhpnsNLnsPN55KKPkumwUuQ3J2mXRxUvJzJs"]'
chainsim send solana-mainnet --as 5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9 --data <base64 transaction>
```

```json
{"status": "success", "logs": ["Program 4AftgCcLYhpnsNLnsPN55KKPkumwUuQ3J2mXRxUvJzJs invoke [1]", "Program 4AftgCcLYhpnsNLnsPN55KKPkumwUuQ3J2mXRxUvJzJs consumed 39 of 200000 compute units", "Program 4AftgCcLYhpnsNLnsPN55KKPkumwUuQ3J2mXRxUvJzJs success"]}
```

A program of the upgradeable loader can be copied this way. A program of an older loader, such
as Memo, is refused with exit 1. A snapshot keeps program accounts, so a program put on the fork
before `snapshot` is still callable after `revert`.

## Call

`call` sends one raw JSON-RPC request to the engine, Surfpool's `surfnet_` methods included:

```sh
chainsim call solana-mainnet getBalance '["8aZetj5YrYXhMT2WtqSTjYVnPW82qEVG7pxG5BcrRT6w", {"commitment": "processed"}]'
chainsim call solana-mainnet getSlot '[{"commitment": "processed"}]'
```

```json
{"context": {"apiVersion": "4.3.2", "slot": 468533591}, "value": 250000000}
468879192
```

Pass `{"commitment": "processed"}` to a read that must see a write at once. `getSlot` at the
default commitment trails the processed slot by about 31 slots.

## Status and clean up

```sh
chainsim status solana-mainnet
chainsim down solana-mainnet
```

`status` adds the head slot and the `modifications` list: every `fund`, `set`, `send --as`,
`time`, `mine`, `revert` and `surfnet_` write made through chainsim since `up`, selftest's own
writes included. This run had 19 entries, and these are 7 of them:

```json
{
  "head": 468879396,
  "uptimeSecs": 63,
  "modifications": [
    {"address": "8aZetj5YrYXhMT2WtqSTjYVnPW82qEVG7pxG5BcrRT6w", "amount": "5", "asset": "native", "at": 1794488283, "kind": "fund", "symbol": "SOL"},
    {"at": 1794488285, "from": "5tzFkiKscXHK5ZXCGbXZxdw7gTjjD1mBwuoFbhUvuAi9", "kind": "send-as", "signature": "2TPAuVUJ2xb3dxFWBaB4A5sjNeR17fAFPgAEXdCCKeDFqDsQDsbyVjUNBVjftMzzvH5ZDnYsSXo8PLpfC1jKsSoX", "signatureCheckSkipped": true, "status": "success", "to": "8aZetj5YrYXhMT2WtqSTjYVnPW82qEVG7pxG5BcrRT6w", "value": "1"},
    {"address": "8aZetj5YrYXhMT2WtqSTjYVnPW82qEVG7pxG5BcrRT6w", "amount": "0.25", "at": 1794488286, "kind": "set-balance", "symbol": "SOL"},
    {"at": 1794488287, "kind": "call", "method": "surfnet_setAccount", "params": ["Ep6CuSTHU4t4YLMxBxRwdmJmZbCVXsTcHaXYCVeSxbTJ", {"data": "0102", "lamports": 1000000, "owner": "noopb9bkMVfRPU8AsbpTUg8AQkHtKwMYZiFUjNRtMmV"}]},
    {"at": 1794488288, "kind": "revert", "note": "account changes listed above this entry and after that snapshot are undone", "snapshot": "s18e74d9700bb735c"},
    {"at": 1794488289, "from": 1794488288, "kind": "time", "slot": 468879188, "to": 1794574688},
    {"at": 1794488289, "kind": "mine", "slot": 468879191, "slots": 3}
  ]
}
```

Entries from before a `revert` stay in the list, and the `revert` entry says which changes it
undid. A write that another client sends to `rpcUrl` is not listed.

`down` removes the engine, the sidecar, the fork volume and the network chainsim made.

## Limits

`chainsim describe solana-mainnet` and the fork object carry these `limits`, word for word, for
a test report:

- Experimental: the engine is Surfpool 1.7.0 (https://github.com/solana-foundation/surfpool), run from the image surfpool/surfpool:1.7.0 that engines.lock.json pins by digest.
- This is a private copy of Solana Mainnet: transactions run on the copy only and never appear on Solana Mainnet or its explorers.
- There is no pinned fork point, so up --block exits 3. The copy starts at the upstream's current slot and reads each account from the upstream when it is first touched, as Solana Mainnet has it at that moment. forkPoint is the upstream's slot read just before the engine started.
- The copy reads Solana Mainnet from https://api.mainnet-beta.solana.com by default, and CHAINSIM_SOLANA_MAINNET_RPC replaces it with a provider URL. The public endpoint throttles scans such as getProgramAccounts and getTokenLargestAccounts, so heavier runs need a provider URL.
- fund, set balance and set token write balances with Surfpool's surfnet_setAccount and surfnet_setTokenAccount. A token balance goes to the owner's token account under the program that owns the mint, Token or Token-2022. Nothing is sent on Solana Mainnet.
- fund refuses an address that holds SOL on Solana Mainnet. set balance and set token write any address.
- Raw account data and owner changes go through chainsim call <name> surfnet_setAccount. status lists every fund, set, send --as, time, mine, revert and surfnet_ write made through chainsim, and keeps the entries from before a revert. A write that another client sends to rpcUrl is not listed.
- getSlot at the default (finalized) commitment trails the processed slot by about 31 slots.
- Each selftest run sets 1 SOL and 1 USDC on the fixed throwaway address US517G5965aydkZ46HS38QLi7UQiSojurfbQfKCELGZ on the copy, sets 1 SOL on a second fixed address, 9JK5Y9jJJkJNbi52uCbxTtNKfmBhSsbindebh2noAWRc, and sends 0.001 SOL from the second to the first with no key. It leaves all of it in place.
- send --as skips the signature check for that one request, on the copy only. The engine keeps checking the signatures of every other client. The address must be a required signer of the transaction, and the fee payer must be a wallet (an account of the System program) that holds SOL on the copy. send sets the copy's latest blockhash and random signature bytes on the transaction, so a transaction passed with --data need not be signed.
- time and mine move the clock and the slot forward only. A target in the past exits 3: revert goes back. After a jump, reads at the default (finalized) commitment show the old slot for about 15 s.
- snapshot writes the engine's account export, program accounts included, to the fork volume: about 2 MB on a fresh fork, and more with every account and program the copy has touched. revert restarts the engine inside its container, which takes about 1 s, restores the accounts, and moves the clock forward to the snapshot's. The slot and the clock after a revert are never earlier than the snapshot's and can be later. A transaction sent before the revert is no longer known to the engine by its signature.
- An account the copy has read keeps its data when the clock moves. A Pyth price account read before time --advance 86400 kept its publish time, so a program that checks the age of a price refuses it until chainsim call <name> surfnet_setAccount writes the account again.
- chainsim call <name> surfnet_cloneProgramAccount copies a program of the upgradeable loader to a new address. A program of an older loader is refused.
- Solana signatures do not bind to this copy. A transaction signed for the copy is valid on Solana Mainnet while its blockhash is recent there. The copy's blockhashes are local, so use test keys that hold no mainnet assets.
- The websocket subscriptions rootSubscribe, blockSubscribe and voteSubscribe send nothing.
- up --publish binds the RPC port on 127.0.0.1. The websocket port is reachable on the fork's Docker network only.
