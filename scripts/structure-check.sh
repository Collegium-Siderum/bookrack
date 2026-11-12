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
#   4  code-point predicates. Outside the text crate, a production file
#      that writes a CJK code point as a character literal `'\u{H}'` or
#      as a hexadecimal integer `0xH` must be listed. H counts when it
#      falls in U+2E80-DFFF, U+F900-FAFF, U+FF00-FFEF, or
#      U+20000-31FFF; `_` separators are ignored. A code point inside a
#      double-quoted string does not count: word lists and patterns are
#      data, a predicate is written as a character or an integer.
#      Exceptions: [[codepoint]].
#   6  paired names. Two production functions of one crate, or two
#      source files of one directory, whose names differ only by kind
#      must be covered by one [[parallel]] entry, or be on the list of
#      legitimate pairs below. Names are split at `_`; the kind words
#      are book, books, paper, papers. Three rules pair a name with
#      another that exists:
#        swap   every kind word replaced by one of the other kind, in
#               either number;
#        strip  one kind word removed, leaving at least two words (a
#               file name has no such floor);
#        verb   the words ingest and glean exchanged, kind words kept or
#               swapped.
#      A site written as a file covers every function in it. The check
#      reads names only: a copy with an asymmetric name is registered
#      by hand.
#
# A [[parallel]] entry names every site of one method implemented more
# than once. Each site's file carries the plain comment line
# `// Parallel implementation: <id>.` (`# ...` in a script), and every
# such line in the repository names an entry that lists its file. A
# marker written as a doc comment fails: it would reach rustdoc, tool
# schemas, and help text.
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
# Crates whose functions are paired as if they were one crate.
ONE_GROUP='ingest glean'
# Crates the paired-name check does not read.
UNPAIRED='test-support'
# Pairs that differ only by kind and are not parallel implementations:
# accessors that return one kind's path or build one kind's id, and a
# stage only one kind has. `fn <crate> <name> <name>`, or
# `file <path> <path>`.
LEGITIMATE='
fn config books_dir papers_dir
fn config catalog_db papers_catalog_db
fn config corpus_db papers_corpus_db
fn config lancedb_dir papers_lancedb_dir
fn ops books_dir papers_dir
fn ops catalog_db papers_catalog_db
fn ops corpus_db papers_corpus_db
fn ops lancedb_dir papers_lancedb_dir
fn core book paper
fn cli-grammar book_intake_id paper_intake_id
file crates/extract/src/pdf.rs crates/extract/src/pdf_paper.rs
'
REASONS='dependency-direction stamp-isolation compile-boundary wire-pin external-constraint transitional'
export TIERS ALLOWED ONE_GROUP UNPAIRED LEGITIMATE

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
    row(key "[]", v); next
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
    keys["file"] = "paths[] reason exit"; need["file"] = "paths[] reason"
    keys["text_dep"] = "from to reason exit"; need["text_dep"] = "from to reason"
    keys["codepoint"] = "paths[] reason exit"; need["codepoint"] = "paths[] reason"
    keys["parallel"] = "id summary sites[] reason detail agreement pin exit"
    need["parallel"] = "id summary sites[] reason agreement"
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
    if ($4 ~ /\[\]$/) {
      if ((id, $5) in seen_item) say($3, "\"" $5 "\" is listed twice")
      seen_item[id, $5] = 1
      if ($4 == "paths[]") {
        if ((t, $5) in seen_path) say($3, "\"" $5 "\" is registered twice")
        seen_path[t, $5] = 1
      }
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
      if (t == "parallel") {
        name = val[id SUBSEP "id"]; agreement = val[id SUBSEP "agreement"]
        if ((id SUBSEP "id") in has) {
          if (name !~ /^[a-z0-9-]+$/) say(at, "id \"" name "\" must be lowercase letters, digits, and hyphens")
          if (name in seen_id) say(at, "id \"" name "\" is registered twice")
          seen_id[name] = 1
        }
        if (agreement == "pinned") {
          if (!((id SUBSEP "pin") in has)) say(at, "a pinned entry needs a \"pin\"")
        } else if (agreement == "divergent") {
          if (!((id SUBSEP "detail") in has)) say(at, "a divergent entry needs a \"detail\"")
        } else if (agreement == "unverified") {
          if (val[id SUBSEP "reason"] != "transitional") say(at, "only a transitional entry may be unverified")
        } else if ((id SUBSEP "agreement") in has) {
          say(at, "agreement \"" agreement "\" is not pinned, divergent, or unverified")
        }
      }
      if ((id SUBSEP "from") in has && (id SUBSEP "to") in has) {
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

# The `paths` of every entry of table $1.
paths() {
  awk -F '\t' -v want="$1" '$1 == want && $4 == "paths[]" { print $5 }' "$tmp/reg.tsv"
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
paths file > "$tmp/file.waived"
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

# Check 4.
paths codepoint > "$tmp/codepoint.waived"
grep -v "^crates/$TEXT_CRATE/" "$tmp/sources" > "$tmp/outside.sources" || true
# shellcheck disable=SC2046
production $(cat "$tmp/outside.sources") |
  awk '
    function hex(digits,    i, v) {
      v = 0
      for (i = 1; i <= length(digits); i++) v = v * 16 + index("0123456789abcdef", tolower(substr(digits, i, 1))) - 1
      return v
    }
    function cjk(v) {
      return (v >= 11904 && v <= 57343) || (v >= 63744 && v <= 64255) ||
        (v >= 65280 && v <= 65519) || (v >= 131072 && v <= 204799)
    }
    {
      file = $0; sub(/:.*/, "", file)
      body = $0; sub(/^[^:]*:[^:]*:/, "", body)
      # A quote written as a character or byte literal would otherwise
      # pair up with another one and hide what stands between them.
      gsub(/b?\047\\?"\047/, " ", body)
      gsub(/r#+"([^"]|"[^#])*"#+/, " ", body)
      gsub(/"([^"\\]|\\.)*"/, " ", body)
      while (match(body, /\047\\u\{[0-9A-Fa-f_]+\}\047|0x[0-9A-Fa-f_]+/)) {
        literal = substr(body, RSTART, RLENGTH)
        body = substr(body, RSTART + RLENGTH)
        gsub(/^\047\\u\{|^0x|\}\047$|_/, "", literal)
        if (length(literal) <= 6 && cjk(hex(literal))) { hit[file] = 1; break }
      }
    }
    END { for (f in hit) print f }
  ' | LC_ALL=C sort > "$tmp/codepoint.hits"
awk -v waived="$tmp/codepoint.waived" -v owner="$TEXT_CRATE" '
  BEGIN { while ((getline line < waived) > 0) waiver[line] = 1 }
  { hit[$0] = 1 }
  !($0 in waiver) {
    print "STRUCTURE: " $0 " writes a CJK code point as a predicate outside " owner " and is not registered in [[codepoint]]"
    bad = 1
  }
  END {
    for (f in waiver) if (!(f in hit)) {
      print "STRUCTURE: stale waiver [[codepoint]] " f ": it holds no code-point predicate, or is gone -- delete it"
      bad = 1
    }
    exit bad
  }
' "$tmp/codepoint.hits" || fail=1

# --- parallel implementations ----------------------------------------

# `id<TAB>site` and `id<TAB>pin` of the [[parallel]] table.
awk -F '\t' '
  $1 != "parallel" { next }
  $4 == "id" { id[$2] = $5 }
  $4 == "sites[]" { count[$2]++; site[$2, count[$2]] = $5 }
  $4 == "pin" { pin[$2] = $5 }
  END {
    for (e in id) {
      for (i = 1; i <= count[e]; i++) print id[e] "\t" site[e, i] > sites
      if (e in pin) print id[e] "\t" pin[e] > pins
    }
  }
' sites="$tmp/parallel.sites" pins="$tmp/parallel.pins" "$tmp/reg.tsv"
touch "$tmp/parallel.sites" "$tmp/parallel.pins"
git ls-files > "$tmp/tracked"

# Sites and pins exist, and every site's file carries the marker.
while IFS="$(printf '\t')" read -r id site; do
  file=${site%%::*}
  if ! grep -qxF "$file" "$tmp/tracked"; then
    echo "STRUCTURE: parallel \"$id\" names a missing site: $site"
    fail=1
    continue
  fi
  if [ "$site" != "$file" ]; then
    item=${site#*::}
    grep -Eq "(^|[^A-Za-z0-9_])(fn|struct|enum|const|static|type) $item([^A-Za-z0-9_]|\$)" "$file" || {
      echo "STRUCTURE: parallel \"$id\" names a missing site: $site"
      fail=1
    }
  fi
  grep -Eq "^[[:space:]]*(//|#) Parallel implementation: $id\\.\$" "$file" || {
    echo "STRUCTURE: parallel \"$id\": site $file carries no marker"
    fail=1
  }
done < "$tmp/parallel.sites"
while IFS="$(printf '\t')" read -r id pin; do
  file=${pin%%::*}
  if ! grep -qxF "$file" "$tmp/tracked"; then
    echo "STRUCTURE: parallel \"$id\": pin is missing: $pin"
    fail=1
  elif [ "$pin" != "$file" ]; then
    grep -Eq "(^|[^A-Za-z0-9_])fn ${pin#*::}([^A-Za-z0-9_]|\$)" "$file" || {
      echo "STRUCTURE: parallel \"$id\": pin is missing: $pin"
      fail=1
    }
  fi
done < "$tmp/parallel.pins"

# Every marker in the repository names an entry that lists its file.
git grep -nE '^[[:space:]]*(//|#) Parallel implementation: [a-z0-9-]+\.$' -- . ':!scripts/structure-check.sh' |
  awk -v sites="$tmp/parallel.sites" '
    BEGIN {
      while ((getline line < sites) > 0) {
        split(line, w, "\t"); known[w[1]] = 1
        file = w[2]; sub(/::.*/, "", file); at[w[1], file] = 1
      }
    }
    {
      file = $0; sub(/:.*/, "", file)
      id = $0; sub(/^.*Parallel implementation: /, "", id); sub(/\.$/, "", id)
      if (!(id in known)) { print "STRUCTURE: marker in " file " names an unknown id \"" id "\""; bad = 1 }
      else if (!((id, file) in at)) { print "STRUCTURE: marker in " file " is not a site of \"" id "\""; bad = 1 }
    }
    END { exit bad }
  ' || fail=1
if git grep -nE '^[[:space:]]*//[/!] Parallel implementation: ' -- crates > "$tmp/doc.markers"; then
  sed 's/^\([^:]*:[^:]*\):.*/STRUCTURE: \1: a marker must be a plain comment, not a doc comment/' "$tmp/doc.markers"
  fail=1
fi

# Check 6.
grep -v "^crates/\\($(echo "$UNPAIRED" | sed 's/ /\\|/g')\\)/" "$tmp/sources" > "$tmp/paired.sources" || true
# shellcheck disable=SC2046
production $(cat "$tmp/paired.sources") |
  awk -v sites="$tmp/parallel.sites" -v sources="$tmp/paired.sources" '
    function crate_of(file,    c) { c = file; sub(/^crates\//, "", c); sub(/\/.*/, "", c); return c }
    function group(file,    c) { c = crate_of(file); return (c in merged) ? "+" : c }
    # Every name the rules derive from the words w[1..n], into out[].
    function expand(i, prefix, flipped, verbs, rule,    x, k, opts, m) {
      if (i > n) {
        if (rule == "swap" && flipped) out[prefix] = 1
        if (rule == "verb" && verbs) out[prefix] = 1
        return
      }
      x = w[i]
      if (x in other) {
        m = split(other[x], opts, " ")
        for (k = 1; k <= m; k++) expand(i + 1, prefix (i > 1 ? "_" : "") opts[k], 1, verbs, rule)
        if (rule == "verb") expand(i + 1, prefix (i > 1 ? "_" : "") x, flipped, verbs, rule)
      } else if (rule == "verb" && x in verb) {
        expand(i + 1, prefix (i > 1 ? "_" : "") verb[x], flipped, 1, rule)
      } else {
        expand(i + 1, prefix (i > 1 ? "_" : "") x, flipped, verbs, rule)
      }
    }
    function partners(name, floor,    i, j, s, kinds) {
      for (s in out) delete out[s]
      n = split(name, w, "_")
      kinds = 0
      for (i = 1; i <= n; i++) if (w[i] in other) kinds++
      if (kinds) expand(1, "", 0, 0, "swap")
      expand(1, "", 0, 0, "verb")
      if (n - 1 >= floor) for (i = 1; i <= n; i++) if (w[i] in other) {
        s = ""
        for (j = 1; j <= n; j++) if (j != i) s = s (s == "" ? "" : "_") w[j]
        if (s != "") out[s] = 1
      }
      delete out[name]
    }
    function covered(fa, a, fb, b,    id) {
      for (id in ids) if (((id, fa) in site || (id, fa "::" a) in site) && ((id, fb) in site || (id, fb "::" b) in site)) return 1
      return 0
    }
    BEGIN {
      other["book"] = "paper papers"; other["books"] = "papers paper"
      other["paper"] = "book books"; other["papers"] = "books book"
      verb["ingest"] = "glean"; verb["glean"] = "ingest"
      m = split(ENVIRON["ONE_GROUP"], f, " "); for (i = 1; i <= m; i++) merged[f[i]] = 1
      m = split(ENVIRON["LEGITIMATE"], rows, "\n")
      for (i = 1; i <= m; i++) {
        k = split(rows[i], f, " ")
        if (k == 4) { fine[f[2], f[3], f[4]] = 1; fine[f[2], f[4], f[3]] = 1 }
        if (k == 3) { fine_file[f[2], f[3]] = 1; fine_file[f[3], f[2]] = 1 }
      }
      while ((getline line < sites) > 0) { split(line, f, "\t"); ids[f[1]] = 1; site[f[1], f[2]] = 1 }
      while ((getline line < sources) > 0) {
        dir = line; sub(/\/[^\/]*$/, "", dir)
        base = line; sub(/^.*\//, "", base); sub(/\.rs$/, "", base)
        in_dir[dir, base] = line; dirs[dir] = dirs[dir] " " base
      }
    }
    {
      file = $0; sub(/:.*/, "", file)
      body = $0; sub(/^[^:]*:[^:]*:/, "", body)
      if (match(body, /^ *(pub(\([^)]*\))? )?(default )?(const )?(async )?(unsafe )?(extern "[^"]*" )?fn [A-Za-z0-9_]+/)) {
        name = substr(body, RSTART, RLENGTH); sub(/^.*fn /, "", name)
        g = group(file)
        if (!((g, name, file) in defined)) { defined[g, name, file] = 1; files[g, name] = files[g, name] " " file; names[g] = names[g] " " name }
      }
    }
    END {
      for (g in names) {
        m = split(names[g], list, " ")
        for (i = 1; i <= m; i++) {
          a = list[i]
          if ((g, a) in done) continue
          done[g, a] = 1
          partners(a, 2)
          for (b in out) {
            if (!((g, b) in files) || (g, b, a) in judged) continue
            judged[g, a, b] = 1
            ka = split(files[g, a], fa, " "); kb = split(files[g, b], fb, " ")
            for (x = 1; x <= ka; x++) for (y = 1; y <= kb; y++) {
              if (crate_of(fa[x]) == crate_of(fb[y]) && (crate_of(fa[x]), a, b) in fine) continue
              if (covered(fa[x], a, fb[y], b)) continue
              print "STRUCTURE: " fa[x] "::" a " and " fb[y] "::" b " differ only by kind; no [[parallel]] entry covers both"
              bad = 1
            }
          }
        }
      }
      for (d in dirs) {
        m = split(dirs[d], list, " ")
        for (i = 1; i <= m; i++) {
          a = list[i]
          partners(a, 1)
          for (b in out) {
            if (!((d, b) in in_dir) || (d, b, a) in judged) continue
            judged[d, a, b] = 1
            fa1 = in_dir[d, a]; fb1 = in_dir[d, b]
            if ((fa1, fb1) in fine_file) continue
            ok = 0
            for (id in ids) if ((id, fa1) in site && (id, fb1) in site) ok = 1
            if (ok) continue
            print "STRUCTURE: " fa1 " and " fb1 " differ only by kind; no [[parallel]] entry lists both files"
            bad = 1
          }
        }
      }
      exit bad
    }
  ' || fail=1

[ "$fail" -eq 0 ] && echo "structure-check: clean"
exit "$fail"
