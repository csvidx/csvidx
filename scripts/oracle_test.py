#!/usr/bin/env python3
"""Differential test: csvidx against Python's `csv` module as an independent oracle.

For several CSV dialects (LF/CRLF, minimal/all quoting, with/without BOM, key by
name/by number) it writes a file full of nasty keys and payloads (commas, quotes,
embedded newlines, unicode, empty keys, blank lines, short rows), builds the index
and checks that `csvidx get` returns exactly the rows Python's parser finds.
Then it appends rows, checks `update`, and checks stale-index detection.
"""
import csv
import io
import os
import random
import subprocess
import sys
import tempfile
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = os.environ.get("CSVIDX", str(ROOT / "target" / "release" / "csvidx"))

NASTY = ["a,b", 'x"y', "multi\nline", "multi\r\nline", "  spaced  ", "ünï©ode-ключ", "日本語",
         "", "'", "\\", "a\tb", ",", '"', '""', "\n", "key with, comma and \"quotes\""]
HEADER = ["id", "key", "payload", "n"]
KEY_COL = 1


def run(*args, check=True):
    p = subprocess.run([BIN, *map(str, args)], capture_output=True)
    if check and p.returncode != 0:
        raise RuntimeError(f"csvidx {args} failed ({p.returncode}): {p.stderr.decode(errors='replace')}")
    return p


def make_rows(rnd, n, start=0):
    pool = [f"k{rnd.randrange(400)}" for _ in range(400)]
    rows = []
    for i in range(start, start + n):
        key = rnd.choice(NASTY) if rnd.random() < 0.4 else rnd.choice(pool)
        payload = "".join(rnd.choice(NASTY + ["x", "yy", " "]) for _ in range(rnd.randrange(0, 4)))
        rows.append([str(i), key, payload, str(rnd.randrange(10**6))])
        if rnd.random() < 0.01:
            rows.append([])            # blank line
        if rnd.random() < 0.01:
            rows.append(["short"])     # row shorter than the key column
    return rows


def write_rows(path, rows, term, quoting, bom=False, header=True, mode="w"):
    enc = "utf-8-sig" if (bom and mode == "w") else "utf-8"
    with open(path, mode, newline="", encoding=enc) as f:
        w = csv.writer(f, lineterminator=term, quoting=quoting)
        if header and mode == "w":
            w.writerow(HEADER)
        w.writerows(rows)


def oracle(path):
    d = defaultdict(list)
    with open(path, newline="", encoding="utf-8-sig") as f:
        r = csv.reader(f)
        next(r)
        for row in r:
            if row and len(row) > KEY_COL:
                d[row[KEY_COL]].append(row)
    return d


def cli_rows(path, key):
    p = run("get", path, "--", key)
    text = p.stdout.decode("utf-8")
    return [r for r in csv.reader(io.StringIO(text, newline="")) if r], p.stderr.decode()


def check_all(path, rnd, label):
    exp = oracle(path)
    keys = list(NASTY) + rnd.sample(sorted(exp), min(150, len(exp))) + ["absent-1", "absent,2", "k99999"]
    for k in keys:
        got, _ = cli_rows(path, k)
        want = exp.get(k, [])
        if got != want:
            raise AssertionError(f"[{label}] key {k!r}: expected {len(want)} rows, got {len(got)}\n want={want[:3]}\n got ={got[:3]}")

    # counts of every key (except those containing line breaks, which break the line-based output)
    simple = [k for k in exp if "\n" not in k and "\r" not in k]
    p = run("get", path, "--count", "--", *simple)
    lines = p.stdout.decode("utf-8").split("\n")
    lines.pop()  # trailing empty string after the final newline
    if len(lines) != len(simple):
        raise AssertionError(f"[{label}] count output has {len(lines)} lines for {len(simple)} keys")
    for k, line in zip(simple, lines):
        _, c = line.rsplit("\t", 1)
        if int(c) != len(exp[k]):
            raise AssertionError(f"[{label}] count for {k!r}: expected {len(exp[k])}, got {c}")
    return len(exp), sum(len(v) for v in exp.values())


def scenario(tmp, term, quoting, bom, by_name, seed):
    label = f"term={'CRLF' if term == chr(13) + chr(10) else 'LF'} quoting={'ALL' if quoting == csv.QUOTE_ALL else 'MIN'} bom={bom} key={'name' if by_name else 'number'}"
    rnd = random.Random(seed)
    path = tmp / f"t{seed}.csv"
    write_rows(path, make_rows(rnd, 3000), term, quoting, bom)
    run("build", path, "--key", "key" if by_name else str(KEY_COL))
    n_keys, n_rows = check_all(path, rnd, label + " /build")
    run("verify", path)

    # CSV grows: old index still answers for old rows and warns; `update --no-compact` writes only the tail
    main_before = Path(str(path) + ".idx").read_bytes()
    write_rows(path, make_rows(rnd, 500, start=3000), term, quoting, mode="a")
    p = run("get", path, "--", "k1")
    assert b"grew by" in p.stderr, f"[{label}] no staleness warning"
    run("update", path, "--no-compact")
    assert Path(str(path) + ".idx").read_bytes() == main_before, f"[{label}] update touched the main index"
    assert Path(str(path) + ".idx.tail").exists(), f"[{label}] no tail file"
    n_keys2, n_rows2 = check_all(path, rnd, label + " /update(tail)")
    run("verify", path)
    assert n_rows2 > n_rows

    # a second append rewrites the tail; main stays untouched
    write_rows(path, make_rows(rnd, 300, start=3500), term, quoting, mode="a")
    run("update", path, "--no-compact")
    assert Path(str(path) + ".idx").read_bytes() == main_before
    check_all(path, rnd, label + " /update2(tail)")
    run("verify", path)

    # compaction gives exactly what a from-scratch build gives
    run("compact", path)
    assert not Path(str(path) + ".idx.tail").exists()
    compacted = Path(str(path) + ".idx").read_bytes()
    check_all(path, rnd, label + " /compact")
    run("build", path, "--key", "key" if by_name else str(KEY_COL))
    assert Path(str(path) + ".idx").read_bytes() == compacted, f"[{label}] compact != rebuild"
    run("verify", path)

    # rewriting the beginning of the file must be detected, not silently mis-served
    data = path.read_bytes()
    path.write_bytes(b"X" + data[1:])
    p = run("get", path, "--", "k1", check=False)
    assert p.returncode != 0 and b"stale" in p.stderr, f"[{label}] stale index not detected"
    print(f"  ok  {label}: {n_rows2} rows, {n_keys2} distinct keys")


def unterminated(tmp):
    rnd = random.Random(777)
    path = tmp / "unterminated.csv"
    write_rows(path, make_rows(rnd, 300), "\n", csv.QUOTE_MINIMAL)
    data = path.read_bytes()
    assert data.endswith(b"\n")
    path.write_bytes(data[:-1])                    # last row has no newline
    run("build", path, "--key", "key")
    check_all(path, rnd, "unterminated /build")
    write_rows(path, [], "\n", csv.QUOTE_MINIMAL, mode="a")
    with open(path, "a", newline="", encoding="utf-8") as f:
        f.write("\n")                              # the writer finishes the last line...
    write_rows(path, make_rows(rnd, 100, start=300), "\n", csv.QUOTE_MINIMAL, mode="a")  # ...and keeps going
    run("update", path, "--no-compact")
    check_all(path, rnd, "unterminated /update(tail)")
    run("verify", path)
    with open(path, "ab") as f:
        f.write(b"zzz-unterminated,tail-row,x,1")        # provisional row inside the tail
    run("update", path, "--no-compact")
    check_all(path, rnd, "unterminated /update2(tail)")
    run("compact", path)
    run("verify", path)
    print("  ok  unterminated last row, then growth through the tail")


def main():
    if not Path(BIN).exists():
        sys.exit(f"binary not found: {BIN} (run `cargo build --release` or set CSVIDX)")
    with tempfile.TemporaryDirectory() as d:
        tmp = Path(d)
        seed = 0
        for term in ("\n", "\r\n"):
            for quoting in (csv.QUOTE_MINIMAL, csv.QUOTE_ALL):
                for bom in (False, True):
                    seed += 1
                    scenario(tmp, term, quoting, bom, by_name=(seed % 2 == 0), seed=seed)
        unterminated(tmp)
    print("ORACLE TEST PASSED")


if __name__ == "__main__":
    main()
