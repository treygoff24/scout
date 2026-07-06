# scout build status

Internal build/eval status record kept during development. Left as-is post-ship for provenance — read the entries below as a chronological log of what was measured during the build, not a live dashboard.

Current phase: shipped. 0.1.0 published to crates.io (`scout-cli`) and the `treygoff24/homebrew-tap` Homebrew tap (`brew install treygoff24/tap/scout`), with tag `v0.1.0` pushed and released on GitHub. Post-review hardening campaign completed before ship (2026-07-06). M1 re-verified green post-refactor (recall 100, coverage 80.95, poison 0, $0.5839). M3 re-verified green with IMPROVED coverage 91.30 (was 82.61; markdown-normalized quote tier recovered false drops), negatives clean, $0.6747. Three reviewed waves: trust/state-machine (f3ba5aa), robustness (b2ba48f), agent-UX (d155630). 46 unit tests. See docs/dogfood/2026-07-06-journal.md for the driving findings and scorecard.

## Gates / numbers

- Local deterministic checks: `cargo fmt --check` PASS, `cargo clippy -- -D warnings` PASS, `cargo test` PASS (46 tests as of 93462b4; 17 at initial build), `cargo install --path .` PASS.
- M1 live gate attempt 1 (boundary_overlap): FAIL cost; candidate-file recall 100.0%, coverage 95.24%, poison survivors 0, avg $0.4687/query, spend $2.8122.
- M1 chunking A/B fixed-window variant: PASS under original ±8 eval matcher; candidate-file recall 100.0%, coverage 71.43%, poison survivors 0, avg $0.1428/query, spend $0.8570.
- M1 post-review default rerun with stricter ±5/fact-aware coverage matcher: FAIL coverage; candidate-file recall 100.0%, coverage 66.67%, poison survivors 0, avg $0.1428/query, spend $0.8570. Failure analysis: fixed windows meet cost but drop too much extraction recall; boundary_overlap preserves recall but misses cost.
- M1 ranked-boundary selector rerun: PASS; candidate-file recall 100.0%, coverage 80.95%, poison survivors 0, avg $0.0638/query, spend $0.3830. Ranked-boundary shipped as default (`SCOUT_CHUNK_CAP` default 24).
- M1 final completion-audit rerun on final code: PASS; candidate-file recall 100.0%, coverage 80.95%, poison survivors 0, negatives_clean true, avg $0.0973/query, spend $0.5839, artifact dir `.scout/eval-runs/m1-1783308263332`.
- M2 deliverables: VERIFIED locally; `brief` ok, `quote_omitted` budget packing ok (20/20 omitted under tiny budget), `.scout/last-run.json` persisted full findings, `capabilities`/`schema`/`doctor` smoke-tested. Live paired-session gate: DEFERRED-TO-HUMAN; predeclaration written at `docs/plans/m2-gate-predeclaration.md`.
- M3 full markdown gate after poison-matcher fix but before exact-claim filter: FAIL negative probe; candidate-file recall 100.0%, coverage 82.61%, poison survivors 0, negatives_clean false, avg $0.1265/query, spend $0.6327, artifact dir `.scout/eval-runs/m3-1783307823301`. Root cause: the negative query returned a partial heading match (`PACT Compact: blank check`) that did not support the full asserted `blank-check foreign-aid grant program` claim.
- M3 targeted negative after exact-claim filter: PASS; candidate-file recall 100.0%, coverage 100.0%, poison survivors 0, negatives_clean true, avg $0.1138/query, artifact `.scout/eval-runs/m3-1783307919473/pact-negative-blank-check.json`.
- M3 full markdown gate after exact-claim filter: PASS; candidate-file recall 100.0%, coverage 82.61%, poison survivors 0, negatives_clean true, avg $0.1265/query, spend $0.6327, artifact dir `.scout/eval-runs/m3-1783307924635`.
- Cursor safe review of M3 fix (`cursor-3`): folded two real findings. Exact-claim matching now uses quote text only (not model-written `fact`) and records filtered hits as `partial_claim_match` drops.
- M3 targeted negative after review fold: PASS; candidate-file recall 100.0%, coverage 100.0%, poison survivors 0, negatives_clean true, avg $0.1138/query, artifact `.scout/eval-runs/m3-1783308182960/pact-negative-blank-check.json`.
- M3 final full markdown gate after review fold: PASS; candidate-file recall 100.0%, coverage 82.61%, poison survivors 0, negatives_clean true, avg $0.1265/query, spend $0.6327, artifact dir `.scout/eval-runs/m3-1783308183980`.
- M3 post-hardening re-verification (2026-07-06, after the three review waves): PASS with IMPROVED coverage; candidate-file recall 100.0%, coverage 91.30%, poison survivors 0, negatives_clean true, avg $0.1349/query, artifact dir `.scout/eval-runs/m3-1783350425981`. The markdown-normalized quote tier recovered previously false-dropped quotes — source of the 82.61 → 91.30 improvement cited in the summary above.
- M4 adapter seam: VERIFIED locally by `cargo run -- doctor --pretty`; `ctags`, `pdftotext`, and `pandoc` all reported present, with `degraded.code_symbols=false`, `degraded.pdf=false`, `degraded.docx=false`. PDF/docx adapters are behind the same `adapter_for`/`read_adapter_text` seam and gracefully report unsupported/tool-missing files in index skips.

## Spend

- Running API spend: $10.5194 initial build; ~$2.0 added 2026-07-06 (post-refactor m1+m3 regression $1.26 + dogfood queries/re-indexes) ≈ $12.5 total.
- User-updated hard cap: $20.00.
- Original stop-and-flag threshold: $8.00; user approved continuing beyond it.
- Remaining headroom under the $20 cap: about $7.5.

## Decisions

- Implemented a compact single-binary Rust CLI in `src/main.rs` rather than scaffolding modules first; pure seams are still unit-tested.
- M1/M3 eval suites were reconciled to current absolute paths before first use.
- Added per-query eval artifacts and `--only` to preserve failure evidence without re-running full suites.
- Kept the quote firewall unchanged; the initial M3 first-query failure was an eval poison-matcher false positive on truthful counterclaims like "enterprise value rather than just invested capital."
- Added a general exact-claim filter for `Where does the corpus say ...?` queries so partial term hits are not delivered as answers to nonexistent legal claims. After review, it only trusts verified quote text, not model-written fact strings.

## Open blockers / deferred

- M2 live paired-session A/B gate remains DEFERRED-TO-HUMAN by design; scout's deliverables and predeclaration are complete, but the live Claude-session comparison cannot be honestly run inside this agent session.
- M4 PDF/docx adapters are implemented and doctor-verified, but no full-PDF/docx policy corpus eval was predeclared or run in this build.

## Human verification commands

```bash
cargo fmt --check
cargo clippy -- -D warnings
cargo test
cargo install --path .
SCOUT_CONCURRENCY=20 cargo run -- eval m1 --max-dollars 2.00 --yes --pretty
SCOUT_CONCURRENCY=20 cargo run -- eval m3 --max-dollars 2.00 --yes --pretty
cargo run -- doctor --pretty
```
