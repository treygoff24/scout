"""scout — repo exploration at retina speed. Query + dir -> cited, quote-verified findings.

Gemma extracts per-chunk facts; a mechanical filter drops any finding whose verbatim
quote isn't actually in the file. Synthesis is the caller's job, not scout's.
Usage: python3 scout.py "where is the API key loaded?" watcher/
"""
from __future__ import annotations

import json
import os
import re
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from collections.abc import Iterable, Iterator, Mapping
from typing import Literal, NotRequired, TypeAlias, TypedDict, cast

import gemma


JsonValue: TypeAlias = (
    None | bool | int | float | str | list["JsonValue"] | dict[str, "JsonValue"]
)


class RawFinding(TypedDict, total=False):
    fact: str
    line: int
    quote: str


class Finding(TypedDict):
    file: str
    fact: str | None
    line: int | None
    quote: str | None


class UnparseableFinding(TypedDict):
    file: str
    error: Literal["unparseable"]
    raw: str


ExtractedFinding = Finding | UnparseableFinding


class DroppedFinding(TypedDict):
    file: str
    why: str
    fact: NotRequired[str | None]
    line: NotRequired[int | None]
    quote: NotRequired[str | None]
    error: NotRequired[Literal["unparseable"]]
    raw: NotRequired[str]


class ScoutResult(TypedDict):
    query: str
    root: str
    files: int
    chunks: int
    chunks_skipped: int
    findings: list[Finding]
    dropped: list[DroppedFinding]
    wall_s: float
    spend: gemma.Spend

CHUNK_LINES = 350
MAX_FILE_BYTES = 200_000
SKIP_DIRS = {".git", "node_modules", "__pycache__", "target", ".venv", "venv",
             "dist", "build", ".next", "scratch", "corpus", "data-archives"}
SKIP_EXT = {".png", ".jpg", ".jpeg", ".gif", ".pdf", ".zip", ".gz", ".tar", ".ico",
            ".woff", ".woff2", ".lock", ".jsonl", ".svg", ".mp4", ".mp3", ".heic"}


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


def walk(root: str) -> Iterator[tuple[str, str]]:
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


def chunks(relpath: str, text: str) -> Iterator[tuple[int, str]]:
    lines = text.splitlines()
    for start in range(0, len(lines), CHUNK_LINES):
        body = "\n".join(f"{i+1}| {l}" for i, l in enumerate(lines[start:start + CHUNK_LINES],
                                                             start))
        yield start + 1, body


def norm(s: str) -> str:
    return re.sub(r"\s+", " ", s).strip()


def extract(query: str, relpath: str, first_line: int, body: str) -> list[ExtractedFinding]:
    user = f"FILE: {relpath} (chunk starting at line {first_line})\n\n{body}\n\nQUERY: {query}"
    messages: list[gemma.ChatMessage] = [
        {"role": "system", "content": SYS},
        {"role": "user", "content": user},
    ]
    r = gemma.chat(messages, temperature=0, max_tokens=2000)
    raw = r["choices"][0]["message"]["content"].strip()
    raw = re.sub(r"^```(json)?|```$", "", raw, flags=re.M).strip()
    try:
        parsed = cast(JsonValue, json.loads(raw))
        assert isinstance(parsed, list)
    except (json.JSONDecodeError, AssertionError):
        return [{"file": relpath, "error": "unparseable", "raw": raw[:200]}]
    findings: list[ExtractedFinding] = []
    for item in parsed:
        if not isinstance(item, dict):
            continue
        finding = cast(RawFinding, item)
        findings.append({
            "file": relpath,
            "fact": finding.get("fact"),
            "line": finding.get("line"),
            "quote": finding.get("quote"),
        })
    return findings


def verify(
    findings: Iterable[ExtractedFinding], files: Mapping[str, str]
) -> tuple[list[Finding], list[DroppedFinding]]:
    """Mechanical firewall: verbatim quote must exist in the cited file, cited line
    within ±5 of where the quote actually lives. No model calls."""
    kept: list[Finding] = []
    dropped: list[DroppedFinding] = []
    for f in findings:
        if "error" in f:
            dropped.append(cast(DroppedFinding, {**f, "why": "unparseable"}))
            continue
        text = files.get(f["file"], "")
        q = norm(f.get("quote") or "")
        if not q or not f.get("fact"):
            dropped.append(cast(DroppedFinding, {**f, "why": "empty"}))
            continue
        nls = [norm(l) for l in text.splitlines()]
        full = " ".join(nls)
        idx = full.find(q)
        hit = None
        if idx >= 0:
            off = 0
            for n, nl in enumerate(nls, 1):
                if idx < off + len(nl) + 1:
                    hit = n
                    break
                off += len(nl) + 1
        if hit is None:
            dropped.append(cast(DroppedFinding, {**f, "why": "quote not in file"}))
        elif not isinstance(f.get("line"), int) or abs(f["line"] - hit) > 5:
            dropped.append(
                cast(DroppedFinding, {**f, "why": f"line {f.get('line')} vs actual {hit}"})
            )
        else:
            kept.append(f)
    return kept, dropped


def scout(query: str, root: str, workers: int = 20) -> ScoutResult:
    t0 = time.time()
    files = dict(walk(root))
    jobs = [(rel, ln, body) for rel, text in files.items() for ln, body in chunks(rel, text)]
    with ThreadPoolExecutor(max_workers=workers) as ex:
        results = list(ex.map(lambda j: extract(query, *j), jobs))
    findings = [f for batch in results for f in batch]
    kept, dropped = verify(findings, files)
    return {
        "query": query, "root": str(root),
        "files": len(files), "chunks": len(jobs), "chunks_skipped": 0,
        "findings": kept, "dropped": dropped,
        "wall_s": round(time.time() - t0, 1),
        "spend": cast(gemma.Spend, dict(gemma.spend)),
    }


if __name__ == "__main__":
    out = scout(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else ".")
    json.dump(out, sys.stdout, indent=1)
    print()
