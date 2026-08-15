//! PATH, the init snippets, and history files. The only module that runs
//! another program or reads a file it did not write.

use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::store::Shell;

pub fn init_script(shell: Shell) -> &'static str {
    match shell {
        Shell::Zsh => include_str!("init/zcomplete.zsh"),
        Shell::Bash => include_str!("init/zcomplete.bash"),
        Shell::Fish => include_str!("init/zcomplete.fish"),
    }
}

fn path_dirs() -> &'static [Vec<u8>] {
    static DIRS: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs: Vec<Vec<u8>> = Vec::new();
        let Some(path) = std::env::var_os("PATH") else {
            return dirs;
        };
        for dir in path.as_bytes().split(|byte| *byte == b':') {
            // A repeated entry can never win over its first appearance, and
            // scanning it again is the single most expensive thing here.
            if !dir.is_empty() && !dirs.iter().any(|seen| seen == dir) {
                dirs.push(dir.to_vec());
            }
        }
        dirs
    })
}

pub fn on_path(name: &str) -> bool {
    if name.is_empty() || name.contains('/') {
        return false;
    }
    let mut buf = [0u8; 4096];
    path_dirs()
        .iter()
        .any(|dir| is_program(&mut buf, dir, name.as_bytes()))
}

fn is_program(buf: &mut [u8; 4096], dir: &[u8], name: &[u8]) -> bool {
    // A C string ends at the first NUL, so `ls\0junk` would be stat'd as `ls`
    // and reported installed. `fs::metadata`, which this replaced, refused it.
    let end = dir.len() + 1 + name.len();
    if end >= buf.len() || name.contains(&0) {
        return false;
    }
    buf[..dir.len()].copy_from_slice(dir);
    buf[dir.len()] = b'/';
    buf[dir.len() + 1..end].copy_from_slice(name);
    buf[end] = 0;

    let mut info = unsafe { std::mem::zeroed::<libc::stat>() };
    let found = unsafe { libc::stat(buf.as_ptr().cast(), &mut info) } == 0;
    found && info.st_mode & libc::S_IFMT == libc::S_IFREG && info.st_mode & 0o111 != 0
}

/// Sorted and deduplicated already: that was done once, when the listing was
/// built, rather than on every miss for a set that has not changed since.
pub fn path_names(wanted: impl Fn(&str) -> bool) -> impl Iterator<Item = &'static str> {
    listing()
        .split(|byte| *byte == 0)
        .filter_map(|name| std::str::from_utf8(name).ok())
        .filter(move |name| !name.is_empty() && wanted(name))
}

/// Every name on PATH, NUL separated, from the cache when the directories are
/// as they were and from a sweep otherwise. Reading every directory is by far
/// the slowest thing zcomplete does, and it is all wasted when nothing changed.
fn listing() -> &'static [u8] {
    static NAMES: OnceLock<Vec<u8>> = OnceLock::new();
    NAMES.get_or_init(|| {
        // Named for the directory list and validated by their metadata. Named
        // for both and every install would orphan a file: a venv, a nix-shell
        // and the login shell each want their own, and each wants it reused.
        let cache = crate::store::db_path().with_file_name(format!("path.{:016x}", path_key()));
        let (stamp, settled) = path_stamp();
        if let Some(names) = fs::read(&cache).ok().and_then(|blob| {
            // The stamp's own length comes last. A directory deleted off PATH
            // since the sweep makes the new stamp shorter, and comparing it as
            // a bare suffix would then be reading part of the old one.
            let end = blob.len().checked_sub(8)?;
            let mut len = [0u8; 8];
            len.copy_from_slice(blob.get(end..)?);
            let split = end.checked_sub(usize::try_from(u64::from_le_bytes(len)).ok()?)?;
            (blob.get(split..end)? == stamp).then(|| blob[..split].to_vec())
        }) {
            return names;
        }

        let (mut names, whole) = sweep();
        names = ordered(names);
        // Not written when a directory would not open, or one EMFILE would be
        // cached as the truth until something happened to change an mtime; nor
        // when a directory changed this second, because on a filesystem that
        // keeps mtimes to the second an install landing right after this sweep
        // would leave a stamp we cannot tell from the one we just took.
        // Unlocked on purpose: two shells that scan at once write the same
        // bytes, and rename decides which copy survives.
        if whole && settled && names.len() <= MAX_LISTING {
            let mut blob = names.clone();
            blob.extend_from_slice(&stamp);
            blob.extend_from_slice(&(stamp.len() as u64).to_le_bytes());
            let temp = cache.with_extension(format!("tmp.{}", std::process::id()));
            let written = fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(cache.parent().unwrap_or(&cache))
                .and_then(|()| {
                    fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .mode(0o600)
                        .open(&temp)?
                        .write_all(&blob)
                })
                .and_then(|()| fs::rename(&temp, &cache));
            if written.is_err() {
                let _ = fs::remove_file(&temp);
            }
            reap_listings(&cache);
        }
        names
    })
}

const MAX_LISTING: usize = 4 << 20;

/// How many PATH listings are worth keeping. Each distinct PATH gets its own,
/// and a venv, a nix-shell, a container and the login shell are all one PATH
/// each, so a handful is a working set rather than a limit anyone reaches.
const KEEP_LISTINGS: usize = 4;

/// The listings for PATHs nobody is using any more. One file per distinct PATH
/// with nothing to remove them was the whole of this tool's disk growth: every
/// venv activated once left a copy of its PATH's contents behind for good.
/// Costs one `read_dir` on the rare path that just swept, and never touches the
/// file it was called about.
fn reap_listings(keep: &Path) {
    let Some(dir) = keep.parent() else {
        return;
    };
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("path.") && !name.contains(".tmp."))
        })
        .filter(|entry| entry.path() != keep)
        .filter_map(|entry| {
            let at = entry.metadata().and_then(|meta| meta.modified()).ok()?;
            Some((at, entry.path()))
        })
        .collect();
    if found.len() < KEEP_LISTINGS {
        return;
    }
    // Newest first, and the one just written is not in here at all, so this
    // keeps `KEEP_LISTINGS - 1` others beside it.
    found.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    for (_, stale) in found.drain(KEEP_LISTINGS - 1..) {
        let _ = fs::remove_file(stale);
    }
}

fn path_key() -> u64 {
    let mut key: u64 = 0xcbf2_9ce4_8422_2325;
    for dir in path_dirs() {
        for byte in dir.iter().chain(b"\0") {
            key ^= u64::from(*byte);
            key = key.wrapping_mul(0x100_0000_01b3);
        }
    }
    key
}

/// What the cache is checked against: one `stat` per directory rather than one
/// `readdir` per entry, so it costs the same on a PATH with ten thousand
/// programs as on one with ten. Installing anything moves its directory's mtime,
/// and swapping a profile symlink moves the inode, which is what makes this
/// sound where mtimes are normalised to a constant.
///
/// The second return says every directory is old enough that a change to one
/// would show: a stamp taken in the same second as the change it is meant to
/// notice cannot be trusted to differ from the next one.
fn path_stamp() -> (Vec<u8>, bool) {
    let mut stamp = Vec::with_capacity(path_dirs().len() * 32);
    let mut settled = true;
    let now = crate::store::now() as i64;
    let mut path = [0u8; 4096];
    for dir in path_dirs() {
        if dir.len() >= path.len() {
            continue;
        }
        path[..dir.len()].copy_from_slice(dir);
        path[dir.len()] = 0;
        let mut info = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::stat(path.as_ptr().cast(), &mut info) } != 0 {
            continue;
        }
        settled &= now.saturating_sub(info.st_mtime) >= 1;
        stamp.extend_from_slice(&info.st_mtime.to_le_bytes());
        stamp.extend_from_slice(&info.st_mtime_nsec.to_le_bytes());
        stamp.extend_from_slice(&info.st_size.to_le_bytes());
        stamp.extend_from_slice(&info.st_ino.to_le_bytes());
    }
    (stamp, settled)
}

/// Sorted and deduplicated once, so the two directories that both hold `python3`
/// cost one name here rather than a sort on every question asked of it.
fn ordered(blob: Vec<u8>) -> Vec<u8> {
    let mut names: Vec<&[u8]> = blob
        .split(|byte| *byte == 0)
        .filter(|n| !n.is_empty())
        .collect();
    names.sort_unstable();
    names.dedup();
    let mut out = Vec::with_capacity(blob.len());
    for name in names {
        out.extend_from_slice(name);
        out.push(0);
    }
    out
}

/// Where errno lives, on the platforms that name it. `None` elsewhere, which
/// costs only the old behaviour of reading a failed `readdir` as the end.
fn errno_slot() -> Option<*mut libc::c_int> {
    #[cfg(any(target_os = "macos", target_os = "ios", target_vendor = "apple"))]
    {
        Some(unsafe { libc::__error() })
    }
    #[cfg(target_os = "linux")]
    {
        Some(unsafe { libc::__errno_location() })
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
    {
        None
    }
}

fn sweep() -> (Vec<u8>, bool) {
    let mut names = Vec::new();
    let mut whole = true;
    let mut path = [0u8; 4096];
    for dir in path_dirs() {
        if dir.len() >= path.len() {
            whole = false;
            continue;
        }
        path[..dir.len()].copy_from_slice(dir);
        path[dir.len()] = 0;
        let handle = unsafe { libc::opendir(path.as_ptr().cast()) };
        if handle.is_null() {
            // A directory on PATH that is not there at all is normal and says
            // nothing; one that exists and would not open is a failure.
            whole &= std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound;
            continue;
        }
        loop {
            // Zeroed first: `readdir` returns NULL both at the end of a
            // directory and on a failure, and errno is the only thing that
            // tells them apart. A listing truncated by an error and then cached
            // as complete would hide every program past it.
            if let Some(errno) = errno_slot() {
                unsafe { *errno = 0 };
            }
            let entry = unsafe { libc::readdir(handle) };
            if entry.is_null() {
                if let Some(errno) = errno_slot() {
                    whole &= unsafe { *errno } == 0;
                }
                break;
            }
            // Through a raw pointer, never `&*entry`: a record is only `d_reclen`
            // bytes, so a reference would claim bytes past the last one in a buffer.
            let name =
                unsafe { std::ffi::CStr::from_ptr(std::ptr::addr_of!((*entry).d_name).cast()) };
            let name = name.to_bytes();
            if name != b"." && name != b".." {
                names.extend_from_slice(name);
                names.push(0);
            }
        }
        unsafe { libc::closedir(handle) };
    }
    (names, whole)
}

pub fn which(name: &str) -> Option<PathBuf> {
    let mut buf = [0u8; 4096];
    path_dirs()
        .iter()
        .find(|dir| is_program(&mut buf, dir, name.as_bytes()))
        .map(|dir| Path::new(std::ffi::OsStr::from_bytes(dir)).join(name))
}

pub fn advertised_verbs(name: &str) -> Vec<String> {
    let Some(text) = help_output(name) else {
        return Vec::new();
    };
    let mut found: Vec<String> = Vec::new();
    for line in text.lines() {
        for verb in verbs_in(line, name) {
            if !found.iter().any(|seen| seen == &verb) {
                found.push(verb);
            }
        }
        if found.len() >= 400 {
            break;
        }
    }
    found
}

/// Four layouts cover essentially every tool:
///
///     git   "   clone      Clone a repository into a new directory"
///     gh    "  auth:          Authenticate gh and git with GitHub"
///     brew  "  brew install FORMULA|CASK..."
///     npm   "    access, adduser, audit, bugs, cache, ci,"
///
/// All four need the line indented and the word bare, which keeps prose out.
fn verbs_in(line: &str, parent: &str) -> Vec<String> {
    let body = line.trim_end();
    let text = body.trim_start();
    let indent = body.len() - text.len();
    if !(2..=8).contains(&indent) || text.is_empty() {
        return Vec::new();
    }
    let mut words = text.split_whitespace();
    let (Some(first), second) = (words.next(), words.next()) else {
        return Vec::new();
    };

    if first == parent {
        return second
            .filter(|w| bare_word(w))
            .map(str::to_owned)
            .into_iter()
            .collect();
    }
    if text.contains(',') {
        let items: Vec<&str> = text
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if items.iter().all(|item| bare_word(item)) {
            return items.into_iter().map(str::to_owned).collect();
        }
    }
    let word = first.strip_suffix(':').unwrap_or(first);
    if text[first.len()..].starts_with("  ") && bare_word(word) {
        return vec![word.to_owned()];
    }
    Vec::new()
}

fn bare_word(word: &str) -> bool {
    (2..=21).contains(&word.len())
        && word.starts_with(|c: char| c.is_ascii_lowercase())
        && word
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Runs something and gives up on it. `keep_errors` merges the two streams
/// through one file offset, for tools that split their help across both.
fn capture(
    program: &Path,
    args: &[&str],
    keep_errors: bool,
    patience: std::time::Duration,
) -> Option<String> {
    use std::process::{Command, Stdio};

    // Beside the database, not in /tmp: the pid is guessable, and `File::create`
    // on a symlink someone else planted there writes through it.
    let sink = crate::store::db_path().with_file_name(format!("out.{}", std::process::id()));
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(sink.parent()?)
        .ok()?;
    // A crash leaves one behind, and pids come round again.
    let _ = fs::remove_file(&sink);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&sink)
        .ok()?;
    let errors = match keep_errors {
        true => Stdio::from(file.try_clone().ok()?),
        false => Stdio::null(),
    };
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(errors)
        .spawn()
        .ok()?;

    let deadline = std::time::Instant::now() + patience;
    let finished = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Err(_) => break false,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    };

    let text = finished
        .then(|| fs::read(&sink).ok())
        .flatten()
        .map(|bytes| String::from_utf8_lossy(&bytes[..bytes.len().min(256 * 1024)]).into_owned());
    let _ = fs::remove_file(&sink);
    text
}

fn help_output(name: &str) -> Option<String> {
    let path = which(name)?;
    capture(
        &path,
        &["--help"],
        true,
        std::time::Duration::from_millis(500),
    )
}

/// The aliases, functions and builtins the shell has right now. History alone
/// cannot see them: `gs` is not on PATH, so an import drops it, and the word
/// you type twenty times a day never enters the database.
///
/// Interactive, because that is the only kind of shell that reads the file your
/// aliases live in. Errors are dropped: an interactive bash with no terminal
/// complains about job control before it does anything useful.
pub fn defined_words(shell: Shell) -> Vec<String> {
    let script = match shell {
        Shell::Zsh => "print -rl -- ${(k)aliases} ${(k)functions} ${(k)builtins}",
        Shell::Bash => "compgen -a; compgen -A function; compgen -b",
        // A fish alias is a function, and `functions -n` is one comma-separated line.
        Shell::Fish => "functions -n | string split ', '; builtin -n",
    };
    // fish reads its config for `-c` as well, and its `-i` wants a terminal.
    let mode = match shell {
        Shell::Fish => "-c",
        _ => "-ic",
    };
    let Some(path) = which(shell.name()) else {
        return Vec::new();
    };
    capture(
        &path,
        &[mode, script],
        false,
        std::time::Duration::from_secs(5),
    )
    .map(|text| {
        text.lines()
            .map(str::trim)
            .filter(|word| !word.is_empty())
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

pub struct Recalled {
    pub line: String,
    pub at: Option<u64>,
}

pub fn history_path(shell: Shell) -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let from_env = |var: &str| {
        std::env::var_os(var)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let path = match shell {
        Shell::Zsh => from_env("HISTFILE").unwrap_or_else(|| home.join(".zsh_history")),
        Shell::Bash => from_env("HISTFILE").unwrap_or_else(|| home.join(".bash_history")),
        Shell::Fish => match from_env("XDG_DATA_HOME") {
            Some(data) => data.join("fish/fish_history"),
            None => home.join(".local/share/fish/fish_history"),
        },
    };
    path.exists().then_some(path)
}

pub fn read_history(shell: Shell, path: &Path) -> std::io::Result<Vec<Recalled>> {
    let text = String::from_utf8_lossy(&fs::read(path)?).into_owned();
    Ok(match shell {
        Shell::Zsh => zsh_history(&text),
        Shell::Bash => bash_history(&text),
        Shell::Fish => fish_history(&text),
    })
}

fn zsh_history(text: &str) -> Vec<Recalled> {
    let mut out = Vec::new();
    let mut pending: Option<String> = None;

    for raw in text.lines() {
        let joined = match pending.take() {
            Some(mut head) => {
                head.push_str(raw);
                head
            }
            None => raw.to_owned(),
        };
        if let Some(head) = joined.strip_suffix('\\') {
            pending = Some(head.to_owned());
            continue;
        }

        let (at, line) = match joined.strip_prefix(": ").and_then(|r| r.split_once(';')) {
            Some((meta, command)) => (
                meta.split(':').next().and_then(|t| t.trim().parse().ok()),
                command.to_owned(),
            ),
            None => (None, joined),
        };
        if !line.trim().is_empty() {
            out.push(Recalled { line, at });
        }
    }
    out
}

fn bash_history(text: &str) -> Vec<Recalled> {
    let mut out = Vec::new();
    let mut stamp = None;
    for line in text.lines() {
        if let Some(seconds) = line.strip_prefix('#').and_then(|r| r.trim().parse().ok()) {
            stamp = Some(seconds);
            continue;
        }
        if !line.trim().is_empty() {
            out.push(Recalled {
                line: line.to_owned(),
                at: stamp.take(),
            });
        }
    }
    out
}

fn fish_history(text: &str) -> Vec<Recalled> {
    let mut out: Vec<Recalled> = Vec::new();
    for line in text.lines() {
        if let Some(command) = line.strip_prefix("- cmd: ") {
            out.push(Recalled {
                line: unescape_fish(command),
                at: None,
            });
        } else if let Some(when) = line.trim().strip_prefix("when: ") {
            if let (Some(last), Ok(seconds)) = (out.last_mut(), when.trim().parse()) {
                last.at = Some(seconds);
            }
        }
    }
    out
}

fn unescape_fish(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match (ch, chars.clone().next()) {
            ('\\', Some('n')) => {
                chars.next();
                out.push('\n');
            }
            ('\\', Some('\\')) => {
                chars.next();
                out.push('\\');
            }
            _ => out.push(ch),
        }
    }
    out
}

pub const WRAPPERS: &[&str] = &[
    "sudo", "doas", "command", "builtin", "nohup", "exec", "env", "time", "nice", "stdbuf",
];

pub fn command_word(line: &str) -> Option<&str> {
    let mut rest = line.trim_start();
    loop {
        let word = rest.split_whitespace().next()?;
        if !WRAPPERS.contains(&word) && !word.contains('=') {
            return (!word.contains('/')
                && !word.starts_with(['#', '-', '$', '(', '"', '\'', '!']))
            .then_some(word);
        }
        rest = rest[word.len()..].trim_start();
        if rest.is_empty() {
            return None;
        }
    }
}

pub fn rc_files(shell: Shell) -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    match shell {
        Shell::Zsh => vec![home.join(".zshrc")],
        Shell::Bash => vec![home.join(".bashrc"), home.join(".bash_profile")],
        Shell::Fish => vec![home.join(".config/fish/config.fish")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_every_history_format() {
        let zsh = zsh_history(": 1700000000:0;git status\nls -la\n: 1700000005:12;make \\\nall\n");
        assert_eq!(zsh.len(), 3);
        assert_eq!(zsh[0].line, "git status");
        assert_eq!(zsh[0].at, Some(1_700_000_000));
        assert_eq!(zsh[1].at, None);
        assert_eq!(zsh[2].line, "make all");

        let bash = bash_history("#1700000000\ngit push\ncargo test\n");
        assert_eq!(bash.len(), 2);
        assert_eq!(bash[0].at, Some(1_700_000_000));
        assert_eq!(bash[1].at, None);

        let fish = fish_history("- cmd: echo hi\\nthere\n  when: 1700000000\n- cmd: ls\n");
        assert_eq!(fish.len(), 2);
        assert_eq!(fish[0].line, "echo hi\nthere");
        assert_eq!(fish[0].at, Some(1_700_000_000));
    }

    #[test]
    fn finds_the_command_behind_the_wrappers() {
        assert_eq!(command_word("sudo apt install vim"), Some("apt"));
        assert_eq!(command_word("FOO=1 BAR=2 make -j8"), Some("make"));
        assert_eq!(command_word("env RUST_LOG=debug cargo run"), Some("cargo"));
        assert_eq!(command_word("  ls"), Some("ls"));
        assert_eq!(command_word("./configure"), None);
        assert_eq!(command_word("/usr/bin/env python"), None);
        assert_eq!(command_word("# a comment"), None);
        assert_eq!(command_word(""), None);
        assert_eq!(command_word("FOO=1"), None);
    }

    #[test]
    fn help_output_yields_verbs_and_not_prose() {
        let verbs = |line: &str, parent: &str| verbs_in(line, parent);

        assert_eq!(verbs("   clone      Clone a repository", "git"), ["clone"]);
        assert_eq!(verbs("  auth:          Authenticate gh", "gh"), ["auth"]);
        assert_eq!(verbs("  brew install FORMULA|CASK...", "brew"), ["install"]);
        assert_eq!(
            verbs("    access, adduser, audit,", "npm"),
            ["access", "adduser", "audit"]
        );

        assert!(verbs("These are common Git commands:", "git").is_empty());
        assert!(verbs("  the quick brown fox", "git").is_empty());
        assert!(verbs("  -v, --verbose    Use verbose output", "cargo").is_empty());
        assert!(verbs("      --list       List installed commands", "cargo").is_empty());
        assert!(verbs("  possible values: auto, always, never", "cargo").is_empty());
        assert!(verbs("  usage: cat [-belnstuv] [file ...]", "cat").is_empty());
    }

    #[test]
    fn a_real_command_advertises_its_real_verbs() {
        let found = advertised_verbs("git");
        assert!(found.contains(&"status".to_string()), "{found:?}");
        assert!(found.contains(&"commit".to_string()), "{found:?}");
        assert!(advertised_verbs("cat").len() < 2);
    }

    #[test]
    fn old_path_listings_are_reaped() {
        let dir = std::env::temp_dir().join(format!("zcomplete-listings-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Ten PATHs' worth of listings, as ten activated venvs would leave.
        // Distinct mtimes, so "keep the newest" has something to sort on.
        // `utimes` rather than `File::set_times`, which needs a newer rustc
        // than this crate builds on.
        for n in 0..10u64 {
            let file = dir.join(format!("path.{n:016x}"));
            fs::write(&file, b"names").unwrap();
            let when = libc::timeval {
                tv_sec: n as libc::time_t + 1,
                tv_usec: 0,
            };
            let mut name = file.into_os_string().as_bytes().to_vec();
            name.push(0);
            assert_eq!(
                unsafe { libc::utimes(name.as_ptr().cast(), [when, when].as_ptr()) },
                0
            );
        }
        let newest = dir.join(format!("path.{:016x}", 9u64));
        reap_listings(&newest);

        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), KEEP_LISTINGS, "kept {left:?}");
        assert!(
            left.contains(&format!("path.{:016x}", 9u64)),
            "the listing in use was reaped: {left:?}"
        );
        assert!(
            !left.contains(&format!("path.{:016x}", 0u64)),
            "the oldest listing survived: {left:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_lookup_agrees_with_the_system() {
        assert!(on_path("sh"));
        assert!(!on_path("definitely-not-a-real-binary-xyzzy"));
        assert!(!on_path("/bin/sh"));
        assert!(!on_path("."));
        assert!(!on_path("sh\0junk"));
        assert!(which("sh\0junk").is_none());
        assert!(which("sh").is_some_and(|p| p.ends_with("sh")));
    }
}
