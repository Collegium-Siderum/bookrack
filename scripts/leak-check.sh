#!/usr/bin/env sh
# Fail if tracked files leak local or private information.
#
# Rules 1-2, 8, and 9 are generic patterns (they carry no private data)
# and run everywhere, including CI. Rules 3 and 10 read patterns from an
# optional, gitignored denylist file, so private literals never enter the
# repository; they are skipped when that file is absent (e.g. a fresh CI
# checkout).
#
# Rules 4-7 guard the same harm one layer in: the maintainer's own
# library must not reach this repository's *tests* either. An
# integration test that names the binary itself, edits the process
# environment by hand, or sets a bookrack variable on a child is a
# test that reads whatever the machine running it happens to have.
# `crates/test-support` is the one implementation of that isolation,
# and these four rules are what make bypassing it a gate failure
# rather than a habit.
#
# Note on pathspecs: git's wildmatch does not set WM_PATHNAME here, so
# `*` crosses `/` and `crates/*/tests/*` reaches `tests/common/mod.rs`.
set -eu
fail=0

# 1. Local filesystem paths (Windows drive letter / Unix home). The drive
#    letter must sit at a token boundary (line start or a non-letter
#    before it) so an escape sequence like "backtrace:\n" — a letter,
#    colon, backslash mid-word — is not mistaken for a `C:\` path.
if git grep -nE '(^|[^A-Za-z])[A-Za-z]:\\|/Users/|/home/[a-z]' -- \
  '*.rs' '*.toml' '*.md' '*.ts' '*.svelte' '*.json' '*.html' '*.css' '*.js'; then
  echo "LEAK: local filesystem path"
  fail=1
fi

# 2. CJK characters in code / config / docs (test fixtures excluded).
#    Covers the unified ideographs plus CJK punctuation, kana
#    (U+3000-30FF), and the fullwidth/halfwidth forms (U+FF00-FFEF),
#    so a stray corner bracket or fullwidth comma fails the same as a
#    hanzi would.
if git grep -nP '[\x{3000}-\x{30ff}\x{4e00}-\x{9fff}\x{ff00}-\x{ffef}]' -- \
  '*.rs' '*.toml' '*.md' '*.ts' '*.svelte' '*.json' '*.html' '*.css' '*.js' \
  ':!*/tests/fixtures/*'; then
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

# 4. Test binaries must not name the bookrack executable. The only way
#    to reach it is `bookrack_test_support::bookrack_cmd!`, which
#    returns a builder whose environment is already redirected.
if git grep -nE 'CARGO_BIN_EXE_bookrack' -- 'crates/*/tests/*'; then
  echo "LEAK: a test names the bookrack binary directly; use bookrack_cmd!"
  fail=1
fi

# 5. Test binaries must not edit the process environment by hand. The
#    one implementation is `bookrack_test_support::process_env`, which
#    installs a whole sandbox and refuses a second, different spec.
if git grep -nE 'env::(set_var|remove_var)' -- 'crates/*/tests/*'; then
  echo "LEAK: a test mutates the process environment; use process_env"
  fail=1
fi

# 6. Test binaries must not set bookrack variables on a child. Two
#    owners of a child's environment is how isolation drifts: the
#    builder sweeps what it did not set, so anything set behind its
#    back is invisible to it.
if git grep -nE '\.env(_remove)?\(\s*"BOOKRACK_' -- 'crates/*/tests/*'; then
  echo "LEAK: a test sets a BOOKRACK_ variable on a child; use the builder"
  fail=1
fi

# 7. The positive rule: a test file that reads environment-derived
#    configuration must name the isolation crate. Deliberately a
#    heuristic — a new test that reads the environment some other way
#    slips past it — but it turns "someone has to remember" into
#    "someone has to work around", and `scripts/test-clean.sh` catches
#    the absentees in CI. The two are complementary and neither alone
#    is sufficient.
env_readers='Config::(load|resolve)|daemon_state_dir|default_registry_path'
env_readers="$env_readers"'|registry_target_path|DaemonRuntime::start'
for file in $(git grep -lE "$env_readers" -- 'crates/*/tests/*'); do
  if ! grep -q 'bookrack_test_support' "$file"; then
    echo "$file: reads environment configuration without bookrack_test_support"
    fail=1
  fi
done

# 8. Section-mark citations. A section mark in committed text is always
#    a pointer into some other document, and the documents this
#    repository is allowed to name are its own — which are addressed by
#    path and heading, never by section number. So the mark itself is
#    the signal: wherever it appears, the target is a document the
#    reader cannot open. The rule reaches doc comments, error strings,
#    and catalog descriptions alike, because the harm grows with how far
#    the text travels and the outermost of them serialize into an MCP
#    tool schema.
#
#    Two legitimate needs keep an outlet. A parser matching a literal
#    section mark in book text escapes it — `\u{a7}` in Rust, `\u00A7`
#    in a TOML basic string — the same discipline rule 2 leaves for CJK,
#    which keeps the byte out of the source while the program still sees
#    the character. Prose citing a public specification writes the word
#    ("RFC 3986 section 3.2"), which costs nothing and reads better.
#
#    This catches the shape, not the vocabulary: a comment naming a
#    working note without citing a section still passes. The names
#    themselves are private, so they belong in the rule 3 denylist.
if git grep -nP '\x{a7}' -- \
  '*.rs' '*.toml' '*.md' '*.ts' '*.svelte' '*.json' '*.html' '*.css' '*.js' \
  ':!*/tests/fixtures/*'; then
  echo "LEAK: a section mark cites a document by number; name an in-tree"
  echo "      path, write \"section N\" for a public spec, or escape a"
  echo "      literal as \\u{a7} (Rust) / \\u00A7 (TOML)"
  fail=1
fi

# 9. Mount-point path segments. Rule 1 covers the user roots; the label
#    of a volume or mount is the same class of local detail and reaches a
#    log by the same route, but sits outside the prefixes rule 1 knows.
#    `/Volumes/`, `/mnt/`, and `/media/` are allowed only with a segment
#    from the synthetic set, so a real disk name cannot ride in as
#    fixture data. Neither a doc comment writing the shape
#    (`/Volumes/<seg>/`) nor a bare prefix constant matches: in both the
#    next character is not part of a name.
mount_ok='/(Volumes|mnt|media)/(disk|DISK|external|stick)([^A-Za-z0-9._-]|$)'
if git grep -nE '/(Volumes|mnt|media)/[A-Za-z0-9._-]' -- \
  '*.rs' '*.toml' '*.md' '*.ts' '*.svelte' '*.json' '*.html' '*.css' '*.js' \
  '*.sh' '*.yml' | grep -vE "$mount_ok"; then
  echo "LEAK: a mount-point path names a real volume; use a synthetic"
  echo "      segment (disk / external / stick) or write the shape as"
  echo "      \`/Volumes/<seg>/\`"
  fail=1
fi

# 10. The escape hatch, closed. Rules 2 and 8 let a source file carry a
#     CJK character or a section mark as `\u{...}` / `\uXXXX`, which is
#     also the one place a private literal sits in plain sight: the bytes
#     are ASCII, so rule 3 never sees the word they spell. This decodes
#     every escape in a tracked text file and runs the denylist against
#     the decoded copy, so a denylist entry only ever needs the plain
#     form and no one has to enumerate the four ways a string can be
#     escaped. It does not re-apply rules 2 and 8 to the decoded text —
#     the escape is their sanctioned outlet, and only the vocabulary
#     behind it is in question.
#
#     Local-only for the same reason as rule 3, so its perl dependency
#     is never on CI's path.
if [ -f "$denylist" ] && command -v perl >/dev/null 2>&1; then
  escaped=$(git grep -lE '\\u\{?[0-9a-fA-F]' -- \
    '*.rs' '*.toml' '*.md' '*.ts' '*.svelte' '*.json' '*.html' '*.css' '*.js' || true)
  if [ -n "$escaped" ]; then
    hits=$(printf '%s\n' "$escaped" | xargs perl -CSD -ne '
        $raw = $_;
        s/\\{1,2}u\{([0-9a-fA-F]{1,6})\}/chr(hex($1))/ge;
        s/\\{1,2}u([0-9a-fA-F]{4})/chr(hex($1))/ge;
        print "$ARGV:$.:$_" if $_ ne $raw;
        close ARGV if eof;
      ' | grep -F -f "$denylist" || true)
    if [ -n "$hits" ]; then
      printf '%s\n' "$hits"
      echo "LEAK: a denylisted identifier hidden behind unicode escapes"
      fail=1
    fi
  fi
fi

if [ "$fail" -eq 0 ]; then
  echo "leak-check: clean"
fi
exit "$fail"
