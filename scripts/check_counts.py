#!/usr/bin/env python3
"""Large-scale correctness check: per-key row counts from csvidx vs pandas.

pandas parses the key column of the whole file independently of our scanner;
the sampled keys include existing, heavy and non-existing ones.
"""
import argparse
import os
import subprocess
import sys
from pathlib import Path

import pandas as pd

ROOT = Path(__file__).resolve().parent.parent
BIN = os.environ.get("CSVIDX", str(ROOT / "target" / "release" / "csvidx"))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--csv", required=True)
    ap.add_argument("--keys", required=True)
    ap.add_argument("--key-col", default="user_id")
    a = ap.parse_args()

    keys = [l.strip() for l in open(a.keys) if l.strip()]
    keys = list(dict.fromkeys(keys))  # unique, keep order
    wanted = set(keys)
    print(f"pandas: streaming column {a.key_col!r} of {a.csv} in chunks ...", flush=True)
    expected = pd.Series(dtype="int64")
    total = 0
    for chunk in pd.read_csv(a.csv, usecols=[a.key_col], dtype=str, chunksize=5_000_000):
        col = chunk[a.key_col]
        total += len(col)
        expected = expected.add(col[col.isin(wanted)].value_counts(), fill_value=0)
    expected = expected.astype("int64")
    print(f"pandas: {total:,} rows scanned", flush=True)

    p = subprocess.run([BIN, "get", a.csv, "--count", "--", *keys], capture_output=True, check=True)
    got = {}
    for line in p.stdout.decode().splitlines():
        k, c = line.rsplit("\t", 1)
        got[k] = int(c)

    bad = [(k, got.get(k), int(expected.get(k, 0))) for k in keys if got.get(k) != int(expected.get(k, 0))]
    present = sum(1 for k in keys if expected.get(k, 0) > 0)
    heavy = max(int(expected.get(k, 0)) for k in keys)
    print(f"checked {len(keys)} keys ({present} present, {len(keys) - present} absent, max rows per key {heavy})")
    if bad:
        print("MISMATCHES (key, csvidx, pandas):", bad[:10])
        sys.exit(1)
    print("COUNTS MATCH")


if __name__ == "__main__":
    main()
