#!/usr/bin/env sh
# Fail if the workspace's structure drifts from its tier table, or from
# the exceptions registered in structure.toml.
#
# The rules live in this script; the exceptions live in the registry.
# Changing a rule is an edit here, and an exception the code no longer
# needs is itself a failure, so the registry only shrinks.
#
# Checks:
#   1  tier direction. Every normal dependency edge between workspace
#      crates must run the way the tier table below allows. A crate the
#      table does not name sits in the front tier. Exceptions: [[edge]].
#   2  file ratchet. A production file of the host or of a front end
#      that names a crate its tier may not depend on, by the path prefix
#      `bookrack_<crate>::`, must be listed. Exceptions: [[file]].
#   3  text libraries. Only the text crate depends directly on a
#      segmentation or normalization library. Exceptions: [[text_dep]].
#
# Production lines are what checks read from Rust sources: tracked files
# under `crates/*/src`, minus every item under a column-0 `#[cfg(test)]`
# or `#[cfg(all(test, ...))]`, minus comment-only lines. The filter
# relies on rustfmt closing a top-level item at column 0. An indented
# `#[cfg(test)]` is not removed and counts as production.
#
# The registry is a rigid subset of TOML: a `[[table]]` header on its
# own line, one `key = "value"` per line, and an array opened by
# `key = [`, holding one quoted element per line, closed by `]` alone.
# Every entry carries a `reason` from a closed set, and a transitional
# one carries the `exit` condition that ends it.
#
# Reads tracked files only: `git add` a new file before running.
#
# Usage, from the repository root:
#   ./scripts/structure-check.sh [registry]
set -eu
reg=${1:-structure.toml}

# Tier of each workspace crate, by directory name under crates/.
TIERS='
base   core config obs session audit-profile index-profile cli-grammar control-client test-support normalize
pure   extract distill metadata
store  catalog corpus vectors refs dbkit
client embed rerank
inner  ingest glean query search diagnose
center ops
host   runtime
front  cli mcp
'
# Tiers each tier may depend on.
ALLOWED='
base   base
pure   base pure
store  base store
client base
inner  base pure store client inner
center base pure store client inner center
host   base client center
front  base center host front
'
# The crate that owns text segmentation and normalization, and the
# libraries only it may depend on directly, as an anchored regex.
TEXT_CRATE=normalize
TEXT_LIBS='^(text-splitter|unicode-normalization|unicode-segmentation|icu_.*)$'
REASONS='dependency-direction stamp-isolation compile-boundary wire-pin external-constraint transitional'
export TIERS ALLOWED

for tool in cargo jq git awk; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "structure-check: $tool is required and was not found"
    exit 1
  }
done
[ -f "$reg" ] || { echo "structure-check: no registry at $reg"; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0

# --- registry -------------------------------------------------------

# One row per value: table, entry number within the table, line of the
# entry header, key, value. An entry header alone yields a row with an
# empty key, so an entry without fields is still seen.
awk -v reg="$reg" '
  function bad() { print "structure-check: cannot read " reg " line " NR > "/dev/stderr"; failed = 1; exit 1 }
  function row(k, v) { printf "%s\t%d\t%d\t%s\t%s\n", table, n[table], head, k, v }
  /^[ \t]*(#.*)?$/ { next }
  in_array {
    if ($0 ~ /^\][ \t]*$/) { in_array = 0; next }
    if ($0 !~ /^[ \t]+"[^"\\]*",[ \t]*$/) bad()
    v = $0; sub(/^[ \t]+"/, "", v); sub(/",[ \t]*$/, "", v)
    row(key, v); next
  }
  /^\[\[[a-z_]+\]\]$/ {
    table = substr($0, 3, length($0) - 4); n[table]++; head = NR
    row("", ""); next
  }
  table == "" { bad() }
  /^[a-z_]+ = \[$/ { key = $1; in_array = 1; next }
  /^[a-z_]+ = "[^"\\]*"$/ {
    key = $1; v = $0; sub(/^[a-z_]+ = "/, "", v); sub(/"$/, "", v)
    row(key, v); next
  }
  { bad() }
  END {
    if (failed) exit 1
    if (in_array) { print "structure-check: " reg " ends inside an array" > "/dev/stderr"; exit 1 }
  }
' "$reg" > "$tmp/reg.tsv" || exit 1

# Shape: known tables, known keys, required keys, the closed reason
# set, and no value registered twice.
awk -F '\t' -v reg="$reg" -v reasons="$REASONS" '
  BEGIN {
    keys["edge"] = "from to reason exit"; need["edge"] = "from to reason"
    keys["file"] = "paths reason exit"; need["file"] = "paths reason"
    keys["text_dep"] = "from to reason exit"; need["text_dep"] = "from to reason"
    split(reasons, r, " "); for (i in r) known_reason[r[i]] = 1
  }
  function say(at, msg) { print reg ": [[" t "]] entry " e " (line " at "): " msg; bad = 1 }
  function listed(word, list,    a, i, m) {
    m = split(list, a, " "); for (i = 1; i <= m; i++) if (a[i] == word) return 1
    return 0
  }
  {
    t = $1; e = $2; id = t SUBSEP e
    if (!(t in keys)) { if ($4 == "") say($3, "unknown table"); next }
    if ($4 == "") { order[++count] = id; line[id] = $3; tab[id] = t; ent[id] = e; next }
    if (!listed($4, keys[t])) { say($3, "unknown key \"" $4 "\""); next }
    if ($5 == "") say($3, "key \"" $4 "\" is empty")
    if ($4 == "paths") {
      if ($5 in seen_path) say($3, "\"" $5 "\" is registered twice")
      seen_path[$5] = 1
    } else if ((id SUBSEP $4) in has) {
      say($3, "key \"" $4 "\" given twice")
    }
    val[id SUBSEP $4] = $5; has[id SUBSEP $4] = 1
    if ($4 == "reason" && !($5 in known_reason)) say($3, "reason \"" $5 "\" is not in the closed set")
  }
  END {
    for (c = 1; c <= count; c++) {
      id = order[c]; t = tab[id]; e = ent[id]; at = line[id]
      m = split(need[t], a, " ")
      for (i = 1; i <= m; i++) if (!((id SUBSEP a[i]) in has)) say(at, "missing \"" a[i] "\"")
      if (val[id SUBSEP "reason"] == "transitional" && !((id SUBSEP "exit") in has))
        say(at, "a transitional entry needs an \"exit\"")
      if (t != "file" && (id SUBSEP "from") in has && (id SUBSEP "to") in has) {
        pair = t SUBSEP val[id SUBSEP "from"] SUBSEP val[id SUBSEP "to"]
        if (pair in seen_pair) say(at, "\"" val[id SUBSEP "from"] " -> " val[id SUBSEP "to"] "\" is registered twice")
        seen_pair[pair] = 1
      }
    }
    exit bad
  }
' "$tmp/reg.tsv" || fail=1

# `from<TAB>to` of every entry of table $1.
pairs() {
  awk -F '\t' -v want="$1" '
    $1 != want { next }
    $4 == "from" { from[$2] = $5 }
    $4 == "to" { to[$2] = $5 }
    END { for (e in from) if (e in to) print from[e] "\t" to[e] }
  ' "$tmp/reg.tsv"
}

# --- dependency graph -----------------------------------------------

# `crate<TAB>dependency` for every normal dependency a workspace crate
# declares, the crate by its directory name under crates/, a workspace
# dependency likewise, anything else by its package name.
cargo metadata --no-deps --format-version 1 --offline > "$tmp/meta.json" || {
  echo "structure-check: cargo metadata failed"
  exit 1
}
jq -r '
  (.packages | map({key: .name, value: (.manifest_path | split("/") | .[-2])}) | from_entries) as $dir
  | .packages[]
  | $dir[.name] as $from
  | (["-", $from, $from]),
    (.dependencies[] | select(.kind == null) | [(if $dir[.name] then "w" else "x" end), $from, ($dir[.name] // .name)])
  | @tsv
' "$tmp/meta.json" > "$tmp/deps.tsv"
[ -s "$tmp/deps.tsv" ] || { echo "structure-check: cargo metadata listed no workspace crate"; exit 1; }

# Check 1.
pairs edge > "$tmp/edge.waived"
awk -F '\t' -v waived="$tmp/edge.waived" '
  function tier(c) { return (c in tier_of) ? tier_of[c] : "front" }
  BEGIN {
    m = split(ENVIRON["TIERS"], rows, "\n")
    for (i = 1; i <= m; i++) { k = split(rows[i], w, " "); for (j = 2; j <= k; j++) tier_of[w[j]] = w[1] }
    m = split(ENVIRON["ALLOWED"], rows, "\n")
    for (i = 1; i <= m; i++) { k = split(rows[i], w, " "); for (j = 2; j <= k; j++) ok[w[1], w[j]] = 1 }
    while ((getline line < waived) > 0) { split(line, w, "\t"); waiver[w[1], w[2]] = 1; text[w[1], w[2]] = w[1] " -> " w[2] }
  }
  $1 == "-" { member[$2] = 1; next }
  $1 == "w" {
    edge[$2, $3] = 1
    if ((tier($2), tier($3)) in ok) next
    forbidden[$2, $3] = 1
    if (!(($2, $3) in waiver)) {
      print "STRUCTURE: " $2 " (" tier($2) ") -> " $3 " (" tier($3) ") runs against the tier table"
      bad = 1
    }
  }
  END {
    for (p in waiver) {
      if (!(p in edge)) why = "the dependency is gone"
      else if (!(p in forbidden)) why = "the tier table allows it"
      else continue
      print "STRUCTURE: stale waiver [[edge]] " text[p] ": " why " -- delete it"
      bad = 1
    }
    for (c in tier_of) if (!(c in member)) {
      print "STRUCTURE: the tier table names \"" c "\", which is not a workspace crate"
      bad = 1
    }
    exit bad
  }
' "$tmp/deps.tsv" || fail=1

# Check 3.
pairs text_dep > "$tmp/text.waived"
awk -F '\t' -v owner="$TEXT_CRATE" -v libs="$TEXT_LIBS" -v waived="$tmp/text.waived" '
  BEGIN {
    while ((getline line < waived) > 0) { split(line, w, "\t"); waiver[w[1], w[2]] = 1; text[w[1], w[2]] = w[1] " -> " w[2] }
  }
  $1 == "x" && $3 ~ libs {
    edge[$2, $3] = 1
    if ($2 == owner) next
    if (!(($2, $3) in waiver)) {
      print "STRUCTURE: " $2 " depends on " $3 " directly; only " owner " may"
      bad = 1
    }
  }
  END {
    for (p in waiver) {
      split(p, w, SUBSEP)
      if (p in edge && w[1] != owner) continue
      print "STRUCTURE: stale waiver [[text_dep]] " text[p] " -- delete it"
      bad = 1
    }
    exit bad
  }
' "$tmp/deps.tsv" || fail=1

# --- production lines -----------------------------------------------

# `file:line:text` for the production lines of the files named on the
# command line.
production() {
  awk '
    FNR == 1 { pending = 0; until = "" }
    until == "semi" { if ($0 ~ /;$/) until = ""; next }
    until == "close" { if ($0 ~ /^\}/ || $0 == ");") until = ""; next }
    /^#\[cfg\(test\)\]/ || /^#\[cfg\(all\(test,/ { pending = 1; next }
    pending {
      if ($0 ~ /^#\[/ || $0 ~ /^[ \t]*\/\//) next
      pending = 0
      head = $0
      sub(/^pub(\([^)]*\))? /, "", head)
      split(head, word, /[ (<{;:]/)
      if (word[1] == "use" || word[1] == "static" || word[1] == "type" || (word[1] == "const" && word[2] != "fn")) {
        if ($0 !~ /;$/) until = "semi"
      } else if ($0 !~ /;$/ && $0 !~ /\{.*\}$/) {
        until = "close"
      }
      next
    }
    /^[ \t]*\/\// { next }
    { print FILENAME ":" FNR ":" $0 }
  ' "$@"
}

# Check 2. The crates a tier may not name follow from the tier table.
git ls-files 'crates/*/src/*.rs' > "$tmp/sources"
awk -F '\t' -v want=paths '$1 == "file" && $4 == want { print $5 }' "$tmp/reg.tsv" > "$tmp/file.waived"
: > "$tmp/file.hits"
for crate in $(echo "$TIERS" | awk '$1 == "host" || $1 == "front" { for (i = 2; i <= NF; i++) print $i }'); do
  below=$(echo "$TIERS" | awk -v crate="$crate" '
    NF { for (i = 2; i <= NF; i++) { tier_of[$i] = $1; name[++count] = $i } }
    END {
      m = split(ENVIRON["ALLOWED"], rows, "\n")
      for (i = 1; i <= m; i++) { k = split(rows[i], w, " "); for (j = 2; j <= k; j++) ok[w[1], w[j]] = 1 }
      for (i = 1; i <= count; i++) if (!((tier_of[crate], tier_of[name[i]]) in ok)) {
        ident = name[i]; gsub(/-/, "_", ident); out = out (out == "" ? "" : "|") ident
      }
      print out
    }')
  [ -n "$below" ] || continue
  grep "^crates/$crate/src/" "$tmp/sources" > "$tmp/crate.sources" || continue
  # shellcheck disable=SC2046
  production $(cat "$tmp/crate.sources") |
    awk -v pattern="(^|[^A-Za-z0-9_])bookrack_($below)::" '
      { file = $0; sub(/:.*/, "", file); body = $0; sub(/^[^:]*:[^:]*:/, "", body) }
      body ~ pattern { hit[file] = 1 }
      END { for (f in hit) print f }
    ' >> "$tmp/file.hits"
done
LC_ALL=C sort -o "$tmp/file.hits" "$tmp/file.hits"
awk -v waived="$tmp/file.waived" '
  BEGIN { while ((getline line < waived) > 0) waiver[line] = 1 }
  { hit[$0] = 1 }
  !($0 in waiver) {
    print "STRUCTURE: " $0 " names a crate below its tier and is not registered in [[file]]"
    bad = 1
  }
  END {
    for (f in waiver) if (!(f in hit)) {
      print "STRUCTURE: stale waiver [[file]] " f ": it names no crate below its tier, or is gone -- delete it"
      bad = 1
    }
    exit bad
  }
' "$tmp/file.hits" || fail=1

[ "$fail" -eq 0 ] && echo "structure-check: clean"
exit "$fail"
