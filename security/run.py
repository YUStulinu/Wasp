"""Runs the hostile modules in security/ under `wasp run` (JIT and interpreter) and checks that
each one is stopped in a controlled way, without harming the host.

    python security/run.py

For every scenario: the outcome (trap message or exit code), wall time, and the host process's
peak memory. The WASI escape test puts a secret file next to (not inside) the preopened
directory and checks that it never reaches the output.

Writes results/security.json.
"""
from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import psutil

ROOT = Path(__file__).resolve().parents[1]
EXE = ".exe" if sys.platform == "win32" else ""
WASP = ROOT / "runtime" / "target" / "release" / f"wasp{EXE}"
SEC = ROOT / "security"

# (module, extra flags, what counts as contained, description)
SCENARIOS = [
    ("oob_load.wat", [], {"trap": "out of bounds memory access"}, "load from an address far outside linear memory"),
    ("oob_store.wat", [], {"trap": "out of bounds memory access"}, "store one byte past the end of memory"),
    ("memory_bomb.wat", ["--max-memory-mb", "64"], {"exit": 0, "max_rss_mb": 200}, "memory.grow in a loop, capped at 64 MiB"),
    ("memory_initial.wat", ["--max-memory-mb", "64"], {"instantiation": "memory limit"}, "module asking for 4 GiB at instantiation"),
    ("infinite_loop.wat", ["--timeout-ms", "500"], {"trap": "interrupted", "max_seconds": 3}, "infinite loop with a 500 ms deadline"),
    ("deep_recursion.wat", [], {"trap": "call stack exhausted"}, "unbounded recursion"),
    ("big_frames.wat", [], {"trap": "call stack exhausted"}, "recursion with 20,000 locals per frame"),
    ("table_oob.wat", [], {"trap": "undefined element"}, "call_indirect past the end of the table"),
    ("signature_mismatch.wat", [], {"trap": "indirect call type mismatch"}, "call_indirect with the wrong signature"),
    ("wasi_escape.wat", ["SANDBOX"], {"exit": 0, "no_secret": True}, "WASI path_open outside the preopened directory"),
    ("wasi_bad_pointer.wat", [], {"exit": 42}, "WASI calls with pointers outside linear memory (EFAULT x2)"),
]


def run(cmd: list[str], timeout: float = 60) -> dict:
    t0 = time.perf_counter()
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    proc = psutil.Process(p.pid)
    peak = 0
    while p.poll() is None:
        try:
            peak = max(peak, proc.memory_info().rss)
        except psutil.Error:
            pass
        if time.perf_counter() - t0 > timeout:
            p.kill()
        time.sleep(0.01)
    out, err = p.communicate()
    return {"exit": p.returncode, "stdout": out.decode(errors="replace"), "stderr": err.decode(errors="replace"),
            "seconds": round(time.perf_counter() - t0, 3), "peak_rss_mb": round(peak / 2**20, 1)}


def main():
    work = Path(tempfile.mkdtemp(prefix="wasp-sec-"))
    sandbox = work / "sandbox"
    (sandbox / "sub").mkdir(parents=True)
    (work / "outside.txt").write_text("SECRET-OUTSIDE-THE-SANDBOX")
    results = []
    ok_all = True
    try:
        for module, flags, expect, desc in SCENARIOS:
            for mode in ["--jit", "--interp"]:
                fl = [f for f in flags if f != "SANDBOX"]
                if "SANDBOX" in flags:
                    fl += ["--dir", str(sandbox)]
                r = run([str(WASP), "run", mode, *fl, str(SEC / module)])
                text = r["stdout"] + r["stderr"]
                ok = r["exit"] in range(0, 256) and "panicked" not in text  # the host itself must not crash
                if "trap" in expect:
                    ok &= r["exit"] == 134 and expect["trap"] in r["stderr"]
                if "exit" in expect:
                    ok &= r["exit"] == expect["exit"]
                if "instantiation" in expect:
                    ok &= r["exit"] == 1 and expect["instantiation"] in r["stderr"]
                if "max_seconds" in expect:
                    ok &= r["seconds"] <= expect["max_seconds"]
                if "max_rss_mb" in expect:
                    ok &= r["peak_rss_mb"] <= expect["max_rss_mb"]
                if expect.get("no_secret"):
                    ok &= "SECRET" not in text
                ok_all &= ok
                outcome = (r["stderr"].strip().splitlines() or [f"exit {r['exit']}"])[-1]
                results.append({"module": module, "mode": mode[2:], "description": desc, "contained": ok, "outcome": outcome,
                                "exit": r["exit"], "seconds": r["seconds"], "peak_rss_mb": r["peak_rss_mb"]})
                print(f"{'ok  ' if ok else 'FAIL'} {module:24} {mode[2:]:6} {r['seconds']:6.2f} s {r['peak_rss_mb']:7.1f} MB  {outcome}", flush=True)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    (ROOT / "results").mkdir(exist_ok=True)
    (ROOT / "results" / "security.json").write_text(json.dumps(results, indent=1))
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
