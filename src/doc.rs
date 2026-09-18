//! An opened CSV file: data + index + view state (cursor, scroll, columns).

use crate::csv::{self, Index, Mmap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

pub const MAX_COL_W: u16 = 40;
const MIN_COL_W: u16 = 3;
const SAMPLE_ROWS: usize = 1000;
const SAMPLE_BYTES: usize = 16 << 20;

pub struct Doc {
    pub path: PathBuf,
    pub name: String,
    map: Arc<Mmap>,
    pub index: Arc<Index>,
    pub delim: u8,
    pub has_header: bool,
    pub headers: Vec<String>,
    pub widths: Vec<u16>,
    pub numeric: Vec<bool>,
    pub row: usize,
    pub col: usize,
    pub row_off: usize,
    pub col_off: usize,
}

impl Doc {
    pub fn open(path: &Path) -> io::Result<Doc> {
        let map = Arc::new(Mmap::open(path)?);
        let index = Arc::new(Index::new());
        {
            let (m, i) = (map.clone(), index.clone());
            thread::Builder::new().name("indexer".into()).spawn(move || i.build(&m))?;
        }
        let delim = csv::sniff_delimiter(map.bytes(), path);
        let mut doc = Doc {
            path: path.to_path_buf(),
            name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            map,
            index,
            delim,
            has_header: true,
            headers: Vec::new(),
            widths: Vec::new(),
            numeric: Vec::new(),
            row: 0,
            col: 0,
            row_off: 0,
            col_off: 0,
        };
        doc.analyze();
        Ok(doc)
    }

    pub fn data(&self) -> &[u8] {
        self.map.bytes()
    }

    pub fn map(&self) -> Arc<Mmap> {
        self.map.clone()
    }

    pub fn size(&self) -> usize {
        self.map.bytes().len()
    }

    fn hdr(&self) -> usize {
        self.has_header as usize
    }

    /// Number of data rows indexed so far.
    pub fn rows(&self) -> usize {
        self.index.rows().saturating_sub(self.hdr())
    }

    pub fn ncols(&self) -> usize {
        self.widths.len()
    }

    pub fn record_start(&self, row: usize) -> Option<usize> {
        self.index.record_start(self.data(), row + self.hdr())
    }

    /// Parses data row `row` into raw field ranges.
    pub fn read_row(&self, row: usize, fields: &mut Vec<(usize, usize)>) -> bool {
        match self.record_start(row) {
            Some(s) => {
                csv::parse_record(self.data(), s, self.delim, fields);
                true
            }
            None => false,
        }
    }

    /// Converts a byte offset into (data row, column).
    pub fn locate(&self, off: usize, fields: &mut Vec<(usize, usize)>) -> (usize, usize) {
        let data = self.data();
        let phys = self.index.record_of_offset(data, off);
        let start = self.index.record_start(data, phys).unwrap_or(0);
        csv::parse_record(data, start, self.delim, fields);
        let col = fields.iter().position(|&(_, e)| off < e).unwrap_or(fields.len().saturating_sub(1));
        (phys.saturating_sub(self.hdr()), col)
    }

    /// Derives column names, widths and alignment from a bounded sample.
    pub fn analyze(&mut self) {
        let data = self.map.bytes();
        let mut fields = Vec::new();
        let mut buf = String::new();
        let mut widths: Vec<u16> = Vec::new();
        let mut stats: Vec<(u32, u32)> = Vec::new(); // (numeric, non-empty)
        let mut headers = Vec::new();
        let mut pos = 0;
        let mut n = 0;
        while pos < data.len() && n < SAMPLE_ROWS && pos < SAMPLE_BYTES {
            let next = csv::parse_record(data, pos, self.delim, &mut fields);
            if n == 0 && self.has_header {
                for &(s, e) in &fields {
                    csv::decode_field(&data[s..e], 200, false, &mut buf);
                    headers.push(buf.trim().to_string());
                }
            } else {
                if widths.len() < fields.len() {
                    widths.resize(fields.len(), 0);
                    stats.resize(fields.len(), (0, 0));
                }
                for (i, &(s, e)) in fields.iter().enumerate() {
                    let (w, _) = csv::decode_field(&data[s..e], MAX_COL_W as usize, false, &mut buf);
                    widths[i] = widths[i].max(w as u16);
                    if !buf.trim().is_empty() {
                        stats[i].1 += 1;
                        stats[i].0 += csv::looks_numeric(&buf) as u32;
                    }
                }
            }
            pos = next;
            n += 1;
        }
        let ncols = widths.len().max(headers.len()).max(1);
        widths.resize(ncols, 0);
        stats.resize(ncols, (0, 0));
        for (i, w) in widths.iter_mut().enumerate() {
            match headers.get_mut(i) {
                Some(h) if !h.is_empty() => {}
                Some(h) => *h = col_name(i),
                None => headers.push(col_name(i)),
            }
            let hw = crate::screen::str_width(&headers[i]).min(MAX_COL_W as usize) as u16;
            *w = (*w).max(hw).clamp(MIN_COL_W, MAX_COL_W);
        }
        self.numeric = stats.iter().map(|&(num, non_empty)| non_empty > 0 && num * 10 >= non_empty * 9).collect();
        self.headers = headers;
        self.widths = widths;
        self.col = self.col.min(ncols - 1);
        self.col_off = self.col_off.min(self.col);
    }

    /// Adds columns discovered in rows wider than the sample.
    pub fn ensure_cols(&mut self, n: usize) {
        while self.widths.len() < n {
            self.headers.push(col_name(self.widths.len()));
            self.widths.push(8);
            self.numeric.push(false);
        }
    }

    pub fn toggle_header(&mut self) {
        self.has_header = !self.has_header;
        let off = if self.has_header { -1 } else { 1 };
        self.row = self.row.saturating_add_signed(off);
        self.analyze();
        self.clamp();
    }

    pub fn cycle_delimiter(&mut self) {
        const CYCLE: [u8; 4] = *b",;\t|";
        let i = CYCLE.iter().position(|&d| d == self.delim).map_or(0, |i| (i + 1) % CYCLE.len());
        self.delim = CYCLE[i];
        self.col = 0;
        self.col_off = 0;
        self.analyze();
    }

    pub fn resize_col(&mut self, delta: i16) {
        if let Some(w) = self.widths.get_mut(self.col) {
            *w = w.saturating_add_signed(delta).clamp(1, 500);
        }
    }

    /// Fits the current column to the widest of the given rows.
    pub fn autofit(&mut self, first: usize, count: usize) {
        let col = self.col;
        let mut fields = Vec::new();
        let mut buf = String::new();
        let mut w = crate::screen::str_width(&self.headers[col]).min(200);
        let data = self.data();
        if let Some(mut pos) = self.record_start(first) {
            for _ in 0..count.min(self.rows().saturating_sub(first)) {
                pos = csv::parse_record(data, pos, self.delim, &mut fields);
                if let Some(&(s, e)) = fields.get(col) {
                    w = w.max(csv::decode_field(&data[s..e], 200, false, &mut buf).0);
                }
            }
        }
        self.widths[col] = (w as u16).max(1);
    }

    pub fn move_row(&mut self, delta: isize) {
        self.row = self.row.saturating_add_signed(delta);
        self.clamp();
    }

    pub fn move_col(&mut self, delta: isize) {
        self.col = self.col.saturating_add_signed(delta);
        self.clamp();
    }

    pub fn clamp(&mut self) {
        self.row = self.row.min(self.rows().saturating_sub(1));
        self.col = self.col.min(self.ncols().saturating_sub(1));
    }
}

impl Drop for Doc {
    fn drop(&mut self) {
        self.index.cancel();
    }
}

/// Spreadsheet-style column name: A, B, …, Z, AA, AB, …
pub fn col_name(mut i: usize) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (i % 26) as u8);
        if i < 26 {
            break;
        }
        i = i / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap()
}

#[cfg(test)]
mod tests {
    #[test]
    fn col_names() {
        assert_eq!(super::col_name(0), "A");
        assert_eq!(super::col_name(25), "Z");
        assert_eq!(super::col_name(26), "AA");
        assert_eq!(super::col_name(701), "ZZ");
        assert_eq!(super::col_name(702), "AAA");
    }
}
