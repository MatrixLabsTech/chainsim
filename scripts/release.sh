#!/usr/bin/env bash
# scripts/release.sh [--targets "linux-arm64 linux-amd64 darwin-arm64"]
#
# Builds chainsim release binaries into dist/ (git-ignored):
#   1. sets the plugin manifests to the Cargo workspace version, before the
#      build, since the binary embeds the plugin
#   2. builds each target: Linux in rust:1-bookworm (glibc 2.36 or later),
#      cross-compiled when the Docker host has the other architecture, and
#      macOS with cargo on a macOS host of that architecture
#   3. writes dist/chainsim_<os>_<arch>.tar.gz and dist/checksums.txt
# It does not tag, push or publish.
set -euo pipefail
cd "$(dirname "$0")/.."

targets="linux-arm64 linux-amd64 darwin-arm64"
if [ "${1:-}" = "--targets" ]; then
  [ $# -ge 2 ] || { echo "usage: scripts/release.sh [--targets \"<os>-<arch> ...\"]" >&2; exit 2; }
  targets=$2
fi
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
[ -n "$version" ] || { echo "release: no version in Cargo.toml" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

step "plugin version $version"
for f in plugins/chainsim/.claude-plugin/plugin.json .claude-plugin/marketplace.json; do
  perl -0pi -e "s/\"version\": \"[^\"]*\"/\"version\": \"$version\"/g" "$f"
done
git --no-pager diff --stat -- plugins .claude-plugin

rm -rf dist
mkdir -p dist
built=""
failed=""

triple() { case $1 in amd64) echo x86_64-unknown-linux-gnu ;; arm64) echo aarch64-unknown-linux-gnu ;; esac; }
gcc_pkg() { case $1 in amd64) echo gcc-x86-64-linux-gnu ;; arm64) echo gcc-aarch64-linux-gnu ;; esac; }
gcc_bin() { case $1 in amd64) echo x86_64-linux-gnu-gcc ;; arm64) echo aarch64-linux-gnu-gcc ;; esac; }

build_linux() {
  local arch=$1 t script docker_arch
  t=$(triple "$arch")
  docker_arch=$(docker version --format '{{.Server.Arch}}') || return 1
  if [ "$arch" = "$docker_arch" ]; then
    script="cargo build --release --locked -p chainsim --target $t"
  else
    local linker_var
    linker_var=CARGO_TARGET_$(echo "$t" | tr 'a-z-' 'A-Z_')_LINKER
    script="apt-get update -qq && apt-get install -y -qq $(gcc_pkg "$arch") >/dev/null && rustup target add $t >/dev/null && $linker_var=$(gcc_bin "$arch") cargo build --release --locked -p chainsim --target $t"
  fi
  docker run --rm -v "$PWD":/src -w /src -v chainsim-cargo:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/src/target/release-linux rust:1-bookworm bash -c "$script" || return 1
  cp "target/release-linux/$t/release/chainsim" "dist/chainsim"
}

build_darwin() {
  local arch=$1 host
  [ "$(uname -s)" = Darwin ] || { echo "release: darwin-$arch needs a macOS host" >&2; return 1; }
  host=$(uname -m); [ "$host" = x86_64 ] && host=amd64
  [ "$host" = "$arch" ] || { echo "release: darwin-$arch needs a $arch Mac" >&2; return 1; }
  cargo build --release --locked -p chainsim || return 1
  cp target/release/chainsim dist/chainsim
}

for target in $targets; do
  os=${target%-*}
  arch=${target#*-}
  step "build $os $arch"
  case $os in
    linux) build_linux "$arch" && ok=1 || ok=0 ;;
    darwin) build_darwin "$arch" && ok=1 || ok=0 ;;
    *) echo "release: unknown target $target" >&2; ok=0 ;;
  esac
  if [ "$ok" = 1 ]; then
    # No macOS extended attributes in the archive: GNU tar warns on them.
    COPYFILE_DISABLE=1 tar --no-xattrs -czf "dist/chainsim_${os}_${arch}.tar.gz" -C dist chainsim
    rm dist/chainsim
    built="$built $target"
  else
    failed="$failed $target"
  fi
done

step "checksums"
(
  cd dist
  if command -v sha256sum >/dev/null 2>&1; then sha256sum ./*.tar.gz; else shasum -a 256 ./*.tar.gz; fi
) | sed 's| \./| |' > dist/checksums.txt
cat dist/checksums.txt

echo
echo "built:${built:- none}"
[ -z "$failed" ] || { echo "failed:$failed" >&2; exit 1; }
