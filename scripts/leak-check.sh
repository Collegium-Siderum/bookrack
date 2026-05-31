#!/usr/bin/env sh
# Fail if tracked files leak local or private information.
#
# Rules 1-2 are generic patterns (they carry no private data) and run
# everywhere, including CI. Rule 3 reads patterns from an optional,
# gitignored denylist file, so private literals never enter the
# repository; it is skipped when that file is absent (e.g. a fresh CI
# checkout).
set -eu
fail=0

# 1. Local filesystem paths (Windows drive letter / Unix home). The drive
#    letter must sit at a token boundary (line start or a non-letter
#    before it) so an escape sequence like "backtrace:\n" — a letter,
#    colon, backslash mid-word — is not mistaken for a `C:\` path.
if git grep -nE '(^|[^A-Za-z])[A-Za-z]:\\|/Users/|/home/[a-z]' -- '*.rs' '*.toml' '*.md'; then
  echo "LEAK: local filesystem path"
  fail=1
fi

# 2. CJK characters in code / config / docs (test fixtures excluded).
if git grep -nP '[\x{4e00}-\x{9fff}]' -- '*.rs' '*.toml' '*.md' ':!*/tests/fixtures/*'; then
  echo "LEAK: CJK in code/config/docs"
  fail=1
fi

# 3. Private identifiers, matched against a gitignored denylist (one
#    pattern per line). Keeping the patterns out of tree means no
#    private literal is ever committed.
denylist="scripts/leak-denylist.txt"
if [ -f "$denylist" ]; then
  if git grep -nF -f "$denylist" -- '*' ":!$denylist"; then
    echo "LEAK: denylisted private identifier"
    fail=1
  fi
else
  echo "note: $denylist absent, rule 3 skipped"
fi

if [ "$fail" -eq 0 ]; then
  echo "leak-check: clean"
fi
exit "$fail"
