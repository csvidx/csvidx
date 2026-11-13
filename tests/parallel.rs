//! The parallel scan must produce exactly what the sequential scan produces, for ANY input:
//! well-formed CSV and arbitrary garbage with unbalanced quotes, stray `\r`, blank lines, etc.
//! "Exactly" means byte-identical index files and identical statistics.

use csvidx::index::*;
use std::fs;
use std::path::{Path, PathBuf};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const ALPHABET: &[u8] = b"ab1 ,,\"\"\"\n\n\r";

fn garbage(rng: &mut Rng, len: usize) -> Vec<u8> {
    (0..len).map(|_| ALPHABET[rng.below(ALPHABET.len() as u64) as usize]).collect()
}

fn opts(key: &str, header: bool, threads: usize, chunk: Option<usize>) -> BuildOptions {
    let mut o = BuildOptions::new(key, header);
    o.par = ScanPar { threads, chunk_bytes: chunk };
    o
}

fn idx_bytes(csv: &Path) -> Vec<u8> {
    fs::read(index_path(csv)).unwrap()
}

#[test]
fn parallel_equals_sequential_on_arbitrary_bytes() {
    let d = tempfile::tempdir().unwrap();
    let csv = d.path().join("g.csv");
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let (mut repairs, mut checked) = (0u64, 0u64);

    for _ in 0..300 {
        let len = 1 + rng.below(400) as usize;
        fs::write(&csv, garbage(&mut rng, len)).unwrap();

        let seq = build(&csv, &opts("0", false, 1, None)).unwrap();
        let want = idx_bytes(&csv);
        assert_eq!(seq.par_chunks, 0);

        for chunk in [1usize, 2, 3, 5, 8, 13, 40] {
            let par = build(&csv, &opts("0", false, 3, Some(chunk))).unwrap();
            let got = idx_bytes(&csv);
            assert_eq!(got, want, "index differs: len {len}, chunk {chunk}, input {:?}", fs::read(&csv).unwrap());
            assert_eq!(
                (par.rows, par.skipped, par.blank, par.unterminated_tail),
                (seq.rows, seq.skipped, seq.blank, seq.unterminated_tail),
                "stats differ: chunk {chunk}"
            );
            repairs += par.par_fallbacks;
            checked += 1;
        }
    }
    assert_eq!(checked, 2100);
    // the guess is often wrong on this input; if it never was, the repair path would be untested
    assert!(repairs > 500, "repair path barely exercised: {repairs}");
}

fn tricky_csv(rows: u32, crlf: bool) -> Vec<u8> {
    let nl = if crlf { "\r\n" } else { "\n" };
    let mut s = format!("id,k,payload{nl}");
    for i in 0..rows {
        let k = (i * 7919) % 211;
        match i % 7 {
            0 => s.push_str(&format!("{i},\"{k}\",\"multi\nline\nvalue {i}\"{nl}")),
            1 => s.push_str(&format!("{i},{k},\"say \"\"hi\"\", ok\"{nl}")),
            2 => s.push_str(&format!("{nl}")),           // blank line
            3 => s.push_str(&format!("{i}{nl}")),        // too short for key column 1
            _ => s.push_str(&format!("{i},{k},plain{nl}")),
        }
    }
    s.into_bytes()
}

#[test]
fn parallel_equals_sequential_on_structured_csv_with_multiline_fields() {
    let d = tempfile::tempdir().unwrap();
    let csv: PathBuf = d.path().join("t.csv");
    let mut repairs_total = 0;
    for crlf in [false, true] {
        for end in [&b""[..], &b"\n"[..], &b"9999,7,\"unterminated"[..], &b"9999,7,tail"[..]] {
            let mut data = tricky_csv(6000, crlf);
            data.extend_from_slice(end);
            fs::write(&csv, &data).unwrap();
            let seq = build(&csv, &opts("k", true, 1, None)).unwrap();
            let want = idx_bytes(&csv);
            for chunk in [7usize, 64, 1000, 4096, 100_000, 10_000_000] {
                for threads in [2usize, 5] {
                    let par = build(&csv, &opts("k", true, threads, Some(chunk))).unwrap();
                    assert_eq!(idx_bytes(&csv), want, "crlf {crlf}, end {end:?}, chunk {chunk}, threads {threads}");
                    assert_eq!((par.rows, par.skipped, par.blank), (seq.rows, seq.skipped, seq.blank));
                    repairs_total += par.par_fallbacks;
                    assert!(verify_opts(&csv, default_hash, DEFAULT_MAX_MEM_ENTRIES, &ScanPar { threads, chunk_bytes: Some(chunk) }).unwrap().ok());
                }
            }
        }
    }
    assert!(repairs_total > 0, "multi-line fields should break some guesses");
}

#[test]
fn parallel_works_together_with_external_sort_update_and_tail() {
    let d = tempfile::tempdir().unwrap();
    let csv = d.path().join("u.csv");
    let mut data = tricky_csv(4000, false);
    fs::write(&csv, &data).unwrap();

    let mut o = opts("k", true, 4, Some(300));
    o.max_mem_entries = 50; // forces spilled runs in the middle of parallel rounds
    let s = build(&csv, &o).unwrap();
    assert!(s.runs > 1 && s.par_chunks > 10, "runs {}, chunks {}", s.runs, s.par_chunks);
    let parallel_idx = idx_bytes(&csv);
    build(&csv, &opts("k", true, 1, None)).unwrap();
    assert_eq!(idx_bytes(&csv), parallel_idx);

    // append, then update (parallel, tiny chunks, tiny sort memory) into the tail
    let more = tricky_csv(1500, false);
    let body = &more[more.iter().position(|&b| b == b'\n').unwrap() + 1..]; // without its header line
    data.extend_from_slice(body);
    fs::write(&csv, &data).unwrap();
    let mut u = UpdateOptions::new(default_hash);
    u.compact = CompactPolicy::Never;
    u.max_mem_entries = 40;
    u.par = ScanPar { threads: 3, chunk_bytes: Some(200) };
    let st = update_opts(&csv, &u).unwrap();
    assert!(st.added > 1000 && st.tail_entries > 0, "{st:?}");
    assert!(verify_opts(&csv, default_hash, 64, &ScanPar { threads: 3, chunk_bytes: Some(150) }).unwrap().ok());
    compact(&csv, default_hash).unwrap();
    let compacted = idx_bytes(&csv);
    build(&csv, &opts("k", true, 1, None)).unwrap();
    assert_eq!(idx_bytes(&csv), compacted, "parallel update + compact != sequential rebuild");
}

#[test]
fn leftover_files_are_cleaned_up_after_parallel_builds() {
    let d = tempfile::tempdir().unwrap();
    let csv = d.path().join("c.csv");
    fs::write(&csv, tricky_csv(3000, false)).unwrap();
    let mut o = opts("k", true, 4, Some(100));
    o.max_mem_entries = 7;
    build(&csv, &o).unwrap();
    let left: Vec<_> = fs::read_dir(d.path()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".tmp")).collect();
    assert!(left.is_empty(), "{left:?}");
}

#[test]
fn guesses_are_right_on_plain_csv_so_no_chunk_needs_repair() {
    // Correctness never depends on the guess (it is validated), but speed does: on a CSV without
    // multi-line fields every guessed chunk start must be a true record start.
    let d = tempfile::tempdir().unwrap();
    let csv = d.path().join("p.csv");
    let mut s = String::from("id,k,v\n");
    for i in 0..20_000u32 {
        s.push_str(&format!("{i},{},p{}\r\n", (i * 31) % 997, i % 13));
    }
    fs::write(&csv, s).unwrap();
    let st = build(&csv, &opts("k", true, 4, Some(1000))).unwrap();
    assert!(st.par_chunks > 100, "chunks {}", st.par_chunks);
    assert_eq!(st.par_fallbacks, 0, "guesses should all be right on plain CSV");
    assert_eq!(st.rows, 20_000);
}
