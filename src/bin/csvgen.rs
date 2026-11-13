//! Test data generator: orders.csv with columns
//! order_id,user_id,ts,amount,status,comment
//!
//! 20% of rows belong to ~1000 "heavy" users (many rows per key); the rest are
//! spread uniformly. With --tricky, some rows contain quoted keys, commas,
//! escaped quotes and newlines inside quoted fields.

use clap::Parser;
use std::fs::File;
use std::io::{BufWriter, Write};

#[derive(Parser)]
#[command(about = "Generate a synthetic orders CSV for csvidx benchmarks")]
struct Args {
    #[arg(long)]
    rows: u64,
    #[arg(long)]
    users: u64,
    #[arg(long)]
    out: String,
    /// Write a file with sample keys (~9% of them do not exist)
    #[arg(long)]
    keys_out: Option<String>,
    #[arg(long, default_value_t = 2000)]
    keys_count: usize,
    #[arg(long, default_value_t = 1)]
    seed: u64,
    #[arg(long)]
    tricky: bool,
    #[arg(long)]
    crlf: bool,
    #[arg(long)]
    bom: bool,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn main() -> std::io::Result<()> {
    let a = Args::parse();
    let mut rng = Rng(a.seed);
    let nl: &[u8] = if a.crlf { b"\r\n" } else { b"\n" };
    let statuses = ["paid", "pending", "refunded", "cancelled"];

    let mut w = BufWriter::with_capacity(1 << 20, File::create(&a.out)?);
    if a.bom {
        w.write_all(&[0xEF, 0xBB, 0xBF])?;
    }
    w.write_all(b"order_id,user_id,ts,amount,status,comment")?;
    w.write_all(nl)?;

    for i in 0..a.rows {
        let r = rng.next();
        let user = if r % 5 == 0 { 1 + (r >> 8) % 1000.min(a.users) } else { 1 + (r >> 8) % a.users };
        let amount = rng.next() % 1_000_000;
        let status = statuses[(rng.next() % 4) as usize];
        let pick = rng.next() % 100;

        write!(w, "{},", i + 1)?;
        if a.tricky && pick == 8 {
            write!(w, "\"{}\",", user)?; // quoted numeric key
        } else {
            write!(w, "{},", user)?;
        }
        write!(w, "{},{}.{:02},{},", 1_735_689_600 + i, amount / 100, amount % 100, status)?;
        if a.tricky {
            match pick {
                0..=2 => w.write_all(b"\"hello, world\"")?,
                3..=4 => w.write_all(b"\"she said \"\"hi\"\"\"")?,
                5..=6 => w.write_all(b"\"line one\nline two\"")?,
                7 => w.write_all(b"\"\"")?,
                _ => {}
            }
        } else if pick < 50 {
            w.write_all(b"ok")?;
        }
        w.write_all(nl)?;
    }
    w.flush()?;

    if let Some(path) = &a.keys_out {
        let mut k = BufWriter::new(File::create(path)?);
        let span = a.users + a.users / 10; // ~9% beyond the real key range
        for j in 0..a.keys_count {
            let key = if j % 10 == 0 { 1 + rng.next() % 1000.min(a.users) } else { 1 + rng.next() % span };
            writeln!(k, "{key}")?;
        }
        k.flush()?;
    }
    eprintln!("wrote {} rows to {}", a.rows, a.out);
    Ok(())
}
