# Scout kill-test: gemma as repo-exploration retina (predeclared 2026-07-05, before build)

## Idea

"receipts, but the corpus is a local file tree." A CLI Claude can call at session start:
query + directory → parallel gemma extraction over file chunks → structured findings with
file:line citations. Gemma is the retina only — per-chunk fact extraction; synthesis stays
with the caller (the watcher lesson: judgment through a keyhole fails).

Hallucination firewall, ported from receipts and *stronger* locally: every finding must carry
a verbatim quote, mechanically checked against the actual file (substring match, zero model
calls). Unverifiable findings are dropped and counted.

## Question to kill

Can gemma-4-31b, given a query + raw line-numbered file content, (a) find the facts that are
there, (b) not assert facts that aren't, (c) survive the negative probe — a query about
something that doesn't exist in the corpus?

## Design (locked)

- MVP: `scout.py` in volley — walk dir (text files, skip .git/caches/binaries), chunk ~350
  lines with line numbers, one gemma call per chunk at temp 0, strict JSON out:
  `[{fact, line, quote}]` or `[]`. Mechanical verify: whitespace-normalized quote must appear
  in the cited file, cited line within ±5 of the quote's true location.
- Corpora: `volley/watcher/` (Python, ~8 files) and `~/Code/recon/src/` (Rust, unfamiliar
  shape to the prompts). Both explored BY HAND by the coordinator (Fable) to write goldens
  before scout ever runs on them.
- Goldens per positive query: 4-8 must-find facts (each pinned to file), 2-3 poison facts
  (plausible, sound like this codebase, but false). 2 negative probes (features that don't
  exist). 6 queries total: 2 positive + 1 negative per corpus.

## Predeclared criteria

KEEP (all must hold):
1. **Coverage** ≥ 70% of must-find facts surfaced across positive queries (fact counted if
   any surviving finding states it, right file).
2. **Post-filter precision**: ≤ 1 surviving finding across the whole run that is misleading
   or false as stated (a real verbatim quote with a wrong fact attached — the failure the
   mechanical filter can't catch).
3. **Zero poison facts** asserted in surviving findings.
4. **Negative probes**: both return zero surviving findings, or only findings that honestly
   say the thing isn't there.
5. **Speed**: full-corpus sweep per query < 60s wall.
6. **Cost**: < $0.15 per query-sweep; whole kill-test < $2.

KILL if: coverage < 50%, OR ≥ 3 misleading survivors, OR any poison fact asserted, OR a
negative probe confabulates a surviving finding. Middle ground → one prompt-iteration pass
allowed (documented), then re-run once; still failing → kill.

Also measured (not gated): raw pre-filter hallucination rate (how hard the firewall works),
tok/s, per-query cost. If the test passes, THEN the Codex hill-climb on prompt + golden
expansion is justified; if it fails at MVP grain, the hill-climb dies with it.

Budget note: repo cumulative ≈ $41 of $50. Hard stop for this experiment: $3.

---

# RESULTS (2026-07-05, same session — run after the above was locked)

Artifacts: `scout.py` (MVP, ~130 lines), `docs/experiments/scout-goldens.json` (hand-written
before scout ran on those queries), raw outputs `scratch/scout-killtest-results.json`.
Smoke queries used during build ("interrupt vs nudge", "API key") were retired as
contaminated; all 6 scored queries were fresh.

## Scorecard against predeclared criteria

| # | Criterion | Result | Verdict |
|---|-----------|--------|---------|
| 1 | Coverage ≥ 70% | **15/21 = 71.4%** (w-redact 5/6, w-tail 4/6, r-budget 3/4, r-verify 3/5) | PASS (barely) |
| 2 | ≤ 1 misleading survivor | 1 borderline: windower.py NOISE_TYPES *skip* described as "redacts" — effect-equivalent, wrong verb | PASS |
| 3 | Zero poison facts asserted | 0 of 10 | PASS |
| 4 | Negative probes clean | both returned **zero survivors** (websockets, SQLite) | PASS |
| 5 | Speed < 60s/sweep | 0.3–1.2s typical; 20.6s worst (a 429 backoff — we pushed 500K tok inside the per-minute window) | PASS |
| 6 | Cost < $0.15/sweep, < $2 total | total **$1.10** PASS; per-sweep **FAIL**: $0.153–0.215 (cost ∝ corpus size; recon/src sweeps $0.21) | SPLIT |

**Verdict: SURVIVES.** No kill condition tripped (coverage ≥ 50%, < 3 misleading, no poison,
no negative confabulation). One keep-criterion miss (per-sweep cost), mechanism understood —
scout ships every chunk to the model, so cost scales linearly with corpus bytes regardless of
relevance. Fix is architectural, not prompt-level: a zero-cost chunk prefilter (keyword/grep
prescreen from the query, or a two-stage cheap-relevance pass) before extraction. That's the
first product iteration, not a reason to kill.

## What the run taught us

- **The mechanical quote firewall is the whole ballgame.** Raw hallucination pressure was
  real but small: 5 of 75 findings dropped (6.7%) — 2 paraphrased quotes on true facts,
  2 line-number misses, 1 unparseable. Post-filter, zero fabricated facts survived. The
  firewall trades recall for precision (it killed a TRUE TOCTOU finding whose quote was
  hand-mangled from budget.rs's doc comment) — the right trade for a context-feeder.
- **Misses cluster at fine grain.** All 6 missed facts were detail-level (len=N placeholder
  format, 5s poll constant, OnceLock stickiness, judge-prompt wording, EOF-attach, paranoid
  policy). Architecture/flow facts were found reliably. For "orient Claude in a repo," that's
  the right failure direction.
- **Gemma out-golded the golden author on r-budget**: exit code 10 on budget hit, per-stage
  worst-case-cost gates, dedup verdict ranking — true, relevant, not in my hand-written set.
  33 verified findings on that sweep vs my 4 golden facts.
- Negative-probe honesty at temp 0 with the "[] is the correct answer for most chunks"
  prompt line: both probes clean. The E2-era confabulation trap did not fire.
- Speed: ~70K tok corpus sweeps in 0.5s wall at 20-way concurrency. The rate limit
  (500K tok/min), not the model, is the throughput ceiling for whole-repo sweeps.

## Next (if pursued)

1. Chunk prefilter to kill the cost criterion miss (and raise the rate-limit ceiling).
2. Coverage hill-climb (Codex-driven prompt iteration against these goldens + expanded set)
   — target the fine-grain miss cluster. Goldens format already supports it.
3. Only then: package as a real CLI Claude can call at session start.

Spend: $1.10 kill-test + ~$0.31 smoke = **$1.41 this experiment**. Repo cumulative ≈ $42.4 of $50.

---

# Iteration 1: chunk prefilter (predeclared 2026-07-05 before running)

Change: zero-model lexical prefilter — stopword-stripped query terms (≥3 chars), a chunk is
sent to gemma only if its lowercased body substring-matches ≥1 term. Same goldens, same 6
queries, same scoring. Declared BEFORE the re-run:

KEEP the prefilter if: coverage stays ≥ 70% (no golden fact found in run 1 may be lost to a
skipped chunk without being counted against this), both negatives stay at zero survivors,
and per-sweep cost < $0.15 on all six sweeps. Measure: cost reduction factor, chunks
skipped, walls. Risk being tested: lexical mismatch between query vocabulary and code
vocabulary silently starving coverage.

**RESULT 1a: FAIL on its own cost bar.** Skipped only 1-9 of ~23-29 chunks/sweep; $1.03
total (-6%); recon sweeps still $0.205-0.212. Findings byte-identical to run 1 (coverage
preserved trivially — the filter barely filtered). Mechanism: binary ≥1-term substring
match is too permissive; generic query words (run, cache, receipts, text) hit nearly every
chunk. Superseded by 1b.

## Iteration 1b: ranked top-K (predeclared 2026-07-05 before running)

Change: score every chunk = count of DISTINCT stemmed query terms (crude suffix strip:
ing/ed/es/s) matched at a word boundary (prefix match, so "transcript" hits "transcripts");
tiebreak by total hits. Send only the top K=12 chunks with score ≥ 1. Cost is now capped by
construction: ≤ K chunks/sweep regardless of corpus size (~$0.08 at current chunk size).

KEEP if: same three conditions as 1a (coverage ≥ 70%, negatives zero, all sweeps < $0.15).
Budget: experiment is at $2.44 of its $3 hard stop — 1b gets ONE full run (~$0.55), no
re-roll. If it fails, document and stop; the next lever (smarter ranking, IDF, K tuning)
waits for a fresh budget line.

**RESULT 1b: FAIL on coverage, by one fact.** 14/21 = 66.7% (bar: 70). Cost and negatives
passed decisively: all sweeps $0.094–0.12, total $0.63 (-43% vs baseline), both negatives
zero survivors, walls ≤ 1.0s. The lost fact: r-budget F4 (adaptive verify escalation is
budget-gated, verify.rs:373) — for the budget query, verify.rs chunks ranked below
budget.rs/ask.rs/mod.rs and fell just outside K=12. w-redact, w-tail, r-verify findings
were identical to the unfiltered run.

Per predeclaration: prefilter stays DEFAULT OFF in scout.py; no re-roll on this budget.
The cost-coverage frontier is now measured: K=∞ → $0.21/sweep at 71.4%; K=12 → $0.10 at
66.7%. The gap is one rank position, so K=16-20 (or IDF-weighting rare terms like
"escalation" over corpus-saturated ones like "budget") very likely clears both bars at
~$0.13 — untested, next budget line.

**Honest ledger note:** experiment closed at **$3.07 of its $3 hard stop** — $0.07 over.
The error was running 1a's full sweep without pricing it first (its mechanism predictably
barely filtered). Logged as process defect, not hidden. Repo cumulative ≈ $44.1 of $50.
