"""Minimal Cerebras chat client. stdlib only — requests isn't installed here."""
import json
import os
import threading
import time
import urllib.request

API = "https://api.cerebras.ai/v1/chat/completions"
UA = "volley/0.1 (github.com/treygoff; experiment harness)"  # default urllib UA gets Cloudflare-403'd

def _key():
    # env first: ccp/ccw inject the active profile's key (personal vs work split)
    if os.environ.get("CEREBRAS_API_KEY"):
        return os.environ["CEREBRAS_API_KEY"].strip()
    with open(os.path.join(os.path.dirname(__file__), ".env")) as f:
        for line in f:
            if line.startswith("CEREBRAS_API_KEY="):
                return line.split("=", 1)[1].strip()
    raise RuntimeError("no key in .env")

PRICE = {  # $/MTok (input, output)
    "gemma-4-31b": (2.15, 2.70),
    "gpt-oss-120b": (0.35, 0.75),  # verified 2026-07-02 (pricepertoken/morphllm rate cards)
    "zai-glm-4.7": (2.25, 2.75),   # verified 2026-07-02; Preview tier, repriceable
}

_lock = threading.Lock()
spend = {"in_tok": 0, "out_tok": 0, "usd": 0.0, "calls": 0}

def chat(messages, model="gemma-4-31b", retries=6, **kw):
    """One chat completion. Returns full parsed response with wall_s added."""
    body = {"model": model, "messages": messages, "stream": False, **kw}
    req = urllib.request.Request(
        API,
        data=json.dumps(body).encode(),
        headers={
            "Authorization": f"Bearer {_key()}",
            "Content-Type": "application/json",
            "User-Agent": UA,
        },
    )
    for attempt in range(retries):
        t0 = time.time()
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                out = json.load(r)
            out["wall_s"] = time.time() - t0
            u = out.get("usage", {})
            pi, po = PRICE.get(model, (2.15, 2.70))
            with _lock:
                spend["in_tok"] += u.get("prompt_tokens", 0)
                spend["out_tok"] += u.get("completion_tokens", 0)
                spend["usd"] += u.get("prompt_tokens", 0) / 1e6 * pi + u.get("completion_tokens", 0) / 1e6 * po
                spend["calls"] += 1
            return out
        except urllib.error.HTTPError as e:
            if e.code in (429, 500, 502, 503) and attempt < retries - 1:
                # 429 = per-minute window exhausted; short backoff can't outlast it
                time.sleep(20 * (attempt + 1) if e.code == 429 else 2**attempt)
                continue
            raise RuntimeError(f"HTTP {e.code}: {e.read().decode()[:500]}") from e

def text(resp):
    return resp["choices"][0]["message"].get("content")

def report():
    return f"{spend['calls']} calls, {spend['in_tok']} in / {spend['out_tok']} out tok, ${spend['usd']:.4f}"

if __name__ == "__main__":
    r = chat([{"role": "user", "content": "Say hi in exactly five words."}])
    ti = r.get("time_info", {})
    ct, n = ti.get("completion_time", 0), r["usage"]["completion_tokens"]
    print(text(r))
    print(f"wall {r['wall_s']*1000:.0f}ms | server completion {ct*1000:.1f}ms | {n} tok -> {n/ct:.0f} tok/s | {report()}")
