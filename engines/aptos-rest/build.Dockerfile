# chainsim's Aptos REST endpoint compiled from this repository. The engine
# workflow builds the `build` stage per architecture from the repository root
# and publishes /chainsim-aptos-rest as a release asset. The final stage puts
# the binary beside the Aptos CLI, for a manual local build of a version that
# has no release yet. It needs the aptos-cli engine's image (chainsim pull
# aptos-cli builds it):
#
#   docker build -f engines/aptos-rest/build.Dockerfile \
#     --build-arg BASE=<the lock's base> -t <the lock's image> .
ARG TOOLCHAIN=rust:1.88-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0
ARG BASE=debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3
ARG CLI=chainsim/aptos-cli:9.6.0
FROM ${TOOLCHAIN} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock engines.lock.json ./
COPY crates ./crates
COPY profiles ./profiles
COPY engines ./engines
RUN set -eu; \
    cargo build --locked --release -p chainsim-aptos --bin chainsim-aptos-rest; \
    install -m 0755 target/release/chainsim-aptos-rest /chainsim-aptos-rest; \
    rm -rf target /usr/local/cargo/registry /usr/local/cargo/git
FROM ${CLI} AS cli
FROM ${BASE}
RUN set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates libssl3 curl; \
    rm -rf /var/lib/apt/lists/*; \
    useradd --create-home --uid 10001 --shell /usr/sbin/nologin chainsim
COPY --from=cli /usr/local/bin/aptos /usr/local/bin/aptos
COPY --from=build /chainsim-aptos-rest /usr/local/bin/chainsim-aptos-rest
USER chainsim
WORKDIR /home/chainsim
ENTRYPOINT ["chainsim-aptos-rest"]
