#!/usr/bin/env bash
# Download a pinned protoc into .tools/protoc so funes can be built without a system-wide
# install. lance's build scripts need protoc at build time; the funes binary does not.
#
# Usage:
#   ./scripts/bootstrap-protoc.sh
#   export PROTOC="$PWD/.tools/protoc/bin/protoc"
#   cargo build
set -euo pipefail

VERSION=28.3
dest=".tools/protoc"

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=osx ;;
  *) echo "unsupported OS $(uname -s) — install protoc manually" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch_64 ;;
  *) echo "unsupported arch $(uname -m) — install protoc manually" >&2; exit 1 ;;
esac

zip="protoc-${VERSION}-${os}-${arch}.zip"
url="https://github.com/protocolbuffers/protobuf/releases/download/v${VERSION}/${zip}"

echo "downloading $url"
rm -rf "$dest" && mkdir -p "$dest"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/protoc.zip" "$url"
unzip -q "$tmp/protoc.zip" -d "$dest"

echo
echo "installed $("$dest/bin/protoc" --version) at $dest"
echo "now run:  export PROTOC=\"$PWD/$dest/bin/protoc\""
