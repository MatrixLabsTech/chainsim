# chainsim's Aptos REST endpoint from its release asset, beside the Aptos CLI
# it runs: the binary the engine workflow compiled with build.Dockerfile,
# checked against the lock's sha256, and the CLI copied from the image chainsim
# built for the aptos-cli engine. A pinned curl stage downloads the binary, and
# the final image keeps curl for reads from the upstream.
# chainsim passes every argument but CLI from engines.lock.json.
ARG BASE
ARG CLI=chainsim/aptos-cli:9.6.0
ARG FETCH=curlimages/curl:8.16.0@sha256:463eaf6072688fe96ac64fa623fe73e1dbe25d8ad6c34404a669ad3ce1f104b6
FROM ${FETCH} AS fetch
ARG TARBALL_URL
ARG TARBALL_SHA256_URL
ARG TARBALL_SHA256
# The curl image runs as curl_user, which cannot write to a WORKDIR Docker creates.
USER root
WORKDIR /tmp/fetch
RUN set -eu; \
    curl -fsSL -o rest.tar.gz "$TARBALL_URL"; \
    published="$(curl -fsSL "$TARBALL_SHA256_URL" | cut -d' ' -f1)"; \
    [ "$published" = "$TARBALL_SHA256" ] || { echo "published sha256 $published differs from the lock $TARBALL_SHA256"; exit 1; }; \
    echo "$TARBALL_SHA256  rest.tar.gz" | sha256sum -c -; \
    tar -xzf rest.tar.gz chainsim-aptos-rest; \
    rm rest.tar.gz
FROM ${CLI} AS cli
FROM ${BASE}
RUN set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates libssl3 curl; \
    rm -rf /var/lib/apt/lists/*; \
    useradd --create-home --uid 10001 --shell /usr/sbin/nologin chainsim
COPY --from=cli /usr/local/bin/aptos /usr/local/bin/aptos
COPY --from=fetch /tmp/fetch/chainsim-aptos-rest /usr/local/bin/chainsim-aptos-rest
USER chainsim
WORKDIR /home/chainsim
ENTRYPOINT ["chainsim-aptos-rest"]
