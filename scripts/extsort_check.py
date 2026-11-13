#!/usr/bin/env python3
"""External sort check: build the same index under different memory limits.

For each --mem-limit-mib value it builds the index, polls /proc/<pid>/status for the
anonymous resident set (RssAnon: real heap memory, without file pages of the mmap'd
CSV, which the kernel can drop at any time), and records time and the sha256 of the
resulting index file. All indexes must be byte-identical. Finally `verify` runs
under the smallest limit.

Linux only (uses /proc).
"""
import argparse
import hashlib
import os
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


def status_kb(pid, field):
    try:
        for line in open(f"/proc/{pid}/status"):
            if line.startswith(field + ":"):
                return int(line.split()[1])
    except OSError:
        pass
    return None


def run_polled(cmd):
    """Run cmd; return (seconds, peak RssAnon MiB, peak total RSS MiB, stdout)."""
    t = time.perf_counter()
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    peak_anon = 0
    while p.poll() is None:
        a = status_kb(p.pid, "RssAnon")
        if a:
            peak_anon = max(peak_anon, a)
        time.sleep(0.01)
    out = p.stdout.read().decode()
    peak_hwm = None
    dt = time.perf_counter() - t
    if p.returncode != 0:
        sys.exit(f"{cmd} failed:\n{out}")
    return dt, peak_anon / 1024, peak_hwm, out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--csv", required=True)
    ap.add_argument("--key", default="user_id")
    ap.add_argument("--limits", type=int, nargs="+", default=[64, 256, 1024, 8192])
    a = ap.parse_args()
    csv = str(Path(a.csv).resolve())
    idx = csv + ".idx"
    print(f"{csv}: {os.path.getsize(csv) / 2**30:.2f} GiB\n")
    print(f"{'limit':>10} {'time':>8} {'peak anon RSS':>14} {'runs':>5}  sha256(index)")
    digests = {}
    for lim in a.limits:
        if os.path.exists(idx):
            os.remove(idx)
        dt, anon, _, out = run_polled([BIN, "build", csv, "--key", a.key, "--mem-limit-mib", str(lim)])
        runs = 0
        for line in out.splitlines():
            if line.startswith("external sort:"):
                runs = int(line.split()[2])
        d = sha256(idx)
        digests[lim] = d
        left = [f for f in os.listdir(os.path.dirname(csv)) if f.endswith(".tmp")]
        assert not left, f"leftover temp files: {left}"
        print(f"{lim:>7} MiB {dt:7.1f}s {anon:11.0f} MiB {runs:>5}  {d[:16]}…")
    if len(set(digests.values())) != 1:
        sys.exit("INDEXES DIFFER between memory limits")
    lo = min(a.limits)
    dt, anon, _, out = run_polled([BIN, "verify", csv, "--mem-limit-mib", str(lo)])
    assert "VERIFY OK" in out, out
    print(f"\nall {len(a.limits)} indexes are byte-identical; `verify --mem-limit-mib {lo}`: OK in {dt:.1f}s, peak anon RSS {anon:.0f} MiB")
    print("EXTSORT CHECK PASSED")


if __name__ == "__main__":
    main()
