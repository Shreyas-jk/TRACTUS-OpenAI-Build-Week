# Tractus

A scope firewall for autonomous coding agents. Turns your request into a deterministic Intent
Contract, intercepts every command the agent proposes, previews consequences in a Docker twin,
and blocks well-intentioned actions that exceed what you asked for.

OpenAI Build Week 2026, Developer Tools track.

- [DESIGN.md](DESIGN.md) — system design
- [TRACTUS_CORE.md](TRACTUS_CORE.md) — Rust engine architecture

## Quickstart

Three commands take you from a fresh clone to Codex running under an enforced contract:

```sh
cargo build --release          # build every binary once
./target/release/tractus init  # one-time setup (installs the Codex hook + enables it)
./target/release/tractus        # pick or create a contract, then launch Codex
```

Requires Codex CLI **v0.114 or newer** with experimental hooks, on macOS or Linux. Tip: add the build directory to your `PATH` (`export PATH="$PWD/target/release:$PATH"`) to type `tractus` and `tractus-console` directly instead of the full path.

### `tractus init`

One-time, idempotent setup that replaces the previous manual steps:

- Generates the machine-local `.codex/hooks.json` pointing at the committed self-locating wrapper (covers both `Bash` and native `apply_patch`). The file is gitignored, so every clone generates its own absolute path.
- Enables the experimental hook feature flag in `~/.codex/config.toml`, preserving your existing settings and writing a timestamped backup before any edit. (Respects `CODEX_HOME`.)
- Verifies the `tractusd` and `tractus-hook` binaries are built and reports anything missing.

Safe to re-run: unchanged files are left untouched.

### `tractus`

Run with no command for the guided flow: it lists the recent contracts for the current repository (or launches the wizard if there are none), activates your choice, then starts Codex with it enforced. Pass Codex options after `--`, for example `tractus -- --model gpt-5.6-terra`.

The individual subcommands remain for scripting:

- `tractus new` — the contract wizard only. Records paths, operations, dependency/network/Git grants, shows a plain-language preview, and saves only after confirmation. Documents live in the local `.tractus/` directory, with the 20 most recently used contracts retained by default (bounded 10–30 policy).
- `tractus codex` — launch with the active contract. Loads the selected document, starts or reuses a workspace-local daemon, verifies that daemon owns the same workspace, registers that exact contract, and launches Codex with its contract ID. A missing, corrupt, cross-workspace, or unregistered document prevents Codex from starting.

Set `TRACTUS_SOCK` only when deliberately overriding the workspace-local socket.

Commands outside the active contract are denied inline before Codex runs them.

## Firewall dashboard (`tractus-console`)

`tractus-console` is a single Rust binary that serves the live firewall dashboard, extracts Intent Contracts with GPT-5.6, and bridges the daemon's event stream to the browser. It replaces the former Python/FastAPI control plane, so the whole product is now one `cargo build` and one toolchain.

```sh
# Put OPENAI_API_KEY in a .env file (OPENAI_API_KEY=sk-...) or export it — the console auto-loads .env.
./target/release/tractus-console
```

Then open <http://127.0.0.1:8787>. The console auto-loads `.env` from the working directory at startup (real environment variables win), so there is nothing to export; without a key it still runs and the dashboard degrades gracefully (intent extraction returns 503 until a key is present). Started from the project root, it auto-selects the same `.tractus/tractusd.sock` that `tractus codex` uses, so `tractus-hook` and `tractus-shim` proposals light up the ledger live with no extra wiring. Flags: `--addr <host:port>`, `--sock <path>`, `--workspace <path>`. All GPT-5.6 calls live in the console, never in `tractusd` — the enforcement path stays deterministic and LLM-free. Model overrides: `INTENT_MODEL` (default `gpt-5.6-sol`), `EXPLAIN_MODEL` (default `gpt-5.6-luna`).

> **Hook payload validation:** verified against a live Codex CLI v0.145.0 run on macOS. Both `Bash` and native `apply_patch` arrive as `PreToolUse` payloads with a string in `tool_input.command`. Set `TRACTUS_HOOK_LOG=/path/to/capture.log` to capture future version changes before relying on a new Codex release.

> **Fail-closed holds:** current Codex `PreToolUse` hooks do not support `permissionDecision: "ask"`; Codex reports it as a failed hook and continues the tool call. Tractus therefore denies unresolved or unavailable requests and requires an explicit contract amendment before retrying.

> Stub. Setup instructions, sample data, demo video link, and the "how GPT-5.6 and Codex were
> used" section land before submission (Mon Jul 20), generated in the Codex session.
