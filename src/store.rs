//! The on-disk store, rewritten whole on every save.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const MAGIC: [u8; 4] = *b"ZCDB";
/// Anything appended after the last table decodes by omission, so an older
/// file still reads. 2 added the settings trailer.
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

/// A live success is 1.0 and a history sighting 0.5: `git status` clears this,
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
    /// The scoped section as it came off disk, plus an index built on first
    /// use. Every caller wants one scope, and decoding all of them into nested
    /// maps was three quarters of the cost of opening the database.
    raw: Vec<u8>,
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
            raw: Vec::new(),
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

/// One decimal place without `core::fmt`'s float printer, which is nine
/// kilobytes of binary linked in for two commands that print a diagnostic.
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
    /// Whether this edit actually holds the lock. `flock` fails outright on some
    /// network mounts, and a caller about to consume something it cannot put
    /// back wants to know that before it starts rather than after.
    pub fn locked(&self) -> bool {
        self.lock.is_some()
    }

    pub fn commit(mut self) -> io::Result<Store> {
        let result = self.store.write(&self.path);
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

impl Store {
    pub fn open(path: &Path) -> Store {
        let Ok(bytes) = fs::read(path) else {
            return Store::default();
        };
        match decode(&bytes) {
            Ok(store) => store,
            Err(Broken::Garbled) => {
                let _ = fs::rename(path, path.with_extension("corrupt"));
                Store::default()
            }
            // A newer zcomplete wrote this; do not overwrite what we cannot read.
            Err(Broken::TooNew) => Store {
                read_only: true,
                ..Store::default()
            },
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
        self.entries.iter().find(|e| e.name == name)
    }

    pub fn is_ignored(&self, name: &str) -> bool {
        self.ignored.iter().any(|n| n == name)
    }

    fn slot(&mut self, name: &str, kind: Kind, by: f32, at: u64) -> &mut Entry {
        self.dirty = true;
        match self.entries.iter().position(|e| e.name == name) {
            Some(found) => {
                let entry = &mut self.entries[found];
                entry.rank += by;
                entry.last = entry.last.max(at);
                entry
            }
            None => {
                self.entries.push(Entry {
                    name: name.to_owned(),
                    kind,
                    rank: by,
                    last: at,
                });
                self.entries.last_mut().expect("just pushed")
            }
        }
    }

    pub fn bump(&mut self, name: &str, kind: Kind, by: f32) {
        self.absorb(name, kind, by, now());
    }

    /// `bump` for something that happened earlier: the journal records when.
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

    /// Where every record of each scope starts in `raw`. One pass that reads a
    /// scope id and steps over the rest, so nothing is allocated per word.
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

    /// One scope, in memory if it has been written to and off disk otherwise.
    fn words(&self, scope: u64) -> Ranks {
        if let Some(words) = self.scoped.get(&scope) {
            return words.clone();
        }
        self.index()
            .get(&scope)
            .into_iter()
            .flatten()
            .filter_map(|start| self.record_at(*start))
            .map(|(name, rank, last)| (name.to_owned(), (rank, last)))
            .collect()
    }

    /// Pulls a scope out of `raw` so it can be written to. Records for it stay
    /// in `raw` and are skipped at encode time, which is cheaper than rebuilding
    /// the section every time one word changes.
    fn open_scope(&mut self, scope: u64) -> &mut Ranks {
        if !self.scoped.contains_key(&scope) {
            let words = self.words(scope);
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
        // Never backwards: journal lines arrive out of order, and one from a
        // shell that died last week must not age today's use.
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
            .into_iter()
            .filter(|(name, _)| !name.starts_with(MARKER))
            .map(|(name, (rank, last))| Entry {
                name,
                kind: Kind::External,
                rank,
                last,
            })
            .collect()
    }

    pub fn verbs(&self, parent: &str) -> Vec<Entry> {
        let mut found = self.in_scope(sub_scope(parent));
        found.retain(|entry| entry.rank >= VERB_CONFIDENCE);
        found
    }

    pub fn takes_verbs(&self, parent: &str) -> bool {
        self.verbs(parent).len() >= VERBS_TO_QUALIFY
    }

    pub fn asked_for_help(&self, parent: &str) -> bool {
        self.scope_knows(sub_scope(parent), ASKED)
    }

    pub fn mark_asked(&mut self, parent: &str) {
        self.bump_in(sub_scope(parent), ASKED, VERB_CONFIDENCE);
    }

    fn scoped_len(&self) -> usize {
        let untouched: usize = self
            .index()
            .iter()
            .filter(|(scope, _)| !self.scoped.contains_key(scope))
            .map(|(_, starts)| starts.len())
            .sum();
        untouched + self.scoped.values().map(HashMap::len).sum::<usize>()
    }

    /// Everything into memory. Only for the three callers that have to see every
    /// scope at once, all of which are already rewriting the whole table.
    fn materialise(&mut self) {
        let scopes: Vec<u64> = self.index().keys().copied().collect();
        for scope in scopes {
            self.open_scope(scope);
        }
        self.raw = Vec::new();
        self.index.take();
    }

    pub fn forget(&mut self, name: &str) -> bool {
        self.materialise();
        let before = self.entries.len();
        self.entries.retain(|e| e.name != name);
        self.scoped.remove(&sub_scope(name));
        for words in self.scoped.values_mut() {
            words.remove(name);
        }
        self.bindings.retain(|b| b.target != name);
        self.dirty = true;
        self.entries.len() != before
    }

    /// Bindings and the ignore list too: they are answers the database gives,
    /// and leaving them behind made `forget --all` report an empty database
    /// that still corrected things.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.scoped.clear();
        self.raw = Vec::new();
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
            // Weight before recency. Accepting and refusing corrections both
            // leave rows here, so ordinary use fills the table on its own, and
            // sorting on age alone drops a `bind` the user typed by hand ahead
            // of five hundred rows nobody asked for. Losing a pin is worse than
            // it sounds: the word does not stop resolving, it starts resolving
            // somewhere else.
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
        let total: f32 = self.entries.iter().map(|e| e.rank).sum();
        if total <= AGE_CEILING {
            return;
        }
        for entry in &mut self.entries {
            entry.rank *= AGE_FACTOR;
        }
        self.entries.retain(|e| e.rank >= AGE_FLOOR);
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
        // Copied, not re-encoded: a scope nobody read is still exactly the bytes
        // it arrived as, and rebuilding it would undo the point of not decoding.
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

/// Cut rather than written with a wrapped length, which would desync the
/// reader and lose the whole file.
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

    // Stepped over rather than read. `Store::index` walks it again on the first
    // question anyone asks of it, and most runs ask about one scope.
    let scoped_count = reader.count()?;
    let scoped_from = reader.at;
    for _ in 0..scoped_count {
        let _scope = reader.u64()?;
        let len = reader.u16()? as usize;
        reader.take(len)?;
        reader.take(12)?;
    }
    store.raw = bytes[scoped_from..reader.at].to_vec();

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

    // Absent in a version 1 file, which is the point of putting it last.
    store.mode = reader
        .byte()
        .ok()
        .and_then(Mode::from_tag)
        .unwrap_or_default();
    store.enabled = reader.byte().map_or(true, |byte| byte != 0);

    match store.entries.iter().any(|e| !e.rank.is_finite()) {
        true => Err(Broken::Garbled),
        false => Ok(store),
    }
}

/// Closing the file releases the lock, so there is no `Drop` of our own and
/// nothing to unlink.
struct Lock {
    _file: fs::File,
}

impl Lock {
    /// `flock` rather than a sentinel file the first writer creates: the kernel
    /// hands it back when the holder dies, so a killed shell leaves nothing for
    /// the next one to guess about, and there is no unlink for two processes to
    /// race. Waiting outright is only safe because no caller holds it across a
    /// prompt; every one of them commits first and asks afterwards.
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
        held.then_some(Lock { _file: file })
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
