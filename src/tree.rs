//! File explorer: a flattened, lazily expanded directory tree.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub depth: u16,
    pub is_dir: bool,
    pub expanded: bool,
    pub size: u64,
}

pub struct Tree {
    pub root: PathBuf,
    pub entries: Vec<Entry>,
    pub sel: usize,
    pub off: usize,
    pub show_hidden: bool,
}

pub fn is_tabular(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".csv", ".tsv", ".tab", ".psv"].iter().any(|e| lower.ends_with(e))
}

fn list(dir: &Path, depth: u16, hidden: bool) -> Vec<Entry> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<Entry> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !hidden && name.starts_with('.') {
                return None;
            }
            let path = e.path();
            // fs::metadata follows symlinks, so linked directories are browsable.
            let meta = fs::metadata(&path).ok();
            let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
            let size = meta.map_or(0, |m| m.len());
            Some(Entry { path, name, depth, is_dir, expanded: false, size })
        })
        .collect();
    v.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase()));
    v
}

impl Tree {
    pub fn new(root: PathBuf) -> Tree {
        let mut t = Tree { root, entries: Vec::new(), sel: 0, off: 0, show_hidden: false };
        t.reload();
        t
    }

    /// Re-reads the filesystem, keeping expanded directories and the selection.
    pub fn reload(&mut self) {
        let expanded: HashSet<PathBuf> =
            self.entries.iter().filter(|e| e.expanded).map(|e| e.path.clone()).collect();
        let selected = self.selected().map(|e| e.path.clone());
        let mut out = Vec::new();
        self.build(&self.root.clone(), 0, &expanded, &mut out);
        self.entries = out;
        self.sel = 0;
        if let Some(p) = selected {
            self.select_path(&p);
        }
    }

    fn build(&self, dir: &Path, depth: u16, expanded: &HashSet<PathBuf>, out: &mut Vec<Entry>) {
        for mut e in list(dir, depth, self.show_hidden) {
            let open = e.is_dir && expanded.contains(&e.path);
            e.expanded = open;
            let path = e.path.clone();
            out.push(e);
            if open {
                self.build(&path, depth + 1, expanded, out);
            }
        }
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.sel)
    }

    pub fn select_path(&mut self, p: &Path) {
        if let Some(i) = self.entries.iter().position(|e| e.path == p) {
            self.sel = i;
        }
    }

    pub fn move_by(&mut self, d: isize) {
        let max = self.entries.len().saturating_sub(1);
        self.sel = self.sel.saturating_add_signed(d).min(max);
    }

    pub fn expand(&mut self) {
        let i = self.sel;
        let Some(e) = self.entries.get_mut(i) else { return };
        if !e.is_dir || e.expanded {
            return;
        }
        e.expanded = true;
        let children = list(&e.path, e.depth + 1, self.show_hidden);
        self.entries.splice(i + 1..i + 1, children);
    }

    pub fn collapse(&mut self) {
        let i = self.sel;
        let Some(e) = self.entries.get_mut(i) else { return };
        if !e.expanded {
            return;
        }
        e.expanded = false;
        let depth = e.depth;
        let end = self.entries[i + 1..].iter().position(|c| c.depth <= depth).map_or(self.entries.len(), |p| i + 1 + p);
        self.entries.drain(i + 1..end);
    }

    pub fn toggle(&mut self) {
        match self.selected() {
            Some(e) if e.expanded => self.collapse(),
            Some(_) => self.expand(),
            None => {}
        }
    }

    /// Collapses the selected directory, or jumps to the parent entry.
    pub fn collapse_or_parent(&mut self) {
        let Some(e) = self.selected() else { return };
        if e.expanded {
            self.collapse();
        } else if e.depth > 0 {
            let depth = e.depth;
            if let Some(p) = self.entries[..self.sel].iter().rposition(|c| c.depth < depth) {
                self.sel = p;
            }
        }
    }

    /// Makes the parent directory the new root.
    pub fn go_up(&mut self) {
        let Some(parent) = self.root.parent().map(Path::to_path_buf) else { return };
        let old = std::mem::replace(&mut self.root, parent);
        self.entries.clear();
        self.reload();
        self.select_path(&old);
    }

    /// Makes the selected directory the new root.
    pub fn enter_dir(&mut self) {
        if let Some(e) = self.selected().filter(|e| e.is_dir) {
            self.root = e.path.clone();
            self.entries.clear();
            self.reload();
        }
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.reload();
    }

    pub fn scroll_into_view(&mut self, height: usize) {
        if self.sel < self.off {
            self.off = self.sel;
        } else if height > 0 && self.sel >= self.off + height {
            self.off = self.sel + 1 - height;
        }
        self.off = self.off.min(self.entries.len().saturating_sub(height.max(1)));
        if self.sel < self.off {
            self.off = self.sel;
        }
    }
}
