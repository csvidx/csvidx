#!/usr/bin/env python3
"""Parallel build check: the same index under different thread counts.

For each --threads value it builds the index (page cache warmed first, so the numbers compare
CPU work, not the disk), records time and the sha256 of the resulting file. All indexes must be
byte-identical. The csvidx output line shows how many chunks needed a sequential repair.

    python3 scripts/threads_check.py --csv data/orders.csv --threads 1 2 4
"""
import argparse
import hashlib
import os
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = os.environ.get("CSVIDX", str(ROOT / "target" / "release" / "csvidx"))


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(8 << 20):
            h.update(chunk)
    return h.hexdigest()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--csv", required=True)
    ap.add_argument("--key", default="user_id")
    ap.add_argument("--threads", type=int, nargs="+", default=[1, 2, 4])
    ap.add_argument("--mem-limit-mib", type=int, default=1024)
    ap.add_argument("--repeat", type=int, default=2, help="builds per setting; the fastest is reported")
    a = ap.parse_args()
    csv = str(Path(a.csv).resolve())
    idx = csv + ".idx"
    print(f"{csv}: {os.path.getsize(csv) / 2**30:.2f} GiB, {os.cpu_count()} CPUs, mem limit {a.mem_limit_mib} MiB\n")
    with open(csv, "rb") as f:                      # warm the page cache
        while f.read(64 << 20):
            pass
    print(f"{'threads':>8} {'best time':>10} {'scan':>8} {'sort':>8} {'write':>8}  {'chunks':>7} {'repairs':>8}  sha256(index)")
    digests, times = {}, {}
    for t in a.threads:
        best = None
        for _ in range(a.repeat):
            if os.path.exists(idx):
                os.remove(idx)
            t0 = time.perf_counter()
            p = subprocess.run([BIN, "build", csv, "--key", a.key, "--threads", str(t), "--mem-limit-mib", str(a.mem_limit_mib)],
                               capture_output=True, text=True)
            dt = time.perf_counter() - t0
            if p.returncode != 0:
                sys.exit(p.stderr)
            if best is None or dt < best[0]:
                best = (dt, p.stdout)
        dt, out = best
        m = re.search(r"scan ([\d.]+)s, sort ([\d.]+)s, write ([\d.]+)s", out)
        c = re.search(r"(\d+) chunks, (\d+) needed", out)
        digests[t] = sha256(idx)
        times[t] = dt
        left = [f for f in os.listdir(os.path.dirname(csv)) if f.endswith(".tmp")]
        assert not left, f"leftover temp files: {left}"
        print(f"{t:>8} {dt:>9.2f}s {m.group(1):>7}s {m.group(2):>7}s {m.group(3):>7}s  {c.group(1) if c else '-':>7} {c.group(2) if c else '-':>8}  {digests[t][:16]}…")
    if len(set(digests.values())) != 1:
        sys.exit("INDEXES DIFFER between thread counts")
    base = times[a.threads[0]]
    print("\nall indexes are byte-identical; speed-up vs", f"{a.threads[0]} thread(s):",
          ", ".join(f"{t}: {base / times[t]:.2f}x" for t in a.threads))
    print("THREADS CHECK PASSED")


if __name__ == "__main__":
    main()
