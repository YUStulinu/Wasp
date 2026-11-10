"""Benchmarks: the C programs in bench/programs, native and in Wasp (JIT and interpreter).

    python bench/run.py [--runs 3] [--skip-interp]

Each program is compiled twice with the same clang flags (-O2 -ffp-contract=off): for wasm32
(no C library; see examples/wasi_min.h) and natively. Each program times its own kernel and
prints a checksum; every run's checksum must equal the native one. The best of `runs` runs is
kept. Startup costs (decode, validate, compile, instantiate) come from `wasp run --stats`.

Writes results/bench.json and docs/bench.png.
"""
from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
EXE = ".exe" if sys.platform == "win32" else ""
WASP = ROOT / "runtime" / "target" / "release" / f"wasp{EXE}"
BUILD = ROOT / "bench" / "build"
FLAGS = ["-O2", "-ffp-contract=off"]
LINE = re.compile(r"(\S+) checksum=(\d+) ns=(\d+)")


def build(src: Path) -> tuple[Path, Path]:
    BUILD.mkdir(parents=True, exist_ok=True)
    wasm = BUILD / f"{src.stem}.wasm"
    native = BUILD / f"{src.stem}{EXE or '.bin'}"
    subprocess.run(["clang", "--target=wasm32", *FLAGS, "-nostdlib", "-Wl,--no-entry", "-o", str(wasm), str(src)], check=True)
    subprocess.run(["clang", *FLAGS, "-o", str(native), str(src)], check=True)
    return wasm, native


def run(cmd: list[str]) -> tuple[int, float, dict]:
    p = subprocess.run(cmd, capture_output=True, text=True, timeout=3600)
    m = LINE.search(p.stdout)
    if not m:
        raise RuntimeError(f"{cmd}: {p.stdout} {p.stderr}")
    stats = {}
    for line in p.stderr.splitlines():
        if line.startswith("{"):
            stats = json.loads(line)
    return int(m.group(2)), int(m.group(3)) / 1e9, stats


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--skip-interp", action="store_true")
    a = ap.parse_args()
    results = {}
    for src in sorted((ROOT / "bench" / "programs").glob("*.c")):
        wasm, native = build(src)
        row = {"wasm_bytes": wasm.stat().st_size}
        modes = [("native", [str(native)]), ("jit", [str(WASP), "run", "--stats", str(wasm)])]
        if not a.skip_interp:
            modes.append(("interp", [str(WASP), "run", "--interp", "--stats", str(wasm)]))
        ref = None
        for mode, cmd in modes:
            times = []
            for _ in range(a.runs):
                checksum, secs, stats = run(cmd)
                if ref is None:
                    ref = checksum
                if checksum != ref:
                    raise RuntimeError(f"{src.stem} {mode}: checksum {checksum} != native {ref}")
                times.append(secs)
                if stats:
                    row[f"{mode}_startup"] = {k: stats[k] for k in ("decode_ms", "validate_ms", "compile_ms", "instantiate_ms", "code_bytes")}
            row[mode] = min(times)
        row["checksum"] = ref
        row["jit_vs_native"] = round(row["jit"] / row["native"], 2)
        if "interp" in row:
            row["interp_vs_native"] = round(row["interp"] / row["native"], 2)
            row["interp_vs_jit"] = round(row["interp"] / row["jit"], 2)
        results[src.stem] = row
        print(f"{src.stem:10} native {row['native']*1e3:9.1f} ms   jit {row['jit']*1e3:9.1f} ms ({row['jit_vs_native']:5.1f}x)"
              + (f"   interp {row['interp']*1e3:10.1f} ms ({row['interp_vs_native']:6.1f}x)" if "interp" in row else ""), flush=True)
    out = ROOT / "results"
    out.mkdir(exist_ok=True)
    (out / "bench.json").write_text(json.dumps(results, indent=1))
    plot(results)


def plot(results: dict):
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    names = list(results)
    fig, ax = plt.subplots(figsize=(10, 4.2))
    width = 0.38
    series = [("jit_vs_native", "#2a78d6", "Wasp JIT"), ("interp_vs_native", "#eb6834", "Wasp interpreter")]
    for k, (key, color, label) in enumerate(series):
        vals = [results[n].get(key) for n in names]
        if any(v is None for v in vals):
            continue
        xs = [i + (k - 0.5) * width for i in range(len(names))]
        bars = ax.bar(xs, vals, width=width - 0.04, color=color, label=label)
        for b, v in zip(bars, vals):
            ax.text(b.get_x() + b.get_width() / 2, v * 1.08, f"{v:.0f}x" if v >= 10 else f"{v:.1f}x", ha="center", va="bottom", fontsize=8, color="#444")
    ax.set_yscale("log")
    ax.axhline(1, color="#888", lw=1)
    ax.set_xticks(range(len(names)), names)
    ax.set_ylabel("time relative to native clang -O2")
    ax.set_title("Kernel time relative to the same C code compiled natively (lower is better)", fontsize=11, loc="left")
    for s in ("top", "right"):
        ax.spines[s].set_visible(False)
    ax.legend(frameon=False, loc="upper left")
    fig.tight_layout()
    (ROOT / "docs").mkdir(exist_ok=True)
    fig.savefig(ROOT / "docs" / "bench.png", dpi=130)


if __name__ == "__main__":
    main()
