# Field report: `scout index` retry storm on Cerebras hourly quota — 105+ min for a ~550-file subtree, zero visible progress

**Date:** 2026-07-08 · **Reporter:** Claude (Fable), driving a real goal run in `prospera-radar-build` · **scout version:** 0.1.0 (crates.io/Homebrew build, `~/.cargo/bin/scout`, 7.4 MB binary) · **Model:** `gemma-4-31b` via Cerebras (default) · **Host:** macOS, Apple Silicon

## TL;DR

`scout index web` on a ~550-file subtree ran for **1h45m+ without completing or printing anything**, while the session that requested it finished an entire 4-phase feature implementation by hand. A process stack sample shows it spending **~98% of wall-clock inside `sleep_retry`**: Cerebras is returning HTTP 429 `request_quota_exceeded` (an **hourly** request quota), scout's retry policy treats that as a transient error and retries on a seconds-scale backoff that can never succeed inside the quota window, and there is no circuit breaker, no progress output, and no partial checkpoint until the very end. The run degenerates into an hours-long, mostly-futile duty cycle. A 2-file controlled repro reproduces the failure in 36 s with the exact 429 body captured below.

## Timeline (all times same session)

| T+ | Event |
| --- | --- |
| 0:00 | `scout index --yes --max-dollars 2` (full repo): printed estimate `957 files to card, ~$7.1152`, exited `budget_hit`. Estimate is local-only (`calls: 0`) — good. |
| 0:02 | `scout index web --yes --max-dollars 5` launched in background (~550 files). |
| 0:02–1:45+ | **No output, no `.scout/` directory, no exit.** (Caveat: stdout was piped through `tail -2`, so line-buffered progress — if any existed — would have been hidden. But see "no progress output" below: there is none to hide.) |
| ~0:40 | `ps`: elapsed 39:23, still running. |
| ~1:45 | Forensics captured (below). Entire consuming session's work had finished long before. |
| +36 s | 2-file repro run: fails cleanly with the 429 body, confirming quota exhaustion as the live cause. |

## Evidence

### 1. Process state at 1h45m

```
  PID  ELAPSED      TIME  %CPU    RSS STAT
35397 01:45:18   0:03.01   0.0  24144 SN
```

**3.01 seconds of CPU in 105 minutes.** One established TCP connection to `104.18.10.146:443` (Cloudflare / api.cerebras.ai). The process is not slow — it is asleep.

### 2. Stack sample (macOS `sample 35397 3`)

2610 of 2656 samples on the main thread are inside the backoff sleep:

```
scout::cmd_index
  scout::CerebrasClient::chat_with_budget   (src/main.rs:343)
    scout::sleep_retry                      (src/main.rs:415)
      std::thread::sleep
```

### 3. Controlled repro (2 tiny files, 36 s, captures the server error)

```
$ mkdir /tmp/scout-repro && <2 trivial files> && scout index /tmp/scout-repro --yes --max-dollars 0.10
scout index estimate before spend: 2 files to card, ~$0.0050
{"state":"provider_error", ... "timings_ms":{"total":35741},
 "skipped":[{"path":"a.js","reason":"provider_error: Cerebras HTTP 429:
   {\"message\":\"Requests per hour limit exceeded - too many requests sent.\",
    \"type\":\"too_many_requests_error\",\"param\":\"quota\",\"code\":\"request_quota_exceeded\"}", ...}],
 "errors":["all card generation attempts failed"]}
```

36 s for 2 files = 2 files × (6 attempts × request RTT + backoff sleeps). Every attempt is doomed: the quota is **per hour**, the max backoff is **60 s**.

## Root-cause chain (with code refs, `src/main.rs` @ 0.1.0)

1. **Client-side limiter is tuned to a limit Cerebras doesn't enforce.** `RateLimiter::per_minute(450.0)` (`:339`) permits a 450-request burst. The account's binding limit is an **hourly request quota** (`code: request_quota_exceeded`). On a 550-file index, scout bursts through the hour's allowance in the first minutes, then every subsequent request 429s for the rest of the hour.
2. **`retryable()` treats all 429s as transient** (`:411-413`). A per-minute 429 is transient; an hourly-quota 429 is not — retrying at seconds scale cannot succeed. The response body carries a machine-readable discriminator (`code: request_quota_exceeded`) that is currently ignored, and any `Retry-After`/`x-ratelimit-*` headers are never read.
3. **Backoff caps at 60 s and gives up per-file, not per-run** (`:415-427`, `chat_with_budget` `:343-408`). Each file independently burns 6 attempts (~30 s of sleeps + 6 RTTs) and then fails. The loop (`for file in files`, `:831`) moves on to the next file and repeats the identical doomed sequence. ~550 files × ~35 s ≈ **5+ hours of scheduled futility**. `check_retry_budget` gates on *spend*, but quota-blocked retries cost $0, so the budget gate never trips.
4. **No global circuit breaker.** N consecutive `request_quota_exceeded` failures (say 3) is proof the run cannot proceed; scout should abort (or sleep until the quota window resets, loudly). Instead the only exit conditions are "all files attempted" or budget hit.
5. **No progress output.** Between the estimate line and the final envelope, `cmd_index` prints nothing — not a per-file line, not an every-N counter, not a stderr heartbeat. A multi-hour run is indistinguishable from a hang without `ps`/`sample`. (This is what forced the forensics in the first place.)
6. **All-or-nothing artifact write.** The manifest is only written at the end of `cmd_index` (`:875+`). Killing a stuck run loses every card that succeeded before quota exhaustion. (Mitigation that already exists: on a *completed* run, `files_to_card`/`can_reuse` reuses cards by `(rel, hash)`, so a re-run after a partial success is cheap — but only if the first run is allowed to finish, which is exactly what a stuck run prevents.)
7. **Duty-cycle pathology (interaction of 1+2+3).** The hourly quota refills; scout is still running; the limiter allows another 450/min burst; the new allowance is consumed in ~the first minute of the hour; 59 more minutes of 429 grinding follow. The observed 105-minute runtime with 3 s of CPU is ~2 of these cycles. Net effective throughput: a few hundred files per *hour*, on an inference provider whose whole selling point is speed.

## Impact on the requesting session (the product-level failure)

I was asked to use scout for codebase exploration during a real implementation goal (6 chat tools + an approval flow in `prospera-radar-build`). The index never became available. The entire recon phase was done with grep/read in ~10 minutes; all 4 implementation phases plus live browser QA finished before one subtree indexed. **Scout could not participate in the session it was invoked for.** For an agent-facing tool, time-to-first-answer is the metric that decides whether the tool gets used at all; as shipped, it is unbounded under a quota 429.

## Recommendations (ordered by leverage)

1. **Parse the 429 body / headers and branch.** `code == "request_quota_exceeded"` (or any hourly/daily quota signal, or a `Retry-After` above ~2× max backoff) → fail the RUN immediately with a dedicated `state: "quota_exhausted"`, the reset horizon if known, and a "re-run later; completed cards will be reused" hint. This is a ~20-line change in `chat_with_budget`/`retryable` and converts a 5-hour silent grind into a 30-second actionable failure.
2. **Consecutive-failure circuit breaker** as a backstop for providers with unhelpful error bodies: 3 consecutive identical provider errors across files → abort run with the last body verbatim.
3. **Write the manifest incrementally (or on abort).** Flush cards to `.scout/` every N files or on any run-fatal error so successful work survives. The reuse machinery already keys by `(rel, hash)`; it just needs a manifest on disk to read.
4. **Progress line to stderr.** `carded 37/550 (skipped 2, $0.41)` every file or every 5 s. Cheap, and it's the difference between "working" and "hung" for both humans and agents (agents will `tail` the file you pipe to).
5. **Learn the real limit from response headers** instead of the hardcoded 450/min (`:339`) — Cerebras returns rate-limit headers; even just halving the burst on first 429 would stop the quota from being consumed in one gulp, spreading the allowance across the run.
6. **Smaller quality-of-life items observed in the same session:**
   - `scout --help` exits 1 with usage on stderr; help should exit 0 on stdout (breaks `scout --help | grep`, confuses agent wrappers).
   - Estimate-before-spend printing is great — but consider printing it (and requiring `--yes`) *before* opening the provider connection, and add a `--dirs`/glob filter so first-touch indexing of a monorepo subtree doesn't require cd-gymnastics (subdir indexing works, but discoverability is poor).
   - No `claude-skill` registration exists for scout (receipts has one). I only found the binary because the user named it. A skill entry with the index-first workflow would fix discoverability.
   - Consider documenting the intended deployment shape: for agent sessions the economics only work with a warm index (e.g., nightly `scout index` cron per active repo). Cold-indexing inside a hot session loses the race against grep even when the provider behaves.

## Reproduction recipe

1. Exhaust the Cerebras hourly request quota (a single burst-heavy `scout index` on a few-hundred-file tree does this organically, or hammer the API directly).
2. `scout index <any dir> --yes --max-dollars 0.10` → observe ~18 s/file of pure backoff, `provider_error` envelope with `request_quota_exceeded` in every skip reason.
3. For the live-hang variant: start a larger index while quota-exhausted and observe `ps -o etime,cputime` (CPU ≪ elapsed) and `sample <pid>` landing in `sleep_retry`.

## Artifacts

- Stack sample: `/tmp/scout-sample.txt` on the reporting host (2610/2656 samples in `sleep_retry`).
- Repro envelope: inline above (2-file run, `timings_ms.total: 35741`).
- Requesting-session journal (full context): `prospera-radar-build/state/journal-2026-07-08-chat-monitor-goal.md`.
- The stuck index (PID 35397) was left running at report time for further live inspection; it is budget-capped at $5 and spending nothing while quota-blocked.
