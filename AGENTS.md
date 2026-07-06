# scout — agent guide

You are probably an AI agent setting this up for a human, or using it yourself. This file is the complete contract. The README is for humans; everything you need is here. If anything here disagrees with `scout capabilities --pretty` or `scout schema --pretty`, trust those — they're generated from the code.

## What this tool does

`scout` gives an AI agent fast, cheap, hallucination-firewalled orientation in a directory of code or documents. `scout index` walks the corpus and builds a two-tier card per file (a machine-verified skeleton plus a small model-written summary, always labeled a hint). `scout brief` assembles the index into a fixed-schema orientation document in at most one model call. `scout "<query>" <dir>` routes a natural-language question against the index and returns findings where every delivered fact carries a verbatim quote machine-checked against the source file at the claimed line — an unverifiable claim is dropped, not shipped.

It spends real money against the Cerebras API (typically well under $0.15/query — see `STATUS.md` for measured numbers). It is non-interactive by design: no prompts beyond the pre-spend cost confirmation, no colors, no spinners. Stdout carries exactly one JSON envelope; a failure still produces exactly one envelope (on stdout, not stderr — see Output contract below).

## Install

```sh
brew install treygoff24/tap/scout
# or
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/treygoff24/scout/releases/latest/download/scout-cli-installer.sh | sh
# or
cargo install scout-cli
```

All three install a binary named `scout` (the crates.io package is `scout-cli`; the bare `scout` name was taken). Verify: `scout --version` prints `scout <semver>`.

## Setup for your human

One secret is required. Do not guess it; ask your human to provide it or set it in the environment/secret manager you have access to:

- `CEREBRAS_API_KEY` (or `SCOUT_API_KEY` as a fallback name) — from https://cloud.cerebras.ai

Then self-verify without spending:

```sh
scout doctor --pretty
```

`doctor` reports API-key presence, optional external tool availability (`ctags`, `pdftotext`, `pandoc`), current index status, and a per-adapter `degraded` map. It never prints secret values. If the key is missing, `doctor` returns `state: provider_error` (exit 13) but still emits a full structured envelope.

## Canonical invocations

```sh
scout index ./some-project                       # build/refresh the index — spends money, resumable, incremental
scout brief ./some-project --budget 8k            # session-start orientation: module map, entry points, hot files
scout "what does the retry policy look like" ./some-project --budget 8k
scout --query "<query>" ./some-project             # explicit form, identical to omitting --query
```

Every paid command (`index`, `brief`, query) prints a cost estimate before spending and asks for confirmation; pass `--yes` (or `-y` for index/eval) to skip the prompt non-interactively. `--max-dollars N` is a hard cap enforced mid-run — a budget-exhausted run still returns a valid `state: budget_hit` envelope with partial results, never a bare failure.

Full flag surface per command (also machine-readable via `scout capabilities --pretty` → `data.flags`):

- `index [dir] [--yes|-y] [--max-dollars N] [--pretty]`
- `[--query] "<query>" [dir] [--budget N] [--max-dollars N] [--refresh] [--compact] [--pretty]`
- `brief [dir] [--budget N] [--max-dollars N] [--refresh] [--pretty]`
- `capabilities | schema | doctor` — read-only, free, `[--pretty]` only
- `eval m1|m3 [--max-dollars N] [--yes] [--corpus DIR] [--only ID] [--pretty]`
- `--version | -V` / `--help | -h`

`dir` defaults to the current directory everywhere it's optional. `--refresh` forces an index rebuild check before `brief`/query instead of trusting a possibly-stale snapshot. `--compact` on query drops `deterministic_score`/`router_rank` and collapses `dropped` findings into `{reason: count}` to save tokens. `--budget N` accepts a bare integer or a `k`-suffixed shorthand (e.g. `8k` = 8000 tokens) and packs findings into that ceiling, omitting `quote` text (`quote_omitted: true`) before dropping findings — the full unabridged set is always persisted to `.scout/last-run.json` regardless of what the response envelope contains.

## Output contract

- **stdout**: exactly one JSON success envelope, schema `scout.cli.response.v1` (`SCHEMA` in `src/main.rs`). This holds even on error states — scout never prints partial JSON or free text to stdout.
- **No prompts beyond cost confirmation, no colors, no spinners.** Safe to drive from any harness; use `--yes` to remove the one interactive prompt.
- Self-description: `scout capabilities --pretty` (provider, env vars, sensitive-file deny list, redaction categories, adapters, states, exit codes, per-command flags, query-artifact paths), `scout schema --pretty` (envelope/finding/query-data/brief-data field shapes).

### Envelope states and exit codes

| Exit | State | Meaning |
| ---: | --- | --- |
| 0 | `ok` | success |
| 1 | `internal_error` | unexpected failure |
| 2 | `usage_error` | bad arguments |
| 3 | `partial` | some findings/cards missing, envelope still useful |
| 4 | `unanswered` | query had no supportable findings |
| 10 | `budget_hit` | stopped at `--max-dollars`, partial results returned — treat as soft success |
| 11 | `index_stale` | index exists but source files changed since — envelope carries `changed_files` and a self-healing `hint` |
| 12 | `index_missing` | run `scout index` first |
| 13 | `provider_error` | API/auth failure |
| 14 | `tool_degraded` | an optional external tool (`ctags`/`pdftotext`/`pandoc`) is missing; index/brief still complete, degraded |

Exit 10 and 11 are the non-obvious ones: nonzero exit but stdout carries a usable envelope with actionable structured data (`hint`, `changed_files`), not just an error message.

## Reading findings

Every finding carries `file` (relative path), `line` (1-based), `fact` (model-written text), and `quote` (verbatim source text backing that fact, unless `quote_omitted: true` under a tight `--budget`). `match_tier` is `exact` or `markdown_normalized` (whitespace/heading normalization allowed for prose corpora). Trust rule: a fact is only as good as its `quote` — if `quote_omitted` is true, treat the fact as a lead to re-verify against `.scout/last-run.json`, not as citable on its own. `deterministic_score` and `router_rank` are omitted under `--compact`; `weak_signal: true` in query data means the top finding's deterministic score is under 40% of typical — treat those results with extra skepticism.

## Environment variables

| Variable | Purpose | Default |
| --- | --- | --- |
| `CEREBRAS_API_KEY` / `SCOUT_API_KEY` | API key (`CEREBRAS_API_KEY` checked first) | none |
| `SCOUT_MODEL` | model override | `gemma-4-31b` |
| `SCOUT_MARKDOWN_ONLY` | set `1` to index only markdown/prose files | unset |
| `SCOUT_CHUNK_MODE` | extraction chunking strategy | `ranked_boundary` |
| `SCOUT_CHUNK_CAP` | max chunks per file for ranked-boundary chunking | `24` |
| `SCOUT_CONCURRENCY` | client-side request concurrency cap | `50` |
| `SCOUT_EVAL_POLICY_CORPUS` | overrides `corpus_root` for every query in a loaded eval suite (despite the name, applies to any milestone) | unset |

## Cost safety

- Every paid command shows a cost estimate and asks for confirmation before spending; `--yes`/`-y` skips the prompt.
- `--max-dollars N` is a hard cap enforced by a reservation check mid-run; a cap hit returns `state: budget_hit` (exit 10) with whatever partial results exist, never a silent overspend.
- `index` progress is durable and resumable (hash-keyed cards; only new/changed files cost money on a rerun).
- A refresh estimate under `$0.25` (`REFRESH_AUTO_YES_MAX_USD` in source) auto-confirms without a prompt; above that it asks like any other paid run.

## Things that will save you a debugging loop

- The index lives inside the corpus at `.scout/` as immutable generation directories with a single atomic `current` pointer — a query mid-rebuild always reads one consistent generation. Don't hand-edit `.scout/`.
- Harness-litter directories (`.delegate`, `.claude`, `.codex`, `.cursor`, `.vscode`, `.idea`, `.desloppify`, `.tldr`) are skipped at the walk by default, same as VCS dirs.
- Sensitive files are default-denied at the walk (`.env*`, SSH/cloud-credential dirs, `*.pem`/`*.key`/certs, etc. — full list in `scout capabilities --pretty` → `data.sensitive_deny`) and outbound chunks are redacted (known token formats, bearer tokens, key/value secrets, JWTs, high-entropy strings, emails, home paths) before they ever reach the model.
- An unrecognized first argument that doesn't start with `-` is treated as a query, not an error — `scout query-shaped-typo` will attempt a query rather than fail fast. A near-miss on a real subcommand name (e.g. `indx`, `brie`) gets a "did you mean" usage error instead.
- PDF and DOCX support depends on `pdftotext`/`pandoc` being installed; `doctor` reports `degraded.pdf`/`degraded.docx` when they're missing, and index/brief still complete with those files skipped rather than failing the run.

## Maintainers

See `docs/plans/2026-07-05-scout-design.md` for architecture and on-disk contracts, and `STATUS.md` for the build/eval status record. `eval/README.md` documents how to run or extend the M1/M3 gate suites.
