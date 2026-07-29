# Working in this repo

Tractus is a Rust cargo workspace (plus a static dashboard asset in
`tractus-console/assets`). Architecture: [TRACTUS_CORE.md](TRACTUS_CORE.md) for the
engine, [DESIGN.md](DESIGN.md) for the system.

## Commands

- Build every binary: `cargo build --release`
- Test the workspace: `cargo test --workspace` (Docker twin tests are `#[ignore]`; run them with `make twin-test`)
- Format before committing: `cargo fmt --all` — the gate is `cargo fmt --all --check`
- Toolchain is pinned in `rust-toolchain.toml` (1.97.0, `+rustfmt`).

## Invariants — do not break

- **No LLM calls in the enforcement path.** `tractus-core` and `tractusd` are deterministic; every GPT-5.6 call lives in `tractus-console`. Keep the core dependency-light — heavy deps (axum, reqwest, PTY) belong in the console, never in `tractusd`.
- **Never map a parse failure or an unclassified command to `InScope`.** Unknown routes to the Docker twin; twin failure routes to `NeedsHuman`. This is property-tested.
- **Fail closed** inside a Tractus-managed session; the globally-installed Codex hook **no-ops** for sessions Tractus did not launch (gated on the `TRACTUS_WORKSPACE_ROOT` marker the launcher exports), so ordinary Codex work is never intercepted.

## Conventions

- Every change lands with an executable check — a passing test, or a documented manual verification. Don't leave the tree red.
- When debugging, paste compiler and test errors verbatim; do not paraphrase them.
- Secrets (`OPENAI_API_KEY` in `.env`, Codex `auth.json`) never get committed or copied into the repo.
