# Security

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
