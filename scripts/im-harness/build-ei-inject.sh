#!/usr/bin/env bash
# Build the libei key injector. Output goes next to the source.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cc -O2 -Wall -Wextra -o "$here/ei-inject" "$here/ei-inject.c" \
   $(pkg-config --cflags --libs libei-1.0) \
   $(pkg-config --cflags --libs libsystemd)
echo "built $here/ei-inject"
