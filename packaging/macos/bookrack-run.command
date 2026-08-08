#!/bin/sh
# Terminal.app entry point for Bookrack.app.
#
# Lives in Contents/Resources so the bookrack binary, libpdfium, and
# any portable data directory all sit alongside it.
#
# The shell at the end keeps the window usable after the daemon exits,
# so its output stays readable instead of being replaced by
# "[Process completed]". Matches the Linux .desktop entry. The daemon's
# exit status is discarded deliberately: under `set -e` a non-zero one
# would end the script here, closing the window in the very case the
# operator needs to read it.
set -eu
cd "$(dirname "$0")"
./bookrack run || true
exec "${SHELL:-/bin/sh}"
