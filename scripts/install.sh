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
REQUESTED_VERSION="${FUNES_VERSION:-latest}"

usage() {
    echo "usage: install.sh [-b install-dir] [-v release-tag]" >&2
    exit "${1:-2}"
}

while getopts "b:v:h" opt; do
    case "$opt" in
        b) BINDIR="$OPTARG" ;;
        v) REQUESTED_VERSION="$OPTARG" ;;
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

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -qO "$2" "$1"; }
else
    echo "funes: need curl or wget on PATH to download." >&2
    exit 1
fi

valid_version() {
    case "$1" in
        '' | *[!0-9.]* | .* | *. | *..*) return 1 ;;
    esac
    old_ifs=$IFS
    IFS=.
    set -- $1
    IFS=$old_ifs
    [ "$#" -eq 3 ] || return 1
    for part in "$@"; do
        case "$part" in
            '' | *[!0-9]*) return 1 ;;
        esac
    done
}

read_version() {
    awk '
        NF == 0 { next }
        NF != 1 || seen { bad = 1; next }
        { value = $1; seen = 1 }
        END {
            if (bad || !seen) exit 1
            print value
        }
    ' "$1"
}

manifest_digest() {
    awk -v wanted="$2" '
        NF != 2 || length($1) != 64 || $1 ~ /[^0-9a-f]/ || $2 ~ /\// || seen[$2]++ {
            bad = 1
            next
        }
        $2 == wanted { digest = $1; found++ }
        END {
            if (bad || found != 1) exit 1
            print digest
        }
    ' "$1"
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        echo "funes: need sha256sum or shasum on PATH to verify the download." >&2
        return 1
    fi
}

mkdir -p "$BINDIR"
tmpdir=$(mktemp -d "$BINDIR/.funes-install.XXXXXX") || {
    echo "funes: could not create a staging directory in $BINDIR." >&2
    exit 1
}
trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM

root="https://huggingface.co/buckets/$BUCKET/resolve"
if [ "$REQUESTED_VERSION" = latest ]; then
    if ! fetch "$root/VERSION" "$tmpdir/latest-version"; then
        echo "funes: could not resolve the latest release." >&2
        exit 1
    fi
    if ! version=$(read_version "$tmpdir/latest-version"); then
        echo "funes: the latest release has an invalid VERSION marker." >&2
        exit 1
    fi
else
    version=${REQUESTED_VERSION#v}
fi

if ! valid_version "$version"; then
    echo "funes: invalid release version: $version" >&2
    exit 1
fi

tag="v$version"
release="$root/$tag"
if ! fetch "$release/VERSION" "$tmpdir/release-version" ||
   ! fetch "$release/SHA256SUMS" "$tmpdir/SHA256SUMS"; then
    echo "funes: release metadata is incomplete for $tag." >&2
    exit 1
fi
if ! tagged_version=$(read_version "$tmpdir/release-version") || [ "$tagged_version" != "$version" ]; then
    echo "funes: release metadata does not match $tag." >&2
    exit 1
fi
if ! expected=$(manifest_digest "$tmpdir/SHA256SUMS" "$asset"); then
    echo "funes: SHA256SUMS is malformed or does not contain $asset." >&2
    exit 1
fi

binary="$tmpdir/$asset"
echo "Downloading $asset ($tag)…"
if ! fetch "$release/$asset" "$binary"; then
    echo "funes: download failed: $release/$asset" >&2
    exit 1
fi
if ! actual=$(sha256_file "$binary") || [ "$actual" != "$expected" ]; then
    echo "funes: checksum verification failed for $asset; nothing was installed." >&2
    exit 1
fi

chmod +x "$binary"
if ! reported=$("$binary" --version 2>/dev/null) || [ "$reported" != "funes $version" ]; then
    echo "funes: $asset does not report the expected version $version; nothing was installed." >&2
    exit 1
fi

mv -f "$binary" "$BINDIR/funes"

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
