//! Everything the shell hooks call. `admin` is what you type.

use std::io::Write;

use crate::matcher::{self, Context};
use crate::store::{self, Kind, Mode, Shell, Store};
use crate::{at, safety, shell, term};
use crate::{flag_value, reject_unknown, split_flags};
use crate::{Fail, DECLINED, DEFERRED, DISABLED, FOUND, NO_MATCH};

const MIN_INPUT: usize = 2;
const MAX_CANDIDATES: usize = 5;
const AMBIGUITY: f32 = 0.75;
const PROBES: usize = 64;

pub(crate) fn resolve(args: &[String]) -> Result<i32, Fail> {
    let (flags, operands) = split_flags(args);
    reject_unknown(&flags, &["--shell", "--subshell"])?;
    let shell = flag_value(&flags, "--shell").and_then(|s| Shell::parse(&s));
    let Some(word) = operands.first() else {
        return Ok(NO_MATCH);
    };

    let alone = Caller {
        line: None,
        borrowed: false,
        subshell: flags.iter().any(|f| f == "--subshell"),
    };
    match decide(word, &operands[1..], shell, alone)? {
        // Line two, when there is one, is `verb <word>`: the shell splices it
        // over its own first argument.
        Outcome::Run(fixed) => {
            let mut out = std::io::stdout();
            writeln!(out, "{}", fixed.word)?;
            if let Some(verb) = fixed.verb {
                writeln!(out, "verb {verb}")?;
            }
            out.flush()?;
            Ok(FOUND)
        }
        Outcome::Nothing => Ok(NO_MATCH),
        Outcome::Declined => Ok(DECLINED),
        Outcome::Disabled => Ok(DISABLED),
        Outcome::Deferred => Ok(DEFERRED),
    }
}

pub(crate) fn retry(args: &[String]) -> Result<i32, Fail> {
    let (flags, operands) = split_flags(args);
    reject_unknown(&flags, &["--shell", "--inline", "--only"])?;
    let shell = flag_value(&flags, "--shell").and_then(|s| Shell::parse(&s));
    let line = operands.join(" ");
    let borrowed = flags.iter().any(|f| f == "--inline");
    // Which words the shell could not run. Without it we would "fix" its own
    // builtins: fish's `set` is one edit from `sed`.
    let only: Vec<String> = flag_value(&flags, "--only")
        .map(|list| list.split(',').map(str::to_owned).collect())
        .unwrap_or_default();

    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut declined = false;
    for (start, end) in commands_in(&line) {
        let Some((word_start, word_end)) = word_span(&line, start, end) else {
            continue;
        };
        let word = &line[word_start..word_end];
        if !only.is_empty() && !only.iter().any(|name| name == word) {
            continue;
        }
        // Quotes stripped, or the danger table waves `docker "rm" x` through.
        let rest: Vec<String> = line[word_end..end]
            .split_whitespace()
            .map(|arg| arg.trim_matches(|c| c == '"' || c == '\'').to_owned())
            .collect();
        let caller = Caller {
            line: Some(Line {
                text: &line,
                word: (word_start, word_end),
            }),
            borrowed,
            subshell: false,
        };
        match decide(word, &rest, shell, caller)? {
            Outcome::Run(fixed) => {
                if let (Some(verb), Some(span)) = (fixed.verb, first_argument(&line, word_end, end))
                {
                    edits.push((span.0, span.1, verb));
                }
                edits.push((word_start, word_end, fixed.word));
            }
            Outcome::Declined => declined = true,
            Outcome::Disabled => return Ok(DISABLED),
            Outcome::Nothing | Outcome::Deferred => {}
        }
    }

    if edits.is_empty() {
        return Ok(if declined { DECLINED } else { NO_MATCH });
    }
    // Back to front, so an earlier replacement cannot move a later one's span.
    edits.sort_by_key(|(start, _, _)| *start);
    let mut fixed = line.clone();
    for (start, end, target) in edits.into_iter().rev() {
        fixed.replace_range(start..end, &target);
    }
    println!("{fixed}");
    Ok(FOUND)
}

fn first_argument(line: &str, from: usize, to: usize) -> Option<(usize, usize)> {
    let segment = line.get(from..to)?;
    let start = from + (segment.len() - segment.trim_start().len());
    let word = line.get(start..to)?.split_whitespace().next()?;
    Some((start, start + word.len()))
}

/// A corrected line is `eval`'d, so only a plain word may ever be spliced in.
pub(crate) fn is_verb(word: &str) -> bool {
    !word.is_empty()
        && !word.starts_with('-')
        && word
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
}

fn plausible_verb(word: &str) -> bool {
    is_verb(word) && word.len() <= 24 && !std::path::Path::new(word).exists()
}

/// Wider than `is_verb`, since `python3.11` and `g++` are real programs, but
/// still nothing a shell would read as punctuation.
pub(crate) fn is_plain_name(word: &str) -> bool {
    !word.is_empty()
        && !word.starts_with(['-', '~'])
        && word
            .chars()
            .all(|c| c.is_alphanumeric() || "-_.+@:".contains(c))
}

pub(crate) struct Call<'a> {
    pub(crate) parent: &'a str,
    pub(crate) verb: (usize, usize),
    args: Vec<String>,
}

/// Correcting a verb reruns the line, and in `cp a b; git sttaus` the copy
/// already happened. A lone command that failed on its verb did nothing.
fn subcommand_call(line: &str) -> Option<Call<'_>> {
    let [span] = commands_in(line)[..] else {
        return None;
    };
    call_in(line, span)
}

pub(crate) fn call_in(line: &str, (start, end): (usize, usize)) -> Option<Call<'_>> {
    let (word_start, word_end) = word_span(line, start, end)?;
    let mut at = word_end;
    let verb = loop {
        let rest = line.get(at..end)?;
        at += rest.len() - rest.trim_start().len();
        let word = line.get(at..end)?.split_whitespace().next()?;
        if is_verb(word) {
            break (at, at + word.len());
        }
        at += word.len();
    };

    Some(Call {
        parent: &line[word_start..word_end],
        verb,
        args: line[verb.0..end]
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
    })
}

fn verb_candidates(
    db_store: &Store,
    parent: &str,
    typed: &str,
    shell: Option<Shell>,
) -> Option<Vec<matcher::Hit>> {
    if db_store.is_ignored(parent) || !db_store.takes_verbs(parent) {
        return None;
    }
    let scope = store::sub_scope(parent);
    if db_store.scope_knows(scope, typed) {
        return None;
    }

    let ctx = matcher::Context::within(db_store, parent, shell);
    let pinned = db_store.sticky(&ctx.learned_as(typed));
    if pinned.is_none() && typed.chars().count() < MIN_INPUT {
        return None;
    }

    let mut hits = match pinned {
        Some(name) if !db_store.scope_knows(scope, name) => return None,
        Some(name) => vec![matcher::Hit::pinned(name, Kind::External)],
        None => {
            let verbs = db_store.verbs(parent);
            matcher::among(typed, verbs.iter().map(matcher::Candidate::from), &ctx)
        }
    };
    if hits.is_empty() {
        return None;
    }
    hits.truncate(MAX_CANDIDATES);
    Some(hits)
}

fn worth_asking(db_store: &Store, parent: &str, typed: &str) -> bool {
    !db_store.asked_for_help(parent)
        && !db_store.takes_verbs(parent)
        && !db_store.is_ignored(parent)
        && is_verb(typed)
        && typed.chars().count() >= MIN_INPUT
        && shell::on_path(parent)
        // Where running it costs something you cannot get back, its
        // subcommands are not worth finding out.
        && crate::safety::inspect(parent, &["--help".to_owned()]).is_none()
}

fn ask_for_verbs(db: &std::path::Path, parent: &str) -> Result<Store, Fail> {
    let advertised = shell::advertised_verbs(parent);
    let mut writing = store::edit(db);
    writing.mark_asked(parent);
    if advertised.len() >= store::VERBS_TO_QUALIFY {
        for verb in advertised {
            writing.bump_in(store::sub_scope(parent), &verb, store::VERB_CONFIDENCE);
        }
    }
    writing.commit().map_err(at(db))
}

fn correct_subcommand(db_store: &Store, line: &str, shell: Option<Shell>) -> Result<Outcome, Fail> {
    let Some(call) = subcommand_call(line) else {
        return Ok(Outcome::Nothing);
    };
    let typed = &line[call.verb.0..call.verb.1];

    let db = store::db_path();
    let asked;
    let db_store = match worth_asking(db_store, call.parent, typed) {
        true => {
            asked = ask_for_verbs(&db, call.parent)?;
            &asked
        }
        false => db_store,
    };

    let Some(hits) = verb_candidates(db_store, call.parent, typed, shell) else {
        return Ok(Outcome::Nothing);
    };
    let ctx = matcher::Context::within(db_store, call.parent, shell);

    let corrected = |verb: &str| {
        let mut args = call.args.clone();
        args[0] = verb.to_owned();
        safety::inspect(call.parent, &args)
    };
    let proposal = Proposal {
        typed,
        hits: &hits,
        concern: &corrected,
        line: Some(Line {
            text: line,
            word: call.verb,
        }),
        borrowed: false,
        settled: hits[0].score.is_infinite(),
        also: None,
    };

    let chosen = match confirm(db_store.mode(), &proposal) {
        Choice::Take { at, .. } => at,
        Choice::No(refused) => {
            let mut writing = store::edit(&db);
            writing.nudge_binding(&ctx.learned_as(typed), &hits[refused].name, -1);
            writing.commit().map_err(at(&db))?;
            return Ok(Outcome::Declined);
        }
        Choice::Silent => return Ok(Outcome::Nothing),
    };

    let target = &hits[chosen].name;
    let mut writing = store::edit(&db);
    writing.bump_in(store::sub_scope(call.parent), target, 1.0);
    writing.nudge_binding(&ctx.learned_as(typed), target, 1);
    writing.commit().map_err(at(&db))?;
    Ok(Outcome::Run(Fixed::word(
        proposal.line.unwrap().with(target),
    )))
}

#[derive(Clone, Copy)]
struct Line<'a> {
    text: &'a str,
    word: (usize, usize),
}

impl Line<'_> {
    fn with(&self, target: &str) -> String {
        format!(
            "{}{target}{}",
            &self.text[..self.word.0],
            &self.text[self.word.1..]
        )
    }
}

struct Caller<'a> {
    line: Option<Line<'a>>,
    borrowed: bool,
    /// zsh and bash run their not-found hook in a fork, which throws away what
    /// an alias or function did. Those go back for the next prompt to run.
    subshell: bool,
}

struct Fixed {
    word: String,
    verb: Option<String>,
}

impl Fixed {
    fn word(word: String) -> Fixed {
        Fixed { word, verb: None }
    }
}

enum Outcome {
    Run(Fixed),
    Nothing,
    Declined,
    Disabled,
    Deferred,
}

struct AlsoVerb {
    typed: String,
    fixed: String,
}

fn decide(
    word: &str,
    rest: &[String],
    shell: Option<Shell>,
    caller: Caller<'_>,
) -> Result<Outcome, Fail> {
    if !correctable(word) {
        return Ok(Outcome::Nothing);
    }

    let db = store::db_path();
    // Read unlocked: holding it across a prompt would queue every other
    // shell's hook behind one keypress. The increments re-open it locked.
    let mut db_store = Store::open(&db);
    // Read, not taken: they stay for whoever next holds the write lock.
    for (text, at) in pending() {
        apply(&mut db_store, &text, at);
    }
    if !db_store.enabled() {
        return Ok(Outcome::Disabled);
    }
    let pinned = db_store.sticky(word).map(str::to_owned);
    if pinned.is_none() && word.chars().count() < MIN_INPUT {
        return Ok(Outcome::Nothing);
    }

    let dir = std::env::current_dir()
        .map(|d| store::dir_key(&d))
        .unwrap_or(0);
    let ctx = Context::new(&db_store, dir, shell);

    let hits = candidates(word, &ctx, pinned.as_deref(), MAX_CANDIDATES);
    if hits.is_empty() {
        return Ok(Outcome::Nothing);
    }
    // Before the question, or it gets asked twice.
    if caller.subshell && matches!(hits[0].kind, Kind::Shell(_)) {
        return Ok(Outcome::Deferred);
    }

    let also = rest
        .first()
        .filter(|verb| plausible_verb(verb))
        .and_then(|verb| {
            let fixed = verb_candidates(&db_store, &hits[0].name, verb, shell)?;
            Some(AlsoVerb {
                typed: verb.clone(),
                fixed: fixed.into_iter().next()?.name,
            })
        });

    // The danger table has to see the line that will run: `docker imgae prne`
    // is a prune however it was spelled.
    let corrected: Option<Vec<String>> = also.as_ref().map(|also| {
        let mut args = rest.to_vec();
        args[0] = also.fixed.clone();
        args
    });

    let (chosen, and_verb) = match confirm(
        db_store.mode(),
        &Proposal {
            typed: word,
            hits: &hits,
            concern: &|name| {
                safety::inspect(name, rest).or_else(|| {
                    corrected
                        .as_deref()
                        .and_then(|args| safety::inspect(name, args))
                })
            },
            line: caller.line,
            borrowed: caller.borrowed,
            settled: pinned.is_some(),
            also: also.as_ref(),
        },
    ) {
        Choice::Take { at, and_verb } => (at, and_verb),
        Choice::No(refused) => {
            let mut writing = store::edit(&db);
            writing.nudge_binding(word, &hits[refused].name, -1);
            writing.commit().map_err(at(&db))?;
            return Ok(Outcome::Declined);
        }
        Choice::Silent => return Ok(Outcome::Nothing),
    };

    let target = hits[chosen].name.clone();
    let verb = also.filter(|_| and_verb && chosen == 0);

    let mut writing = store::edit(&db);
    let folded = fold(&mut writing);
    remember(&mut writing, word, &target, dir, caller.borrowed);
    if let Some(verb) = &verb {
        writing.bump_in(store::sub_scope(&target), &verb.fixed, 1.0);
        writing.nudge_binding(&format!("{target} {}", verb.typed), &verb.fixed, 1);
    }
    writing.commit_taking(folded).map_err(at(&db))?;

    Ok(Outcome::Run(Fixed {
        word: target,
        verb: verb.map(|v| v.fixed),
    }))
}

/// The preexec hook never saw this command. Except in fish, where it did.
fn remember(db_store: &mut Store, word: &str, target: &str, dir: u64, shell_will_count: bool) {
    if !shell_will_count {
        let kind = db_store
            .get(target)
            .map_or(Kind::External, |entry| entry.kind);
        db_store.bump(target, kind, 1.0);
        db_store.bump_in(dir, target, 1.0);
    }
    db_store.nudge_binding(word, target, 1);
}

fn candidates(word: &str, ctx: &Context, pinned: Option<&str>, keep: usize) -> Vec<matcher::Hit> {
    // `runnable` stats PATH, so this costs a sweep per candidate asked. The
    // list is sorted, so a survivor past PROBES was never going to be shown.
    let mut hits: Vec<matcher::Hit> = matcher::rank(word, ctx)
        .into_iter()
        .take(PROBES)
        .filter(|hit| runnable(hit.kind, &hit.name, ctx.shell))
        .take(keep)
        .collect();

    let only_guesses = hits.iter().all(matcher::Hit::is_speculative);
    if (hits.is_empty() || only_guesses) && pinned.is_none() {
        let mut found = matcher::among(
            word,
            shell::path_names(|name| plausibly_installed(word, name)).map(|name| {
                matcher::Candidate {
                    name,
                    kind: Kind::External,
                    rank: 0.0,
                    last: 0,
                }
            }),
            ctx,
        );
        found.retain(|hit| match hit.tier {
            matcher::Tier::Prefix => true,
            matcher::Tier::Typo => hit.distance <= 1,
            _ => false,
        });
        found.retain(|hit| !hits.iter().any(|known| known.name == hit.name));
        // Installed before cut: a dead name must not take a live one's place.
        found.truncate(PROBES);
        found.retain(|hit| shell::on_path(&hit.name));
        found.truncate(MAX_CANDIDATES);
        hits.extend(found);
        matcher::sort(&mut hits);
    }

    if let Some(pinned) = pinned {
        let kind = ctx
            .store
            .get(pinned)
            .map_or(Kind::External, |entry| entry.kind);
        match hits.iter().position(|hit| hit.name == pinned) {
            Some(at) => hits.swap(0, at),
            None if runnable(kind, pinned, ctx.shell) => {
                hits.insert(0, matcher::Hit::pinned(pinned, kind))
            }
            None => {}
        }
    }
    // The one gate between a filename and a line the shell will `eval`. A file
    // called `zqx;touch PWNED` is a prefix match for `zqx` like any other.
    hits.retain(|hit| is_plain_name(&hit.name));
    hits.truncate(keep);
    hits
}

fn plausibly_installed(word: &str, name: &str) -> bool {
    if !word.is_ascii() || !name.is_ascii() {
        return true;
    }
    let (typed, candidate) = (word.as_bytes(), name.as_bytes());
    candidate.len().abs_diff(typed.len()) <= 1
        || (candidate.len() > typed.len() && candidate[..typed.len()].eq_ignore_ascii_case(typed))
}

struct Proposal<'a> {
    typed: &'a str,
    hits: &'a [matcher::Hit],
    concern: &'a dyn Fn(&str) -> Option<safety::Concern>,
    line: Option<Line<'a>>,
    borrowed: bool,
    settled: bool,
    also: Option<&'a AlsoVerb>,
}

enum Choice {
    Take {
        at: usize,
        and_verb: bool,
    },
    /// Which candidate was turned down.
    No(usize),
    Silent,
}

/// Every mode goes through here, so one place decides what runs unasked.
fn confirm(mode: Mode, proposal: &Proposal) -> Choice {
    let Some(mut tty) = term::Tty::open_as(proposal.borrowed) else {
        return Choice::Silent;
    };
    let hits = proposal.hits;
    let ambiguous = !proposal.settled
        && hits.len() > 1
        && hits[1].tier == hits[0].tier
        && hits[1].score >= AMBIGUITY * hits[0].score;
    let concern = (proposal.concern)(&hits[0].name);

    let must_ask = match mode {
        Mode::Safe => true,
        Mode::Unsafe => concern.is_some() || ambiguous,
        Mode::Bypass => false,
    };
    if !must_ask {
        // Not while the line editor is drawing: the alternate screen it would
        // land on is discarded the moment this returns.
        if mode == Mode::Unsafe && !proposal.borrowed {
            let notice = format!("zcomplete: {} -> {}\n", proposal.typed, hits[0].name);
            tty.say(&tty.paint("2", &notice));
        }
        return Choice::Take {
            at: 0,
            and_verb: proposal.also.is_some(),
        };
    }

    let Some((chosen, and_verb)) = ask(&mut tty, proposal, concern.as_ref(), ambiguous) else {
        return Choice::No(0);
    };

    if chosen != 0 {
        if let Some(concern) = (proposal.concern)(&hits[chosen].name) {
            let question = format!(
                "zcomplete: run {}? {}",
                tty.paint("1", &hits[chosen].name),
                tty.paint("33", &format!("({})", concern.reason))
            );
            if !tty.ask(&question, false) {
                return Choice::No(chosen);
            }
        }
    }
    Choice::Take {
        at: chosen,
        and_verb,
    }
}

fn ask(
    tty: &mut term::Tty,
    proposal: &Proposal,
    concern: Option<&safety::Concern>,
    ambiguous: bool,
) -> Option<(usize, bool)> {
    let hits = proposal.hits;
    let typed = tty.paint("1", proposal.typed);

    if ambiguous && concern.is_none() {
        let options: Vec<String> = hits
            .iter()
            .map(|hit| format!("{:<20} {}", hit.name, tty.paint("2", hit.tier.label())))
            .collect();
        return Some((
            tty.choose(&format!("zcomplete: '{typed}' could be:"), &options)?,
            false,
        ));
    }

    let target = &hits[0].name;
    let action = match proposal.line {
        Some(line) => format!("run {}", tty.paint("1", &line.with(target))),
        None => format!("run {} instead of '{typed}'", tty.paint("1", target)),
    };
    let question = match concern {
        Some(c) => format!(
            "zcomplete: {action}? {}",
            tty.paint("33", &format!("({})", c.reason))
        ),
        None => format!("zcomplete: {action}?"),
    };

    let Some(also) = proposal.also else {
        return tty.ask(&question, concern.is_none()).then_some((0, false));
    };
    let offer = tty.paint("2", &format!("  u: also {} -> {}", also.typed, also.fixed));
    match tty.ask_with(
        &format!("{question}{offer}"),
        concern.is_none(),
        &['u', 'i'],
    ) {
        'y' => Some((0, false)),
        'u' => Some((0, true)),
        _ => None,
    }
}

pub(crate) fn commands_in(line: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = None;
    let (mut single, mut double, mut escaped) = (false, false, false);

    for (at, byte) in line.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if !single => escaped = true,
            b'\'' if !double => single = !single,
            b'"' if !single => double = !double,
            _ if single || double => {}
            b'|' | b'&' | b';' | b'(' | b'\n' => {
                if let Some(from) = start.take() {
                    spans.push((from, at));
                }
            }
            b' ' | b'\t' => {}
            _ => start = start.or(Some(at)),
        }
    }
    if let Some(from) = start {
        spans.push((from, line.len()));
    }
    spans
}

fn word_span(line: &str, from: usize, to: usize) -> Option<(usize, usize)> {
    let mut at = from;
    loop {
        let segment = line.get(at..to)?;
        at += segment.len() - segment.trim_start().len();
        let word = line.get(at..to)?.split_whitespace().next()?;
        if !shell::WRAPPERS.contains(&word) && !word.contains('=') {
            return Some((at, at + word.len()));
        }
        at += word.len();
    }
}

fn correctable(word: &str) -> bool {
    !word.is_empty()
        && !word.contains('/')
        && !word.contains('=')
        && !word.starts_with(['-', '.', '#'])
        && !shell::on_path(word)
}

fn runnable(kind: Kind, name: &str, shell: Option<Shell>) -> bool {
    match kind {
        Kind::External => shell::on_path(name),
        kind => kind.usable_in(shell),
    }
}

/// What the shell appends instead of starting us up, one line per command:
///
///     <seconds> <kind> <word> <verb> <directory>
///
/// Space separated with the free-form field last, so a path with spaces still
/// parses. One file per shell session, so a line has exactly one writer.
///
/// This reads every journal and leaves it where it is.
pub(crate) fn pending() -> Vec<(String, u64)> {
    let dir = store::data_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_str().is_some_and(is_journal) {
            if let Ok(part) = std::fs::read_to_string(entry.path()) {
                found.push((part, written(&entry.path())));
            }
        }
    }
    found
}

/// fish has no clock builtin, so its lines are dated by this instead.
fn written(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or_else(store::now, |since| since.as_secs())
}

fn is_journal(name: &str) -> bool {
    name.starts_with("journal.") && folding_owner(name).is_none()
}

fn folding_owner(name: &str) -> Option<u32> {
    let (head, pid) = name.rsplit_once('.')?;
    (head.starts_with("journal.") && head.ends_with(".folding"))
        .then(|| pid.parse().ok())
        .flatten()
}

/// `EPERM` is somebody else's process rather than a free pid.
fn alive(pid: u32) -> bool {
    let answer = unsafe { libc::kill(pid as libc::pid_t, 0) };
    answer == 0 || std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
}

/// An empty journal back at `name`, 0600. Only while its session is alive, or
/// a dead shell's would be remade at every fold and never taken away.
fn replace(dir: &std::path::Path, name: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    let owner = name.rsplit('.').next().and_then(|pid| pid.parse().ok());
    if !owner.is_some_and(alive) {
        return;
    }
    // `create_new`, so a shell that got its append in first keeps its file.
    let _ = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(dir.join(name));
}

/// Every journal, taken. Only for a caller holding the write lock.
///
/// The files come back rather than going away: one unlinked before the database
/// it fed has landed is a session's counts lost. `commit_taking` deletes them.
#[must_use]
pub(crate) fn fold(writing: &mut store::Editing) -> Vec<std::path::PathBuf> {
    // No lock, corrections off, or read-only: the counts have nowhere to land,
    // and taking the journals would only lose them.
    if !writing.locked() || !writing.enabled() || writing.is_read_only() {
        return Vec::new();
    }
    let dir = store::data_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut taken = Vec::new();
    let mut mine = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Renamed before it is read: a shell appending at that moment opens
        // the path afresh.
        let taking = if is_journal(name) {
            // The pid keeps this off a journal an earlier fold still holds.
            let taking = dir.join(format!("{name}.folding.{}", std::process::id()));
            if std::fs::rename(entry.path(), &taking).is_err() {
                continue;
            }
            // The shells make the journal 0600 at startup and only append
            // after, so leaving the name free hands the next one to a redirect
            // - and a redirect takes the user's umask.
            replace(&dir, name);
            taking
        } else if let Some(owner) = folding_owner(name) {
            // Left by a fold that died. Only once its taker is gone, or two
            // folds would count every line twice.
            if owner == std::process::id() || alive(owner) {
                continue;
            }
            entry.path()
        } else {
            continue;
        };
        // Bytes, not text: one non-UTF-8 directory name would throw away every
        // other line in the file with it.
        let Ok(part) = std::fs::read(&taking) else {
            continue;
        };
        taken.push((
            String::from_utf8_lossy(&part).into_owned(),
            written(&taking),
        ));
        mine.push(taking);
    }
    for (text, at) in taken {
        // Past the cap the rest is set aside for the next fold, not dropped.
        let (head, rest) = split_at_limit(&text);
        apply(writing, head, at);
        if !rest.is_empty() {
            spill(&dir, rest);
        }
    }
    mine
}

fn split_at_limit(text: &str) -> (&str, &str) {
    match text.match_indices('\n').nth(FOLD_LIMIT - 1) {
        Some((end, _)) => text.split_at(end + 1),
        None => (text, ""),
    }
}

fn spill(dir: &std::path::Path, rest: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    let path = dir.join(format!("journal.rest.{}", std::process::id()));
    let _ = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(rest.as_bytes()));
}

pub(crate) fn apply(db_store: &mut Store, text: &str, unstamped: u64) {
    if text.is_empty() || !db_store.enabled() {
        return;
    }

    let ceiling = store::now();
    // One `canonicalize` per directory, not per line. `on_path` memoizes too.
    let mut real: std::collections::HashMap<String, std::path::PathBuf> =
        std::collections::HashMap::new();
    for line in text.lines().take(FOLD_LIMIT) {
        let mut field = line.splitn(5, ' ');
        let (Some(stamp), Some(tag), Some(word), Some(verb), Some(dir)) = (
            field.next(),
            field.next(),
            field.next(),
            field.next(),
            field.next(),
        ) else {
            continue;
        };
        // Clamped: a wrong clock would stick a rank in the hour bucket.
        let at = match stamp.parse().unwrap_or(0) {
            0 => unstamped,
            stamped => stamped,
        }
        // `max(1)`: a clock at the epoch would invert `clamp`'s range.
        .clamp(1, ceiling.max(1));
        let kind = match Shell::parse(tag) {
            Some(shell) => Kind::Shell(shell),
            // Checked now: a tool uninstalled since is dropped.
            None if shell::on_path(word) => Kind::External,
            None => continue,
        };
        if db_store.is_ignored(word) {
            continue;
        }
        db_store.absorb(word, kind, 1.0, at);
        // Canonical: other readers use `current_dir`, and `$PWD` keeps the
        // symlinks in.
        let dir = real.entry(dir.to_owned()).or_insert_with(|| {
            std::fs::canonicalize(dir).unwrap_or_else(|_| std::path::PathBuf::from(dir))
        });
        db_store.bump_in_at(store::dir_key(dir), word, 1.0, at);
        if !verb.is_empty() && is_verb(verb) && verb.len() <= 24 && !dir.join(verb).exists() {
            db_store.bump_in_at(store::sub_scope(word), verb, 1.0, at);
        }
    }
}

/// A shell that never mistypes would buffer for ever, so the hooks ask for
/// this every so often.
pub(crate) fn flush() -> Result<i32, Fail> {
    let db = store::db_path();
    let mut db_store = store::edit(&db);
    let folded = fold(&mut db_store);
    db_store.commit_taking(folded).map_err(at(&db))?;
    Ok(0)
}

const FOLD_LIMIT: usize = 20_000;

pub(crate) fn record(args: &[String]) -> Result<i32, Fail> {
    let (flags, operands) = split_flags(args);
    reject_unknown(&flags, &["--shell", "--kind", "--status"])?;
    let line = operands.join(" ");
    let Some(word) = shell::command_word(&line) else {
        return Ok(0);
    };
    let shell = flag_value(&flags, "--shell").and_then(|s| Shell::parse(&s));
    let status = flag_value(&flags, "--status").and_then(|s| s.parse::<i32>().ok());
    let kind = match flag_value(&flags, "--kind").as_deref() {
        Some("shell") => match shell {
            Some(shell) => Kind::Shell(shell),
            None => fail!("--kind shell needs --shell"),
        },
        _ if shell::on_path(word) => Kind::External,
        _ => return Ok(0),
    };

    let db = store::db_path();
    let mut db_store = store::edit(&db);
    let folded = fold(&mut db_store);
    if !db_store.enabled() {
        return Ok(0);
    }
    if !db_store.is_ignored(word) {
        db_store.bump(word, kind, 1.0);
        if let Ok(dir) = std::env::current_dir() {
            db_store.bump_in(store::dir_key(&dir), word, 1.0);
        }
        // Only once it worked, or `sttaus` becomes a thing git does.
        if status == Some(0) {
            if let Some(call) = subcommand_call(&line) {
                let verb = &line[call.verb.0..call.verb.1];
                if plausible_verb(verb) {
                    db_store.bump_in(store::sub_scope(word), verb, 1.0);
                }
            }
        }
    }
    let saved = db_store.commit_taking(folded).map_err(at(&db))?;

    // Only with the lock let go is it safe to ask a human anything.
    if status.is_some_and(|code| code != 0) {
        if let Outcome::Run(fixed) = correct_subcommand(&saved, &line, shell)? {
            println!("{}", fixed.word);
            return Ok(FOUND);
        }
    }
    Ok(0)
}

pub(crate) fn query(args: &[String]) -> Result<i32, Fail> {
    let (flags, operands) = split_flags(args);
    {
        let db = store::db_path();
        let mut writing = store::edit(&db);
        let folded = fold(&mut writing);
        writing.commit_taking(folded).map_err(at(&db))?;
    }
    let Some(word) = operands.last() else {
        fail!("query needs a word")
    };
    reject_unknown(&flags, &["--score", "-n", "--limit", "--shell"])?;
    let db_store = Store::open(&store::db_path());
    let dir = std::env::current_dir()
        .map(|d| store::dir_key(&d))
        .unwrap_or(0);
    let shell = flag_value(&flags, "--shell").and_then(|s| Shell::parse(&s));
    let parent = (operands.len() > 1).then(|| operands[operands.len() - 2].clone());
    let ctx = match &parent {
        Some(parent) => Context::within(&db_store, parent, shell),
        None => Context::new(&db_store, dir, shell),
    };

    let scored = flags.iter().any(|f| f == "--score");
    let limit = flag_value(&flags, "-n")
        .or_else(|| flag_value(&flags, "--limit"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(if scored { 10 } else { 1 });

    let sticky = db_store.sticky(&ctx.learned_as(word));
    let hits = match &parent {
        _ if sticky.is_none() && word.chars().count() < MIN_INPUT => Vec::new(),
        Some(parent) => verb_candidates(&db_store, parent, word, shell).unwrap_or_default(),
        None if !correctable(word) => Vec::new(),
        None => candidates(word, &ctx, sticky, limit.max(MAX_CANDIDATES)),
    };
    if hits.is_empty() {
        match &parent {
            Some(parent) if db_store.scope_knows(store::sub_scope(parent), word) => {
                eprintln!("zcomplete: '{parent} {word}' already works");
            }
            None if shell::on_path(word) => {
                eprintln!("zcomplete: '{word}' is already a command");
            }
            _ => {}
        }
        return Ok(NO_MATCH);
    }

    for hit in hits.iter().take(limit) {
        match scored {
            true => println!(
                "{:<24} {:<9} {:>8} {:>8}",
                hit.name,
                hit.tier.label(),
                store::tenths(hit.score),
                store::tenths(hit.rank)
            ),
            false => println!("{}", hit.name),
        }
    }
    Ok(FOUND)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_that_is_no_longer_installed_is_not_offered() {
        assert!(!runnable(
            Kind::External,
            "definitely-not-installed-xyzzy",
            None
        ));
        assert!(runnable(Kind::External, "sh", None));
        assert!(runnable(
            Kind::Shell(Shell::Zsh),
            "mygitfn",
            Some(Shell::Zsh)
        ));
        assert!(!runnable(
            Kind::Shell(Shell::Zsh),
            "mygitfn",
            Some(Shell::Fish)
        ));
    }

    #[test]
    fn a_command_that_costs_something_is_never_run_to_read_its_help() {
        let store = Store::default();
        // `sh` is the ordinary case: on PATH, no verbs yet, worth asking.
        assert!(worth_asking(&store, "sh", "sttaus"));
        for dangerous in ["rm", "sudo", "dd", "shutdown", "mkfs.ext4"] {
            assert!(
                !worth_asking(&store, dangerous, "sttaus"),
                "{dangerous} --help would have been run"
            );
        }
    }

    #[test]
    fn a_filename_the_shell_would_read_as_punctuation_is_never_offered() {
        assert!(!is_plain_name("zqx;touch PWNED"));
        assert!(!is_plain_name("a`id`"));
        assert!(!is_plain_name("a$(id)"));
        assert!(!is_plain_name("a|b"));
        assert!(!is_plain_name("a b"));
        assert!(!is_plain_name("a&b"));
        assert!(!is_plain_name("a>b"));
        assert!(!is_plain_name("a*"));
        assert!(!is_plain_name("~evil"));
        assert!(!is_plain_name("-rf"));
        assert!(!is_plain_name(""));

        assert!(is_plain_name("git"));
        assert!(is_plain_name("python3.11"));
        assert!(is_plain_name("g++"));
        assert!(is_plain_name("x86_64-linux-gnu-gcc"));
        assert!(is_plain_name("ld.gold"));
    }

    #[test]
    fn pathlike_words_are_left_alone() {
        assert!(!correctable("./build"));
        assert!(!correctable("bin/tool"));
        assert!(!correctable("FOO=1"));
        assert!(!correctable("-l"));
        assert!(!correctable("sh"), "sh exists, so there is nothing to fix");
        assert!(correctable("mkd"));
    }

    #[test]
    fn every_command_in_a_line_is_found() {
        let spans = |line: &str| {
            commands_in(line)
                .into_iter()
                .filter_map(|(from, to)| {
                    word_span(line, from, to).map(|(s, e)| line[s..e].to_string())
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(spans("ls -la"), ["ls"]);
        assert_eq!(spans("printf hi | ct | wc -l"), ["printf", "ct", "wc"]);
        assert_eq!(spans("make && ct out.txt"), ["make", "ct"]);
        assert_eq!(spans("cd /tmp; ls"), ["cd", "ls"]);
        assert_eq!(spans("FOO=1 sudo mkd thing"), ["mkd"]);
        assert_eq!(spans("echo 'a | b'"), ["echo"]);
        assert_eq!(spans(r#"echo "x; y" && ls"#), ["echo", "ls"]);
        assert_eq!(spans(r"echo a\|b"), ["echo"]);
    }

    #[test]
    fn the_prompt_previews_the_word_it_is_actually_replacing() {
        let preview = |text: &str, typo: &str, target: &str| {
            let (from, to) = commands_in(text)
                .into_iter()
                .filter_map(|(from, to)| word_span(text, from, to))
                .find(|(from, to)| &text[*from..*to] == typo)
                .expect("the typo is one of the command words");
            Line {
                text,
                word: (from, to),
            }
            .with(target)
        };
        assert_eq!(
            preview("mkd 'two words'", "mkd", "mkdir"),
            "mkdir 'two words'"
        );
        assert_eq!(preview("  mkd x", "mkd", "mkdir"), "  mkdir x");
        assert_eq!(preview("FOO=1 mkd x", "mkd", "mkdir"), "FOO=1 mkdir x");
        assert_eq!(preview("printf hi | ct", "ct", "cat"), "printf hi | cat");
    }

    #[test]
    fn the_path_prefilter_keeps_everything_the_ranking_could_want() {
        assert!(plausibly_installed("doc", "docker-compose"));
        assert!(plausibly_installed("KUB", "kubectl"));
        assert!(plausibly_installed("gti", "git"));
        assert!(plausibly_installed("gitx", "git"));
        assert!(plausibly_installed("mkdi", "mkdir"));
        assert!(!plausibly_installed("gti", "pinentry-tty"));
        assert!(!plausibly_installed("mkd", "docker-compose"));
        assert!(plausibly_installed("wörterbuch", "ls"));
    }

    #[test]
    fn a_command_earns_its_subcommands_by_repeating_them() {
        let mut store = Store::default();
        let git = store::sub_scope("git");

        store.bump_in(git, "status", 1.0);
        assert!(!store.takes_verbs("git"), "one use, one word");
        store.bump_in(git, "status", 1.0);
        assert!(!store.takes_verbs("git"), "still one word");
        store.bump_in(git, "commit", 1.0);
        store.bump_in(git, "commit", 1.0);
        assert!(store.takes_verbs("git"));

        let mut store = Store::default();
        for pattern in ["fixme", "todo", "panic", "unwrap", "async"] {
            store.bump_in(store::sub_scope("grep"), pattern, 1.0);
        }
        assert!(!store.takes_verbs("grep"));

        let mut store = Store::default();
        store.bump_in(store::sub_scope("git"), "status", 3.0);
        store.bump_in(store::sub_scope("git"), "somefile.txt", 1.0);
        let offered: Vec<String> = store.verbs("git").into_iter().map(|e| e.name).collect();
        assert_eq!(offered, ["status"]);
    }

    #[test]
    fn a_file_sitting_right_there_is_not_a_subcommand() {
        assert!(plausible_verb("status"));
        assert!(plausible_verb("filter-branch"));
        assert!(!plausible_verb("--force"));
        assert!(!plausible_verb("src"), "src exists, so it is an argument");
        assert!(!plausible_verb("Cargo.toml"));
    }

    #[test]
    fn the_verb_is_found_past_the_options_and_the_wrappers() {
        let verb = |line: &str| {
            subcommand_call(line).map(|call| {
                (
                    call.parent.to_string(),
                    line[call.verb.0..call.verb.1].to_string(),
                    call.args.join(" "),
                )
            })
        };
        let named = |line: &str| verb(line).map(|(p, v, _)| format!("{p} {v}"));

        assert_eq!(named("git sttaus").as_deref(), Some("git sttaus"));
        assert_eq!(named("git  -C /tmp  sttaus").as_deref(), Some("git sttaus"));
        assert_eq!(named("sudo docker rn box").as_deref(), Some("docker rn"));
        assert_eq!(named("GIT_DIR=. git sttaus").as_deref(), Some("git sttaus"));
        assert_eq!(named("cargo -v buidl").as_deref(), Some("cargo buidl"));
        assert_eq!(named("git"), None);
        assert_eq!(named("git --version"), None);
        assert_eq!(
            verb("git rest --hard HEAD~1").map(|(_, _, args)| args),
            Some("rest --hard HEAD~1".to_string())
        );
    }

    #[test]
    fn a_line_that_would_be_replayed_is_left_alone() {
        assert!(subcommand_call("cp a b; git sttaus").is_none());
        assert!(subcommand_call("git sttaus && echo done").is_none());
        assert!(subcommand_call("git sttaus | cat").is_none());
        assert!(subcommand_call("git sttaus & echo hi").is_none());
        assert!(subcommand_call("git sttaus").is_some());
        assert!(subcommand_call("git cmomit -m 'a; b'").is_some());
    }

    #[test]
    fn only_a_plain_word_can_ever_be_spliced_into_a_line() {
        assert!(is_verb("status"));
        assert!(is_verb("filter-branch"));
        assert!(is_verb("run_tests"));
        assert!(!is_verb("--force"));
        assert!(!is_verb("-C"));
        assert!(!is_verb(""));
        for hostile in [
            "a;rm", "$(id)", "`id`", "a b", "a|b", "a>b", "../x", "a&b", "a\nb", "'x'",
        ] {
            assert!(!is_verb(hostile), "{hostile} would reach an eval");
        }
        assert!(subcommand_call("git $(whoami)").is_none());
    }
}
