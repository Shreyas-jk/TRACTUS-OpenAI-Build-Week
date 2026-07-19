# Chaos Twin

A scope firewall for autonomous coding agents. Turns your request into a deterministic Intent
Contract, intercepts every command the agent proposes, previews consequences in a Docker twin,
and blocks well-intentioned actions that exceed what you asked for.

OpenAI Build Week 2026, Developer Tools track.

- [DESIGN.md](DESIGN.md) — system design
- [CHAOS_CORE.md](CHAOS_CORE.md) — Rust engine architecture

## Install as a Codex plugin

Chaos Twin's Codex hook requires Codex CLI **v0.114 or newer** with experimental hooks, and is supported on macOS and Linux only.

1. Build the hook binary with `cargo build --bin ct-hook`. The repo-local [`.codex/hooks.json`](.codex/hooks.json) registers its absolute binary path for the `PreToolUse` `Bash` matcher.
2. Enable hooks in `~/.codex/config.toml`:

   ```toml
   [features]
   codex_hooks = true
   ```

3. Start `chaosd`, then use the control plane to confirm an Intent Contract.
4. Run Codex normally. Commands outside that contract are denied inline before Codex runs them.

`ct-hook` sends proposals to the same `chaosd` instance as `ct-shim`, so the existing dashboard lights up live with no extra wiring.

> Stub. Setup instructions, sample data, demo video link, and the "how GPT-5.6 and Codex were
> used" section land before submission (Mon Jul 20), generated in the Codex session.
