//! The fence file (`.idx.fence`) is a pure accelerator: it must never change a result.
use csvidx::index::*;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_64;

/// Few distinct hashes => runs of equal hashes that span several 4 KiB index pages.
fn coarse(k: &[u8]) -> u64 {
    (k.first().copied().unwrap_or(0) as u64 % 7) << 56
}

fn line(i: u32) -> String {
    format!("{},{},p", i, (i * 7919) % 53)
}

fn make(dir: &tempfile::TempDir, rows: u32) -> (PathBuf, Vec<String>) {
    let lines: Vec<String> = (0..rows).map(line).collect();
    let mut s = String::from("id,k,v\n");
    for l in &lines {
        s.push_str(l);
        s.push('\n');
    }
    let p = dir.path().join("t.csv");
    fs::write(&p, s).unwrap();
    (p, lines)
}

fn want(lines: &[String], k: u32) -> Vec<String> {
    lines.iter().filter(|l| l.split(',').nth(1).unwrap() == k.to_string()).cloned().collect()
}

fn check_all(csv: &Path, lines: &[String], hash: HashFn) {
    let idx = Index::open_with_hash(csv, hash).unwrap();
    let mut nonempty = 0;
    for k in 0..55u32 {
        let got: Vec<String> = idx.lookup(k.to_string().as_bytes()).into_iter().map(|(_, r)| String::from_utf8(r).unwrap()).collect();
        assert_eq!(got, want(lines, k), "key {k}");
        nonempty += !got.is_empty() as u32;
    }
    assert_eq!(nonempty, 53);
}

fn build_with(csv: &Path, hash: HashFn) {
    let mut o = BuildOptions::new("k", true);
    o.hash = hash;
    build(csv, &o).unwrap();
}

/// Rewrites the fence file with arbitrary values but valid checksums.
fn forge_fence(csv: &Path, f: impl Fn(u64) -> u64) {
    let fp = fence_path(csv);
    let mut b = fs::read(&fp).unwrap();
    for c in b[32..].chunks_exact_mut(8) {
        let v = f(u64::from_le_bytes((&*c).try_into().unwrap()));
        c.copy_from_slice(&v.to_le_bytes());
    }
    let sum = xxh3_64(&b[32..]);
    b[24..32].copy_from_slice(&sum.to_le_bytes());
    fs::write(&fp, b).unwrap();
}

#[test]
fn results_are_identical_with_and_without_fences_for_both_hash_shapes() {
    for hash in [default_hash as HashFn, coarse as HashFn] {
        let d = tempfile::tempdir().unwrap();
        let (csv, lines) = make(&d, 4000); // 4000 entries = 16 pages
        build_with(&csv, hash);
        let fp = fence_path(&csv);
        assert!(fp.exists());
        assert_eq!(fs::metadata(&fp).unwrap().len(), 32 + 8 * 16);
        check_all(&csv, &lines, hash);
        fs::remove_file(&fp).unwrap();
        check_all(&csv, &lines, hash); // plain binary search
    }
}

#[test]
fn the_fences_are_really_used_not_just_present() {
    // mutation check: a fence file that lies (valid checksums) must change the outcome
    let d = tempfile::tempdir().unwrap();
    let (csv, lines) = make(&d, 4000);
    build_with(&csv, default_hash);
    forge_fence(&csv, |_| u64::MAX);
    let idx = Index::open(&csv).unwrap();
    let misses = (0..53u32).filter(|k| idx.lookup(k.to_string().as_bytes()).len() != want(&lines, *k).len()).count();
    assert!(misses > 20, "forged fences changed only {misses} lookups: they are not being used");
}

#[test]
fn damaged_or_stale_fence_files_are_ignored() {
    let d = tempfile::tempdir().unwrap();
    let (csv, lines) = make(&d, 4000);
    build_with(&csv, default_hash);
    let fp = fence_path(&csv);
    let good = fs::read(&fp).unwrap();

    // header damage: the fences are ignored and lookups stay exact
    for pos in [0usize, 9, 17] {
        let mut b = good.clone();
        b[pos] ^= 0x55;
        fs::write(&fp, b).unwrap();
        check_all(&csv, &lines, default_hash);
        assert!(verify(&csv, default_hash).unwrap().fence_problem.is_some());
    }
    // data damage is not read at open (that is the point of mmap), but `verify` must catch it
    for pos in [25usize, 40, good.len() - 1] {
        let mut b = good.clone();
        b[pos] ^= 0x55;
        fs::write(&fp, b).unwrap();
        let r = verify(&csv, default_hash).unwrap();
        assert!(!r.ok() && r.fence_problem.is_some(), "pos {pos}");
    }
    fs::write(&fp, &good).unwrap();
    assert!(verify(&csv, default_hash).unwrap().ok());
    fs::write(&fp, &good[..good.len() - 8]).unwrap(); // truncated
    check_all(&csv, &lines, default_hash);
    fs::write(&fp, b"").unwrap();
    check_all(&csv, &lines, default_hash);

    // a fence left over from an older, different index (same size!) is rejected via the header hash
    fs::write(&fp, &good).unwrap();
    let (csv2, lines2) = {
        let mut s = String::from("id,k,v\n");
        let ls: Vec<String> = (0..4000u32).map(|i| format!("{},{},p", i, (i * 104_729 + 11) % 53)).collect();
        for l in &ls {
            s.push_str(l);
            s.push('\n');
        }
        fs::write(&csv, s).unwrap();
        (csv.clone(), ls)
    };
    build_with(&csv2, default_hash);
    fs::write(&fp, &good).unwrap(); // put the OLD fence back
    let idx = Index::open(&csv2).unwrap();
    for k in 0..53u32 {
        let got = idx.lookup(k.to_string().as_bytes()).len();
        let exp = lines2.iter().filter(|l| l.split(',').nth(1).unwrap() == k.to_string()).count();
        assert_eq!(got, exp, "key {k}");
    }
}

#[test]
fn tail_and_compact_keep_fences_correct() {
    let d = tempfile::tempdir().unwrap();
    let (csv, mut lines) = make(&d, 3000);
    build_with(&csv, default_hash);
    let fence_before = fs::read(fence_path(&csv)).unwrap();

    let mut f = fs::OpenOptions::new().append(true).open(&csv).unwrap();
    for i in 3000..3400u32 {
        writeln!(f, "{}", line(i)).unwrap();
        lines.push(line(i));
    }
    drop(f);
    let mut o = UpdateOptions::new(default_hash);
    o.compact = CompactPolicy::Never;
    update_opts(&csv, &o).unwrap();
    assert_eq!(fs::read(fence_path(&csv)).unwrap(), fence_before, "update must not touch the main fences");
    check_all(&csv, &lines, default_hash);

    compact(&csv, default_hash).unwrap();
    let after = fs::read(fence_path(&csv)).unwrap();
    assert_ne!(after, fence_before);
    check_all(&csv, &lines, default_hash);

    // fences written by compact equal the fences of a from-scratch build of the same CSV
    build_with(&csv, default_hash);
    assert_eq!(fs::read(fence_path(&csv)).unwrap(), after);
    assert!(verify(&csv, default_hash).unwrap().ok());
}

#[test]
fn page_boundaries_are_exact() {
    // every entry count around the 256-entry page edges, with every key looked up
    for rows in [0u32, 1, 3, 4, 5, 251, 252, 253, 255, 256, 257, 507, 508, 509, 510, 1020] {
        let d = tempfile::tempdir().unwrap();
        let lines: Vec<String> = (0..rows).map(|i| format!("{},k{},p", i, i)).collect();
        let mut s = String::from("id,k,v\n");
        for l in &lines {
            s.push_str(l);
            s.push('\n');
        }
        let csv = d.path().join("b.csv");
        fs::write(&csv, s).unwrap();
        build_with(&csv, default_hash);
        let idx = Index::open(&csv).unwrap();
        for i in 0..rows {
            assert_eq!(idx.lookup(format!("k{i}").as_bytes()).len(), 1, "rows={rows} key k{i}");
        }
        assert!(idx.lookup(b"absent").is_empty());
    }
}
