//! Application state, key handling and rendering.

use crate::csv;
use crate::doc::{Doc, MAX_COL_W};
use crate::screen::{BOLD, DIM, REVERSE, Rect, Screen, Style, UNDERLINE, char_width, str_width};
use crate::term::{Event, Key};
use crate::tree::{self, Tree};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

// Palette (terminal colors, so the user's theme is respected).
const GREEN: u8 = 2;
const YELLOW: u8 = 3;
const BLUE: u8 = 4;
const CYAN: u8 = 6;
const WHITE: u8 = 15;
const RED: u8 = 1;
const SEL_BG: u8 = BLUE;
const ROW_BG: u8 = 236;
const SEL_BG_UNFOCUSED: u8 = 240;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tree,
    Table,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    Search,
    Goto,
}

struct RecordView {
    row: usize,
    scroll: usize,
    jump: bool,
    fields: Vec<(String, String)>,
}

enum Overlay {
    None,
    Help(usize),
    Record(RecordView),
}

struct SearchJob {
    rx: Receiver<Option<(usize, bool)>>,
    cancel: Arc<AtomicBool>,
    found: Option<(usize, bool)>,
}

pub struct App {
    pub quit: bool,
    focus: Focus,
    show_tree: bool,
    tree: Tree,
    doc: Option<Doc>,
    prompt: Option<(PromptKind, String)>,
    overlay: Overlay,
    msg: Option<(String, bool)>,
    search: Option<SearchJob>,
    needle: Option<String>,
    /// Raw bytes to send to the terminal after the next frame (OSC 52).
    output: Vec<u8>,
    // Layout of the last frame, used for paging and mouse hit-testing.
    tree_rect: Rect,
    table_rect: Rect,
    rows_y: u16,
    page: usize,
    tree_page: usize,
    col_hits: Vec<(u16, u16, usize)>,
    relayout: bool,
    // Scratch buffers reused across frames.
    fields: Vec<(usize, usize)>,
    buf: String,
}

impl App {
    pub fn new(root: PathBuf, file: Option<PathBuf>) -> App {
        let mut app = App {
            quit: false,
            focus: Focus::Tree,
            show_tree: true,
            tree: Tree::new(root),
            doc: None,
            prompt: None,
            overlay: Overlay::None,
            msg: None,
            search: None,
            needle: None,
            output: Vec::new(),
            tree_rect: Rect::default(),
            table_rect: Rect::default(),
            rows_y: 0,
            page: 20,
            tree_page: 20,
            col_hits: Vec::new(),
            relayout: false,
            fields: Vec::new(),
            buf: String::new(),
        };
        if let Some(f) = file {
            app.tree.select_path(&f);
            app.open_file(&f);
        }
        app
    }

    /// True while background work is running (the UI then refreshes faster).
    pub fn busy(&self) -> bool {
        self.search.is_some() || self.doc.as_ref().is_some_and(|d| !d.index.done())
    }

    /// Periodic housekeeping, see `Mmap::release`.
    pub fn release_memory(&self) {
        if let Some(d) = &self.doc {
            d.map().release();
        }
    }

    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    fn info(&mut self, s: impl Into<String>) {
        self.msg = Some((s.into(), false));
    }

    fn error(&mut self, s: impl Into<String>) {
        self.msg = Some((s.into(), true));
    }

    fn open_file(&mut self, path: &Path) {
        if self.doc.as_ref().is_some_and(|d| d.path == path) {
            self.focus = Focus::Table;
            return;
        }
        self.cancel_search();
        match Doc::open(path) {
            Ok(d) => {
                self.doc = Some(d);
                self.focus = Focus::Table;
                self.overlay = Overlay::None;
            }
            Err(e) => self.error(format!("Cannot open {}: {e}", path.display())),
        }
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    fn cancel_search(&mut self) {
        if let Some(j) = self.search.take() {
            j.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn start_search(&mut self, forward: bool) {
        let Some(needle) = self.needle.clone() else {
            self.error("No previous search");
            return;
        };
        self.cancel_search();
        let Some(doc) = &self.doc else { return };
        // Start after the current cell (or before it when going backwards),
        // so repeated n/N always land on a different cell.
        let from = if doc.read_row(doc.row, &mut self.fields) {
            self.fields.get(doc.col).map_or(0, |&(s, e)| if forward { e } else { s })
        } else {
            0
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (map, stop) = (doc.map(), cancel.clone());
        thread::spawn(move || {
            let _ = tx.send(csv::search(&map, &needle, from, forward, &stop));
        });
        self.search = Some(SearchJob { rx, cancel, found: None });
    }

    /// Polls background jobs. Called once per loop iteration.
    pub fn tick(&mut self) {
        let Some(mut job) = self.search.take() else { return };
        if job.found.is_none() {
            match job.rx.try_recv() {
                Ok(Some(f)) => job.found = Some(f),
                Ok(None) => {
                    let n = self.needle.clone().unwrap_or_default();
                    self.error(format!("Pattern not found: {n}"));
                    return;
                }
                Err(TryRecvError::Empty) => {
                    self.search = Some(job);
                    return;
                }
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let Some((off, wrapped)) = job.found else { return };
        let Some(doc) = self.doc.as_mut() else { return };
        if !doc.index.done() && off >= doc.index.scanned() {
            // Match found beyond what the indexer has reached: wait for it.
            self.search = Some(job);
            return;
        }
        let (row, col) = doc.locate(off, &mut self.fields);
        doc.ensure_cols(col + 1);
        doc.row = row;
        doc.col = col;
        doc.clamp();
        if wrapped {
            self.info("Search wrapped around");
        }
    }

    // -----------------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------------

    pub fn handle(&mut self, ev: Event) {
        match ev {
            Event::Key(k) => self.on_key(k),
            Event::Click(x, y) => self.on_click(x, y),
            Event::Scroll(x, y, d) => self.on_scroll(x, y, d as isize * 3),
        }
    }

    fn on_key(&mut self, k: Key) {
        self.msg = None;
        if self.prompt.is_some() {
            self.prompt_key(k);
            return;
        }
        match &mut self.overlay {
            Overlay::None => {}
            Overlay::Help(scroll) => {
                match k {
                    Key::Char('j') | Key::Down => *scroll += 1,
                    Key::Char('k') | Key::Up => *scroll = scroll.saturating_sub(1),
                    Key::Ctrl('c') => self.quit = true,
                    _ => self.overlay = Overlay::None,
                }
                return;
            }
            Overlay::Record(_) => {
                self.record_key(k);
                return;
            }
        }
        match k {
            Key::Char('q') | Key::Ctrl('c') => self.quit = true,
            Key::Char('?') => self.overlay = Overlay::Help(0),
            Key::Tab | Key::BackTab => self.set_focus(match self.focus {
                Focus::Tree => Focus::Table,
                Focus::Table => Focus::Tree,
            }),
            Key::Char('1') => self.set_focus(Focus::Tree),
            Key::Char('2') => self.set_focus(Focus::Table),
            Key::Char('e') => {
                self.show_tree = !self.show_tree;
                if !self.show_tree && self.doc.is_some() {
                    self.focus = Focus::Table;
                }
            }
            _ => match self.focus {
                Focus::Tree => self.tree_key(k),
                Focus::Table => self.table_key(k),
            },
        }
    }

    fn set_focus(&mut self, f: Focus) {
        match f {
            Focus::Tree => {
                self.show_tree = true;
                self.focus = Focus::Tree;
            }
            Focus::Table if self.doc.is_some() => self.focus = Focus::Table,
            Focus::Table => self.info("No file open: pick one in the explorer"),
        }
    }

    fn prompt_key(&mut self, k: Key) {
        let Some((kind, input)) = self.prompt.as_mut() else { return };
        match k {
            Key::Esc | Key::Ctrl('c') => self.prompt = None,
            Key::Backspace if input.is_empty() => self.prompt = None,
            Key::Backspace => {
                input.pop();
            }
            Key::Ctrl('u') => input.clear(),
            Key::Char(c) => input.push(c),
            Key::Enter => {
                let (kind, input) = (*kind, std::mem::take(input));
                self.prompt = None;
                match kind {
                    PromptKind::Search if !input.is_empty() => {
                        self.needle = Some(input);
                        self.start_search(true);
                    }
                    PromptKind::Search => {}
                    PromptKind::Goto => self.goto(&input),
                }
            }
            _ => {}
        }
    }

    fn goto(&mut self, input: &str) {
        let Some(doc) = self.doc.as_mut() else { return };
        let digits: String = input.chars().filter(|c| c.is_ascii_digit()).collect();
        match digits.parse::<usize>() {
            Ok(n) if n > 0 => {
                let total = doc.rows();
                doc.row = n - 1;
                doc.clamp();
                if n > total && !doc.index.done() {
                    let msg = format!("Only {} rows indexed so far", fmt_num(total));
                    self.info(msg);
                }
            }
            _ => self.error(format!("Invalid row number: {input}")),
        }
    }

    fn tree_key(&mut self, k: Key) {
        let page = self.tree_page.max(1) as isize;
        let t = &mut self.tree;
        match k {
            Key::Char('j') | Key::Down => t.move_by(1),
            Key::Char('k') | Key::Up => t.move_by(-1),
            Key::PageDown | Key::Ctrl('f') => t.move_by(page),
            Key::PageUp | Key::Ctrl('b') => t.move_by(-page),
            Key::Ctrl('d') => t.move_by(page / 2),
            Key::Ctrl('u') => t.move_by(-page / 2),
            Key::Char('g') | Key::Home => t.sel = 0,
            Key::Char('G') | Key::End => t.move_by(isize::MAX),
            Key::Char('h') | Key::Left => t.collapse_or_parent(),
            Key::Backspace | Key::Char('-') => t.go_up(),
            Key::Char('.') => t.toggle_hidden(),
            Key::Char('R') => t.reload(),
            Key::Char('c') => t.enter_dir(),
            Key::Char('l') | Key::Right => self.activate_tree(true),
            Key::Enter | Key::Char(' ') => self.activate_tree(false),
            _ => {}
        }
    }

    fn activate_tree(&mut self, expand_only: bool) {
        let Some(e) = self.tree.selected() else { return };
        if e.is_dir {
            if expand_only {
                self.tree.expand();
            } else {
                self.tree.toggle();
            }
        } else {
            let p = e.path.clone();
            self.open_file(&p);
        }
    }

    fn table_key(&mut self, k: Key) {
        let page = self.page.max(1) as isize;
        let Some(doc) = self.doc.as_mut() else {
            self.focus = Focus::Tree;
            return;
        };
        match k {
            Key::Char('j') | Key::Down => doc.move_row(1),
            Key::Char('k') | Key::Up => doc.move_row(-1),
            Key::Char('h') | Key::Left => doc.move_col(-1),
            Key::Char('l') | Key::Right => doc.move_col(1),
            Key::PageDown | Key::Ctrl('f') => doc.move_row(page),
            Key::PageUp | Key::Ctrl('b') => doc.move_row(-page),
            Key::Ctrl('d') => doc.move_row(page / 2),
            Key::Ctrl('u') => doc.move_row(-page / 2),
            Key::Char('g') | Key::Home => doc.row = 0,
            Key::Char('G') | Key::End => doc.move_row(isize::MAX),
            Key::Char('0') | Key::Char('^') => doc.col = 0,
            Key::Char('$') => doc.move_col(isize::MAX),
            Key::Char('w') => doc.move_col(5),
            Key::Char('b') => doc.move_col(-5),
            Key::Char('T') => doc.toggle_header(),
            Key::Char('D') => {
                doc.cycle_delimiter();
                let d = delim_name(doc.delim);
                self.info(format!("Delimiter: {d}"));
            }
            Key::Char('+') | Key::Char('>') => doc.resize_col(2),
            Key::Char('<') => doc.resize_col(-2),
            Key::Char('=') => doc.autofit(doc.row_off, self.page),
            Key::Char('/') => self.prompt = Some((PromptKind::Search, String::new())),
            Key::Char(':') => self.prompt = Some((PromptKind::Goto, String::new())),
            Key::Char('n') => self.start_search(true),
            Key::Char('N') => self.start_search(false),
            Key::Esc => {
                self.needle = None;
                self.cancel_search();
            }
            Key::Enter => {
                self.overlay = Overlay::Record(RecordView { row: usize::MAX, scroll: 0, jump: true, fields: Vec::new() })
            }
            Key::Char('y') => self.copy_cell(),
            Key::Char('Y') => self.copy_row(),
            _ => {}
        }
    }

    fn record_key(&mut self, k: Key) {
        let Overlay::Record(rv) = &mut self.overlay else { return };
        let Some(doc) = self.doc.as_mut() else {
            self.overlay = Overlay::None;
            return;
        };
        match k {
            Key::Esc | Key::Enter | Key::Char('q') => self.overlay = Overlay::None,
            Key::Ctrl('c') => self.quit = true,
            Key::Char('j') | Key::Down => rv.scroll += 1,
            Key::Char('k') | Key::Up => rv.scroll = rv.scroll.saturating_sub(1),
            Key::PageDown | Key::Ctrl('d') | Key::Ctrl('f') => rv.scroll += 10,
            Key::PageUp | Key::Ctrl('u') | Key::Ctrl('b') => rv.scroll = rv.scroll.saturating_sub(10),
            Key::Char('g') | Key::Home => rv.scroll = 0,
            Key::Char('G') | Key::End => rv.scroll = usize::MAX,
            Key::Char('h') | Key::Left => doc.move_row(-1),
            Key::Char('l') | Key::Right => doc.move_row(1),
            Key::Char('y') => self.copy_cell(),
            Key::Char('Y') => self.copy_row(),
            _ => {}
        }
    }

    fn on_click(&mut self, x: u16, y: u16) {
        if !matches!(self.overlay, Overlay::None) {
            self.overlay = Overlay::None;
            return;
        }
        let ti = self.tree_rect.inner();
        if self.show_tree && ti.contains(x, y) {
            self.focus = Focus::Tree;
            let i = self.tree.off + (y - ti.y) as usize;
            if i < self.tree.entries.len() {
                self.tree.sel = i;
                self.activate_tree(false);
            }
        } else if self.table_rect.contains(x, y)
            && let Some(doc) = self.doc.as_mut()
        {
            self.focus = Focus::Table;
            if y >= self.rows_y {
                let row = doc.row_off + (y - self.rows_y) as usize;
                if row < doc.rows() {
                    doc.row = row;
                }
            }
            if let Some(&(_, _, c)) = self.col_hits.iter().find(|h| x >= h.0 && x < h.1) {
                doc.col = c;
            }
        }
    }

    fn on_scroll(&mut self, x: u16, y: u16, d: isize) {
        match &mut self.overlay {
            Overlay::Help(s) => *s = s.saturating_add_signed(d),
            Overlay::Record(rv) => rv.scroll = rv.scroll.saturating_add_signed(d),
            Overlay::None => {
                if self.show_tree && self.tree_rect.contains(x, y) {
                    self.tree.move_by(d);
                } else if let Some(doc) = self.doc.as_mut() {
                    doc.move_row(d);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Clipboard (OSC 52: works over SSH and needs no external tool)
    // -----------------------------------------------------------------------

    fn copy(&mut self, text: &str, what: &str) {
        self.output.extend_from_slice(b"\x1b]52;c;");
        self.output.extend_from_slice(base64(text.as_bytes()).as_bytes());
        self.output.push(0x07);
        self.info(format!("Copied {what} to clipboard ({} chars)", text.chars().count()));
    }

    fn copy_cell(&mut self) {
        let Some(doc) = &self.doc else { return };
        if !doc.read_row(doc.row, &mut self.fields) {
            return;
        }
        let mut s = String::new();
        if let Some(&(a, b)) = self.fields.get(doc.col) {
            csv::decode_field(&doc.data()[a..b], 1 << 24, true, &mut s);
        }
        self.copy(&s, "cell");
    }

    fn copy_row(&mut self) {
        let Some(doc) = &self.doc else { return };
        let Some(start) = doc.record_start(doc.row) else { return };
        let data = doc.data();
        let end = csv::next_record(data, start);
        let s = String::from_utf8_lossy(&data[start..end]).trim_end_matches(['\r', '\n']).to_string();
        self.copy(&s, "row");
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    pub fn render(&mut self, s: &mut Screen) {
        s.clear();
        let (w, h) = (s.w, s.h);
        if w < 30 || h < 6 {
            s.text(0, 0, w, "Terminal too small", Style::fg(RED));
            return;
        }
        let main_h = h - 1;
        let tree_w = if self.show_tree { (w / 4).clamp(24, 44).min(w / 2) } else { 0 };
        self.tree_rect = Rect { x: 0, y: 0, w: tree_w, h: main_h };
        self.table_rect = Rect { x: tree_w, y: 0, w: w - tree_w, h: main_h };
        if self.show_tree {
            self.draw_tree(s);
        }
        self.draw_table(s);
        self.draw_status(s, h - 1);
        match self.overlay {
            Overlay::None => {}
            Overlay::Help(_) => self.draw_help(s),
            Overlay::Record(_) => self.draw_record(s),
        }
        // Column widths changed while drawing: redraw with the new layout.
        if std::mem::take(&mut self.relayout) {
            self.render(s);
        }
    }

    fn draw_tree(&mut self, s: &mut Screen) {
        let r = self.tree_rect;
        let focused = self.focus == Focus::Tree;
        let inner = r.inner();
        let root = home_relative(&self.tree.root);
        let title = format!(" [1] {} ", tail_fit(&root, r.w.saturating_sub(10) as usize));
        let t = &mut self.tree;
        let footer = if t.entries.is_empty() { String::new() } else { format!(" {} of {} ", t.sel + 1, t.entries.len()) };
        s.frame(r, &title, &footer, focused);
        self.tree_page = inner.h as usize;
        t.scroll_into_view(inner.h as usize);
        if t.entries.is_empty() {
            s.text(inner.x + 1, inner.y, inner.w.saturating_sub(1), "(empty)", Style::DEFAULT.attr(DIM));
            return;
        }
        let open = self.doc.as_ref().map(|d| d.path.as_path());
        for (i, e) in t.entries.iter().enumerate().skip(t.off).take(inner.h as usize) {
            let y = inner.y + (i - t.off) as u16;
            let selected = i == t.sel;
            let bg = match (selected, focused) {
                (true, true) => Some(SEL_BG),
                (true, false) => Some(ROW_BG),
                _ => None,
            };
            let paint = |st: Style| match bg {
                Some(SEL_BG) => Style { fg: crate::screen::Color::Idx(WHITE), ..st }.with_bg(SEL_BG),
                Some(b) => st.with_bg(b),
                None => st,
            };
            if bg.is_some() {
                s.fill(Rect { x: inner.x, y, w: inner.w, h: 1 }, paint(Style::DEFAULT));
            }
            let mut x = inner.x + 1 + e.depth * 2;
            let right = inner.right().saturating_sub(1);
            if x >= right {
                continue;
            }
            let (icon, name_st) = if e.is_dir {
                (if e.expanded { "▾ " } else { "▸ " }, Style::fg(BLUE).attr(BOLD))
            } else if open == Some(e.path.as_path()) {
                ("● ", Style::fg(GREEN).attr(BOLD))
            } else if tree::is_tabular(&e.name) {
                ("  ", Style::fg(GREEN))
            } else {
                ("  ", Style::DEFAULT.attr(DIM))
            };
            x += s.text(x, y, right - x, icon, paint(name_st));
            let size = if e.is_dir { String::new() } else { human_size(e.size) };
            let sw = size.len() as u16;
            let name_w = if right.saturating_sub(x) > sw + 2 { right - x - sw - 1 } else { right.saturating_sub(x) };
            s.text_ellipsis(x, y, name_w, &e.name, paint(name_st));
            if !size.is_empty() && right.saturating_sub(x) > sw + 2 {
                s.text(right - sw, y, sw, &size, paint(Style::DEFAULT.attr(DIM)));
            }
        }
    }

    fn draw_table(&mut self, s: &mut Screen) {
        let r = self.table_rect;
        let focused = self.focus == Focus::Table;
        let inner = r.inner();
        self.col_hits.clear();
        let Some(doc) = self.doc.as_mut() else {
            s.frame(r, " [2] Preview ", "", focused);
            draw_welcome(s, inner);
            return;
        };
        doc.clamp();
        let total = doc.rows();
        let done = doc.index.done();
        let title = format!(" [2] {} ", doc.name);
        let footer = if total == 0 {
            String::new()
        } else {
            format!(" {} of {}{} ", fmt_num(doc.row + 1), fmt_num(total), if done { "" } else { "+" })
        };
        s.frame(r, &title, &footer, focused);
        if inner.h < 3 || inner.w < 10 {
            return;
        }

        // Vertical scroll.
        let vis = (inner.h - 2) as usize;
        self.page = vis;
        self.rows_y = inner.y + 2;
        if doc.row < doc.row_off {
            doc.row_off = doc.row;
        } else if doc.row >= doc.row_off + vis {
            doc.row_off = doc.row + 1 - vis;
        }

        // Horizontal scroll: keep the current column fully visible.
        let digits = fmt_len(total.max(doc.row_off + vis)).max(2) as u16;
        let left = inner.x + digits + 1;
        let right = inner.right();
        let avail = right.saturating_sub(left) as usize;
        if doc.col < doc.col_off {
            doc.col_off = doc.col;
        }
        while doc.col_off < doc.col
            && doc.widths[doc.col_off..=doc.col].iter().map(|&w| w as usize + 3).sum::<usize>() > avail
        {
            doc.col_off += 1;
        }
        let mut x = left;
        for c in doc.col_off..doc.ncols() {
            if x >= right {
                break;
            }
            let slot = (doc.widths[c] + 2).min(right - x);
            self.col_hits.push((x, x + slot, c));
            x = x.saturating_add(doc.widths[c] + 3);
        }

        let dim = Style::DEFAULT.attr(DIM);
        // Header.
        let hy = inner.y;
        s.text(inner.x, hy, digits, &format!("{:>w$}", "#", w = digits as usize), dim);
        s.set(inner.x + digits, hy, '│', dim);
        for &(x, end, c) in &self.col_hits {
            let name = &doc.headers[c];
            let st = if c == doc.col { Style::fg(YELLOW).attr(BOLD | UNDERLINE) } else { Style::fg(YELLOW).attr(BOLD) };
            let w = str_width(name);
            draw_cell(s, x, end, hy, doc.widths[c], name, w, w > doc.widths[c] as usize, doc.numeric[c], st);
            separator(s, x, doc.widths[c], right, hy, dim);
        }
        // Rule under the header.
        s.hline(inner.x, hy + 1, inner.w, '─', dim);
        s.set(inner.x + digits, hy + 1, '┼', dim);
        for &(x, _, c) in &self.col_hits {
            let sx = x + doc.widths[c] + 2;
            if sx < right {
                s.set(sx, hy + 1, '┼', dim);
            }
        }

        if total == 0 {
            let m = if done { "(no rows)" } else { "Indexing…" };
            s.text(left + 1, self.rows_y, avail as u16, m, dim);
            return;
        }

        // Rows: jump to the first visible record once, then parse sequentially.
        let needle = self.needle.as_deref().filter(|n| !n.is_empty());
        let ci = needle.is_some_and(|n| !n.bytes().any(|b| b.is_ascii_uppercase()));
        let data = doc.data();
        let mut pos = doc.record_start(doc.row_off);
        let mut max_fields = 0;
        let mut grow = Vec::new();
        for i in 0..vis {
            let row = doc.row_off + i;
            let Some(p) = pos.filter(|_| row < total) else { break };
            pos = Some(csv::parse_record(data, p, doc.delim, &mut self.fields));
            max_fields = max_fields.max(self.fields.len());
            let y = self.rows_y + i as u16;
            let is_cur = row == doc.row;
            let base = if is_cur { Style::DEFAULT.with_bg(ROW_BG) } else { Style::DEFAULT };
            if is_cur {
                s.fill(Rect { x: inner.x, y, w: inner.w, h: 1 }, base);
            }
            let num_st = if is_cur { Style::fg(YELLOW).with_bg(ROW_BG).attr(BOLD) } else { dim };
            s.text(inner.x, y, digits, &format!("{:>w$}", row + 1, w = digits as usize), num_st);
            let sep = if is_cur { dim.with_bg(ROW_BG) } else { dim };
            s.set(inner.x + digits, y, '│', sep);
            for &(x, end, c) in &self.col_hits {
                let raw = self.fields.get(c).map_or(&[][..], |&(a, b)| &data[a..b]);
                let cw = doc.widths[c];
                let (tw, trunc) = csv::decode_field(raw, cw as usize, false, &mut self.buf);
                if trunc && doc.numeric[c] && cw < MAX_COL_W {
                    // A truncated number is misleading: widen the column instead.
                    let (full, _) = csv::decode_field(raw, MAX_COL_W as usize, false, &mut self.buf);
                    grow.push((c, full as u16));
                }
                let is_sel = is_cur && c == doc.col;
                let hit = needle.is_some_and(|n| contains(&self.buf, n, ci));
                let st = if is_sel {
                    if focused { Style::fg(WHITE).with_bg(SEL_BG).attr(BOLD) } else { Style::DEFAULT.with_bg(SEL_BG_UNFOCUSED) }
                } else if hit {
                    Style::fg(0).with_bg(YELLOW)
                } else {
                    base
                };
                if is_sel || hit {
                    s.fill(Rect { x, y, w: end - x, h: 1 }, st);
                }
                draw_cell(s, x, end, y, cw, &self.buf, tw, trunc, doc.numeric[c], st);
                separator(s, x, cw, right, y, sep);
            }
        }
        doc.ensure_cols(max_fields);
        for (c, w) in grow {
            doc.widths[c] = doc.widths[c].max(w);
            self.relayout = true;
        }
    }

    fn draw_status(&mut self, s: &mut Screen, y: u16) {
        let w = s.w;
        if let Some((kind, input)) = &self.prompt {
            let label = match kind {
                PromptKind::Search => "Search: /",
                PromptKind::Goto => "Go to row: ",
            };
            let x = s.text(0, y, w, label, Style::fg(CYAN).attr(BOLD));
            let x = x + s.text(x, y, w - x, input, Style::DEFAULT);
            s.set(x, y, ' ', Style::DEFAULT.attr(REVERSE));
            let hint = "enter: confirm | esc: cancel";
            let hw = hint.len() as u16;
            if x + hw + 4 < w {
                s.text(w - hw, y, hw, hint, Style::DEFAULT.attr(DIM));
            }
            return;
        }

        let mut right = String::new();
        if let Some(doc) = &self.doc {
            if self.search.is_some() {
                right.push_str("searching… │ ");
            }
            if !doc.index.done() {
                let pct = doc.index.scanned() as f64 * 100.0 / doc.size().max(1) as f64;
                right.push_str(&format!("indexing {pct:.0}% │ "));
            }
            right.push_str(&format!(
                "{} × {} │ {} │ {} │ {}",
                fmt_num(doc.rows()),
                doc.ncols(),
                delim_name(doc.delim),
                if doc.has_header { "header" } else { "no header" },
                human_size(doc.size() as u64)
            ));
        }
        let rw = str_width(&right) as u16;
        let left_max = if rw + 10 < w { w - rw - 2 } else { w };
        if rw + 10 < w {
            s.text(w - rw, y, rw, &right, Style::fg(CYAN));
        }

        if let Some((m, err)) = &self.msg {
            s.text_ellipsis(0, y, left_max, m, Style::fg(if *err { RED } else { GREEN }).attr(BOLD));
            return;
        }
        let hints: &[(&str, &str)] = match (self.focus, self.doc.is_some()) {
            (Focus::Tree, _) => &[
                ("Open", "enter"),
                ("Collapse", "h"),
                ("Up dir", "⌫"),
                ("Hidden", "."),
                ("Switch", "tab"),
                ("Help", "?"),
                ("Quit", "q"),
            ],
            (Focus::Table, _) => &[
                ("Search", "/"),
                ("Next", "n/N"),
                ("Go to", ":"),
                ("Record", "enter"),
                ("Copy", "y"),
                ("Header", "T"),
                ("Delim", "D"),
                ("Help", "?"),
            ],
        };
        let mut x = 0u16;
        for (i, (label, key)) in hints.iter().enumerate() {
            let part_w = (label.len() + 2 + str_width(key) + if i > 0 { 3 } else { 0 }) as u16;
            if x + part_w > left_max {
                break;
            }
            if i > 0 {
                x += s.text(x, y, 3, " | ", Style::DEFAULT.attr(DIM));
            }
            x += s.text(x, y, left_max - x, label, Style::DEFAULT);
            x += s.text(x, y, left_max - x, ": ", Style::DEFAULT);
            x += s.text(x, y, left_max - x, key, Style::fg(BLUE).attr(BOLD));
        }
    }

    fn draw_help(&mut self, s: &mut Screen) {
        let Overlay::Help(scroll) = &mut self.overlay else { return };
        let w = 62.min(s.w - 2);
        let h = (HELP.len() as u16 + 2).min(s.h - 2);
        let r = centered(s, w, h);
        s.fill(r, Style::DEFAULT);
        s.frame(r, " Keybindings ", " esc: close ", true);
        let inner = r.inner();
        *scroll = (*scroll).min(HELP.len().saturating_sub(inner.h as usize));
        for (i, (key, desc)) in HELP.iter().skip(*scroll).take(inner.h as usize).enumerate() {
            let y = inner.y + i as u16;
            if desc.is_empty() {
                s.text(inner.x + 1, y, inner.w - 1, key, Style::fg(GREEN).attr(BOLD));
            } else {
                s.text(inner.x + 3, y, 16, key, Style::fg(BLUE).attr(BOLD));
                s.text(inner.x + 20, y, inner.w.saturating_sub(21), desc, Style::DEFAULT);
            }
        }
    }

    fn draw_record(&mut self, s: &mut Screen) {
        let Overlay::Record(rv) = &mut self.overlay else { return };
        let Some(doc) = &self.doc else { return };
        if rv.row != doc.row {
            rv.row = doc.row;
            rv.fields.clear();
            if doc.read_row(doc.row, &mut self.fields) {
                let data = doc.data();
                for c in 0..doc.ncols().max(self.fields.len()) {
                    let mut v = String::new();
                    if let Some(&(a, b)) = self.fields.get(c) {
                        csv::decode_field(&data[a..b], 1 << 20, true, &mut v);
                    }
                    let name = doc.headers.get(c).cloned().unwrap_or_else(|| crate::doc::col_name(c));
                    rv.fields.push((name, v));
                }
            }
        }

        let w = (s.w * 4 / 5).max(40).min(s.w - 2);
        let h = (s.h * 4 / 5).max(8).min(s.h - 2);
        let r = centered(s, w, h);
        s.fill(r, Style::DEFAULT);
        let title = format!(" Row {} of {} ", fmt_num(doc.row + 1), fmt_num(doc.rows()));
        s.frame(r, &title, " j/k scroll │ h/l prev/next row │ y copy │ esc close ", true);
        let inner = r.inner();
        let label_w = rv.fields.iter().map(|(n, _)| str_width(n)).max().unwrap_or(1).clamp(4, 28) as u16;
        let val_x = inner.x + 1 + label_w + 3;
        let val_w = inner.right().saturating_sub(val_x + 1) as usize;
        if val_w < 4 {
            return;
        }

        // Flatten fields into wrapped lines: (column, first line of field, text).
        let mut lines: Vec<(usize, bool, String)> = Vec::new();
        let mut focus_line = 0;
        for (c, (_, v)) in rv.fields.iter().enumerate() {
            if c == doc.col {
                focus_line = lines.len();
            }
            for (i, l) in wrap(v, val_w, 2000).into_iter().enumerate() {
                lines.push((c, i == 0, l));
            }
        }
        let vis = inner.h as usize;
        if rv.jump {
            rv.jump = false;
            rv.scroll = focus_line.saturating_sub(vis / 3);
        }
        rv.scroll = rv.scroll.min(lines.len().saturating_sub(vis));
        for (i, (c, first, text)) in lines.iter().skip(rv.scroll).take(vis).enumerate() {
            let y = inner.y + i as u16;
            let cur = *c == doc.col;
            let bg = if cur { Style::DEFAULT.with_bg(ROW_BG) } else { Style::DEFAULT };
            if cur {
                s.fill(Rect { x: inner.x, y, w: inner.w, h: 1 }, bg);
            }
            if *first {
                let name = &rv.fields[*c].0;
                s.text_ellipsis(inner.x + 1, y, label_w, name, bg.with_fg(YELLOW).attr(BOLD));
            }
            s.set(val_x - 2, y, '│', bg.attr(DIM));
            if text.is_empty() && *first {
                s.text(val_x, y, val_w as u16, "∅", bg.attr(DIM));
            } else {
                s.text(val_x, y, val_w as u16, text, bg);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Drawing helpers
// ---------------------------------------------------------------------------

/// Draws one table cell into the slot `[x, end)` (1 column of padding each side).
#[allow(clippy::too_many_arguments)]
fn draw_cell(s: &mut Screen, x: u16, end: u16, y: u16, width: u16, text: &str, tw: usize, trunc: bool, right: bool, st: Style) {
    let avail = width.min(end.saturating_sub(x + 1));
    if avail == 0 {
        return;
    }
    if trunc && avail == width {
        s.text_ellipsis(x + 1, y, avail, text, st);
        if str_width(text) <= avail as usize {
            s.set(x + avail, y, '…', st);
        }
    } else {
        let off = if right && tw < avail as usize { avail - tw as u16 } else { 0 };
        s.text(x + 1 + off, y, avail - off, text, st);
    }
}

fn separator(s: &mut Screen, x: u16, width: u16, right: u16, y: u16, st: Style) {
    let sx = x + width + 2;
    if sx < right {
        s.set(sx, y, '│', st);
    }
}

fn centered(s: &Screen, w: u16, h: u16) -> Rect {
    Rect { x: (s.w - w) / 2, y: (s.h - h) / 2, w, h }
}

const LOGO: [&str; 6] = [
    " _                                 ",
    "| | __ _ _____   _  ___ _____   __ ",
    "| |/ _` |_  / | | |/ __/ __\\ \\ / / ",
    "| | (_| |/ /| |_| | (__\\__ \\\\ V /  ",
    "|_|\\__,_/___|\\__, |\\___|___/ \\_/   ",
    "             |___/                 ",
];

fn draw_welcome(s: &mut Screen, r: Rect) {
    let lines = [
        "",
        "Pick a file in the explorer and press enter.",
        "",
        "tab  switch panel      /  search      ?  help",
        "e    toggle explorer   :  go to row   q  quit",
    ];
    let total = (LOGO.len() + lines.len()) as u16;
    if r.h < total || r.w < 40 {
        s.text(r.x + 1, r.y, r.w.saturating_sub(1), "Pick a file in the explorer and press enter.", Style::DEFAULT);
        return;
    }
    let mut y = r.y + (r.h - total) / 2;
    for l in LOGO {
        let x = r.x + (r.w.saturating_sub(l.len() as u16)) / 2;
        s.text(x, y, r.w, l, Style::fg(GREEN).attr(BOLD));
        y += 1;
    }
    for l in lines {
        let x = r.x + (r.w.saturating_sub(str_width(l) as u16)) / 2;
        s.text(x, y, r.w, l, Style::DEFAULT.attr(DIM));
        y += 1;
    }
}

const HELP: &[(&str, &str)] = &[
    ("Global", ""),
    ("tab / 1 / 2", "switch panel"),
    ("e", "show / hide the explorer"),
    ("?", "this help"),
    ("q, ctrl-c", "quit"),
    ("", ""),
    ("Explorer", ""),
    ("j k ↑ ↓", "move"),
    ("enter / space", "open file / toggle directory"),
    ("l →  /  h ←", "expand / collapse (or go to parent)"),
    ("backspace / -", "set parent directory as root"),
    ("c", "set selected directory as root"),
    (".", "show hidden files"),
    ("R", "refresh"),
    ("", ""),
    ("Table", ""),
    ("h j k l, arrows", "move cell"),
    ("ctrl-d / ctrl-u", "half page down / up"),
    ("pgdn / pgup", "page down / up"),
    ("g / G", "first / last row"),
    ("0 / $", "first / last column"),
    ("w / b", "5 columns right / left"),
    ("/", "search (smart case)"),
    ("n / N", "next / previous match"),
    ("esc", "clear search highlight"),
    (":", "go to row number"),
    ("enter", "show full record"),
    ("y / Y", "copy cell / row (OSC 52)"),
    ("+ < =", "widen / narrow / autofit column"),
    ("T", "toggle header row"),
    ("D", "cycle delimiter , ; tab |"),
    ("mouse", "click to select, wheel to scroll"),
];

// ---------------------------------------------------------------------------
// Small utilities
// ---------------------------------------------------------------------------

fn contains(hay: &str, needle: &str, ci: bool) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.len() > h.len() {
        return false;
    }
    if ci { h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n)) } else { h.windows(n.len()).any(|w| w == n) }
}

/// Word-wraps `s` to `width` columns, honoring embedded newlines.
fn wrap(s: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for para in s.split('\n') {
        let mut line = String::new();
        let mut lw = 0;
        for word in para.split_inclusive(' ') {
            let ww = str_width(word);
            if lw + ww > width && lw > 0 {
                lines.push(std::mem::take(&mut line));
                lw = 0;
            }
            if ww > width {
                for c in word.chars() {
                    let cw = char_width(c);
                    if lw + cw > width {
                        lines.push(std::mem::take(&mut line));
                        lw = 0;
                    }
                    line.push(c);
                    lw += cw;
                }
            } else {
                line.push_str(word);
                lw += ww;
            }
        }
        lines.push(line);
        if lines.len() >= max_lines {
            lines.truncate(max_lines);
            lines.push("…".into());
            break;
        }
    }
    lines
}

fn fmt_len(n: usize) -> usize {
    fmt_num(n).len()
}

/// 1234567 -> "1,234,567"
fn fmt_num(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n}B") } else if v < 10.0 { format!("{v:.1}{}", UNITS[u]) } else { format!("{v:.0}{}", UNITS[u]) }
}

fn delim_name(d: u8) -> &'static str {
    match d {
        b',' => "comma",
        b';' => "semicolon",
        b'\t' => "tab",
        b'|' => "pipe",
        _ => "?",
    }
}

fn home_relative(p: &Path) -> String {
    let s = p.display().to_string();
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s,
    }
}

/// Keeps the end of `s` when it is wider than `max` ("…/dir/sub").
fn tail_fit(s: &str, max: usize) -> String {
    if str_width(s) <= max || max < 2 {
        return s.to_string();
    }
    let mut w = 0;
    let mut start = s.len();
    for (i, c) in s.char_indices().rev() {
        w += char_width(c);
        if w > max - 1 {
            break;
        }
        start = i;
    }
    format!("…{}", &s[start..])
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utils() {
        assert_eq!(base64(b"hello"), "aGVsbG8=");
        assert_eq!(base64(b"hi!"), "aGkh");
        assert_eq!(fmt_num(1234567), "1,234,567");
        assert_eq!(fmt_num(12), "12");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(wrap("aa bb cc", 4, 100), vec!["aa ", "bb ", "cc"]);
        assert_eq!(tail_fit("/very/long/path", 8), "…ng/path");
    }
}
