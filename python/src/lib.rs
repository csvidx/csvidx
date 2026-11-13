//! Python bindings: thin layer over the `csvidx` library. The user-facing API
//! (pandas / pyarrow conversion, key normalisation) lives in `csvidx/__init__.py`.

use csvidx::index::{self, BuildOptions, CompactPolicy, ScanPar, UpdateOptions};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};
use std::path::PathBuf;

create_exception!(_native, CsvIdxError, PyException, "Raised for any csvidx failure (stale or missing index, bad key column, I/O).");

fn err(e: anyhow::Error) -> PyErr {
    CsvIdxError::new_err(format!("{e:#}"))
}

fn keys_from(list: &Bound<'_, PyList>) -> PyResult<Vec<Vec<u8>>> {
    list.iter()
        .map(|k| {
            k.cast::<PyBytes>()
                .map(|b| b.as_bytes().to_vec())
                .map_err(|_| pyo3::exceptions::PyTypeError::new_err("keys must be bytes (the Python wrapper converts str/int)"))
        })
        .collect()
}

#[pyclass(name = "Index", module = "csvidx._native")]
struct PyIndex {
    inner: index::Index,
}

#[pymethods]
impl PyIndex {
    #[new]
    fn new(csv: PathBuf) -> PyResult<Self> {
        Ok(PyIndex { inner: index::Index::open(&csv).map_err(err)? })
    }

    /// Column names from the header row, or None.
    fn columns(&self) -> Option<Vec<String>> {
        self.inner.columns()
    }

    fn key_column(&self) -> usize {
        self.inner.key_column()
    }

    /// Entries on disk (main + tail).
    fn __len__(&self) -> usize {
        self.inner.len()
    }

    /// Bytes the CSV has grown by since the index was last updated (0 = fresh).
    fn bytes_behind(&self) -> u64 {
        match self.inner.freshness() {
            index::Freshness::Fresh => 0,
            index::Freshness::Behind(n) => n,
        }
    }

    fn tail_entries(&self) -> usize {
        self.inner.tail_header().map_or(0, |t| t.entry_count as usize)
    }

    /// Raw CSV rows of all given keys, in file order, each row followed by "\n".
    /// Duplicate keys are looked up once. The GIL is released while searching.
    fn get_bytes<'py>(&self, py: Python<'py>, keys: &Bound<'py, PyList>) -> PyResult<Bound<'py, PyBytes>> {
        let mut keys = keys_from(keys)?;
        keys.sort_unstable();
        keys.dedup();
        let out: Vec<u8> = py.detach(|| {
            let mut rows: Vec<(u64, Vec<u8>)> = Vec::new();
            for k in &keys {
                self.inner.lookup_each(k, |off, rec| rows.push((off, rec.to_vec())));
            }
            rows.sort_unstable_by_key(|r| r.0);
            let mut out = Vec::with_capacity(rows.iter().map(|r| r.1.len() + 1).sum());
            for (_, r) in rows {
                out.extend_from_slice(&r);
                out.push(b'\n');
            }
            out
        });
        Ok(PyBytes::new(py, &out))
    }

    /// Number of matching rows per key, in the order of `keys`.
    fn count(&self, py: Python<'_>, keys: &Bound<'_, PyList>) -> PyResult<Vec<usize>> {
        let keys = keys_from(keys)?;
        Ok(py.detach(|| {
            keys.iter()
                .map(|k| {
                    let mut n = 0usize;
                    self.inner.lookup_each(k, |_, _| n += 1);
                    n
                })
                .collect()
        }))
    }
}

#[pyfunction]
#[pyo3(signature = (csv, key, header=true, mem_limit_mib=1024, threads=0))]
fn build<'py>(py: Python<'py>, csv: PathBuf, key: String, header: bool, mem_limit_mib: usize, threads: usize) -> PyResult<Bound<'py, PyDict>> {
    let mut o = BuildOptions::new(&key, header);
    o.max_mem_entries = index::mem_limit_entries(mem_limit_mib);
    o.par.threads = threads;
    let s = py.detach(|| index::build(&csv, &o)).map_err(err)?;
    let d = PyDict::new(py);
    d.set_item("rows", s.rows)?;
    d.set_item("skipped", s.skipped)?;
    d.set_item("key_column", s.key_col)?;
    d.set_item("csv_bytes", s.csv_bytes)?;
    d.set_item("runs", s.runs)?;
    d.set_item("threads", s.threads.max(1))?;
    d.set_item("par_chunks", s.par_chunks)?;
    d.set_item("par_fallbacks", s.par_fallbacks)?;
    d.set_item("secs", s.scan_secs + s.sort_secs + s.write_secs)?;
    Ok(d)
}

#[pyfunction]
#[pyo3(signature = (csv, compact="auto", mem_limit_mib=1024, threads=0))]
fn update<'py>(py: Python<'py>, csv: PathBuf, compact: &str, mem_limit_mib: usize, threads: usize) -> PyResult<Bound<'py, PyDict>> {
    let mut o = UpdateOptions::new(index::default_hash);
    o.max_mem_entries = index::mem_limit_entries(mem_limit_mib);
    o.par.threads = threads;
    o.compact = match compact {
        "auto" => CompactPolicy::Auto { percent: index::DEFAULT_COMPACT_PERCENT },
        "never" => CompactPolicy::Never,
        "always" => CompactPolicy::Always,
        other => return Err(pyo3::exceptions::PyValueError::new_err(format!("compact must be 'auto', 'never' or 'always', got {other:?}"))),
    };
    let u = py.detach(|| index::update_opts(&csv, &o)).map_err(err)?;
    let d = PyDict::new(py);
    d.set_item("up_to_date", u.up_to_date)?;
    d.set_item("added", u.added)?;
    d.set_item("tail_entries", u.tail_entries)?;
    d.set_item("entries_on_disk", u.entries_on_disk)?;
    d.set_item("compacted", u.compacted)?;
    d.set_item("secs", u.secs)?;
    Ok(d)
}

#[pyfunction]
fn compact<'py>(py: Python<'py>, csv: PathBuf) -> PyResult<Bound<'py, PyDict>> {
    let c = py.detach(|| index::compact(&csv, index::default_hash)).map_err(err)?;
    let d = PyDict::new(py);
    d.set_item("merged", c.merged)?;
    d.set_item("entries", c.entries)?;
    d.set_item("secs", c.secs)?;
    Ok(d)
}

#[pyfunction]
#[pyo3(signature = (csv, mem_limit_mib=1024, threads=0))]
fn verify<'py>(py: Python<'py>, csv: PathBuf, mem_limit_mib: usize, threads: usize) -> PyResult<Bound<'py, PyDict>> {
    let par = ScanPar { threads, chunk_bytes: None };
    let r = py
        .detach(|| index::verify_opts(&csv, index::default_hash, index::mem_limit_entries(mem_limit_mib), &par))
        .map_err(err)?;
    let d = PyDict::new(py);
    d.set_item("ok", r.ok())?;
    d.set_item("entries", r.entries)?;
    d.set_item("rescanned", r.rescanned)?;
    d.set_item("mismatches", r.mismatches)?;
    d.set_item("first_mismatch", r.first_mismatch)?;
    Ok(d)
}

#[pymodule]
fn _native(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyIndex>()?;
    m.add_function(wrap_pyfunction!(build, m)?)?;
    m.add_function(wrap_pyfunction!(update, m)?)?;
    m.add_function(wrap_pyfunction!(compact, m)?)?;
    m.add_function(wrap_pyfunction!(verify, m)?)?;
    m.add("CsvIdxError", py.get_type::<CsvIdxError>())?;
    Ok(())
}
