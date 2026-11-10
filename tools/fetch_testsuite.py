"""Downloads the WebAssembly 2.0 core spec tests (tag wg-2.0 of WebAssembly/spec) into
data/testsuite/, plus the SIMD tests into data/testsuite/simd/.

    python tools/fetch_testsuite.py
"""
import json
import sys
import time
import urllib.request
from pathlib import Path

TAG = "wg-2.0"
API = f"https://api.github.com/repos/WebAssembly/spec/contents/test/core{{}}?ref={TAG}"
RAW = f"https://raw.githubusercontent.com/WebAssembly/spec/{TAG}/test/core/{{}}"
OUT = Path(__file__).resolve().parents[1] / "data" / "testsuite"


def get(url: str) -> bytes:
    for k in range(5):
        try:
            with urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "wasp-fetch"}), timeout=60) as r:
                return r.read()
        except OSError:
            time.sleep(3 * (k + 1))
    raise RuntimeError(url)


def main():
    for sub in ["", "/simd"]:
        entries = json.loads(get(API.format(sub)))
        (OUT / sub.strip("/")).mkdir(parents=True, exist_ok=True)
        n = 0
        for e in entries:
            if e["name"].endswith(".wast"):
                dest = OUT / sub.strip("/") / e["name"]
                if not dest.exists():
                    dest.write_bytes(get(RAW.format((sub.strip("/") + "/" if sub else "") + e["name"])))
                n += 1
        print(f"test/core{sub}: {n} files")


if __name__ == "__main__":
    sys.exit(main())
