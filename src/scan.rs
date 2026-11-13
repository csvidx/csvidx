//! Allocation-free CSV record scanner (RFC 4180 with lenient edge cases).
//!
//! Handles quoted fields, `""` escapes, newlines inside quotes, `\n` and `\r\n`
//! terminators, empty fields, blank lines and an unterminated last record.

use memchr::{memchr, memchr2};
use std::borrow::Cow;

pub const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// One field of a record. `value()` removes the surrounding quotes and
/// un-escapes `""`; it only allocates when the field actually contained `""`.
#[derive(Clone, Copy, Debug)]
pub struct Field<'a> {
    raw: &'a [u8],
    escaped: bool,
}

impl<'a> Field<'a> {
    #[inline]
    pub fn value(&self) -> Cow<'a, [u8]> {
        if !self.escaped {
            return Cow::Borrowed(self.raw);
        }
        let mut out = Vec::with_capacity(self.raw.len());
        let mut i = 0;
        while i < self.raw.len() {
            out.push(self.raw[i]);
            if self.raw[i] == b'"' && i + 1 < self.raw.len() && self.raw[i + 1] == b'"' {
                i += 2;
            } else {
                i += 1;
            }
        }
        Cow::Owned(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordEnd {
    /// Offset where the next record starts.
    pub end: usize,
    /// End of the record's bytes without its line terminator.
    pub line_end: usize,
    /// The line was empty (nothing to index).
    pub blank: bool,
    /// The record ended with `\n` (false: it runs into EOF and may still grow).
    pub terminated: bool,
    pub cols: usize,
}

/// Scan the record that starts at `start`, calling `on_field(column, field)` for
/// each field. Returns `None` at EOF.
pub fn scan_record<'a>(
    buf: &'a [u8],
    start: usize,
    mut on_field: impl FnMut(usize, Field<'a>),
) -> Option<RecordEnd> {
    let n = buf.len();
    if start >= n {
        return None;
    }
    if buf[start] == b'\n' {
        return Some(RecordEnd { end: start + 1, line_end: start, blank: true, terminated: true, cols: 0 });
    }
    if buf[start] == b'\r' && start + 1 < n && buf[start + 1] == b'\n' {
        return Some(RecordEnd { end: start + 2, line_end: start, blank: true, terminated: true, cols: 0 });
    }

    let mut pos = start;
    let mut col = 0usize;
    loop {
        let (field, term) = if pos < n && buf[pos] == b'"' {
            // Quoted field: find the closing quote, skipping `""` pairs.
            let cs = pos + 1;
            let mut p = cs;
            let mut escaped = false;
            let (ce, after) = loop {
                match memchr(b'"', &buf[p..]) {
                    None => break (n, n),
                    Some(i) => {
                        let q = p + i;
                        if q + 1 < n && buf[q + 1] == b'"' {
                            escaped = true;
                            p = q + 2;
                        } else {
                            break (q, q + 1);
                        }
                    }
                }
            };
            let term = if after >= n { None } else { memchr2(b',', b'\n', &buf[after..]).map(|i| after + i) };
            (Field { raw: &buf[cs..ce], escaped }, term)
        } else {
            let term = if pos >= n { None } else { memchr2(b',', b'\n', &buf[pos..]).map(|i| pos + i) };
            let mut ce = term.unwrap_or(n);
            if let Some(t) = term {
                if buf[t] == b'\n' && ce > pos && buf[ce - 1] == b'\r' {
                    ce -= 1;
                }
            }
            let cs = pos.min(ce);
            (Field { raw: &buf[cs..ce], escaped: false }, term)
        };

        on_field(col, field);
        col += 1;

        match term {
            Some(t) if buf[t] == b',' => pos = t + 1,
            Some(t) => {
                let mut le = t;
                if le > start && buf[le - 1] == b'\r' {
                    le -= 1;
                }
                return Some(RecordEnd { end: t + 1, line_end: le, blank: false, terminated: true, cols: col });
            }
            None => {
                return Some(RecordEnd { end: n, line_end: n, blank: false, terminated: false, cols: col });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(buf: &[u8], start: usize) -> (Vec<Vec<u8>>, RecordEnd) {
        let mut v = Vec::new();
        let r = scan_record(buf, start, |_, f| v.push(f.value().into_owned())).unwrap();
        (v, r)
    }

    fn s(v: &[Vec<u8>]) -> Vec<String> {
        v.iter().map(|x| String::from_utf8_lossy(x).into_owned()).collect()
    }

    #[test]
    fn simple() {
        let (f, r) = fields(b"a,b,c\nx\n", 0);
        assert_eq!(s(&f), ["a", "b", "c"]);
        assert_eq!((r.end, r.line_end, r.terminated, r.cols), (6, 5, true, 3));
    }

    #[test]
    fn crlf() {
        let buf = b"a,b\r\nc,d\r\n";
        let (f, r) = fields(buf, 0);
        assert_eq!(s(&f), ["a", "b"]);
        assert_eq!((r.end, r.line_end), (5, 3));
        let (f, _) = fields(buf, r.end);
        assert_eq!(s(&f), ["c", "d"]);
    }

    #[test]
    fn quotes_commas_newlines_escapes() {
        let buf = b"1,\"he said \"\"hi\"\", ok\nbye\",x\nnext\n";
        let (f, r) = fields(buf, 0);
        assert_eq!(s(&f), ["1", "he said \"hi\", ok\nbye", "x"]);
        assert_eq!(&buf[r.end..], b"next\n");
    }

    #[test]
    fn quoted_then_crlf() {
        let buf = b"\"a\",\"b\"\r\nz\n";
        let (f, r) = fields(buf, 0);
        assert_eq!(s(&f), ["a", "b"]);
        assert_eq!(r.end, 9);
        assert_eq!(r.line_end, 7);
    }

    #[test]
    fn empty_fields() {
        let (f, r) = fields(b",,\n", 0);
        assert_eq!(s(&f), ["", "", ""]);
        assert!(r.terminated);
    }

    #[test]
    fn trailing_comma_at_eof() {
        let (f, r) = fields(b"a,b,", 0);
        assert_eq!(s(&f), ["a", "b", ""]);
        assert!(!r.terminated);
    }

    #[test]
    fn unterminated_last_record() {
        let (f, r) = fields(b"a,b", 0);
        assert_eq!(s(&f), ["a", "b"]);
        assert!(!r.terminated);
        assert_eq!(r.end, 3);
    }

    #[test]
    fn blank_lines() {
        let r = scan_record(b"\n", 0, |_, _| {}).unwrap();
        assert!(r.blank);
        let r = scan_record(b"\r\n", 0, |_, _| {}).unwrap();
        assert!(r.blank && r.end == 2);
        assert!(scan_record(b"abc", 3, |_, _| {}).is_none());
    }

    #[test]
    fn empty_quoted_field() {
        let (f, _) = fields(b"\"\",x\n", 0);
        assert_eq!(s(&f), ["", "x"]);
    }
}
