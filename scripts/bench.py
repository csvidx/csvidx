#!/usr/bin/env python3
"""Benchmark csvidx against full-scan alternatives on one large CSV.

Compared for "give me all rows of user X":
  * csvidx (CLI process per lookup, warm and cold page cache; in-process latency)
  * grep (full scan), pandas (full scan of the key column), DuckDB (full scan, optional)
Also reports index build time, peak RSS, index size and incremental `update` time.

The page cache is evicted with posix_fadvise(DONTNEED) for the "cold" numbers,
which needs no root.  Absolute numbers depend on the disk; ratios are what matter.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = os.environ.get("CSVIDX", str(ROOT / "target" / "release" / "csvidx"))


def timed(cmd, capture=False):
    """Run cmd; return (seconds, peak_rss_mb, stdout_bytes)."""
    t = time.perf_counter()
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE if capture else subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         env={**os.environ, "LC_ALL": "C"})
    out = p.stdout.read() if capture else b""
    _, status, ru = os.wait4(p.pid, 0)
    dt = time.perf_counter() - t
    p.returncode = os.waitstatus_to_exitcode(status)
    if capture:
        p.stdout.close()
    return dt, ru.ru_maxrss / 1024, out


def evict(*paths):
    for path in paths:
        fd = os.open(path, os.O_RDONLY)
        try:
            os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        finally:
            os.close(fd)


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * p))]


def fmt(s):
    return f"{s * 1000:.1f} ms" if s < 1 else f"{s:.2f} s"


def meminfo_gb():
    for line in open("/proc/meminfo"):
        if line.startswith("MemTotal"):
            return int(line.split()[1]) / 1024 / 1024
    return 0.0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--csv", required=True)
    ap.add_argument("--keys", required=True)
    ap.add_argument("--key-col", default="user_id")
    ap.add_argument("--cli-keys", type=int, default=200)
    ap.add_argument("--cold-keys", type=int, default=20)
    ap.add_argument("--no-pandas", action="store_true")
    ap.add_argument("--no-duckdb", action="store_true")
    ap.add_argument("--out", default=None, help="write results as JSON")
    a = ap.parse_args()
    csv_path = str(Path(a.csv).resolve())
    idx_path = csv_path + ".idx"
    keys = [l.strip() for l in open(a.keys) if l.strip()]
    res = {"csv_gib": os.path.getsize(csv_path) / 2**30, "cpus": os.cpu_count(), "ram_gib": meminfo_gb()}
    print(f"CSV {csv_path}: {res['csv_gib']:.2f} GiB, {res['cpus']} CPUs, {res['ram_gib']:.1f} GiB RAM\n")

    # --- 1. build ---------------------------------------------------------------
    if os.path.exists(idx_path):
        os.remove(idx_path)
    evict(csv_path)
    dt, rss, _ = timed([BIN, "build", csv_path, "--key", a.key_col])
    res["build_cold_s"], res["build_rss_mb"] = dt, rss
    res["index_gib"] = os.path.getsize(idx_path) / 2**30
    dt2, rss2, _ = timed([BIN, "build", csv_path, "--key", a.key_col])
    res["build_warm_s"] = dt2
    print(f"build index: {fmt(dt)} cold / {fmt(dt2)} warm cache, peak RSS {rss:.0f} MB, index {res['index_gib']:.2f} GiB "
          f"({res['index_gib'] / res['csv_gib'] * 100:.0f}% of the CSV)")

    # --- 2. pick three representative keys ---------------------------------------
    out = subprocess.run([BIN, "get", csv_path, "--count", "--", *keys[:400]], capture_output=True, check=True).stdout.decode()
    counts = {k: int(c) for k, c in (l.rsplit("\t", 1) for l in out.splitlines())}
    typical = next(k for k, c in counts.items() if 5 <= c <= 30)
    heavy = max(counts, key=counts.get)
    missing = next(k for k, c in counts.items() if c == 0)
    picks = {"typical": typical, "heavy": heavy, "missing": missing}
    print("keys:", ", ".join(f"{n}={k} ({counts[k]} rows)" for n, k in picks.items()), "\n")
    res["picks"] = {n: {"key": k, "rows": counts[k]} for n, k in picks.items()}

    # --- 3. csvidx CLI latency ----------------------------------------------------
    sample = [k for k in keys if counts.get(k, 1) > 0 and counts.get(k, 1) < 100][: a.cli_keys] or keys[: a.cli_keys]
    warm = [timed([BIN, "get", csv_path, "--", k])[0] for k in sample]
    cold = []
    for k in sample[: a.cold_keys]:
        evict(csv_path, idx_path)
        cold.append(timed([BIN, "get", csv_path, "--", k])[0])
    res["cli_warm_ms"] = {"p50": pct(warm, .5) * 1e3, "p90": pct(warm, .9) * 1e3, "max": max(warm) * 1e3}
    res["cli_cold_ms"] = {"p50": pct(cold, .5) * 1e3, "p90": pct(cold, .9) * 1e3, "max": max(cold) * 1e3}
    print(f"csvidx get (new process per lookup, <100 rows): warm p50 {pct(warm, .5) * 1e3:.1f} ms, p90 {pct(warm, .9) * 1e3:.1f} ms | "
          f"cold cache p50 {pct(cold, .5) * 1e3:.1f} ms, p90 {pct(cold, .9) * 1e3:.1f} ms, max {max(cold) * 1e3:.1f} ms")

    subprocess.run([BIN, "bench", csv_path, a.keys], capture_output=True, check=True)   # warm-up pass, discarded
    b = subprocess.run([BIN, "bench", csv_path, a.keys, "--repeat", "3"], capture_output=True, check=True).stdout.decode()
    print("csvidx in-process, warm cache (index already open; the key file mixes light and heavy keys):\n  " + b.strip().replace("\n", "\n  "))
    res["inprocess"] = b.strip()

    for name, k in picks.items():
        evict(csv_path, idx_path)
        c = timed([BIN, "get", csv_path, "--", k])[0]
        w = timed([BIN, "get", csv_path, "--", k])[0]
        res.setdefault("csvidx_by_key", {})[name] = {"cold_ms": c * 1e3, "warm_ms": w * 1e3}
    print()

    # --- 4. grep full scan -----------------------------------------------------------
    print("full-scan alternatives (one lookup each):")
    res["grep"] = {}
    for name, k in picks.items():
        pattern = f"^[^,]*,{k},"          # valid for this generated file (key = 2nd column, never quoted)
        evict(csv_path)
        # stdout must be a pipe, not /dev/null: GNU grep quits at the first match when
        # writing to /dev/null, which would make the scan look absurdly fast.
        cold_t, _, o = timed(["grep", "-c", "-E", pattern, csv_path], capture=True)
        warm_t, _, _ = timed(["grep", "-c", "-E", pattern, csv_path], capture=True)
        n = int(o.strip() or 0)
        assert n == counts[k], f"grep found {n} rows for {k}, csvidx {counts[k]}"
        res["grep"][name] = {"cold_s": cold_t, "warm_s": warm_t}
        print(f"  grep       {name:8s} cold {fmt(cold_t):>9}  warm {fmt(warm_t):>9}   ({n} rows, matches csvidx)")

    # --- 5. pandas -----------------------------------------------------------------------
    if not a.no_pandas:
        code = (f"import pandas as pd,time,sys; t=time.perf_counter(); "
                f"s=pd.read_csv(sys.argv[1],usecols=[{a.key_col!r}],dtype='int64')[{a.key_col!r}]; "
                f"n=int((s==int(sys.argv[2])).sum()); print(n, time.perf_counter()-t)")
        evict(csv_path)
        dt, rss, o = timed([sys.executable, "-I", "-c", code, csv_path, typical], capture=True)
        n, inner = o.decode().split()[-2:]
        assert int(n) == counts[typical]
        res["pandas"] = {"total_s": dt, "read_s": float(inner), "rss_mb": rss}
        print(f"  pandas     read_csv(usecols=[key]) + filter: {fmt(dt)} total, peak RSS {rss:.0f} MB")

    # --- 6. duckdb -------------------------------------------------------------------------
    if not a.no_duckdb:
        try:
            import duckdb  # noqa: F401
            code = ("import duckdb,time,sys; con=duckdb.connect(); con.execute('set enable_progress_bar=false'); t=time.perf_counter(); "
                    f"n=con.execute(\"select count(*) from read_csv(?, header=true) where {a.key_col} = ?\", [sys.argv[1], int(sys.argv[2])]).fetchone()[0]; "
                    "print(n, time.perf_counter()-t)")
            res["duckdb"] = {}
            for name in ("typical", "heavy"):
                evict(csv_path)
                dt, rss, o = timed([sys.executable, "-I", "-c", code, csv_path, picks[name]], capture=True)
                n, inner = o.decode().split()[-2:]      # last line only, in case a library prints extras
                assert int(n) == counts[picks[name]], (n, counts[picks[name]])
                res["duckdb"][name] = {"total_s": dt, "query_s": float(inner), "rss_mb": rss}
                print(f"  duckdb     {name:8s} count(*) WHERE key=? : {fmt(dt)} total (cold), peak RSS {rss:.0f} MB")
        except ImportError:
            print("  duckdb     skipped (pip install duckdb)")

    # --- 7. incremental update ------------------------------------------------------------------
    # Works on a copy so the original CSV stays untouched.
    work = csv_path + ".update_bench.csv"
    shutil.copyfile(csv_path, work)
    try:
        subprocess.run([BIN, "build", work, "--key", a.key_col], capture_output=True, check=True)
        size0 = os.path.getsize(work)
        with open(work, "rb") as f:
            f.seek(size0 - max(1, int(size0 * 0.02)))
            f.readline()                # align to a row boundary
            chunk = f.read()
        with open(work, "ab") as f:
            f.write(chunk)              # append ~2% duplicate rows (keeps the file well-formed)
        main_before = Path(work + ".idx").read_bytes() if size0 < (1 << 31) else None
        evict(work, work + ".idx")
        dt_upd, _, _ = timed([BIN, "update", work, "--no-compact"])
        tail_mib = os.path.getsize(work + ".idx.tail") / 2**20
        # a key that occurs in the appended part: its row count must now be higher than before
        tail_key = chunk.split(b"\n", 1)[0].split(b",")[1].decode()
        n_idx = int(subprocess.run([BIN, "get", work, "--count", "--", tail_key], capture_output=True, check=True)
                    .stdout.decode().rsplit("\t", 1)[1])
        n_grep = int(timed(["grep", "-c", "-E", f"^[^,]*,{tail_key},", work], capture=True)[2].strip() or 0)
        assert n_idx == n_grep, f"tail lookup: csvidx {n_idx} rows, grep {n_grep}"
        keys_t = [k for k in keys[:300]]
        subprocess.run([BIN, "get", work, "--count", "--", *keys_t[:5]], capture_output=True, check=True)
        warm_tail = [timed([BIN, "get", work, "--", k])[0] for k in sample[:100]]
        ok_tail = subprocess.run([BIN, "verify", work], capture_output=True).returncode == 0
        dt_comp, _, _ = timed([BIN, "compact", work])
        ok_comp = subprocess.run([BIN, "verify", work], capture_output=True).returncode == 0
        dt_full, _, _ = timed([BIN, "build", work, "--key", a.key_col])                  # for comparison only
        res.update(update_tail_s=dt_upd, compact_s=dt_comp, rebuild_s=dt_full, tail_mib=tail_mib,
                   verify_after_update=ok_tail, verify_after_compact=ok_comp,
                   cli_warm_with_tail_ms=pct(warm_tail, .5) * 1e3)
        print(f"\nappended {len(chunk) / 2**20:.0f} MiB (~2%): `update --no-compact` {fmt(dt_upd)} "
              f"(tail file {tail_mib:.1f} MiB, main index untouched) | `compact` {fmt(dt_comp)} | full rebuild {fmt(dt_full)}")
        print(f"verify with tail: {'OK' if ok_tail else 'FAILED'}, after compact: {'OK' if ok_comp else 'FAILED'}; "
              f"lookup of a key from the appended part: csvidx {n_idx} rows == grep {n_grep}; "
              f"warm CLI lookup with tail p50 {pct(warm_tail, .5) * 1e3:.1f} ms")
    finally:
        for p_ in (work, work + ".idx", work + ".idx.tail"):
            if os.path.exists(p_):
                os.remove(p_)

    # --- summary --------------------------------------------------------------------------------------
    g = res["grep"]["typical"]["cold_s"]
    c = res["cli_cold_ms"]["p50"] / 1e3
    print(f"\nsummary: typical lookup  grep cold {fmt(g)}  vs  csvidx cold {fmt(c)}  (~{g / c:.0f}x),  "
          f"csvidx warm {fmt(res['cli_warm_ms']['p50'] / 1e3)}")
    if a.out:
        Path(a.out).write_text(json.dumps(res, indent=2))
        print(f"results written to {a.out}")


if __name__ == "__main__":
    main()
