#!/usr/bin/env sh
# Fail if an outbound error boundary stringifies an error with
# `to_string()` instead of flattening it with `bookrack_core::error_chain`.
#
# `Display` on a wrapper variant prints only its own text ("query
# error"), so `to_string()` at a process boundary drops the root cause
# exactly where the caller can no longer reach it.
#
# The rule is scoped to a file list rather than to a call shape. Two
# reasons:
#
#   1. `git grep -nE` matches within a single line, and rustfmt's
#      `max_width = 100` wraps the long `RpcError::new(..., e.to_string())`
#      forms across lines — a call-shape pattern silently stops matching
#      as soon as the call grows.
#   2. Every file below is a pure boundary-mapping layer, so a bare
#      `to_string()` in one is wrong regardless of the call it sits in.
#      Banning the token outright is both stricter and unforgeable.
#
# `crates/mcp/src/lib.rs` is deliberately absent: it is the whole MCP
# service implementation (version strings, DTO assembly, test
# literals), and pinning it by path would need a long allowlist. Its
# outbound mapping lives in `crates/mcp/src/error_map.rs`, which is on
# the list.
#
# Static-message helpers in these files (e.g. `plan_lookup_err`) hold no
# `to_string()` and are unaffected.
#
# Adding a new outbound error-boundary file means adding it here.
#
# Escape hatch: append `// error-boundary-check: allow` to a line that
# must keep `to_string()` (test fixtures pinning the unflattened text).
set -eu
fail=0

boundary_files="
crates/runtime/src/control/error_map.rs
crates/runtime/src/control/methods/reads_library.rs
crates/mcp/src/error_map.rs
"

for f in $boundary_files; do
  if [ ! -f "$f" ]; then
    echo "error-boundary-check: listed file is missing: $f"
    fail=1
    continue
  fi
  if grep -n '\.to_string()' "$f" | grep -v 'error-boundary-check: allow'; then
    echo "BOUNDARY: bare to_string() in $f -- use bookrack_core::error_chain"
    fail=1
  fi
done

if [ "$fail" -eq 0 ]; then
  echo "error-boundary-check: clean"
fi
exit "$fail"
