//! Talking to the terminal from inside a command substitution.
//!
//! The shell hook captures our stdout, so anything the user is meant to read or
//! answer goes straight to /dev/tty. If there is no controlling terminal — a
//! script, a cron job, a CI runner — there is nobody to ask, and callers treat
//! that as "no".

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

use crate::config::Color;

pub struct Tty {
    file: File,
    pub color: bool,
}

impl Tty {
    pub fn open(preference: Color) -> Option<Tty> {
        // Opening /dev/tty fails with ENXIO when the process has no controlling
        // terminal, which is exactly how we detect CI and cron. Checking
        // isatty(0) would be wrong: stdin is a pipe in the case we care about.
        let file = File::options()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        let color = match preference {
            Color::Always => true,
            Color::Never => false,
            Color::Auto => {
                std::env::var_os("NO_COLOR").is_none()
                    && std::env::var("TERM").is_ok_and(|term| term != "dumb")
            }
        };
        Some(Tty { file, color })
    }

    pub fn say(&mut self, text: &str) {
        let _ = self.file.write_all(text.as_bytes());
        let _ = self.file.flush();
    }

    pub fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    /// One keypress, no Enter. ISIG stays on, so Ctrl-C still raises SIGINT and
    /// the guard below puts the terminal back before we die.
    fn key(&mut self) -> Option<char> {
        let _raw = Raw::enter(&self.file)?;

        let mut byte = [0u8; 1];
        let first = match self.file.read(&mut byte) {
            Ok(1) => byte[0],
            _ => return None,
        };
        if first == 0x1b {
            // An arrow key is three bytes. Swallow the tail, or the shell reads
            // "[A" as typing the moment we hand the terminal back.
            Raw::poll_briefly(&self.file);
            let _ = self.file.read(&mut [0u8; 16]);
        }
        Some(first as char)
    }

    fn line(&mut self) -> Option<char> {
        let mut text = String::new();
        BufReader::new(&self.file).read_line(&mut text).ok()?;
        text.trim().chars().next()
    }

    pub fn ask(&mut self, question: &str, default_yes: bool) -> bool {
        let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
        self.say(&format!("{question} {} ", self.paint("2", hint)));

        let answer = self.key().or_else(|| self.line());
        let yes = match answer {
            Some('y' | 'Y') => true,
            Some('\r' | '\n') => default_yes,
            _ => false,
        };
        self.say(if yes { "y\n" } else { "n\n" });
        yes
    }

    /// A numbered menu for when the top match is not clearly the right one.
    /// Returns the chosen index, or `None` if the user backed out.
    pub fn choose(&mut self, header: &str, options: &[String]) -> Option<usize> {
        self.say(&format!("{header}\n"));
        for (i, option) in options.iter().enumerate() {
            self.say(&format!(
                "  {}  {option}\n",
                self.paint("1", &(i + 1).to_string())
            ));
        }
        self.say(&format!(
            "{} ",
            self.paint("2", "pick 1-9, or n to cancel:")
        ));

        let key = self.key().or_else(|| self.line())?;
        self.say("\n");
        let picked = key.to_digit(10)?.checked_sub(1)? as usize;
        (picked < options.len()).then_some(picked)
    }
}

struct Saved {
    fd: i32,
    state: libc::termios,
}

/// Set by `Raw::enter` so a signal arriving mid-prompt can put the terminal
/// back. Leaving somebody's shell in a non-echoing state is the worst failure
/// this program could have.
static PENDING: AtomicPtr<Saved> = AtomicPtr::new(ptr::null_mut());

struct Raw;

impl Raw {
    fn enter(tty: &File) -> Option<Raw> {
        let fd = tty.as_raw_fd();
        let mut state = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut state) } != 0 {
            return None;
        }

        let saved = Box::into_raw(Box::new(Saved { fd, state }));
        if PENDING
            .compare_exchange(ptr::null_mut(), saved, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            drop(unsafe { Box::from_raw(saved) });
            return None;
        }
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
            unsafe { libc::signal(signal, rescue as *const () as libc::sighandler_t) };
        }

        // Character at a time with no echo, but ISIG left alone so Ctrl-C keeps
        // working the way the user expects at a prompt.
        let mut raw = state;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            restore();
            return None;
        }
        Some(Raw)
    }

    /// Wait up to a tenth of a second for more bytes, then stop.
    fn poll_briefly(tty: &File) {
        let fd = tty.as_raw_fd();
        let mut state = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut state) } == 0 {
            state.c_cc[libc::VMIN] = 0;
            state.c_cc[libc::VTIME] = 1;
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &state) };
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    let saved = PENDING.swap(ptr::null_mut(), Ordering::AcqRel);
    if saved.is_null() {
        return;
    }
    let saved = unsafe { Box::from_raw(saved) };
    unsafe { libc::tcsetattr(saved.fd, libc::TCSANOW, &saved.state) };
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        unsafe { libc::signal(signal, libc::SIG_DFL) };
    }
}

extern "C" fn rescue(signal: libc::c_int) {
    let saved = PENDING.swap(ptr::null_mut(), Ordering::AcqRel);
    if !saved.is_null() {
        // Deliberately not freeing: the allocator is not safe to call from a
        // signal handler, and this process is about to die anyway.
        unsafe { libc::tcsetattr((*saved).fd, libc::TCSANOW, &(*saved).state) };
    }
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}
