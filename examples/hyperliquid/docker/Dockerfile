# Tools for this example in one image: chainsim built from this checkout, Foundry, Node.js 22
# with yarn, the Docker CLI and jq. scripts/docker.sh builds it and runs a script in it. Forks
# start on the host's Docker daemon through the mounted socket.
FROM rust:1.88-bookworm AS chainsim
WORKDIR /src
COPY Cargo.toml Cargo.lock engines.lock.json ./
COPY crates ./crates
COPY profiles ./profiles
COPY engines ./engines
COPY .claude-plugin ./.claude-plugin
COPY plugins ./plugins
RUN --mount=type=cache,target=/usr/local/cargo/registry --mount=type=cache,target=/src/target \
    cargo build --release --locked -p chainsim && cp target/release/chainsim /usr/local/bin/chainsim

FROM ghcr.io/foundry-rs/foundry:v1.8.5 AS foundry
FROM docker:27.3.1-cli AS docker

FROM node:22.11.0-bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git jq && rm -rf /var/lib/apt/lists/*
COPY --from=foundry /usr/local/bin/forge /usr/local/bin/cast /usr/local/bin/
COPY --from=docker /usr/local/bin/docker /usr/local/bin/docker
COPY --from=chainsim /usr/local/bin/chainsim /usr/local/bin/chainsim
WORKDIR /repo/examples/hyperliquid
