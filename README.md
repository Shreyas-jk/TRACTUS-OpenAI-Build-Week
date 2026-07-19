# Chaos Twin

A scope firewall for autonomous coding agents. Turns your request into a deterministic Intent
Contract, intercepts every command the agent proposes, previews consequences in a Docker twin,
and blocks well-intentioned actions that exceed what you asked for.

OpenAI Build Week 2026, Developer Tools track.

- [DESIGN.md](DESIGN.md) — system design
- [CHAOS_CORE.md](CHAOS_CORE.md) — Rust engine architecture

## Install as a Codex plugin

Chaos Twin's Codex hook requires Codex CLI **v0.114 or newer** with experimental hooks, and is supported on macOS and Linux only.

1. Build and install the hook with:

   ```sh
   cargo build --release --bin ct-hook && ./scripts/install-hook.sh
   ```

   This generates the machine-local `.codex/hooks.json` with the absolute path to the committed self-locating wrapper. The file is intentionally gitignored: every clone must generate its own path. The hook covers both `Bash` and native `apply_patch` edits.
2. Enable hooks in `~/.codex/config.toml`:

   ```toml
   [features]
   codex_hooks = true
   ```

3. Start `chaosd`, then use the control plane to confirm an Intent Contract.
4. Run Codex normally. Commands outside that contract are denied inline before Codex runs them.

`ct-hook` sends proposals to the same `chaosd` instance as `ct-shim`, so the existing dashboard lights up live with no extra wiring.

> **Hook schema verification pending:** `ct-hook`'s `PreToolUse` payload handling has not yet been confirmed against a live Codex run. Set `CHAOSTWIN_HOOK_LOG=/path/to/capture.log` for that run to append the exact raw payload before parsing, then compare it with the supported fields before relying on the integration.

> Stub. Setup instructions, sample data, demo video link, and the "how GPT-5.6 and Codex were
> used" section land before submission (Mon Jul 20), generated in the Codex session.
