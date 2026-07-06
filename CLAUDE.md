# scout — working agreements

Read `docs/plans/2026-07-05-scout-design.md` FIRST. It is the source of truth: architecture, on-disk contracts, envelope state machine, milestone gates. It survived two adversarial review rounds (Codex + opus plan-reviewer); do not silently deviate — if implementation reveals the plan is wrong somewhere, say so out loud and update the plan in the same commit.

## Non-negotiables (from the plan, enforced here)

- **The retina never synthesizes.** gemma extracts per-chunk facts; synthesis belongs to the caller or deterministic assembly. Model-written card fields are hints (`model_hint: true` wherever caller-visible), never delivered facts.
- **Trust boundaries are mechanical.** Every delivered fact carries a verbatim quote machine-checked against the source file. The union-superset property (extractor set ⊇ deterministic candidates, for arbitrary router output) is a test, not a comment.
- **Secrets never leave the machine.** Default-deny sensitive files at the walk + redaction pass on every outbound chunk (`reference/` has the battle-tested patterns in volley's redact.py lineage). This ships in M1, not later.
- **Snapshot mechanism as specced.** Generation dirs + single atomic `current` pointer rename. Per-card renames are a known-wrong design; the plan explains why.
- **Milestone gates are measured, not vibed.** Predeclare before running, report numbers, and a failed gate is a finding to write down, not to quietly re-run. Cost estimates print BEFORE spend.

## Practicalities

- API key: `CEREBRAS_API_KEY` env (prefer an env var injected by your shell profile over any `.env` file).
- Rate limits (verified 2026-07-01): gemma-4-31b 500 req/min, 500K tok/min. Client-side concurrency cap ~50, jittered backoff on 429 — the prototype's bare 20-worker loop is NOT the spec.
- Set a real User-Agent on all HTTP calls (default UAs get Cloudflare-403'd; bit us before).
- `reference/prototype.py` is runnable executable spec: `python3 reference/prototype.py "query" <dir>`. When a Rust behavior differs from the prototype, the plan decides which is right.
- Rust conventions: mirror [receipts](https://github.com/treygoff24/receipts) for envelope/budget/provider module patterns — it's the proven sibling (same envelope contract, exit-code dictionary, and agent-first CLI conventions).
- Eval spend needs a per-run cost counter and a declared cap before any golden sweep.
