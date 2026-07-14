# chaos-core — Rust Architecture

**Status:** Draft v1 (Jul 13, 2026). Companion to DESIGN.md Sections 4.1 and 4.2.

## 1. Process model

Three Rust artifacts in one cargo workspace, plus the Python control plane:

```
chaostwin/
├── Cargo.toml            # workspace
├── chaos-core/           # lib: contract, parse, classify, verdict, history. Pure, no IO.
├── chaosd/               # bin: daemon. Owns enforcement state, twin pool, event bus.
├── ct-shim/              # bin: interceptor. Thin client invoked per command.
└── control/              # Python: FastAPI UI server + GPT-5.6 calls (separate from workspace)
```

- **`ct-shim`** mimics the `sh -c` interface so any agent that shells out can use it: Codex CLI is configured with `SHELL=ct-shim` (or its shell hook), and each proposed command arrives as `ct-shim -c "<command>"`. The shim sends the command to `chaosd` over a Unix domain socket, waits for the verdict, then either `exec`s the real `/bin/sh -c <command>` (preserving cwd, env, tty, and exit code) or prints the synthetic handoff output and exits 1. The shim contains zero decision logic.
- **`chaosd`** is the single long-lived daemon: loads the active contract, runs the verdict pipeline from `chaos-core`, owns the Docker twin pool, holds `NEEDS_HUMAN` commands pending user decisions, and broadcasts events. Socket at `$XDG_RUNTIME_DIR/chaostwin.sock`, JSON-lines protocol.
- **`control/` (FastAPI)** connects to the same socket: subscribes to the event stream for the UI WebSocket, sends `set_contract` after intent extraction, and sends `resolve` when the user clicks approve/reject. All GPT-5.6 calls live here, never in Rust, preserving the "no LLM in the enforcement path" invariant at the process boundary.

## 2. Core data types (`chaos-core`)

```rust
use globset::GlobSet;
use std::path::PathBuf;

pub struct Contract {
    pub task: String,
    pub allowed_paths: GlobSet,          // compiled from user-approved patterns
    pub allowed_ops: OpSet,              // bitflags over OpClass
    pub deps_may_change: bool,
    pub git_ops: GitOpSet,               // e.g. {Status, Diff}
    pub network: bool,
    pub workspace_root: PathBuf,
}

#[derive(Clone, Copy, PartialEq)]
pub enum OpClass { Read, Edit, Create, Delete, Test, Build, Run }

#[derive(Clone, Copy, PartialEq)]
pub enum GitOp { Status, Diff, Log, Add, Commit, Checkout, Push, ForcePush, ResetHard, Clean }

/// What a command will do, either declared by the classifier or observed by the twin.
#[derive(Default)]
pub struct Effects {
    pub reads: Vec<PathBuf>,
    pub writes: Vec<PathBuf>,
    pub deletes: Vec<PathBuf>,
    pub dep_change: Option<DepChange>,   // manifest or lockfile touched
    pub git: Option<GitOp>,
    pub network: bool,
    pub privileged: bool,                // sudo, chown, mount...
    pub op: OpClass,
}

pub struct DepChange {
    pub manifest: PathBuf,               // Cargo.toml, package.json, pyproject.toml...
    pub summary: String,                 // "add axios@1.6" (for the explainer prompt)
}

pub enum Verdict {
    InScope,
    ScopeViolation(Vec<ProofTrace>),     // all violated clauses, for the UI
    NeedsHuman(Reason),
    Loop { n: u32, signature: String },
}

pub enum Reason { Opaque, TwinTimeout, UnresolvedVar, ContractAmbiguous }

pub struct ProofTrace {
    pub rule: &'static str,              // stable id, e.g. "R-DEP-01"
    pub clause: String,                  // "deps_may_change = false"
    pub effect: String,                  // "Cargo.toml modified (add axios@1.6)"
    pub rendered: String,                // "deps_may_change=false ∧ Cargo.toml modified ⇒ SCOPE_VIOLATION"
}
```

Design invariant, enforced by a property test: **no code path maps a parse failure or an unclassified command to `InScope`.** Unknown always routes to the twin; twin failure always routes to `NeedsHuman`.

## 3. Parsing pipeline (`parse.rs`, shell-words specifics)

`shell-words` tokenizes a single simple command; it does not understand operators, so parsing is staged:

**Stage 0 — opacity gate.** If the raw string contains command substitution (`$(`, backticks), process substitution (`<(`), `eval`, `source`/`.`, heredocs, or unbalanced quotes, return `Opaque` immediately → twin path. We do not attempt to be clever here; the twin is ground truth.

**Stage 1 — operator split.** A small quote-aware scanner (one pass, tracks `'`/`"`/escape state) splits the top level on `;`, `&&`, `||`, `|`, `&` into simple-command segments, preserving the operators. A pipeline or `&&` chain is verdict-checked per segment; the composite verdict is the most severe segment verdict.

**Stage 2 — redirection extraction.** Within each segment, `>`, `>>`, `2>`, `&>` targets are pulled out as `writes` effects (and `<` as `reads`) before tokenization, since redirects are effects regardless of the command.

**Stage 3 — tokenize.** `shell_words::split(segment)` → argv. `Err(_)` → `Opaque`.

**Stage 4 — env prefix.** Leading `KEY=value` tokens are stripped and recorded; classification applies to the remaining argv.

**Stage 5 — variable policy.** Tokens containing `$VAR`/`${VAR}` in path positions: substitute from the environment snapshot captured by the shim if the variable is defined there; otherwise return `NeedsHuman(UnresolvedVar)`. We never guess what an undefined variable expands to (`rm -rf $DIR/` with empty `DIR` is exactly the classic failure).

**Stage 6 — path normalization.** Join against the shim-reported cwd, lexically normalize (`.`/`..` resolution, no filesystem access), then check containment under `workspace_root`. Lexical escape (`../../etc`) fails `allowed_paths` automatically; symlink escapes are not caught here by design; the twin and its read-only mounts outside the snapshot catch those.

```rust
pub enum ParseOutcome {
    Commands(Vec<SimpleCommand>),   // argv + redirect effects + env prefix, per segment
    Opaque(String),                 // reason, for the UI
}

pub struct SimpleCommand {
    pub argv: Vec<String>,
    pub redirect_writes: Vec<PathBuf>,
    pub redirect_reads: Vec<PathBuf>,
}
```

## 4. Command classifier (`classify.rs`)

A static corpus embedded at compile time (RON file, `include_str!` + `serde`), ~40 families. Matching is on argv0 basename, optional subcommand, then a flag-modifier table.

```ron
// corpus.ron (excerpt)
[
    (
        family: "cargo-add",
        argv0: "cargo", subcommand: Some("add"),
        effects: (op: Edit, dep_change: true, network: true,
                  writes: [Manifest("Cargo.toml"), Lockfile("Cargo.lock")]),
    ),
    (
        family: "git-push",
        argv0: "git", subcommand: Some("push"),
        effects: (op: Run, git: Some(Push), network: true),
        flag_escalations: { "--force": (git: Some(ForcePush)), "-f": (git: Some(ForcePush)) },
    ),
    (
        family: "rm",
        argv0: "rm",
        effects: (op: Delete, deletes: FromArgs),   // positional args become delete paths
        flag_escalations: { "-r": (recursive: true), "-rf": (recursive: true, forced: true) },
    ),
    (
        family: "cargo-test",
        argv0: "cargo", subcommand: Some("test"),
        effects: (op: Test, writes: [Dir("target/")]),
    ),
]
```

`FromArgs` path fields resolve through the Stage 6 normalizer. No corpus match → `Unclassified` → twin. The five scripted demo commands each have a dedicated corpus entry plus an integration test, per DESIGN.md Section 4.2; reliability of the demo path is a test suite obligation, not a hardcode.

## 5. Verdict algorithm (`verdict.rs`)

Ordered, short-circuit-free (collect all violations for the UI; latency is trivial):

```rust
pub fn check(effects: &Effects, c: &Contract, history: &History) -> Verdict {
    if let Some(l) = history.loop_detected() { return Verdict::Loop { .. }; }
    let mut proofs = vec![];
    if effects.privileged                          { proofs.push(trace("R-PRIV-01", ..)); }
    if effects.network && !c.network               { proofs.push(trace("R-NET-01", ..)); }
    if effects.dep_change.is_some() && !c.deps_may_change
                                                   { proofs.push(trace("R-DEP-01", ..)); }
    if let Some(g) = effects.git {
        if !c.git_ops.contains(g)                  { proofs.push(trace("R-GIT-01", ..)); }
    }
    for p in effects.writes.iter().chain(&effects.deletes) {
        if !c.allowed_paths.is_match(p)            { proofs.push(trace("R-PATH-01", ..)); }
    }
    if !c.allowed_ops.contains(effects.op)         { proofs.push(trace("R-OP-01", ..)); }
    if proofs.is_empty() { Verdict::InScope } else { Verdict::ScopeViolation(proofs) }
}
```

The same function checks twin-observed diffs: the twin's file diff is converted into an `Effects` (writes/deletes from the overlay upper dir, `dep_change` if a manifest/lockfile appears in the diff) and passed through `check` unchanged. One enforcement path, two effect sources.

## 6. Loop detector (`history.rs`)

- Signature: `blake3(argv0, subcommand, sorted_flags, exit_class)` where `exit_class` buckets exit codes (0 / nonzero / signal).
- Ring buffer of the last 20 executed commands per agent session (post-execution, shim reports exit codes back to chaosd).
- 3 identical failure signatures within the window → `Loop`. Only failures count; re-running `cargo test` successfully is normal.

## 7. chaosd wire protocol (JSON lines over UDS)

```jsonc
// ct-shim → chaosd
{"type":"propose","id":"c17","cmd":"cargo add axios@1.6","cwd":"/work/repo","env":{"DIR":"..."} }
{"type":"report","id":"c17","exit_code":101}            // after real execution, feeds history

// chaosd → ct-shim
{"type":"verdict","id":"c17","action":"allow"}
{"type":"verdict","id":"c17","action":"block","exit_code":1,
 "synthetic_stdout":"Command blocked by Chaos Twin: dependency changes are not in scope..."}
{"type":"verdict","id":"c17","action":"hold"}            // NEEDS_HUMAN, shim waits (60 s cap)

// control (FastAPI) ↔ chaosd
{"type":"subscribe"}                                     // → stream of contract/proposed/twin-diff/verdict/blocked/loop-halt events
{"type":"set_contract","contract":{...}}
{"type":"resolve","id":"c17","decision":"approve_once"}  // or "reject"
```

The `hold` path implements DESIGN.md 4.1: shim blocks up to 60 s for a `resolve`; timeout or reject produces the deferred synthetic handoff.

## 8. Twin executor (`chaosd::twin`)

- v1 shells out to the `docker` CLI (no bollard dependency; one less thing to debug): `docker run --rm --network=none --pids-limit=256 -v <lowerdir>:/work:ro -v <upperdir>:/upper ...` with an overlay mount inside the container so the workspace snapshot is copy-on-write.
- Pool of 2 pre-warmed containers, replenished in the background after each use.
- `tokio::time::timeout(Duration::from_secs(3), run)`; on expiry: `docker kill`, discard upper dir, return `NeedsHuman(TwinTimeout)` (DESIGN.md 4.3).
- Diff = walk of the overlay upper dir (created/modified) plus whiteout files (deleted). Converted to `Effects`, passed to `check`.
- `--network=none` always in the twin, even when the contract allows network: the twin previews filesystem consequences; allowing real network side effects from a speculative run would make "speculative" a lie.

## 9. Testing plan

| Test | Tool |
|---|---|
| Classifier corpus: every family × verdict snapshot | `insta` snapshot tests |
| Never-allow-by-default invariant: random/fuzzed strings never yield `InScope` without a corpus match | `proptest` |
| The 5 demo scenarios end-to-end through ct-shim against a scripted fake agent | integration test, runs in CI on every commit |
| Twin timeout: `sleep 10` returns `NeedsHuman(TwinTimeout)` in ~3 s | tokio integration test |
| Loop detector: 3 identical failing `cargo build`s → `Loop` | unit |
| Synthetic handoff phrasing: Codex CLI asks the user instead of retrying | manual, Thu 16, logged transcripts in repo |

## 10. Crates

`serde`, `serde_json`, `shell-words`, `globset`, `blake3`, `tokio` (chaosd/shim only), `tracing`, `ron`; dev: `insta`, `proptest`. `chaos-core` itself is sync and dependency-light so the whole verdict path unit-tests without a runtime.

## 11. Build order (maps to DESIGN.md Section 9)

- **Mon 13:** workspace scaffold, data types, Stage 0 to 3 of the parser, corpus schema.
- **Tue 14:** classifier + verdict + proof traces, loop detector, snapshot/property tests green.
- **Wed 15:** ct-shim + chaosd + UDS protocol; Docker twin with timeout and diffing.
- **Thu 16 onward:** per DESIGN.md (control plane, UI, hardening, Sunday video).
