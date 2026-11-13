"""csvidx: look up rows of a huge CSV by key without importing it.

    import csvidx
    csvidx.build("orders.csv", key="user_id")        # once; creates orders.csv.idx
    idx = csvidx.Index("orders.csv")
    df = idx.get(8841207)                             # pandas.DataFrame with that user's rows
    df = idx.get([1, 2, 3], dtype={"amount": "float64"})

Keys are compared byte for byte, the way the CSV spells them: ``"007"`` is not ``7``.
Integers are converted with ``str()``; ``bytes`` and ``str`` are used as they are.
"""
from __future__ import annotations

import io
import os
from typing import Any, Iterable, Optional, Union

from . import _native
from ._native import CsvIdxError, build, compact, update, verify

__all__ = ["Index", "CsvIdxError", "build", "update", "compact", "verify"]
__version__ = "0.1.0"

Key = Union[str, bytes, int]


def _key_bytes(k: Any) -> bytes:
    if isinstance(k, bytes):
        return k
    if isinstance(k, str):
        return k.encode("utf-8")
    # bool is an int subclass, but True/False as keys is almost certainly a bug
    if isinstance(k, bool):
        raise TypeError("bool is not a valid key")
    if hasattr(k, "__index__"):  # int, numpy integers
        return str(int(k)).encode("ascii")
    raise TypeError(
        f"key must be str, bytes or int, got {type(k).__name__}; "
        "convert floats yourself, because 7.0 is not the CSV text '7'"
    )


def _keys_list(keys: Union[Key, Iterable[Key]]) -> list:
    if isinstance(keys, (str, bytes)) or hasattr(keys, "__index__"):
        return [_key_bytes(keys)]
    return [_key_bytes(k) for k in keys]


class Index:
    """An opened index of ``csv`` (``<csv>.idx`` and, if present, ``<csv>.idx.tail``).

    The index is memory-mapped and opens in microseconds. After ``csvidx.update(...)`` open a
    new ``Index``: an open one keeps seeing the rows that were indexed when it was opened
    (``bytes_behind`` tells you whether the CSV has grown since).
    """

    def __init__(self, csv: Union[str, "os.PathLike[str]"]):
        self.csv = os.fspath(csv)
        self._n = _native.Index(self.csv)

    # -- metadata ---------------------------------------------------------------------------
    @property
    def columns(self) -> Optional[list]:
        """Column names from the CSV header row, or None if the CSV has no header row."""
        return self._n.columns()

    @property
    def key_column(self) -> int:
        """Zero-based number of the indexed column."""
        return self._n.key_column()

    @property
    def bytes_behind(self) -> int:
        """How many bytes were appended to the CSV after the index was last updated."""
        return self._n.bytes_behind()

    @property
    def tail_entries(self) -> int:
        return self._n.tail_entries()

    def __len__(self) -> int:
        """Number of index entries on disk (main + tail)."""
        return len(self._n)

    def __repr__(self) -> str:
        return f"csvidx.Index({self.csv!r}, entries={len(self)}, key_column={self.key_column})"

    # -- lookups ------------------------------------------------------------------------------
    def count(self, keys: Union[Key, Iterable[Key]]) -> list:
        """Number of rows for each key, in the order given. Reads only the index and the matching rows."""
        return self._n.count(_keys_list(keys))

    def get_bytes(self, keys: Union[Key, Iterable[Key]]) -> bytes:
        """Raw CSV text of the matching rows (file order, each row ended with ``\\n``), without the header."""
        return self._n.get_bytes(_keys_list(keys))

    def get(self, keys: Union[Key, Iterable[Key]], **read_csv_kwargs):
        """Matching rows as a ``pandas.DataFrame`` (file order, columns from the CSV header).

        Extra keyword arguments go to ``pandas.read_csv`` (``dtype=``, ``parse_dates=``, ...).
        Types are inferred from the *matching rows only*, so pass ``dtype=`` if you need the same
        types for every call (e.g. an integer column that is empty in one result).
        """
        import pandas as pd

        raw = self.get_bytes(keys)
        cols = self.columns
        if not raw:
            df = pd.DataFrame({c: pd.Series(dtype="object") for c in cols}) if cols else pd.DataFrame()
            dtype = read_csv_kwargs.get("dtype")
            if dtype is not None and cols:
                df = df.astype(dtype)
            return df
        kw = dict(read_csv_kwargs)
        if cols is not None:
            kw["names"] = cols
        return pd.read_csv(io.BytesIO(raw), header=None, **kw)

    def get_arrow(self, keys: Union[Key, Iterable[Key]], column_types: Optional[dict] = None):
        """Matching rows as a ``pyarrow.Table`` (needs pyarrow). ``column_types``: ``{"col": pa.int64()}``."""
        import pyarrow as pa
        import pyarrow.csv as pacsv

        raw = self.get_bytes(keys)
        cols = self.columns
        if cols is None:
            cols = [f"f{i}" for i in range(0)] or None
        if not raw:
            if cols is None:
                return pa.table({})
            types = column_types or {}
            return pa.table({c: pa.array([], type=types.get(c, pa.string())) for c in cols})
        ropts = pacsv.ReadOptions(column_names=cols) if cols else pacsv.ReadOptions(autogenerate_column_names=True)
        copts = pacsv.ConvertOptions(column_types=column_types or {}, strings_can_be_null=False)
        return pacsv.read_csv(io.BytesIO(raw), read_options=ropts, convert_options=copts)
