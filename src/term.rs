//! The hook captures our stdout, so anything a human reads goes to /dev/tty.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};

pub struct Tty {
    file: File,
    pub color: bool,
    borrowed: bool,
    disturbed: bool,
}

impl Tty {
    pub fn open() -> Option<Tty> {
        Tty::open_as(false)
    }

    pub fn open_as(borrowed: bool) -> Option<Tty> {
        // Opening /dev/tty fails with ENXIO when there is no controlling terminal,
        // which is how CI and cron are detected. isatty(0) would be wrong: stdin
        // is a pipe in the case we care about.
        let file = File::options()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        // A background job can open a terminal but owns none: asking there stops
        // the job and leaves a [Y/n] nobody can answer without fg.
        if unsafe { libc::tcgetpgrp(file.as_raw_fd()) } != unsafe { libc::getpgrp() } {
            return None;
        }
        let color = std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").is_ok_and(|term| term != "dumb");
        Some(Tty {
            file,
            color,
            borrowed,
            disturbed: false,
        })
    }

    pub fn say(&mut self, text: &str) {
        if self.borrowed && !self.disturbed {
            // Ask on the alternate screen, the way fzf does: the terminal restores the
            // primary one byte for byte. Save-and-restore does not work, because the
            // question moves the cursor to a row the editor never hears about.
            // Published first: a signal landing between the two would otherwise
            // leave the alternate screen up with nothing recorded to undo it.
            ALTERNATE.store(self.file.as_raw_fd(), Ordering::Release);
            let _ = self.file.write_all(b"\x1b[?1049h\x1b[H");
            self.disturbed = true;
        }
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

    fn key(&mut self) -> Option<char> {
        let _raw = Raw::enter(&self.file)?;

        let mut byte = [0u8; 1];
        let first = match self.file.read(&mut byte) {
            Ok(1) => byte[0],
            _ => return None,
        };
        if first == 0x1b {
            // An arrow key is three bytes; the shell would read the tail as typing.
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
        self.ask_with(question, default_yes, &[]) == 'y'
    }

    pub fn ask_with(&mut self, question: &str, default_yes: bool, extra: &[char]) -> char {
        let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
        self.say(&format!("{question} {} ", self.paint("2", hint)));

        let answer = self.key().or_else(|| self.line());
        let picked = match answer.map(|key| key.to_ascii_lowercase()) {
            Some(key) if extra.contains(&key) => key,
            Some('y') => 'y',
            Some('\r' | '\n') if default_yes => 'y',
            _ => 'n',
        };
        self.say(&format!("{picked}\n"));
        picked
    }

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

impl Drop for Tty {
    fn drop(&mut self) {
        if self.disturbed {
            ALTERNATE.store(-1, Ordering::Release);
            let _ = self.file.write_all(b"\x1b[?1049l");
            let _ = self.file.flush();
        }
    }
}

struct Saved {
    fd: i32,
    state: libc::termios,
}

/// So a signal mid-prompt can put the terminal back. Leaving a shell
/// non-echoing is the worst failure this could have.
static PENDING: AtomicPtr<Saved> = AtomicPtr::new(ptr::null_mut());
static ALTERNATE: AtomicI32 = AtomicI32::new(-1);

/// SIGABRT included: the release profile aborts on panic rather than
/// unwinding, so `Raw::drop` is not what puts the terminal back.
const RESCUED: [libc::c_int; 5] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGABRT,
];

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
        for signal in RESCUED {
            unsafe { libc::signal(signal, rescue as *const () as libc::sighandler_t) };
        }

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
    for signal in RESCUED {
        unsafe { libc::signal(signal, libc::SIG_DFL) };
    }
}

extern "C" fn rescue(signal: libc::c_int) {
    let alternate = ALTERNATE.swap(-1, Ordering::AcqRel);
    if alternate >= 0 {
        const LEAVE: &[u8] = b"\x1b[?1049l";
        unsafe { libc::write(alternate, LEAVE.as_ptr().cast(), LEAVE.len()) };
    }
    let saved = PENDING.swap(ptr::null_mut(), Ordering::AcqRel);
    if !saved.is_null() {
        // Not freed: the allocator is not signal-safe and we are about to die.
        unsafe { libc::tcsetattr((*saved).fd, libc::TCSANOW, &(*saved).state) };
    }
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}
