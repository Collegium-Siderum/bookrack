#!/usr/bin/env sh
# Run the workspace test suite against a host that carries nothing.
#
# The ordinary `cargo nextest run --workspace` inherits this machine:
# its home directory, its registry, its exported bookrack variables,
# and the repository's own `.env`. That is the right loop for
# development and the wrong one for a contract — a suite that passes
# here and fails on a fresh runner has been reading the machine, and
# nobody finds out until the runner says so.
#
# This script is the contract. It starts from an empty environment and
# lets through exactly the variables named below, each for a reason
# written next to it. Everything else — every `BOOKRACK_*` in
# particular — is gone, so a test that needs one has to say so itself.
#
# It is deliberately not a per-commit gate: it runs the whole workspace
# and costs what `cargo nextest run --workspace` costs, while the
# per-commit scripts are all sub-second. Its home is CI, plus a
# contributor running it by hand before a push.
#
# Arguments are passed through to nextest, so
# `./scripts/test-clean.sh -p bookrack-cli` narrows the run while
# keeping the scrubbed environment. With no arguments the whole
# workspace runs.
set -eu

# `--workspace` only when nothing else was asked for: appending it
# unconditionally would silently widen every narrowed invocation back
# to the full run.
if [ "$#" -eq 0 ]; then
  set -- --workspace
fi

# Captured before the scrub, so the cargo and rustup homes below can be
# derived from it without `env -i` having already taken it away.
REAL_HOME="$HOME"

FAKE=$(mktemp -d)
trap 'rm -rf "$FAKE"' EXIT
mkdir -p "$FAKE/.config" "$FAKE/.local/share" "$FAKE/.cache"

# The PDF tests find PDFium where the product does: in the managed
# directory under the home directory, which `bookrack setup pdfium`
# populates. The scrubbed home starts empty, so the builds installed in
# the real one are mirrored into it: a directory made for every
# directory, a link for every file. No directory is ever linked — a
# test that writes into the managed tree under this script then writes
# into the scrubbed home, never into the real one. A host with nothing
# installed mirrors nothing, and the PDF tests skip or, under `CI`,
# fail.
case "$(uname -s)" in
  Darwin)
    SLOT_SRC="$REAL_HOME/Library/Application Support/bookrack/pdfium"
    SLOT_DST="$FAKE/Library/Application Support/bookrack/pdfium"
    ;;
  *)
    SLOT_SRC="${XDG_DATA_HOME:-$REAL_HOME/.local/share}/bookrack/pdfium"
    SLOT_DST="$FAKE/.local/share/bookrack/pdfium"
    ;;
esac
if [ -d "$SLOT_SRC" ]; then
  (cd "$SLOT_SRC" && find -L . -type d) | while IFS= read -r dir; do
    mkdir -p "$SLOT_DST/$dir"
  done
  (cd "$SLOT_SRC" && find -L . -type f) | while IFS= read -r file; do
    ln -s "$SLOT_SRC/$file" "$SLOT_DST/$file"
  done
fi

# The pass list, and why each entry is on it:
#
#   PATH, TMPDIR, CARGO_HOME, RUSTUP_HOME, CARGO_TARGET_DIR
#     Cargo's own machinery. Without the first four cargo stops working
#     and a run that cannot build tests nothing; the last is forwarded
#     so a contributor who keeps build artifacts elsewhere gets one
#     tree rather than two.
#
#   CI
#     `bookrack_extract::pdfium_gate` turns a missing PDFium into a
#     loud failure only where `CI` is set, and into a visible skip
#     otherwise. Scrubbing it would make this gate manufacture the
#     silent skip the project is trying to eliminate.
#
#   BOOKRACK_NO_DOTENV=1
#     `env -i` does not reach `.env`. Cargo runs every test binary with
#     its working directory set to the package root, so `dotenvy`'s
#     upward search lands on the repository's own file and re-sets
#     whatever this script just cleared. Suppressing the load is the
#     only reason that variable exists.
#
#   RUST_BACKTRACE
#     Diagnostics. Forwarded rather than pinned so a contributor can
#     ask for a backtrace without editing this file.
#
# Nothing else is added. In particular BOOKRACK_DATA_DIR,
# BOOKRACK_REGISTRY, BOOKRACK_RUNTIME_DIR, BOOKRACK_DAEMON_STATE_DIR
# and BOOKRACK_OLLAMA_URL stay out: a test that needs one builds it
# through `bookrack-test-support`. `env -i` already achieves that; this
# paragraph exists so the next person adding "just one" variable has to
# argue with it first.
env -i \
  PATH="$PATH" \
  HOME="$FAKE" \
  XDG_CONFIG_HOME="$FAKE/.config" \
  XDG_DATA_HOME="$FAKE/.local/share" \
  XDG_CACHE_HOME="$FAKE/.cache" \
  TMPDIR="${TMPDIR:-/tmp}" \
  CARGO_HOME="${CARGO_HOME:-$REAL_HOME/.cargo}" \
  RUSTUP_HOME="${RUSTUP_HOME:-$REAL_HOME/.rustup}" \
  RUST_BACKTRACE="${RUST_BACKTRACE:-0}" \
  ${CARGO_TARGET_DIR:+CARGO_TARGET_DIR="$CARGO_TARGET_DIR"} \
  ${CI:+CI="$CI"} \
  BOOKRACK_NO_DOTENV=1 \
  cargo nextest run --no-fail-fast "$@"
