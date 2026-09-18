//! Double-buffered cell grid. Each frame is drawn into a buffer and only the
//! cells that differ from the previous frame are sent to the terminal.

use std::io::{self, Write};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Color {
    #[default]
    Reset,
    Idx(u8),
}

pub const BOLD: u8 = 1;
pub const DIM: u8 = 2;
pub const ITALIC: u8 = 4;
pub const UNDERLINE: u8 = 8;
pub const REVERSE: u8 = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub attr: u8,
}

impl Style {
    pub const DEFAULT: Style = Style { fg: Color::Reset, bg: Color::Reset, attr: 0 };

    pub const fn fg(c: u8) -> Style {
        Style { fg: Color::Idx(c), bg: Color::Reset, attr: 0 }
    }
    pub const fn with_fg(mut self, c: u8) -> Style {
        self.fg = Color::Idx(c);
        self
    }
    pub const fn with_bg(mut self, c: u8) -> Style {
        self.bg = Color::Idx(c);
        self
    }
    pub const fn attr(mut self, a: u8) -> Style {
        self.attr |= a;
        self
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
    pub fn inner(&self) -> Rect {
        Rect {
            x: self.x + 1,
            y: self.y + 1,
            w: self.w.saturating_sub(2),
            h: self.h.saturating_sub(2),
        }
    }
    pub fn right(&self) -> u16 {
        self.x + self.w
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Cell {
    ch: char,
    st: Style,
}

const BLANK: Cell = Cell { ch: ' ', st: Style::DEFAULT };
/// Marks the right half of a double-width character.
const CONT: char = '\0';

pub struct Screen {
    pub w: u16,
    pub h: u16,
    cells: Vec<Cell>,
    prev: Vec<Cell>,
    full: bool,
    out: Vec<u8>,
}

impl Screen {
    pub fn new() -> Self {
        Screen { w: 0, h: 0, cells: Vec::new(), prev: Vec::new(), full: true, out: Vec::with_capacity(1 << 16) }
    }

    pub fn resize(&mut self, w: u16, h: u16) {
        if w != self.w || h != self.h {
            self.w = w;
            self.h = h;
            let n = w as usize * h as usize;
            self.cells = vec![BLANK; n];
            self.prev = vec![BLANK; n];
            self.full = true;
        }
    }

    pub fn clear(&mut self) {
        self.cells.fill(BLANK);
    }

    pub fn set(&mut self, x: u16, y: u16, ch: char, st: Style) {
        if x >= self.w || y >= self.h {
            return;
        }
        let mut ch = ch;
        let mut cw = char_width(ch);
        if cw == 0 {
            return;
        }
        if cw == 2 && x + 1 >= self.w {
            ch = ' ';
            cw = 1;
        }
        let i = y as usize * self.w as usize + x as usize;
        // Overwriting half of a wide character: blank the other half.
        if self.cells[i].ch == CONT && x > 0 {
            self.cells[i - 1].ch = ' ';
        }
        if char_width(self.cells[i].ch) == 2 && x + 1 < self.w {
            self.cells[i + 1].ch = ' ';
        }
        self.cells[i] = Cell { ch, st };
        if cw == 2 {
            if char_width(self.cells[i + 1].ch) == 2 && x + 2 < self.w {
                self.cells[i + 2].ch = ' ';
            }
            self.cells[i + 1] = Cell { ch: CONT, st };
        }
    }

    /// Draws `s` clipped to `max_w` columns. Returns the width used.
    pub fn text(&mut self, x: u16, y: u16, max_w: u16, s: &str, st: Style) -> u16 {
        let mut used = 0u16;
        for c in s.chars() {
            let cw = char_width(c) as u16;
            if cw == 0 {
                continue;
            }
            if used + cw > max_w {
                break;
            }
            self.set(x + used, y, c, st);
            used += cw;
        }
        used
    }

    /// Like `text`, but ends with '…' when `s` does not fit.
    pub fn text_ellipsis(&mut self, x: u16, y: u16, max_w: u16, s: &str, st: Style) -> u16 {
        if max_w == 0 {
            return 0;
        }
        if str_width(s) <= max_w as usize {
            return self.text(x, y, max_w, s, st);
        }
        let used = self.text(x, y, max_w - 1, s, st);
        self.set(x + used, y, '…', st);
        used + 1
    }

    pub fn fill(&mut self, r: Rect, st: Style) {
        for y in r.y..(r.y + r.h).min(self.h) {
            for x in r.x..(r.x + r.w).min(self.w) {
                self.set(x, y, ' ', st);
            }
        }
    }

    pub fn hline(&mut self, x: u16, y: u16, w: u16, ch: char, st: Style) {
        for i in 0..w {
            self.set(x + i, y, ch, st);
        }
    }

    /// Rounded frame with a title on the top edge and a footer on the bottom edge.
    pub fn frame(&mut self, r: Rect, title: &str, footer: &str, focused: bool) {
        if r.w < 2 || r.h < 2 {
            return;
        }
        let border = if focused { Style::fg(2).attr(BOLD) } else { Style::DEFAULT };
        let (x1, y1) = (r.x + r.w - 1, r.y + r.h - 1);
        self.set(r.x, r.y, '╭', border);
        self.set(x1, r.y, '╮', border);
        self.set(r.x, y1, '╰', border);
        self.set(x1, y1, '╯', border);
        self.hline(r.x + 1, r.y, r.w - 2, '─', border);
        self.hline(r.x + 1, y1, r.w - 2, '─', border);
        for y in r.y + 1..y1 {
            self.set(r.x, y, '│', border);
            self.set(x1, y, '│', border);
        }
        if !title.is_empty() && r.w > 4 {
            let st = if focused { Style::fg(2).attr(BOLD) } else { Style::DEFAULT.attr(BOLD) };
            self.text_ellipsis(r.x + 1, r.y, r.w - 3, title, st);
        }
        let fw = str_width(footer) as u16;
        if !footer.is_empty() && fw + 4 <= r.w {
            self.text(x1 - fw - 1, y1, fw, footer, border);
        }
    }

    /// Sends the changed cells (plus `extra` raw bytes) to the terminal.
    pub fn present(&mut self, extra: &[u8]) -> io::Result<()> {
        let out = &mut self.out;
        out.clear();
        if self.full {
            out.extend_from_slice(b"\x1b[0m\x1b[2J");
        }
        let w = self.w as usize;
        let mut cur_style: Option<Style> = None;
        let mut cur_pos: Option<(usize, usize)> = None;
        for (i, c) in self.cells.iter().enumerate() {
            if c.ch == CONT || (!self.full && *c == self.prev[i]) {
                continue;
            }
            let (x, y) = (i % w, i / w);
            if cur_pos != Some((x, y)) {
                let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
            }
            if cur_style != Some(c.st) {
                write_sgr(out, c.st);
                cur_style = Some(c.st);
            }
            let mut tmp = [0u8; 4];
            out.extend_from_slice(c.ch.encode_utf8(&mut tmp).as_bytes());
            cur_pos = Some((x + char_width(c.ch), y));
        }
        if cur_style.is_some() {
            out.extend_from_slice(b"\x1b[0m");
        }
        out.extend_from_slice(extra);
        self.prev.copy_from_slice(&self.cells);
        self.full = false;
        if !out.is_empty() {
            let mut stdout = io::stdout().lock();
            stdout.write_all(out)?;
            stdout.flush()?;
        }
        Ok(())
    }
}

fn write_sgr(out: &mut Vec<u8>, st: Style) {
    out.extend_from_slice(b"\x1b[0");
    for (bit, code) in [(BOLD, b";1"), (DIM, b";2"), (ITALIC, b";3"), (UNDERLINE, b";4"), (REVERSE, b";7")] {
        if st.attr & bit != 0 {
            out.extend_from_slice(code);
        }
    }
    if let Color::Idx(n) = st.fg {
        let _ = match n {
            0..=7 => write!(out, ";{}", 30 + n),
            8..=15 => write!(out, ";{}", 90 + n - 8),
            _ => write!(out, ";38;5;{n}"),
        };
    }
    if let Color::Idx(n) = st.bg {
        let _ = match n {
            0..=7 => write!(out, ";{}", 40 + n),
            8..=15 => write!(out, ";{}", 100 + n - 8),
            _ => write!(out, ";48;5;{n}"),
        };
    }
    out.push(b'm');
}

/// Terminal column width of a character (0, 1 or 2). A compact wcwidth.
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp < 0x300 {
        return 1;
    }
    if matches!(cp,
        0x0300..=0x036F | 0x0483..=0x0489 | 0x0591..=0x05BD | 0x0610..=0x061A
        | 0x064B..=0x065F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x200B..=0x200F
        | 0x202A..=0x202E | 0x2060..=0x2064 | 0x20D0..=0x20FF | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F | 0xFEFF | 0xE0100..=0xE01EF)
    {
        return 0;
    }
    if matches!(cp,
        0x1100..=0x115F | 0x231A..=0x231B | 0x2329..=0x232A | 0x23E9..=0x23EC
        | 0x23F0 | 0x23F3 | 0x25FD..=0x25FE | 0x2614..=0x2615 | 0x2648..=0x2653
        | 0x267F | 0x2693 | 0x26A1 | 0x26AA..=0x26AB | 0x26BD..=0x26BE
        | 0x26C4..=0x26C5 | 0x26CE | 0x26D4 | 0x26EA | 0x26F2..=0x26F3 | 0x26F5
        | 0x26FA | 0x26FD | 0x2705 | 0x270A..=0x270B | 0x2728 | 0x274C | 0x274E
        | 0x2753..=0x2755 | 0x2757 | 0x2795..=0x2797 | 0x27B0 | 0x27BF
        | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55 | 0x2E80..=0x303E | 0x3041..=0x33FF
        | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xA960..=0xA97F
        | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE10..=0xFE19 | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F004 | 0x1F0CF | 0x1F18E
        | 0x1F191..=0x1F19A | 0x1F200..=0x1F251 | 0x1F300..=0x1F64F
        | 0x1F680..=0x1F6FF | 0x1F7E0..=0x1F7EB | 0x1F90C..=0x1F9FF
        | 0x1FA70..=0x1FAFF | 0x20000..=0x3FFFD)
    {
        return 2;
    }
    1
}

pub fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths() {
        assert_eq!(str_width("abc"), 3);
        assert_eq!(str_width("日本"), 4);
        assert_eq!(str_width("e\u{301}"), 1);
    }

    #[test]
    fn wide_overwrite_keeps_grid_consistent() {
        let mut s = Screen::new();
        s.resize(4, 1);
        s.set(0, 0, '日', Style::DEFAULT);
        s.set(1, 0, 'x', Style::DEFAULT);
        assert_eq!(s.cells[0].ch, ' ');
        assert_eq!(s.cells[1].ch, 'x');
    }
}
