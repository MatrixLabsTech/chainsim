#!/bin/sh
# Install the chainsim CLI from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/MatrixLabsTech/chainsim/main/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --version v0.1.0
#   sh install.sh --from dist        # archives built by scripts/release.sh
#   sh install.sh --gh               # download through the GitHub CLI instead of curl
#
# Options: --version <tag> (default: the latest release), --dir <bin dir>
# (default $CHAINSIM_BIN_DIR, else ~/.local/bin). CHAINSIM_RELEASE_URL replaces
# the release download URL, for a mirror.
# Then run `chainsim doctor` to check the setup.

set -eu

repo=MatrixLabsTech/chainsim
bin_dir=${CHAINSIM_BIN_DIR:-$HOME/.local/bin}
from=""
version=""
use_gh=""
usage() { echo "usage: install.sh [--version <tag>] [--from <dist dir>] [--gh] [--dir <bin dir>]" >&2; exit 2; }
while [ $# -gt 0 ]; do
    case $1 in
        --from) [ $# -ge 2 ] || usage; from=$2; shift 2 ;;
        --version) [ $# -ge 2 ] || usage; version=$2; shift 2 ;;
        --dir) [ $# -ge 2 ] || usage; bin_dir=$2; shift 2 ;;
        --gh) use_gh=1; shift ;;
        *) usage ;;
    esac
done

case $(uname -s) in
    Darwin) os=darwin ;;
    Linux) os=linux ;;
    *) echo "chainsim: unsupported OS $(uname -s)" >&2; exit 1 ;;
esac
case $(uname -m) in
    x86_64 | amd64) arch=amd64 ;;
    arm64 | aarch64) arch=arm64 ;;
    *) echo "chainsim: unsupported architecture $(uname -m)" >&2; exit 1 ;;
esac
if [ "$os-$arch" = darwin-amd64 ]; then
    echo "chainsim: there is no release build for Intel Macs; build from source with cargo install --locked --git https://github.com/$repo chainsim" >&2
    exit 1
fi
name=chainsim_${os}_${arch}.tar.gz

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ -n "$from" ]; then
    [ -f "$from/$name" ] || { echo "chainsim: $from/$name not found; build it with scripts/release.sh" >&2; exit 1; }
    cp "$from/$name" "$from/checksums.txt" "$tmp/"
elif [ -n "$use_gh" ]; then
    command -v gh >/dev/null 2>&1 || { echo "chainsim: --gh needs the GitHub CLI (https://cli.github.com)" >&2; exit 1; }
    echo "downloading $name from $repo ${version:-latest} with gh"
    # shellcheck disable=SC2086
    gh release download $version -R "$repo" -p "$name" -p checksums.txt -D "$tmp"
else
    command -v curl >/dev/null 2>&1 || { echo "chainsim: curl is needed to download a release" >&2; exit 1; }
    if [ -n "${CHAINSIM_RELEASE_URL:-}" ]; then
        base=$CHAINSIM_RELEASE_URL
    elif [ -n "$version" ]; then
        base=https://github.com/$repo/releases/download/$version
    else
        base=https://github.com/$repo/releases/latest/download
    fi
    echo "downloading $name from $base"
    for f in "$name" checksums.txt; do
        curl -fsSL -o "$tmp/$f" "$base/$f" || {
            echo "chainsim: could not download $base/$f; check the version at https://github.com/$repo/releases" >&2
            exit 1
        }
    done
fi

want=$(awk -v n="$name" '$2 == n { print $1 }' "$tmp/checksums.txt")
if command -v sha256sum >/dev/null 2>&1; then
    got=$(sha256sum "$tmp/$name" | awk '{ print $1 }')
else
    got=$(shasum -a 256 "$tmp/$name" | awk '{ print $1 }')
fi
if [ -z "$want" ] || [ "$want" != "$got" ]; then
    echo "chainsim: checksum mismatch for $name" >&2
    exit 1
fi

tar -xzf "$tmp/$name" -C "$tmp" chainsim
mkdir -p "$bin_dir"
# Stage inside the destination so the final rename is atomic on one filesystem.
staged="$bin_dir/.chainsim-new-$$"
cp "$tmp/chainsim" "$staged"
chmod 755 "$staged"
mv -f "$staged" "$bin_dir/chainsim"
# Piped, --version prints a JSON envelope; keep the version number only.
installed=$("$bin_dir/chainsim" --version | sed -n 's/.*chainsim \([0-9][0-9A-Za-z.+-]*\).*/\1/p')
echo "installed chainsim ${installed:-?} to $bin_dir/chainsim"

case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) echo "note: $bin_dir is not on your PATH; add this line to your shell profile:"
       echo "  export PATH=\"$bin_dir:\$PATH\"" ;;
esac
echo "next: run 'chainsim doctor' to check Docker, the plugin and the upstreams"
