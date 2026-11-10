#!/bin/bash
# Benches a chainsim fork of HyperEVM mainnet: cold starts, order to fill, sustained orders, proxy
# overhead and throughput, mirror and replay scaling, memory and image sizes. It writes
# bench.json and README.md in this folder.
# Run it with scripts/docker.sh bench/run.sh. It starts and removes its own forks, one at a time,
# and waits a minute between cold starts on the public RPC.
set -euo pipefail
cd "$(dirname "$0")"
export CHAINSIM_FORK=${CHAINSIM_FORK:-hl-bench}
export BENCH_KEY=${BENCH_KEY:-$(cast keccak "chainsim-hl-bench-test-key")}   # test key with no mainnet history

cleanup(){ docker network disconnect "chainsim-$CHAINSIM_FORK" "$(hostname)" >/dev/null 2>&1 || true; chainsim down "$CHAINSIM_FORK" >/dev/null 2>&1 || true; }
trap cleanup EXIT

if [ "$(chainsim ls | jq '.data | length')" != 0 ]; then echo "bench: another fork is running, remove it first (chainsim ls)" >&2; exit 1; fi
yarn install --frozen-lockfile --silent
node bench.mjs
