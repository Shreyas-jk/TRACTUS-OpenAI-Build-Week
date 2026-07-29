#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -P "$(dirname "$0")" && pwd)
ROOT_DIR=$(CDPATH='' cd -P "$SCRIPT_DIR/.." && pwd)

for BINARY in "$ROOT_DIR/target/release/tractus-hook" "$ROOT_DIR/target/debug/tractus-hook"; do
    if [ -x "$BINARY" ]; then
        exec "$BINARY" "$@"
    fi
done

printf '%s\n' \
    'Tractus tractus-hook binary not found. Build it with: cargo build --release --bin tractus-hook' \
    >&2
exit 2
