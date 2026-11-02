# chainsim's HyperEVM proxy compiled from this repository. The engine workflow
# builds the `build` stage per architecture from the repository root and
# publishes /chainsim-hyperproxy as a release asset. The final stage copies the
# binary onto the pinned debian base, for a manual local build of a version
# that has no release yet:
#
#   docker build -f engines/hyperevm-proxy/build.Dockerfile \
#     --build-arg BASE=<the lock's base> -t <the lock's image> .
ARG TOOLCHAIN=rust:1.88-bookworm@sha256:af306cfa71d987911a781c37b59d7d67d934f49684058f96cf72079c3626bfe0
ARG BASE=debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3
FROM ${TOOLCHAIN} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock engines.lock.json ./
COPY crates ./crates
COPY profiles ./profiles
COPY engines ./engines
RUN set -eu; \
    cargo build --locked --release -p chainsim-hyperproxy; \
    install -m 0755 target/release/chainsim-hyperproxy /chainsim-hyperproxy; \
    rm -rf target /usr/local/cargo/registry /usr/local/cargo/git
FROM ${BASE}
COPY --from=build /chainsim-hyperproxy /usr/local/bin/chainsim-hyperproxy
USER 10001:10001
ENTRYPOINT ["chainsim-hyperproxy"]
