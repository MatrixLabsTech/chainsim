# The Aptos CLI from its official release zip (design section 3), for the
# Aptos simulation session engine. The release publishes no .sha256 file, so
# the zip is checked against the sha256 in engines.lock.json alone. Nothing of
# the CLI is redistributed: each build downloads it from the Aptos release.
# chainsim passes every argument from engines.lock.json.
ARG BASE
FROM ${BASE}
ARG TARBALL_URL
ARG TARBALL_SHA256_URL
ARG TARBALL_SHA256
RUN set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates libssl3 curl unzip; \
    cd /tmp; \
    curl -fsSL -o aptos-cli.zip "$TARBALL_URL"; \
    echo "$TARBALL_SHA256  aptos-cli.zip" | sha256sum -c -; \
    unzip -q aptos-cli.zip aptos; \
    install -m 0755 aptos /usr/local/bin/aptos; \
    rm -f aptos-cli.zip aptos; \
    apt-get purge -y --auto-remove curl unzip; \
    rm -rf /var/lib/apt/lists/*; \
    useradd --create-home --uid 10001 --shell /usr/sbin/nologin chainsim; \
    aptos --version
USER chainsim
WORKDIR /home/chainsim
ENTRYPOINT ["sleep"]
CMD ["infinity"]
