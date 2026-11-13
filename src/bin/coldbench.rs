//! Where does a lookup spend its time when the page cache is cold?
//!
//! Splits every lookup into two phases and counts the major page faults (= real disk reads) of each:
//!   search : find the first index entry with the key's hash (binary search in the index file)
//!   fetch  : read the matching entries and the CSV rows they point to
//! and compares two ways to do the search phase:
//!   flat   : binary search over the whole index file (what csvidx does now)
//!   fenced : a prototype of what a static B+-tree boils down to: one "fence" key per 4 KiB index
//!            page in a separate small file (<csv>.idx.fence); binary search the fences, then read
//!            exactly one index page. Leaves are page-aligned (entry 256*j-4 starts page j).
//!
//! Cache states (each lookup runs in a fresh mapping; files are evicted with posix_fadvise(DONTNEED)):
//!   cold      : index, fences and CSV evicted before every lookup
//!   csv-cold  : index and fences fully cached, CSV evicted before every lookup
//!   service   : index evicted once, then `--warm-lookups` untimed lookups load the pages a running
//!               service would have cached (upper levels of the search); CSV evicted before every lookup
//!   hot       : everything cached
//!
//! Linux only.

#[cfg(not(unix))]
fn main() {
    eprintln!("coldbench needs posix_fadvise and /proc: Linux only");
}

#[cfg(unix)]
fn main() {
    real::main()
}

#[cfg(unix)]
mod real {
    use clap::Parser;
    use csvidx::header::*;
    use csvidx::scan::scan_record;
    use memmap2::{Advice, Mmap};
    use std::fs::File;
    use std::os::unix::io::AsRawFd;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    const LEAF: usize = 256; // 256 entries * 16 B = one 4 KiB page
    const FIRST_LEAF_SHORT: usize = 4; // header is 64 B = 4 entries, so page j starts at entry 256*j - 4

    #[derive(Parser)]
    #[command(about = "Cold-cache lookup profile: index search vs row fetch; flat binary search vs page fences")]
    struct Args {
        csv: PathBuf,
        keys_file: PathBuf,
        /// timed lookups per (state, mode)
        #[arg(long, default_value_t = 300)]
        n: usize,
        /// untimed lookups that warm the index for the `service` state
        #[arg(long, default_value_t = 2000)]
        warm_lookups: usize,
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Flat,
        Fenced,
    }

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum State {
        Cold,
        CsvCold,
        Service,
        Hot,
    }

    fn evict(path: &Path) {
        let f = File::open(path).unwrap();
        unsafe {
            libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
    }

    fn read_through(path: &Path) {
        use std::io::Read;
        let mut f = File::open(path).unwrap();
        let mut buf = vec![0u8; 8 << 20];
        while f.read(&mut buf).unwrap() > 0 {}
    }

    fn majflt() -> u64 {
        let s = std::fs::read_to_string("/proc/self/stat").unwrap();
        s.rsplit_once(')').unwrap().1.split_whitespace().nth(9).unwrap().parse().unwrap()
    }

    fn map(path: &Path) -> Mmap {
        let f = File::open(path).unwrap();
        let m = unsafe { Mmap::map(&f).unwrap() };
        let _ = m.advise(Advice::Random); // like Index::open: no readahead
        m
    }

    #[inline]
    fn entry(idx: &[u8], i: usize) -> (u64, u64) {
        let p = HEADER_LEN + i * ENTRY_LEN;
        (u64::from_le_bytes(idx[p..p + 8].try_into().unwrap()), u64::from_le_bytes(idx[p + 8..p + 16].try_into().unwrap()))
    }

    #[inline]
    fn fence(f: &[u8], j: usize) -> u64 {
        u64::from_le_bytes(f[j * 8..j * 8 + 8].try_into().unwrap())
    }

    fn lower_bound_range(idx: &[u8], mut lo: usize, mut hi: usize, h: u64) -> usize {
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if entry(idx, mid).0 < h {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn search(mode: Mode, idx: &[u8], fences: &[u8], n: usize, h: u64) -> usize {
        match mode {
            Mode::Flat => lower_bound_range(idx, 0, n, h),
            Mode::Fenced => {
                // largest leaf j whose first hash is < h (leaf 0 if none); the answer is in leaf j or at its end
                let leaves = fences.len() / 8;
                let (mut lo, mut hi) = (0usize, leaves);
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    if fence(fences, mid) < h {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                let j = lo.saturating_sub(1);
                let start = if j == 0 { 0 } else { LEAF * j - FIRST_LEAF_SHORT };
                let end = (LEAF * (j + 1) - FIRST_LEAF_SHORT).min(n);
                lower_bound_range(idx, start, end, h)
            }
        }
    }

    fn fetch(idx: &[u8], csv: &[u8], mut i: usize, n: usize, h: u64, key: &[u8], key_col: usize) -> usize {
        let mut rows = 0;
        while i < n {
            let (eh, off) = entry(idx, i);
            if eh != h {
                break;
            }
            i += 1;
            let mut matched = false;
            scan_record(csv, off as usize, |c, f| {
                if c == key_col && &*f.value() == key {
                    matched = true;
                }
            });
            rows += matched as usize;
        }
        rows
    }

    fn build_fences(idx_path: &Path, fence_path: &Path) {
        let idx = map(idx_path);
        let n = Header::decode(&idx).unwrap().entry_count as usize;
        let leaves = (n + FIRST_LEAF_SHORT).div_ceil(LEAF);
        let mut out = Vec::with_capacity(leaves * 8);
        for j in 0..leaves {
            let start = if j == 0 { 0 } else { LEAF * j - FIRST_LEAF_SHORT };
            out.extend_from_slice(&entry(&idx, start).0.to_le_bytes());
        }
        std::fs::write(fence_path, out).unwrap();
    }

    #[derive(Default, Clone)]
    struct Sample {
        search_us: f64,
        fetch_us: f64,
        search_faults: u64,
        fetch_faults: u64,
        rows: usize,
    }

    struct Files {
        csv: PathBuf,
        idx: PathBuf,
        fence: PathBuf,
        key_col: usize,
        n: usize,
    }

    fn lookup(f: &Files, mode: Mode, key: &str, evict_csv: bool, evict_index: bool) -> Sample {
        if evict_index {
            evict(&f.idx);
            evict(&f.fence);
        }
        if evict_csv {
            evict(&f.csv);
        }
        let idx = map(&f.idx);
        let fences = map(&f.fence);
        let csv = map(&f.csv);
        let h = xxhash_rust::xxh3::xxh3_64(key.as_bytes());

        let f0 = majflt();
        let t0 = Instant::now();
        let at = search(mode, &idx, &fences, f.n, h);
        let t1 = Instant::now();
        let f1 = majflt();
        let t1b = Instant::now(); // /proc read above is excluded from both phases
        let rows = fetch(&idx, &csv, at, f.n, h, key.as_bytes(), f.key_col);
        let t2 = Instant::now();
        let f2 = majflt();
        Sample {
            search_us: (t1 - t0).as_secs_f64() * 1e6,
            fetch_us: (t2 - t1b).as_secs_f64() * 1e6,
            search_faults: f1 - f0,
            fetch_faults: f2 - f1,
            rows,
        }
    }

    fn med(v: &mut Vec<f64>) -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    }

    pub fn main() {
        let a = Args::parse();
        let idx_path = {
            let mut s = a.csv.as_os_str().to_owned();
            s.push(".idx");
            PathBuf::from(s)
        };
        let fence_path = {
            let mut s = idx_path.as_os_str().to_owned();
            s.push(".fence-proto");
            PathBuf::from(s)
        };
        let hdr = Header::decode(&map(&idx_path)).expect("index (run `csvidx build`; no tail supported here)");
        assert!(!idx_path.with_extension("idx.tail").exists(), "this prototype ignores a tail; run `csvidx compact`");
        if !fence_path.exists() {
            build_fences(&idx_path, &fence_path);
        }
        let f = Files { csv: a.csv.clone(), idx: idx_path.clone(), fence: fence_path.clone(), key_col: hdr.key_col as usize, n: hdr.entry_count as usize };
        let idx_mib = std::fs::metadata(&idx_path).unwrap().len() as f64 / 1048576.0;
        let fence_mib = std::fs::metadata(&fence_path).unwrap().len() as f64 / 1048576.0;
        println!("index {:.0} MiB ({} entries), fences {:.1} MiB ({} leaves of one 4 KiB page)\n", idx_mib, f.n, fence_mib, (fence_mib * 1048576.0 / 8.0) as usize);

        // keys with 1..=100 matching rows; flat and fenced must agree on every row count
        let text = std::fs::read_to_string(&a.keys_file).unwrap();
        let mut keys: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        read_through(&f.idx);
        read_through(&f.fence);
        read_through(&f.csv);
        for k in text.lines().filter(|l| !l.is_empty()) {
            if !seen.insert(k.to_string()) {
                continue;
            }
            let (s1, s2) = (lookup(&f, Mode::Flat, k, false, false), lookup(&f, Mode::Fenced, k, false, false));
            assert_eq!(s1.rows, s2.rows, "flat and fenced disagree for key {k}");
            if (1..=100).contains(&s1.rows) {
                keys.push(k.to_string());
            }
            if keys.len() >= a.n + a.warm_lookups {
                break;
            }
        }
        assert!(keys.len() >= a.n, "not enough keys with 1..=100 rows");
        let (timed, warm) = keys.split_at(a.n);
        let warm: Vec<String> = if warm.is_empty() { timed.to_vec() } else { warm.to_vec() };
        println!("{} timed lookups per cell, {} warm-up lookups for `service`; flat and fenced returned identical row counts\n", timed.len(), warm.len());

        println!("{:<9} {:<7} {:>11} {:>11} {:>11} {:>10} {:>10}   {:>9}", "cache", "search", "search µs", "fetch µs", "total µs", "faults/idx", "faults/csv", "idx share");
        let mut summary: Vec<(State, Mode, f64, f64)> = Vec::new();
        for state in [State::Cold, State::CsvCold, State::Service, State::Hot] {
            for mode in [Mode::Flat, Mode::Fenced] {
                match state {
                    State::CsvCold | State::Hot => {
                        read_through(&f.idx);
                        read_through(&f.fence);
                        read_through(&f.csv);
                    }
                    State::Service => {
                        evict(&f.idx);
                        evict(&f.fence);
                        for k in &warm {
                            lookup(&f, mode, k, false, false);
                        }
                    }
                    State::Cold => {}
                }
                let samples: Vec<Sample> = timed
                    .iter()
                    .map(|k| match state {
                        State::Cold => lookup(&f, mode, k, true, true),
                        State::CsvCold | State::Service => lookup(&f, mode, k, true, false),
                        State::Hot => lookup(&f, mode, k, false, false),
                    })
                    .collect();
                let mut s: Vec<f64> = samples.iter().map(|x| x.search_us).collect();
                let mut t: Vec<f64> = samples.iter().map(|x| x.fetch_us).collect();
                let mut tot: Vec<f64> = samples.iter().map(|x| x.search_us + x.fetch_us).collect();
                let (ms, mt, mtot) = (med(&mut s), med(&mut t), med(&mut tot));
                let fi = samples.iter().map(|x| x.search_faults as f64).sum::<f64>() / samples.len() as f64;
                let fc = samples.iter().map(|x| x.fetch_faults as f64).sum::<f64>() / samples.len() as f64;
                println!(
                    "{:<9} {:<7} {:>11.1} {:>11.1} {:>11.1} {:>10.1} {:>10.1}   {:>8.0}%",
                    format!("{:?}", state).to_lowercase(),
                    if mode == Mode::Flat { "flat" } else { "fenced" },
                    ms, mt, mtot, fi, fc, 100.0 * ms / (ms + mt)
                );
                summary.push((state, mode, mtot, fi));
            }
        }
        println!("\n(times are medians; faults are means of major page faults = reads from disk)");
        let g = |st: State, m: Mode| summary.iter().find(|x| x.0 == st && x.1 == m).unwrap();
        for st in [State::Cold, State::Service] {
            let (fl, fe) = (g(st, Mode::Flat), g(st, Mode::Fenced));
            println!(
                "{:?}: fences save {:.1} index page reads per lookup and {:.0} µs of {:.0} µs ({:.0}%)",
                st, fl.3 - fe.3, fl.2 - fe.2, fl.2, 100.0 * (fl.2 - fe.2) / fl.2
            );
        }
    }
}
