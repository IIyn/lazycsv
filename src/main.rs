//! lazycsv — a fast, dependency-free terminal CSV viewer.

mod app;
mod csv;
mod doc;
mod screen;
mod sys;
mod term;
mod tree;

use app::App;
use screen::Screen;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::exit;

const USAGE: &str = "\
lazycsv - fast terminal CSV viewer

USAGE:
    lazycsv [FILE | DIR]

Opens FILE directly (with its directory in the explorer),
or browses DIR (default: current directory).

Press ? inside the app for keybindings.";

fn main() {
    let arg = std::env::args_os().nth(1);
    if let Some(a) = arg.as_ref().and_then(|a| a.to_str()) {
        match a {
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            "-V" | "--version" => {
                println!("lazycsv {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ => {}
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (root, file) = match arg {
        None => (cwd, None),
        Some(a) => {
            let p = PathBuf::from(&a);
            let p = p.canonicalize().unwrap_or(p);
            if p.is_dir() {
                (p, None)
            } else if p.exists() {
                (p.parent().map(Path::to_path_buf).unwrap_or(cwd), Some(p))
            } else {
                eprintln!("lazycsv: {}: no such file or directory", p.display());
                exit(1);
            }
        }
    };
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("lazycsv: must be run in a terminal");
        exit(1);
    }

    let mut app = App::new(root, file);
    if let Err(e) = term::enter() {
        eprintln!("lazycsv: cannot initialize terminal: {e}");
        exit(1);
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        term::leave();
        default_hook(info);
    }));
    let result = run(&mut app);
    term::leave();
    if let Err(e) = result {
        eprintln!("lazycsv: {e}");
        exit(1);
    }
}

fn run(app: &mut App) -> io::Result<()> {
    let mut screen = Screen::new();
    let mut input = vec![0u8; 8192];
    let mut events = Vec::new();
    let mut last_release = std::time::Instant::now();
    loop {
        if last_release.elapsed().as_secs() >= 10 {
            app.release_memory();
            last_release = std::time::Instant::now();
        }
        let (w, h) = term::size();
        screen.resize(w, h);
        app.tick();
        // Sampled before drawing: if the work finishes mid-frame we still
        // come back quickly to draw the final state.
        let busy = app.busy();
        app.render(&mut screen);
        let extra = app.take_output();
        screen.present(&extra)?;
        if app.quit {
            return Ok(());
        }
        // Refresh often while indexing/searching so progress is visible;
        // otherwise sleep until input (or SIGWINCH interrupts poll).
        let timeout = if busy { 50 } else { 1000 };
        if term::poll_input(timeout) {
            let n = term::read_input(&mut input);
            events.clear();
            term::parse(&input[..n], &mut events);
            for &ev in &events {
                app.handle(ev);
                if app.quit {
                    break;
                }
            }
        }
    }
}
