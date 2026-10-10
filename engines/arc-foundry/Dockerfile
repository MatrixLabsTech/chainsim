# Circle's arc-foundry from its release tarball (design section 3). The
# binaries need glibc 2.38 or later, so the base is ubuntu:24.04. chainsim
# passes every argument from engines.lock.json.
ARG BASE
FROM ${BASE}
ARG TARBALL_URL
ARG TARBALL_SHA256_URL
ARG TARBALL_SHA256
RUN set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl; \
    cd /tmp; \
    curl -fsSL -o arc-foundry.tar.gz "$TARBALL_URL"; \
    published="$(curl -fsSL "$TARBALL_SHA256_URL" | cut -d' ' -f1)"; \
    [ "$published" = "$TARBALL_SHA256" ] || { echo "published sha256 $published differs from the lock"; exit 1; }; \
    echo "$TARBALL_SHA256  arc-foundry.tar.gz" | sha256sum -c -; \
    mkdir -p /opt/arc-foundry; \
    tar -xzf arc-foundry.tar.gz -C /opt/arc-foundry; \
    for bin in anvil cast forge; do \
      path="$(find /opt/arc-foundry -type f -name "$bin" | head -n 1)"; \
      [ -n "$path" ] || { echo "$bin missing from the tarball"; exit 1; }; \
      install -m 0755 "$path" "/usr/local/bin/$bin"; \
    done; \
    rm -rf /tmp/arc-foundry.tar.gz /opt/arc-foundry /var/lib/apt/lists/*; \
    useradd --create-home --uid 10001 --shell /usr/sbin/nologin chainsim; \
    anvil --version
USER chainsim
WORKDIR /home/chainsim
ENTRYPOINT ["anvil"]
