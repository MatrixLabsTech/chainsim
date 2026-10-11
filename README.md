# chainsim

chainsim runs local mainnet forks in Docker for testing. One command starts a private copy of a
real chain (Ethereum, Arc, Robinhood Chain or Sui) that reads live mainnet state on demand, so
tests can use real contracts, pools and tokens with test wallets and no real money. Every
command prints one JSON object and exits with a code an agent can branch on, and a plugin
teaches Claude Code and Codex the workflow.

## Install

chainsim needs Docker. Nothing else is installed on the host: every engine runs as a pinned
container image that chainsim pulls or builds on first use.

From a clone of this repo, build the release archives and install the one for this machine:

```sh
scripts/release.sh --targets linux-arm64
sh install.sh --from dist
```

- `scripts/release.sh` builds Linux binaries in Docker (`rust:1-bookworm`) and the macOS binary
  with cargo on a Mac, and writes `dist/chainsim_<os>_<arch>.tar.gz` plus `dist/checksums.txt`.
  With no `--targets` it builds `linux-arm64`, `linux-amd64` and `darwin-arm64`.
- `install.sh` checks the archive against `checksums.txt` and copies the binary to
  `~/.local/bin` (or `--dir <folder>`, or `$CHAINSIM_BIN_DIR`). Once a GitHub release exists,
  `install.sh --version <tag>` downloads it through `gh`, since the repo is private.

Then add the plugin to Claude Code and Codex:

```sh
chainsim install
```

It writes the plugin to `~/.chainsim/marketplace` (or `--dir`), adds that folder as a
local marketplace in each CLI found on `PATH`, and prints a suggested Claude Code permission
rule: allow `chainsim *`, but keep asking for `chainsim down --all` and `chainsim prune`.

Codex's default sandbox blocks the Docker socket, so chainsim commands must run outside it in
Codex. Approve the escalation when Codex asks, or use a sandbox setting that allows the Docker
socket.

## Quick start

This session forks Ethereum mainnet, funds a test wallet, pauses USDC as its pauser without the
pauser's key, and cleans up:

```sh
chainsim describe ethereum-mainnet
chainsim up ethereum-mainnet
chainsim selftest ethereum-mainnet
chainsim fund ethereum-mainnet 0xd8d244aF69B839999883F9f32a68C7F184012e8a --native 10 --token USDC:1000

id=$(chainsim snapshot ethereum-mainnet | jq -r .data.id)
# Pause USDC as its pauser. No key is needed: the fork impersonates the sender.
chainsim send ethereum-mainnet --as 0x4914f61d25e5c567143774b76edbf4d5109a8566 \
  --to 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 --sig "pause()"
chainsim revert ethereum-mainnet "$id"

# Send USDC from the test wallet, then override its native balance directly.
chainsim send ethereum-mainnet --as 0xd8d244aF69B839999883F9f32a68C7F184012e8a \
  --to 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 \
  --sig "transfer(address,uint256)" 0x000000000000000000000000000000000000dEaD 1000000
chainsim set ethereum-mainnet balance 0xd8d244aF69B839999883F9f32a68C7F184012e8a 5

chainsim status ethereum-mainnet
chainsim down ethereum-mainnet
```

- `up` prints the `fork` object: the in-network `rpcUrl`, the Docker `network`, the fork block,
  the upstream, and the chain's `limits`. It returns only after the engine answers and a
  warm-up transaction is mined.
- `selftest` exits 0 only when every check passes. Each check and its cause are in `data`.
- `fund` refuses an address that is in use on mainnet (exit 7), because a transaction signed
  for the fork is also valid on the real chain. Use a fresh test wallet.
- `send` and `set` change the fork in ways mainnet would not allow. `status` lists each one
  under `modifications`, so a report can say which results depend on them.

A fork is reachable from containers on its network. From the host, use
`chainsim call <name> <method> '<params>'`, or start the fork with `--publish`.

## Commands

`chainsim --help` and `chainsim <command> --help` are the full reference.

- Discover: `describe [<chain>]` lists the chains, or one chain's engine, maturity, operations,
  tokens, limits and error codes.
- Run: `up <chain>`, `ls`, `status <name>`, `down <name> | --all`.
- Use: `fund`, `snapshot`, `revert`, `call`, `send`, `set`, `selftest`. `set` takes a kind:
  `balance`, `token`, `storage`, `code`, `owner` or `object`. `chainsim describe <chain>` and
  the chain pages say which kinds each chain supports.
- Engines: `pull [<engine>]`, `engines`, `prune`.
- Plugin: `install`.

## The agent contract

- stdout carries one JSON object when it is not a terminal, or with `--json`, even on error.
  Logs go to stderr. The shape is `{"ok": true, "data": ...}` or
  `{"ok": false, "error": {"code", "message", "next"}, "data"?}`.
- `error.next` is the command that fixes or works around the error, such as setting a provider
  URL after a rate limit. Run it instead of guessing.
- Exit codes:
  - 0 ok
  - 1 error (`docker_unavailable`, `engine_failed`, `rpc_error`, `readback_mismatch`,
    `tx_reverted`, `internal`)
  - 2 usage
  - 3 unsupported on this chain, with the substitute in `next`
  - 4 not found (`fork_not_found`, `unknown_chain`, `unknown_token`, `snapshot_not_found`)
  - 5 upstream failure (`upstream_unavailable`, `rate_limited`)
  - 6 selftest failed
  - 7 refused for safety (`mainnet_address`)
  - 8 conflict (`name_conflict`)
- Repeating a command is safe: `up` returns a running fork of the same name and chain as is,
  `down` on a missing fork exits 0, and `fund` sets balances to the asked amounts and reads
  them back.
- `describe` and the `fork` object carry each chain's `limits`: plain sentences on how the fork
  differs from the real chain, to copy into a report word for word.

## How it works

### One engine per chain family

EVM chains run Foundry's anvil, and Arc runs Circle's arc-foundry build of it. Sui runs
chainsim's patched build of `sui-fork` (see [chains/sui.md](chains/sui.md)). Each engine is a
container. No engine is linked into the binary: chainsim drives Docker through its API.

### Two containers per fork

`up` creates the network `chainsim-<name>`, the engine container, and a control sidecar
(`curlimages/curl`, or `fullstorydev/grpcurl` for Sui). chainsim runs every engine call as a
`docker exec` in that sidecar, so a fork needs no host port. Docker labels on the containers
are the fork registry: chainsim keeps no state file.

### Joining a caller's network

A test harness in containers can put the fork on its own network under a host name of its
choice:

```sh
docker network create qa-net
chainsim up ethereum-mainnet --name eth-qa --network qa-net --alias eth
docker run --rm --network qa-net curlimages/curl:8.16.0 -s -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' http://eth:8545
chainsim down eth-qa
docker network rm qa-net
```

### Reaching a fork from the host

`--publish` binds a free port on `127.0.0.1` only and reports it as `fork.hostRpcUrl`, for a
tool on the host such as `forge`, a script or a browser wallet:

```sh
chainsim up ethereum-mainnet --name eth-host --publish
chainsim down eth-host
```

### The engine lock

`engines.lock.json` pins every engine image by tag and digest per architecture. For an engine
chainsim builds, it pins the source instead, such as arc-foundry's release tarball and its
checksum. `up` pulls or builds an engine on first use and tags it
`chainsim/<engine>:<version>`. `chainsim pull`, `chainsim engines` and `chainsim prune` manage
engines ahead of time:

```sh
chainsim pull foundry
chainsim engines
```

### Upstreams

Each chain reads mainnet state from a public RPC by default. For heavier runs, or to fork an
old block, set the chain's variable (for example `CHAINSIM_ETHEREUM_MAINNET_RPC`) to a provider
URL. chainsim passes it to the engine through the container environment and never writes it to
files, logs or output: errors scrub the host. `chainsim describe <chain>` shows the variable and
whether it is set.

### Profiles

Each chain is one file, `profiles/<chain>.json`, embedded in the binary: chain id, engine,
upstream, tokens, overrides, warm-up, selftest extras and limits. Adding an EVM chain takes a
new profile, not new code.

## Chains

| Chain | Page | Maturity | Engine |
| --- | --- | --- | --- |
| `ethereum-mainnet` | [chains/ethereum.md](chains/ethereum.md) | stable | Foundry anvil v1.8.5 |
| `arc-mainnet` | [chains/arc.md](chains/arc.md) | stable | Circle arc-foundry v0.8.0-2 |
| `robinhood-mainnet` | [chains/robinhood.md](chains/robinhood.md) | stable | Foundry anvil v1.8.5 with an ArbGasInfo mock |
| `sui-mainnet` | [chains/sui.md](chains/sui.md) | experimental | chainsim's patched build of sui-fork |

Solana (Surfpool) is planned. `chainsim describe` is the live list.

## Design docs

The design, its decisions and the build plan live in the maintainers' private knowledge base,
not in this repo. The code, profiles and `--help` text here are the source of truth for how
chainsim behaves.
