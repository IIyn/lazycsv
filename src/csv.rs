//! CSV engine: memory mapping, SIMD record indexing, record parsing,
//! field decoding and byte search.
//!
//! Memory model: the file is never copied into the heap. It is mmap'ed and
//! the only structure we build is a *sparse* index holding the byte offset
//! of one record out of every `STRIDE`. For 100M rows that is ~6 MB. Any
//! record is then reached by jumping to the nearest checkpoint and skipping
//! at most `STRIDE - 1` records.

use crate::screen::char_width;
use crate::sys;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::ptr;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// One checkpoint every `STRIDE` records.
pub const STRIDE: usize = 128;

// ---------------------------------------------------------------------------
// File mapping
// ---------------------------------------------------------------------------

enum Source {
    Mapped { ptr: *const u8, len: usize, file: File },
    Owned(Vec<u8>),
}

/// Read-only view of a file's bytes.
///
/// Two access paths, on purpose:
/// - `bytes()` (mmap) for the few records on screen: random access, only
///   the touched pages ever become resident.
/// - `read_at()` (pread into a caller buffer) for full-file passes
///   (indexing, search). Streaming through the mapping would leave every
///   page of the file attached to the process, making its RSS grow to the
///   size of the file.
///
/// Pipes and special files fall back to a heap copy.
pub struct Mmap(Source);

// The mapping is read-only and never mutated, so sharing it is sound.
unsafe impl Send for Mmap {}
unsafe impl Sync for Mmap {}

impl Mmap {
    pub fn open(path: &Path) -> io::Result<Mmap> {
        let mut file = File::open(path)?;
        let meta = file.metadata()?;
        let len = meta.len() as usize;
        if meta.is_file() && len > 0 {
            let p = unsafe {
                sys::mmap(ptr::null_mut(), len, sys::PROT_READ, sys::MAP_PRIVATE, file.as_raw_fd(), 0)
            };
            if p != sys::MAP_FAILED {
                // Only on-screen records go through the mapping: no read-ahead.
                unsafe { sys::madvise(p, len, sys::MADV_RANDOM) };
                return Ok(Mmap(Source::Mapped { ptr: p as *const u8, len, file }));
            }
        }
        let mut v = Vec::new();
        file.read_to_end(&mut v)?;
        Ok(Mmap(Source::Owned(v)))
    }

    #[cfg(test)]
    pub fn from_vec(v: Vec<u8>) -> Mmap {
        Mmap(Source::Owned(v))
    }

    pub fn bytes(&self) -> &[u8] {
        match &self.0 {
            Source::Mapped { ptr, len, .. } => unsafe { std::slice::from_raw_parts(*ptr, *len) },
            Source::Owned(v) => v,
        }
    }

    /// Drops the pages the viewer touched from this process (they stay in
    /// the page cache). Keeps RSS flat during long browsing sessions.
    pub fn release(&self) {
        if let Source::Mapped { ptr, len, .. } = self.0 {
            unsafe { sys::madvise(ptr as *mut _, len, sys::MADV_DONTNEED) };
        }
    }

    pub fn len(&self) -> usize {
        self.bytes().len()
    }

    /// Fills `buf` from byte `off` without touching the mapping. Returns
    /// the number of bytes read (short only at end of file).
    pub fn read_at(&self, buf: &mut [u8], off: usize) -> usize {
        match &self.0 {
            Source::Mapped { file, len, .. } => {
                let want = buf.len().min(len.saturating_sub(off));
                let mut done = 0;
                while done < want {
                    match file.read_at(&mut buf[done..want], (off + done) as u64) {
                        Ok(0) => break,
                        Ok(n) => done += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                done
            }
            Source::Owned(v) => {
                let src = v.get(off..).unwrap_or(&[]);
                let n = buf.len().min(src.len());
                buf[..n].copy_from_slice(&src[..n]);
                n
            }
        }
    }
}

impl Drop for Mmap {
    fn drop(&mut self) {
        if let Source::Mapped { ptr, len, .. } = self.0 {
            unsafe { sys::munmap(ptr as *mut _, len) };
        }
    }
}

/// Size of the streaming buffer used by full-file passes (multiple of 64).
/// Small enough to stay in L2: the scan then reads what `pread` just wrote
/// from cache (256K: 4.2 GB/s, 4M: 1.8 GB/s).
const STREAM_BUF: usize = 256 << 10;

// ---------------------------------------------------------------------------
// SIMD helpers
// ---------------------------------------------------------------------------

/// Bitmasks of the positions of '\n' and '"' in a 64-byte block.
#[inline(always)]
fn block_masks(b: &[u8; 64]) -> (u64, u64) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::*;
        let nl = _mm_set1_epi8(b'\n' as i8);
        let qt = _mm_set1_epi8(b'"' as i8);
        let mut mn = 0u64;
        let mut mq = 0u64;
        for i in 0..4 {
            let v = _mm_loadu_si128(b.as_ptr().add(i * 16) as *const __m128i);
            mn |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, nl)) as u32 as u64) << (i * 16);
            mq |= (_mm_movemask_epi8(_mm_cmpeq_epi8(v, qt)) as u32 as u64) << (i * 16);
        }
        (mn, mq)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let mut mn = 0u64;
        let mut mq = 0u64;
        for (i, &c) in b.iter().enumerate() {
            mn |= ((c == b'\n') as u64) << i;
            mq |= ((c == b'"') as u64) << i;
        }
        (mn, mq)
    }
}

/// Bit i of the result = XOR of bits 0..=i of x. Turns a mask of quote
/// characters into a mask of "inside quotes" regions (simdjson trick).
#[inline(always)]
fn prefix_xor(mut x: u64) -> u64 {
    x ^= x << 1;
    x ^= x << 2;
    x ^= x << 4;
    x ^= x << 8;
    x ^= x << 16;
    x ^= x << 32;
    x
}

/// Position of the first byte equal to `a` or `b`.
#[inline]
pub fn find2(hay: &[u8], a: u8, b: u8) -> Option<usize> {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::*;
        let va = _mm_set1_epi8(a as i8);
        let vb = _mm_set1_epi8(b as i8);
        let mut i = 0;
        while i + 16 <= hay.len() {
            let v = _mm_loadu_si128(hay.as_ptr().add(i) as *const __m128i);
            let m = _mm_movemask_epi8(_mm_or_si128(_mm_cmpeq_epi8(v, va), _mm_cmpeq_epi8(v, vb)));
            if m != 0 {
                return Some(i + m.trailing_zeros() as usize);
            }
            i += 16;
        }
        hay[i..].iter().position(|&c| c == a || c == b).map(|p| p + i)
    }
    #[cfg(not(target_arch = "x86_64"))]
    hay.iter().position(|&c| c == a || c == b)
}

// ---------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------

/// Sparse record index, built by a background thread and readable while
/// being built.
pub struct Index {
    /// Byte offset of record `k * STRIDE`.
    checkpoints: RwLock<Vec<u64>>,
    rows: AtomicU64,
    scanned: AtomicU64,
    done: AtomicBool,
    cancel: AtomicBool,
}

impl Index {
    pub fn new() -> Index {
        Index {
            checkpoints: RwLock::new(vec![0]),
            rows: AtomicU64::new(0),
            scanned: AtomicU64::new(0),
            done: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
        }
    }

    pub fn rows(&self) -> usize {
        self.rows.load(Ordering::Acquire) as usize
    }
    pub fn scanned(&self) -> usize {
        self.scanned.load(Ordering::Acquire) as usize
    }
    pub fn done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn publish(&self, local: &mut Vec<u64>, rows: u64, scanned: usize) {
        if !local.is_empty() {
            self.checkpoints.write().unwrap().append(local);
        }
        self.rows.store(rows, Ordering::Release);
        self.scanned.store(scanned as u64, Ordering::Release);
    }

    /// Scans the whole file, 64 bytes per iteration, without branching on
    /// individual bytes. A newline ends a record unless it sits inside a
    /// quoted region (RFC 4180 multi-line fields are supported).
    pub fn build(&self, src: &Mmap) {
        let len = src.len();
        let mut s = Scanner {
            local: Vec::with_capacity(4096),
            rows: 0,
            until_cp: STRIDE as u64,
            in_quote: 0,
            last_end: 0,
        };
        let mut buf = vec![0u8; STREAM_BUF];
        let mut next_publish = 1usize << 20; // first results quickly, then in batches
        let mut base = 0usize;
        while base < len {
            let n = src.read_at(&mut buf, base);
            if n == 0 {
                break; // file shrank under us
            }
            let full = n / 64 * 64;
            for off in (0..full).step_by(64) {
                s.step(base + off, buf[off..off + 64].try_into().unwrap());
            }
            if full < n {
                // Only happens at end of file.
                let mut block = [0u8; 64];
                block[..n - full].copy_from_slice(&buf[full..n]);
                s.step(base + full, &block);
            }
            base += n;
            if base >= next_publish && base < len {
                self.publish(&mut s.local, s.rows, base);
                if self.cancel.load(Ordering::Relaxed) {
                    return;
                }
                next_publish = base + (8 << 20);
            }
        }
        if s.last_end < base {
            s.rows += 1; // final record without trailing newline
        }
        self.publish(&mut s.local, s.rows, len);
        self.done.store(true, Ordering::Release);
    }

    /// Byte offset where physical record `row` starts.
    pub fn record_start(&self, data: &[u8], row: usize) -> Option<usize> {
        if row >= self.rows() {
            return None;
        }
        let mut pos = *self.checkpoints.read().unwrap().get(row / STRIDE)? as usize;
        for _ in 0..row % STRIDE {
            pos = next_record(data, pos);
        }
        Some(pos)
    }

    /// Physical record containing byte `off`. The offset must already be
    /// covered by the index (`off < scanned()`).
    pub fn record_of_offset(&self, data: &[u8], off: usize) -> usize {
        let (k, mut pos) = {
            let cps = self.checkpoints.read().unwrap();
            let k = cps.partition_point(|&c| c as usize <= off).saturating_sub(1);
            (k, cps[k] as usize)
        };
        let mut row = k * STRIDE;
        loop {
            let next = next_record(data, pos);
            if next > off || next >= data.len() {
                return row;
            }
            pos = next;
            row += 1;
        }
    }
}

struct Scanner {
    /// Checkpoints found since the last publish.
    local: Vec<u64>,
    rows: u64,
    /// Records left before the next checkpoint.
    until_cp: u64,
    /// All ones when the previous block ended inside quotes.
    in_quote: u64,
    /// Offset right after the last record-ending newline.
    last_end: usize,
}

impl Scanner {
    #[inline(always)]
    fn step(&mut self, base: usize, block: &[u8; 64]) {
        let (nl, qt) = block_masks(block);
        let inside = prefix_xor(qt) ^ self.in_quote;
        self.in_quote = ((inside as i64) >> 63) as u64;
        let ends = nl & !inside;
        if ends == 0 {
            return;
        }
        self.last_end = base + 64 - ends.leading_zeros() as usize;
        let n = ends.count_ones() as u64;
        self.rows += n;
        if n < self.until_cp {
            self.until_cp -= n;
            return;
        }
        let mut m = ends;
        while m != 0 {
            let bit = m.trailing_zeros() as usize;
            m &= m - 1;
            self.until_cp -= 1;
            if self.until_cp == 0 {
                self.local.push((base + bit + 1) as u64);
                self.until_cp = STRIDE as u64;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Offset of the record following the one starting at `pos`.
pub fn next_record(data: &[u8], mut pos: usize) -> usize {
    let mut in_q = false;
    while let Some(p) = find2(&data[pos..], b'\n', b'"') {
        pos += p + 1;
        if data[pos - 1] == b'"' {
            in_q = !in_q;
        } else if !in_q {
            return pos;
        }
    }
    data.len()
}

/// Splits the record at `pos` into raw `(start, end)` field ranges (quotes
/// included). Returns the offset of the next record.
pub fn parse_record(data: &[u8], pos: usize, delim: u8, fields: &mut Vec<(usize, usize)>) -> usize {
    fields.clear();
    let mut start = pos;
    let mut i = pos;
    let mut in_q = false;
    while i < data.len() {
        let b = data[i];
        if b == b'"' {
            in_q = !in_q;
        } else if !in_q {
            if b == delim {
                fields.push((start, i));
                start = i + 1;
            } else if b == b'\n' {
                let end = if i > start && data[i - 1] == b'\r' { i - 1 } else { i };
                fields.push((start, end));
                return i + 1;
            }
        }
        i += 1;
    }
    let end = if i > start && data[i - 1] == b'\r' { i - 1 } else { i };
    fields.push((start, end));
    data.len()
}

/// Decodes a raw field for display into `out`: strips quotes, unescapes
/// `""`, replaces control characters. Stops once `max_w` columns are
/// filled. Returns `(width, truncated)`. Only a bounded prefix of the field
/// is ever looked at, so multi-megabyte cells cost nothing.
pub fn decode_field(raw: &[u8], max_w: usize, multiline: bool, out: &mut String) -> (usize, bool) {
    out.clear();
    let quoted = raw.first() == Some(&b'"');
    let body = if quoted { &raw[1..] } else { raw };
    let limit = max_w.saturating_add(2).saturating_mul(4);
    let (body, cut) = if body.len() > limit { (&body[..limit], true) } else { (body, false) };

    let mut d = Decoder { out, w: 0, max_w, quoted, multiline, pending_quote: false };
    for chunk in body.utf8_chunks() {
        for c in chunk.valid().chars() {
            if !d.push(c) {
                return (d.w, true);
            }
        }
        if !chunk.invalid().is_empty() && !d.push('\u{FFFD}') {
            return (d.w, true);
        }
    }
    (d.w, cut)
}

struct Decoder<'a> {
    out: &'a mut String,
    w: usize,
    max_w: usize,
    quoted: bool,
    multiline: bool,
    pending_quote: bool,
}

impl Decoder<'_> {
    /// Appends one source char. Returns false when the width budget is exhausted.
    #[inline]
    fn push(&mut self, c: char) -> bool {
        if self.quoted && c == '"' {
            // `""` is an escaped quote; a lone quote closes the field.
            self.pending_quote = !self.pending_quote;
            if self.pending_quote {
                return true;
            }
        } else {
            self.pending_quote = false;
        }
        let c = match c {
            '\r' => return true,
            '\n' if self.multiline => {
                self.out.push('\n');
                return true;
            }
            '\n' => '↵',
            '\t' => '→',
            c if (c as u32) < 0x20 || c as u32 == 0x7f => '·',
            c => c,
        };
        let cw = char_width(c);
        if self.w + cw > self.max_w {
            return false;
        }
        self.out.push(c);
        self.w += cw;
        true
    }
}

/// Guesses the delimiter from the file extension and the first lines.
pub fn sniff_delimiter(data: &[u8], path: &Path) -> u8 {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("tsv" | "tab") => return b'\t',
        Some("psv") => return b'|',
        _ => {}
    }
    let sample = &data[..data.len().min(64 << 10)];
    let truncated = sample.len() < data.len();
    let mut best = (b',', 0usize);
    let mut fields = Vec::new();
    for d in *b",;\t|" {
        let mut counts = Vec::new();
        let mut pos = 0;
        while pos < sample.len() && counts.len() < 30 {
            let next = parse_record(sample, pos, d, &mut fields);
            if next >= sample.len() && truncated && !counts.is_empty() {
                break; // last record is probably cut
            }
            counts.push(fields.len());
            pos = next;
        }
        let Some(&first) = counts.first() else { continue };
        if first < 2 {
            continue;
        }
        let consistent = counts.iter().filter(|&&c| c == first).count();
        let score = consistent * 1000 + first;
        if score > best.1 {
            best = (d, score);
        }
    }
    best.0
}

pub fn looks_numeric(s: &str) -> bool {
    let s = s.trim();
    let Some(first) = s.bytes().next() else { return false };
    (first.is_ascii_digit() || matches!(first, b'-' | b'+' | b'.'))
        && (s.parse::<f64>().is_ok() || s.replace(',', ".").parse::<f64>().is_ok())
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

const CHUNK: usize = STREAM_BUF;

fn find_in(hay: &[u8], n: &[u8], ci: bool) -> Option<usize> {
    if hay.len() < n.len() {
        return None;
    }
    let f = n[0];
    let (a, b) = if ci { (f.to_ascii_lowercase(), f.to_ascii_uppercase()) } else { (f, f) };
    let last = hay.len() - n.len();
    let mut i = 0;
    while i <= last {
        let j = i + find2(&hay[i..=last], a, b)?;
        let cand = &hay[j..j + n.len()];
        if if ci { cand.eq_ignore_ascii_case(n) } else { cand == n } {
            return Some(j);
        }
        i = j + 1;
    }
    None
}

/// Streams the file through one reusable buffer (see `Mmap` for why the
/// mapping is not used for full-file passes).
struct Searcher<'a> {
    src: &'a Mmap,
    n: &'a [u8],
    ci: bool,
    buf: Vec<u8>,
    cancel: &'a AtomicBool,
}

impl Searcher<'_> {
    /// Reads `[start, end)` (at most CHUNK + n - 1 bytes) into the buffer.
    fn load(&mut self, start: usize, end: usize) -> &[u8] {
        let got = self.src.read_at(&mut self.buf[..end - start], start);
        &self.buf[..got]
    }

    /// First match fully inside `[start, end)`.
    fn find(&mut self, start: usize, end: usize) -> Option<usize> {
        let (n, ci) = (self.n, self.ci);
        let mut cs = start;
        while cs < end {
            if self.cancel.load(Ordering::Relaxed) {
                return None;
            }
            let ce = (cs + CHUNK + n.len() - 1).min(end);
            if let Some(p) = find_in(self.load(cs, ce), n, ci) {
                return Some(cs + p);
            }
            cs += CHUNK;
        }
        None
    }

    /// Last match fully inside `[start, end)`.
    fn rfind(&mut self, start: usize, end: usize) -> Option<usize> {
        let (n, ci) = (self.n, self.ci);
        let mut ce = end;
        while ce > start {
            if self.cancel.load(Ordering::Relaxed) {
                return None;
            }
            let cs = ce.saturating_sub(CHUNK).max(start);
            let hay = self.load(cs, (ce + n.len() - 1).min(end));
            let mut found = None;
            let mut i = 0;
            while let Some(p) = find_in(&hay[i..], n, ci) {
                found = Some(cs + i + p);
                i += p + 1;
            }
            if found.is_some() {
                return found;
            }
            ce = cs;
        }
        None
    }
}

/// Searches `needle` from byte `from`, wrapping around the file. Smart
/// case: case-insensitive (ASCII) unless the needle has an uppercase letter.
/// Returns `(offset, wrapped)`.
pub fn search(src: &Mmap, needle: &str, from: usize, forward: bool, cancel: &AtomicBool) -> Option<(usize, bool)> {
    let n = needle.as_bytes();
    let len = src.len();
    if n.is_empty() || n.len() > len {
        return None;
    }
    let ci = !n.iter().any(u8::is_ascii_uppercase);
    let buf = vec![0u8; CHUNK + n.len() - 1];
    let mut s = Searcher { src, n, ci, buf, cancel };
    let from = from.min(len);
    let tail = n.len() - 1;
    if forward {
        s.find(from, len)
            .map(|p| (p, false))
            .or_else(|| s.find(0, (from + tail).min(len)).map(|p| (p, true)))
    } else {
        s.rfind(0, (from + tail).min(len))
            .map(|p| (p, false))
            .or_else(|| s.rfind(from, len).map(|p| (p, true)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(data: &[u8]) -> Index {
        let i = Index::new();
        i.build(&Mmap::from_vec(data.to_vec()));
        i
    }

    fn naive_records(data: &[u8]) -> Vec<usize> {
        let mut starts = Vec::new();
        let mut pos = 0;
        while pos < data.len() {
            starts.push(pos);
            pos = next_record(data, pos);
        }
        starts
    }

    #[test]
    fn index_matches_naive_scan() {
        let mut data = Vec::new();
        for i in 0..5000 {
            match i % 4 {
                0 => data.extend_from_slice(format!("{i},plain,row\n").as_bytes()),
                1 => data.extend_from_slice(format!("{i},\"multi\nline\",x\r\n").as_bytes()),
                2 => data.extend_from_slice(format!("{i},\"esc \"\"q\"\"\",y\n").as_bytes()),
                _ => data.extend_from_slice(format!("{i},{}\n", "z".repeat(i % 200)).as_bytes()),
            }
        }
        data.extend_from_slice(b"last,no,newline");
        let idx = index(&data);
        let starts = naive_records(&data);
        assert_eq!(idx.rows(), starts.len());
        for (r, &s) in starts.iter().enumerate() {
            assert_eq!(idx.record_start(&data, r), Some(s), "row {r}");
            assert_eq!(idx.record_of_offset(&data, s), r);
        }
    }

    #[test]
    fn edge_cases() {
        assert_eq!(index(b"").rows(), 0);
        assert_eq!(index(b"a").rows(), 1);
        assert_eq!(index(b"a\n").rows(), 1);
        assert_eq!(index(b"a\n\nb").rows(), 3);
        assert_eq!(index(b"\"a\nb\"\nc\n").rows(), 2);
    }

    #[test]
    fn parse_and_decode() {
        let data = b"a,\"b,\"\"c\"\"\",d\r\nx";
        let mut f = Vec::new();
        let next = parse_record(data, 0, b',', &mut f);
        assert_eq!(f.len(), 3);
        let mut s = String::new();
        decode_field(&data[f[1].0..f[1].1], 100, false, &mut s);
        assert_eq!(s, "b,\"c\"");
        decode_field(&data[f[2].0..f[2].1], 100, false, &mut s);
        assert_eq!(s, "d");
        assert_eq!(&data[next..], b"x");
        assert_eq!(decode_field(b"abcdef", 3, false, &mut s), (3, true));
        assert_eq!(s, "abc");
    }

    #[test]
    fn sniffing() {
        assert_eq!(sniff_delimiter(b"a;b;c\n1;2;3\n", Path::new("x.csv")), b';');
        assert_eq!(sniff_delimiter(b"a\tb\n1\t2\n", Path::new("x.txt")), b'\t');
        assert_eq!(sniff_delimiter(b"a,b\n1,2\n", Path::new("x.csv")), b',');
    }

    #[test]
    fn searching() {
        let c = AtomicBool::new(false);
        let data = &Mmap::from_vec(b"hello world, Hello again".to_vec());
        assert_eq!(search(data, "hello", 1, true, &c), Some((13, false)));
        assert_eq!(search(data, "Hello", 14, true, &c), Some((13, true)));
        assert_eq!(search(data, "hello", 13, false, &c), Some((0, false)));
        assert_eq!(search(data, "nope", 0, true, &c), None);
    }
}

#[cfg(test)]
mod bench {
    /// `LAZYCSV_BENCH=file.csv cargo test --release bench_index -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_index() {
        let path = std::env::var("LAZYCSV_BENCH").expect("set LAZYCSV_BENCH");
        let map = super::Mmap::open(std::path::Path::new(&path)).unwrap();
        let t = std::time::Instant::now();
        let idx = super::Index::new();
        idx.build(&map);
        let dt = t.elapsed();
        let mb = map.bytes().len() as f64 / 1e6;
        println!("{} rows, {mb:.0} MB in {dt:?} ({:.2} GB/s)", idx.rows(), mb / 1e3 / dt.as_secs_f64());
        let t = std::time::Instant::now();
        let r = idx.record_start(map.bytes(), idx.rows() - 1);
        println!("random access to last row: {:?} ({r:?})", t.elapsed());
    }
}
