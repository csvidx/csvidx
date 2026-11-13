use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use csvidx::index::*;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "csvidx", version, about = "Secondary index for huge CSV files: look up rows by key without importing")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build <csv>.idx for a key column (name from the header row, or zero-based number)
    Build {
        csv: PathBuf,
        #[arg(short, long)]
        key: String,
        /// The file has no header row (then --key must be a column number)
        #[arg(long)]
        no_header: bool,
        /// RAM budget for sorting, in MiB; bigger inputs spill sorted runs to disk
        #[arg(long, default_value_t = 1024)]
        mem_limit_mib: usize,
        /// Worker threads for scanning and sorting: 0 = all cores, 1 = sequential
        #[arg(long, default_value_t = 0)]
        threads: usize,
    },
    /// Index only the bytes appended since the last build/update
    Update {
        csv: PathBuf,
        /// Keep the new rows in <csv>.idx.tail and never merge them into the main index
        #[arg(long)]
        no_compact: bool,
        /// RAM budget for sorting, in MiB; bigger inputs spill sorted runs to disk
        #[arg(long, default_value_t = 1024)]
        mem_limit_mib: usize,
        /// Worker threads for scanning and sorting: 0 = all cores, 1 = sequential
        #[arg(long, default_value_t = 0)]
        threads: usize,
    },
    /// Print the rows whose key equals one of KEYS
    Get {
        csv: PathBuf,
        keys: Vec<String>,
        /// Print "key<TAB>number_of_rows" instead of the rows
        #[arg(long)]
        count: bool,
        /// Print timings to stderr
        #[arg(long)]
        time: bool,
    },
    /// Merge <csv>.idx.tail into the main index (sequential; no CSV parsing)
    Compact { csv: PathBuf },
    /// Show the index header
    Info { csv: PathBuf },
    /// Check that the index equals a fresh full rescan of the CSV
    Verify {
        csv: PathBuf,
        /// RAM budget for sorting, in MiB; bigger inputs spill sorted runs to disk
        #[arg(long, default_value_t = 1024)]
        mem_limit_mib: usize,
        /// Worker threads for scanning and sorting: 0 = all cores, 1 = sequential
        #[arg(long, default_value_t = 0)]
        threads: usize,
    },
    /// In-process latency benchmark: look up every key from a file (one per line)
    Bench {
        csv: PathBuf,
        keys_file: PathBuf,
        #[arg(long, default_value_t = 1)]
        repeat: usize,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(2);
    }
}

fn gib(b: u64) -> f64 {
    b as f64 / (1u64 << 30) as f64
}

fn run() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Build { csv, key, no_header, mem_limit_mib, threads } => {
            let t = Instant::now();
            let mut opts = BuildOptions::new(&key, !no_header);
            opts.max_mem_entries = mem_limit_entries(mem_limit_mib);
            opts.par.threads = threads;
            let s = build(&csv, &opts)?;
            let total = t.elapsed().as_secs_f64();
            let idx_bytes = std::fs::metadata(index_path(&csv))?.len();
            println!(
                "indexed {} rows ({:.2} GiB) on column #{} in {:.2}s  [scan {:.2}s, sort {:.2}s, write {:.2}s]  scan speed {:.0} MB/s",
                s.rows,
                gib(s.csv_bytes),
                s.key_col,
                total,
                s.scan_secs,
                s.sort_secs,
                s.write_secs,
                s.csv_bytes as f64 / 1e6 / s.scan_secs.max(1e-9)
            );
            println!("index: {} ({:.2} GiB)", index_path(&csv).display(), gib(idx_bytes));
            println!(
                "scan: {} thread(s){}",
                s.threads.max(1),
                if s.par_chunks > 0 { format!(", {} chunks, {} needed a sequential repair", s.par_chunks, s.par_fallbacks) } else { String::new() }
            );
            if s.runs > 0 {
                println!("external sort: {} sorted runs spilled to disk (limit {} MiB)", s.runs, mem_limit_mib);
            }
            if s.skipped > 0 {
                eprintln!("warning: {} rows have fewer columns than the key column and were not indexed", s.skipped);
            }
            if s.unterminated_tail {
                eprintln!("note: the last row has no trailing newline; it is indexed provisionally and re-checked by `update`");
            }
        }
        Cmd::Update { csv, no_compact, mem_limit_mib, threads } => {
            let mut o = UpdateOptions::new(default_hash);
            o.max_mem_entries = mem_limit_entries(mem_limit_mib);
            o.par.threads = threads;
            if no_compact {
                o.compact = CompactPolicy::Never;
            }
            let u = update_opts(&csv, &o)?;
            if u.up_to_date {
                println!("index is up to date ({} entries on disk, {} of them in the tail)", u.entries_on_disk, u.tail_entries);
            } else if u.compacted {
                println!(
                    "added {} rows (replaced {} provisional) in {:.2}s; tail merged into the main index ({} entries)",
                    u.added, u.dropped_provisional, u.secs, u.entries_on_disk
                );
            } else {
                println!(
                    "added {} rows (replaced {} provisional) in {:.2}s; main index untouched, tail holds {} entries",
                    u.added, u.dropped_provisional, u.secs, u.tail_entries
                );
            }
        }
        Cmd::Compact { csv } => {
            let c = compact(&csv, default_hash)?;
            if c.merged {
                println!("merged the tail into the main index: {} entries, {:.2}s", c.entries, c.secs);
            } else {
                println!("no tail to merge ({} entries)", c.entries);
            }
        }
        Cmd::Get { csv, keys, count, time } => {
            let t0 = Instant::now();
            let idx = Index::open(&csv)?;
            let t_open = t0.elapsed();
            if let Freshness::Behind(n) = idx.freshness() {
                eprintln!("warning: the CSV grew by {n} bytes since it was indexed; run `csvidx update` to include new rows");
            }
            let stdout = std::io::stdout();
            let mut out = BufWriter::new(stdout.lock());
            let t1 = Instant::now();
            let mut total = 0u64;
            let mut write_failed = false;
            for k in &keys {
                let mut c = 0u64;
                idx.lookup_each(k.as_bytes(), |_, rec| {
                    c += 1;
                    if !count && !write_failed {
                        write_failed = out.write_all(rec).is_err() || out.write_all(b"\n").is_err();
                    }
                });
                if count {
                    let _ = writeln!(out, "{k}\t{c}");
                }
                total += c;
            }
            let _ = out.flush();
            if time {
                eprintln!("{} key(s), {} row(s): open {:?}, lookup {:?}", keys.len(), total, t_open, t1.elapsed());
            }
        }
        Cmd::Info { csv } => {
            let idx = Index::open(&csv)?;
            let h = idx.header();
            println!("index file : {}", index_path(&csv).display());
            println!("entries    : {} (main)", h.entry_count);
            println!("key column : #{}  (CSV header row: {})", h.key_col, h.has_header());
            println!("csv size   : {} bytes at indexing time", h.csv_size);
            println!("resume at  : {}", h.indexed_up_to);
            match idx.tail_header() {
                Some(t) => println!(
                    "tail       : {} entries, covers CSV bytes {}..{} (main index untouched since; `csvidx compact` merges)",
                    t.entry_count, t.base_offset, t.indexed_up_to
                ),
                None => println!("tail       : none"),
            }
            println!("freshness  : {:?}", idx.freshness());
        }
        Cmd::Verify { csv, mem_limit_mib, threads } => {
            let t = Instant::now();
            let r = verify_opts(&csv, default_hash, mem_limit_entries(mem_limit_mib), &ScanPar { threads, chunk_bytes: None })?;
            println!(
                "entries in index: {}, rows found by rescan: {}, sorted: {}, mismatches: {}  ({:.2}s)",
                r.entries,
                r.rescanned,
                r.sorted,
                r.mismatches,
                t.elapsed().as_secs_f64()
            );
            if let Some(m) = &r.first_mismatch {
                println!("first mismatch: {m}");
            }
            if !r.ok() {
                println!("VERIFY FAILED");
                std::process::exit(1);
            }
            println!("VERIFY OK");
        }
        Cmd::Bench { csv, keys_file, repeat } => {
            let text = std::fs::read_to_string(&keys_file).with_context(|| format!("read {}", keys_file.display()))?;
            let keys: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
            let idx = Index::open(&csv)?;
            let mut lat: Vec<u64> = Vec::with_capacity(keys.len() * repeat);
            let (mut rows, mut bytes) = (0u64, 0u64);
            for _ in 0..repeat {
                for k in &keys {
                    let t = Instant::now();
                    let mut c = 0u64;
                    idx.lookup_each(k.as_bytes(), |_, r| {
                        c += 1;
                        bytes += r.len() as u64;
                    });
                    lat.push(t.elapsed().as_nanos() as u64);
                    rows += c;
                }
            }
            lat.sort_unstable();
            let pct = |p: f64| lat[((lat.len() as f64 - 1.0) * p) as usize] as f64 / 1000.0;
            let avg = lat.iter().sum::<u64>() as f64 / lat.len() as f64 / 1000.0;
            println!(
                "lookups: {}  rows returned: {}  bytes read: {}\nlatency us: avg {:.1}  p50 {:.1}  p90 {:.1}  p99 {:.1}  max {:.1}",
                lat.len(),
                rows,
                bytes,
                avg,
                pct(0.50),
                pct(0.90),
                pct(0.99),
                *lat.last().unwrap() as f64 / 1000.0
            );
        }
    }
    Ok(())
}
