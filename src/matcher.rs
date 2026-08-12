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

pub fn among<'a>(
    query: &str,
    entries: impl Iterator<Item = Candidate<'a>>,
    ctx: &Context,
) -> Vec<Hit> {
    let query = lower(query);
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
        let said = learned.iter().find(|b| b.target == entry.name);
        if said.is_some_and(|b| b.weight <= crate::store::BURIED_AT) {
            continue;
        }
        let Some((tier, similarity, distance)) = classify(&query, entry.name) else {
            continue;
        };

        let mut base = frecency(entry.rank, entry.last, ctx.now);
        if ctx.under.is_none() {
            base += CONTEXT_WEIGHT * here(entry.name);
        }
        let confirmed = said.map_or(0, |b| b.weight.clamp(0, 8)) as f32;

        hits.push(Hit {
            name: entry.name.to_owned(),
            kind: entry.kind,
            tier,
            // Two edits away is a different word, or twelve uses of `chmod` beat the
            // `chown` behind `chwon`. A length change is usually a guess too, which
            // is what keeps `rmd` on `rmdir` rather than on `rm`. A doubled letter
            // is the exception: every letter you meant is still there.
            speculative: tier == Tier::Typo
                && (distance > 1
                    || (entry.name.chars().count() != query.chars().count()
                        && !stutter(&query, &lower(entry.name)))),
            // Logarithmic on purpose: multiplied in directly, a much-used command wins
            // from far away; left out, an unused binary wins on spelling alone.
            score: (1.0 + base.max(0.0).ln_1p()) * similarity * (1.0 + 0.5 * confirmed),
            rank: base,
            distance,
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

fn classify(query: &str, candidate: &str) -> Option<(Tier, f32, usize)> {
    if query.is_empty() || candidate.is_empty() {
        return None;
    }
    let lower = lower(candidate);
    let (qlen, clen) = (query.chars().count(), lower.chars().count());
    if qlen > clen + max_typo_distance(qlen) {
        return None;
    }

    if lower.starts_with(query) {
        return Some((Tier::Prefix, 0.35 + 0.65 * (qlen as f32 / clen as f32), 0));
    }

    if let Some(quality) = initials_match(query, &lower) {
        return Some((Tier::Initials, quality, 0));
    }

    let allowed = max_typo_distance(qlen);
    // A two-letter word gets no typo budget, because one substitution reaches a
    // dozen real commands. Its one transposition is not a guess: both letters
    // are the ones you typed, and there is only ever one candidate. Scored at
    // the floor so a deliberate abbreviation still reads first.
    if allowed == 0 && transposed(query, &lower) {
        return Some((Tier::Typo, 0.35, 1));
    }
    let distance = match allowed {
        0 => usize::MAX,
        _ => edit_distance(query, &lower, allowed),
    };
    let quality = |distance| {
        typo_quality(
            distance,
            qlen,
            clen,
            query.chars().next() == lower.chars().next(),
        )
    };
    if distance <= 1 {
        return Some((Tier::Typo, quality(distance), distance));
    }
    if let Some(quality) = subsequence_match(query, &lower) {
        return Some((Tier::Subsequence, quality, 0));
    }
    (distance <= allowed).then(|| (Tier::Typo, quality(distance), distance))
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
    let agrees = if qlen == clen { 1.05 } else { 0.55 };
    let opening = if starts_alike { 1.0 } else { 0.82 };
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
    let mut prev = '\0';
    for (i, ch) in candidate.chars().enumerate() {
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
    let anchored = if first == 0 { 0.12 } else { 0.0 };
    let boundary_bonus = 0.10 * (boundaries as f32 / matched as f32);
    Some((0.20 + 0.30 * density + anchored + boundary_bonus).min(0.60))
}

/// Optimal string alignment, so `gti` -> `git` is one edit, not two.
pub fn edit_distance(a: &str, b: &str, limit: usize) -> usize {
    let (mut abuf, mut bbuf) = ([' '; MAX_WORD], [' '; MAX_WORD]);
    let (Some(a), Some(b)) = (fill(&mut abuf, a), fill(&mut bbuf, b)) else {
        return limit + 1;
    };
    if a.len().abs_diff(b.len()) > limit {
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
