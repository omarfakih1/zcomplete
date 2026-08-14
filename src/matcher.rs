//! Runs once per stored command per failed keystroke, so nothing below
//! `among` allocates.

use std::borrow::Cow;

use crate::store::{frecency, Binding, Entry, Kind, Shell, Store};

const CONTEXT_WEIGHT: f32 = 4.0;

const MAX_WORD: usize = 48;

pub fn max_typo_distance(len: usize) -> usize {
    match len {
        0..=2 => 0,
        3..=4 => 1,
        _ => 2,
    }
}

fn lower(word: &str) -> Cow<'_, str> {
    match word.bytes().any(|b| b.is_ascii_uppercase()) {
        true => Cow::Owned(word.to_lowercase()),
        false => Cow::Borrowed(word),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Prefix,
    Initials,
    Subsequence,
    Typo,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Prefix => "prefix",
            Tier::Initials => "initials",
            Tier::Subsequence => "subseq",
            Tier::Typo => "typo",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub name: String,
    pub kind: Kind,
    pub tier: Tier,
    pub score: f32,
    pub rank: f32,
    pub distance: usize,
    speculative: bool,
}

impl Hit {
    pub fn is_speculative(&self) -> bool {
        self.speculative
    }

    pub fn pinned(name: &str, kind: Kind) -> Hit {
        Hit {
            name: name.to_owned(),
            kind,
            tier: Tier::Prefix,
            score: f32::INFINITY,
            rank: 0.0,
            distance: 0,
            speculative: false,
        }
    }
}

pub struct Context<'a> {
    pub store: &'a Store,
    pub dir: u64,
    pub shell: Option<Shell>,
    pub now: u64,
    pub under: Option<&'a str>,
}

impl<'a> Context<'a> {
    pub fn new(store: &'a Store, dir: u64, shell: Option<Shell>) -> Context<'a> {
        Context {
            store,
            dir,
            shell,
            now: crate::store::now(),
            under: None,
        }
    }

    pub fn within(store: &'a Store, parent: &'a str, shell: Option<Shell>) -> Context<'a> {
        Context {
            under: Some(parent),
            ..Context::new(store, 0, shell)
        }
    }

    pub fn learned_as(&self, query: &str) -> String {
        match self.under {
            Some(parent) => format!("{parent} {query}"),
            None => query.to_owned(),
        }
    }
}

/// A thing worth scoring, borrowed. The names off `PATH` are slices of one
/// cached buffer, and wrapping each in an owned `Entry` to score it cost more
/// than the scoring did.
pub struct Candidate<'a> {
    pub name: &'a str,
    pub kind: Kind,
    pub rank: f32,
    pub last: u64,
}

impl<'a> From<&'a Entry> for Candidate<'a> {
    fn from(entry: &'a Entry) -> Candidate<'a> {
        Candidate {
            name: &entry.name,
            kind: entry.kind,
            rank: entry.rank,
            last: entry.last,
        }
    }
}

pub fn rank(query: &str, ctx: &Context) -> Vec<Hit> {
    among(query, ctx.store.entries.iter().map(Candidate::from), ctx)
}

/// The query, measured once. Its length, its budget and the letters it uses
/// were being worked out again for every command in the store, which on the
/// fallback sweep is every name on `PATH`.
struct Probe<'a> {
    text: &'a str,
    len: usize,
    allowed: usize,
    letters: u32,
    opens_with: Option<char>,
}

/// The bit each byte stands for. Everything outside `a-z0-9` and the separators
/// shares the last one, and two letters that share a bit read as the same
/// letter - so a word can look more like a candidate than it is, never less.
/// The test built on this may only ever be allowed to keep a candidate it
/// should have dropped.
static LETTER_BIT: [u32; 256] = {
    let mut table = [1u32 << 31; 256];
    let mut byte = 0usize;
    while byte < 256 {
        let bit = match byte as u8 {
            b'a'..=b'z' => byte as u32 - b'a' as u32,
            b'A'..=b'Z' => byte as u32 - b'A' as u32,
            b'0'..=b'9' => 26,
            b'-' => 27,
            b'_' => 28,
            b'.' => 29,
            b'+' => 30,
            _ => 31,
        };
        table[byte] = 1 << bit;
        byte += 1;
    }
    table
};

/// One bit per letter used. A table rather than a `match`, because this runs
/// over every byte of every name in the store for every word that fails.
fn letters_of(word: &str) -> u32 {
    word.bytes()
        .fold(0u32, |letters, byte| letters | LETTER_BIT[byte as usize])
}

impl<'a> Probe<'a> {
    fn new(text: &'a str) -> Probe<'a> {
        let len = text.chars().count();
        Probe {
            text,
            len,
            allowed: max_typo_distance(len),
            letters: letters_of(text),
            opens_with: text.chars().next(),
        }
    }

    /// How many of the query's letters the candidate does not have at all.
    /// Cheaper than any of the four tests it stands in front of, and it answers
    /// both of the questions those tests would have had to walk the word to
    /// answer: past the typo budget nothing can match, and above zero a prefix,
    /// a run of initials and a subsequence are all already out - each of those
    /// needs every letter, wherever it sits.
    fn missing_from(&self, candidate: &str) -> usize {
        (self.letters & !letters_of(candidate)).count_ones() as usize
    }
}

pub fn among<'a>(
    query: &str,
    entries: impl Iterator<Item = Candidate<'a>>,
    ctx: &Context,
) -> Vec<Hit> {
    let query = lower(query);
    let probe = Probe::new(&query);
    let key = ctx.learned_as(&query);
    let learned: Vec<&Binding> = ctx
        .store
        .bindings
        .iter()
        .filter(|b| b.input == key)
        .collect();
    let here = ctx.store.ranker(ctx.dir, ctx.now);
    let ignored = !ctx.store.ignored.is_empty();
    let mut hits: Vec<Hit> = Vec::new();

    for entry in entries {
        if entry.name == query.as_ref() || !entry.kind.usable_in(ctx.shell) {
            continue;
        }
        if ignored && ctx.under.is_none() && ctx.store.is_ignored(entry.name) {
            continue;
        }
        let Some(found) = classify(&probe, entry.name) else {
            continue;
        };
        let said = learned.iter().find(|b| b.target == entry.name);
        if said.is_some_and(|b| b.weight <= crate::store::BURIED_AT) {
            continue;
        }

        let mut base = frecency(entry.rank, entry.last, ctx.now);
        if ctx.under.is_none() {
            base += CONTEXT_WEIGHT * here(entry.name);
        }
        let confirmed = said.map_or(0, |b| b.weight.clamp(0, 8)) as f32;

        hits.push(Hit {
            name: entry.name.to_owned(),
            kind: entry.kind,
            tier: found.tier,
            speculative: found.speculative,
            // Logarithmic on purpose: multiplied in directly, a much-used command wins
            // from far away; left out, an unused binary wins on spelling alone.
            score: (1.0 + base.max(0.0).ln_1p()) * found.similarity * (1.0 + 0.5 * confirmed),
            rank: base,
            distance: found.distance,
        });
    }

    sort(&mut hits);
    hits
}

/// Score, not tier: ordering by tier makes a prefix of anything outrank a
/// transposition of the command you run all day, and `gti` becomes `gtimeout`.
pub fn sort(hits: &mut [Hit]) {
    hits.sort_by(|a, b| {
        a.speculative
            .cmp(&b.speculative)
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// What one candidate turned out to be, once the query had been spelled
/// against it.
struct Match {
    tier: Tier,
    similarity: f32,
    distance: usize,
    speculative: bool,
}

impl Match {
    fn sure(tier: Tier, similarity: f32) -> Match {
        Match {
            tier,
            similarity,
            distance: 0,
            speculative: false,
        }
    }
}

fn classify(probe: &Probe, candidate: &str) -> Option<Match> {
    if probe.text.is_empty() || candidate.is_empty() {
        return None;
    }
    // Bytes are never fewer than the letters they spell, so a candidate too
    // short by this count is too short by any count. Before the scans, because
    // it is the only test here that reads none of the word.
    if probe.len > candidate.len() + probe.allowed {
        return None;
    }
    let missing = probe.missing_from(candidate);
    if missing > probe.allowed {
        return None;
    }
    let lower = lower(candidate);
    let clen = count(&lower);
    let qlen = probe.len;
    if qlen > clen + probe.allowed {
        return None;
    }
    // Every letter accounted for, so the three tiers that ask where the letters
    // sit are worth asking. One letter short and they cannot match however they
    // are arranged, and a typo is the only reading left.
    let spelled = missing == 0;

    if spelled && lower.starts_with(probe.text) {
        return Some(Match::sure(
            Tier::Prefix,
            0.35 + 0.65 * (qlen as f32 / clen as f32),
        ));
    }

    if spelled {
        if let Some(quality) = initials_match(probe.text, &lower) {
            return Some(Match::sure(Tier::Initials, quality));
        }
    }

    // A two-letter word gets no typo budget, because one substitution reaches a
    // dozen real commands. Its one transposition is not a guess: both letters
    // are the ones you typed, and there is only ever one candidate. Scored at
    // the floor so a deliberate abbreviation still reads first.
    if probe.allowed == 0 && transposed(probe.text, &lower) {
        return Some(Match {
            tier: Tier::Typo,
            similarity: 0.35,
            distance: 1,
            speculative: false,
        });
    }
    let distance = match probe.allowed {
        0 => usize::MAX,
        allowed => edit_distance(probe.text, &lower, allowed),
    };
    let typo = |distance| {
        Match {
            tier: Tier::Typo,
            similarity: typo_quality(
                distance,
                qlen,
                clen,
                probe.opens_with == lower.chars().next(),
            ),
            distance,
            // Two edits away is a different word, or twelve uses of `chmod` beat
            // the `chown` behind `chwon`. One edit is read by which way the
            // length went, because the two are not the same mistake. A letter
            // short is a key missed, and the longer name is the only word it
            // could have been: `ocker` is `docker`. A letter over is a word you
            // typed more of than the candidate has, and cutting it down to a
            // shorter command is a guess - which is what keeps `rmd` on `rmdir`
            // rather than on `rm`, where being wrong deletes something. A
            // doubled letter is the exception both ways: every letter you meant
            // is still there, in order.
            speculative: distance > 1 || (qlen > clen && !stutter(probe.text, &lower)),
        }
    };
    if distance <= 1 {
        return Some(typo(distance));
    }
    if spelled {
        if let Some(quality) = subsequence_match(probe.text, &lower) {
            return Some(Match::sure(Tier::Subsequence, quality));
        }
    }
    (distance <= probe.allowed).then(|| typo(distance))
}

/// Characters, without walking the string when the bytes already are the
/// characters. Command names are ASCII almost to a name.
fn count(word: &str) -> usize {
    match word.is_ascii() {
        true => word.len(),
        false => word.chars().count(),
    }
}

/// `lls` for `ls`, `gss` for `gs`. Holding a key a beat too long is the ordinary
/// typo, and unlike a deletion in general it leaves every letter you meant where
/// you put it, so frecency is allowed to decide.
fn stutter(query: &str, candidate: &str) -> bool {
    let mut letters = query.char_indices().peekable();
    while let Some((at, ch)) = letters.next() {
        let Some(&(after, again)) = letters.peek() else {
            return false;
        };
        if ch == again
            && candidate.starts_with(&query[..at])
            && candidate.get(at..) == query.get(after..)
        {
            return true;
        }
    }
    false
}

/// `sl` for `ls`, `vm` for `mv`. Two letters, both right, in the wrong order.
fn transposed(query: &str, candidate: &str) -> bool {
    let (mut typed, mut real) = (query.chars(), candidate.chars());
    match (typed.next(), typed.next(), typed.next()) {
        (Some(a), Some(b), None) => {
            a != b && (real.next(), real.next(), real.next()) == (Some(b), Some(a), None)
        }
        _ => false,
    }
}

fn typo_quality(distance: usize, qlen: usize, clen: usize, starts_alike: bool) -> f32 {
    let accuracy = 1.0 - distance as f32 / qlen as f32;
    // Same length is a substitution or a swap, and every letter meant is there.
    // One letter out, by one edit, is a single slip as well - a key missed or a
    // key hit twice - and reading it as one guess among many was the whole of
    // why `ocker` found `docker-compose` instead of `docker`. Further apart
    // than that and the word really is more guess than typo.
    let agrees = match clen.abs_diff(qlen) {
        0 => 1.05,
        1 if distance == 1 => 0.75,
        _ => 0.55,
    };
    let opening = if starts_alike || qlen != clen {
        1.0
    } else {
        0.82
    };
    accuracy * agrees * opening
}

fn initials_match(query: &str, candidate: &str) -> Option<f32> {
    let mut want = query.chars();
    let mut expect = want.next();
    let (mut initials, mut used) = (0usize, 0usize);
    let mut fresh = true;
    for ch in candidate.chars() {
        if matches!(ch, '-' | '_' | '.' | '+') {
            fresh = true;
            continue;
        }
        if fresh || ch.is_ascii_digit() {
            initials += 1;
            if let Some(want_ch) = expect {
                if want_ch != ch {
                    return None;
                }
                used += 1;
                expect = want.next();
            }
        }
        fresh = false;
    }
    if expect.is_some() || initials < 2 {
        return None;
    }
    Some(if initials == used { 0.62 } else { 0.45 })
}

fn subsequence_match(query: &str, candidate: &str) -> Option<f32> {
    let mut want = query.chars().peekable();
    let (mut first, mut end, mut matched, mut boundaries) = (usize::MAX, 0usize, 0usize, 0usize);
    let (mut prev, mut length) = ('\0', 0usize);
    for (i, ch) in candidate.chars().enumerate() {
        length = i + 1;
        if want.peek() == Some(&ch) {
            want.next();
            if first == usize::MAX {
                first = i;
            }
            if i == 0 || matches!(prev, '-' | '_' | '.' | '+') {
                boundaries += 1;
            }
            matched += 1;
            end = i + 1;
        }
        prev = ch;
    }
    if want.peek().is_some() || matched == 0 {
        return None;
    }

    let density = matched as f32 / (end - first).max(1) as f32;
    // How much of the word the letters actually account for, as well as how
    // tightly they sit. Density alone reads a run buried in a long name as
    // perfect - `iff` is flawlessly dense inside `ifconfig` - and that beat the
    // one dropped letter that makes it `diff`.
    let coverage = matched as f32 / length.max(1) as f32;
    let anchored = if first == 0 { 0.12 } else { 0.0 };
    let boundary_bonus = 0.10 * (boundaries as f32 / matched as f32);
    Some((0.20 + 0.15 * density + 0.18 * coverage + anchored + boundary_bonus).min(0.60))
}

/// Optimal string alignment, so `gti` -> `git` is one edit, not two.
pub fn edit_distance(a: &str, b: &str, limit: usize) -> usize {
    // Bytes when the bytes are the letters, which for command names is all but
    // always: the copy into a buffer of `char` was the whole cost of the short
    // words this is called on.
    if a.is_ascii() && b.is_ascii() {
        return between(a.as_bytes(), b.as_bytes(), limit);
    }
    let (mut abuf, mut bbuf) = ([' '; MAX_WORD], [' '; MAX_WORD]);
    let (Some(a), Some(b)) = (fill(&mut abuf, a), fill(&mut bbuf, b)) else {
        return limit + 1;
    };
    between(a, b, limit)
}

fn between<T: Copy + PartialEq>(a: &[T], b: &[T], limit: usize) -> usize {
    if a.len() > MAX_WORD || b.len() > MAX_WORD || a.len().abs_diff(b.len()) > limit {
        return limit + 1;
    }

    let cap = limit.min(u8::MAX as usize) as u8;
    let mut prev2 = [0u8; MAX_WORD + 1];
    let mut prev = [0u8; MAX_WORD + 1];
    let mut curr = [0u8; MAX_WORD + 1];
    for (j, cell) in prev.iter_mut().enumerate().take(b.len() + 1) {
        *cell = j as u8;
    }

    for i in 1..=a.len() {
        curr[0] = i as u8;
        let mut best = curr[0];
        for j in 1..=b.len() {
            let sub = u8::from(a[i - 1] != b[j - 1]);
            let mut cost = (curr[j - 1] + 1).min(prev[j] + 1).min(prev[j - 1] + sub);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                cost = cost.min(prev2[j - 2] + 1);
            }
            curr[j] = cost;
            best = best.min(cost);
        }
        if best > cap {
            return limit + 1;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()] as usize
}

fn fill<'a>(buf: &'a mut [char; MAX_WORD], word: &str) -> Option<&'a [char]> {
    let mut len = 0;
    for ch in word.chars() {
        *buf.get_mut(len)? = ch;
        len += 1;
    }
    Some(&buf[..len])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    fn store_with(commands: &[(&str, f32)]) -> Store {
        let mut store = Store::default();
        for (name, rank) in commands {
            store.bump(name, Kind::External, *rank);
        }
        store
    }

    fn best(query: &str, store: &Store) -> Option<String> {
        let ctx = Context::new(store, 0, None);
        rank(query, &ctx).into_iter().next().map(|h| h.name)
    }

    #[test]
    fn a_word_that_is_not_a_command_cannot_win() {
        let store = store_with(&[("clear", 12.0), ("clang", 3.0)]);
        assert_eq!(best("cle", &store).as_deref(), Some("clear"));
    }

    #[test]
    fn how_you_match_matters_more_than_how_often_you_run_it() {
        let store = store_with(&[("dconf", 20.0), ("docker-compose", 20.0)]);
        assert_eq!(best("dco", &store).as_deref(), Some("dconf"));
        assert_eq!(best("dc", &store).as_deref(), Some("docker-compose"));

        let store = store_with(&[("rm", 400.0), ("rmdir", 1.0)]);
        assert_eq!(best("rmd", &store).as_deref(), Some("rmdir"));

        let store = store_with(&[("dconf", 1.0), ("docker-compose", 900.0)]);
        assert_eq!(best("dc", &store).as_deref(), Some("docker-compose"));
    }

    #[test]
    fn frecency_breaks_ties_inside_a_tier() {
        let mut store = store_with(&[("cargo", 2.0), ("cat", 2.0)]);
        store.bump("cat", Kind::External, 40.0);
        assert_eq!(best("ca", &store).as_deref(), Some("cat"));
        store.bump("cargo", Kind::External, 400.0);
        assert_eq!(best("ca", &store).as_deref(), Some("cargo"));
    }

    #[test]
    fn buried_corrections_stop_being_offered() {
        let mut store = store_with(&[("rm", 50.0), ("rmdir", 4.0)]);
        store.nudge_binding("rmd", "rm", crate::store::BURIED_AT);
        assert_eq!(best("rmd", &store).as_deref(), Some("rmdir"));
    }

    #[test]
    fn ignored_commands_are_never_offered() {
        let mut store = store_with(&[("sl", 50.0)]);
        assert_eq!(best("sla", &store).as_deref(), Some("sl"));
        store.ignore("sl");
        assert_eq!(best("sla", &store).as_deref(), None);
    }

    #[test]
    fn context_lifts_a_command_used_in_this_directory() {
        let mut store = store_with(&[("man", 20.0), ("make", 20.0)]);
        for _ in 0..10 {
            store.bump_in(7, "make", 1.0);
        }
        let here = |dir| Context::new(&store, dir, None);
        assert_eq!(rank("ma", &here(7))[0].name, "make");
        assert_eq!(rank("ma", &here(99))[0].name, "man");
    }

    #[test]
    fn a_bash_function_is_not_offered_inside_fish() {
        let mut store = Store::default();
        store.bump("gs", Kind::Shell(Shell::Bash), 50.0);
        store.bump("gsutil", Kind::External, 1.0);
        let in_fish = Context::new(&store, 0, Some(Shell::Fish));
        assert_eq!(rank("gs", &in_fish)[0].name, "gsutil");
    }

    #[test]
    fn distance_bails_out_instead_of_scanning_everything() {
        assert_eq!(edit_distance("abcdef", "zzzzzz", 2), 3);
        assert_eq!(edit_distance("kitten", "sitting", 5), 3);
        assert_eq!(edit_distance("gti", "git", 2), 1);
        assert_eq!(edit_distance(&"x".repeat(80), &"x".repeat(80), 2), 3);
    }

    fn realistic() -> Store {
        store_with(&[
            ("git", 400.0),
            ("ls", 380.0),
            ("cd", 300.0),
            ("cargo", 120.0),
            ("mkdir", 60.0),
            ("clear", 55.0),
            ("cat", 50.0),
            ("make", 45.0),
            ("man", 40.0),
            ("grep", 38.0),
            ("curl", 30.0),
            ("docker-compose", 25.0),
            ("docker", 30.0),
            ("kubectl", 20.0),
            ("python3", 18.0),
            ("mawk", 3.0),
            ("gpg", 3.0),
            ("chmod", 12.0),
            ("rmdir", 4.0),
            ("rm", 40.0),
            ("clang", 6.0),
            ("md5sum", 5.0),
            ("gcloud", 8.0),
            ("gtimeout", 0.5),
            ("gitk", 0.5),
            ("git-lfs", 0.5),
            ("chown", 0.5),
            ("ctags", 0.5),
            ("less", 2.0),
            ("dconf", 1.0),
        ])
    }

    #[test]
    fn the_resolution_table() {
        let store = realistic();
        let cases: &[(&str, Option<&str>)] = &[
            ("mkd", Some("mkdir")),
            ("cle", Some("clear")),
            ("carg", Some("cargo")),
            ("kubect", Some("kubectl")),
            ("doc", Some("docker")),
            ("py", Some("python3")),
            ("gre", Some("grep")),
            ("cur", Some("curl")),
            ("chm", Some("chmod")),
            ("rmd", Some("rmdir")),
            ("gti", Some("git")),
            ("mkidr", Some("mkdir")),
            ("clera", Some("clear")),
            ("gut", Some("git")),
            ("mkae", Some("make")),
            ("carrgo", Some("cargo")),
            ("dockr-compose", Some("docker-compose")),
            ("dc", Some("docker-compose")),
            ("dkr", Some("docker")),
            ("kbctl", Some("kubectl")),
            ("mak", Some("make")),
            ("ma", Some("man")),
            ("g", Some("git")),
            ("clean", Some("clear")),
            ("chwon", Some("chown")),
            ("cta", Some("cat")),
            ("dco", Some("docker-compose")),
            ("mawq", Some("mawk")),
            ("mkdr", Some("mkdir")),
            ("sl", Some("ls")),
            ("dc", Some("docker-compose")),
            ("lls", Some("ls")),
            ("gitt", Some("git")),
            ("catt", Some("cat")),
            ("rmd", Some("rmdir")),
            ("mkdirr", Some("mkdir")),
            ("qwzzxv", None),
            ("zzzzzzzz", None),
            ("xyzzy", None),
            ("hjkl", None),
            ("qqq", None),
            ("wumpus", None),
            ("nonexistentcommand", None),
        ];

        let mut wrong = Vec::new();
        for (input, want) in cases {
            let got = best(input, &store);
            if got.as_deref() != *want {
                wrong.push(format!("{input}: wanted {want:?}, got {got:?}"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }

    #[test]
    fn a_word_with_two_honest_readings_offers_both() {
        let store = realistic();
        let ctx = Context::new(&store, 0, None);
        for (input, wanted) in [("gitt", "git"), ("gitx", "git"), ("lss", "ls")] {
            let offered: Vec<String> = rank(input, &ctx)
                .into_iter()
                .take(3)
                .map(|hit| hit.name)
                .collect();
            assert!(
                offered.iter().any(|name| name == wanted),
                "{input} should still offer {wanted}, offered {offered:?}"
            );
        }
    }

    #[test]
    fn a_word_two_edits_away_never_wins_over_one_a_single_edit_away() {
        let store = store_with(&[("chmod", 400.0), ("chown", 0.0)]);
        assert_eq!(best("chwon", &store).as_deref(), Some("chown"));
    }

    #[test]
    fn a_single_edit_is_read_before_a_scattered_abbreviation() {
        let store = store_with(&[("pinentry-tty", 300.0), ("printf", 1.0)]);
        assert_eq!(best("pintf", &store).as_deref(), Some("printf"));
        let store = store_with(&[("docker", 5.0), ("kubectl", 5.0)]);
        assert_eq!(best("dkr", &store).as_deref(), Some("docker"));
        assert_eq!(best("kbctl", &store).as_deref(), Some("kubectl"));
    }

    /// The keys either side of one, on a US QWERTY board. Typos are a hand
    /// missing by a key far more often than they are a random letter, so a
    /// corpus built from anything else measures a mistake nobody makes.
    fn neighbours(ch: char) -> &'static str {
        match ch {
            'q' => "wa",
            'w' => "qes",
            'e' => "wrd",
            'r' => "etf",
            't' => "ryg",
            'y' => "tuh",
            'u' => "yij",
            'i' => "uok",
            'o' => "ipl",
            'p' => "ol",
            'a' => "qsz",
            's' => "awdx",
            'd' => "sefc",
            'f' => "drgv",
            'g' => "fthb",
            'h' => "gyjn",
            'j' => "hukm",
            'k' => "jil",
            'l' => "kop",
            'z' => "asx",
            'x' => "zsdc",
            'c' => "xdfv",
            'v' => "cfgb",
            'b' => "vghn",
            'n' => "bhjm",
            'm' => "njk",
            '-' => "0p",
            '.' => ",l",
            '0' => "9-",
            '1' => "2",
            '2' => "13",
            '3' => "24",
            '5' => "46",
            '6' => "57",
            '9' => "80",
            _ => "",
        }
    }

    /// The commands a working machine actually has, most-used first.
    const CORPUS: &[&str] = &[
        "git",
        "ls",
        "cd",
        "cat",
        "grep",
        "make",
        "cargo",
        "docker",
        "kubectl",
        "python3",
        "node",
        "npm",
        "curl",
        "wget",
        "ssh",
        "scp",
        "rsync",
        "tar",
        "unzip",
        "chmod",
        "chown",
        "mkdir",
        "rmdir",
        "rm",
        "cp",
        "mv",
        "find",
        "sed",
        "awk",
        "sort",
        "uniq",
        "head",
        "tail",
        "less",
        "more",
        "man",
        "ps",
        "kill",
        "top",
        "df",
        "du",
        "mount",
        "ping",
        "dig",
        "systemctl",
        "journalctl",
        "apt",
        "brew",
        "vim",
        "nano",
        "tmux",
        "screen",
        "htop",
        "jq",
        "tree",
        "which",
        "whoami",
        "echo",
        "printf",
        "touch",
        "ln",
        "stat",
        "diff",
        "patch",
        "gcc",
        "clang",
        "rustc",
        "java",
        "ruby",
        "perl",
        "php",
        "sqlite3",
        "psql",
        "mysql",
        "redis-cli",
        "docker-compose",
        "terraform",
        "ansible",
        "helm",
        "gh",
        "git-lfs",
        "gpg",
        "openssl",
        "base64",
        "md5sum",
        "sha256sum",
        "xargs",
        "tee",
        "watch",
        "env",
        "history",
        "gzip",
        "bzip2",
        "zcat",
        "ifconfig",
        "netstat",
        "traceroute",
    ];

    fn corpus_store() -> Store {
        let mut store = Store::default();
        for (i, name) in CORPUS.iter().enumerate() {
            // Zipf-ish, so the head of the list is the head of the day.
            store.bump(name, Kind::External, 400.0 / (i + 1) as f32);
        }
        store
    }

    /// Every typo one slip of the hand makes of `name`, at every position.
    fn slips(name: &str) -> Vec<String> {
        let letters: Vec<char> = name.chars().collect();
        let mut out = Vec::new();
        let mut put = |word: String| {
            if word.len() > 1 && !CORPUS.contains(&word.as_str()) {
                out.push(word);
            }
        };
        for i in 0..letters.len() {
            let mut hit_next_door = letters.clone();
            if let Some(near) = neighbours(letters[i]).chars().next() {
                hit_next_door[i] = near;
                put(hit_next_door.iter().collect());
            }
            let mut dropped = letters.clone();
            dropped.remove(i);
            put(dropped.iter().collect());
            let mut doubled = letters.clone();
            doubled.insert(i, letters[i]);
            put(doubled.iter().collect());
            if i + 1 < letters.len() && letters[i] != letters[i + 1] {
                let mut swapped = letters.clone();
                swapped.swap(i, i + 1);
                put(swapped.iter().collect());
            }
        }
        out
    }

    /// Share of slips whose command comes back first, and within the first
    /// three. A slip that reads as some other real command is left out: there
    /// is no right answer to measure there.
    fn recovery(store: &Store) -> (f32, f32, usize) {
        let ctx = Context::new(store, 0, None);
        let (mut first, mut near, mut total) = (0usize, 0usize, 0usize);
        for name in CORPUS {
            for typo in slips(name) {
                let offered = rank(&typo, &ctx);
                total += 1;
                if offered.first().is_some_and(|hit| &hit.name == name) {
                    first += 1;
                }
                if offered.iter().take(3).any(|hit| &hit.name == name) {
                    near += 1;
                }
            }
        }
        (
            first as f32 / total as f32,
            near as f32 / total as f32,
            total,
        )
    }

    #[test]
    #[ignore = "a listing, not a check: cargo test --release -- --ignored --nocapture"]
    fn which_slips_it_reads_wrong() {
        let store = corpus_store();
        let ctx = Context::new(&store, 0, None);
        for name in CORPUS {
            for typo in slips(name) {
                let offered = rank(&typo, &ctx);
                let got = offered.first().map(|hit| hit.name.as_str());
                if got != Some(name) {
                    let rest: Vec<&str> = offered.iter().take(3).map(|h| h.name.as_str()).collect();
                    println!("{typo:<16} meant {name:<14} got {rest:?}");
                }
            }
        }
    }

    #[test]
    fn a_slip_of_the_hand_lands_on_the_command_that_was_meant() {
        let (first, near, total) = recovery(&corpus_store());
        println!(
            "top1 {:.4} ({} missed)  top3 {:.4} ({} missed)  over {total} slips",
            first,
            total - (first * total as f32).round() as usize,
            near,
            total - (near * total as f32).round() as usize,
        );
        // A floor just under where it stands, not a target. Two words a letter
        // apart are two words, and no ranking gets all of those right - what is
        // left wrong here is mostly `dd` for `df` and `hq` for `jq`, which have
        // no right answer. What this catches is a change that quietly trades
        // away the ones it does get.
        assert!(first >= 0.955, "top-1 recovery fell to {first:.4}");
        assert!(near >= 0.985, "top-3 recovery fell to {near:.4}");
    }

    #[test]
    #[ignore = "a measurement, not a check: cargo test --release -- --ignored --nocapture"]
    fn how_long_one_failed_word_takes() {
        use std::time::Instant;
        // The fallback sweep scores every name on `PATH`, so the worst case is
        // a word that matches nothing and is spelled against all of them. A
        // word shorter than MIN_INPUT never gets here, so none is measured.
        let mut store = corpus_store();
        for i in 0..2_000 {
            store.bump(&format!("pkg-tool-{i:04}"), Kind::External, 1.0);
        }
        let ctx = Context::new(&store, 0, None);
        let words = [
            "gti",
            "dcoker",
            "kubctl",
            "qwzzxv",
            "systemclt",
            "mkidr",
            "gs",
        ];
        let rounds = 200;
        let mut worst = 0f64;
        let mut total = 0f64;
        for word in words {
            let start = Instant::now();
            let mut kept = 0usize;
            for _ in 0..rounds {
                kept += rank(word, &ctx).len();
            }
            let each = start.elapsed().as_secs_f64() / rounds as f64;
            total += each;
            worst = worst.max(each);
            println!(
                "  {word:<10} {:>7.1} us  {:>5.1} ns/candidate  {} hits",
                each * 1e6,
                each * 1e9 / store.entries.len() as f64,
                kept / rounds,
            );
        }
        println!(
            "mean {:.1} us, worst {:.1} us, over {} candidates",
            total * 1e6 / words.len() as f64,
            worst * 1e6,
            store.entries.len(),
        );
    }

    #[test]
    fn a_suggestion_is_always_something_the_store_knows() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet = b"abcdefgim-k_rtz3";
        let word = |len: usize, next: &mut dyn FnMut() -> u64| {
            (0..len)
                .map(|_| alphabet[(next() % alphabet.len() as u64) as usize] as char)
                .collect::<String>()
        };

        for _ in 0..2_000 {
            let mut store = Store::default();
            let known: Vec<String> = (0..(next() % 12) + 1)
                .map(|_| word((next() % 9 + 1) as usize, &mut next))
                .collect();
            for name in &known {
                store.bump(name, Kind::External, (next() % 50) as f32);
            }
            let query = word((next() % 10) as usize, &mut next);
            let ctx = Context::new(&store, next(), None);
            for hit in rank(&query, &ctx) {
                assert!(known.contains(&hit.name), "invented {:?}", hit.name);
            }
        }
    }
}
