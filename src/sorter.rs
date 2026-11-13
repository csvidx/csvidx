//! External merge sort for `(key_hash, row_offset)` pairs.
//!
//! Pairs are collected in memory up to a limit; a full buffer is sorted and spilled
//! to a temporary *run* file (raw 16-byte little-endian entries). `finish()` returns
//! a sorted iterator: a plain in-memory iterator if nothing was spilled, otherwise a
//! k-way merge over the runs. With more than `FAN_IN` runs, groups of runs are first
//! merged into bigger runs, so the number of open files stays bounded.
//!
//! Run files live next to the index (`<csv>.idx.runN.tmp`) and are removed when the
//! sorter or the iterator is dropped, including on error paths.

use anyhow::{Context, Result};
use rayon::slice::ParallelSliceMut;
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

pub type Entry = (u64, u64);

/// Maximum number of run files open at once during a merge.
const FAN_IN: usize = 128;
/// Total read-buffer budget shared by all runs of one merge.
const MERGE_BUDGET: usize = 64 << 20;

pub struct Sorter {
    buf: Vec<Entry>,
    cap: usize,
    base: PathBuf,
    runs: Vec<PathBuf>,
    seq: usize,
    /// Seconds spent sorting and writing/merging runs (not scanning).
    pub secs: f64,
}

impl Sorter {
    /// `max_entries`: how many 16-byte pairs may be held in memory (min 1).
    /// `reserve_hint`: expected number of pairs, to avoid regrowing the buffer.
    pub fn new(base: &Path, max_entries: usize, reserve_hint: usize) -> Sorter {
        let cap = max_entries.max(1);
        Sorter {
            buf: Vec::with_capacity(reserve_hint.min(cap)),
            cap,
            base: base.to_path_buf(),
            runs: Vec::new(),
            seq: 0,
            secs: 0.0,
        }
    }

    fn run_path(&mut self) -> PathBuf {
        let mut s = self.base.as_os_str().to_owned();
        s.push(format!(".run{}.tmp", self.seq));
        self.seq += 1;
        PathBuf::from(s)
    }

    #[inline]
    pub fn push(&mut self, h: u64, off: u64) -> Result<()> {
        // Spill only when one more entry would exceed the limit, so an input that
        // fits exactly in memory is never written to disk.
        if self.buf.len() >= self.cap {
            self.spill()?;
        }
        self.buf.push((h, off));
        Ok(())
    }

    fn spill(&mut self) -> Result<()> {
        let t = Instant::now();
        self.buf.par_sort_unstable(); // runs on the current rayon pool
        let p = self.run_path();
        self.runs.push(p.clone()); // registered first, so Drop removes it even if writing fails
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&p).with_context(|| format!("create {}", p.display()))?);
        for &(h, o) in &self.buf {
            w.write_all(&h.to_le_bytes())?;
            w.write_all(&o.to_le_bytes())?;
        }
        w.flush()?;
        self.buf.clear();
        self.secs += t.elapsed().as_secs_f64();
        Ok(())
    }

    /// Finish collecting and return everything in sorted order.
    pub fn finish(&mut self) -> Result<SortedIter> {
        if self.runs.is_empty() {
            let t = Instant::now();
            self.buf.par_sort_unstable(); // runs on the current rayon pool
            self.secs += t.elapsed().as_secs_f64();
            return Ok(SortedIter::Mem(std::mem::take(&mut self.buf).into_iter(), 0));
        }
        if !self.buf.is_empty() {
            self.spill()?;
        }
        self.buf = Vec::new(); // give the memory back before merging
        let total_runs = self.runs.len();

        while self.runs.len() > FAN_IN {
            let group: Vec<PathBuf> = self.runs.drain(..FAN_IN).collect();
            let out = self.run_path();
            self.runs.push(out.clone());
            let t = Instant::now();
            let merge = Merge::open(group)?;
            let mut w = BufWriter::with_capacity(1 << 20, File::create(&out).with_context(|| format!("create {}", out.display()))?);
            for e in SortedIter::Merge(merge, 0) {
                let (h, o) = e?;
                w.write_all(&h.to_le_bytes())?;
                w.write_all(&o.to_le_bytes())?;
            }
            w.flush()?;
            self.secs += t.elapsed().as_secs_f64();
        }
        let paths = std::mem::take(&mut self.runs);
        Ok(SortedIter::Merge(Merge::open(paths)?, total_runs))
    }
}

impl Drop for Sorter {
    fn drop(&mut self) {
        for p in &self.runs {
            let _ = fs::remove_file(p);
        }
    }
}

fn read_entry(r: &mut impl Read) -> Result<Option<Entry>> {
    let mut b = [0u8; 16];
    match r.read_exact(&mut b) {
        Ok(()) => Ok(Some((u64::from_le_bytes(b[0..8].try_into().unwrap()), u64::from_le_bytes(b[8..16].try_into().unwrap())))),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub struct Merge {
    heap: BinaryHeap<Reverse<(u64, u64, usize)>>,
    readers: Vec<BufReader<File>>,
    paths: Vec<PathBuf>,
}

impl Merge {
    /// Takes ownership of the run files: they are deleted when the `Merge` is dropped.
    fn open(paths: Vec<PathBuf>) -> Result<Merge> {
        let per = (MERGE_BUDGET / paths.len().max(1)).clamp(64 << 10, 4 << 20);
        let mut m = Merge { heap: BinaryHeap::new(), readers: Vec::new(), paths };
        for i in 0..m.paths.len() {
            let f = File::open(&m.paths[i]).with_context(|| format!("open {}", m.paths[i].display()))?;
            let mut r = BufReader::with_capacity(per, f);
            if let Some((h, o)) = read_entry(&mut r)? {
                m.heap.push(Reverse((h, o, i)));
            }
            m.readers.push(r);
        }
        Ok(m)
    }

    fn next_entry(&mut self) -> Result<Option<Entry>> {
        let Some(Reverse((h, o, i))) = self.heap.pop() else {
            return Ok(None);
        };
        if let Some((nh, no)) = read_entry(&mut self.readers[i])? {
            self.heap.push(Reverse((nh, no, i)));
        }
        Ok(Some((h, o)))
    }
}

impl Drop for Merge {
    fn drop(&mut self) {
        self.readers.clear(); // close files first (matters on Windows)
        for p in &self.paths {
            let _ = fs::remove_file(p);
        }
    }
}

/// Sorted stream of entries; the `usize` is the number of spilled runs (0 = fully in memory).
pub enum SortedIter {
    Mem(std::vec::IntoIter<Entry>, usize),
    Merge(Merge, usize),
}

impl SortedIter {
    pub fn runs(&self) -> usize {
        match self {
            SortedIter::Mem(_, r) | SortedIter::Merge(_, r) => *r,
        }
    }
}

impl Iterator for SortedIter {
    type Item = Result<Entry>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            SortedIter::Mem(it, _) => it.next().map(Ok),
            SortedIter::Merge(m, _) => m.next_entry().transpose(),
        }
    }
}
