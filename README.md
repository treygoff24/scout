<h1 align="center">scout</h1>

<p align="center"><b>Corpus hydration at retina speed.</b></p>

<p align="center">
  <a href="https://github.com/treygoff24/scout/actions/workflows/ci.yml"><img src="https://github.com/treygoff24/scout/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://crates.io/crates/scout-cli"><img src="https://img.shields.io/crates/v/scout-cli.svg" alt="crates.io"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="License: Apache-2.0"></a>
</p>

An AI agent dropped into an unfamiliar directory burns 50-200K frontier tokens and minutes of subagent time just orienting itself before doing any real work. `scout` is a CLI that does that orientation instead: it walks a directory of code or documents, builds a lightweight index with a fast open-weight model (Cerebras `gemma-4-31b`, ~1,500 tok/s), and answers "what's here" and "what does the corpus say about X" in seconds for cents.

`scout` is agent-first: one JSON envelope on stdout per command, a stable exit-code dictionary, and every delivered fact carries a verbatim quote machine-checked against the source file. The extraction model never gets to synthesize an unverified claim into a finding — if a fact can't be matched back to a real quote in a real file, it doesn't ship as a finding. That trust boundary is the whole point: cheap models hallucinate, so `scout` is built so hallucination can't survive the pipeline, not so it's merely rare.

## Install

```sh
# Homebrew (macOS/Linux)
brew install treygoff24/tap/scout

# Shell installer — prebuilt binary, no toolchain needed
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/treygoff24/scout/releases/latest/download/scout-cli-installer.sh | sh

# Cargo — builds from source
cargo install scout-cli
```

All three install a binary named `scout`. (The crates.io package is `scout-cli` because the bare `scout` name was taken.)

## Quickstart

```sh
export CEREBRAS_API_KEY=...   # get one at https://cloud.cerebras.ai

scout index ./some-project                       # build the index (spends money, resumable, incremental)
scout brief ./some-project --budget 8k            # session-start orientation: module map, entry points, hot files
scout "what does the retry policy look like" ./some-project --budget 8k
```

Every paid command prints a cost estimate before spending and asks for confirmation (`--yes` to skip). `--max-dollars N` is a hard cap; a budget-exhausted run still returns a valid envelope rather than failing.

## How it works

`scout index` walks the corpus, skips harness litter and secrets by default, and builds a two-tier card per file: a machine-verified skeleton (symbols via `ctags`, import edges, git churn, heading outlines) plus a small model-written summary explicitly labeled as a hint, never delivered as fact. Cards are hash-keyed and reused across runs — only new or changed files cost money. The index lives in `.scout/` as immutable generation directories with a single atomic `current` pointer, so a query mid-rebuild always reads one consistent generation.

`scout brief` assembles that index into a fixed-schema orientation document — module map, entry points ranked by import centrality and churn, conventions, unsupported-file coverage — with at most one model call.

`scout "<query>" <dir>` routes the query against the index to a candidate file set, extracts facts per chunk with the model, and keeps only findings whose `quote` is verified against the source file at the claimed line. Every finding is addressable (`file:line`); under a tight `--budget`, quotes can be omitted from the response but the full unabridged set is always persisted to `.scout/last-run.json`.

## Environment

| Variable | Purpose | Default |
| --- | --- | --- |
| `CEREBRAS_API_KEY` / `SCOUT_API_KEY` | API key (`CEREBRAS_API_KEY` checked first, `SCOUT_API_KEY` as fallback) | none |
| `SCOUT_MODEL` | model override | `gemma-4-31b` |
| `SCOUT_MARKDOWN_ONLY` | set `1` to index only markdown/prose files | unset |
| `SCOUT_CHUNK_MODE` | extraction chunking strategy | `ranked_boundary` |
| `SCOUT_CHUNK_CAP` | max chunks per file for ranked-boundary chunking | `24` |
| `SCOUT_CONCURRENCY` | client-side request concurrency cap | `50` |

## Cost and measured numbers

All numbers are measured, not projected — see `STATUS.md` for the full gate history.

| Milestone | Candidate-file recall | Coverage | Poison survivors | Avg cost/query |
| --- | --- | --- | --- | --- |
| M1 (code corpora) | 100% | 80.95% | 0 | ~$0.097 |
| M3 (policy/markdown corpus) | 100% | 91.30% | 0 | ~$0.127 |

Kill-test hallucination pressure (the predeclared adversarial probe that validated the extractor + quote firewall): 6.7% raw hallucination pressure on unfirewalled model output, 0% after the quote firewall. See [`docs/2026-07-05-killtest-evidence.md`](docs/2026-07-05-killtest-evidence.md).

## Exit codes

| Code | State | Meaning |
| ---: | --- | --- |
| 0 | `ok` | success envelope on stdout |
| 1 | `internal_error` | unexpected failure |
| 2 | `usage_error` | bad arguments |
| 3 | `partial` | some findings/cards missing, envelope still useful |
| 4 | `unanswered` | query had no supportable findings |
| 10 | `budget_hit` | stopped at `--max-dollars`, partial results returned |
| 11 | `index_stale` | index exists but source files changed since |
| 12 | `index_missing` | run `scout index` first |
| 13 | `provider_error` | API/auth failure |
| 14 | `tool_degraded` | an optional external tool (`ctags`/`pdftotext`/`pandoc`) is missing |

## Doctor and self-description

```sh
scout doctor --pretty          # api key presence, external tool availability, index status
scout capabilities --pretty    # commands, env vars, sensitive-file deny list, redaction rules, exit codes
scout schema --pretty          # JSON Schema-ish shape of the envelope and finding types
```

## Limitations

- The M2 hydration budget contract (`brief`/query token savings vs. cold exploration) is implemented and unit-tested, but the live paired-session A/B gate is deferred — it needs a human running matched fresh agent sessions, not something an unattended build agent can honestly self-grade. See [`docs/plans/m2-gate-predeclaration.md`](docs/plans/m2-gate-predeclaration.md).
- PDF and DOCX adapters are implemented and `doctor`-verified (detects `pdftotext`/`pandoc` and degrades gracefully when absent), but no full PDF/DOCX corpus eval has been run yet — only the markdown subset of the M3 policy corpus is gated.
- The M3 policy-corpus eval needs a corpus and goldens file you supply yourself; see [`eval/README.md`](eval/README.md).

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

The design doc — architecture, on-disk contracts, envelope state machine, and two rounds of adversarial review — is at [`docs/plans/2026-07-05-scout-design.md`](docs/plans/2026-07-05-scout-design.md).

## Provenance

scout was designed, built, reviewed, and shipped almost entirely by [Claude](https://claude.com/claude-code), coordinated through a multi-model build loop with Codex and Cursor lanes as adversarial reviewers. The human in the loop is [Trey Goff](https://github.com/treygoff24), who mostly said "yes," "ship it," and paid for the tokens. Every cost and performance number in this README was measured on real runs.

Sibling of [receipts](https://github.com/treygoff24/receipts) — same species (agent-first CLI, JSON envelope, budget discipline, mechanical verification), different corpus: local files instead of the web.

## License

Apache-2.0
