# chainsim's patched sui-fork compiled from source: Mysten's sui-fork built from
# chainsim's branch of github.com/MatrixLabsTech/sui at a pinned commit, which
# adds ForkingService/SetObject. The engine workflow builds the `build` stage
# per architecture and publishes /sui-fork as a release asset. The final stage
# copies the binary onto the pinned sui-tools image, for a manual local build.
# Every argument comes from the sui-fork-chainsim entry of engines.lock.json.
# The source and cargo cache are removed in the step that builds, so the build
# stage keeps only the toolchain layer.
ARG TOOLCHAIN
ARG BASE
FROM ${TOOLCHAIN} AS build
ARG REPO
ARG COMMIT
RUN set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends cmake clang libpq-dev; \
    git init -q /src; \
    cd /src; \
    git fetch -q --depth 1 "$REPO" "$COMMIT"; \
    git checkout -q FETCH_HEAD; \
    head="$(git rev-parse HEAD)"; \
    [ "$head" = "$COMMIT" ] || { echo "fetched $head, not $COMMIT"; exit 1; }; \
    CARGO_PROFILE_RELEASE_DEBUG=0 cargo build --locked --release -j 8 --bin sui-fork; \
    install -m 0755 target/release/sui-fork /sui-fork; \
    cd /; \
    rm -rf /src /usr/local/cargo/registry /usr/local/cargo/git /var/lib/apt/lists/*
FROM ${BASE}
COPY --from=build /sui-fork /usr/local/bin/sui-fork
RUN ["sui-fork", "--version"]
