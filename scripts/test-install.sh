#!/bin/sh
set -eu

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT HUP INT TERM

fixtures="$work/fixtures"
fakebin="$work/bin"
install="$work/install"
asset="funes-x86_64-linux"
version="9.9.9"
mkdir -p "$fixtures/v$version" "$fakebin" "$install"

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
cp "$FIXTURE_ROOT/$relative" "$output"
SH
chmod +x "$fakebin/curl"

printf '%s\n' "$version" > "$fixtures/VERSION"
printf '%s\n' "$version" > "$fixtures/v$version/VERSION"
cat > "$fixtures/v$version/$asset" <<SH
#!/bin/sh
echo 'funes $version'
SH
digest=$(sha256sum "$fixtures/v$version/$asset" | awk '{ print $1 }')
printf '%s  %s\n' "$digest" "$asset" > "$fixtures/v$version/SHA256SUMS"

PATH="$fakebin:$PATH" FIXTURE_ROOT="$fixtures" FUNES_INSTALL_DIR="$install" \
    sh scripts/install.sh >/dev/null
test "$("$install/funes" --version)" = "funes $version"

sentinel="$work/sentinel"
executed="$work/executed"
mkdir -p "$sentinel"
printf '%s\n' 'existing install' > "$sentinel/funes"
cat > "$fixtures/v$version/$asset" <<'SH'
#!/bin/sh
touch "$EXECUTED"
echo 'funes 9.9.9'
SH

if PATH="$fakebin:$PATH" FIXTURE_ROOT="$fixtures" FUNES_INSTALL_DIR="$sentinel" EXECUTED="$executed" \
    sh scripts/install.sh >/dev/null 2>&1; then
    echo "installer accepted a binary with the wrong checksum" >&2
    exit 1
fi
test ! -e "$executed"
test "$(cat "$sentinel/funes")" = "existing install"
