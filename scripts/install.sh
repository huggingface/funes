#!/bin/sh
# Install the funes binary from the Hugging Face bucket (huggingface/funes).
#
#   curl -fsSL https://huggingface.co/buckets/huggingface/funes/resolve/install.sh | sh
#
# Detects the platform, downloads the matching prebuilt binary, and installs it
# onto your PATH. Flags (pass after `sh -s --` when piping):
#   -b <dir>   install dir          (default: $HOME/.local/bin; env: FUNES_INSTALL_DIR)
#   -v <tag>   release tag to fetch (default: latest;           env: FUNES_VERSION)
set -eu

REPO="huggingface/funes"
BUCKET="huggingface/funes"
BINDIR="${FUNES_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${FUNES_VERSION:-latest}"

usage() {
    echo "usage: install.sh [-b install-dir] [-v release-tag]" >&2
    exit "${1:-2}"
}

while getopts "b:v:h" opt; do
    case "$opt" in
        b) BINDIR="$OPTARG" ;;
        v) VERSION="$OPTARG" ;;
        h) usage 0 ;;
        *) usage 2 ;;
    esac
done

# (OS, arch) -> the asset name the release workflow publishes. Only these two
# targets are built; everything else falls through to build-from-source.
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)                  asset="funes-x86_64-linux" ;;
    Linux-aarch64 | Linux-arm64)   asset="funes-aarch64-linux" ;;
    Darwin-arm64 | Darwin-aarch64) asset="funes-arm64-apple-darwin" ;;
    *)
        echo "funes: no prebuilt binary for $(uname -s)/$(uname -m)." >&2
        echo "Build from source: https://github.com/$REPO#building-from-source" >&2
        exit 1
        ;;
esac

# Bucket files have no revision: the root holds the latest binaries, and each release keeps a
# pinned copy under its tag. Buckets are non-versioned, so the resolve path carries no `main`.
if [ "$VERSION" = latest ]; then
    url="https://huggingface.co/buckets/$BUCKET/resolve/$asset"
else
    url="https://huggingface.co/buckets/$BUCKET/resolve/$VERSION/$asset"
fi

# Download to a temp file so a failed/partial fetch never lands on PATH.
tmp="$(mktemp 2>/dev/null || mktemp -t funes)"
trap 'rm -f "$tmp"' EXIT INT TERM

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO "$2" "$1"; }
else
    echo "funes: need curl or wget on PATH to download." >&2
    exit 1
fi

echo "Downloading $asset ($VERSION)…"
if ! fetch "$url" "$tmp"; then
    echo "funes: download failed: $url" >&2
    echo "  (no matching release published yet? see https://huggingface.co/buckets/$BUCKET)" >&2
    exit 1
fi

mkdir -p "$BINDIR"
chmod +x "$tmp"
mv "$tmp" "$BINDIR/funes"
trap - EXIT INT TERM

echo "Installed funes -> $BINDIR/funes"
"$BINDIR/funes" --version 2>/dev/null || true

case ":$PATH:" in
    *":$BINDIR:"*) ;;
    *)
        echo
        echo "$BINDIR is not on your PATH. Add it:"
        echo "  export PATH=\"$BINDIR:\$PATH\""
        ;;
esac
