"""Tests for the Python bindings. pandas' own CSV parser is the oracle."""
import csv
import random
import threading
from concurrent.futures import ThreadPoolExecutor

import pandas as pd
import pytest

import csvidx

NASTY = ["a,b", 'x"y', "multi\nline", "  spaced  ", "ünï©ode-ключ", "日本語", "'", "\\", "key with, comma and \"quotes\""]
HEADER = ["id", "key", "amount", "note"]


def make_csv(path, rows=6000, seed=1, term="\n", start=0, mode="w"):
    rnd = random.Random(seed)
    pool = [f"k{rnd.randrange(300)}" for _ in range(300)]
    with open(path, mode, newline="", encoding="utf-8") as f:
        w = csv.writer(f, lineterminator=term)
        if mode == "w":
            w.writerow(HEADER)
        for i in range(start, start + rows):
            key = rnd.choice(NASTY) if rnd.random() < 0.3 else rnd.choice(pool)
            note = rnd.choice(["", "ok", "he said \"hi\"", "a,b", "line1\nline2"])
            w.writerow([i, key, f"{rnd.randrange(10**6) / 100:.2f}", note])


def oracle(path):
    return pd.read_csv(path, dtype=str, keep_default_na=False, encoding="utf-8")


def same(a: pd.DataFrame, b: pd.DataFrame):
    a = a.reset_index(drop=True)
    b = b.reset_index(drop=True)
    assert list(a.columns) == list(b.columns)
    pd.testing.assert_frame_equal(a, b, check_dtype=False)


@pytest.fixture()
def csv_path(tmp_path):
    p = tmp_path / "t.csv"
    make_csv(p)
    csvidx.build(str(p), key="key")
    return p


STR = dict(dtype=str, keep_default_na=False)


def present(full, n=3):
    """The n most frequent regular keys of the file (so tests never compare two empty results)."""
    vc = full[full["key"].str.startswith("k")]["key"].value_counts()
    keys = list(vc.index[:n])
    assert len(keys) == n and all(vc[k] > 1 for k in keys)
    return keys


def test_get_matches_pandas_filter(csv_path):
    full = oracle(csv_path)
    idx = csvidx.Index(csv_path)
    assert idx.columns == HEADER and idx.key_column == 1
    nonempty = 0
    for k in [*present(full, 5), *NASTY, "absent"]:
        got = idx.get(k, **STR)
        same(got, full[full["key"] == k])
        nonempty += len(got) > 0
    assert nonempty >= 10, "the test would be vacuous"


def test_get_many_keys_is_in_file_order_and_deduplicated(csv_path):
    full = oracle(csv_path)
    idx = csvidx.Index(csv_path)
    k1, k2, _ = present(full)
    keys = [k1, k2, k1, "multi\nline", "x\"y", "nope"]
    want = full[full["key"].isin(set(keys))]
    assert len(want) > 20
    same(idx.get(keys, **STR), want)
    ids = idx.get(keys, **STR)["id"].astype(int)
    assert ids.is_monotonic_increasing


def test_key_normalisation(tmp_path):
    p = tmp_path / "n.csv"
    p.write_text("id,user\n1,7\n2,007\n3,7\n4,x\n")
    csvidx.build(str(p), key="user")
    idx = csvidx.Index(p)
    assert list(idx.get(7)["id"]) == [1, 3]
    assert list(idx.get("7")["id"]) == [1, 3]
    assert list(idx.get(b"7")["id"]) == [1, 3]
    assert list(idx.get("007")["id"]) == [2]          # byte-exact, as documented
    assert idx.count([7, "007", "zzz"]) == [2, 1, 0]
    import numpy as np
    assert list(idx.get(np.int64(7))["id"]) == [1, 3]
    with pytest.raises(TypeError):
        idx.get(7.0)
    with pytest.raises(TypeError):
        idx.get(True)
    with pytest.raises(TypeError):
        idx.get(None)


def test_empty_result_keeps_columns_and_dtypes(csv_path):
    idx = csvidx.Index(csv_path)
    df = idx.get("definitely-absent")
    assert len(df) == 0 and list(df.columns) == HEADER
    df = idx.get("definitely-absent", dtype={"id": "int64", "amount": "float64"})
    assert str(df["id"].dtype) == "int64" and str(df["amount"].dtype) == "float64"
    assert len(idx.get([])) == 0
    t = idx.get_arrow("definitely-absent")
    assert t.num_rows == 0 and t.column_names == HEADER


def test_type_inference_and_kwargs(csv_path):
    idx = csvidx.Index(csv_path)
    k = present(oracle(csv_path), 1)[0]
    df = idx.get(k)
    assert len(df) > 1
    assert str(df["id"].dtype).startswith("int") and str(df["amount"].dtype) == "float64"
    df = idx.get(k, dtype={"id": "string"})
    assert str(df["id"].dtype) in ("string", "str")


def test_count_matches_pandas(csv_path):
    full = oracle(csv_path)
    idx = csvidx.Index(csv_path)
    keys = [*present(full), "a,b", "none"]
    got = idx.count(keys)
    assert got == [int((full["key"] == k).sum()) for k in keys]
    assert got[0] > 1 and got[3] > 0 and got[4] == 0


def test_get_arrow_matches_pandas(csv_path):
    pa = pytest.importorskip("pyarrow")
    full = oracle(csv_path)
    idx = csvidx.Index(csv_path)
    k5, k6, _ = present(full)
    t = idx.get_arrow(k5, column_types={"id": pa.int64(), "key": pa.string(), "amount": pa.string(), "note": pa.string()})
    want = full[full["key"] == k5]
    assert len(want) > 1 and t.num_rows == len(want)
    got = t.to_pandas()
    assert list(got["id"]) == [int(x) for x in want["id"]]
    assert list(got["note"]) == list(want["note"])
    assert idx.get_arrow([k5, k6]).num_rows == int(full["key"].isin([k5, k6]).sum())


def test_crlf_file(tmp_path):
    p = tmp_path / "crlf.csv"
    make_csv(p, rows=1500, term="\r\n", seed=3)
    csvidx.build(str(p), key="key")
    full = oracle(p)
    idx = csvidx.Index(p)
    for k in [*present(full, 2), "multi\nline"]:
        got = idx.get(k, **STR)
        assert len(got) > 0
        same(got, full[full["key"] == k])


def test_no_header_csv(tmp_path):
    p = tmp_path / "nh.csv"
    p.write_text("1,a\n2,b\n3,a\n")
    csvidx.build(str(p), key="1", header=False)
    idx = csvidx.Index(p)
    assert idx.columns is None
    df = idx.get("a")
    assert df.shape == (2, 2) and list(df[0]) == [1, 3]


def test_update_tail_compact_roundtrip(tmp_path):
    p = tmp_path / "grow.csv"
    make_csv(p, rows=8000, seed=5)
    csvidx.build(str(p), key="key")
    idx = csvidx.Index(p)
    ka, kb, _ = present(oracle(p))
    n0 = len(idx.get(ka))
    assert n0 > 1 and idx.bytes_behind == 0

    make_csv(p, rows=60, seed=6, start=8000, mode="a")
    assert csvidx.Index(p).bytes_behind > 0
    assert len(idx.get(ka)) == n0                      # an open Index keeps its snapshot of the rows
    u = csvidx.update(str(p), compact="never")
    assert u["added"] == 60 and not u["compacted"] and u["tail_entries"] == 60
    idx2 = csvidx.Index(p)
    assert idx2.bytes_behind == 0 and idx2.tail_entries == 60
    full = oracle(p)
    for k in [ka, kb, "x\"y"]:
        got = idx2.get(k, **STR)
        assert len(got) > 0
        same(got, full[full["key"] == k])
    assert len(idx2.get(ka)) >= n0
    assert csvidx.verify(str(p))["ok"]

    c = csvidx.compact(str(p))
    assert c["merged"] and csvidx.Index(p).tail_entries == 0
    same(csvidx.Index(p).get(ka, **STR), full[full["key"] == ka])
    assert csvidx.verify(str(p), mem_limit_mib=1)["ok"]
    assert csvidx.update(str(p))["up_to_date"]
    with pytest.raises(ValueError):
        csvidx.update(str(p), compact="sometimes")


def test_errors_are_csvidx_errors(tmp_path):
    p = tmp_path / "e.csv"
    p.write_text("a,b\n1,2\n")
    with pytest.raises(csvidx.CsvIdxError, match="build"):
        csvidx.Index(p)                                 # no index yet
    with pytest.raises(csvidx.CsvIdxError, match="not found"):
        csvidx.build(str(p), key="nope")
    csvidx.build(str(p), key="a")
    data = p.read_bytes()
    p.write_bytes(b"X" + data[1:])
    with pytest.raises(csvidx.CsvIdxError, match="stale"):
        csvidx.Index(p)
    with pytest.raises(csvidx.CsvIdxError):
        csvidx.Index(tmp_path / "missing.csv")
    assert issubclass(csvidx.CsvIdxError, Exception)


def test_lookups_from_many_threads_agree(csv_path):
    full = oracle(csv_path)
    idx = csvidx.Index(csv_path)
    keys = present(full, 25) + [f"k{i}" for i in range(300, 310)]   # 25 hits + 10 absent
    want = {k: int((full["key"] == k).sum()) for k in keys}
    assert sum(want.values()) > 100

    def work(k):
        return k, len(idx.get(k, **STR))

    with ThreadPoolExecutor(8) as ex:
        results = list(ex.map(work, keys * 8))
    assert all(want[k] == n for k, n in results)


def test_gil_is_released_during_lookups(csv_path):
    """One long native call (hundreds of thousands of lookups) must not freeze other Python threads."""
    import time

    idx = csvidx.Index(csv_path)
    keys = [f"k{i % 300}" for i in range(400_000)]
    ticks = []
    running = threading.Event()
    stop = threading.Event()

    def ticker():
        running.set()
        while not stop.is_set():
            ticks.append(time.perf_counter())
            time.sleep(0.001)

    t = threading.Thread(target=ticker)
    t.start()
    running.wait()
    time.sleep(0.02)
    t0 = time.perf_counter()
    counts = idx.count(keys)
    t1 = time.perf_counter()
    stop.set()
    t.join()
    during = sum(t0 <= x <= t1 for x in ticks)
    elapsed = t1 - t0
    assert elapsed > 0.05, f"the call was too short ({elapsed:.3f}s) to prove anything"
    # a 1 ms ticker gets at least a few ticks per 10 ms if the GIL is free; with the GIL held it gets ~0
    assert during >= elapsed * 100, f"only {during} ticks in {elapsed:.2f}s: the GIL looks held"
    assert len(counts) == len(keys)


def test_threads_parameter_gives_identical_index(tmp_path):
    p = tmp_path / "th.csv"
    make_csv(p, rows=9000, seed=11)
    idx_file = tmp_path / "th.csv.idx"
    csvidx.build(str(p), key="key", threads=1)
    one = idx_file.read_bytes()
    st = csvidx.build(str(p), key="key", threads=4, mem_limit_mib=1)
    assert idx_file.read_bytes() == one
    assert st["threads"] >= 1 and "par_fallbacks" in st and st["rows"] == 9000
    assert csvidx.verify(str(p), threads=3)["ok"]
    make_csv(p, rows=50, seed=12, start=9000, mode="a")
    assert csvidx.update(str(p), compact="never", threads=2)["added"] == 50
