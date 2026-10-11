# chainsim

chainsim runs local mainnet forks in Docker for testing. One command starts a private copy of a
real chain that reads live mainnet state on demand, so tests can use real contracts, pools and
tokens with test wallets and no real money. Every command prints one JSON object and exits with
a code a script or an AI agent can branch on, and a plugin teaches Claude Code and Codex the
workflow.

## Why it exists

Each chain family forks differently. Ethereum and Arbitrum-based chains such as Robinhood Chain
fork with Foundry's anvil.
Arc needs Circle's own build of anvil to run its native USDC. Sui forks with Mysten's
experimental `sui-fork`, which speaks gRPC instead of JSON-RPC. Each engine has its own flags,
readiness signal, funding method and failure modes, and a test suite that covers several
chains ends up carrying glue code for each one.

chainsim puts every engine behind one CLI. `up`, `fund`, `snapshot`, `revert`, `send` and `down`
work the same way on every supported chain, and each chain page lists what differs. A test
can act as any account without its key, such as a protocol's admin, and change contract state
directly, so it can cover admin-only paths of real mainnet deployments.

## Supported chains

Each chain has its own folder with a README on what is specific to it: engine, tokens, funding,
`send` and `set` support, selftest checks and limits.

| Chain | Folder | Maturity | Engine |
| --- | --- | --- | --- |
| Ethereum mainnet | [chains/ethereum](chains/ethereum/README.md) | stable | Foundry anvil v1.8.5 |
| Arc mainnet | [chains/arc](chains/arc/README.md) | stable | Circle arc-foundry v0.8.0-2 |
| Robinhood Chain mainnet | [chains/robinhood](chains/robinhood/README.md) | stable | Foundry anvil v1.8.5 with an ArbGasInfo mock |
| Sui mainnet | [chains/sui](chains/sui/README.md) | experimental | Mysten sui-fork, mainnet-v1.81.1 |

`chainsim describe` prints the same list from the binary, with each chain's limits.

## Install

chainsim needs Docker and, to build it, a Rust toolchain. Every engine runs as a pinned
container image that chainsim pulls or builds on first use, so nothing else is installed on the
host.

```sh
git clone https://github.com/Lucklyric/chainsim
cd chainsim
cargo install --locked --path crates/chainsim
```

Release binaries are not published yet. `scripts/release.sh` builds the archives (Linux in
Docker, macOS with cargo on a Mac) into `dist/`, and `sh install.sh --from dist` checks one
against `dist/checksums.txt` and copies it to `~/.local/bin`.

To add the plugin to Claude Code and Codex:

```sh
chainsim install
```

It writes the plugin to `~/.chainsim/marketplace` (or `--dir`), adds that folder as a local
marketplace in each CLI found on `PATH`, and prints a suggested Claude Code permission rule:
allow `chainsim *`, but keep asking for `chainsim down --all` and `chainsim prune`. Codex's
default sandbox blocks the Docker socket, so in Codex chainsim commands must run outside it:
approve the escalation when Codex asks, or use a sandbox setting that allows the Docker socket.

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
  `balance`, `token`, `storage`, `code` or `owner`. `chainsim describe <chain>` and the chain
  pages say which kinds each chain supports.
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
Mysten's `sui-fork` (see [chains/sui](chains/sui/README.md)). Each engine is a container. No engine is linked into the binary: chainsim drives Docker through its API.

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

## Adding a chain

An EVM chain that stock anvil can fork takes a new profile, `profiles/<chain>.json`, and no new
code: chain id, engine, upstream URL and its environment variable, tokens, overrides (such as
Robinhood's ArbGasInfo mock), warm-up transactions, selftest extras and limits. Copy
`profiles/ethereum-mainnet.json`, then add a `chains/<chain>/README.md` that says what differs.
A new chain family needs an adapter crate that implements the `Adapter` trait in
`crates/chainsim-core`, as `crates/chainsim-sui` does for Sui.

## Safety

- A fork keeps the real chain id, so a transaction signed for it is also valid on mainnet. Use
  test keys that hold no mainnet assets. `fund` refuses an address that is in use on mainnet
  (exit 7).
- `send --as` sends unsigned transactions that only a fork accepts, so impersonation cannot
  reach mainnet.
- Provider URLs stay in environment variables. chainsim never writes them to files or output,
  and removes their host from error messages, since some providers put the key in the host.
- A fork publishes no host port unless `--publish` is given, and then binds `127.0.0.1` only.
  Engines run with a read-only root filesystem, all Linux capabilities dropped and
  `no-new-privileges`.

## Roadmap

- Solana, through Surfpool, with `time` and `mine` commands for every chain family.
- Sui object-content overrides (`set object`). The engine change lives on the
  `chainsim/mainnet-v1.81.1` branch of [Lucklyric/sui](https://github.com/Lucklyric/sui), a
  fork of MystenLabs/sui, and chainsim will build its Sui engine from it.
- DeepQA's runner moves its fork handling onto chainsim (see Maintainers).
- Published release binaries, so installing does not need a Rust toolchain.

## Maintainers

MatrixLabs builds and maintains chainsim. Its first user is DeepQA, MatrixLabs' agent-based QA
service for web apps, which is moving its on-chain test runs onto chainsim forks. On Arc, the
native USDC that chainsim's engine runs lets DeepQA's runner drop the testing-only USDC
stand-in and balance-sync process it uses today.

## Contributing

Issues and pull requests are welcome. `cargo test --workspace` runs the unit and contract
tests. The acceptance tests start real forks, need Docker and network access, and are marked
`#[ignore]`: run them with `cargo test --workspace -- --ignored --test-threads=1`.

## License

Apache-2.0. See [LICENSE](LICENSE). The engine images chainsim pulls or builds keep their own
licenses.
