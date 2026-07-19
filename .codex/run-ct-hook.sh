#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -P "$(dirname "$0")" && pwd)
ROOT_DIR=$(CDPATH='' cd -P "$SCRIPT_DIR/.." && pwd)

for BINARY in "$ROOT_DIR/target/release/ct-hook" "$ROOT_DIR/target/debug/ct-hook"; do
    if [ -x "$BINARY" ]; then
        exec "$BINARY" "$@"
    fi
done

printf '%s\n' \
    'Chaos Twin ct-hook binary not found. Build it with: cargo build --release --bin ct-hook' \
    >&2
exit 2
