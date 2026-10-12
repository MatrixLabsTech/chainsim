# chainsim's patched sui-fork from its release asset (design section 3): the
# binary the engine workflow compiled from the pinned commit with
# build.Dockerfile, checked against the lock's sha256 and copied onto the pinned
# sui-tools image for the host. sui-tools has no curl, so a pinned curl stage
# downloads it. chainsim passes every argument from engines.lock.json.
ARG BASE
ARG FETCH=curlimages/curl:8.16.0@sha256:463eaf6072688fe96ac64fa623fe73e1dbe25d8ad6c34404a669ad3ce1f104b6
FROM ${FETCH} AS fetch
ARG TARBALL_URL
ARG TARBALL_SHA256_URL
ARG TARBALL_SHA256
# The curl image runs as curl_user, which cannot write to a WORKDIR Docker creates.
USER root
WORKDIR /tmp/fetch
RUN set -eu; \
    curl -fsSL -o sui-fork.tar.gz "$TARBALL_URL"; \
    published="$(curl -fsSL "$TARBALL_SHA256_URL" | cut -d' ' -f1)"; \
    [ "$published" = "$TARBALL_SHA256" ] || { echo "published sha256 $published differs from the lock $TARBALL_SHA256"; exit 1; }; \
    echo "$TARBALL_SHA256  sui-fork.tar.gz" | sha256sum -c -; \
    tar -xzf sui-fork.tar.gz sui-fork; \
    rm sui-fork.tar.gz
FROM ${BASE}
COPY --from=fetch /tmp/fetch/sui-fork /usr/local/bin/sui-fork
RUN ["sui-fork", "--version"]
