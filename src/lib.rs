//! csvidx: a secondary index for huge CSV files.
//!
//! The CSV is never modified or imported. Next to it lives `<file>.idx`, a flat
//! array of `(key_hash, row_offset)` pairs sorted by hash. A lookup is a binary
//! search in the (memory-mapped) index followed by reading only the matching rows.

pub mod header;
pub mod index;
pub mod scan;
pub mod sorter;
