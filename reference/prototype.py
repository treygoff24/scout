"""scout — repo exploration at retina speed. Query + dir -> cited, quote-verified findings.

Gemma extracts per-chunk facts; a mechanical filter drops any finding whose verbatim
quote isn't actually in the file. Synthesis is the caller's job, not scout's.
Usage: python3 scout.py "where is the API key loaded?" watcher/
"""
import json
import os
import re
import sys
import time
from concurrent.futures import ThreadPoolExecutor

import gemma

CHUNK_LINES = 350
MAX_FILE_BYTES = 200_000
SKIP_DIRS = {".git", "node_modules", "__pycache__", "target", ".venv", "venv",
             "dist", "build", ".next", "scratch", "corpus", "data-archives"}
SKIP_EXT = {".png", ".jpg", ".jpeg", ".gif", ".pdf", ".zip", ".gz", ".tar", ".ico",
            ".woff", ".woff2", ".lock", ".jsonl", ".svg", ".mp4", ".mp3", ".heic"}

STOP = set("the a an and or of to in for on at is are was be with how does do did what "
           "where when which who why by from it its this that as before after not any "
           "one all between during into out over under between".split())
TOP_K = 12  # ponytail: fixed K caps cost by construction; tune/IDF-weight if coverage pays


def terms(query):
    return {_stem(w) for w in re.findall(r"[a-z0-9_]{3,}", query.lower()) if w not in STOP}


def _stem(w):
    for suf in ("ing", "ed", "es", "s"):
        if w.endswith(suf) and len(w) - len(suf) >= 3:
            return w[: len(w) - len(suf)]
    return w


def rank(jobs, query):
    """Top-K chunks by distinct stemmed query terms at word boundaries. Zero model calls."""
    ts = terms(query)
    scored = []
    for j in jobs:
        low = j[2].lower()
        hits = {t: len(re.findall(r"\b" + re.escape(t), low)) for t in ts}
        distinct = sum(1 for n in hits.values() if n)
        if distinct:
            scored.append((distinct, sum(hits.values()), j))
    scored.sort(key=lambda s: (s[0], s[1]), reverse=True)
    return [j for _, _, j in scored[:TOP_K]]


SYS = """You are a code-exploration extractor. You get ONE chunk of ONE file (with line \
numbers) and a query. Return ONLY a JSON array of findings relevant to the query, found \
IN THIS CHUNK. Each finding: {"fact": one precise sentence, "line": int, "quote": string}.

Rules — violating any makes the finding worthless:
- "quote" must be COPIED VERBATIM from the chunk (without the line-number prefix). It will \
be machine-checked against the file; paraphrased quotes are discarded.
- "fact" must be fully supported by the quote and its immediate context. State only what \
the code shows, not what you infer about code you cannot see.
- Only include findings genuinely relevant to the query. Irrelevant trivia is noise.
- If nothing in this chunk is relevant, return []. That is the correct answer for most \
chunks. NEVER invent a finding to have something to say.
Return the JSON array only, no prose, no markdown fence."""


def walk(root):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS and not d.startswith(".")]
        for name in sorted(filenames):
            path = os.path.join(dirpath, name)
            if os.path.splitext(name)[1].lower() in SKIP_EXT:
                continue
            try:
                if os.path.getsize(path) > MAX_FILE_BYTES:
                    continue
                with open(path, encoding="utf-8") as f:
                    text = f.read()
            except (UnicodeDecodeError, OSError):
                continue
            if "\x00" in text:
                continue
            yield os.path.relpath(path, root), text


def chunks(relpath, text):
    lines = text.splitlines()
    for start in range(0, len(lines), CHUNK_LINES):
        body = "\n".join(f"{i+1}| {l}" for i, l in enumerate(lines[start:start + CHUNK_LINES],
                                                             start))
        yield start + 1, body


def norm(s):
    return re.sub(r"\s+", " ", s).strip()


def extract(query, relpath, first_line, body):
    user = f"FILE: {relpath} (chunk starting at line {first_line})\n\n{body}\n\nQUERY: {query}"
    r = gemma.chat([{"role": "system", "content": SYS}, {"role": "user", "content": user}],
                   temperature=0, max_tokens=2000)
    raw = r["choices"][0]["message"]["content"].strip()
    raw = re.sub(r"^```(json)?|```$", "", raw, flags=re.M).strip()
    try:
        arr = json.loads(raw)
        assert isinstance(arr, list)
    except (json.JSONDecodeError, AssertionError):
        return [{"file": relpath, "error": "unparseable", "raw": raw[:200]}]
    return [{"file": relpath, "fact": f.get("fact"), "line": f.get("line"),
             "quote": f.get("quote")} for f in arr if isinstance(f, dict)]


def verify(findings, files):
    """Mechanical firewall: verbatim quote must exist in the cited file, cited line
    within ±5 of where the quote actually lives. No model calls."""
    kept, dropped = [], []
    for f in findings:
        if "error" in f:
            dropped.append({**f, "why": "unparseable"})
            continue
        text = files.get(f["file"], "")
        q = norm(f.get("quote") or "")
        if not q or not f.get("fact"):
            dropped.append({**f, "why": "empty"})
            continue
        nls = [norm(l) for l in text.splitlines()]
        full = " ".join(nls)
        idx = full.find(q)
        hit = None
        if idx >= 0:  # map char offset back to the 1-based line where the quote starts
            off = 0
            for n, nl in enumerate(nls, 1):
                if idx < off + len(nl) + 1:
                    hit = n
                    break
                off += len(nl) + 1
        if hit is None:
            dropped.append({**f, "why": "quote not in file"})
        elif not isinstance(f.get("line"), int) or abs(f["line"] - hit) > 5:
            dropped.append({**f, "why": f"line {f.get('line')} vs actual {hit}"})
        else:
            kept.append(f)
    return kept, dropped


def scout(query, root, workers=20, prefilter=False):
    # prefilter defaults OFF: top-K=12 failed its predeclared coverage bar by one fact
    # (66.7% vs 70) — see docs/experiments/2026-07-05-scout-killtest.md iteration 1b.
    # K~16-20 or IDF weighting is the untested next lever.
    t0 = time.time()
    files = dict(walk(root))
    jobs = [(rel, ln, body) for rel, text in files.items() for ln, body in chunks(rel, text)]
    skipped = 0
    if prefilter:
        keep = rank(jobs, query)
        skipped, jobs = len(jobs) - len(keep), keep
    with ThreadPoolExecutor(max_workers=workers) as ex:
        results = list(ex.map(lambda j: extract(query, *j), jobs))
    findings = [f for batch in results for f in batch]
    kept, dropped = verify(findings, files)
    return {
        "query": query, "root": str(root),
        "files": len(files), "chunks": len(jobs), "chunks_skipped": skipped,
        "findings": kept, "dropped": dropped,
        "wall_s": round(time.time() - t0, 1),
        "spend": dict(gemma.spend),
    }


if __name__ == "__main__":
    out = scout(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else ".")
    json.dump(out, sys.stdout, indent=1)
    print()
