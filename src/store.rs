//! The on-disk store, rewritten whole on every save.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const MAGIC: [u8; 4] = *b"ZCDB";
/// Trailing tables decode by omission, so an older file still reads.
const FORMAT: u32 = 2;

const HOUR: u64 = 3_600;
const DAY: u64 = 24 * HOUR;
const WEEK: u64 = 7 * DAY;

const AGE_CEILING: f32 = 4_000.0;
const AGE_FACTOR: f32 = 0.92;
const AGE_FLOOR: f32 = 0.6;

const MAX_SCOPED: usize = 4_096;
const MAX_BINDINGS: usize = 512;

pub const PINNED: i32 = 1 << 20;
pub const STICKY_AT: i32 = 3;
pub const BURIED_AT: i32 = -2;

/// A live success is 1.0, a history sighting 0.5: `git status` clears this,
/// the `foo` in `grep foo x.c` never does.
pub const VERB_CONFIDENCE: f32 = 2.0;
pub const VERBS_TO_QUALIFY: usize = 2;

/// `is_verb` forbids NUL, so bookkeeping rows cannot collide with a real word.
const MARKER: char = '\0';
const ASKED: &str = "\0asked";

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
    External,
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
            (Kind::Shell(owner), Some(here)) => owner == here,
            _ => true,
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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    #[default]
    Safe,
    Unsafe,
    Bypass,
}

impl Mode {
    pub fn parse(name: &str) -> Option<Mode> {
        match name.trim_start_matches('-') {
            "safe" => Some(Mode::Safe),
            "unsafe" => Some(Mode::Unsafe),
            "bypass" => Some(Mode::Bypass),
            _ => None,
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Mode::Safe => "confirm every correction",
            Mode::Unsafe => "confirm only dangerous corrections",
            Mode::Bypass => "never confirm",
        }
    }

    fn tag(self) -> u8 {
        self as u8
    }

    fn from_tag(tag: u8) -> Option<Mode> {
        [Mode::Safe, Mode::Unsafe, Mode::Bypass]
            .get(tag as usize)
            .copied()
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mode::Safe => "safe",
            Mode::Unsafe => "unsafe",
            Mode::Bypass => "bypass",
        })
    }
}

type Ranks = HashMap<String, (f32, u64)>;

pub struct Store {
    pub entries: Vec<Entry>,
    /// Name to position in `entries`, and the running sum of their ranks. Both
    /// were linear scans per bump.
    at_name: HashMap<String, usize>,
    total: f32,
    /// The scoped section as it came off disk, indexed on first use. Every
    /// caller wants one scope, and decoding all of them was most of the cost
    /// of opening the database.
    raw: Vec<u8>,
    raw_count: usize,
    index: OnceLock<HashMap<u64, Vec<u32>>>,
    /// Scopes that have been read into memory. Overrides `raw` for those ids.
    scoped: HashMap<u64, Ranks>,
    pub bindings: Vec<Binding>,
    pub ignored: Vec<String>,
    mode: Mode,
    enabled: bool,
    dirty: bool,
    read_only: bool,
}

impl Default for Store {
    fn default() -> Store {
        Store {
            entries: Vec::new(),
            at_name: HashMap::new(),
            total: 0.0,
            raw: Vec::new(),
            raw_count: 0,
            index: OnceLock::new(),
            scoped: HashMap::new(),
            bindings: Vec::new(),
            ignored: Vec::new(),
            mode: Mode::default(),
            enabled: true,
            dirty: false,
            read_only: false,
        }
    }
}

enum Broken {
    Garbled,
    TooNew,
}

pub fn data_dir() -> PathBuf {
    match std::env::var_os("ZCOMPLETE_DATA_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => match std::env::var_os("XDG_DATA_HOME") {
            Some(dir) if !dir.is_empty() => PathBuf::from(dir),
            _ => PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/".into()))
                .join(".local/share"),
        }
        .join("zcomplete"),
    }
}

pub fn db_path() -> PathBuf {
    data_dir().join("commands.bin")
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// One decimal place without `core::fmt`'s float printer, which is 9KB of
/// binary for two commands that print a diagnostic.
pub fn tenths(value: f32) -> String {
    let scaled = (value.max(0.0) * 10.0).round() as u64;
    format!("{}.{}", scaled / 10, scaled % 10)
}

pub fn frecency(rank: f32, last: u64, now: u64) -> f32 {
    match now.saturating_sub(last) {
        d if d < HOUR => rank * 4.0,
        d if d < DAY => rank * 2.0,
        d if d < WEEK => rank * 0.5,
        _ => rank * 0.25,
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

pub fn dir_key(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    fnv(path.as_os_str().as_bytes())
}

pub fn sub_scope(parent: &str) -> u64 {
    let mut key = Vec::with_capacity(parent.len() + 1);
    key.push(0);
    key.extend_from_slice(parent.as_bytes());
    fnv(&key)
}

pub struct Editing {
    path: PathBuf,
    store: Store,
    lock: Option<Lock>,
}

pub fn edit(path: &Path) -> Editing {
    let lock = Lock::take(path);
    Editing {
        path: path.to_owned(),
        store: Store::open(path),
        lock,
    }
}

impl Editing {
    /// `flock` fails outright on some network mounts.
    pub fn locked(&self) -> bool {
        self.lock.is_some()
    }

    pub fn commit(self) -> io::Result<Store> {
        self.commit_taking(Vec::new())
    }

    /// Commit, then delete `taken` before the lock goes: unlinking after it
    /// was released let the next fold apply the same journals again. Only after
    /// a write that landed, since journals kept are counts kept.
    pub fn commit_taking(mut self, taken: Vec<PathBuf>) -> io::Result<Store> {
        // The inode we locked is no longer at that name, so somebody else
        // holds what is. Losing a run's counts beats overwriting mid-write.
        if self.lock.as_ref().is_some_and(|lock| !lock.current()) {
            self.lock = None;
            return Ok(self.store);
        }
        let result = self.store.write(&self.path);
        if result.is_ok() {
            for path in taken {
                let _ = fs::remove_file(path);
            }
        }
        self.lock = None;
        result.map(|()| self.store)
    }
}

impl std::ops::Deref for Editing {
    type Target = Store;

    fn deref(&self) -> &Store {
        &self.store
    }
}

impl std::ops::DerefMut for Editing {
    fn deref_mut(&mut self) -> &mut Store {
        &mut self.store
    }
}

/// Moves an unreadable database aside, to a name nothing else holds.
fn quarantine(path: &Path) -> Option<PathBuf> {
    for attempt in 0..16 {
        let kept = path.with_extension(match attempt {
            0 => format!("corrupt.{}", now()),
            n => format!("corrupt.{}.{n}", now()),
        });
        // `link` fails outright if the name is taken; `rename` would replace it.
        if fs::hard_link(path, &kept).is_ok() {
            let _ = fs::remove_file(path);
            reap_quarantined(path, &kept);
            return Some(kept);
        }
        if !kept.exists() {
            return None;
        }
    }
    None
}

const KEEP_QUARANTINED: usize = 2;

/// Deletes the oldest matching files, keeping the newest `keep - 1` beside
/// `keep_path`.
pub(crate) fn reap(dir: &Path, keep_path: &Path, keep: usize, matches: impl Fn(&str) -> bool) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.file_name().to_str().is_some_and(&matches))
        .filter(|entry| entry.path() != keep_path)
        .filter_map(|entry| {
            let at = entry.metadata().and_then(|meta| meta.modified()).ok()?;
            Some((at, entry.path()))
        })
        .collect();
    if found.len() < keep {
        return;
    }
    found.sort_unstable_by_key(|(at, _)| std::cmp::Reverse(*at));
    for (_, stale) in found.drain(keep - 1..) {
        let _ = fs::remove_file(stale);
    }
}

fn reap_quarantined(path: &Path, keep: &Path) {
    // The stem: `with_extension` replaced `commands.bin`'s extension.
    let (Some(dir), Some(stem)) = (path.parent(), path.file_stem().and_then(|n| n.to_str())) else {
        return;
    };
    let prefix = format!("{stem}.corrupt.");
    reap(dir, keep, KEEP_QUARANTINED, |name| {
        name.starts_with(&prefix)
    });
}

impl Store {
    pub fn open(path: &Path) -> Store {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            // Missing is the first run. Anything else exists and could not be
            // read, and starting empty would rewrite it as empty.
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Store::default(),
            Err(err) => return Store::unreadable(path, &err.to_string()),
        };
        match decode(&bytes) {
            Ok(store) => store,
            Err(Broken::Garbled) => match quarantine(path) {
                Some(kept) => {
                    eprintln!(
                        "zcomplete: {} was unreadable, kept at {}",
                        path.display(),
                        kept.display()
                    );
                    Store::default()
                }
                // Could not be set aside, so it is not ours to overwrite.
                None => Store::unreadable(path, "unreadable, and could not be set aside"),
            },
            // A newer zcomplete wrote this; do not overwrite what we cannot read.
            Err(Broken::TooNew) => Store {
                read_only: true,
                ..Store::default()
            },
        }
    }

    /// An empty store that refuses to be written.
    fn unreadable(path: &Path, why: &str) -> Store {
        eprintln!("zcomplete: {}: {why}, leaving it alone", path.display());
        Store {
            read_only: true,
            ..Store::default()
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn mode(&self) -> Mode {
        std::env::var("ZCOMPLETE_MODE")
            .ok()
            .as_deref()
            .and_then(Mode::parse)
            .unwrap_or(self.mode)
    }

    pub fn enabled(&self) -> bool {
        self.enabled && std::env::var_os("ZCOMPLETE_DISABLE").is_none()
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.dirty = true;
    }

    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        self.dirty = true;
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.at_name.get(name).map(|found| &self.entries[*found])
    }

    pub fn is_ignored(&self, name: &str) -> bool {
        self.ignored.iter().any(|n| n == name)
    }

    /// After anything that moved or dropped an entry. First position wins.
    fn reindex(&mut self) {
        self.at_name.clear();
        self.total = 0.0;
        for (found, entry) in self.entries.iter().enumerate() {
            self.at_name.entry(entry.name.clone()).or_insert(found);
            self.total += entry.rank;
        }
    }

    fn slot(&mut self, name: &str, kind: Kind, by: f32, at: u64) -> &mut Entry {
        self.dirty = true;
        self.total += by;
        let found = match self.at_name.get(name) {
            Some(found) => *found,
            None => {
                self.at_name.insert(name.to_owned(), self.entries.len());
                self.entries.push(Entry {
                    name: name.to_owned(),
                    kind,
                    rank: 0.0,
                    last: at,
                });
                self.entries.len() - 1
            }
        };
        let entry = &mut self.entries[found];
        entry.rank += by;
        entry.last = entry.last.max(at);
        entry
    }

    pub fn bump(&mut self, name: &str, kind: Kind, by: f32) {
        self.absorb(name, kind, by, now());
    }

    pub fn absorb(&mut self, name: &str, kind: Kind, by: f32, at: u64) {
        self.slot(name, kind, by, at).kind = kind;
        self.age();
    }

    pub fn seed(&mut self, name: &str, kind: Kind, by: f32, at: u64) {
        self.slot(name, kind, by, at);
    }

    pub fn compact(&mut self) {
        self.age();
    }

    /// Where each scope's records start in `raw`, in one pass that allocates
    /// nothing per word.
    fn index(&self) -> &HashMap<u64, Vec<u32>> {
        self.index.get_or_init(|| {
            let mut found: HashMap<u64, Vec<u32>> = HashMap::new();
            let mut at = 0usize;
            while at + 10 <= self.raw.len() {
                let start = at;
                let scope = u64::from_le_bytes(self.raw[at..at + 8].try_into().expect("8 bytes"));
                let len =
                    u16::from_le_bytes(self.raw[at + 8..at + 10].try_into().expect("2 bytes"));
                at += 10 + len as usize + 12;
                if at > self.raw.len() {
                    break;
                }
                found.entry(scope).or_default().push(start as u32);
            }
            found
        })
    }

    fn record_at(&self, start: u32) -> Option<(&str, f32, u64)> {
        let at = start as usize;
        let len = u16::from_le_bytes(self.raw.get(at + 8..at + 10)?.try_into().ok()?) as usize;
        let name = std::str::from_utf8(self.raw.get(at + 10..at + 10 + len)?).ok()?;
        let tail = self.raw.get(at + 10 + len..at + 10 + len + 12)?;
        let rank = f32::from_le_bytes(tail[..4].try_into().ok()?);
        let last = u64::from_le_bytes(tail[4..].try_into().ok()?);
        Some((name, rank, last))
    }

    /// One scope, borrowed if it is in memory and decoded off `raw` otherwise.
    fn words(&self, scope: u64) -> Cow<'_, Ranks> {
        match self.scoped.get(&scope) {
            Some(words) => Cow::Borrowed(words),
            None => Cow::Owned(
                self.index()
                    .get(&scope)
                    .into_iter()
                    .flatten()
                    .filter_map(|start| self.record_at(*start))
                    .map(|(name, rank, last)| (name.to_owned(), (rank, last)))
                    .collect(),
            ),
        }
    }

    /// Pulls a scope out of `raw` so it can be written to. Its records stay in
    /// `raw` and are skipped at encode time.
    fn open_scope(&mut self, scope: u64) -> &mut Ranks {
        if !self.scoped.contains_key(&scope) {
            let words = self.words(scope).into_owned();
            self.scoped.insert(scope, words);
        }
        self.scoped.get_mut(&scope).expect("just inserted")
    }

    pub fn bump_in(&mut self, scope: u64, name: &str, by: f32) {
        self.bump_in_at(scope, name, by, now());
    }

    pub fn bump_in_at(&mut self, scope: u64, name: &str, by: f32, at: u64) {
        self.dirty = true;
        let slot = self
            .open_scope(scope)
            .entry(name.to_owned())
            .or_insert((0.0, at));
        slot.0 += by;
        // Never backwards: journal lines arrive out of order.
        slot.1 = slot.1.max(at);
        if self.scoped_len() > MAX_SCOPED {
            self.evict_scoped();
        }
    }

    pub fn ranker(&self, scope: u64, at: u64) -> impl Fn(&str) -> f32 + '_ {
        let words = self.words(scope);
        move |name| {
            words
                .get(name)
                .map_or(0.0, |(rank, last)| frecency(*rank, *last, at))
        }
    }

    pub fn scope_knows(&self, scope: u64, name: &str) -> bool {
        match self.scoped.get(&scope) {
            Some(words) => words.contains_key(name),
            None => self
                .index()
                .get(&scope)
                .into_iter()
                .flatten()
                .any(|start| self.record_at(*start).is_some_and(|(had, ..)| had == name)),
        }
    }

    pub fn in_scope(&self, scope: u64) -> Vec<Entry> {
        self.words(scope)
            .iter()
            .filter(|(name, _)| !name.starts_with(MARKER))
            .map(|(name, (rank, last))| Entry {
                name: name.clone(),
                kind: Kind::External,
                rank: *rank,
                last: *last,
            })
            .collect()
    }

    pub fn verbs(&self, parent: &str) -> Vec<Entry> {
        let mut found = self.in_scope(sub_scope(parent));
        found.retain(|entry| entry.rank >= VERB_CONFIDENCE);
        found
    }

    /// Counted, not collected: `doctor` asks this of every command it knows.
    pub fn takes_verbs(&self, parent: &str) -> bool {
        self.words(sub_scope(parent))
            .iter()
            .filter(|(name, (rank, _))| *rank >= VERB_CONFIDENCE && !name.starts_with(MARKER))
            .take(VERBS_TO_QUALIFY)
            .count()
            >= VERBS_TO_QUALIFY
    }

    pub fn asked_for_help(&self, parent: &str) -> bool {
        self.scope_knows(sub_scope(parent), ASKED)
    }

    pub fn mark_asked(&mut self, parent: &str) {
        self.bump_in(sub_scope(parent), ASKED, VERB_CONFIDENCE);
    }

    /// Every bump checks this against the cap, so it counts the few open
    /// scopes rather than the whole index.
    fn scoped_len(&self) -> usize {
        let opened: usize = self
            .scoped
            .keys()
            .map(|scope| self.index().get(scope).map_or(0, Vec::len))
            .sum();
        self.raw_count.saturating_sub(opened)
            + self.scoped.values().map(HashMap::len).sum::<usize>()
    }

    /// Only for callers that must see every scope at once.
    fn materialise(&mut self) {
        let scopes: Vec<u64> = self.index().keys().copied().collect();
        for scope in scopes {
            self.open_scope(scope);
        }
        self.raw = Vec::new();
        self.raw_count = 0;
        self.index.take();
    }

    pub fn forget(&mut self, name: &str) -> bool {
        self.materialise();
        let before = (self.entries.len(), self.bindings.len());
        self.entries.retain(|e| e.name != name);
        let scoped = self.scoped.remove(&sub_scope(name)).is_some();
        let mut dropped = scoped;
        for words in self.scoped.values_mut() {
            dropped |= words.remove(name).is_some();
        }
        self.bindings.retain(|b| b.target != name);
        self.reindex();
        // Only when something went, or `forget nosuchcommand` rewrites the
        // whole file to say nothing changed.
        let now = (self.entries.len(), self.bindings.len());
        self.dirty |= dropped || now != before;
        self.entries.len() != before.0
    }

    /// Bindings and the ignore list too, or `forget --all` reports an empty
    /// database that still corrects things.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.at_name.clear();
        self.total = 0.0;
        self.scoped.clear();
        self.raw = Vec::new();
        self.raw_count = 0;
        self.index.take();
        self.bindings.clear();
        self.ignored.clear();
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
        self.dirty |= self.ignored.len() != before;
        self.ignored.len() != before
    }

    pub fn sticky(&self, input: &str) -> Option<&str> {
        self.bindings
            .iter()
            .filter(|b| b.input == input && b.weight >= STICKY_AT)
            .max_by_key(|b| b.weight)
            .map(|b| b.target.as_str())
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
            // Weight before recency: ordinary use fills this table, and a
            // lost pin does not stop resolving, it resolves somewhere else.
            self.bindings.sort_by_key(|b| {
                (
                    std::cmp::Reverse(b.weight >= PINNED),
                    std::cmp::Reverse(b.last),
                )
            });
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
        if self.total <= AGE_CEILING {
            return;
        }
        for entry in &mut self.entries {
            entry.rank *= AGE_FACTOR;
        }
        self.entries.retain(|e| e.rank >= AGE_FLOOR);
        self.reindex();
    }

    fn evict_scoped(&mut self) {
        self.materialise();
        let at = now();
        let mut scored: Vec<_> = self
            .scoped
            .iter()
            .flat_map(|(scope, words)| {
                words.iter().map(move |(name, (rank, last))| {
                    (
                        *scope,
                        name,
                        *rank >= VERB_CONFIDENCE,
                        frecency(*rank, *last, at),
                    )
                })
            })
            .collect();
        scored.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| b.3.total_cmp(&a.3)));
        let keep: std::collections::HashSet<_> = scored
            .into_iter()
            .take(MAX_SCOPED / 2)
            .map(|(scope, name, _, _)| (scope, name.clone()))
            .collect();
        self.scoped.retain(|scope, words| {
            words.retain(|name, _| keep.contains(&(*scope, name.clone())));
            !words.is_empty()
        });
    }

    fn write(&self, path: &Path) -> io::Result<()> {
        if !self.dirty || self.read_only {
            return Ok(());
        }
        // Still made here: `flock` failing on a network mount leaves nobody
        // else to make it.
        if let Some(parent) = path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }

        let temp = path.with_extension(format!("tmp.{}", std::process::id()));
        let written = (|| {
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temp)?
                .write_all(&self.encode())?;
            // No fsync: 5ms on APFS, once per command. The rename is still atomic.
            fs::rename(&temp, path)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temp);
        }
        written
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 * self.entries.len() + self.raw.len() + 1024);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FORMAT.to_le_bytes());

        put_u32(&mut out, self.entries.len());
        for entry in &self.entries {
            put_str(&mut out, &entry.name);
            out.push(entry.kind.tag());
            out.extend_from_slice(&entry.rank.to_le_bytes());
            out.extend_from_slice(&entry.last.to_le_bytes());
        }

        put_u32(&mut out, self.scoped_len());
        // Copied, not re-encoded: a scope nobody read is the bytes it arrived
        // as.
        for (scope, starts) in self.index() {
            if self.scoped.contains_key(scope) {
                continue;
            }
            for start in starts {
                let at = *start as usize;
                let len = u16::from_le_bytes(
                    self.raw[at + 8..at + 10]
                        .try_into()
                        .expect("indexed record"),
                ) as usize;
                out.extend_from_slice(&self.raw[at..at + 10 + len + 12]);
            }
        }
        for (scope, words) in &self.scoped {
            for (name, (rank, last)) in words {
                out.extend_from_slice(&scope.to_le_bytes());
                put_str(&mut out, name);
                out.extend_from_slice(&rank.to_le_bytes());
                out.extend_from_slice(&last.to_le_bytes());
            }
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

        out.push(self.mode.tag());
        out.push(u8::from(self.enabled));
        out
    }

    pub fn touch(&mut self) {
        self.dirty = true;
    }
}

fn put_u32(out: &mut Vec<u8>, value: usize) {
    out.extend_from_slice(&(value as u32).to_le_bytes());
}

/// Cut rather than written with a wrapped length, which would desync the reader.
fn put_str(out: &mut Vec<u8>, value: &str) {
    let mut end = value.len().min(u16::MAX as usize);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    out.extend_from_slice(&(end as u16).to_le_bytes());
    out.extend_from_slice(&value.as_bytes()[..end]);
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

    fn byte(&mut self) -> Field<u8> {
        Ok(self.take(1)?[0])
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
        match count > self.bytes.len() - self.at {
            true => Err(Broken::Garbled),
            false => Ok(count),
        }
    }
}

fn decode(bytes: &[u8]) -> Field<Store> {
    let mut reader = Reader { bytes, at: 0 };
    if reader.take(4)? != MAGIC {
        return Err(Broken::Garbled);
    }
    match reader.u32()? {
        version if version > FORMAT => return Err(Broken::TooNew),
        0 => return Err(Broken::Garbled),
        _ => {}
    }

    let mut store = Store::default();

    let count = reader.count()?;
    store.entries.reserve_exact(count);
    for _ in 0..count {
        store.entries.push(Entry {
            name: reader.string()?,
            kind: Kind::from_tag(reader.byte()?),
            rank: reader.f32()?,
            last: reader.u64()?,
        });
    }

    // Stepped over, not read: `Store::index` walks it on the first question.
    let scoped_count = reader.count()?;
    let scoped_from = reader.at;
    for _ in 0..scoped_count {
        let _scope = reader.u64()?;
        let len = reader.u16()? as usize;
        reader.take(len)?;
        reader.take(12)?;
    }
    store.raw = bytes[scoped_from..reader.at].to_vec();
    store.raw_count = scoped_count;

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

    // Absent in a version 1 file, which is why it is last.
    store.mode = reader
        .byte()
        .ok()
        .and_then(Mode::from_tag)
        .unwrap_or_default();
    store.enabled = reader.byte().map_or(true, |byte| byte != 0);

    if store.entries.iter().any(|e| !e.rank.is_finite()) {
        return Err(Broken::Garbled);
    }
    store.reindex();
    Ok(store)
}

/// Closing the file releases the lock, so there is nothing to unlink.
struct Lock {
    file: fs::File,
    path: PathBuf,
}

impl Lock {
    /// Deleting the data directory unlinks the lock without releasing it, so
    /// the next process takes a fresh one and both write believing themselves
    /// alone.
    fn current(&self) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Ok(ours) = self.file.metadata() else {
            return false;
        };
        fs::metadata(&self.path).is_ok_and(|now| now.ino() == ours.ino() && now.dev() == ours.dev())
    }
}

impl Lock {
    /// `flock`, so a killed shell leaves nothing to guess about. Waiting
    /// outright is safe only because no caller holds it across a prompt.
    fn take(db: &Path) -> Option<Lock> {
        let path = db.with_extension("lock");
        let mut made_dir = false;
        let file = loop {
            match fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)
            {
                Ok(file) => break file,
                Err(err) if err.kind() == io::ErrorKind::NotFound && !made_dir => {
                    made_dir = true;
                    fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(path.parent()?)
                        .ok()?;
                }
                Err(_) => return None,
            }
        };
        let held = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0;
        held.then_some(Lock { file, path })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zcomplete-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("db.bin")
    }

    #[test]
    fn a_database_that_cannot_be_read_is_never_written_over() {
        let path = scratch("unreadable");
        let mut store = edit(&path);
        store.bump("git", Kind::External, 1.0);
        store.bump("cargo", Kind::External, 1.0);
        store.nudge_binding("gs", "git", PINNED);
        store.commit().unwrap();
        let before = fs::read(&path).unwrap();
        assert!(before.len() > 40, "nothing was written to begin with");

        // Not missing: present, and unreadable. Starting empty here and saving
        // that is how a database is destroyed by the run that could not read it.
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        let mut blind = edit(&path);
        blind.bump("ls", Kind::External, 1.0);
        blind.commit().unwrap();

        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "the database was rewritten"
        );
        let back = Store::open(&path);
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.sticky("gs"), Some("git"));
    }

    #[test]
    fn a_second_corruption_does_not_destroy_the_first_ones_copy() {
        let path = scratch("quarantine");
        let mut store = edit(&path);
        store.bump("git", Kind::External, 1.0);
        store.commit().unwrap();
        // Truncated rather than replaced: what a half-written file looks like,
        // and it is still most of the database somebody would want back.
        let real = fs::read(&path).unwrap();
        let hurt = real[..real.len() - 4].to_vec();
        fs::write(&path, &hurt).unwrap();
        assert!(Store::open(&path).entries.is_empty());

        let mut second = edit(&path);
        second.bump("ls", Kind::External, 1.0);
        second.commit().unwrap();
        let small = fs::read(&path).unwrap();
        fs::write(&path, &small[..small.len() - 4]).unwrap();
        assert!(Store::open(&path).entries.is_empty());

        // Both corruptions kept a copy, and the first one still holds the real
        // database. One fixed `commands.corrupt` had the second rename over it.
        let dir = path.parent().unwrap();
        let kept: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".corrupt"))
            })
            .collect();
        assert_eq!(kept.len(), 2, "a quarantine was overwritten: {kept:?}");
        assert!(
            kept.iter().any(|p| fs::read(p).unwrap() == hurt),
            "the first copy was destroyed by the second corruption"
        );
    }

    #[test]
    fn quarantined_copies_do_not_pile_up_forever() {
        let path = scratch("quarantine-cap");
        // Six corruptions in a row. Each one used to leave a full-sized copy of
        // the database behind with nothing to ever remove it.
        for round in 0..6 {
            let mut store = edit(&path);
            store.bump("git", Kind::External, 1.0 + round as f32);
            store.commit().unwrap();
            let real = fs::read(&path).unwrap();
            fs::write(&path, &real[..real.len() - 4]).unwrap();
            assert!(Store::open(&path).entries.is_empty());
        }
        let kept = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.contains(".corrupt"))
            })
            .count();
        assert!(
            kept <= KEEP_QUARANTINED,
            "{kept} quarantined copies kept, expected at most {KEEP_QUARANTINED}"
        );
    }

    #[test]
    fn the_index_and_the_row_count_agree_with_the_long_way_round() {
        let path = scratch("invariants");
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let check = |store: &Store| {
            for (found, entry) in store.entries.iter().enumerate() {
                let first = store.entries.iter().position(|e| e.name == entry.name);
                assert_eq!(
                    store.at_name.get(&entry.name).copied(),
                    first,
                    "{}",
                    entry.name
                );
                let _ = found;
            }
            let summed: f32 = store.entries.iter().map(|e| e.rank).sum();
            assert!(
                (store.total - summed).abs() < 0.5,
                "{} vs {summed}",
                store.total
            );
            let rows = decode(&store.encode()).ok().expect("re-readable");
            assert_eq!(rows.raw_count, store.scoped_len(), "row count is wrong");
        };

        let mut store = edit(&path);
        for round in 0..600u64 {
            let name = format!("cmd{}", next() % 40);
            match next() % 6 {
                0 => {
                    store.forget(&name);
                }
                1 => store.bump_in(next() % 7, &name, 1.0),
                2 => store.bump_in(sub_scope("git"), &name, 2.0),
                3 => store.nudge_binding(&name, "git", 1),
                _ => store.bump(&name, Kind::External, 1.0),
            }
            if round % 97 == 0 {
                store.touch();
                // Dropped first: `edit` waits on a lock this process still holds.
                store = {
                    let saved = store.commit().unwrap();
                    drop(saved);
                    edit(&path)
                };
            }
            check(&store);
        }
        store.touch();
        store.commit().unwrap();
        check(&Store::open(&path));
    }

    #[test]
    fn a_forget_that_found_nothing_writes_nothing() {
        let path = scratch("noop-forget");
        let mut store = edit(&path);
        store.bump("git", Kind::External, 1.0);
        store.commit().unwrap();

        let mut store = edit(&path);
        assert!(!store.forget("nosuchcommand"));
        assert!(
            !store.dirty,
            "a forget that removed nothing marked it dirty"
        );
    }

    #[test]
    fn a_pin_outlives_the_churn_that_fills_the_table() {
        let mut store = Store::default();
        store.nudge_binding("gs", "git", PINNED);
        // Every accepted and every refused correction leaves a row here, so
        // ordinary use fills the table without anyone asking it to.
        for i in 0..MAX_BINDINGS + 8 {
            store.nudge_binding(&format!("churn{i}"), "git", 1);
        }
        for binding in &mut store.bindings {
            if binding.input == "gs" {
                binding.last = 1;
            }
        }
        store.nudge_binding("one-more", "git", 1);

        assert_eq!(store.bindings.len(), MAX_BINDINGS);
        assert_eq!(store.sticky("gs"), Some("git"), "the pin was evicted");
    }

    #[test]
    fn round_trips_every_table() {
        let path = scratch("roundtrip");
        let mut store = edit(&path);
        store.bump("mkdir", Kind::External, 1.0);
        store.bump("clear", Kind::External, 3.0);
        store.bump("gs", Kind::Shell(Shell::Zsh), 2.0);
        store.bump_in(dir_key(Path::new("/tmp/project")), "make", 1.0);
        store.nudge_binding("mkd", "mkdir", 2);
        store.ignore("sl");
        store.commit().unwrap();

        let back = Store::open(&path);
        assert_eq!(back.entries.len(), 3);
        assert_eq!(back.get("gs").unwrap().kind, Kind::Shell(Shell::Zsh));
        assert_eq!(back.get("clear").unwrap().rank, 3.0);
        let binding = back
            .bindings
            .iter()
            .find(|b| b.input == "mkd" && b.target == "mkdir");
        assert_eq!(binding.unwrap().weight, 2);
        assert!(back.is_ignored("sl"));
        assert!(back.ranker(dir_key(Path::new("/tmp/project")), now())("make") > 0.0);
    }

    #[test]
    fn a_scope_nobody_read_survives_a_write_that_touched_another() {
        let path = scratch("lazy");
        let mut store = edit(&path);
        for dir in 0..40u64 {
            for n in 0..5 {
                store.bump_in(dir, &format!("cmd{dir}-{n}"), (n + 1) as f32);
            }
        }
        store.bump_in(sub_scope("git"), "status", 3.0);
        store.commit().unwrap();

        // Reopened, so every scope is raw. Touch exactly one, write, reopen.
        let mut again = edit(&path);
        assert_eq!(again.in_scope(7).len(), 5);
        again.bump_in(7, "cmd7-0", 10.0);
        again.commit().unwrap();

        let back = Store::open(&path);
        for dir in 0..40u64 {
            assert_eq!(back.in_scope(dir).len(), 5, "scope {dir} lost words");
        }
        assert!(back.scope_knows(sub_scope("git"), "status"));
        assert_eq!(back.verbs("git").len(), 1);
        let bumped = back
            .in_scope(7)
            .into_iter()
            .find(|e| e.name == "cmd7-0")
            .expect("cmd7-0");
        assert_eq!(bumped.rank, 11.0);
        assert!(back.ranker(7, now())("cmd7-0") > 0.0);
        assert_eq!(back.ranker(9, now())("cmd7-0"), 0.0);
    }

    #[test]
    fn forgetting_reaches_scopes_that_were_never_read() {
        let path = scratch("lazyforget");
        let mut store = edit(&path);
        store.bump("make", Kind::External, 1.0);
        store.bump_in(3, "make", 5.0);
        store.bump_in(4, "make", 5.0);
        store.bump_in(4, "cargo", 5.0);
        store.commit().unwrap();

        let mut again = edit(&path);
        assert!(again.forget("make"));
        again.commit().unwrap();

        let back = Store::open(&path);
        assert_eq!(back.ranker(3, now())("make"), 0.0);
        assert_eq!(back.ranker(4, now())("make"), 0.0);
        assert!(back.ranker(4, now())("cargo") > 0.0);
    }

    #[test]
    fn emptying_the_database_leaves_nothing_that_still_answers() {
        let path = scratch("clear");
        let mut store = edit(&path);
        store.bump("mkdir", Kind::External, 1.0);
        store.nudge_binding("mkd", "mkdir", 2);
        store.ignore("sl");
        store.clear();
        store.commit().unwrap();

        let back = Store::open(&path);
        assert!(back.entries.is_empty());
        assert!(back.bindings.is_empty());
        assert!(!back.is_ignored("sl"));
        assert!(back.sticky("mkd").is_none());
    }

    #[test]
    fn a_database_from_before_the_settings_trailer_still_reads() {
        let mut store = Store::default();
        store.bump("mkdir", Kind::External, 7.0);
        store.set_mode(Mode::Bypass);

        let mut bytes = store.encode();
        bytes.truncate(bytes.len() - 2);
        bytes[4..8].copy_from_slice(&1u32.to_le_bytes());

        let back = decode(&bytes).ok().expect("a version 1 file is readable");
        assert_eq!(back.get("mkdir").unwrap().rank, 7.0);
        assert_eq!(back.mode(), Mode::Safe, "an absent setting is the default");
        assert!(back.enabled());
    }

    #[test]
    fn a_directory_and_a_command_of_the_same_name_are_different_scopes() {
        let mut store = Store::default();
        store.bump_in(dir_key(Path::new("git")), "status", 5.0);
        assert!(!store.scope_knows(sub_scope("git"), "status"));

        store.bump_in(sub_scope("git"), "status", 1.0);
        assert!(store.scope_knows(sub_scope("git"), "status"));
        assert_eq!(
            store.in_scope(sub_scope("git")).len(),
            1,
            "the directory's ranks leaked into the command's scope"
        );
    }

    #[test]
    fn forgetting_a_command_takes_its_subcommands_and_bindings_with_it() {
        let mut store = Store::default();
        store.bump("git", Kind::External, 1.0);
        store.bump_in(sub_scope("git"), "status", 3.0);
        store.nudge_binding("gti", "git", 5);
        assert!(store.forget("git"));
        assert!(store.in_scope(sub_scope("git")).is_empty());
        assert!(store.sticky("gti").is_none());
    }

    #[test]
    fn truncated_file_is_quarantined_not_fatal() {
        let path = scratch("truncated");
        let mut store = edit(&path);
        store.bump("mkdir", Kind::External, 1.0);
        store.commit().unwrap();

        let bytes = fs::read(&path).unwrap();
        fs::write(&path, &bytes[..bytes.len() - 4]).unwrap();

        let back = Store::open(&path);
        assert!(back.entries.is_empty());
        let dir = path.parent().unwrap();
        assert!(fs::read_dir(dir).unwrap().flatten().any(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.contains(".corrupt"))
        }));
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
        let mut store = edit(&path);
        store.bump("mkdir", Kind::External, 1.0);
        store.commit().unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[4] = 99;
        fs::write(&path, &bytes).unwrap();

        let mut back = edit(&path);
        assert!(back.is_read_only());
        back.bump("clear", Kind::External, 1.0);
        back.commit().unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "the newer file was clobbered"
        );
    }

    #[test]
    fn the_database_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch("perms");
        let mut store = edit(&path);
        store.bump("mkdir", Kind::External, 1.0);
        store.commit().unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "mode was {mode:o}");
    }

    #[test]
    fn recent_use_outranks_a_stale_habit() {
        let at = now();
        let stale = frecency(20.0, at - 40 * DAY, at);
        assert!(frecency(1.0, at, at) < stale, "frequency should still win");
        assert!(frecency(6.0, at, at) > stale);
    }

    #[test]
    fn an_absurdly_long_name_does_not_destroy_the_database() {
        let mut store = Store::default();
        store.bump("mkdir", Kind::External, 3.0);
        store.ignore(&"x".repeat(70_000));
        let back = decode(&store.encode()).ok().expect("still readable");
        assert_eq!(back.get("mkdir").unwrap().rank, 3.0);
        assert_eq!(back.ignored[0].len(), u16::MAX as usize);
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
}
