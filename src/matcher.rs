//! Turning a word the shell could not run into a ranked list of words it can.
//!
//! Four ways a typed word can point at a command: it is a prefix of it, it
//! spells out the initials of its hyphenated parts, its letters appear in order
//! inside it, or it is within a typo or two of it. Each produces a similarity on
//! one common scale, and frecency multiplies it — the kind of match is evidence,
//! not a veto. Ranking tiers absolutely instead was a real bug: on a machine
//! with coreutils installed, `gti` resolved to `gtimeout` rather than `git`,
//! because a prefix of anything outranked a transposition of the command the
//! user runs a hundred times a day.

use crate::config::Config;
use crate::store::{frecency, Entry, Shell, Store};

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
    pub tier: Tier,
    pub score: f32,
    /// Frecency alone, for `query --score` and `stats`.
    pub rank: f32,
    /// Edit distance for typo matches, zero for the other tiers.
    pub distance: usize,
    /// The match is a guess rather than a slip: it had to invent or drop
    /// letters, or it is more than one edit away. Those are only used when
    /// nothing solid matched, however often the command gets run.
    speculative: bool,
}

impl Hit {
    pub fn is_speculative(&self) -> bool {
        self.speculative
    }

    /// A shortcut the user set by hand. It is not competing with anything.
    pub fn pinned(name: &str) -> Hit {
        Hit {
            name: name.to_owned(),
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
}

/// Everything the user has run that `query` could plausibly have meant, best
/// first.
pub fn rank(query: &str, ctx: &Context, cfg: &Config) -> Vec<Hit> {
    among(query, &ctx.store.entries, ctx, cfg)
}

/// Rank an explicit candidate list. The store still supplies the bindings, the
/// ignore list and the per-directory ranks; only the pool of names differs.
pub fn among(query: &str, entries: &[Entry], ctx: &Context, cfg: &Config) -> Vec<Hit> {
    let query = query.to_lowercase();
    let mut hits: Vec<Hit> = Vec::new();

    for entry in entries {
        if entry.name == query
            || !entry.kind.usable_in(ctx.shell)
            || ctx.store.is_ignored(&entry.name)
            || ctx.store.buried(&query, &entry.name)
        {
            continue;
        }
        let Some((tier, similarity, distance)) = classify(&query, &entry.name, cfg) else {
            continue;
        };

        let base = frecency(entry.rank, entry.last, ctx.now)
            + cfg.context_weight * ctx.store.dir_rank(ctx.dir, &entry.name);
        // Every correction the user has confirmed for this exact word pulls it
        // further ahead of whatever else the letters could have meant.
        let confirmed = ctx
            .store
            .binding(&query, &entry.name)
            .map_or(0, |b| b.weight.clamp(0, 8)) as f32;

        hits.push(Hit {
            name: entry.name.clone(),
            tier,
            // Two edits away is a different word, not a mistyped one. Without
            // this, twelve uses of `chmod` beat the `chown` that `chwon` is a
            // single transposition from.
            speculative: tier == Tier::Typo
                && (distance > 1 || entry.name.chars().count() != query.chars().count()),
            // Frecency enters logarithmically on purpose. Multiplying by it
            // directly lets a much-used command win from far away, while
            // ignoring it lets an unused binary win on spelling alone.
            // Compressed, it overturns a moderate similarity gap and not a wide
            // one.
            score: (1.0 + base.max(0.0).ln_1p()) * similarity * (1.0 + 0.5 * confirmed),
            rank: base,
            distance,
        });
    }

    sort(&mut hits);
    hits
}

/// Best first: anything solid before any guess, then score.
pub fn sort(hits: &mut [Hit]) {
    hits.sort_by(|a, b| {
        a.speculative
            .cmp(&b.speculative)
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| a.name.len().cmp(&b.name.len()))
            .then_with(|| a.name.cmp(&b.name))
    });
}

/// How `candidate` matches `query`, how strongly on a 0..1 scale, and the edit
/// distance where the match is a typo.
fn classify(query: &str, candidate: &str, cfg: &Config) -> Option<(Tier, f32, usize)> {
    if query.is_empty() || candidate.is_empty() {
        return None;
    }
    let lower = candidate.to_lowercase();
    let (qlen, clen) = (query.chars().count(), lower.chars().count());
    if qlen > clen + cfg.max_typo_distance(qlen) {
        return None;
    }

    if lower.starts_with(query) {
        // Three letters of a twenty-letter binary is weak evidence; most of a
        // short one is strong. The curve has to fall far enough that `gti` finds
        // `git` rather than `gtimeout`.
        let coverage = qlen as f32 / clen as f32;
        return Some((Tier::Prefix, 0.35 + 0.65 * coverage, 0));
    }

    if let Some(quality) = initials_match(query, &lower) {
        return Some((Tier::Initials, quality, 0));
    }

    // A single edit is the likeliest explanation of a near-miss, so it is worth
    // testing before the looser readings: `pintf` is `printf` with a dropped
    // letter, not an abbreviation that happens to appear in order.
    let allowed = cfg.max_typo_distance(qlen);
    let distance = if allowed > 0 {
        edit_distance(query, &lower, allowed)
    } else {
        usize::MAX
    };
    if distance <= 1 {
        return Some((Tier::Typo, typo_quality(distance, qlen, clen), distance));
    }

    if let Some(quality) = subsequence_match(query, &lower) {
        return Some((Tier::Subsequence, quality, 0));
    }
    if distance <= allowed {
        return Some((Tier::Typo, typo_quality(distance, qlen, clen), distance));
    }

    None
}

/// A word of the same length one edit off is a slip of the fingers, and
/// outranks a short prefix of some much longer name: `gti` means `git`, not
/// `gtimeout`. One that also changed length is a guess about a word the user
/// may not have been reaching for at all.
fn typo_quality(distance: usize, qlen: usize, clen: usize) -> f32 {
    let accuracy = 1.0 - distance as f32 / qlen as f32;
    let agrees = if qlen == clen { 1.05 } else { 0.55 };
    accuracy * agrees
}

/// `dc` for `docker-compose`, `gsl` for `git-svn-log`, `p3` for `python3`.
fn initials_match(query: &str, candidate: &str) -> Option<f32> {
    let mut initials = String::new();
    let mut fresh = true;
    for ch in candidate.chars() {
        if matches!(ch, '-' | '_' | '.' | '+') {
            fresh = true;
            continue;
        }
        if fresh || ch.is_ascii_digit() {
            initials.push(ch);
        }
        fresh = false;
    }
    if initials.chars().count() < 2 || !initials.starts_with(query) {
        return None;
    }
    let exact = initials.chars().count() == query.chars().count();
    Some(if exact { 0.62 } else { 0.45 })
}

/// Letters in order, anywhere. A match that starts at the beginning and stays
/// tight scores above one scattered through a long name.
fn subsequence_match(query: &str, candidate: &str) -> Option<f32> {
    let candidate: Vec<char> = candidate.chars().collect();
    let mut at = 0usize;
    let mut first = None;
    let mut boundaries = 0usize;
    let mut matched = 0usize;

    for want in query.chars() {
        let found = candidate[at..].iter().position(|c| *c == want)? + at;
        if first.is_none() {
            first = Some(found);
        }
        if found == 0 || matches!(candidate[found - 1], '-' | '_' | '.' | '+') {
            boundaries += 1;
        }
        matched += 1;
        at = found + 1;
    }

    let span = at - first.unwrap_or(0);
    let density = matched as f32 / span.max(1) as f32;
    let anchored = if first == Some(0) { 0.12 } else { 0.0 };
    let boundary_bonus = 0.10 * (boundaries as f32 / matched as f32);
    Some((0.20 + 0.30 * density + anchored + boundary_bonus).min(0.60))
}

/// Optimal string alignment distance, which unlike plain Levenshtein counts
/// `gti` -> `git` as one edit. Bails out early once the band is exceeded.
pub fn edit_distance(a: &str, b: &str, limit: usize) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > limit {
        return limit + 1;
    }

    let mut prev2 = vec![0usize; b.len() + 1];
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];

    for i in 1..=a.len() {
        curr[0] = i;
        let mut best = curr[0];
        for j in 1..=b.len() {
            let sub = usize::from(a[i - 1] != b[j - 1]);
            let mut cost = (curr[j - 1] + 1).min(prev[j] + 1).min(prev[j - 1] + sub);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                cost = cost.min(prev2[j - 2] + 1);
            }
            curr[j] = cost;
            best = best.min(cost);
        }
        if best > limit {
            return limit + 1;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Kind, Store};

    fn store_with(commands: &[(&str, f32)]) -> Store {
        let mut store = Store::default();
        for (name, rank) in commands {
            store.bump(name, Kind::External, *rank);
        }
        store
    }

    fn best(query: &str, store: &Store) -> Option<String> {
        let cfg = Config::default();
        let ctx = Context {
            store,
            dir: 0,
            shell: None,
            now: crate::store::now(),
        };
        rank(query, &ctx, &cfg).into_iter().next().map(|h| h.name)
    }

    #[test]
    fn prefix_beats_everything_else() {
        let store = store_with(&[("mkdir", 5.0), ("make", 90.0), ("md5sum", 40.0)]);
        assert_eq!(best("mkd", &store).as_deref(), Some("mkdir"));
    }

    #[test]
    fn a_word_that_is_not_a_command_cannot_win() {
        // The user types `clean` all day, but it never becomes a candidate
        // because it never entered the store.
        let store = store_with(&[("clear", 12.0), ("clang", 3.0)]);
        assert_eq!(best("cle", &store).as_deref(), Some("clear"));
    }

    #[test]
    fn transposition_costs_one_edit() {
        let store = store_with(&[("git", 30.0), ("gpg", 1.0)]);
        assert_eq!(best("gti", &store).as_deref(), Some("git"));
        assert_eq!(edit_distance("gti", "git", 2), 1);
    }

    #[test]
    fn initials_resolve_hyphenated_commands() {
        let store = store_with(&[("docker-compose", 8.0), ("dig", 40.0)]);
        assert_eq!(best("dc", &store).as_deref(), Some("docker-compose"));
    }

    #[test]
    fn how_you_match_matters_more_than_how_often_you_run_it() {
        // Equally used: a solid prefix beats the letters merely appearing.
        let store = store_with(&[("dconf", 20.0), ("docker-compose", 20.0)]);
        assert_eq!(best("dco", &store).as_deref(), Some("dconf"));
        // And a deliberate-looking abbreviation beats a thin prefix.
        assert_eq!(best("dc", &store).as_deref(), Some("docker-compose"));

        // Use can overturn the reading, but only by a wide margin. This is what
        // keeps `rmd` off a much-used `rm` and on the `rmdir` nobody runs.
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
        let cfg = Config::default();
        let here = |dir| Context {
            store: &store,
            dir,
            shell: None,
            now: crate::store::now(),
        };
        assert_eq!(rank("ma", &here(7), &cfg)[0].name, "make");
        assert_eq!(rank("ma", &here(99), &cfg)[0].name, "man");
    }

    #[test]
    fn a_bash_function_is_not_offered_inside_fish() {
        let mut store = Store::default();
        store.bump("gs", Kind::Shell(Shell::Bash), 50.0);
        store.bump("gsutil", Kind::External, 1.0);
        let cfg = Config::default();
        let in_fish = Context {
            store: &store,
            dir: 0,
            shell: Some(Shell::Fish),
            now: crate::store::now(),
        };
        assert_eq!(rank("gs", &in_fish, &cfg)[0].name, "gsutil");
    }

    #[test]
    fn distance_bails_out_instead_of_scanning_everything() {
        assert_eq!(edit_distance("abcdef", "zzzzzz", 2), 3);
        assert_eq!(edit_distance("kitten", "sitting", 5), 3);
    }

    /// A plausible shell's worth of history. The second half is there to give
    /// every row below something to lose to: a coreutils `gtimeout` beside
    /// `git`, a `chown` beside `chmod`, a `ctags` beside `cat`. Without them the
    /// table grades uncontested answers and proves nothing.
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
            ("mawk", 3.0),
            ("dconf", 1.0),
        ])
    }

    #[test]
    fn the_resolution_table() {
        let store = realistic();
        let cases: &[(&str, Option<&str>)] = &[
            // prefix truncation
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
            // transposition
            ("gti", Some("git")),
            ("mkidr", Some("mkdir")),
            ("clera", Some("clear")),
            // substitution and doubled or dropped letters
            ("gut", Some("git")),
            ("mkae", Some("make")),
            ("carrgo", Some("cargo")),
            ("dockr-compose", Some("docker-compose")),
            // initials
            ("dc", Some("docker-compose")),
            // subsequence
            ("dkr", Some("docker")),
            ("kbctl", Some("kubectl")),
            // frecency breaking a tie inside one tier
            ("mak", Some("make")),
            ("ma", Some("man")),
            ("g", Some("git")),
            // a word that is not a command cannot be the answer
            ("clean", Some("clear")),
            // competition: the popular command must not win from further away
            ("chwon", Some("chown")),
            ("cta", Some("cat")),
            ("dco", Some("docker-compose")),
            // nothing plausible: say nothing
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

    /// Some words genuinely have two readings — `gitt` is one keystroke from
    /// both `git` and the `gitk` beside it. Picking a winner there would be
    /// pretending; what matters is that the right answer is on offer, which is
    /// what makes the prompt show a menu instead of a yes/no.
    #[test]
    fn a_word_with_two_honest_readings_offers_both() {
        let store = realistic();
        let cfg = Config::default();
        let ctx = Context {
            store: &store,
            dir: 0,
            shell: None,
            now: crate::store::now(),
        };
        for (input, wanted) in [("gitt", "git"), ("gitx", "git"), ("lss", "ls")] {
            let offered: Vec<String> = rank(input, &ctx, &cfg)
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
        // `chwon` is one transposition from `chown` and two substitutions from
        // `chmod`. No amount of running chmod makes it the better reading.
        let store = store_with(&[("chmod", 400.0), ("chown", 0.0)]);
        assert_eq!(best("chwon", &store).as_deref(), Some("chown"));
    }

    #[test]
    fn a_single_edit_is_read_before_a_scattered_abbreviation() {
        // `pintf` is `printf` missing a letter. Its letters also appear in order
        // inside `pinentry-tty`, which is not what anyone meant.
        let store = store_with(&[("pinentry-tty", 300.0), ("printf", 1.0)]);
        assert_eq!(best("pintf", &store).as_deref(), Some("printf"));
        // A genuine abbreviation still reads as one.
        let store = store_with(&[("docker", 5.0), ("kubectl", 5.0)]);
        assert_eq!(best("dkr", &store).as_deref(), Some("docker"));
        assert_eq!(best("kbctl", &store).as_deref(), Some("kubectl"));
    }

    #[test]
    fn distance_beats_popularity_once_it_gets_far_enough() {
        // `mawk` is one edit from `mak`; `git` is enormously more popular and
        // nowhere near it.
        let store = realistic();
        assert_eq!(best("mawq", &store).as_deref(), Some("mawk"));
    }

    /// The one property that has to hold for every input: whatever comes back
    /// was in the candidate set. There is no path that invents a name.
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

        let cfg = Config::default();
        for _ in 0..2_000 {
            let mut store = Store::default();
            let known: Vec<String> = (0..(next() % 12) + 1)
                .map(|_| word((next() % 9 + 1) as usize, &mut next))
                .collect();
            for name in &known {
                store.bump(name, Kind::External, (next() % 50) as f32);
            }
            let query = word((next() % 10) as usize, &mut next);
            let ctx = Context {
                store: &store,
                dir: next(),
                shell: None,
                now: crate::store::now(),
            };
            for hit in rank(&query, &ctx, &cfg) {
                assert!(
                    known.contains(&hit.name),
                    "invented {:?} for {:?}",
                    hit.name,
                    query
                );
            }
        }
    }
}
