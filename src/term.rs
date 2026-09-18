//! Raw terminal handling and input decoding (replaces crossterm/termion).

use crate::sys;
use std::ffi::c_int;
use std::io::{self, Write};
use std::sync::OnceLock;

static ORIGINAL: OnceLock<sys::Termios> = OnceLock::new();

// alt screen, hide cursor, no autowrap, mouse (button events + SGR encoding)
const ENTER: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[?1000h\x1b[?1006h\x1b[2J";
const LEAVE: &[u8] = b"\x1b[?1006l\x1b[?1000l\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1049l";

extern "C" fn on_winch(_: c_int) {
    // Nothing to do: the signal only exists to interrupt `poll`, after which
    // the main loop re-reads the terminal size.
}

pub fn enter() -> io::Result<()> {
    let mut t = sys::Termios::zeroed();
    unsafe {
        if sys::tcgetattr(0, &mut t) != 0 {
            return Err(io::Error::last_os_error());
        }
        let _ = ORIGINAL.set(t);
        let mut raw = t;
        sys::cfmakeraw(&mut raw);
        if sys::tcsetattr(0, sys::TCSANOW, &raw) != 0 {
            return Err(io::Error::last_os_error());
        }
        sys::signal(sys::SIGWINCH, on_winch);
    }
    let mut out = io::stdout().lock();
    out.write_all(ENTER)?;
    out.flush()
}

pub fn leave() {
    if let Some(t) = ORIGINAL.get() {
        unsafe { sys::tcsetattr(0, sys::TCSANOW, t) };
    }
    let mut out = io::stdout().lock();
    let _ = out.write_all(LEAVE);
    let _ = out.flush();
}

pub fn size() -> (u16, u16) {
    let mut ws = sys::Winsize::default();
    let ok = unsafe { sys::ioctl(1, sys::TIOCGWINSZ, &mut ws as *mut sys::Winsize) } == 0;
    if ok && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

/// Waits up to `timeout_ms` for input. Returns false on timeout or signal.
pub fn poll_input(timeout_ms: i32) -> bool {
    let mut fd = sys::PollFd { fd: 0, events: sys::POLLIN, revents: 0 };
    let r = unsafe { sys::poll(&mut fd, 1, timeout_ms) };
    r > 0 && fd.revents & sys::POLLIN != 0
}

/// Reads whatever is available on stdin (unbuffered, after a successful poll).
pub fn read_input(buf: &mut [u8]) -> usize {
    loop {
        let n = unsafe { sys::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return n as usize;
        }
        if io::Error::last_os_error().raw_os_error() != Some(sys::EINTR) {
            return 0;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Ctrl(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    Click(u16, u16),
    Scroll(u16, u16, i8),
}

/// Decodes a chunk of raw terminal input into events.
pub fn parse(buf: &[u8], out: &mut Vec<Event>) {
    let mut i = 0;
    while i < buf.len() {
        let b = buf[i];
        match b {
            0x1b => {
                if i + 1 >= buf.len() {
                    out.push(Event::Key(Key::Esc));
                    i += 1;
                } else if buf[i + 1] == b'[' {
                    i = parse_csi(buf, i + 2, out);
                } else if buf[i + 1] == b'O' && i + 2 < buf.len() {
                    if let Some(k) = final_key(buf[i + 2]) {
                        out.push(Event::Key(k));
                    }
                    i += 3;
                } else {
                    // ESC followed by something else (Alt+key): report a bare Esc.
                    out.push(Event::Key(Key::Esc));
                    i += 1;
                }
            }
            b'\r' | b'\n' => {
                out.push(Event::Key(Key::Enter));
                i += 1;
            }
            b'\t' => {
                out.push(Event::Key(Key::Tab));
                i += 1;
            }
            0x7f | 0x08 => {
                out.push(Event::Key(Key::Backspace));
                i += 1;
            }
            1..=26 => {
                out.push(Event::Key(Key::Ctrl((b'a' + b - 1) as char)));
                i += 1;
            }
            0 | 28..=31 => i += 1,
            _ => {
                let len = match b {
                    0xf0..=0xf7 => 4,
                    0xe0..=0xef => 3,
                    0xc0..=0xdf => 2,
                    _ => 1,
                };
                let end = (i + len).min(buf.len());
                if let Some(c) = std::str::from_utf8(&buf[i..end]).ok().and_then(|s| s.chars().next()) {
                    out.push(Event::Key(Key::Char(c)));
                }
                i = end;
            }
        }
    }
}

fn final_key(b: u8) -> Option<Key> {
    Some(match b {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        b'Z' => Key::BackTab,
        _ => return None,
    })
}

/// Parses a CSI sequence starting after `ESC [`. Returns the next index.
fn parse_csi(buf: &[u8], start: usize, out: &mut Vec<Event>) -> usize {
    let mut j = start;
    while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
        j += 1;
    }
    if j >= buf.len() {
        return buf.len();
    }
    let params = &buf[start..j];
    let fin = buf[j];
    if params.first() == Some(&b'<') {
        // SGR mouse: ESC [ < b ; x ; y (M|m)
        let mut nums = params[1..]
            .split(|&c| c == b';')
            .map(|p| std::str::from_utf8(p).ok().and_then(|s| s.parse::<u16>().ok()).unwrap_or(0));
        let (cb, x, y) = (nums.next().unwrap_or(0), nums.next().unwrap_or(1), nums.next().unwrap_or(1));
        let (x, y) = (x.saturating_sub(1), y.saturating_sub(1));
        if cb & 64 != 0 {
            out.push(Event::Scroll(x, y, if cb & 1 == 0 { -1 } else { 1 }));
        } else if fin == b'M' && cb & 3 == 0 && cb & 32 == 0 {
            out.push(Event::Click(x, y));
        }
    } else if fin == b'~' {
        let first = params.split(|&c| c == b';').next().unwrap_or(b"");
        let k = match first {
            b"1" | b"7" => Some(Key::Home),
            b"4" | b"8" => Some(Key::End),
            b"3" => Some(Key::Delete),
            b"5" => Some(Key::PageUp),
            b"6" => Some(Key::PageDown),
            _ => None,
        };
        if let Some(k) = k {
            out.push(Event::Key(k));
        }
    } else if let Some(k) = final_key(fin) {
        out.push(Event::Key(k));
    }
    j + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_keys_and_mouse() {
        let mut ev = Vec::new();
        parse(b"j\x1b[A\x1b[6~\x1b[<0;5;3M\x1b[<65;1;1M\x03\xc3\xa9", &mut ev);
        assert_eq!(
            ev,
            vec![
                Event::Key(Key::Char('j')),
                Event::Key(Key::Up),
                Event::Key(Key::PageDown),
                Event::Click(4, 2),
                Event::Scroll(0, 0, 1),
                Event::Key(Key::Ctrl('c')),
                Event::Key(Key::Char('é')),
            ]
        );
    }
}
