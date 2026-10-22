//! Terminal handling: color, the full-screen watch display and raw keys.

use std::ffi::c_void;
use std::io::Write as _;
use std::os::raw::c_int;

use crate::ffi::{
    ioctl, isatty, poll, read, tcgetattr, tcsetattr, PollFd, Termios, WinSize, ECHO, ICANON,
    POLLIN, TCSANOW, TIOCGWINSZ, VMIN, VTIME,
};

pub fn color_enabled() -> bool {
    let tty = unsafe { isatty(1) } == 1;
    tty && std::env::var_os("NO_COLOR").map_or(true, |v| v.is_empty())
}

pub fn emit_raw(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// Watch mode on a terminal: alternate screen, no cursor, no line wrap (long
/// rows are clipped), and keys read unbuffered without echo when stdin is a
/// terminal too. Dropping it restores all of that.
pub struct Screen {
    saved: Option<Termios>,
}

impl Screen {
    pub fn enter(keys: bool) -> Screen {
        let mut saved = None;
        let mut t = Termios::default();
        if keys && unsafe { tcgetattr(0, &mut t) } == 0 {
            let mut raw = t;
            raw.c_lflag &= !(ICANON | ECHO);
            raw.c_cc[VMIN] = 0;
            raw.c_cc[VTIME] = 0;
            if unsafe { tcsetattr(0, TCSANOW, &raw) } == 0 {
                saved = Some(t);
            }
        }
        emit_raw("\x1b[?1049h\x1b[?25l\x1b[?7l");
        Screen { saved }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if let Some(t) = &self.saved {
            unsafe { tcsetattr(0, TCSANOW, t) };
        }
        emit_raw("\x1b[?7h\x1b[?25h\x1b[?1049l");
    }
}

pub fn term_rows() -> Option<usize> {
    let mut ws = WinSize::default();
    let rc = unsafe { ioctl(1, TIOCGWINSZ, &mut ws as *mut WinSize) };
    (rc == 0 && ws.ws_row > 0).then_some(ws.ws_row as usize)
}

pub fn term_cols() -> Option<usize> {
    let mut ws = WinSize::default();
    let rc = unsafe { ioctl(1, TIOCGWINSZ, &mut ws as *mut WinSize) };
    (rc == 0 && ws.ws_col > 0).then_some(ws.ws_col as usize)
}

/// The next key pressed within `timeout_ms`, if any.
pub fn wait_key(timeout_ms: c_int) -> Option<u8> {
    let mut fd = PollFd {
        fd: 0,
        events: POLLIN,
        revents: 0,
    };
    if unsafe { poll(&mut fd, 1, timeout_ms) } <= 0 || fd.revents & POLLIN == 0 {
        return None;
    }
    let mut key = 0u8;
    let n = unsafe { read(0, &mut key as *mut u8 as *mut c_void, 1) };
    (n == 1).then_some(key)
}

/// A key press, arrow keys decoded from their escape sequences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(u8),
    Up,
    Down,
    Left,
    Right,
    Esc,
}

/// The key that starts with byte `first`, reading the rest of an escape
/// sequence from `next`. Sequences other than arrows read as Esc.
fn decode(first: u8, next: &mut dyn FnMut() -> Option<u8>) -> Key {
    if first != 0x1b {
        return Key::Char(first);
    }
    if !matches!(next(), Some(b'[' | b'O')) {
        return Key::Esc;
    }
    match next() {
        Some(b'A') => Key::Up,
        Some(b'B') => Key::Down,
        Some(b'C') => Key::Right,
        Some(b'D') => Key::Left,
        _ => Key::Esc,
    }
}

/// The next key pressed within `timeout_ms`, if any.
pub fn read_key(timeout_ms: c_int) -> Option<Key> {
    let first = wait_key(timeout_ms)?;
    Some(decode(first, &mut || wait_key(10)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_keys() {
        let d = |bytes: &[u8]| {
            let mut rest = bytes[1..].iter().copied();
            decode(bytes[0], &mut || rest.next())
        };
        assert_eq!(d(b"a"), Key::Char(b'a'));
        assert_eq!(d(b"\x1b[A"), Key::Up);
        assert_eq!(d(b"\x1bOB"), Key::Down);
        assert_eq!(d(b"\x1b[C"), Key::Right);
        assert_eq!(d(b"\x1b[D"), Key::Left);
        assert_eq!(d(b"\x1b"), Key::Esc);
        assert_eq!(d(b"\x1b[Z"), Key::Esc);
    }
}
