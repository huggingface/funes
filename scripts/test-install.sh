#!/bin/sh
set -eu

# An explicit template: BSD mktemp requires one, GNU accepts it.
work=$(mktemp -d "${TMPDIR:-/tmp}/funes-install.XXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM

fixtures="$work/fixtures"
fakebin="$work/bin"
version="9.9.9"
mkdir -p "$fixtures/v$version" "$fakebin"

# The detected platform comes from the environment, so a single run covers every
# branch of install.sh's mapping rather than whichever host it runs on.
cat > "$fakebin/uname" <<'SH'
#!/bin/sh
set -eu
case "${1:-}" in
    -s) printf '%s\n' "$FAKE_OS" ;;
    -m) printf '%s\n' "$FAKE_ARCH" ;;
    *) exit 2 ;;
esac
SH

cat > "$fakebin/curl" <<'SH'
#!/bin/sh
set -eu
url=
output=
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) output=$2; shift 2 ;;
        -*) shift ;;
        *) url=$1; shift ;;
    esac
done
relative=${url#*/resolve/}
printf '%s\n' "$relative" >> "$REQUESTS"
cp "$FIXTURE_ROOT/$relative" "$output"
SH
chmod +x "$fakebin/uname" "$fakebin/curl"

# Mirrors install.sh's own hasher selection: macOS ships shasum, not sha256sum.
sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        echo "installer test needs sha256sum or shasum on PATH" >&2
        return 1
    fi
}

write_asset() {
    cat > "$fixtures/v$version/$1" <<SH
#!/bin/sh
echo 'funes $version'
SH
}

printf '%s\n' "$version" > "$fixtures/VERSION"
printf '%s\n' "$version" > "$fixtures/v$version/VERSION"
: > "$fixtures/v$version/SHA256SUMS"
for asset in funes-x86_64-linux funes-aarch64-linux funes-arm64-apple-darwin; do
    write_asset "$asset"
    digest=$(sha256_file "$fixtures/v$version/$asset")
    printf '%s  %s\n' "$digest" "$asset" >> "$fixtures/v$version/SHA256SUMS"
done

# Every (uname -s, uname -m) pair install.sh publishes an asset for, aliases
# included. The fixtures are byte-identical, so the request log is what proves
# the right asset was picked.
while read -r os arch asset; do
    install="$work/install-$os-$arch"
    requests="$work/requests-$os-$arch"
    mkdir -p "$install"
    : > "$requests"
    PATH="$fakebin:$PATH" FIXTURE_ROOT="$fixtures" FUNES_INSTALL_DIR="$install" \
        REQUESTS="$requests" FAKE_OS="$os" FAKE_ARCH="$arch" \
        sh scripts/install.sh >/dev/null
    test "$("$install/funes" --version)" = "funes $version"
    grep -qx "v$version/$asset" "$requests"
done <<TARGETS
Linux x86_64 funes-x86_64-linux
Linux aarch64 funes-aarch64-linux
Linux arm64 funes-aarch64-linux
Darwin arm64 funes-arm64-apple-darwin
Darwin aarch64 funes-arm64-apple-darwin
TARGETS

# A platform with no prebuilt asset stops before it touches the install dir.
unsupported="$work/unsupported"
if PATH="$fakebin:$PATH" FIXTURE_ROOT="$fixtures" FUNES_INSTALL_DIR="$unsupported" \
    REQUESTS="$work/requests-unsupported" FAKE_OS=Linux FAKE_ARCH=riscv64 \
    sh scripts/install.sh >/dev/null 2>&1; then
    echo "installer claimed a prebuilt binary for an unsupported platform" >&2
    exit 1
fi
test ! -e "$unsupported"

sentinel="$work/sentinel"
executed="$work/executed"
asset="funes-x86_64-linux"
mkdir -p "$sentinel"
printf '%s\n' 'existing install' > "$sentinel/funes"
cat > "$fixtures/v$version/$asset" <<'SH'
#!/bin/sh
touch "$EXECUTED"
echo 'funes 9.9.9'
SH

if PATH="$fakebin:$PATH" FIXTURE_ROOT="$fixtures" FUNES_INSTALL_DIR="$sentinel" \
    REQUESTS="$work/requests-tamper" FAKE_OS=Linux FAKE_ARCH=x86_64 \
    EXECUTED="$executed" sh scripts/install.sh >/dev/null 2>&1; then
    echo "installer accepted a binary with the wrong checksum" >&2
    exit 1
fi
test ! -e "$executed"
test "$(cat "$sentinel/funes")" = "existing install"
