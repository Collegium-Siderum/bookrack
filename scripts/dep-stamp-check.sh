#!/usr/bin/env sh
# Fail if a behaviour-sensitive dependency moved in Cargo.lock while the
# frozen value its version stamp is pinned to did not.
#
# `tests/dep_hash.rs` in extract, normalize, ingest, and glean check the
# same thing, but a commit that changes only Cargo.lock touches no crate,
# so a gate narrowed to touched crates never runs them. This script reads
# Cargo.lock and the frozen constants straight from source, compiles
# nothing, and finishes in well under a second.
#
# Each guard's selection below mirrors its test. The two cannot drift
# apart unnoticed: the frozen value is what the test computed, so a
# selection that differs here yields a different value and fails.
#
# Two frozen forms:
#   hash  `FROZEN_DEPS_HASH`: SHA-256 of the bytewise-sorted
#         `name@version` lines joined by "\n", no trailing newline
#   list  `FROZEN_SPLITTER_DEPS`: those lines themselves, one string
#         per line of a `&[&str]` constant
#
# Usage, from the repository root:
#   ./scripts/dep-stamp-check.sh [lockfile]
set -eu
lock=${1:-Cargo.lock}
fail=0

if command -v sha256sum >/dev/null 2>&1; then
  sha() { sha256sum | cut -d' ' -f1; }
else
  sha() { shasum -a 256 | cut -d' ' -f1; }
fi

# Sorted `name@version` of every locked package whose name matches the
# extended regex $1 in full; a crate locked twice yields two lines.
entries() {
  awk -v want="$1" '
    $0 == "[[package]]" { name = ""; next }
    /^name = "/ { name = $3; gsub(/"/, "", name); next }
    /^version = "/ && name != "" {
      v = $3; gsub(/"/, "", v)
      if (name ~ want) print name "@" v
      name = ""
    }
  ' "$lock" | LC_ALL=C sort
}

# The string literal of `const $2: &str` in file $1.
frozen_hash() {
  awk -v c="const $2: &str" '
    index($0, c) { on = 1 }
    on && match($0, /"[0-9a-f]+"/) { print substr($0, RSTART + 1, RLENGTH - 2); exit }
  ' "$1"
}

# The string literals of `const $2: &[&str] = &[ ... ];` in file $1.
frozen_list() {
  awk -v c="const $2: &[&str]" '
    index($0, c) { on = 1; next }
    on && /^\];/ { exit }
    on && match($0, /"[^"]+"/) { print substr($0, RSTART + 1, RLENGTH - 2) }
  ' "$1" | LC_ALL=C sort
}

report() {
  # $1 label, $2 file, $3 constant, $4 stamp
  echo "STAMP: $1 deps moved in $lock; $2 still holds the old $3"
  echo "  either keep the previous versions in Cargo.lock, or bump $4 and"
  echo "  refresh $3 (its tests/dep_hash.rs prints the new value)"
  echo "  if Cargo.lock did not change, the $1 selection in this script no longer"
  echo "  mirrors its tests/dep_hash.rs"
  fail=1
}

check_hash() {
  label=$1 want=$2 file=$3 const=$4 stamp=$5
  frozen=$(frozen_hash "$file" "$const")
  if [ -z "$frozen" ]; then
    echo "dep-stamp-check: no $const found in $file"
    fail=1
    return
  fi
  locked=$(entries "$want")
  if [ "$(printf '%s' "$locked" | sha)" != "$frozen" ]; then
    report "$label" "$file" "$const" "$stamp"
    echo "$locked" | sed 's/^/  locked now: /'
  fi
}

check_list() {
  label=$1 want=$2 file=$3 const=$4 stamp=$5
  frozen=$(frozen_list "$file" "$const")
  if [ -z "$frozen" ]; then
    echo "dep-stamp-check: no $const found in $file"
    fail=1
    return
  fi
  locked=$(entries "$want")
  if [ "$locked" != "$frozen" ]; then
    report "$label" "$file" "$const" "$stamp"
    FROZEN=$frozen LOCKED=$locked awk 'BEGIN {
      nf = split(ENVIRON["FROZEN"], f, "\n"); nl = split(ENVIRON["LOCKED"], l, "\n")
      for (i = 1; i <= nf; i++) inf[f[i]] = 1
      for (i = 1; i <= nl; i++) inl[l[i]] = 1
      for (i = 1; i <= nf; i++) if (!(f[i] in inl)) print "  no longer locked: " f[i]
      for (i = 1; i <= nl; i++) if (!(l[i] in inf)) print "  newly locked:     " l[i]
    }'
  fi
}

[ -f "$lock" ] || { echo "dep-stamp-check: no lockfile at $lock"; exit 1; }

check_hash extract '^(encoding_rs|pdfium-render|rbook|scraper)$' \
  crates/extract/src/lib.rs FROZEN_DEPS_HASH bookrack_extract::EXTRACTOR_VERSION
check_hash normalize '^unicode-normalization$' \
  crates/normalize/src/lib.rs FROZEN_DEPS_HASH bookrack_normalize::NORMALIZE_VERSION
check_list ingest '^(text-splitter|icu_.*)$' \
  crates/ingest/src/chunk.rs FROZEN_SPLITTER_DEPS bookrack_ingest::CHUNK_VERSION
check_list glean '^(text-splitter|icu_.*)$' \
  crates/glean/src/lib.rs FROZEN_SPLITTER_DEPS bookrack_glean::CHUNK_VERSION

[ "$fail" -eq 0 ] && echo "dep-stamp-check: clean"
exit "$fail"
