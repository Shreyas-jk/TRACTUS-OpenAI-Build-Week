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

- Installs the Codex hook **globally** in `~/.codex/hooks.json` (covers both `Bash` and native `apply_patch`), pointing at the committed self-locating wrapper. It **merges** into any existing hooks rather than clobbering them, and backs up the file before editing. A global install means enforcement applies to **every** project you launch through `tractus`, not just this repo.
- Enables the experimental hook feature flag in `~/.codex/config.toml`, preserving your existing settings and writing a timestamped backup before any edit. (Respects `CODEX_HOME`.)
- Verifies the `tractusd` and `tractus-hook` binaries are built and reports anything missing.

Safe to re-run: unchanged files are left untouched. The global hook is safe to leave installed: it **no-ops for any Codex session `tractus` did not launch** (detected via the `TRACTUS_WORKSPACE_ROOT` marker the launcher exports), so ordinary Codex work is never intercepted or blocked.

**Hook trust:** Codex gates hooks behind a one-time trust prompt, so the first time it runs the Tractus hook it will ask you to approve it — do so once. To confirm the global hook actually fires on a real tool call, run `scripts/verify_hook.sh` (needs a Codex account with available usage); it launches a throwaway session outside any workspace and reports whether the hook was invoked.

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

## How GPT-5.6 is used

The design is a deliberate **neurosymbolic split**: the model turns fuzzy human intent into structure once; a deterministic Rust engine enforces that structure forever.

- **Intent extraction** (`gpt-5.6-sol`, structured outputs) turns your natural-language request into the Intent Contract you confirm.
- **Divergence explainer** (`gpt-5.6-luna`) writes the one-sentence, advisory "why this was blocked" note on the dashboard. It is advisory only — it can never turn a block into an allow.
- **The enforcement path has zero LLM calls.** Every allow/block/hold verdict is computed by `tractus-core` from the confirmed contract, so it is deterministic and reproducible.

Both model calls live in `tractus-console`, never in the enforcement daemon. Tractus began as an OpenAI Build Week 2026 project (Developer Tools track) built in the Codex CLI with GPT-5.6.

## Continuous integration and evals

Two layers, matching standard practice for LLM-backed systems — deterministic checks gate every change; the costly, non-deterministic LLM sweep runs separately.

- **`ci.yml`** (every push/PR): `cargo fmt --check`, `cargo build`, `cargo test --workspace`. Fast and fully deterministic; the enforcement engine's snapshot/property tests live here. (Docker twin tests are `#[ignore]`d.)
- **`tractus-eval`** + **`eval.yml`** (manual/nightly): an **LLM-as-judge** harness for the model surface. For each case in a versioned dataset (`tractus-eval/cases.json`) it runs the real intent extraction, applies deterministic must-checks (deps/network/run gates, required paths), then grades the contract with a rubric judge (structured output, criteria scored 1–5: faithfulness, least-privilege, path-scope) using **repeat + min-pass** to absorb non-determinism. It never runs on the PR gate (cost + flakiness) and never touches the enforcement path. Run it locally with `cargo run -p tractus-eval -- --report eval-report.json` (needs `OPENAI_API_KEY`); in CI it needs the `OPENAI_API_KEY` repository secret.

## Repository layout

| Crate | Role |
|---|---|
| `tractus-core` | Pure, IO-free engine: contract, parse, classify, verdict, history |
| `tractusd` | Long-lived daemon: enforcement state, Docker twin pool, event bus |
| `tractus-shim` | `sh -c` interceptor for any agent that shells out |
| `tractus-hook` | Native Codex `PreToolUse` plugin |
| `tractus` | Contract wizard + fail-closed Codex launcher (`init`, `new`, `codex`) |
| `tractus-console` | axum dashboard + GPT-5.6 intent extraction + event bridge |
| `tractus-eval` | LLM-as-judge eval harness for intent extraction (dev tooling) |

See [DESIGN.md](DESIGN.md) for the full design and [TRACTUS_CORE.md](TRACTUS_CORE.md) for the engine internals.
