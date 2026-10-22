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
