//! Keep typed-ahead secrets off the screen.
//!
//! Between prompts the terminal is in its normal mode, which echoes what is typed: a key
//! pasted while setup is still checking the previous answer would be printed in clear and
//! stay in the scrollback. Echo stays off for the whole of setup (the prompts draw their own
//! input), and input typed before a secret prompt appears is discarded.

use std::os::fd::AsRawFd;

pub struct NoEcho {
    saved: Option<libc::termios>,
}

impl NoEcho {
    pub fn new() -> Self {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: tcgetattr/tcsetattr only read and write the termios struct we own.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::isatty(fd) != 1 || libc::tcgetattr(fd, &mut t) != 0 {
                return Self { saved: None };
            }
            let saved = t;
            t.c_lflag &= !(libc::ECHO | libc::ECHONL);
            libc::tcsetattr(fd, libc::TCSANOW, &t);
            Self { saved: Some(saved) }
        }
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        if let Some(t) = self.saved {
            // SAFETY: restores the attributes read in `new`.
            unsafe {
                libc::tcsetattr(std::io::stdin().as_raw_fd(), libc::TCSANOW, &t);
            }
        }
    }
}

/// Drop whatever was typed before the prompt that is about to read a secret.
pub fn discard_typeahead() {
    // SAFETY: tcflush on stdin has no memory effects.
    unsafe {
        libc::tcflush(std::io::stdin().as_raw_fd(), libc::TCIFLUSH);
    }
}
