//! On-disk header of an index file (64 bytes, little-endian). Used by both the main
//! index (`file.csv.idx`) and the tail index (`file.csv.idx.tail`).
//!
//! ```text
//!  0  magic          [u8; 8]  "CSVIDX01"
//!  8  version        u32      2
//! 12  flags          u32      bit0: CSV has a header row, bit1: this is a tail file
//! 16  csv_size       u64      CSV size when this file was last written
//! 24  base_offset    u64      first CSV byte this file's scan started at
//!                             (main: first data row; tail: end of the main index's coverage)
//! 32  indexed_up_to  u64      where the next incremental scan starts
//! 40  entry_count    u64
//! 48  prefix_hash    u64      xxh3 of the first min(64 KiB, indexed_up_to) bytes of the CSV
//! 56  key_col        u32      zero-based key column
//! 60  header_crc32   u32      CRC32 of bytes 0..60
//! ```
//! Entries follow: `(key_hash: u64, row_offset: u64)`, sorted by that pair.
//!
//! A file covers the rows that start in `[base_offset, indexed_up_to)`. A row at or after
//! `indexed_up_to` may be present as well: it is the last row of the file without a
//! trailing newline, indexed provisionally because it may still grow.

use anyhow::{bail, Result};

pub const MAGIC: &[u8; 8] = b"CSVIDX01";
pub const HEADER_LEN: usize = 64;
pub const ENTRY_LEN: usize = 16;
pub const VERSION: u32 = 2;
pub const FLAG_HAS_HEADER: u32 = 1;
pub const FLAG_TAIL: u32 = 2;
pub const PREFIX_LEN: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub flags: u32,
    pub csv_size: u64,
    pub base_offset: u64,
    pub indexed_up_to: u64,
    pub entry_count: u64,
    pub prefix_hash: u64,
    pub key_col: u32,
}

impl Header {
    pub fn has_header(&self) -> bool {
        self.flags & FLAG_HAS_HEADER != 0
    }

    pub fn is_tail(&self) -> bool {
        self.flags & FLAG_TAIL != 0
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..8].copy_from_slice(MAGIC);
        b[8..12].copy_from_slice(&VERSION.to_le_bytes());
        b[12..16].copy_from_slice(&self.flags.to_le_bytes());
        b[16..24].copy_from_slice(&self.csv_size.to_le_bytes());
        b[24..32].copy_from_slice(&self.base_offset.to_le_bytes());
        b[32..40].copy_from_slice(&self.indexed_up_to.to_le_bytes());
        b[40..48].copy_from_slice(&self.entry_count.to_le_bytes());
        b[48..56].copy_from_slice(&self.prefix_hash.to_le_bytes());
        b[56..60].copy_from_slice(&self.key_col.to_le_bytes());
        let crc = crc32fast::hash(&b[0..60]);
        b[60..64].copy_from_slice(&crc.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Header> {
        if b.len() < HEADER_LEN {
            bail!("index file is too short to contain a header");
        }
        if &b[0..8] != MAGIC {
            bail!("not a csvidx file (bad magic)");
        }
        let u32at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let u64at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        if u32at(8) != VERSION {
            bail!(
                "unsupported index version {} (this build writes version {}); rebuild the index with `csvidx build`",
                u32at(8),
                VERSION
            );
        }
        if crc32fast::hash(&b[0..60]) != u32at(60) {
            bail!("index header checksum mismatch (corrupt index)");
        }
        Ok(Header {
            flags: u32at(12),
            csv_size: u64at(16),
            base_offset: u64at(24),
            indexed_up_to: u64at(32),
            entry_count: u64at(40),
            prefix_hash: u64at(48),
            key_col: u32at(56),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_corruption() {
        let h = Header {
            flags: FLAG_HAS_HEADER | FLAG_TAIL,
            csv_size: 123456789012,
            base_offset: 42,
            indexed_up_to: 123456789000,
            entry_count: 7,
            prefix_hash: 0xDEADBEEF,
            key_col: 3,
        };
        let mut b = h.encode();
        let d = Header::decode(&b).unwrap();
        assert_eq!(d, h);
        assert!(d.is_tail() && d.has_header());
        b[20] ^= 1;
        assert!(Header::decode(&b).is_err());
    }

    #[test]
    fn old_version_is_rejected_with_a_hint() {
        let mut b = Header { flags: 0, csv_size: 1, base_offset: 0, indexed_up_to: 1, entry_count: 0, prefix_hash: 0, key_col: 0 }.encode();
        b[8..12].copy_from_slice(&1u32.to_le_bytes());
        let e = Header::decode(&b).unwrap_err().to_string();
        assert!(e.contains("version 1") && e.contains("csvidx build"), "{e}");
    }
}
