#!/bin/sh
# Install the chainsim CLI from a release.
#
#   sh install.sh --from dist            # artifacts from scripts/release.sh
#   sh install.sh --version v0.1.0       # a GitHub release, through gh (the repo is private)
#   sh install.sh                        # the latest GitHub release
#
# Options: --dir <bin dir> (default $CHAINSIM_BIN_DIR, else ~/.local/bin).
# Then run `chainsim install` to add the plugin to Claude Code and Codex.

set -eu

repo=Lucklyric/chainsim
bin_dir=${CHAINSIM_BIN_DIR:-$HOME/.local/bin}
from=""
version=""
usage() { echo "usage: install.sh [--from <dist dir> | --version <tag>] [--dir <bin dir>]" >&2; exit 2; }
while [ $# -gt 0 ]; do
    case $1 in
        --from) [ $# -ge 2 ] || usage; from=$2; shift 2 ;;
        --version) [ $# -ge 2 ] || usage; version=$2; shift 2 ;;
        --dir) [ $# -ge 2 ] || usage; bin_dir=$2; shift 2 ;;
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
name=chainsim_${os}_${arch}.tar.gz

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if [ -n "$from" ]; then
    [ -f "$from/$name" ] || { echo "chainsim: $from/$name not found; build it with scripts/release.sh" >&2; exit 1; }
    cp "$from/$name" "$from/checksums.txt" "$tmp/"
else
    command -v gh >/dev/null 2>&1 || { echo "chainsim: the repo is private, so a GitHub install needs gh (https://cli.github.com) logged in" >&2; exit 1; }
    echo "downloading $name from $repo ${version:-latest}"
    # shellcheck disable=SC2086
    gh release download $version -R "$repo" -p "$name" -p checksums.txt -D "$tmp"
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
    *) echo "note: $bin_dir is not on your PATH; add it to your shell profile" ;;
esac
echo "next: run 'chainsim install' to add the plugin to Claude Code and Codex"
