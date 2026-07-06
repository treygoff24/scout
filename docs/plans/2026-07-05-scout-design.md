# scout — corpus hydration at retina speed

**Design doc, 2026-07-05.** Validated by the volley kill-test (`docs/experiments/2026-07-05-scout-killtest.md`); this plan is the blueprint for the standalone product build in `~/Code/scout`.

## One-sentence product

A CLI that gives an AI agent fast, cheap, hallucination-firewalled orientation in any directory of code or documents — the session-start exploration that today burns 50–200K frontier tokens and minutes of subagent time, done in seconds for cents on fast open-weight inference.

## Thesis and evidence base

Frontier-agent sessions spend their slowest, most expensive phase on exploration: spawning subagents to read a repo and report back. That work is extraction-shaped — "find what's there, cite it" — which is exactly what fast small models are good at and exactly where they've been measured doing well:

- gemma-4-31b on Cerebras: "excellent fact-finder and per-claim verifier, mediocre analyst" (receipts model A/B, 2026-07-02).
- Volley kill-test (2026-07-05, predeclared criteria): 71.4% golden-fact coverage, **zero confabulation surviving the quote firewall** (0/10 poison facts, both negative probes clean), 0.5s walls per ~70K-token sweep, ~$0.10–0.21/query. One-fact coverage miss at top-K=12 lexical ranking → the router design below replaces lexical ranking entirely.

Two design laws inherited from measured failures elsewhere (volley watcher, E2/E3):

1. **The retina never synthesizes.** Judgment through a keyhole fails; per-chunk extraction is safe, cross-file narrative is not. The small model extracts; the calling agent (or deterministic assembly) synthesizes.
2. **Trust boundaries are mechanical.** Every delivered fact carries a verbatim quote checked by machine against the source (substring, whitespace-normalized). Unverifiable findings are dropped and counted, never delivered. Measured raw hallucination pressure: 6.7% of findings; measured post-filter: zero.

## Architecture: three layers, two loops

```
 SLOW LOOP (index time, incremental)
 walk ──► skeleton (deterministic) ──► cards (gemma fills semantic gaps)
                                          │
 FAST LOOP (query time, ~1-2s)            ▼
 query ──► router (1 call over thin cards) ──► extractor (parallel, per chunk)
                                                  │
                                          quote firewall (deterministic)
                                                  ▼
                                     budget-packed findings envelope
```

### Layer 1: the index (slow loop)

`scout index [dir]` walks the corpus and maintains `.scout/` (gitignored):

- **Walk hygiene is first-class.** Default deny-list (node_modules, .git, target, dist, build, caches, `*.map`, binaries), `.gitignore` respected, `.scoutignore` escape hatch. Measured motivation: a real policy corpus used for evaluation is 8,515 files of which ~60% is node_modules; hygiene failure poisons routing with junk cards, not just cost.
- **Deterministic skeleton first.** Per file, by adapter (see below): symbols via universal-ctags, import edges (→ graph centrality = entry-point prior), git churn (hot files), section outlines for prose, path semantics (dir names like `01-current`/`90-archive` carry recency/role for free). Zero model calls. Everything mechanically extractable is a hallucination class deleted.
- **Cards, two tiers, hash-keyed.** For each file whose content hash is new: one gemma call fills only what machines can't — role one-liner, key invariants/gotchas, notable constants. Card = machine part (verbatim, trusted) + model part (labeled). Tiers:
  - **thin line** (~25 tok): path + role one-liner → the router's working set.
  - **fat card** (~150 tok): + symbols, terms, outline, invariants → extraction guidance.
- **Incremental by construction, invalidation by manifest.** A card is reused only when ALL its inputs are unchanged — not just file content. `.scout/manifest.json` records: card schema version, card-prompt hash, model id, adapter + external-tool versions (ctags etc.), ignore-config hash, and per-card the content hash + relative path. Any manifest-level change invalidates the affected cards (schema/prompt/model change → full re-index; ctags upgrade → code cards only). Graph-derived fields (import centrality, churn) are recomputed deterministically every index run — they're cheap and never cached stale.
- **Snapshot mechanism (load-bearing — do not improvise).** Per-file rename is atomic per inode, NOT across a multi-file index; per-card renames would hand a mid-rebuild query a mixed-generation index. Therefore: each index build writes a complete generation directory `.scout/gen-<n>/` (cards + manifest), then atomically renames a single `current` pointer file to it. A query resolves `current` exactly once at startup and reads only that generation; rebuilds never mutate an existing generation; old generations are pruned after the pointer moves. Named concurrency test: a query issued while a rebuild is in flight returns results consistent with exactly one generation. Day-two refresh of a working corpus costs pennies. Initial index of a large corpus costs real money: `scout index` prints a cost estimate derived from actual walked token counts BEFORE spending, and asks (`--yes` to skip).
- **Model card fields are hints, never facts.** Card generation asks the model for role/invariants/gotchas — that is synthesis, and it is not quote-firewalled. The design holds the line by scope, and the envelope enforces it: model-written card fields are (a) routing/guidance metadata internally, and (b) wherever they surface to the caller (`brief`), tagged `model_hint: true` and rendered separately from machine-extracted fields (symbols, outlines, churn — verbatim, trusted). Card quality gets its own eval (see Testing) because bad cards silently starve recall.

### Layer 2: the router (fast loop, one call)

The thin-card index for ~1,300 files ≈ 32K tokens — fits ONE gemma call (131K paid-tier context, 40K max output, per volley NOTES.md; the post-hygiene policy corpus fits without two-hop). Router input: full thin-card list + query. Output (strict JSON): candidate files ranked, plus "what to look for in each." The hypothesis is that this beats the measured K=12 lexical-ranking coverage miss because the router sees every file's semantic one-liner — but **router recall over cards is the plan's central unvalidated assumption** (the kill-test's only passing configuration was exhaustive extraction). Two mitigations are design requirements, not options:

1. **Candidate union.** The extractor's file set = router candidates ∪ deterministic candidates (lexical term hits, ctags symbol matches, path matches). The router can only ADD recall relative to the dumb baseline, never silently subtract it.
2. **Router-only eval gates M1** (see Testing): for every golden fact, its source file must appear in the routed candidate set (recall@N). If measured recall doesn't clear the exhaustive baseline's coverage, the router is demoted to a re-ranker over the union until it does.

Corpora whose thin index exceeds the context budget route in two hops: dir-level roll-up cards → file cards within chosen dirs. Router failure (wrong files) costs recall, never correctness — routing metadata is never delivered as fact.

**The M1 tension, named.** Guaranteeing no golden-fact file is lost may require a wide deterministic union; K=12 is measured-too-narrow, exhaustive is measured-too-expensive ($0.15–0.21). The bet is that the semantic router recovers what narrow lexical missed without widening toward exhaustive. If measurement says both bars can't fall at once, the predeclared fallback ladder, in order: (1) IDF-weight the deterministic ranking (rare terms beat corpus-saturated ones — directly targets the measured 1b miss), (2) widen union K to 16–20 (projected ~$0.13), (3) router as re-ranker over a wide-but-capped union. Decision rule: take the first rung that clears BOTH recall (no golden-fact file lost) and cost (< $0.15/query); if none does, the cost bar moves, not the recall bar — a cheap tool that misses files is worthless.

### Contracts for the net-new components (a fresh session must not guess these)

**Router I/O (JSON, strict).** Input: `{"query": str, "cards": [{"path": str, "role": str}]}` (thin tier only). Output: `{"files": [{"path": str, "look_for": str, "rank": int}]}` — `look_for` is passed into the extractor prompt for that file's chunks as guidance; paths not in the input card list are discarded (router cannot invent files). Extractor set = router `files` ∪ deterministic candidates.

**Card schema (v1).** Machine part (trusted, verbatim): `path`, `hash`, `adapter`, `symbols[]` (ctags), `imports[]`, `outline[]` (prose headings), `churn` (commits last 90d), `loc`, `harness_meta` (bool: CLAUDE.md/memory/.claude-class). Model part (hint tier, `model_hint: true` wherever caller-visible): `role` (one-liner ≤ 20 words — this is also the thin-tier line), `invariants[]`, `gotchas[]`, `terms[]` (prose defined-terms). Card JSON schema version field pins migration.

**`.scout/` layout.**
```
.scout/
  current            # pointer file: name of active generation (atomic rename target)
  gen-<n>/
    manifest.json    # versions, prompt hash, model id, tool versions, ignore-config hash
    cards.jsonl      # one card per line, both tiers
  last-run.json      # full unpacked findings of the most recent query (mandatory write)
  lock               # index-build lockfile; queries never take it
```

### Layer 3: the extractor + firewall (fast loop, parallel)

Ported from the validated volley prototype (shipped in this repo as `reference/prototype.py`, with its goldens as `reference/goldens.json` — golden line numbers reconciled against source at copy time):

- Top routed files → chunks on **semantic boundaries** (functions/classes from ctags for code; headings for prose — measured fine-grain misses in the kill-test were plausibly blind-window boundary casualties) with line numbers.
- Parallel gemma calls, temp 0, strict JSON `{fact, line, quote}` per chunk, "[] is the correct answer for most chunks" honesty clause (measured: both negative probes clean).
- **Mechanical quote firewall**: verbatim whitespace-normalized quote must exist in the cited file, cited line within ±5 of the quote's true location. Dropped findings are counted in the envelope (`dropped`, with reasons) — the caller sees how hard the firewall worked.

### The wire boundary: secrets never leave the machine

Scout ships local file content to a third-party inference provider. That is a real trust boundary and gets receipts-grade discipline (and the watcher's battle-tested patterns — volley `watcher/redact.py` is the reference implementation):

- **Default-deny sensitive files at the walk**: `.env*`, key/cert material (`*.pem`, `*.key`, `id_rsa*`), credential stores, cloud config (`.aws/`, `.config/gcloud/`), SSH dirs, password exports. Denied files appear in the envelope as `skipped: sensitive` so the agent knows they exist without their content moving.
- **Redaction pass on every outbound chunk**: known token formats (sk-, AKIA, ghp_, JWT, xox*…), key=value secret patterns, bearer tokens, high-entropy strings (with the measured 2+-slash path exemption), home-path → `~/` rewrite. Port of redact.py, which survived a live day-run.
- `--allow-sensitive` exists only for local-provider setups and requires interactive confirmation.
- `scout capabilities` discloses exactly what classes of content leave the machine.

### Hydration mode (no query)

`scout brief` — the session-start product. Mostly deterministic assembly OF the index: module map grouped from thin cards, entry points from import centrality, hot files from churn, conventions/harness metadata (CLAUDE.md, memory/, .claude/ tagged by the skeleton and surfaced prominently — they are pre-digested orientation). At most one model call (labeled). Fixed schema, no free narrative. This is "context hydration": the agent starts knowing the map instead of exploring for it.

## Format adapters (per-FILE, not per-corpus)

Measured motivation: real working directories are hybrid — a real policy corpus used for evaluation interleaves markdown, PDFs, Python, and toolchain code. Adapter chosen per file:

| Adapter | v1 | Skeleton | Chunking |
|---------|----|----------|----------|
| code (ctags langs) | ✅ | symbols, imports, churn | function/class boundaries |
| markdown/plaintext | ✅ | heading outline, links, defined terms | heading boundaries |
| PDF | fast-follow | pdftotext + outline | page/heading |
| docx | fast-follow | pandoc → md pipeline | heading boundaries |

The adapter trait is the seam: `skeleton(file) -> Skeleton`, `chunks(file) -> [Chunk]`. Two adapters from day one keeps the seam honest; PDF/docx (~30% of the measured policy corpus) bolt on after the seam is proven.

## CLI surface and envelope

Agent-first, receipts-conventions: JSON envelope on stdout, human `--pretty` optional, exit codes meaningful (0 ok, 10 budget hit, 11 no index), `scout capabilities` self-describes.

```
scout index [dir] [--yes] [--max-dollars N]     build/refresh the index
scout "query" [dir] [--budget 2k|8k|32k] [--max-dollars N]
scout brief [dir] [--budget ...]                 hydration, no query
scout capabilities | scout schema                machine contract
```

**Token-budget contract (the caller-side economics).** Principle: **coverage-complete, depth-shallow, pointer-dense.** Every finding carries file:line; compression drops prose, never addressability. Packing order: router-ranked findings first (by `rank`), then deterministic-only findings (files the router didn't score) ordered by deterministic score — fully deterministic packing, no unscored ambiguity. Verbatim quotes included until the budget is spent, bare pointers after. A quote-omitted finding is explicitly marked (`quote_omitted: true`) because a model-written fact string without its quote has lost its verification surface — and persisting the FULL findings to `.scout/last-run.json` (path in the envelope) is mandatory, not best-effort, so the omitted evidence is always one local read away. This dissolves the "more tokens is marginally richer" tension: depth is one targeted Read away, and scout's job is to make that Read targeted.

**Envelope state machine.** Agents need deterministic failure semantics, not prose errors. Every response reports one of: `ok`, `partial` (some chunks failed/rate-limited; per-file skip reasons included), `unanswered` (honest zero findings — a first-class success state, per the measured negative probes), `budget_hit` (spend or token cap; what was covered before the stop), `index_stale` (index exists but manifest mismatches; hint: `scout index`), `index_missing`, `provider_error` (after retries; retry counts included), `tool_degraded` (ctags/pdftotext missing; which skeleton fields are absent). Exit codes map 1:1. Concurrent `index` runs take a lockfile; queries never block on it (old snapshot). Every envelope carries spend, token counts, and timing — receipts conventions.

> **Amendment 2026-07-06 (hardening campaign, commits f3ba5aa..93462b4):** two states added to the machine — `usage_error` (bad args/flags/unknown command; exit 2) and `internal_error` (unexpected failure; exit 1) — so no error path can masquerade as `provider_error`. The full 1:1 table (ok=0, partial=3, unanswered=4, budget_hit=10, index_stale=11, index_missing=12, provider_error=13, tool_degraded=14) is published machine-readably by `capabilities`/`schema`. `partial` is scoped to genuine chunk/provider failures; quote-firewall drops are reporting, not degradation. The firewall gained a second mechanical tier (`match_tier: markdown_normalized`, prose adapters only, same ±5 line check) — verbatim-modulo-emphasis, never fuzzy. Query/brief gained `--refresh`, `--compact`, `weak_signal` (ratio-based), and a `.scout/runs/` envelope history; `last-run.json` now stores the full envelope. Measured effect: M3 coverage 82.61 → 91.30 with negatives still clean.

**Model provider layer.** Cerebras first (validated numbers), but the provider trait is thin chat-completions — Groq/Fireworks/local are config, not code. `SCOUT_MODEL`, `SCOUT_API_KEY`/provider-specific env. Never require a frontier key.

## Language and repo

**Rust.** Same reasons as receipts (single static binary, agent-first CLI conventions already proven, fast deterministic layers at 8K-file scale) plus direct pattern reuse: envelope/budget/provider modules in receipts are the templates. External tools (ctags, pdftotext, pandoc) are subprocess calls behind graceful degradation — missing ctags degrades the code skeleton to regex symbols, never blocks. `scout doctor` reports what's installed.

The validated Python prototype (volley's `scout.py`) and its goldens are copied into the repo at init as `reference/prototype.py` and `reference/goldens.json` — executable spec, runnable, alongside this doc.

## Testing and eval

Golden-fact methodology carried over (it out-performed summary-matching in the kill-test and caught real defects), but **split by subsystem** — the new risks are cards, routing, and chunking, and an end-to-end score can't localize which one failed:

- **Router recall eval** (gates M1): for every golden fact, its file appears in the FINAL extractor candidate set (router ∪ deterministic — the union, not router output alone). Threshold: 100% of golden-fact files present — routing recall upper-bounds end-to-end coverage, so any lost file directly craters coverage below the extraction ceiling. Router demoted to re-ranker if the union needs it.
- **Union superset property test**: for arbitrary router output — including garbage, empty, or invented paths — the final extractor set ⊇ deterministic candidate set. Cheap property test; it IS the router's safety guarantee, so it gets asserted directly, not implied.
- **Card eval**: role one-liner accuracy (human-judged sample), no unsupported invariant claims, and a corruption probe — routing recall re-measured with model card fields blanked, to quantify how much recall actually depends on unverified card content.
- **Chunking A/B** (gates the semantic-chunker switch): fixed 350-line windows vs boundary-aware vs boundary+overlap, scored on golden coverage, quote-line accuracy, and cost. The blind-window prototype numbers are the floor; semantic chunking ships only if it measurably wins.
- **Extractor eval**: the kill-test design as-is — must-find facts + poison facts + negative probes. Volley goldens seed it.
- **Held-out discipline**: any goldens used to tune prompts/router are dead for gating; each gate runs on goldens the tuning never saw (the policy-corpus subset stays held out until M3).
- Negative probes are non-negotiable in CI — confabulation on "what does the bill say about X (nothing)" is the catastrophic failure for policy work.
- **Addressability test**: over a budget-truncated envelope, every finding still carries file:line, every `quote_omitted: true` finding resolves in `last-run.json`, and `last-run.json` contains the full un-omitted set. (Packing *order* is a separate, weaker property.)
- **Generation-consistency test**: query during an in-flight rebuild returns results from exactly one generation.
- Unit level: firewall (quote matching, line mapping), walk hygiene, redaction, manifest invalidation, adapter skeletons, packing order — pure functions, no API needed.
- Deterministic-by-default: temp 0 everywhere; eval runs assert stability.

## Cost model

Formulas, with the honest labels — measured numbers are from the kill-test, everything else is projection to be validated at M1:

- Query cost ≈ (thin-index tokens: router input) + (routed chunk tokens × input rate) + output + retry margin.
- Index cost ≈ Σ per-new-file (file tokens + card prompt overhead) — computed from ACTUAL walked token counts and shown before spending.

| Op | Cost | Status |
|----|------|--------|
| query, exhaustive extraction, ~70K-tok corpus | $0.15–0.21/sweep, 0.5s wall | **measured** (kill-test baseline; note: this config *failed* the $0.15 bar, which is the whole motivation for the index) |
| query, top-K=12 lexical | $0.09–0.12/sweep | **measured, failed coverage** — floor for what routed extraction costs, not evidence it works |
| query, index-backed router | router ~1 call over thin index + K-ish chunks | **projected — M1 gate measures it** |
| initial index | Σ file tokens ≈ $2.15/MTok input | projected; estimate printed pre-spend |
| incremental refresh | changed files only | projected |

Rate ceiling is the provider window (500K tok/min on Cerebras), not the model. Comparison point: one frontier explore-subagent pass ≈ $0.50–3 and 1–3 minutes.

**Concurrency and backoff (measured requirement, not a detail).** The kill-test's worst wall (20.6s vs 0.5s typical) was a 429 from pushing 500K tokens inside the per-minute window. The Rust extractor ships from day one with: client-side concurrency cap (default 50, configurable), jittered exponential backoff on 429/5xx with a retry budget, and the mapping — retries exhausted on some chunks → envelope `partial` with per-chunk reasons; provider down entirely → `provider_error` with retry counts. The prototype's fixed `workers=20`, no-jitter loop is NOT the spec; this paragraph is.

## Milestones

1. **M1 — engine on code corpora**: walk+hygiene+redaction, ctags/markdown skeletons, two-tier cards + manifest invalidation + generation snapshots, router with candidate union, extractor+firewall port, query command, envelope state machine, concurrency/backoff policy. Gate (all measured, not projected): 100% of golden-fact files present in the final candidate union (see Testing — NOT the 71.4% coverage number, which is an extraction metric on a different axis); end-to-end coverage ≥ kill-test baseline (71.4%) at < $0.15/query on the same corpora; negatives clean; chunking A/B run and the winner shipped; union-superset property test and generation-consistency test green.
2. **M2 — hydration + budget contract**: brief command, budget packing with `quote_omitted` semantics, mandatory last-run persistence, capabilities/schema/doctor. Gate, predeclared to kill-test standard before running: pick the repo and task in advance (task must have an objective completion check), N≥3 paired runs (hydrated vs control) to bound session variance, threshold ≥30% fewer exploration-phase tokens, token-accounting rule fixed in advance — scout's own brief/query output tokens COUNT AGAINST the hydrated session (the map isn't free), exploration phase ends at the first mutating tool call. All four parameters written down before run one; anything else is vibes.
3. **M3 — prose corpus proof, honestly scoped**: goldens for the *markdown subset* of a real policy corpus (held out until now), markdown adapter hardening, defined-terms extraction. The envelope reports unsupported-file coverage ("257 PDFs present, not indexed — adapter pending") so the gate can't silently claim corpus coverage it doesn't have. Gate: policy-markdown goldens pass; negative probes clean on legal-flavored queries; unsupported-coverage reporting verified.
4. **M4 — fast-follows**: PDF/docx adapters (then re-run M3's corpus with full coverage), two-hop routing for huge corpora, provider capability matrix (each provider/model gated through the same goldens before being declared supported — context window, JSON mode, pricing, retry taxonomy as first-class metadata, not config strings), packaging/distribution (crates.io + install one-liner + skills file, receipts playbook).

## Open questions (deliberately deferred)

- Card schema versioning / index migration story (v1: version field + full re-index on breaking change).
- Whether `brief` warrants one synthesis call on frontier-via-caller instead of gemma (v1: no — deterministic assembly only).
- Embeddings as a router assist (v1: no — one-call router over thin cards is simpler and measured-adequate at target scale; revisit only if two-hop routing proves weak).
- Multi-corpus federation (code repo + policy dir in one session). v1: run scout twice.
