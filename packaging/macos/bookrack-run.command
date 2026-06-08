#!/bin/sh
# Terminal.app entry point for Bookrack.app.
#
# Lives in Contents/Resources so the bookrack binary, libpdfium, and
# any portable data directory all sit alongside it.
set -eu
cd "$(dirname "$0")"
exec ./bookrack run
