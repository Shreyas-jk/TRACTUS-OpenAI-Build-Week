#!/bin/sh
set -eu
# Verify the globally-installed Tractus Codex hook actually fires on a real tool
# call. One command, no arguments.
#
# Prerequisites:
#   - `tractus init` has been run (global hook in ~/.codex/hooks.json, feature on)
#   - the Codex CLI is authenticated on an account with available usage
#
# How it works: it launches a throwaway `codex exec` in a scratch directory
# OUTSIDE any Tractus-managed workspace and asks the agent to run one harmless
# shell command. Because it is an unmanaged session, the hook fires, logs the
# PreToolUse payload, and returns "continue" (safe pass-through). If the log
# captured bytes, the global hook fired.
#
# Notes:
#   - We do NOT pass --dangerously-bypass-approvals-and-sandbox: that sets
#     approval=never and skips the hook pipeline (the hook IS the pipeline).
#   - We DO pass --dangerously-bypass-hook-trust so this runs non-interactively;
#     in normal use Codex instead asks you to trust the hook once.

command -v codex >/dev/null 2>&1 || {
    printf '%s\n' "codex CLI not found on PATH" >&2
    exit 2
}

PROJECT=$(mktemp -d)
LOG=$(mktemp)
trap 'rm -rf "$PROJECT" "$LOG"' EXIT

printf 'Running Codex in %s (outside any Tractus workspace)…\n' "$PROJECT"
TRACTUS_HOOK_LOG="$LOG" codex exec \
    --cd "$PROJECT" \
    --skip-git-repo-check \
    --dangerously-bypass-hook-trust \
    'Run exactly this shell command and nothing else: echo tractus-hook-probe. Then stop.' \
    >/dev/null 2>&1 || true

if [ -s "$LOG" ]; then
    printf 'PASS: the global hook fired (%s bytes of PreToolUse payload captured).\n' "$(wc -c <"$LOG" | tr -d ' ')"
    exit 0
fi

printf 'FAIL: the hook did not fire (empty capture).\n' >&2
printf 'Check: (1) `tractus init` installed ~/.codex/hooks.json, (2) [features] hooks = true,\n' >&2
printf '       (3) the Codex account has available usage, (4) the hook is trusted.\n' >&2
exit 1
