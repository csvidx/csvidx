use csvidx::index::*;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn tmp_csv(dir: &tempfile::TempDir, content: &[u8]) -> PathBuf {
    let p = dir.path().join("t.csv");
    fs::write(&p, content).unwrap();
    p
}

fn get(csv: &Path, key: &str) -> Vec<String> {
    let idx = Index::open(csv).unwrap();
    idx.lookup(key.as_bytes()).into_iter().map(|(_, r)| String::from_utf8(r).unwrap()).collect()
}

fn append(csv: &Path, bytes: &[u8]) {
    let mut f = fs::OpenOptions::new().append(true).open(csv).unwrap();
    f.write_all(bytes).unwrap();
}

#[test]
fn basic_lookup_by_name_and_missing_key() {
    let d = tempfile::tempdir().unwrap();
    let csv = tmp_csv(&d, b"id,user,v\n1,alice,10\n2,bob,20\n3,alice,30\n");
    let s = build(&csv, &BuildOptions::new("user", true)).unwrap();
    assert_eq!((s.rows, s.key_col), (3, 1));
    assert_eq!(get(&csv, "alice"), ["1,alice,10", "3,alice,30"]); // file order
    assert_eq!(get(&csv, "bob"), ["2,bob,20"]);
    assert!(get(&csv, "carol").is_empty());
    assert!(verify(&csv, default_hash).unwrap().ok());
}

#[test]
fn quoted_keys_escapes_and_multiline_rows() {
    let d = tempfile::tempdir().unwrap();
    let csv = tmp_csv(
        &d,
        b"k,v\n\"a,b\",1\n\"x\"\"y\",2\n\"multi\nline\",3\nplain,\"has\nnewline\"\n\"plain\",5\n\"\",6\n",
    );
    build(&csv, &BuildOptions::new("k", true)).unwrap();
    assert_eq!(get(&csv, "a,b"), ["\"a,b\",1"]);
    assert_eq!(get(&csv, "x\"y"), ["\"x\"\"y\",2"]);
    assert_eq!(get(&csv, "multi\nline"), ["\"multi\nline\",3"]);
    // quoted and unquoted spellings of the same key are the same key
    assert_eq!(get(&csv, "plain"), ["plain,\"has\nnewline\"", "\"plain\",5"]);
    assert_eq!(get(&csv, ""), ["\"\",6"]);
    assert!(verify(&csv, default_hash).unwrap().ok());
}

#[test]
fn crlf_and_bom() {
    let d = tempfile::tempdir().unwrap();
    let csv = tmp_csv(&d, b"\xEF\xBB\xBFid,name\r\n1,ann\r\n2,\"bo\r\nb\"\r\n3,ann\r\n");
    build(&csv, &BuildOptions::new("name", true)).unwrap(); // BOM must not break the column name
    assert_eq!(get(&csv, "ann"), ["1,ann", "3,ann"]);
    assert_eq!(get(&csv, "bo\r\nb"), ["2,\"bo\r\nb\""]);
}

#[test]
fn blank_lines_and_short_rows_are_skipped() {
    let d = tempfile::tempdir().unwrap();
    let csv = tmp_csv(&d, b"a,b\n\n1,x\nshort\n\n2,y\n");
    let s = build(&csv, &BuildOptions::new("b", true)).unwrap();
    assert_eq!((s.rows, s.skipped, s.blank), (2, 1, 2));
    assert_eq!(get(&csv, "x"), ["1,x"]);
}

#[test]
fn no_header_numeric_key() {
    let d = tempfile::tempdir().unwrap();
    let csv = tmp_csv(&d, b"1,a\n2,b\n1,c\n");
    build(&csv, &BuildOptions::new("0", false)).unwrap();
    assert_eq!(get(&csv, "1"), ["1,a", "1,c"]);
    assert!(build(&csv, &BuildOptions::new("id", false)).is_err());
}

#[test]
fn unknown_column_is_an_error() {
    let d = tempfile::tempdir().unwrap();
    let csv = tmp_csv(&d, b"a,b\n1,2\n");
    let e = build(&csv, &BuildOptions::new("nope", true)).unwrap_err().to_string();
    assert!(e.contains("not found") && e.contains("a, b"), "{e}");
}

#[test]
fn hash_collisions_are_filtered_by_real_key() {
    let d = tempfile::tempdir().unwrap();
    let csv = tmp_csv(&d, b"k,v\na,1\nb,2\na,3\nc,4\nb,5\n");
    fn all_collide(_: &[u8]) -> u64 {
        42
    }
    let mut o = BuildOptions::new("k", true);
    o.hash = all_collide;
    build(&csv, &o).unwrap();
    let idx = Index::open_with_hash(&csv, all_collide).unwrap();
    let got = |k: &str| -> Vec<String> {
        idx.lookup(k.as_bytes()).into_iter().map(|(_, r)| String::from_utf8(r).unwrap()).collect()
    };
    assert_eq!(got("a"), ["a,1", "a,3"]);
    assert_eq!(got("b"), ["b,2", "b,5"]);
    assert_eq!(got("c"), ["c,4"]);
    assert!(got("zzz").is_empty());
    assert!(verify(&csv, all_collide).unwrap().ok());
}

#[test]
fn append_then_update_is_incremental_and_exact() {
    let d = tempfile::tempdir().unwrap();
    let csv = tmp_csv(&d, b"k,v\na,1\nb,2\n");
    build(&csv, &BuildOptions::new("k", true)).unwrap();

    append(&csv, b"a,3\nc,4\n\"multi\nline\",5\n");
    let idx = Index::open(&csv).unwrap();
    assert!(matches!(idx.freshness(), Freshness::Behind(n) if n > 0));
    drop(idx);
    assert_eq!(get(&csv, "a"), ["a,1"]); // new rows are invisible until `update`

    let u = update(&csv, default_hash).unwrap();
    assert!(!u.up_to_date);
    assert_eq!((u.added, u.entries_on_disk), (3, 5));
    assert_eq!(get(&csv, "a"), ["a,1", "a,3"]);
    assert_eq!(get(&csv, "multi\nline"), ["\"multi\nline\",5"]);
    assert!(verify(&csv, default_hash).unwrap().ok());
    assert!(update(&csv, default_hash).unwrap().up_to_date);
}

#[test]
fn unterminated_last_row_is_indexed_and_fixed_up_on_update() {
    let d = tempfile::tempdir().unwrap();
    let csv = tmp_csv(&d, b"k,v\na,1\nb,2"); // no trailing newline
    let s = build(&csv, &BuildOptions::new("k", true)).unwrap();
    assert!(s.unterminated_tail);
    assert_eq!(get(&csv, "b"), ["b,2"]); // visible right away

    append(&csv, b"0\nc,3\n"); // the unterminated row grows: "b,2" -> "b,20"
    update(&csv, default_hash).unwrap();
    assert_eq!(get(&csv, "b"), ["b,20"]); // exactly one entry, no stale duplicate
    assert_eq!(get(&csv, "c"), ["c,3"]);
    let rep = verify(&csv, default_hash).unwrap();
    assert!(rep.ok(), "{rep:?}");
    assert_eq!(rep.entries, 3);
}

#[test]
fn rewritten_or_truncated_csv_is_detected_as_stale() {
    let d = tempfile::tempdir().unwrap();
    let csv = tmp_csv(&d, b"k,v\na,1\nb,2\nc,3\n");
    build(&csv, &BuildOptions::new("k", true)).unwrap();

    fs::write(&csv, b"k,v\nz,9\nb,2\nc,3\n").unwrap(); // same size, different content
    // content of the first 64 KiB changed -> prefix hash mismatch
    assert!(Index::open(&csv).err().unwrap().to_string().contains("stale"));

    fs::write(&csv, b"k,v\na,1\n").unwrap(); // shrunk
    assert!(Index::open(&csv).err().unwrap().to_string().contains("stale"));
}

#[test]
fn corrupt_index_is_rejected() {
    let d = tempfile::tempdir().unwrap();
    let csv = tmp_csv(&d, b"k,v\na,1\n");
    build(&csv, &BuildOptions::new("k", true)).unwrap();
    let ip = index_path(&csv);
    let mut b = fs::read(&ip).unwrap();
    b[17] ^= 0xFF;
    fs::write(&ip, &b).unwrap();
    assert!(Index::open(&csv).err().unwrap().to_string().contains("checksum"));

    let mut b = fs::read(&ip).unwrap();
    b[17] ^= 0xFF; // restore header
    b.extend_from_slice(&[0u8; 5]); // trailing garbage -> size mismatch
    fs::write(&ip, &b).unwrap();
    assert!(Index::open(&csv).is_err());
}

#[test]
fn many_rows_with_heavy_keys() {
    let d = tempfile::tempdir().unwrap();
    let mut s = String::from("id,k\n");
    for i in 0..50_000u32 {
        s.push_str(&format!("{},{}\n", i, i % 97));
    }
    let csv = tmp_csv(&d, s.as_bytes());
    build(&csv, &BuildOptions::new("k", true)).unwrap();
    for k in [0u32, 1, 50, 96] {
        let expected: Vec<String> = (0..50_000u32).filter(|i| i % 97 == k).map(|i| format!("{},{}", i, k)).collect();
        assert_eq!(get(&csv, &k.to_string()), expected);
    }
    assert!(get(&csv, "97").is_empty());
    assert!(verify(&csv, default_hash).unwrap().ok());
}

// ---------------------------------------------------------------------------
// External sort: a tiny memory limit must not change the result by a single byte
// ---------------------------------------------------------------------------

fn sample_csv(dir: &tempfile::TempDir, rows: u32) -> PathBuf {
    let mut s = String::from("id,k,v\n");
    for i in 0..rows {
        let k = (i * 7919) % 53;
        if i % 11 == 0 {
            s.push_str(&format!("{i},\"{k}\",\"a,b\"\n"));
        } else if i % 13 == 0 {
            s.push_str(&format!("{i},{k},\"x\ny\"\n"));
        } else {
            s.push_str(&format!("{i},{k},p\n"));
        }
    }
    tmp_csv(dir, s.as_bytes())
}

fn leftovers(dir: &tempfile::TempDir) -> Vec<String> {
    fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect()
}

#[test]
fn external_sort_index_is_byte_identical_and_cleans_up() {
    let d = tempfile::tempdir().unwrap();
    let csv = sample_csv(&d, 1500);
    let base = build(&csv, &BuildOptions::new("k", true)).unwrap();
    assert_eq!(base.runs, 0);
    let expected = fs::read(index_path(&csv)).unwrap();

    // 1 and 3: > 128 runs, so the multi-pass merge is exercised; 40 and 700: a few runs
    for cap in [1usize, 3, 40, 700, 1499, 1500, 100_000] {
        let mut o = BuildOptions::new("k", true);
        o.max_mem_entries = cap;
        let s = build(&csv, &o).unwrap();
        assert_eq!(s.rows, 1500);
        assert_eq!(s.runs > 0, cap < 1500, "cap {cap}: runs = {}", s.runs);
        assert_eq!(fs::read(index_path(&csv)).unwrap(), expected, "index differs for cap {cap}");
        assert!(leftovers(&d).is_empty(), "leftover temp files for cap {cap}: {:?}", leftovers(&d));
    }
    assert!(verify_with(&csv, default_hash, 7).unwrap().ok());
    assert!(leftovers(&d).is_empty());
}

#[test]
fn external_sort_update_matches_full_rebuild() {
    let d = tempfile::tempdir().unwrap();
    let csv = sample_csv(&d, 800);
    let mut o = BuildOptions::new("k", true);
    o.max_mem_entries = 5;
    build(&csv, &o).unwrap();

    let mut extra = String::new();
    for i in 800..1300u32 {
        extra.push_str(&format!("{i},{},p\n", (i * 31) % 53));
    }
    extra.push_str("1300,7,unterminated"); // growing last row
    append(&csv, extra.as_bytes());

    update_with(&csv, default_hash, 5).unwrap();
    let updated = fs::read(index_path(&csv)).unwrap();
    assert!(verify_with(&csv, default_hash, 5).unwrap().ok());

    build(&csv, &BuildOptions::new("k", true)).unwrap(); // from scratch, in memory
    assert_eq!(fs::read(index_path(&csv)).unwrap(), updated, "incremental update != full rebuild");
    assert!(leftovers(&d).is_empty());
}

#[test]
fn verify_detects_a_damaged_index_body() {
    let d = tempfile::tempdir().unwrap();
    let csv = sample_csv(&d, 300);
    build(&csv, &BuildOptions::new("k", true)).unwrap();
    let ip = index_path(&csv);
    let good = fs::read(&ip).unwrap();

    // change one row offset (header CRC does not cover the body)
    let mut b = good.clone();
    let p = HEADER_LEN_FOR_TEST + 16 * 100 + 8;
    b[p] ^= 0x01;
    fs::write(&ip, &b).unwrap();
    let r = verify_with(&csv, default_hash, 16).unwrap();
    assert!(!r.ok() && r.mismatches >= 1, "{r:?}");

    // swap two neighbouring entries: still the right multiset, but not sorted
    let mut b = good.clone();
    let (a, c) = (HEADER_LEN_FOR_TEST + 16 * 50, HEADER_LEN_FOR_TEST + 16 * 51);
    let first: Vec<u8> = b[a..a + 16].to_vec();
    let second: Vec<u8> = b[c..c + 16].to_vec();
    if first != second {
        b[a..a + 16].copy_from_slice(&second);
        b[c..c + 16].copy_from_slice(&first);
        fs::write(&ip, &b).unwrap();
        let r = verify_with(&csv, default_hash, 16).unwrap();
        assert!(!r.ok() && !r.sorted, "{r:?}");
    }

    fs::write(&ip, &good).unwrap();
    assert!(verify_with(&csv, default_hash, 16).unwrap().ok());
}

const HEADER_LEN_FOR_TEST: usize = csvidx::header::HEADER_LEN;

// ---------------------------------------------------------------------------
// Tail index: `update` writes only <csv>.idx.tail and leaves the main index alone
// ---------------------------------------------------------------------------

fn data_line(i: u32) -> String {
    format!("{},{},p", i, (i * 7919) % 53)
}

fn plain_csv(dir: &tempfile::TempDir, rows: u32) -> (PathBuf, Vec<String>) {
    let lines: Vec<String> = (0..rows).map(data_line).collect();
    let mut s = String::from("id,k,v\n");
    for l in &lines {
        s.push_str(l);
        s.push('\n');
    }
    (tmp_csv(dir, s.as_bytes()), lines)
}

fn expected(lines: &[String], k: u32) -> Vec<String> {
    lines.iter().filter(|l| l.split(',').nth(1).unwrap() == k.to_string()).cloned().collect()
}

fn assert_all_keys(csv: &Path, lines: &[String]) {
    for k in 0..54u32 {
        assert_eq!(get(csv, &k.to_string()), expected(lines, k), "key {k}");
    }
}

fn never() -> UpdateOptions {
    let mut o = UpdateOptions::new(default_hash);
    o.compact = CompactPolicy::Never;
    o
}

#[test]
fn update_writes_only_the_tail_and_lookups_see_both() {
    let d = tempfile::tempdir().unwrap();
    let (csv, mut lines) = plain_csv(&d, 5000);
    build(&csv, &BuildOptions::new("k", true)).unwrap();
    let main_before = fs::read(index_path(&csv)).unwrap();
    assert!(!tail_path(&csv).exists());

    let mut extra = String::new();
    for i in 5000..5030u32 {
        lines.push(data_line(i));
        extra.push_str(&format!("{}\n", data_line(i)));
    }
    append(&csv, extra.as_bytes());
    assert_eq!(get(&csv, "0").len(), expected(&lines[..5000], 0).len()); // not visible before update

    let u = update_opts(&csv, &never()).unwrap();
    assert!(!u.compacted && u.added == 30 && u.tail_entries == 30, "{u:?}");
    assert_eq!(fs::read(index_path(&csv)).unwrap(), main_before, "main index must not change");
    assert!(tail_path(&csv).exists());
    assert_all_keys(&csv, &lines);
    assert!(verify(&csv, default_hash).unwrap().ok());
    let idx = Index::open(&csv).unwrap();
    assert_eq!(idx.freshness(), Freshness::Fresh);
    assert_eq!(idx.tail_header().unwrap().entry_count, 30);
    drop(idx);

    // a second update grows the tail, main still untouched
    let mut extra = String::new();
    for i in 5030..5100u32 {
        lines.push(data_line(i));
        extra.push_str(&format!("{}\n", data_line(i)));
    }
    append(&csv, extra.as_bytes());
    let u = update_opts(&csv, &never()).unwrap();
    assert_eq!((u.added, u.tail_entries), (70, 100), "{u:?}");
    assert_eq!(fs::read(index_path(&csv)).unwrap(), main_before);
    assert_all_keys(&csv, &lines);
    assert!(update_opts(&csv, &never()).unwrap().up_to_date);

    // compaction == a fresh full build, byte for byte, and the tail is gone
    let c = compact(&csv, default_hash).unwrap();
    assert!(c.merged && c.entries == 5100);
    assert!(!tail_path(&csv).exists());
    let compacted = fs::read(index_path(&csv)).unwrap();
    assert_all_keys(&csv, &lines);
    build(&csv, &BuildOptions::new("k", true)).unwrap();
    assert_eq!(fs::read(index_path(&csv)).unwrap(), compacted, "compact != rebuild");
    assert!(!compact(&csv, default_hash).unwrap().merged);
}

#[test]
fn tail_replaces_the_provisional_last_row_of_the_main_index() {
    let d = tempfile::tempdir().unwrap();
    let (csv, mut lines) = plain_csv(&d, 3000);
    // chop the final newline: the last row is indexed provisionally in the MAIN index
    let mut data = fs::read(&csv).unwrap();
    data.pop();
    fs::write(&csv, &data).unwrap();
    let s = build(&csv, &BuildOptions::new("k", true)).unwrap();
    assert!(s.unterminated_tail);
    assert_all_keys(&csv, &lines);

    // the last row grows ("..,p" -> "..,p!") and more rows arrive; it must not appear twice
    let last = lines.last_mut().unwrap();
    last.push('!');
    let mut extra = String::from("!\n");
    for i in 3000..3010u32 {
        lines.push(data_line(i));
        extra.push_str(&format!("{}\n", data_line(i)));
    }
    append(&csv, extra.as_bytes());
    update_opts(&csv, &never()).unwrap();
    assert_all_keys(&csv, &lines);
    let r = verify(&csv, default_hash).unwrap();
    assert!(r.ok() && r.entries == 3010, "{r:?}");

    // and again, this time ending in an unterminated row that is itself provisional in the tail
    let mut extra = String::new();
    for i in 3010..3020u32 {
        lines.push(data_line(i));
        extra.push_str(&format!("{}\n", data_line(i)));
    }
    lines.push("9999,7,unterminated".to_string());
    extra.push_str("9999,7,unterminated");
    append(&csv, extra.as_bytes());
    let u = update_opts(&csv, &never()).unwrap();
    assert_eq!(u.dropped_provisional, 0, "{u:?}");
    assert_all_keys(&csv, &lines);

    append(&csv, b"-more\n"); // the provisional tail row grows and gets terminated
    *lines.last_mut().unwrap() = "9999,7,unterminated-more".to_string();
    let u = update_opts(&csv, &never()).unwrap();
    assert_eq!(u.dropped_provisional, 1, "{u:?}");
    assert_all_keys(&csv, &lines);
    let r = verify(&csv, default_hash).unwrap();
    assert!(r.ok() && r.entries == 3021, "{r:?}");

    compact(&csv, default_hash).unwrap();
    let compacted = fs::read(index_path(&csv)).unwrap();
    build(&csv, &BuildOptions::new("k", true)).unwrap();
    assert_eq!(fs::read(index_path(&csv)).unwrap(), compacted);
}

#[test]
fn a_stale_tail_left_by_an_interrupted_compact_is_ignored() {
    let d = tempfile::tempdir().unwrap();
    let (csv, mut lines) = plain_csv(&d, 2000);
    build(&csv, &BuildOptions::new("k", true)).unwrap();
    let mut extra = String::new();
    for i in 2000..2040u32 {
        lines.push(data_line(i));
        extra.push_str(&format!("{}\n", data_line(i)));
    }
    append(&csv, extra.as_bytes());
    update_opts(&csv, &never()).unwrap();
    let tail_bytes = fs::read(tail_path(&csv)).unwrap();

    compact(&csv, default_hash).unwrap();
    // simulate a crash between "rename new main" and "delete tail": the old tail is back
    fs::write(tail_path(&csv), &tail_bytes).unwrap();
    assert_all_keys(&csv, &lines); // every row exactly once
    assert!(Index::open(&csv).unwrap().tail_header().is_none());
    assert!(verify(&csv, default_hash).unwrap().ok());

    // and the next update simply replaces it
    append(&csv, format!("{}\n", data_line(2040)).as_bytes());
    lines.push(data_line(2040));
    update_opts(&csv, &never()).unwrap();
    assert_all_keys(&csv, &lines);
    assert_eq!(Index::open(&csv).unwrap().tail_header().unwrap().entry_count, 1);
}

#[test]
fn build_discards_an_existing_tail() {
    let d = tempfile::tempdir().unwrap();
    let (csv, mut lines) = plain_csv(&d, 2000);
    build(&csv, &BuildOptions::new("k", true)).unwrap();
    append(&csv, format!("{}\n", data_line(2000)).as_bytes());
    lines.push(data_line(2000));
    update_opts(&csv, &never()).unwrap();
    assert!(tail_path(&csv).exists());
    build(&csv, &BuildOptions::new("k", true)).unwrap();
    assert!(!tail_path(&csv).exists());
    assert_all_keys(&csv, &lines);
}

#[test]
fn compaction_policy_auto_merges_only_when_the_tail_is_large() {
    let d = tempfile::tempdir().unwrap();
    let (csv, mut lines) = plain_csv(&d, 5000);
    build(&csv, &BuildOptions::new("k", true)).unwrap();

    let add = |csv: &Path, lines: &mut Vec<String>, from: u32, to: u32| {
        let mut extra = String::new();
        for i in from..to {
            lines.push(data_line(i));
            extra.push_str(&format!("{}\n", data_line(i)));
        }
        append(csv, extra.as_bytes());
    };

    add(&csv, &mut lines, 5000, 5100); // 100 / 5000 = 2% < 10%
    let u = update(&csv, default_hash).unwrap();
    assert!(!u.compacted && u.tail_entries == 100, "{u:?}");
    assert!(tail_path(&csv).exists());

    add(&csv, &mut lines, 5100, 5600); // tail would be 600 / 5000 = 12% >= 10%
    let u = update(&csv, default_hash).unwrap();
    assert!(u.compacted && u.tail_entries == 0 && u.entries_on_disk == 5600, "{u:?}");
    assert!(!tail_path(&csv).exists());
    assert_all_keys(&csv, &lines);

    add(&csv, &mut lines, 5600, 5601);
    let mut o = UpdateOptions::new(default_hash);
    o.compact = CompactPolicy::Always;
    assert!(update_opts(&csv, &o).unwrap().compacted);
    assert_all_keys(&csv, &lines);
}

#[test]
fn corrupt_or_foreign_tail_is_an_error_not_silent_data_loss() {
    let d = tempfile::tempdir().unwrap();
    let (csv, _) = plain_csv(&d, 2000);
    build(&csv, &BuildOptions::new("k", true)).unwrap();
    append(&csv, format!("{}\n", data_line(2000)).as_bytes());
    update_opts(&csv, &never()).unwrap();

    let tp = tail_path(&csv);
    let good = fs::read(&tp).unwrap();
    let mut b = good.clone();
    b[17] ^= 0xFF; // header CRC
    fs::write(&tp, &b).unwrap();
    assert!(Index::open(&csv).err().unwrap().to_string().contains("checksum"));

    fs::write(&tp, &good[..good.len() - 3]).unwrap(); // truncated
    assert!(Index::open(&csv).is_err());

    fs::write(&tp, &good).unwrap();
    assert!(Index::open(&csv).is_ok());
}

#[test]
fn verify_detects_a_damaged_tail_body() {
    let d = tempfile::tempdir().unwrap();
    let (csv, _) = plain_csv(&d, 2000);
    build(&csv, &BuildOptions::new("k", true)).unwrap();
    let mut extra = String::new();
    for i in 2000..2020u32 {
        extra.push_str(&format!("{}\n", data_line(i)));
    }
    append(&csv, extra.as_bytes());
    update_opts(&csv, &never()).unwrap();
    let tp = tail_path(&csv);
    let mut b = fs::read(&tp).unwrap();
    b[HEADER_LEN_FOR_TEST + 16 * 5 + 8] ^= 0x04; // one row offset in the tail
    fs::write(&tp, &b).unwrap();
    let r = verify(&csv, default_hash).unwrap();
    assert!(!r.ok() && r.mismatches >= 1, "{r:?}");
}

#[test]
fn tail_works_with_collisions_and_tiny_sort_memory() {
    let d = tempfile::tempdir().unwrap();
    let csv = tmp_csv(&d, b"k,v\na,1\nb,2\na,3\n");
    fn all_collide(_: &[u8]) -> u64 {
        7
    }
    let mut o = BuildOptions::new("k", true);
    o.hash = all_collide;
    build(&csv, &o).unwrap();
    append(&csv, b"b,4\na,5\nc,6\n");
    let mut u = UpdateOptions::new(all_collide);
    u.compact = CompactPolicy::Never;
    u.max_mem_entries = 1;
    update_opts(&csv, &u).unwrap();
    let idx = Index::open_with_hash(&csv, all_collide).unwrap();
    let got = |k: &str| -> Vec<String> { idx.lookup(k.as_bytes()).into_iter().map(|(_, r)| String::from_utf8(r).unwrap()).collect() };
    assert_eq!(got("a"), ["a,1", "a,3", "a,5"]);
    assert_eq!(got("b"), ["b,2", "b,4"]);
    assert_eq!(got("c"), ["c,6"]);
    assert!(verify_with(&csv, all_collide, 1).unwrap().ok());
}
