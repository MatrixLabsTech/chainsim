#!/bin/bash
# Runs a script of this example in a container that has every tool it needs, with chainsim built
# from this checkout. The only requirement on the host is Docker.
#   scripts/docker.sh scripts/test.sh       the unit tests and the e2e scripts
#   scripts/docker.sh bench/run.sh          the benches
# Set CHAINSIM_HYPEREVM_MAINNET_RPC to read through a provider instead of the public RPC, and
# BENCH_MACHINE to name the machine in the results.
set -euo pipefail
cd "$(dirname "$0")/../../.."
docker build -q -f examples/hyperliquid/docker/Dockerfile -t chainsim-hyperliquid-example . >/dev/null
exec docker run --rm -v /var/run/docker.sock:/var/run/docker.sock -v "$PWD":/repo \
  -e CHAINSIM_HYPEREVM_MAINNET_RPC -e BENCH_MACHINE -e CHAINSIM_FORK -e E2E_RESULTS -e BENCH_UPS \
  -e BENCH_ACCOUNTS -e REPLAY_DAY -e CHAINSIM_COMMIT \
  chainsim-hyperliquid-example "$@"
