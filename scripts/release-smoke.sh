#!/usr/bin/env sh
# Execute a packaged release archive the way an operator first meets
# it: check the archive against its `.sha256` file, unpack it into an
# empty directory, and run the bundled binary from there with every
# `BOOKRACK_*` variable removed, dotenv loading off, and an empty home,
# so nothing outside the archive can answer for it.
#
# Checks, in order; the first failure stops the run:
#   1. `<archive>.sha256` sits beside the archive and verifies it
#   2. the archive unpacks to a single `<stage>/` directory holding the
#      binary, inside `Bookrack.app/Contents/Resources` when the bundle
#      is present
#   3. `bookrack --version` prints exactly `bookrack <version>`
#   4. on an archive carrying `Bookrack.app`, the unpacked bundle passes
#      `codesign --verify --deep --strict`
#   5. `bookrack doctor --json` exits 0 or 1 (a failed row, expected
#      with no data root and no Ollama) and its one `PDFium library` row
#      is `ok` at `<stage>/<payload>/<pdfium-library-filename>`, which
#      only the copy bundled beside the binary satisfies
#
# The doctor row reports where the library was found, not that it
# binds: the report checks for the file only.
#
# Needs `jq`, `tar` or `unzip`, and `sha256sum` or `shasum`.
#
# Usage:
#   ./scripts/release-smoke.sh <archive> <version> <pdfium-library-filename>
set -eu

fail() {
  echo "release-smoke: $*" >&2
  exit 1
}

[ $# -eq 3 ] || fail "usage: $0 <archive> <version> <pdfium-library-filename>"
archive=$1
version=$2
pdfium_lib=$3

[ -f "$archive" ] || fail "no archive at $archive"
command -v jq >/dev/null 2>&1 || fail "jq is not on PATH"

archive_dir=$(cd "$(dirname "$archive")" && pwd)
archive_name=$(basename "$archive")
case $archive_name in
  *.tar.gz) stage=${archive_name%.tar.gz} ;;
  *.zip) stage=${archive_name%.zip} ;;
  *) fail "$archive_name is neither a .tar.gz nor a .zip" ;;
esac

# 1. Checksum file. It names the archive by basename, so verify from
# the archive's own directory.
[ -f "$archive_dir/$archive_name.sha256" ] || fail "no $archive_name.sha256 beside the archive"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$archive_dir" && sha256sum -c "$archive_name.sha256") >/dev/null ||
    fail "$archive_name.sha256 does not verify the archive"
else
  (cd "$archive_dir" && shasum -a 256 -c "$archive_name.sha256") >/dev/null ||
    fail "$archive_name.sha256 does not verify the archive"
fi
echo "release-smoke: checksum ok"

# 2. Unpack into an empty directory.
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/unpacked" "$work/home" "$work/cwd"
case $archive_name in
  *.tar.gz) tar -xzf "$archive_dir/$archive_name" -C "$work/unpacked" ;;
  *.zip) unzip -q "$archive_dir/$archive_name" -d "$work/unpacked" ;;
esac
top=$(ls -A "$work/unpacked")
[ "$top" = "$stage" ] || fail "archive top level is '$top', expected the single directory '$stage'"
root=$work/unpacked/$stage

if [ -d "$root/Bookrack.app" ]; then
  payload=Bookrack.app/Contents/Resources
else
  payload=.
fi
if [ -f "$root/$payload/bookrack.exe" ]; then
  bin=$root/$payload/bookrack.exe
elif [ -f "$root/$payload/bookrack" ]; then
  bin=$root/$payload/bookrack
else
  fail "no bookrack binary in $stage/$payload"
fi

# Run "$@" with no BOOKRACK_* variable but the dotenv switch, a home
# that holds nothing, and an empty working directory.
isolated() {
  (
    for name in $(env | sed -n 's/^\(BOOKRACK_[A-Za-z0-9_]*\)=.*/\1/p'); do
      unset "$name"
    done
    HOME=$work/home
    XDG_CONFIG_HOME=$work/home/.config
    XDG_DATA_HOME=$work/home/.local/share
    XDG_STATE_HOME=$work/home/.local/state
    XDG_CACHE_HOME=$work/home/.cache
    XDG_RUNTIME_DIR=$work/home/runtime
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 700 "$XDG_RUNTIME_DIR"
    BOOKRACK_NO_DOTENV=1
    export HOME XDG_CONFIG_HOME XDG_DATA_HOME XDG_STATE_HOME XDG_CACHE_HOME XDG_RUNTIME_DIR
    export BOOKRACK_NO_DOTENV
    cd "$work/cwd"
    "$@"
  )
}

# 3. Version.
got=$(isolated "$bin" --version) || fail "bookrack --version exited non-zero"
[ "$got" = "bookrack $version" ] || fail "bookrack --version printed '$got', expected 'bookrack $version'"
echo "release-smoke: $got"

# 4. Bundle signature.
if [ -d "$root/Bookrack.app" ]; then
  codesign --verify --deep --strict "$root/Bookrack.app" ||
    fail "Bookrack.app does not pass codesign --verify"
  echo "release-smoke: Bookrack.app signature ok"
fi

# 5. PDFium found beside the binary.
if isolated "$bin" doctor --json >"$work/doctor.json" 2>"$work/doctor.err"; then
  code=0
else
  code=$?
fi
if [ "$code" -ne 0 ] && [ "$code" -ne 1 ]; then
  cat "$work/doctor.err" >&2
  fail "bookrack doctor --json exited $code"
fi
row=$(jq -c '[.rows[] | select(.label == "PDFium library")]
  | if length == 1 then .[0] else error("\(length) PDFium library rows") end' \
  "$work/doctor.json") || {
  cat "$work/doctor.json" "$work/doctor.err" >&2
  fail "doctor --json did not carry exactly one PDFium library row"
}
status=$(printf '%s' "$row" | jq -r .status)
found=$(printf '%s' "$row" | jq -r .value | tr '\\' '/')
if [ "$payload" = . ]; then
  want=/$stage/$pdfium_lib
else
  want=/$stage/$payload/$pdfium_lib
fi
[ "$status" = ok ] || fail "doctor reports the PDFium library as $status: $row"
case $found in
  *"$want") ;;
  *) fail "doctor found the PDFium library at $found, not at the bundled $want" ;;
esac
echo "release-smoke: PDFium library found at the bundled copy"
