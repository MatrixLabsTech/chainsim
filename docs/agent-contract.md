# Agent contract

How chainsim talks to scripts and AI agents. `chainsim describe` prints the same exit codes and
error codes from the binary.

## Commands

`chainsim --help` and `chainsim <command> --help` are the full reference.

- Discover: `describe [<chain>]` lists the chains, or one chain's engine, maturity, operations,
  tokens, limits and error codes. `chains` lists every chain with its engine and, for each
  feature (fund native and tokens, `send --as`, each `set` kind, snapshot and revert, `time`,
  `mine`, `call`, `selftest`), whether it is supported. It prints a table in a terminal and the
  JSON envelope when piped or with `--json`.
- Run: `up <chain>`, `ls`, `status <name>`, `down <name> | --all`.
- Use: `fund`, `snapshot`, `revert`, `call`, `send`, `set`, `time`, `mine`, `selftest`. `set`
  takes a kind: `balance`, `token`, `storage`, `code`, `owner`, `object`, `resource`,
  `precompile` or `hypercore`. On HyperEVM, `set precompile <address> <calldata> (--result <hex>
  | --live)` stores a HyperCore read precompile's answer, `set hypercore price|spot-balance|position`
  sets prices and an account's Core state, `set hypercore book <coin> --live` refreshes a perp's
  order book, `set hypercore mirror <user>` copies a mainnet account onto the copy, and `set
  hypercore funding <perp> <rate>` sets a perp's hourly funding rate. `set hypercore price` also
  fills crossed resting orders and liquidates, and returns `filled` and `liquidated`. `time <name>
  --replay <perp> --from <time> --to <time>` replays hourly mainnet candles as the price path. `call <name>
  corewriter_actions [{"fromBlock": <n>}]` returns every CoreWriter action decoded with its
  `result`, and `call <name> hypercore_state <user>` returns the account's Core state as
  `clearinghouseState`, `spotClearinghouseState`, `openOrders` and `history` (fills from price
  moves, funding and liquidations).
  `chainsim describe <chain>` and the chain pages say which kinds and commands each chain
  supports, and the rest exit 3.
- Engines: `pull [<engine>]`, `engines`, `prune`.
- Setup: `doctor`, and `install` for the plugin.

## Output, exit codes and error.next

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
  - 7 refused for safety (`mainnet_address`, `latest_state_only`)
  - 8 conflict (`name_conflict`)
- Repeating a command is safe: `up` returns a running fork of the same name and chain as is,
  `down` on a missing fork exits 0, and `fund` sets balances to the asked amounts and reads
  them back.
- `describe` and the `fork` object carry each chain's `limits`: plain sentences on how the fork
  differs from the real chain, to copy into a report word for word.
- The `fork` object's `rpcUrl` is the engine's in-network RPC URL, or `null` when the engine
  serves no RPC (Aptos). `forkPoint.kind` is `block`, `checkpoint` (Sui) or `version` (Aptos).
- On HyperEVM, the `fork` object carries a `hypercore` field: the HyperCore answers `up` seeded
  and, under `core`, the order books it took. In `status` it adds `coreWriterActions`, the count
  of captured CoreWriter actions, and `coreState`. Other chains' fork objects have no such field.
- HyperEVM applies CoreWriter actions and bridge transfers to the copy's HyperCore when a
  chainsim command runs on the fork. A transaction sent straight to the RPC port by another tool
  takes effect at the next chainsim command on that fork, so run one (`status` will do) before
  reading Core state that such a transaction changed. `describe hyperevm-mainnet` lists in
  `limits` what HyperCore does not model yet, such as fees, funding and liquidations.
- `latest_state_only` (exit 7) means `up --block` was refused because the default upstream
  answers state reads at its latest block only. `error.next` names the variable that takes an
  archive provider URL.
