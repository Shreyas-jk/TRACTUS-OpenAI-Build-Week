#!/bin/sh
set -eu

SCRIPT_DIR=$(CDPATH='' cd -P "$(dirname "$0")" && pwd)
ROOT_DIR=$(CDPATH='' cd -P "$SCRIPT_DIR/.." && pwd)
WRAPPER="$ROOT_DIR/.codex/run-ct-hook.sh"
HOOKS_FILE="$ROOT_DIR/.codex/hooks.json"

if [ ! -x "$WRAPPER" ]; then
    printf '%s\n' "Tractus hook wrapper is missing or not executable: $WRAPPER" >&2
    exit 2
fi

cat > "$HOOKS_FILE" <<EOF
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash|apply_patch",
        "hooks": [
          {
            "type": "command",
            "command": "$WRAPPER"
          }
        ]
      }
    ]
  }
}
EOF

printf '%s\n' "Installed Tractus hook configuration at $HOOKS_FILE"
