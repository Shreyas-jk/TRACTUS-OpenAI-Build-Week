# Tractus — Design Doc

**Track:** Developer Tools (OpenAI Build Week, submission due Tue Jul 21, 5:00 PM PT)
**Author:** Shreyas J Kiran
**Status:** v2 (Jul 13, 2026) — rescoped per council review. v1 world model and AST parser cut.

## 1. One-liner

A scope firewall for autonomous coding agents. Tractus turns your request into a deterministic Intent Contract, intercepts every command the agent proposes, previews the real consequences in a Docker twin, and blocks well-intentioned actions that exceed what you actually asked for.

## 2. Problem

GPT-5.6 already ships aggressive safeguards against overtly destructive and malicious commands. The residual, unsolved failure mode is **unprompted overreach**: the agent takes well-intentioned actions beyond the user's intent. You ask it to fix a flaky test; it decides the real fix is upgrading a major dependency, rewriting the lockfile, and touching twelve files you never mentioned. Every action is individually "safe", so no native guardrail fires, and the blast radius lands in your working tree anyway.

No existing tool checks agent actions against *what the user asked for*. Approval prompts check "is this command dangerous", not "is this command in scope".

## 3. Core mechanism: the Intent Contract

1. **Extract.** When a task starts, the user's natural-language request is sent to GPT-5.6, which returns a structured scope contract (internal wire format; the user never sees this JSON):
   ```json
   {
     "task": "fix the failing test in tests/api_test.rs",
     "allowed_paths": ["tests/**", "src/api/**", "target/**"],
     "allowed_ops": ["read", "edit", "test", "build"],
     "deps_may_change": false,
     "git_ops": ["status", "diff"],
     "network": false
   }
   ```
   The extraction prompt always includes the project's build-artifact directories (`target/`,
   `node_modules/`, `__pycache__/`, `.venv/`) in `allowed_paths`; otherwise every test or build
   command false-blocks on artifact writes. Likewise, any code-editing task implies the `test`
   and `build` op grants; `run` stays a separate, explicit grant. Artifact paths and implied ops
   are listed in the toggle card but pre-checked and visually de-emphasized.
2. **Confirm.** No raw JSON in front of the user. The UI renders the contract as a plain-language card with toggles: "May edit files in: `tests/`, `src/api/`" (editable path chips), "May change dependencies: OFF", "May access network: OFF", "Git: read-only". One click approves; flipping a toggle amends the contract. From here on, no LLM is in the enforcement path.
3. **Enforce.** The Rust engine checks every proposed command, and every observed twin diff, against the contract. Deterministically: same input, same verdict, always.

Verdicts: `IN_SCOPE` (pass through), `SCOPE_VIOLATION` (block, show proof), `NEEDS_HUMAN` (contract ambiguous, show preview). Every violation carries a proof trace: the contract clause plus the observed effect that breached it.

This is the neurosymbolic split: **LLM proposes structure, symbolic engine enforces it.** GPT-5.6 is used where it is strong (translating fuzzy human intent into structure) and kept out of the path where it is weak (consistent enforcement).

## 4. Architecture

```
user request ──▶ GPT-5.6: intent extraction ──▶ Intent Contract (user-confirmed)
                                                        │
┌──────────────┐   proposed cmd    ┌────────────────────▼────────────┐
│ Coding agent  │ ────────────────▶ │  Interceptor (CLI/PTY shim)      │
│ (Codex CLI)   │ ◀──────────────── │  allow / block / ask-human       │
└──────────────┘   verdict          └──────────────┬─────────────────┘
                                                    ▼
                                    ┌─────────────────────────────────┐
                                    │  Rust engine (chaos-core)        │
                                    │  1. argv split (shell-words)     │
                                    │  2. command classifier (corpus)  │
                                    │  3. contract checker + proofs    │
                                    │  4. loop detector                │
                                    └──────┬───────────────┬──────────┘
                                    classified          unrecognized
                                           │                │
                                           │                ▼
                                           │   ┌──────────────────────────┐
                                           │   │  Docker twin              │
                                           │   │  speculative exec on CoW  │
                                           │   │  snapshot → observed diff │
                                           │   │  → diff checked vs        │
                                           │   │    contract               │
                                           │   └───────────┬──────────────┘
                                           │               │ diff
                                           │               ▼
                                           │   ┌──────────────────────────┐
                                           │   │  GPT-5.6 divergence       │
                                           │   │  explainer (advisory      │
                                           │   │  only, never blocks)      │
                                           │   └───────────┬──────────────┘
                                           ▼               ▼
                                    ┌─────────────────────────────────┐
                                    │  FastAPI + WebSocket → Ghost UI  │
                                    │  contract panel | real terminal  │
                                    │  twin preview diff | proof trace │
                                    └─────────────────────────────────┘
```

### 4.1 Interceptor

Wrapper the agent uses as its shell (Codex CLI hook or `SHELL=` shim). Captures each proposed command plus a cheap snapshot (cwd, git status hash). Blocks until chaos-core returns a verdict.

**Synthetic agent handoffs.** A block must guide the agent, not brick its loop. The interceptor never returns a bare non-zero exit code; every non-allow verdict maps to synthetic terminal output fed back to the agent as if the command had run:

- `SCOPE_VIOLATION` → exit 1, stdout: `Command blocked by Tractus: <violated clause>. This action is outside the approved task scope. Continue within scope, or ask the user to approve this specific action.`
- `NEEDS_HUMAN` → the interceptor holds the command while the UI prompts the user. On approval the command runs normally; on rejection or a 60 s decision timeout: exit 1, stdout: `Command deferred by Tractus: awaiting user approval. Do not retry this command; proceed with other in-scope work or ask the user.`
- `LOOP` → exit 1, stdout: `Halted by Tractus: this command has failed <n> times with the same error. Stop retrying and report the blocker to the user.`

Messages are written for agent consumption (imperative, next-action oriented) and tested against Codex CLI to confirm the agent asks for permission instead of thrashing.

### 4.2 Rust engine (`chaos-core`)

The deterministic blocking path. Zero LLM calls.

1. **Argv split.** `shell-words` crate for tokenization plus handling of pipes/`&&` by splitting on operators. Explicitly NOT a full shell AST (cut per council). Compound or unparseable input drops to the twin path, never to "allow".
2. **Command classifier.** Curated corpus of ~40 command families (git subcommands, cargo/npm/pip/uv, rm/mv/cp/chmod, curl/wget, docker, make). Each classified command maps to declared effects: paths touched, op class, dep-change, network, reversibility. The demo commands have dedicated, pre-tested classifier entries so the presentation path is bulletproof, but the classifier is real: judges can type arbitrary commands and get honest verdicts or twin fallback.
3. **Contract checker.** Effects vs contract clauses. Violations produce machine-readable proof traces, e.g. `deps_may_change=false ∧ effect: Cargo.toml modified (axios 0.27 → 1.6) ⇒ SCOPE_VIOLATION`.
4. **Loop detector.** Sliding window over (verb, normalized args, exit-class) hashes. Same failure class 3 times → halt agent, surface the repeating traceback. Doubles as API-budget protection.

### 4.3 Docker twin (ground truth for unknowns)

Unrecognized commands and scripts execute speculatively in a throwaway container on an overlayfs copy-on-write snapshot of the workspace. The observed diff (files created/modified/deleted, exit code, lockfile changes) is checked against the contract exactly like classified effects. Target: < 2 s round trip on the demo repo, with a **hard 3 s cap**: if speculative execution exceeds 3 seconds the container is killed, the snapshot discarded, and the verdict returned as `NEEDS_HUMAN` with reason `twin-timeout`, so neither the UI nor the agent ever hangs on a slow or interactive command. My systems background goes here instead of into a world model: pre-warmed container pool, upper-dir diffing only, workspace size capped.

### 4.4 GPT-5.6 divergence explainer (advisory only)

When a twin diff or classifier verdict is a violation, GPT-5.6 gets (contract, command, diff summary) and returns one plain-English sentence for the UI: "The agent is upgrading axios from 0.27 to 1.6 to fix the test, but you only asked it to fix the test." Advisory only; it can never turn a block into an allow. This layer plus intent extraction means GPT-5.6 is load-bearing in the product itself, not just in the build process, which is what the judging criteria evaluate.

### 4.5 Control plane and UI

FastAPI + WebSocket events: `contract`, `proposed`, `twin-diff`, `verdict`, `blocked`, `loop-halt`. Single-page dashboard: contract panel (the plain-language toggle card from Section 3; clauses light up red when violated), real terminal (xterm.js), twin preview diff pane, proof trace on block, per-violation "approve once" button so a block is a conversation, not a dead end.

## 5. Cut from v1 (and why)

| Cut | Reason |
|---|---|
| PyTorch world model (~15 M params, synthetic training) | Diluted Codex/GPT-5.6 usage (the judged criterion), added hallucination risk and 2 days of training work. Docker twin made fast instead. |
| Hand-rolled shell AST parser | Over-engineered for 7 days. Replaced by argv splitting + curated classifier, with twin fallback as the safe default. |
| "Blocks destructive attacks" as the headline | GPT-5.6 native safeguards already cover it. Headline is now scope enforcement; the destructive corpus survives as a secondary rule set. |

## 6. Verdict flow (demo storyline)

1. User task: "fix the failing test in tests/api_test.rs". GPT-5.6 emits the contract above; user confirms.
2. Agent: `cargo test` → classified, in scope → allowed, < 5 ms.
3. Agent edits `src/api/handler.rs` → in `allowed_paths` → allowed.
4. Agent decides the real problem is the HTTP client version: `cargo add axios@1.6` (well-intentioned, disastrous). Classifier: dep-change effect ∧ `deps_may_change=false` → **SCOPE_VIOLATION**, blocked in milliseconds. Contract panel flashes the violated clause, proof trace renders, explainer says why in one sentence. The agent receives the synthetic handoff and, instead of thrashing, asks the user for permission. User can approve-once or reject.
5. Agent tries `./scripts/fix_deps.sh` → unrecognized → Docker twin runs it: diff shows Cargo.lock rewritten, 214 transitive changes → same violation, caught by ground truth, with the diff rendered.
6. Agent retries the same failing build fix 3 times → LOOP verdict, halt, human handoff.

## 7. Tech stack

| Layer | Choice | Why |
|---|---|---|
| Engine | Rust (`shell-words`, custom classifier) | Deterministic ms-latency blocking path |
| Twin | Docker + overlayfs CoW snapshots, pre-warmed pool | Ground truth without a learned model |
| Intent + explainer | GPT-5.6 API (structured outputs) | LLM where it is strong; judged criterion |
| Control plane | FastAPI + WebSocket | Known stack |
| UI | Single page, xterm.js, vanilla JS | Demo-focused, no framework ceremony |
| Agent under test | Codex CLI | Hackathon requirement and build tool |

## 8. GPT-5.6 / Codex usage plan

In the product: Sol for intent-contract extraction (structured outputs, accuracy matters), Luna or Terra for divergence explanations (cheap, low stakes).

In the build: Sol for architecture decisions and the contract-checker design; Terra for boilerplate, FastAPI, Dockerfiles, UI, and all debugging; cache pinning of system prompts and core files; 3 to 5 iteration cap on autonomous fix loops (also the product's own feature). $100 credits ample; front $5 to 10 personal if credits lag (escalation already sent to build-week-event@openai.com).

## 9. Milestones (Jul 13 → Jul 21)

| Day | Deliverable |
|---|---|
| Mon 13 | Repo scaffold, chaos-core skeleton, contract schema, classifier corpus drafted. Offline (credits pending). |
| Tue 14 | Contract checker + proof traces; classifier v1; destructive/dep-change corpus tests green; loop detector. |
| Wed 15 | Interceptor shim wrapping Codex CLI; Docker twin with snapshot diffing and pre-warmed pool. |
| Thu 16 | GPT-5.6 intent extraction + divergence explainer wired; FastAPI control plane + WS events. |
| Fri 17 | Ghost UI complete: contract panel, terminals, diff pane, proof traces, approve-once. (Credits deadline 12 PM PT.) |
| Sat 18 | End-to-end hardening; scripted demo scenarios pre-tested ten times; judge-poking pass (arbitrary commands behave honestly). |
| **Sun 19** | **Record the < 3 min video.** Multiple takes, best one wins. README, setup instructions, sample repo. |
| Mon 20 | Buffer: polish, /feedback session ID, dry-run the judge test path from a clean machine. |
| Tue 21 | Submit well before 5 PM PT. |

Cut order if behind: pre-warmed container pool → plain container spawn; approve-once → block-only; explainer → static template text. The contract checker and the demo path are never cut.

## 10. Demo script (< 3 min, recorded Sun Jul 19)

1. (0:00) Hook: "You asked your agent to fix a test. It upgraded your HTTP client instead." Show a vanilla Codex session doing exactly that, lockfile churn scrolling.
2. (0:35) Same task through Tractus: contract appears from the plain-English request, one click to confirm.
3. (1:00) Agent works normally; in-scope commands flow with zero friction.
4. (1:20) The dependency upgrade attempt: contract clause flashes red, proof trace, one-sentence explanation, blocked in milliseconds. Approve-once shown as the escape hatch.
5. (2:00) Unknown script caught by the Docker twin with the real diff rendered: "even what we can't classify, we execute in the twin first."
6. (2:30) Close on the split: GPT-5.6 turns intent into structure, Rust enforces it deterministically, and Codex built the system. Highlight where GPT-5.6 and Codex were used, per judging criteria.

## 11. Risks

| Risk | Mitigation |
|---|---|
| Intent extraction produces bad contracts | User confirms/edits the contract before enforcement; extraction uses structured outputs with a fixed schema |
| Judges type commands outside the demo script | Classifier is real, twin fallback is real; unrecognized never defaults to allow |
| Twin too slow live | Pre-warmed pool, capped demo repo, and the video is recorded Sunday from the best take |
| Codex CLI hook API friction | Fallback: generic `SHELL=` shim works with any agent that shells out |
| Runaway Codex burns credits during build | Iteration caps + cache pinning + Terra for debugging |
| "Tractus" is unfamiliar | README and description define it as a scope firewall for autonomous coding agents |

## 12. Success criteria

- 100% of the scripted demo scenarios blocked/allowed correctly across ten consecutive runs before recording.
- Curated violation corpus (~30 scope-creep cases: dep changes, out-of-path edits, git overreach, network calls) fully caught; zero false allows.
- A real Codex session completes an in-scope task end-to-end through the shim without a false block.
- Twin round trip < 2 s on the demo repo; classified verdicts < 5 ms.
- Video recorded Sun Jul 19; submission (repo, README, video, Codex session ID) in before Tue 5 PM PT.
