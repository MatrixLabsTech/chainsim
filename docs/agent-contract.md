# Agent contract

How chainsim talks to scripts and AI agents. `chainsim describe` prints the same exit codes and
error codes from the binary.

## Commands

`chainsim --help` and `chainsim <command> --help` are the full reference.

- Discover: `describe [<chain>]` lists the chains, or one chain's engine, maturity, operations,
  tokens, limits and error codes.
- Run: `up <chain>`, `ls`, `status <name>`, `down <name> | --all`.
- Use: `fund`, `snapshot`, `revert`, `call`, `send`, `set`, `selftest`. `set` takes a kind:
  `balance`, `token`, `storage`, `code` or `owner`. `chainsim describe <chain>` and the chain
  pages say which kinds each chain supports.
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
  - 7 refused for safety (`mainnet_address`)
  - 8 conflict (`name_conflict`)
- Repeating a command is safe: `up` returns a running fork of the same name and chain as is,
  `down` on a missing fork exits 0, and `fund` sets balances to the asked amounts and reads
  them back.
- `describe` and the `fork` object carry each chain's `limits`: plain sentences on how the fork
  differs from the real chain, to copy into a report word for word.
