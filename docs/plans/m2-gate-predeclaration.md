# M2 gate predeclaration — scout hydration token A/B

Status: DEFERRED-TO-HUMAN. This gate requires paired fresh Claude sessions and cannot be honestly run by this non-interactive build agent.

## Fixed parameters before run one

- **Repo:** a local checkout of this repo (path elided; substitute your own)
- **Task:** In a fresh session, identify the files and invariants needed to change scout's query budget packing without mutating files. Objective completion check: the session must name the packing function, last-run persistence path, quote omission flag, and one relevant test/gate command.
- **Pairs:** N = 3 paired fresh-session runs minimum.
- **Arms:**
  - Control: no scout output; agent may explore normally.
  - Hydrated: prepend `scout brief <repo> --budget 8k` output and allow one targeted `scout "budget packing quote_omitted last-run" <repo> --budget 8k` call before exploration.
- **Threshold:** hydrated arm must use at least 30% fewer exploration-phase tokens than control.
- **Token accounting:** scout brief/query output tokens count against the hydrated session; the map is not free.
- **Exploration stop rule:** exploration phase ends at the first mutating tool call, or when the agent declares the file/invariant map complete if no mutation is needed.
- **Randomization:** alternate arm order by pair: H/C, C/H, H/C.
- **Failure rule:** if any run cannot complete the objective check, mark that run failed and do not exclude it unless both arms in the pair failed for an external outage.

## Human verification commands

```bash
cargo install --path .
scout brief <repo> --budget 8k > /tmp/scout-brief.json
scout "budget packing quote_omitted last-run" <repo> --budget 8k > /tmp/scout-query.json
```
