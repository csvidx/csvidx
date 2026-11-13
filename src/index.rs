//! Building, updating, verifying and querying the index.
//!
//! Layout on disk:
//! * `file.csv.idx`       main index, written by `build` and `compact`
//! * `file.csv.idx.fence` optional search accelerator of the main index (see [`FENCE_MAGIC`])
//! * `file.csv.idx.tail`  optional tail index written by `update`: only the rows appended
//!                        since the main index was built, so `update` costs O(new rows)
//!
//! A lookup searches both files. If a tail exists, main entries at or after
//! `tail.base_offset` are *shadowed* (ignored): that is how a provisional last row of the
//! main index (a row without trailing newline) is replaced by its rescanned version.

use crate::header::*;
use crate::scan::*;
use crate::sorter::{Entry, Sorter, SortedIter};
use anyhow::{anyhow, bail, ensure, Context, Result};
use memchr::memchr;
use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use xxhash_rust::xxh3::xxh3_64;

/// Key hash function. It must be stable across runs and platforms, so the std
/// `DefaultHasher` is NOT an option. Tests inject a degenerate one to force collisions.
pub type HashFn = fn(&[u8]) -> u64;

pub fn default_hash(b: &[u8]) -> u64 {
    xxh3_64(b)
}

/// How many 16-byte pairs the sorter may keep in RAM by default (1 GiB).
pub const DEFAULT_MAX_MEM_ENTRIES: usize = (1 << 30) / ENTRY_LEN;

/// `update` merges the tail into the main index once it holds this percentage of the main entries.
pub const DEFAULT_COMPACT_PERCENT: u32 = 10;

/// `--mem-limit-mib` -> number of in-memory pairs.
pub fn mem_limit_entries(mib: usize) -> usize {
    (mib.max(1) << 20) / ENTRY_LEN
}

/// `data.csv` -> `data.csv.idx`
pub fn index_path(csv: &Path) -> PathBuf {
    let mut s = csv.as_os_str().to_owned();
    s.push(".idx");
    PathBuf::from(s)
}

/// `data.csv` -> `data.csv.idx.tail`
pub fn fence_path(csv: &Path) -> PathBuf {
    let mut p = index_path(csv).into_os_string();
    p.push(".fence");
    PathBuf::from(p)
}

pub fn tail_path(csv: &Path) -> PathBuf {
    let mut s = csv.as_os_str().to_owned();
    s.push(".idx.tail");
    PathBuf::from(s)
}

fn prefix_hash(buf: &[u8], up_to: u64) -> u64 {
    let n = PREFIX_LEN.min(up_to) as usize;
    xxh3_64(&buf[..n])
}

// ---------------------------------------------------------------------------
// CSV layout helpers
// ---------------------------------------------------------------------------

pub fn header_names(buf: &[u8]) -> Vec<String> {
    let start = if buf.starts_with(UTF8_BOM) { 3 } else { 0 };
    let mut names = Vec::new();
    scan_record(buf, start, |_, f| names.push(String::from_utf8_lossy(&f.value()).into_owned()));
    names
}

/// Offset of the first data row (after BOM and the optional header row).
pub fn data_start(buf: &[u8], has_header: bool) -> usize {
    let bom = if buf.starts_with(UTF8_BOM) { 3 } else { 0 };
    if !has_header {
        return bom;
    }
    match scan_record(buf, bom, |_, _| {}) {
        Some(r) => r.end,
        None => buf.len(),
    }
}

/// Resolve `--key`: a column name (needs a header row) or a zero-based number.
pub fn resolve_key(buf: &[u8], has_header: bool, key: &str) -> Result<usize> {
    if has_header {
        let names = header_names(buf);
        if let Some(i) = names.iter().position(|n| n == key) {
            return Ok(i);
        }
        if let Ok(i) = key.parse::<usize>() {
            ensure!(i < names.len(), "column index {} out of range (header has {} columns)", i, names.len());
            return Ok(i);
        }
        bail!("column {:?} not found in header; available: {}", key, names.join(", "));
    }
    key.parse::<usize>()
        .map_err(|_| anyhow!("without a header row --key must be a zero-based column number, got {:?}", key))
}

// ---------------------------------------------------------------------------
// Scanning rows into (hash, offset) entries
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ScanOut {
    pub rows: u64,
    pub skipped: u64,
    pub blank: u64,
    /// Where an incremental scan must resume. Equals the start of the last record
    /// if that record is not newline-terminated (it is indexed provisionally and
    /// re-scanned on `update`, since the file may still be growing).
    pub resume: usize,
    pub unterminated: bool,
}

/// Scan the records that *start* in `[from, limit)` (the last one may extend past `limit`).
/// `from` must be a true record boundary. `resume` of the result is where the next record starts.
fn scan_range(
    buf: &[u8],
    from: usize,
    limit: usize,
    key_col: usize,
    hash: HashFn,
    sink: &mut impl FnMut(u64, u64) -> Result<()>,
) -> Result<ScanOut> {
    let mut s = ScanOut { resume: from, ..Default::default() };
    let mut pos = from;
    while pos < limit {
        let mut kh: Option<u64> = None;
        let Some(r) = scan_record(buf, pos, |c, f| {
            if c == key_col {
                kh = Some(hash(&f.value()));
            }
        }) else {
            break;
        };
        if r.blank {
            s.blank += 1;
            pos = r.end;
            s.resume = pos;
            continue;
        }
        match kh {
            Some(h) => {
                sink(h, pos as u64)?;
                s.rows += 1;
            }
            None => s.skipped += 1, // row has fewer columns than the key column
        }
        if !r.terminated {
            s.unterminated = true;
            s.resume = pos;
            break;
        }
        pos = r.end;
        s.resume = pos;
    }
    Ok(s)
}

fn scan_entries(
    buf: &[u8],
    from: usize,
    key_col: usize,
    hash: HashFn,
    sink: &mut impl FnMut(u64, u64) -> Result<()>,
) -> Result<ScanOut> {
    scan_range(buf, from, usize::MAX, key_col, hash, sink)
}

/// Average record length in bytes, from a sample at `from`.
fn avg_record_len(buf: &[u8], from: usize) -> Option<f64> {
    let sample = &buf[..buf.len().min(from + (1 << 20))];
    let (mut pos, mut n) = (from, 0usize);
    while n < 20_000 {
        match scan_record(sample, pos, |_, _| {}) {
            Some(r) if r.terminated => {
                pos = r.end;
                n += 1;
            }
            _ => break,
        }
    }
    if n == 0 || pos == from {
        None
    } else {
        Some((pos - from) as f64 / n as f64)
    }
}

fn estimate_rows(buf: &[u8], from: usize) -> usize {
    match avg_record_len(buf, from) {
        None => 1024,
        Some(avg) => {
            let est = ((buf.len() - from) as f64 / avg * 1.1) as usize + 16;
            est.min(buf.len() / 2 + 16)
        }
    }
}

// ---------------------------------------------------------------------------
// Parallel scan: speculative chunks + stitching at the true record boundaries
// ---------------------------------------------------------------------------
//
// A CSV record boundary cannot be found by looking at a chunk alone: a `\n` inside a quoted
// field is not a boundary. So every chunk but the first *guesses* that its first `\n` ends a
// record and scans from there, in parallel. Afterwards the chunks are stitched in order: the true
// start of chunk k is where chunk k-1 really stopped (its first record start >= the chunk
// boundary). If the guess equals the true start, the speculative result is exactly what a
// sequential scan would have produced (scanning is deterministic from a true boundary) and is
// used; otherwise the chunk is rescanned sequentially from the true start ("repair"). The result
// is therefore identical to a sequential scan for ANY input, including malformed quoting;
// only the speed depends on how often guesses fail.

/// Parallel scan settings.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanPar {
    /// Worker threads: 0 = all cores, 1 = plain sequential scan.
    pub threads: usize,
    /// Chunk size in bytes (default: derived from the memory limit). Mainly for tests.
    pub chunk_bytes: Option<usize>,
}

#[derive(Debug, Default, Clone)]
pub struct ParStats {
    pub threads: usize,
    pub chunks: u64,
    /// Chunks whose guessed start was wrong and had to be rescanned sequentially.
    pub fallbacks: u64,
}

/// Below this much data to scan, threads would only add overhead.
const PAR_MIN_BYTES: usize = 8 << 20;

fn in_pool<T: Send>(threads: usize, f: impl FnOnce() -> Result<T> + Send) -> Result<T> {
    let n = if threads == 0 { std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) } else { threads };
    let pool = rayon::ThreadPoolBuilder::new().num_threads(n).build().map_err(|e| anyhow!("cannot create thread pool: {e}"))?;
    pool.install(f)
}

fn scan_chunk(buf: &[u8], start: usize, limit: usize, key_col: usize, hash: HashFn, hint: usize) -> (Result<ScanOut>, Vec<Entry>) {
    let mut v = Vec::with_capacity(hint);
    let r = scan_range(buf, start, limit, key_col, hash, &mut |h, o| {
        v.push((h, o));
        Ok(())
    });
    (r, v)
}

fn scan_parallel(
    buf: &[u8],
    from: usize,
    key_col: usize,
    hash: HashFn,
    sorter: &mut Sorter,
    chunk_bytes: usize,
    round_chunks: usize,
    avg_len: f64,
) -> Result<(ScanOut, ParStats)> {
    let n = buf.len();
    let mut total = ScanOut { resume: from, ..Default::default() };
    let mut st = ParStats { threads: rayon::current_num_threads(), ..Default::default() };
    let hint = ((chunk_bytes as f64 / avg_len) * 1.25) as usize + 16;
    let mut b = from; // a true record boundary
    while b < n {
        // One round = a few chunks per thread; its results are held in memory until stitched.
        let round_end = b.saturating_add(chunk_bytes.saturating_mul(round_chunks)).min(n);
        let nch = (round_end - b).div_ceil(chunk_bytes).max(1);
        let bound = |k: usize| -> usize { if k >= nch { round_end } else { b + k * chunk_bytes } };

        let results: Vec<(usize, Result<ScanOut>, Vec<Entry>)> = (0..nch)
            .into_par_iter()
            .map(|k| {
                let start = if k == 0 {
                    b
                } else {
                    let s = bound(k);
                    // guess: the first '\n' at or after s-1 ends a record
                    memchr(b'\n', &buf[s - 1..]).map_or(n, |i| s + i)
                };
                let (r, v) = scan_chunk(buf, start, bound(k + 1), key_col, hash, hint);
                (start, r, v)
            })
            .collect();

        let mut cur = b;
        for (k, (start, r, v)) in results.into_iter().enumerate() {
            st.chunks += 1;
            let (out, entries) = if start == cur {
                (r?, v)
            } else {
                st.fallbacks += 1;
                let (r2, v2) = scan_chunk(buf, cur, bound(k + 1), key_col, hash, hint);
                (r2?, v2)
            };
            for &(h, o) in &entries {
                sorter.push(h, o)?;
            }
            total.rows += out.rows;
            total.skipped += out.skipped;
            total.blank += out.blank;
            cur = out.resume;
            if out.unterminated {
                // the last record runs to EOF without a newline: nothing can follow it
                total.unterminated = true;
                total.resume = out.resume;
                return Ok((total, st));
            }
        }
        total.resume = cur;
        b = cur;
    }
    Ok((total, st))
}

fn scan_all(buf: &[u8], from: usize, key_col: usize, hash: HashFn, sorter: &mut Sorter, par: &ScanPar, cap: usize) -> Result<(ScanOut, ParStats)> {
    let threads = rayon::current_num_threads();
    let remaining = buf.len().saturating_sub(from);
    if threads <= 1 || (par.chunk_bytes.is_none() && remaining < PAR_MIN_BYTES) {
        let so = scan_entries(buf, from, key_col, hash, &mut |h, o| sorter.push(h, o))?;
        return Ok((so, ParStats { threads: 1, ..Default::default() }));
    }
    let avg = avg_record_len(buf, from).unwrap_or(64.0).max(4.0);
    let (chunk_bytes, round_chunks) = match par.chunk_bytes {
        Some(c) => (c.max(1), threads * 4),
        None => {
            // a round may hold up to a quarter of the memory limit in entries
            let round_bytes = (((cap / 4) as f64 * avg) as usize).clamp(4 << 20, 1 << 30);
            let chunk = (round_bytes / (threads * 4)).clamp(256 << 10, 16 << 20);
            (chunk, (round_bytes / chunk).max(threads * 2))
        }
    };
    scan_parallel(buf, from, key_col, hash, sorter, chunk_bytes, round_chunks, avg)
}

struct Scanned {
    so: ScanOut,
    par: ParStats,
    sorted: SortedIter,
    scan_secs: f64,
    sort_secs: f64,
}

/// Scan `buf[from..]` (in parallel if allowed) and return all entries sorted. The whole thing runs
/// in a rayon pool of `par.threads` threads, which the sorter's parallel sort uses as well.
fn scan_sorted(buf: &[u8], from: usize, key_col: usize, hash: HashFn, base: &Path, cap: usize, par: &ScanPar) -> Result<Scanned> {
    in_pool(par.threads, || {
        let t0 = Instant::now();
        let mut sorter = Sorter::new(base, cap, estimate_rows(buf, from));
        let (so, pst) = scan_all(buf, from, key_col, hash, &mut sorter, par, cap)?;
        let scan_total = t0.elapsed().as_secs_f64();
        let during = sorter.secs; // sorting/spilling that already happened while scanning
        let sorted = sorter.finish()?;
        Ok(Scanned { so, par: pst, sorted, scan_secs: (scan_total - during).max(0.0), sort_secs: sorter.secs })
    })
}

// ---------------------------------------------------------------------------
// Fence file: the first hash of every 4 KiB page of the main index
// ---------------------------------------------------------------------------

/// `file.csv.idx.fence`: a small, purely optional cache. Layout: magic (8), xxh3 of the main
/// index header (8), leaf count (8), xxh3 of the fence data (8), then one u64 per leaf.
/// Leaf `j` is one 4 KiB page of the main index (entries start at byte 64, so leaf `j` covers
/// entries `[256j-4, 256j+252)`) and its fence is the hash of its first entry. A lookup binary
/// searches the fences (a ~3 MiB file per 100M rows, memory-mapped) and then reads exactly one index page instead
/// of ~20 scattered ones. A missing, stale or damaged fence file is ignored: lookups just fall
/// back to the plain binary search, so it can never change results.
pub const FENCE_MAGIC: &[u8; 8] = b"CSVFENC1";
const FENCE_HEADER: usize = 32;
const LEAF_ENTRIES: usize = 256;
const FIRST_LEAF_SHORT: usize = HEADER_LEN / ENTRY_LEN; // 4

fn write_fence(path: &Path, main_hdr: &[u8], fences: &[u64]) -> Result<()> {
    let mut data = Vec::with_capacity(fences.len() * 8);
    for f in fences {
        data.extend_from_slice(&f.to_le_bytes());
    }
    let mut out = Vec::with_capacity(FENCE_HEADER + data.len());
    out.extend_from_slice(FENCE_MAGIC);
    out.extend_from_slice(&xxh3_64(main_hdr).to_le_bytes());
    out.extend_from_slice(&(fences.len() as u64).to_le_bytes());
    out.extend_from_slice(&xxh3_64(&data).to_le_bytes());
    out.extend_from_slice(&data);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, out).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename into {}", path.display()))?;
    Ok(())
}

/// The memory-mapped fence file for `main`, if it exists and its header matches. The data itself
/// is NOT read here (that would cost a 3 MiB sequential read on every open of a cold index);
/// `verify` checks it against its checksum and against the main index.
fn load_fence(path: &Path, main_hdr: &[u8], entry_count: u64) -> Option<Mmap> {
    let f = File::open(path).ok()?;
    let b = unsafe { Mmap::map(&f).ok()? };
    if b.len() < FENCE_HEADER || &b[0..8] != FENCE_MAGIC {
        return None;
    }
    let u = |r: std::ops::Range<usize>| u64::from_le_bytes(b[r].try_into().unwrap());
    let leaves = u(16..24) as usize;
    let want = (entry_count as usize + FIRST_LEAF_SHORT).div_ceil(LEAF_ENTRIES);
    if u(8..16) != xxh3_64(main_hdr) || leaves != want || leaves == 0 || b.len() != FENCE_HEADER + leaves * 8 {
        return None;
    }
    #[cfg(unix)]
    let _ = b.advise(memmap2::Advice::Random);
    Some(b)
}

#[inline]
fn fence_at(b: &[u8], j: usize) -> u64 {
    let p = FENCE_HEADER + j * 8;
    u64::from_le_bytes(b[p..p + 8].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// Index file writer (tmp file + atomic rename)
// ---------------------------------------------------------------------------

struct IdxWriter {
    w: BufWriter<File>,
    tmp: PathBuf,
    count: u64,
    /// Some(..) for a main index: collects the first hash of every 4 KiB page.
    fences: Option<Vec<u64>>,
}

impl IdxWriter {
    fn create(final_path: &Path) -> Result<Self> {
        let mut tmp = final_path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        let mut w = BufWriter::with_capacity(1 << 20, f);
        w.write_all(&[0u8; HEADER_LEN])?; // placeholder, patched in finish()
        Ok(IdxWriter { w, tmp, count: 0, fences: None })
    }

    fn with_fences(mut self) -> Self {
        self.fences = Some(Vec::new());
        self
    }

    #[inline]
    fn push(&mut self, h: u64, off: u64) -> Result<()> {
        if let Some(f) = &mut self.fences {
            if self.count == 0 || (self.count as usize + FIRST_LEAF_SHORT) % LEAF_ENTRIES == 0 {
                f.push(h);
            }
        }
        self.w.write_all(&h.to_le_bytes())?;
        self.w.write_all(&off.to_le_bytes())?;
        self.count += 1;
        Ok(())
    }

    fn finish(self, mut hdr: Header, final_path: &Path) -> Result<()> {
        hdr.entry_count = self.count;
        let mut f = self.w.into_inner().map_err(|e| e.into_error())?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&hdr.encode())?;
        f.sync_all()?;
        drop(f);
        if let Some(fences) = &self.fences {
            // A fence file that does not match the main index is ignored, so the order of the two
            // renames does not matter for correctness; the fence goes first.
            let fp = PathBuf::from({
                let mut p = final_path.as_os_str().to_owned();
                p.push(".fence");
                p
            });
            if fences.is_empty() {
                let _ = fs::remove_file(&fp);
            } else {
                write_fence(&fp, &hdr.encode(), fences)?;
            }
        }
        fs::rename(&self.tmp, final_path).with_context(|| format!("rename into {}", final_path.display()))?;
        Ok(())
    }
}

fn drain_into(it: impl Iterator<Item = Result<Entry>>, w: &mut IdxWriter) -> Result<()> {
    for e in it {
        let (h, o) = e?;
        w.push(h, o)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Run: one memory-mapped index file; Merge2: merge of two sorted streams
// ---------------------------------------------------------------------------

struct Run {
    map: Mmap,
    hdr: Header,
    fences: Option<Mmap>,
}

impl Run {
    fn open(path: &Path) -> Result<Run> {
        let f = File::open(path).with_context(|| format!("open index {} (run `csvidx build` first)", path.display()))?;
        let map = unsafe { Mmap::map(&f)? };
        let hdr = Header::decode(&map).map_err(|e| anyhow!("{}: {}", path.display(), e))?;
        ensure!(
            map.len() as u64 == HEADER_LEN as u64 + hdr.entry_count * ENTRY_LEN as u64,
            "{}: file size does not match its header (truncated or corrupt)",
            path.display()
        );
        Ok(Run { map, hdr, fences: None })
    }

    fn len(&self) -> usize {
        self.hdr.entry_count as usize
    }

    #[inline]
    fn entry(&self, i: usize) -> Entry {
        let p = HEADER_LEN + i * ENTRY_LEN;
        let b = &self.map[p..p + ENTRY_LEN];
        (u64::from_le_bytes(b[0..8].try_into().unwrap()), u64::from_le_bytes(b[8..16].try_into().unwrap()))
    }

    fn lower_bound(&self, h: u64) -> usize {
        let (mut lo, mut hi) = (0usize, self.len());
        if let Some(fences) = &self.fences {
            // largest leaf whose first hash is < h (leaf 0 if none); the answer lies in that leaf
            // or exactly at its end
            let leaves = (fences.len() - FENCE_HEADER) / 8;
            let (mut a, mut b) = (0usize, leaves);
            while a < b {
                let mid = a + (b - a) / 2;
                if fence_at(fences, mid) < h {
                    a = mid + 1;
                } else {
                    b = mid;
                }
            }
            let j = a.saturating_sub(1);
            lo = if j == 0 { 0 } else { LEAF_ENTRIES * j - FIRST_LEAF_SHORT };
            hi = (LEAF_ENTRIES * (j + 1) - FIRST_LEAF_SHORT).min(hi);
        }
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.entry(mid).0 < h {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

/// Entries of `run` (if any) whose row offset is below `limit`, in index order.
fn run_iter<'a>(run: Option<&'a Run>, limit: u64) -> impl Iterator<Item = Result<Entry>> + 'a {
    let n = run.map_or(0, |r| r.len());
    (0..n).filter_map(move |i| {
        let e = run.unwrap().entry(i);
        if e.1 < limit {
            Some(Ok::<Entry, anyhow::Error>(e))
        } else {
            None
        }
    })
}

/// Merge of two streams that are each sorted by `(hash, offset)`.
struct Merge2<A, B> {
    a: A,
    b: B,
    x: Option<Entry>,
    y: Option<Entry>,
    primed: bool,
}

impl<A, B> Merge2<A, B>
where
    A: Iterator<Item = Result<Entry>>,
    B: Iterator<Item = Result<Entry>>,
{
    fn new(a: A, b: B) -> Self {
        Merge2 { a, b, x: None, y: None, primed: false }
    }
}

impl<A, B> Iterator for Merge2<A, B>
where
    A: Iterator<Item = Result<Entry>>,
    B: Iterator<Item = Result<Entry>>,
{
    type Item = Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.primed {
            self.primed = true;
            match self.a.next().transpose() {
                Ok(v) => self.x = v,
                Err(e) => return Some(Err(e)),
            }
            match self.b.next().transpose() {
                Ok(v) => self.y = v,
                Err(e) => return Some(Err(e)),
            }
        }
        let take_x = match (self.x, self.y) {
            (None, None) => return None,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(x), Some(y)) => x <= y,
        };
        if take_x {
            let v = self.x.unwrap();
            match self.a.next().transpose() {
                Ok(n) => self.x = n,
                Err(e) => return Some(Err(e)),
            }
            Some(Ok(v))
        } else {
            let v = self.y.unwrap();
            match self.b.next().transpose() {
                Ok(n) => self.y = n,
                Err(e) => return Some(Err(e)),
            }
            Some(Ok(v))
        }
    }
}

// ---------------------------------------------------------------------------
// Build
// ---------------------------------------------------------------------------

pub struct BuildOptions {
    pub key: String,
    pub has_header: bool,
    pub hash: HashFn,
    /// Pairs kept in RAM before spilling a sorted run to disk.
    pub max_mem_entries: usize,
    pub par: ScanPar,
}

impl BuildOptions {
    pub fn new(key: &str, has_header: bool) -> Self {
        BuildOptions {
            key: key.to_string(),
            has_header,
            hash: default_hash,
            max_mem_entries: DEFAULT_MAX_MEM_ENTRIES,
            par: ScanPar::default(),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct BuildStats {
    pub csv_bytes: u64,
    pub rows: u64,
    pub skipped: u64,
    pub blank: u64,
    pub unterminated_tail: bool,
    pub key_col: usize,
    /// Number of spilled sorted runs; 0 means everything was sorted in memory.
    pub runs: usize,
    /// Threads used, chunks scanned and chunks that needed a sequential repair (see `ScanPar`).
    pub threads: usize,
    pub par_chunks: u64,
    pub par_fallbacks: u64,
    pub scan_secs: f64,
    pub sort_secs: f64,
    pub write_secs: f64,
}

pub fn build(csv: &Path, opts: &BuildOptions) -> Result<BuildStats> {
    let file = File::open(csv).with_context(|| format!("open {}", csv.display()))?;
    let meta = file.metadata()?;
    ensure!(meta.len() > 0, "CSV file is empty");
    let map = unsafe { Mmap::map(&file)? };
    #[cfg(unix)]
    let _ = map.advise(memmap2::Advice::Sequential);
    let buf: &[u8] = &map;

    let key_col = resolve_key(buf, opts.has_header, &opts.key)?;
    let start = data_start(buf, opts.has_header);

    let ip = index_path(csv);
    let Scanned { so, par, sorted, scan_secs, sort_secs } =
        scan_sorted(buf, start, key_col, opts.hash, &ip, opts.max_mem_entries, &opts.par)?;
    let runs = sorted.runs();

    let t2 = Instant::now();
    let mut w = IdxWriter::create(&ip)?.with_fences();
    drain_into(sorted, &mut w)?;
    let hdr = Header {
        flags: if opts.has_header { FLAG_HAS_HEADER } else { 0 },
        csv_size: buf.len() as u64,
        base_offset: start as u64,
        indexed_up_to: so.resume as u64,
        entry_count: 0,
        prefix_hash: prefix_hash(buf, so.resume as u64),
        key_col: key_col as u32,
    };
    w.finish(hdr, &ip)?;
    // A tail belongs to the previous main index. If we crash before this line, the tail is
    // recognised as stale at open time (its base_offset no longer matches).
    let _ = fs::remove_file(tail_path(csv));
    let write_secs = t2.elapsed().as_secs_f64();

    Ok(BuildStats {
        csv_bytes: buf.len() as u64,
        rows: so.rows,
        skipped: so.skipped,
        blank: so.blank,
        unterminated_tail: so.unterminated,
        key_col,
        runs,
        threads: par.threads,
        par_chunks: par.chunks,
        par_fallbacks: par.fallbacks,
        scan_secs,
        sort_secs,
        write_secs,
    })
}

// ---------------------------------------------------------------------------
// Index (read side)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Freshness {
    Fresh,
    /// The CSV grew by this many bytes since indexing (`update` will catch up).
    Behind(u64),
}

pub struct Index {
    csv: Mmap,
    main: Run,
    tail: Option<Run>,
    hash: HashFn,
}

impl Index {
    pub fn open(csv: &Path) -> Result<Index> {
        Self::open_with_hash(csv, default_hash)
    }

    pub fn open_with_hash(csv: &Path, hash: HashFn) -> Result<Index> {
        let ip = index_path(csv);
        let mut main = Run::open(&ip)?;
        main.fences = load_fence(&fence_path(csv), &main.map[..HEADER_LEN], main.hdr.entry_count);
        ensure!(!main.hdr.is_tail(), "{} is a tail file, not a main index", ip.display());
        let h = &main.hdr;
        ensure!(h.indexed_up_to <= h.csv_size, "corrupt index header (indexed_up_to > csv_size)");

        let cfile = File::open(csv).with_context(|| format!("open {}", csv.display()))?;
        let cmap = unsafe { Mmap::map(&cfile).with_context(|| format!("mmap {}", csv.display()))? };
        let clen = cmap.len() as u64;
        if clen < h.csv_size {
            bail!(
                "index is stale: the CSV is now smaller ({} bytes) than when it was indexed ({} bytes); rebuild with `csvidx build`",
                clen,
                h.csv_size
            );
        }
        if prefix_hash(&cmap, h.indexed_up_to) != h.prefix_hash {
            bail!("index is stale: the already-indexed part of the CSV has changed; rebuild with `csvidx build`");
        }

        let tp = tail_path(csv);
        let tail = if tp.exists() {
            let t = Run::open(&tp)?;
            let th = &t.hdr;
            ensure!(th.is_tail(), "{} is not a tail file", tp.display());
            ensure!(th.key_col == h.key_col, "{} was built for another key column; run `csvidx build`", tp.display());
            if th.base_offset != h.indexed_up_to {
                // Left over from an interrupted `compact` or `build`: the main index already
                // covers (or replaced) what this tail describes. Ignore it; `update` rewrites it.
                None
            } else {
                ensure!(
                    th.csv_size >= h.csv_size && th.csv_size <= clen && th.indexed_up_to >= th.base_offset && th.indexed_up_to <= th.csv_size,
                    "tail index {} is inconsistent; run `csvidx build`",
                    tp.display()
                );
                if prefix_hash(&cmap, th.indexed_up_to) != th.prefix_hash {
                    bail!("tail index is stale: the CSV has changed; rebuild with `csvidx build`");
                }
                Some(t)
            }
        } else {
            None
        };

        #[cfg(unix)]
        {
            let _ = main.map.advise(memmap2::Advice::Random);
            if let Some(t) = &tail {
                let _ = t.map.advise(memmap2::Advice::Random);
            }
            let _ = cmap.advise(memmap2::Advice::Random);
        }
        Ok(Index { csv: cmap, main, tail, hash })
    }

    /// Header of the main index.
    pub fn header(&self) -> &Header {
        &self.main.hdr
    }

    /// Column names from the CSV header row, or `None` if the file has no header row.
    pub fn columns(&self) -> Option<Vec<String>> {
        if self.main.hdr.has_header() {
            Some(header_names(&self.csv))
        } else {
            None
        }
    }

    /// Zero-based number of the indexed key column.
    pub fn key_column(&self) -> usize {
        self.main.hdr.key_col as usize
    }

    /// Header of the tail index, if there is a valid one.
    pub fn tail_header(&self) -> Option<&Header> {
        self.tail.as_ref().map(|t| &t.hdr)
    }

    /// `open` tunes the mappings for random access (lookups). Bulk passes over the
    /// whole index (update, verify, compact) need readahead instead, or every 4 KiB page
    /// becomes its own synchronous read.
    fn advise_sequential(&self) {
        #[cfg(unix)]
        {
            let _ = self.main.map.advise(memmap2::Advice::Sequential);
            if let Some(t) = &self.tail {
                let _ = t.map.advise(memmap2::Advice::Sequential);
            }
            let _ = self.csv.advise(memmap2::Advice::Sequential);
        }
    }

    /// Entries on disk (main + tail). May count one provisional last row twice until `compact`.
    pub fn len(&self) -> usize {
        self.main.len() + self.tail.as_ref().map_or(0, |t| t.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// CSV size the newest index file knows about.
    fn covered_size(&self) -> u64 {
        self.tail.as_ref().map_or(self.main.hdr.csv_size, |t| t.hdr.csv_size)
    }

    /// Main entries at or after this offset are shadowed by the tail.
    fn main_limit(&self) -> u64 {
        if self.tail.is_some() {
            self.main.hdr.indexed_up_to
        } else {
            u64::MAX
        }
    }

    pub fn freshness(&self) -> Freshness {
        let clen = self.csv.len() as u64;
        let covered = self.covered_size();
        if clen == covered {
            Freshness::Fresh
        } else {
            Freshness::Behind(clen - covered)
        }
    }

    fn lookup_run(&self, run: &Run, limit: u64, h: u64, key: &[u8], f: &mut impl FnMut(u64, &[u8])) {
        let key_col = run.hdr.key_col as usize;
        let buf: &[u8] = &self.csv;
        let n = run.len();
        let mut i = run.lower_bound(h);
        while i < n {
            let (eh, off) = run.entry(i);
            if eh != h {
                break;
            }
            i += 1;
            if off >= limit {
                continue;
            }
            let off = off as usize;
            if off >= buf.len() {
                continue;
            }
            let mut matched = false;
            let r = scan_record(buf, off, |c, fld| {
                if c == key_col && &*fld.value() == key {
                    matched = true;
                }
            });
            if let (true, Some(r)) = (matched, r) {
                f(off as u64, &buf[off..r.line_end]);
            }
        }
    }

    /// Call `f(row_offset, raw_record_bytes)` for every row whose key equals `key`,
    /// in file order. Hash collisions are filtered out by comparing the real key.
    pub fn lookup_each(&self, key: &[u8], mut f: impl FnMut(u64, &[u8])) {
        let h = (self.hash)(key);
        // main offsets are all below tail offsets, so this order is file order
        self.lookup_run(&self.main, self.main_limit(), h, key, &mut f);
        if let Some(t) = &self.tail {
            self.lookup_run(t, u64::MAX, h, key, &mut f);
        }
    }

    pub fn lookup(&self, key: &[u8]) -> Vec<(u64, Vec<u8>)> {
        let mut v = Vec::new();
        self.lookup_each(key, |o, r| v.push((o, r.to_vec())));
        v
    }
}

// ---------------------------------------------------------------------------
// Incremental update (CSV only grows): writes only the tail index
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum CompactPolicy {
    /// Merge the tail into the main index once it holds `percent`% of the main entries.
    Auto { percent: u32 },
    Never,
    Always,
}

pub struct UpdateOptions {
    pub hash: HashFn,
    pub max_mem_entries: usize,
    pub compact: CompactPolicy,
    pub par: ScanPar,
}

impl UpdateOptions {
    pub fn new(hash: HashFn) -> Self {
        UpdateOptions {
            hash,
            max_mem_entries: DEFAULT_MAX_MEM_ENTRIES,
            compact: CompactPolicy::Auto { percent: DEFAULT_COMPACT_PERCENT },
            par: ScanPar::default(),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct UpdateStats {
    pub up_to_date: bool,
    pub added: u64,
    /// Entries of the previous tail that were dropped and rescanned (last row without newline).
    pub dropped_provisional: u64,
    /// Entries in the tail file afterwards (0 if it was merged into the main index).
    pub tail_entries: u64,
    /// Entries in main + tail afterwards; can include one provisional row twice until `compact`.
    pub entries_on_disk: u64,
    pub compacted: bool,
    pub secs: f64,
}

pub fn update(csv: &Path, hash: HashFn) -> Result<UpdateStats> {
    update_opts(csv, &UpdateOptions::new(hash))
}

pub fn update_with(csv: &Path, hash: HashFn, max_mem_entries: usize) -> Result<UpdateStats> {
    let mut o = UpdateOptions::new(hash);
    o.max_mem_entries = max_mem_entries;
    update_opts(csv, &o)
}

/// Index only the bytes appended since the last build/update into `<file>.idx.tail`.
/// The main index is not touched (unless the compaction policy merges the tail into it).
pub fn update_opts(csv: &Path, o: &UpdateOptions) -> Result<UpdateStats> {
    let t0 = Instant::now();
    let old = Index::open_with_hash(csv, o.hash)?;
    old.advise_sequential();
    let clen = old.csv.len() as u64;
    if clen == old.covered_size() {
        return Ok(UpdateStats {
            up_to_date: true,
            tail_entries: old.tail.as_ref().map_or(0, |t| t.len()) as u64,
            entries_on_disk: old.len() as u64,
            secs: t0.elapsed().as_secs_f64(),
            ..Default::default()
        });
    }

    let mh = old.main.hdr.clone();
    // Scanning resumes where the newest file stopped. Entries of the existing tail at or after
    // that point are provisional (last row without newline) and are replaced by the rescan.
    let from = old.tail.as_ref().map_or(mh.indexed_up_to, |t| t.hdr.indexed_up_to);
    let buf: &[u8] = &old.csv;
    let ip = index_path(csv);
    let tp = tail_path(csv);

    let Scanned { so, sorted: fresh, .. } =
        scan_sorted(buf, from as usize, mh.key_col as usize, o.hash, &ip, o.max_mem_entries, &o.par)?;

    let dropped = old.tail.as_ref().map_or(0, |t| (0..t.len()).filter(|&i| t.entry(i).1 >= from).count() as u64);
    let mut w = IdxWriter::create(&tp)?;
    drain_into(Merge2::new(run_iter(old.tail.as_ref(), from), fresh), &mut w)?;
    let tail_entries = w.count;
    let thdr = Header {
        flags: mh.flags | FLAG_TAIL,
        csv_size: clen,
        base_offset: mh.indexed_up_to,
        indexed_up_to: so.resume as u64,
        entry_count: 0,
        prefix_hash: prefix_hash(buf, so.resume as u64),
        key_col: mh.key_col,
    };
    w.finish(thdr, &tp)?;
    let main_entries = old.main.len() as u64;
    drop(old);

    let compact_now = match o.compact {
        CompactPolicy::Never => false,
        CompactPolicy::Always => true,
        CompactPolicy::Auto { percent } => tail_entries * 100 >= main_entries * percent as u64,
    };
    let (tail_left, on_disk) = if compact_now {
        let c = compact(csv, o.hash)?;
        (0, c.entries)
    } else {
        (tail_entries, main_entries + tail_entries)
    };
    Ok(UpdateStats {
        up_to_date: false,
        added: so.rows,
        dropped_provisional: dropped,
        tail_entries: tail_left,
        entries_on_disk: on_disk,
        compacted: compact_now,
        secs: t0.elapsed().as_secs_f64(),
    })
}

// ---------------------------------------------------------------------------
// Compact: merge the tail into the main index
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct CompactStats {
    /// A tail existed and was merged.
    pub merged: bool,
    pub entries: u64,
    pub secs: f64,
}

/// Sequential merge of main and tail into a new main index (atomic rename), then the tail is deleted.
pub fn compact(csv: &Path, hash: HashFn) -> Result<CompactStats> {
    let t0 = Instant::now();
    let ip = index_path(csv);
    let tp = tail_path(csv);
    let idx = Index::open_with_hash(csv, hash)?;
    let Some(tail) = idx.tail.as_ref() else {
        let _ = fs::remove_file(&tp); // a stale tail, if any
        return Ok(CompactStats { merged: false, entries: idx.len() as u64, secs: t0.elapsed().as_secs_f64() });
    };
    idx.advise_sequential();
    let mh = &idx.main.hdr;
    let hdr = Header {
        flags: mh.flags & !FLAG_TAIL,
        csv_size: tail.hdr.csv_size,
        base_offset: mh.base_offset,
        indexed_up_to: tail.hdr.indexed_up_to,
        entry_count: 0,
        prefix_hash: tail.hdr.prefix_hash,
        key_col: mh.key_col,
    };
    let mut w = IdxWriter::create(&ip)?.with_fences();
    drain_into(Merge2::new(run_iter(Some(&idx.main), idx.main_limit()), run_iter(Some(tail), u64::MAX)), &mut w)?;
    let entries = w.count;
    w.finish(hdr, &ip)?;
    drop(idx);
    let _ = fs::remove_file(&tp);
    Ok(CompactStats { merged: true, entries, secs: t0.elapsed().as_secs_f64() })
}

// ---------------------------------------------------------------------------
// Verify: the index (main + tail) must equal a fresh full rescan
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct VerifyReport {
    /// Logical entries (shadowed ones not counted).
    pub entries: u64,
    pub rescanned: u64,
    pub sorted: bool,
    pub mismatches: u64,
    pub first_mismatch: Option<String>,
    /// The fence file exists but does not match the main index (lookups ignore it; rebuild to fix).
    pub fence_problem: Option<String>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.sorted && self.mismatches == 0 && self.entries == self.rescanned && self.fence_problem.is_none()
    }
}

pub fn verify(csv: &Path, hash: HashFn) -> Result<VerifyReport> {
    verify_with(csv, hash, DEFAULT_MAX_MEM_ENTRIES)
}

pub fn verify_with(csv: &Path, hash: HashFn, max_mem_entries: usize) -> Result<VerifyReport> {
    verify_opts(csv, hash, max_mem_entries, &ScanPar::default())
}

pub fn verify_opts(csv: &Path, hash: HashFn, max_mem_entries: usize, par: &ScanPar) -> Result<VerifyReport> {
    let idx = Index::open_with_hash(csv, hash)?;
    idx.advise_sequential();
    let h = idx.main.hdr.clone();
    let buf = &idx.csv[..idx.covered_size() as usize];
    let start = data_start(buf, h.has_header());
    let ip = index_path(csv);
    let mut fresh = scan_sorted(buf, start, h.key_col as usize, hash, &ip, max_mem_entries, par)?.sorted;

    let logical = Merge2::new(run_iter(Some(&idx.main), idx.main_limit()), run_iter(idx.tail.as_ref(), u64::MAX));
    let mut rep = VerifyReport { sorted: true, ..Default::default() };
    let mut prev: Option<Entry> = None;
    for e in logical {
        let e = e?;
        if let Some(p) = prev {
            if p > e {
                rep.sorted = false;
            }
        }
        prev = Some(e);
        let f = fresh.next().transpose()?;
        if f.is_some() {
            rep.rescanned += 1;
        }
        if f != Some(e) {
            if rep.first_mismatch.is_none() {
                rep.first_mismatch = Some(format!("entry #{}: index has {:?}, rescan has {:?}", rep.entries, e, f));
            }
            rep.mismatches += 1;
        }
        rep.entries += 1;
    }
    for e in fresh {
        e?;
        rep.rescanned += 1; // rows the rescan found beyond the end of the index
    }
    rep.fence_problem = check_fence(csv, &idx.main);
    Ok(rep)
}

/// None if there is no fence file or it is exactly what a fresh build would have written.
fn check_fence(csv: &Path, main: &Run) -> Option<String> {
    let fp = fence_path(csv);
    let raw = fs::read(&fp).ok()?;
    let fix = "ignored by lookups; run `csvidx build` to recreate it";
    let Some(map) = &main.fences else {
        return Some(format!("{} does not match the main index ({fix})", fp.display()));
    };
    let leaves = (map.len() - FENCE_HEADER) / 8;
    let data_sum = u64::from_le_bytes(raw[24..32].try_into().unwrap());
    if xxh3_64(&raw[FENCE_HEADER..]) != data_sum {
        return Some(format!("{} fails its checksum ({fix})", fp.display()));
    }
    for j in 0..leaves {
        let start = if j == 0 { 0 } else { LEAF_ENTRIES * j - FIRST_LEAF_SHORT };
        if fence_at(map, j) != main.entry(start).0 {
            return Some(format!("{}: fence #{j} differs from the index ({fix})", fp.display()));
        }
    }
    None
}
