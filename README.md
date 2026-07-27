# Tractus

A scope firewall for autonomous coding agents. Turns your request into a deterministic Intent
Contract, intercepts every command the agent proposes, previews consequences in a Docker twin,
and blocks well-intentioned actions that exceed what you asked for.

OpenAI Build Week 2026, Developer Tools track.

- [DESIGN.md](DESIGN.md) — system design
- [TRACTUS_CORE.md](TRACTUS_CORE.md) — Rust engine architecture

## Durable project contracts from the CLI

Build the launcher and daemon once, create a contract for the current repository, then launch Codex through it:

```sh
cargo build --release --bin tractus --bin chaosd --bin ct-hook
./target/release/tractus new
./target/release/tractus codex
```

`tractus new` is a terminal wizard: it records paths, operations, dependency/network/Git grants, shows a plain-language preview, and saves only after confirmation. Documents live in the local `.tractus/` directory, with the 20 most recently used contracts retained by default (the store supports a bounded 10–30 policy). `tractus codex` loads the selected document, starts or reuses a workspace-local daemon, verifies that daemon owns the same workspace, registers that exact contract, and launches Codex with its contract ID. A missing, corrupt, cross-workspace, or unregistered document prevents Codex from starting.

Pass Codex options after `--`, for example `tractus codex -- --model gpt-5.6-terra`. Set `TRACTUS_SOCK` only when deliberately overriding the workspace-local socket.

## Install as a Codex plugin

Tractus's Codex hook requires Codex CLI **v0.114 or newer** with experimental hooks, and is supported on macOS and Linux only.

1. Build and install the hook with:

   ```sh
   cargo build --release --bin ct-hook && ./scripts/install-hook.sh
   ```

   This generates the machine-local `.codex/hooks.json` with the absolute path to the committed self-locating wrapper. The file is intentionally gitignored: every clone must generate its own path. The hook covers both `Bash` and native `apply_patch` edits.
2. Enable hooks in `~/.codex/config.toml`:

   ```toml
   [features]
   hooks = true
   ```

3. Create a contract with `tractus new`, then start Codex with `tractus codex`.
4. Commands outside that contract are denied inline before Codex runs them.

`ct-hook` sends proposals to the same `chaosd` instance as `ct-shim`, so the existing dashboard lights up live with no extra wiring. Start the dashboard from the project root (or set `TRACTUS_WORKSPACE_ROOT` to it) and it will select `.tractus/chaosd.sock` automatically.

> **Hook payload validation:** verified against a live Codex CLI v0.145.0 run on macOS. Both `Bash` and native `apply_patch` arrive as `PreToolUse` payloads with a string in `tool_input.command`. Set `TRACTUS_HOOK_LOG=/path/to/capture.log` to capture future version changes before relying on a new Codex release.

> **Fail-closed holds:** current Codex `PreToolUse` hooks do not support `permissionDecision: "ask"`; Codex reports it as a failed hook and continues the tool call. Tractus therefore denies unresolved or unavailable requests and requires an explicit contract amendment before retrying.

> Stub. Setup instructions, sample data, demo video link, and the "how GPT-5.6 and Codex were
> used" section land before submission (Mon Jul 20), generated in the Codex session.
