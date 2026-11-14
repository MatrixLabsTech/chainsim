# chainsim

Local mainnet forks in Docker, with a JSON CLI built for scripts and AI agents.

[![ci](https://github.com/MatrixLabsTech/chainsim/actions/workflows/ci.yml/badge.svg)](https://github.com/MatrixLabsTech/chainsim/actions/workflows/ci.yml)
[![release](https://img.shields.io/github/v/release/MatrixLabsTech/chainsim)](https://github.com/MatrixLabsTech/chainsim/releases)
[![license](https://img.shields.io/github/license/MatrixLabsTech/chainsim)](LICENSE)

chainsim starts a private copy of a real chain that reads live mainnet state on demand, so tests
can use real contracts, pools and tokens with test wallets and no real money. Each chain family
needs a different engine with its own flags, readiness signal and funding method. chainsim puts
them behind one CLI, so `up`, `fund`, `snapshot`, `revert`, `send` and `down` work the same way
on every supported chain.

<p align="center">
  <a href="chains/ethereum/README.md"><img src="assets/chains/ethereum.svg" height="48" alt="Ethereum"></a>
  &nbsp;&nbsp;&nbsp;
  <a href="chains/arc/README.md"><img src="assets/chains/arc.svg" height="48" alt="Arc"></a>
  &nbsp;&nbsp;&nbsp;
  <a href="chains/robinhood/README.md"><img src="assets/chains/robinhood-chain.png" height="48" alt="Robinhood Chain"></a>
  &nbsp;&nbsp;&nbsp;
  <a href="chains/sui/README.md"><img src="assets/chains/sui.svg" height="48" alt="Sui"></a>
  &nbsp;&nbsp;&nbsp;
  <a href="chains/aptos/README.md"><img src="assets/chains/aptos.svg" height="48" alt="Aptos"></a>
  &nbsp;&nbsp;&nbsp;
  <a href="chains/hyperliquid/README.md"><img src="assets/chains/hyperliquid.svg" height="48" alt="Hyperliquid"></a>
  &nbsp;&nbsp;&nbsp;
  <a href="chains/solana/README.md"><img src="assets/chains/solana.svg" height="48" alt="Solana"></a>
</p>

## What it solves

- Every chain family forks with a different engine: anvil for Ethereum, Arbitrum chains and
  HyperEVM, Circle's arc-foundry tarballs for Arc, Mysten's gRPC-only `sui-fork` for Sui, the
  Aptos CLI's simulation sessions for Aptos, Surfpool for Solana. Each has its own flags, readiness signal and funding
  method. chainsim pins each engine, pulls or builds it on first use, and puts all of them
  behind the same commands.
- Testing admin-only paths of a deployed protocol normally needs keys nobody on the test team
  holds. `send --as` sends as the admin on the fork, and `set` changes state directly.
- Funding a test wallet with real tokens on a fork means finding each token's balance slot or
  owned coins. `fund` and `set` do it per chain and read the result back.
- A fork differs from mainnet in ways that are easy to miss. Each chain's `limits` list those
  differences in plain sentences for a test report, and `selftest` checks a fork before a test
  relies on it.
- Fork tools print logs written for people. chainsim prints one JSON object per command with a
  fixed exit code and the next command to run, so a script or an AI agent can act on it.
- Public RPC endpoints rate-limit and keep only recent state, and the failures are hard to
  read. chainsim reports them as upstream errors that name the variable to set and say when an
  archive node is needed.

## Features

- One command set for Ethereum, Arc, Robinhood Chain, HyperEVM, Sui, Aptos and Solana mainnet forks,
  each running in Docker, with the same JSON envelope and exit codes on every chain.
- `send --as` sends as any account without its key, including a protocol's admin. On Sui and
  Aptos, chainsim handles the Move authentication for you.
- `fund` gives a test wallet native and token balances by symbol and reads them back, with no
  balance slots, coin objects or fungible stores to look up.
- `set` overrides state directly: balances, tokens, storage, code, owners, Move objects and
  resources, and HyperCore state.
- `snapshot` and `revert` reset a fork between scenarios, and `time` and `mine` move its clock.
- `selftest` proves a fork is fit before a test spends anything on it.
- `chainsim chains` prints every chain's engine and which commands it supports.
- One-line install from GitHub releases for Linux and macOS, `chainsim doctor` to check the
  setup, and a Claude Code and Codex plugin that teaches agents the workflow.
- Forks publish no host ports by default, and provider keys never appear in output.

## Install

chainsim needs Docker. Install the latest release on Linux (x86_64 or arm64) or macOS (Apple
silicon):

```sh
curl -fsSL https://raw.githubusercontent.com/MatrixLabsTech/chainsim/main/install.sh | sh
chainsim doctor
```

The script checks the archive's SHA-256 and copies `chainsim` to `~/.local/bin`. Pass
`sh -s -- --version v0.1.0` for a given version. `chainsim doctor` reports Docker, the engine
images, the plugin and each chain's upstream variable.

From source, with Rust 1.88 or later:

```sh
git clone https://github.com/MatrixLabsTech/chainsim
cd chainsim
cargo install --locked --path crates/chainsim
```

## Quick start

Fork Ethereum mainnet, fund a test wallet, pause USDC as its pauser without the pauser's key,
and clean up:

```sh
chainsim up ethereum-mainnet
chainsim selftest ethereum-mainnet
chainsim fund ethereum-mainnet 0xd8d244aF69B839999883F9f32a68C7F184012e8a --native 10 --token USDC:1000
chainsim send ethereum-mainnet --as 0x4914f61d25e5c567143774b76edbf4d5109a8566 \
  --to 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48 --sig "pause()"
chainsim status ethereum-mainnet
chainsim down ethereum-mainnet
```

`up` prints the `fork` object with the in-network `rpcUrl` and the chain's `limits`. A fork is
reachable from containers on its Docker network. From the host, use `chainsim call`, or start
the fork with `--publish`.

## Built for agents

- Every command prints one JSON object on stdout, even on error. Logs go to stderr.
- Exit codes are fixed, so an agent branches on the code: 0 ok, 1 error, 2 usage, 3 unsupported,
  4 not found, 5 upstream failure, 6 selftest failed, 7 refused for safety, 8 conflict.
- On failure, `error.next` is the next command to run:

  ```console
  $ chainsim status demo | jq -r '.error.code, .error.next'
  fork_not_found
  chainsim ls
  ```

- `up`, `down` and `fund` are safe to repeat: a running fork is returned as is, a missing fork
  is already down, and `fund` sets balances to the asked amounts.
- `describe` and the `fork` object carry each chain's `limits`, plain sentences an agent copies
  into its report.
- `selftest` exits 0 only when every check passes, with each check's cause in `data`.
- `status` lists every `fund`, `send` and `set` under `modifications`, so a report can say which
  results depend on a change mainnet would not allow.
- The Claude Code and Codex plugin adds a skill that starts with a setup check through
  `chainsim doctor`, then walks through up, selftest, fund, snapshot and revert, and cleanup.

The full contract, with every error code, is in [docs/agent-contract.md](docs/agent-contract.md).

## Supported chains

| Chain | Engine | Details |
| --- | --- | --- |
| <img src="assets/chains/ethereum.svg" height="32" alt="Ethereum"> &nbsp;**Ethereum mainnet** `ethereum-mainnet` | Foundry anvil v1.8.5 | [chains/ethereum](chains/ethereum/README.md) |
| <img src="assets/chains/arc.svg" height="32" alt="Arc"> &nbsp;**Arc mainnet** `arc-mainnet` | Circle arc-foundry v0.8.0-2 | [chains/arc](chains/arc/README.md) |
| <img src="assets/chains/robinhood-chain.png" height="32" alt="Robinhood Chain"> &nbsp;**Robinhood Chain mainnet** `robinhood-mainnet` | Foundry anvil v1.8.5 with an ArbGasInfo mock | [chains/robinhood](chains/robinhood/README.md) |
| <img src="assets/chains/sui.svg" height="32" alt="Sui"> &nbsp;**Sui mainnet** `sui-mainnet` | Mysten sui-fork mainnet-v1.82.3-de683df9e4e0, built with chainsim's SetObject patch | [chains/sui](chains/sui/README.md) |
| <img src="assets/chains/aptos.svg" height="32" alt="Aptos"> &nbsp;**Aptos mainnet** `aptos-mainnet` | Aptos CLI 9.6.0 simulation sessions | [chains/aptos](chains/aptos/README.md) |
| <img src="assets/chains/solana.svg" height="32" alt="Solana"> &nbsp;**Solana mainnet** `solana-mainnet` | Surfpool 1.7.0 | [chains/solana](chains/solana/README.md) |
| <img src="assets/chains/hyperliquid.svg" height="32" alt="Hyperliquid"> &nbsp;**HyperEVM mainnet** `hyperevm-mainnet` | Foundry anvil v1.8.5 with HyperCore modeled on the copy | [chains/hyperliquid](chains/hyperliquid/README.md) |

The chain logos are trademarks of their owners and identify the chains only.

What each chain adds:

- [Ethereum](chains/ethereum/README.md): mainnet state, with USDC and WETH funded by symbol and any other ERC-20 by address.
- [Arc](chains/arc/README.md): Circle's own engine, so native USDC behaves as on Arc.
- [Robinhood Chain](chains/robinhood/README.md): Arbitrum's ArbSys and ArbGasInfo precompiles answer on the copy.
- [Sui](chains/sui/README.md): `set object` changes any Move object's fields, and SUI and USDC funding takes seconds.
- [Aptos](chains/aptos/README.md): a fullnode REST endpoint for the Aptos SDKs, entry functions as any account, APT and native USDC funding, and `set resource` for any Move resource.
- [Solana](chains/solana/README.md): SOL, USDC and any SPL or Token-2022 token funded by mint, SOL transfers and SDK-built transactions sent as any account, and Surfpool's cheatcodes through `call`.
- [HyperEVM](chains/hyperliquid/README.md): a local HyperCore, so CoreWriter orders fill against the mainnet book, prices, funding and liquidations can be scripted or replayed, and an RPC proxy serves any client and the `/info` API.

Each chain page covers its engine, tokens, funding, `send` and `set` support, and selftest
checks. `chainsim describe <chain>` prints one chain's details.

## Claude Code and Codex plugin

Install the plugin from GitHub. In Claude Code:

```sh
claude plugin marketplace add MatrixLabsTech/chainsim
claude plugin install chainsim@chainsim
```

In Codex:

```sh
codex plugin marketplace add MatrixLabsTech/chainsim
codex plugin add chainsim@chainsim
```

Offline, `chainsim install` adds the copy of the plugin built into the binary to each CLI found
on `PATH`, and prints a suggested Claude Code permission rule.

## DeepQA

<a href="https://deepqa.app">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/deepqa-logo-dark.svg">
    <img src="assets/deepqa-logo-light.svg" height="40" alt="DeepQA">
  </picture>
</a>

chainsim is maintained by MatrixLabs and is the chain engine behind [DeepQA](https://deepqa.app),
MatrixLabs' agentic QA workflow for dApps.

## Documentation

- [docs/architecture.md](docs/architecture.md): engines, the control sidecar, networks,
  `--publish`, the engine lock, upstreams and profiles.
- [docs/agent-contract.md](docs/agent-contract.md): commands, the JSON envelope and every exit
  and error code.
- [docs/adding-a-chain.md](docs/adding-a-chain.md): a new profile, or a new chain family.
- [docs/security.md](docs/security.md): replay risk, impersonation, provider keys and container
  isolation.

## Contributing

Issues and pull requests are welcome. `cargo test --workspace` runs the unit and contract
tests. The acceptance tests start real forks, need Docker and network access, and are marked
`#[ignore]`: run them with `cargo test --workspace -- --ignored --test-threads=1`.

## License

Apache-2.0. See [LICENSE](LICENSE). The engine images chainsim pulls or builds keep their own
licenses.
