"""Start-up cost of a function.

    python bench/startup.py [--go path/to/hello.wasm]

1. In process (`wasp run --stats`): decode, validate, JIT compile and instantiate, for a small C
   program, a mid-size one, and (optionally) a 2.6 MB Go program.
2. Through the server: a running server (started here) is asked to run the same small function
   N times; each call is a new `wasp` process. Reports the median and 95th percentile of the
   server-side wall time (process start to exit) and of the whole HTTP round trip.

Writes results/startup.json.
"""
from __future__ import annotations

import argparse
import json
import statistics
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
EXE = ".exe" if sys.platform == "win32" else ""
WASP = ROOT / "runtime" / "target" / "release" / f"wasp{EXE}"


def stats(module: Path, mode: str, runs: int = 15) -> dict:
    rows = []
    for _ in range(runs):
        p = subprocess.run([str(WASP), "run", mode, "--stats", str(module)], capture_output=True, text=True)
        rows.append(json.loads([x for x in p.stderr.splitlines() if x.startswith("{")][-1]))
    keys = ["decode_ms", "validate_ms", "compile_ms", "instantiate_ms"]
    return {k: round(statistics.median(r[k] for r in rows), 3) for k in keys} | {"code_bytes": rows[0]["code_bytes"], "module_bytes": module.stat().st_size}


def post(url: str, body: dict) -> dict:
    req = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.loads(r.read())


def pct(xs: list[float], q: float) -> float:
    xs = sorted(xs)
    return round(xs[min(len(xs) - 1, int(q * len(xs)))], 2)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--go", help="a Go program compiled with GOOS=wasip1")
    ap.add_argument("-n", type=int, default=50)
    a = ap.parse_args()
    build = ROOT / "bench" / "build"
    build.mkdir(parents=True, exist_ok=True)
    hello = build / "hello.wasm"
    subprocess.run(["clang", "--target=wasm32", "-O2", "-nostdlib", "-Wl,--no-entry", "-I", str(ROOT / "examples"),
                    "-o", str(hello), str(ROOT / "examples" / "hello.c")], check=True)
    out = {"in_process": {}}
    modules = {"hello (C)": hello, "mixmark (C)": build / "mixmark.wasm"}
    if a.go:
        modules["hello (Go, wasip1)"] = Path(a.go)
    for name, m in modules.items():
        if m.exists():
            out["in_process"][name] = {"jit": stats(m, "--jit"), "interp": stats(m, "--interp")}
            print(name, out["in_process"][name], flush=True)

    server = subprocess.Popen([str(ROOT / "server" / f"wasp-server{EXE}"), "-addr", "127.0.0.1:8099", "-static", ""],
                              cwd=ROOT / "server", stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                urllib.request.urlopen("http://127.0.0.1:8099/api/examples", timeout=1)
                break
            except OSError:
                time.sleep(0.1)
        id_ = post("http://127.0.0.1:8099/api/compile", {"language": "c", "source": (ROOT / "examples" / "hello.c").read_text()})["id"]
        for mode in ["jit", "interp"]:
            walls, trips = [], []
            for _ in range(a.n):
                t0 = time.perf_counter()
                r = post(f"http://127.0.0.1:8099/api/functions/{id_}/run", {"mode": mode})
                trips.append((time.perf_counter() - t0) * 1e3)
                walls.append(r["wall_ms"])
            out[f"server_{mode}"] = {"runs": a.n, "process_median_ms": pct(walls, 0.5), "process_p95_ms": pct(walls, 0.95),
                                     "http_median_ms": pct(trips, 0.5), "http_p95_ms": pct(trips, 0.95)}
            print(mode, out[f"server_{mode}"], flush=True)
    finally:
        server.kill()
    (ROOT / "results").mkdir(exist_ok=True)
    (ROOT / "results" / "startup.json").write_text(json.dumps(out, indent=1))


if __name__ == "__main__":
    main()
