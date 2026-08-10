//! On-disk command store: frecency ranks, per-directory ranks, learned bindings.
//!
//! The file is rewritten whole on every save. That is fine at the sizes involved
//! (a busy shell knows a few hundred commands, not a few hundred thousand) and it
//! keeps writes atomic via rename.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAGIC: [u8; 4] = *b"ZCDB";
const FORMAT: u32 = 1;

const HOUR: u64 = 3_600;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;

/// Once the ranks add up to this, everything is scaled down so the store cannot
/// grow without bound and old habits eventually fade out of it.
const AGE_CEILING: f32 = 4_000.0;
const AGE_FACTOR: f32 = 0.92;
const AGE_FLOOR: f32 = 0.6;

const MAX_DIR_ENTRIES: usize = 4_096;
const MAX_BINDINGS: usize = 512;

/// A binding weight this large was set by hand and is never decayed away.
pub const PINNED: i32 = 1 << 20;
/// Accepting the same correction this many times makes it authoritative.
pub const STICKY_AT: i32 = 3;
/// Rejecting a correction this many times retires it for good.
pub const BURIED_AT: i32 = -2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

impl Shell {
    pub fn parse(name: &str) -> Option<Shell> {
        match name.rsplit('/').next().unwrap_or(name) {
            "zsh" => Some(Shell::Zsh),
            "bash" | "sh" => Some(Shell::Bash),
            "fish" => Some(Shell::Fish),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Shell::Zsh => "zsh",
            Shell::Bash => "bash",
            Shell::Fish => "fish",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// An executable found on PATH.
    External,
    /// A function, alias or builtin, and the shell it belongs to. A bash
    /// function is not a candidate when the user is sitting in fish.
    Shell(Shell),
}

impl Kind {
    fn tag(self) -> u8 {
        match self {
            Kind::External => 0,
            Kind::Shell(Shell::Zsh) => 1,
            Kind::Shell(Shell::Bash) => 2,
            Kind::Shell(Shell::Fish) => 3,
        }
    }

    fn from_tag(tag: u8) -> Kind {
        match tag {
            1 => Kind::Shell(Shell::Zsh),
            2 => Kind::Shell(Shell::Bash),
            3 => Kind::Shell(Shell::Fish),
            _ => Kind::External,
        }
    }

    pub fn usable_in(self, shell: Option<Shell>) -> bool {
        match (self, shell) {
            (Kind::External, _) => true,
            (Kind::Shell(owner), Some(here)) => owner == here,
            (Kind::Shell(_), None) => true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    pub rank: f32,
    pub last: u64,
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub input: String,
    pub target: String,
    pub weight: i32,
    pub last: u64,
}

#[derive(Default)]
pub struct Store {
    pub entries: Vec<Entry>,
    /// (directory hash, command) -> rank in that directory.
    dirs: HashMap<(u64, String), (f32, u64)>,
    pub bindings: Vec<Binding>,
    pub ignored: Vec<String>,
    dirty: bool,
    read_only: bool,
}

enum Broken {
    Garbled,
    TooNew,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// zoxide's decay curve. Recency dominates for a day, then frequency takes over.
pub fn frecency(rank: f32, last: u64, now: u64) -> f32 {
    match now.saturating_sub(last) {
        d if d < HOUR => rank * 4.0,
        d if d < DAY => rank * 2.0,
        d if d < WEEK => rank * 0.5,
        _ => rank * 0.25,
    }
}

pub fn dir_key(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.as_os_str().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

impl Store {
    pub fn open(path: &Path) -> Store {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => return Store::default(),
        };
        match decode(&bytes) {
            Ok(store) => store,
            Err(Broken::Garbled) => {
                // Never let a damaged file wedge the shell: keep it for forensics
                // and carry on from empty.
                let _ = fs::rename(path, path.with_extension("corrupt"));
                Store::default()
            }
            Err(Broken::TooNew) => Store {
                // A newer zcomplete wrote this. Run without it rather than
                // overwriting something we cannot read.
                read_only: true,
                ..Store::default()
            },
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn is_ignored(&self, name: &str) -> bool {
        self.ignored.iter().any(|n| n == name)
    }

    pub fn bump(&mut self, name: &str, kind: Kind, by: f32) {
        let at = now();
        self.dirty = true;
        match self.entries.iter_mut().find(|e| e.name == name) {
            Some(entry) => {
                entry.rank += by;
                entry.last = at;
                entry.kind = kind;
            }
            None => self.entries.push(Entry {
                name: name.to_owned(),
                kind,
                rank: by,
                last: at,
            }),
        }
        self.age();
    }

    /// Insert history without pretending it just happened. Imported commands
    /// carry the timestamp they actually ran at, so a year-old habit does not
    /// arrive looking like this morning's.
    pub fn seed(&mut self, name: &str, kind: Kind, by: f32, at: u64) {
        self.dirty = true;
        match self.entries.iter_mut().find(|e| e.name == name) {
            Some(entry) => {
                entry.rank += by;
                entry.last = entry.last.max(at);
            }
            None => self.entries.push(Entry {
                name: name.to_owned(),
                kind,
                rank: by,
                last: at,
            }),
        }
    }

    pub fn compact(&mut self) {
        self.age();
    }

    pub fn bump_in(&mut self, dir: u64, name: &str, by: f32) {
        let at = now();
        self.dirty = true;
        let slot = self.dirs.entry((dir, name.to_owned())).or_insert((0.0, at));
        slot.0 += by;
        slot.1 = at;
        if self.dirs.len() > MAX_DIR_ENTRIES {
            self.evict_dirs();
        }
    }

    /// Rank of `name` in `dir` alone, ignoring everywhere else it has been run.
    pub fn dir_rank(&self, dir: u64, name: &str) -> f32 {
        // `HashMap::get` on a tuple key needs an owned String, and this runs a
        // few hundred times per resolve at most.
        self.dirs
            .get(&(dir, name.to_owned()))
            .map_or(0.0, |(rank, last)| frecency(*rank, *last, now()))
    }

    pub fn forget(&mut self, name: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.name != name);
        self.dirs.retain(|(_, cmd), _| cmd != name);
        self.bindings.retain(|b| b.target != name);
        self.dirty = true;
        self.entries.len() != before
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.dirs.clear();
        self.dirty = true;
    }

    pub fn ignore(&mut self, name: &str) {
        if !self.is_ignored(name) {
            self.ignored.push(name.to_owned());
            self.dirty = true;
        }
    }

    pub fn unignore(&mut self, name: &str) -> bool {
        let before = self.ignored.len();
        self.ignored.retain(|n| n != name);
        self.dirty = self.dirty || self.ignored.len() != before;
        self.ignored.len() != before
    }

    pub fn binding(&self, input: &str, target: &str) -> Option<&Binding> {
        self.bindings
            .iter()
            .find(|b| b.input == input && b.target == target)
    }

    /// The correction to use for `input` without asking anything else, if the
    /// user has confirmed it often enough (or pinned it).
    pub fn sticky(&self, input: &str) -> Option<&str> {
        self.bindings
            .iter()
            .filter(|b| b.input == input && b.weight >= STICKY_AT)
            .max_by_key(|b| b.weight)
            .map(|b| b.target.as_str())
    }

    pub fn buried(&self, input: &str, target: &str) -> bool {
        self.binding(input, target)
            .is_some_and(|b| b.weight <= BURIED_AT)
    }

    pub fn nudge_binding(&mut self, input: &str, target: &str, by: i32) {
        let at = now();
        self.dirty = true;
        match self
            .bindings
            .iter_mut()
            .find(|b| b.input == input && b.target == target)
        {
            Some(binding) => {
                binding.weight = if by == PINNED {
                    PINNED
                } else {
                    binding.weight.saturating_add(by)
                };
                binding.last = at;
            }
            None => self.bindings.push(Binding {
                input: input.to_owned(),
                target: target.to_owned(),
                weight: by,
                last: at,
            }),
        }
        if self.bindings.len() > MAX_BINDINGS {
            self.bindings.sort_by_key(|b| std::cmp::Reverse(b.last));
            self.bindings.truncate(MAX_BINDINGS);
        }
    }

    pub fn unbind(&mut self, input: &str) -> bool {
        let before = self.bindings.len();
        self.bindings.retain(|b| b.input != input);
        self.dirty = true;
        self.bindings.len() != before
    }

    fn age(&mut self) {
        let total: f32 = self.entries.iter().map(|e| e.rank).sum();
        if total <= AGE_CEILING {
            return;
        }
        for entry in &mut self.entries {
            entry.rank *= AGE_FACTOR;
        }
        self.entries.retain(|e| e.rank >= AGE_FLOOR);
    }

    fn evict_dirs(&mut self) {
        let at = now();
        let mut scored: Vec<_> = self
            .dirs
            .iter()
            .map(|(key, (rank, last))| (key.clone(), frecency(*rank, *last, at)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        let keep: std::collections::HashSet<_> = scored
            .into_iter()
            .take(MAX_DIR_ENTRIES * 3 / 4)
            .map(|(key, _)| key)
            .collect();
        self.dirs.retain(|key, _| keep.contains(key));
    }

    /// Write through a temporary file in the same directory so a reader either
    /// sees the old database or the new one, never half of either.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if !self.dirty || self.read_only {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            // A log of everything the user runs is nobody else's business.
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let _guard = Lock::take(path)?;

        let temp = path.with_extension(format!("tmp.{}", std::process::id()));
        let written = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(&self.encode())?;
            // Deliberately no fsync. It measured at 5ms on APFS, and this runs
            // once per command the user types; the rename is still atomic, so
            // the worst a power cut can cost is the last few rank bumps, which
            // `zcomplete import` can rebuild from shell history anyway.
            fs::rename(&temp, path)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temp);
        }
        written
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 * self.entries.len() + 64);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());

        put_u32(&mut out, self.entries.len());
        for entry in &self.entries {
            put_str(&mut out, &entry.name);
            out.push(entry.kind.tag());
            out.extend_from_slice(&entry.rank.to_le_bytes());
            out.extend_from_slice(&entry.last.to_le_bytes());
        }

        put_u32(&mut out, self.dirs.len());
        for ((dir, name), (rank, last)) in &self.dirs {
            out.extend_from_slice(&dir.to_le_bytes());
            put_str(&mut out, name);
            out.extend_from_slice(&rank.to_le_bytes());
            out.extend_from_slice(&last.to_le_bytes());
        }

        put_u32(&mut out, self.bindings.len());
        for binding in &self.bindings {
            put_str(&mut out, &binding.input);
            put_str(&mut out, &binding.target);
            out.extend_from_slice(&binding.weight.to_le_bytes());
            out.extend_from_slice(&binding.last.to_le_bytes());
        }

        put_u32(&mut out, self.ignored.len());
        for name in &self.ignored {
            put_str(&mut out, name);
        }
        out
    }

    pub fn touch(&mut self) {
        self.dirty = true;
    }
}

fn put_u32(out: &mut Vec<u8>, value: usize) {
    out.extend_from_slice(&(value as u32).to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

type Field<T> = Result<T, Broken>;

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Field<&'a [u8]> {
        let end = self.at.checked_add(count).ok_or(Broken::Garbled)?;
        let slice = self.bytes.get(self.at..end).ok_or(Broken::Garbled)?;
        self.at = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Field<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| Broken::Garbled)
    }

    fn u16(&mut self) -> Field<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Field<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i32(&mut self) -> Field<i32> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Field<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn f32(&mut self) -> Field<f32> {
        Ok(f32::from_le_bytes(self.array()?))
    }

    fn string(&mut self) -> Field<String> {
        let len = self.u16()? as usize;
        std::str::from_utf8(self.take(len)?)
            .map(str::to_owned)
            .map_err(|_| Broken::Garbled)
    }

    fn count(&mut self) -> Field<usize> {
        let count = self.u32()? as usize;
        // A length field can only be honest if the bytes to fill it exist.
        if count > self.bytes.len() - self.at {
            return Err(Broken::Garbled);
        }
        Ok(count)
    }
}

fn decode(bytes: &[u8]) -> Field<Store> {
    let mut reader = Reader { bytes, at: 0 };
    if reader.take(4)? != MAGIC {
        return Err(Broken::Garbled);
    }
    match reader.u32()? {
        FORMAT => {}
        newer if newer > FORMAT => return Err(Broken::TooNew),
        _ => return Err(Broken::Garbled),
    }

    let mut store = Store::default();

    for _ in 0..reader.count()? {
        store.entries.push(Entry {
            name: reader.string()?,
            kind: Kind::from_tag(reader.take(1)?[0]),
            rank: reader.f32()?,
            last: reader.u64()?,
        });
    }

    for _ in 0..reader.count()? {
        let dir = reader.u64()?;
        let name = reader.string()?;
        let rank = reader.f32()?;
        let last = reader.u64()?;
        store.dirs.insert((dir, name), (rank, last));
    }

    for _ in 0..reader.count()? {
        store.bindings.push(Binding {
            input: reader.string()?,
            target: reader.string()?,
            weight: reader.i32()?,
            last: reader.u64()?,
        });
    }

    for _ in 0..reader.count()? {
        store.ignored.push(reader.string()?);
    }

    if store.entries.iter().any(|e| !e.rank.is_finite()) {
        return Err(Broken::Garbled);
    }
    Ok(store)
}

/// Advisory lock around the read-modify-write window. Shells fire commands fast
/// enough that two saves can genuinely overlap.
struct Lock(PathBuf);

impl Lock {
    fn take(db: &Path) -> io::Result<Option<Lock>> {
        let path = db.with_extension("lock");
        for attempt in 0..60 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Some(Lock(path))),
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                    if attempt == 0 && stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(3));
                }
                // An unwritable data directory is the caller's problem, not ours;
                // let the save itself produce the real error.
                Err(_) => return Ok(None),
            }
        }
        Ok(None)
    }
}

fn stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .and_then(|at| at.elapsed().map_err(|_| io::ErrorKind::Other.into()))
        .map_or(true, |age| age.as_secs() > 10)
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zcomplete-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("db.bin")
    }

    #[test]
    fn round_trips_every_table() {
        let path = scratch("roundtrip");
        let mut store = Store::default();
        store.bump("mkdir", Kind::External, 1.0);
        store.bump("clear", Kind::External, 3.0);
        store.bump("gs", Kind::Shell(Shell::Zsh), 2.0);
        store.bump_in(dir_key(Path::new("/tmp/project")), "make", 1.0);
        store.nudge_binding("mkd", "mkdir", 2);
        store.ignore("sl");
        store.save(&path).unwrap();

        let back = Store::open(&path);
        assert_eq!(back.entries.len(), 3);
        assert_eq!(back.get("gs").unwrap().kind, Kind::Shell(Shell::Zsh));
        assert_eq!(back.get("clear").unwrap().rank, 3.0);
        assert_eq!(back.binding("mkd", "mkdir").unwrap().weight, 2);
        assert!(back.is_ignored("sl"));
        assert!(back.dir_rank(dir_key(Path::new("/tmp/project")), "make") > 0.0);
    }

    #[test]
    fn truncated_file_is_quarantined_not_fatal() {
        let path = scratch("truncated");
        let mut store = Store::default();
        store.bump("mkdir", Kind::External, 1.0);
        store.save(&path).unwrap();

        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..bytes.len() - 4]).unwrap();

        let back = Store::open(&path);
        assert!(back.entries.is_empty());
        assert!(path.with_extension("corrupt").exists());
    }

    #[test]
    fn garbage_length_prefix_does_not_allocate_wildly() {
        assert!(matches!(
            decode(b"ZCDB\x01\x00\x00\x00\xff\xff\xff\xff"),
            Err(Broken::Garbled)
        ));
    }

    #[test]
    fn a_database_from_the_future_is_left_alone() {
        let path = scratch("future");
        let mut store = Store::default();
        store.bump("mkdir", Kind::External, 1.0);
        store.save(&path).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[4] = 99;
        fs::write(&path, &bytes).unwrap();

        let mut back = Store::open(&path);
        assert!(back.is_read_only());
        back.bump("clear", Kind::External, 1.0);
        back.save(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes, "the newer file was clobbered");
    }

    #[test]
    fn the_database_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch("perms");
        let mut store = Store::default();
        store.bump("mkdir", Kind::External, 1.0);
        store.save(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "mode was {:o}", mode);
    }

    #[test]
    fn recent_use_outranks_a_stale_habit() {
        let at = now();
        let fresh = frecency(1.0, at, at);
        let stale = frecency(20.0, at - 40 * DAY, at);
        assert!(fresh < stale, "frequency should still win at 20x the rank");
        assert!(frecency(6.0, at, at) > stale);
    }

    #[test]
    fn ranks_are_scaled_down_once_they_pile_up() {
        let mut store = Store::default();
        for i in 0..50 {
            store.bump(&format!("cmd{i}"), Kind::External, 100.0);
        }
        let total: f32 = store.entries.iter().map(|e| e.rank).sum();
        assert!(total <= AGE_CEILING * 1.05, "total rank ran away: {total}");
    }

    #[test]
    fn forgetting_a_command_takes_its_bindings_with_it() {
        let mut store = Store::default();
        store.bump("mkdir", Kind::External, 1.0);
        store.nudge_binding("mkd", "mkdir", 5);
        assert!(store.forget("mkdir"));
        assert!(store.sticky("mkd").is_none());
    }
}
