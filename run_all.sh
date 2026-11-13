#!/usr/bin/env bash
# Full test stand:
#   Rust tests -> Python oracle -> generate data -> build + verify -> counts vs pandas -> benchmark
#
#   ./run_all.sh                          # 20M rows (~0.9 GB CSV)
#   ROWS=100000000 ./run_all.sh           # ~4.5 GB CSV, needs ~8 GB RAM for the index build + disk for 2 copies
#   ROWS=300000 DATA=./data_smoke ./run_all.sh   # quick smoke run
#
# Needs: rust (cargo), python3 with pandas (and optionally duckdb):  pip install -r requirements.txt
set -euo pipefail
cd "$(dirname "$0")"

ROWS="${ROWS:-20000000}"
USERS="${USERS:-$((ROWS / 10))}"
DATA="${DATA:-./data}"
BIN=./target/release

echo "== 1/9 build";                    cargo build --release
echo "== 2/9 rust unit + integration tests"; cargo test --release
echo "== 3/9 python oracle (independent CSV parser)"; python3 -I scripts/oracle_test.py

echo "== 4/9 test data: $ROWS rows, $USERS users"
mkdir -p "$DATA"
if [ ! -f "$DATA/orders.csv" ]; then
  "$BIN/csvgen" --rows "$ROWS" --users "$USERS" --out "$DATA/orders.csv" --keys-out "$DATA/keys.txt"
fi

echo "== 5/9 build index, verify against a full rescan, compare counts with pandas"
"$BIN/csvidx" build  "$DATA/orders.csv" --key user_id
"$BIN/csvidx" verify "$DATA/orders.csv"
python3 -I scripts/check_counts.py --csv "$DATA/orders.csv" --keys "$DATA/keys.txt"

echo "== 6/9 external sort: same index under tiny memory limits (4 MiB forces ~77 spilled runs at 20M rows)"
python3 -I scripts/extsort_check.py --csv "$DATA/orders.csv" --limits 4 64 1024

echo "== 7/9 parallel build: same index at 1, 2 and 4 threads"
python3 -I scripts/threads_check.py --csv "$DATA/orders.csv" --threads 1 2 4

echo "== 8/9 benchmark (grep / pandas / duckdb vs csvidx)"
python3 -I scripts/bench.py --csv "$DATA/orders.csv" --keys "$DATA/keys.txt" --out "$DATA/bench.json"

if [ "${SKIP_PY:-0}" = "1" ]; then
  echo "== 9/9 python bindings: skipped (SKIP_PY=1)"
else
  echo "== 9/9 python bindings: build with maturin, pytest, pandas benchmark"
  python3 -m venv --system-site-packages .venv-py
  .venv-py/bin/pip install -q maturin pytest pyarrow pandas duckdb
  (cd python && VIRTUAL_ENV="$PWD/../.venv-py" PATH="$PWD/../.venv-py/bin:$PATH" maturin develop --release)
  .venv-py/bin/python -m pytest -q python/tests
  .venv-py/bin/python python/bench_pandas.py --csv "$DATA/orders.csv" --keys "$DATA/keys.txt" --out "$DATA/pandas_bench.json"
fi
