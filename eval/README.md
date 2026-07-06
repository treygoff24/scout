# Eval suites

`scout eval m1|m3` runs a predeclared golden-fact suite against a real corpus, spends real Cerebras API dollars, and reports measured gate numbers (candidate-file recall, coverage, poison survivors, negatives-clean, avg $/query). See `docs/plans/2026-07-05-scout-design.md` for what each milestone gate means.

## M1 — code corpus (`eval/goldens/code.json`)

Tracked and runnable out of the box. Queries point at two local Rust repos (`scout`'s own sibling projects) that ship alongside this one in the same checkout layout the original author used. If you don't have those repos, point `--corpus DIR` (or `SCOUT_EVAL_POLICY_CORPUS`, despite the name — it overrides `corpus_root` for every query in the loaded suite regardless of milestone) at your own code directory, or write your own `eval/goldens/code.json` variant against a corpus you control.

```bash
scout eval m1 --max-dollars 2.00 --yes --pretty
```

## M3 — policy/markdown corpus (`eval/goldens/private/policy_markdown.json`)

**Not tracked.** M3 was validated against a real private policy corpus (a legislative-drafting workspace: briefs, section-by-section analyses, changelogs, one-pagers) that cannot ship publicly. `eval/goldens/private/` is gitignored — nothing under it is ever committed.

To run M3 yourself, supply your own goldens file and corpus:

1. Point a markdown-heavy corpus somewhere on disk (a policy brief collection, an internal wiki export, a docs tree — anything with prose facts you can hand-verify).
2. Write `eval/goldens/private/policy_markdown.json` following the schema below, with `must_find` facts and `file`/`line` anchors you've confirmed by hand against that corpus.
3. Run:

```bash
scout eval m3 --max-dollars 2.00 --yes --pretty
```

`corpus_root` in the goldens file can be a fixed absolute path, or you can override it for every query in the suite at run time with `--corpus DIR` or `SCOUT_EVAL_POLICY_CORPUS=DIR` — handy for running the same suite against a corpus that lives in a different place on your machine than wherever it lived when the goldens were written.

## Goldens schema

```json
{
  "queries": [
    {
      "id": "unique-query-id",
      "corpus_root": "/absolute/path/to/corpus",
      "query": "What does the corpus say about X?",
      "must_find": [
        {"id": "F1", "fact": "a fact scout's extractor must surface", "file": "relative/path.md", "line": 42}
      ],
      "poison": [
        {"id": "P1", "fact": "a plausible-sounding but false claim the corpus does NOT support"}
      ],
      "negative": false
    }
  ]
}
```

- `must_find`: facts scout is expected to surface, each anchored to a `file` and 1-indexed `line` in the corpus so the eval harness can check coverage against the real source.
- `poison`: false claims that must NOT survive as delivered findings — a poison "survivor" is a gate failure (hallucination pressure leaking through).
- `negative`: set `true` for queries that probe something the corpus does not contain; `must_find` should be empty and any `poison` claims must be cleanly rejected.

Gate thresholds (candidate-file recall, coverage bar, poison-survivor tolerance, cost ceiling) are computed by `cmd_eval` in `src/main.rs`, not by the goldens file — see `docs/plans/2026-07-05-scout-design.md` for the predeclared bars per milestone.
