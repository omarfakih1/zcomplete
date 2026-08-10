mod config;
mod matcher;
mod safety;
mod shell;
mod store;
mod term;

use std::fmt;
use std::io::Write;
use std::path::PathBuf;

use config::{Color, Config, Mode};
use matcher::Context;
use store::{Kind, Shell, Store};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Resolution succeeded; the command to run is on stdout.
const FOUND: i32 = 0;
/// Nothing worth suggesting. The shell should report the original word.
const NO_MATCH: i32 = 1;
/// A suggestion was made and turned down.
const DECLINED: i32 = 3;
/// Corrections are switched off.
const DISABLED: i32 = 4;

pub struct Fail(String);

impl fmt::Display for Fail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<std::io::Error> for Fail {
    fn from(err: std::io::Error) -> Fail {
        Fail(plain(&err))
    }
}

/// An error the user can act on names the thing that failed, not just the
/// errno: "zcomplete: /nope/seed.tsv: No such file or directory".
fn at(path: &std::path::Path) -> impl Fn(std::io::Error) -> Fail + '_ {
    move |err| Fail(format!("{}: {}", path.display(), plain(&err)))
}

/// Rust renders an OS error as "No such file or directory (os error 2)". The
/// number is for us, not for whoever is looking at their prompt.
fn plain(err: &std::io::Error) -> String {
    let text = err.to_string();
    match text.rfind(" (os error ") {
        Some(cut) => text[..cut].to_owned(),
        None => text,
    }
}

macro_rules! fail {
    ($($arg:tt)*) => { return Err(Fail(format!($($arg)*))) };
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(&args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("zcomplete: {err}");
            2
        }
    };
    std::process::exit(code);
}

fn run(args: &[String]) -> Result<i32, Fail> {
    let (verb, rest) = match args.split_first() {
        Some((verb, rest)) => (verb.as_str(), rest),
        None => {
            print!("{}", usage());
            return Ok(0);
        }
    };

    match verb {
        "init" => init(rest),
        "resolve" => resolve(rest),
        "retry" => retry(rest),
        "record" => record(rest),
        "query" => query(rest),
        "stats" | "list" => stats(rest),
        "import" => import(rest),
        "forget" | "remove" => forget(rest),
        "bind" => bind(rest),
        "unbind" => unbind(rest),
        "ignore" => ignore(rest),
        "mode" => mode(rest),
        "on" => switch(true),
        "off" => switch(false),
        "doctor" => doctor(),
        "export" => export(),
        "--safe" | "--unsafe" | "--bypass" => mode(&[verb.trim_start_matches("--").to_string()]),
        "--version" | "-V" | "version" => {
            println!("zcomplete {VERSION}");
            Ok(0)
        }
        "help" | "--help" | "-h" => {
            print!("{}", usage());
            Ok(0)
        }
        other => fail!("unknown command '{other}' (try `zcomplete help`)"),
    }
}

fn usage() -> String {
    format!(
        "\
zcomplete {VERSION} - run the command you meant

  init <zsh|bash|fish>     print the shell integration
  query <word> [--score]   show what a word would resolve to
  stats [-n N]             learned commands, strongest first
  import [zsh|bash|fish]   seed the database from shell history
  bind <word> <command>    always resolve <word> to <command>
  unbind <word>            drop a pinned or learned shortcut
  forget <command>...      remove commands from the database
  ignore <command>...      keep a command out of every suggestion
  mode [safe|unsafe|bypass]  show or set the confirmation mode
  on | off                 enable or disable corrections
  doctor                   check the installation
  export                   dump the database as text

  --safe                   confirm every correction (default)
  --unsafe                 confirm only dangerous corrections
  --bypass                 never confirm

Setup:
  zsh    eval \"$(zcomplete init zsh)\"        >> ~/.zshrc
  bash   eval \"$(zcomplete init bash)\"       >> ~/.bashrc
  fish   zcomplete init fish | source          >> ~/.config/fish/config.fish

The shell integration also calls resolve, record and retry. They are its
business rather than yours.
"
    )
}

fn init(args: &[String]) -> Result<i32, Fail> {
    let Some(name) = args.first() else {
        fail!("init needs a shell: zsh, bash or fish")
    };
    let Some(shell) = Shell::parse(name) else {
        fail!("unsupported shell '{name}' (zsh, bash and fish are supported)")
    };
    print!("{}", shell::init_script(shell));
    Ok(0)
}

/// The hot path: the shell could not find a word and is asking what to do.
fn resolve(args: &[String]) -> Result<i32, Fail> {
    let (flags, operands) = split_flags(args);
    let shell = flag_value(&flags, "--shell").and_then(|s| Shell::parse(&s));
    let Some(word) = operands.first() else {
        return Ok(NO_MATCH);
    };

    match decide(word, &operands[1..], shell, None)? {
        Outcome::Run(target) => {
            let mut out = std::io::stdout();
            writeln!(out, "{target}")?;
            out.flush()?;
            Ok(FOUND)
        }
        Outcome::Nothing => Ok(NO_MATCH),
        Outcome::Declined => Ok(DECLINED),
        Outcome::Disabled => Ok(DISABLED),
    }
}

/// The bash 3.2 path. There is no way to intercept an unknown command on the
/// bash that ships with macOS, so the shell tells us afterwards: the last line
/// exited 127, here it is, offer to run it properly. Unlike the not-found
/// handler this runs in the user's own shell, so the rewritten line is eval'd
/// there and a `cd` in it actually sticks.
fn retry(args: &[String]) -> Result<i32, Fail> {
    let (flags, operands) = split_flags(args);
    let shell = flag_value(&flags, "--shell").and_then(|s| Shell::parse(&s));
    let line = operands.join(" ");
    // The shell says which words it could not run. Without that we would try to
    // "fix" its own builtins, which are not on PATH: fish's `set` is one edit
    // from `sed`.
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
        let rest: Vec<String> = line[word_end..end]
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let within = Line {
            text: &line,
            word: (word_start, word_end),
        };
        match decide(word, &rest, shell, Some(within))? {
            Outcome::Run(target) => edits.push((word_start, word_end, target)),
            Outcome::Declined => declined = true,
            Outcome::Disabled => return Ok(DISABLED),
            Outcome::Nothing => {}
        }
    }

    if edits.is_empty() {
        return Ok(if declined { DECLINED } else { NO_MATCH });
    }
    let mut fixed = line.clone();
    for (start, end, target) in edits.into_iter().rev() {
        fixed.replace_range(start..end, &target);
    }
    println!("{fixed}");
    Ok(FOUND)
}

/// A command word in its surrounding line, so the prompt can show the whole
/// line as it would run rather than the word on its own.
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

enum Outcome {
    Run(String),
    Nothing,
    Declined,
    Disabled,
}

/// Everything between "this word does not name a command" and "run this
/// instead": ranking, the safety check, the prompt, and recording the answer.
fn decide(
    word: &str,
    rest: &[String],
    shell: Option<Shell>,
    within: Option<Line<'_>>,
) -> Result<Outcome, Fail> {
    let cfg = Config::load();
    if !cfg.enabled {
        return Ok(Outcome::Disabled);
    }
    if !correctable(word) {
        return Ok(Outcome::Nothing);
    }

    let db = config::db_path();
    // Read unlocked. The database must not be held while a human decides
    // whether to answer the prompt: every other shell's per-command hook would
    // queue behind their keypress. The increments below re-open it locked, for
    // the microseconds that actually need it.
    let db_store = Store::open(&db);
    // A shortcut the user pinned or confirmed repeatedly is not a guess, so it
    // sidesteps both the minimum word length and the matcher entirely.
    let pinned = db_store.sticky(word).map(str::to_owned);
    if pinned.is_none() && word.chars().count() < cfg.min_input {
        return Ok(Outcome::Nothing);
    }

    let dir = std::env::current_dir()
        .map(|d| store::dir_key(&d))
        .unwrap_or(0);
    let ctx = Context {
        store: &db_store,
        dir,
        shell,
        now: store::now(),
    };

    let mut hits = candidates(word, &ctx, &cfg, pinned.as_deref());
    if hits.is_empty() {
        return Ok(Outcome::Nothing);
    }
    hits.truncate(cfg.max_candidates);

    // No terminal means a script, a cron job or CI. Silently substituting a
    // different command in somebody's automation is not a feature.
    let Some(mut tty) = term::Tty::open(cfg.color) else {
        return Ok(Outcome::Nothing);
    };

    let ambiguous = hits.len() > 1
        && hits[1].tier == hits[0].tier
        && hits[1].score >= cfg.ambiguity * hits[0].score
        && pinned.is_none();
    let concern = safety::inspect(&hits[0].name, rest, &cfg.always_confirm);
    let must_ask = match cfg.mode {
        Mode::Safe => true,
        Mode::Unsafe => concern.is_some() || ambiguous,
        Mode::Bypass => false,
    };

    let chosen = if must_ask {
        match ask(&mut tty, word, &hits, concern.as_ref(), within, ambiguous) {
            Some(picked) => picked,
            None => {
                // Two refusals of the same guess and we stop making it.
                let mut writing = store::edit(&db);
                writing.nudge_binding(word, &hits[0].name, -1);
                writing.commit().map_err(at(&db))?;
                return Ok(Outcome::Declined);
            }
        }
    } else {
        if cfg.mode == Mode::Unsafe {
            let notice = format!("zcomplete: {word} -> {}\n", hits[0].name);
            tty.say(&tty.paint("2", &notice));
        }
        0
    };

    let target = hits[chosen].name.clone();
    let mut writing = store::edit(&db);
    remember(&mut writing, word, &target, dir);
    writing.commit().map_err(at(&db))?;
    Ok(Outcome::Run(target))
}

/// The command is about to run, so it counts as used — the preexec hook never
/// saw it, because what the user typed was the word that did not exist.
fn remember(db_store: &mut Store, word: &str, target: &str, dir: u64) {
    let kind = db_store
        .get(target)
        .map_or(Kind::External, |entry| entry.kind);
    db_store.bump(target, kind, 1.0);
    db_store.bump_in(dir, target, 1.0);
    db_store.nudge_binding(word, target, 1);
}

/// What `word` could mean, best first: learned commands that still exist, then
/// merely-installed ones if nothing learned fits, with any pinned shortcut
/// pushed to the front.
fn candidates(word: &str, ctx: &Context, cfg: &Config, pinned: Option<&str>) -> Vec<matcher::Hit> {
    let store = ctx.store;
    let mut hits = matcher::rank(word, ctx, cfg);
    hits.retain(|hit| runnable(store, &hit.name, ctx.shell));

    // Consult PATH when nothing learned fits, and also when everything learned
    // is only a guess: a command the user has never run but is one keystroke
    // away beats one they have run a dozen times and is two.
    let only_guesses = hits.iter().all(matcher::Hit::is_speculative);
    if (hits.is_empty() || only_guesses) && pinned.is_none() && cfg.path_fallback {
        let installed: Vec<store::Entry> = shell::path_commands()
            .into_iter()
            .map(|name| store::Entry {
                name,
                kind: Kind::External,
                rank: 0.0,
                last: 0,
            })
            .collect();
        let mut found = matcher::among(word, &installed, ctx, cfg);
        // Nothing here has been vouched for by use, so the bar is higher: a
        // prefix, or a single-edit slip like `gti` for `git`. Two edits away
        // from a command you have never run is not evidence of anything.
        found.retain(|hit| match hit.tier {
            matcher::Tier::Prefix => true,
            matcher::Tier::Typo => hit.distance <= 1,
            _ => false,
        });
        found.retain(|hit| !hits.iter().any(|known| known.name == hit.name));
        // Only now is it worth a stat call each; the tier filter has to come
        // first or a handful of loose subsequence matches fill the list and
        // squeeze out the one-edit match this is here for.
        found.truncate(cfg.max_candidates);
        found.retain(|hit| shell::on_path(&hit.name));
        hits.extend(found);
        matcher::sort(&mut hits);
    }

    if let Some(pinned) = pinned {
        match hits.iter().position(|hit| hit.name == pinned) {
            Some(at) => hits.swap(0, at),
            None if runnable(store, pinned, ctx.shell) => {
                hits.insert(0, matcher::Hit::pinned(pinned))
            }
            None => {}
        }
    }
    hits
}

/// Put the correction to the user. `None` means they turned it down.
fn ask(
    tty: &mut term::Tty,
    word: &str,
    hits: &[matcher::Hit],
    concern: Option<&safety::Concern>,
    within: Option<Line<'_>>,
    ambiguous: bool,
) -> Option<usize> {
    let typed = tty.paint("1", word);

    if ambiguous && concern.is_none() {
        let options: Vec<String> = hits
            .iter()
            .map(|hit| format!("{:<20} {}", hit.name, tty.paint("2", hit.tier.label())))
            .collect();
        return tty.choose(&format!("zcomplete: '{typed}' could be:"), &options);
    }

    let target = &hits[0].name;
    let action = match within {
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
    tty.ask(&question, concern.is_none()).then_some(0)
}

/// Where each command in a line begins and ends: the start, and everything
/// after an unquoted `|`, `&&`, `;` or `(`. Quotes and backslashes are honoured
/// so a pipe inside a string does not look like a pipeline.
fn commands_in(line: &str) -> Vec<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut start = None;
    let (mut single, mut double, mut escaped) = (false, false, false);

    for (at, byte) in bytes.iter().enumerate() {
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
            _ => {
                if start.is_none() {
                    start = Some(at);
                }
            }
        }
    }
    if let Some(from) = start {
        spans.push((from, line.len()));
    }
    spans
}

/// The command word inside `line[from..to]`, skipping the `FOO=bar` assignments
/// and the wrappers that can come before it.
fn word_span(line: &str, from: usize, to: usize) -> Option<(usize, usize)> {
    let mut at = from;
    loop {
        let segment = line.get(at..to)?;
        at += segment.len() - segment.trim_start().len();
        let word = line.get(at..to)?.split_whitespace().next()?;
        let wrapper = matches!(
            word,
            "sudo" | "doas" | "command" | "builtin" | "nohup" | "exec" | "env" | "time" | "nice"
        );
        if !wrapper && !word.contains('=') {
            return Some((at, at + word.len()));
        }
        at += word.len();
    }
}

/// Words that are not a mistyped command name in the first place: paths,
/// assignments, options, and anything that already runs.
fn correctable(word: &str) -> bool {
    !word.is_empty()
        && !word.contains('/')
        && !word.contains('=')
        && !word.starts_with(['-', '.', '#'])
        && !shell::on_path(word)
}

/// A suggestion is only worth making if the command is still there. External
/// entries are re-checked against PATH every time, which is what stops an
/// uninstalled tool from lingering as a correction.
fn runnable(db_store: &Store, name: &str, shell: Option<Shell>) -> bool {
    match db_store.get(name).map(|entry| entry.kind) {
        Some(Kind::External) | None => shell::on_path(name),
        Some(kind) => kind.usable_in(shell),
    }
}

fn record(args: &[String]) -> Result<i32, Fail> {
    let (flags, operands) = split_flags(args);
    let Some(word) = operands.first() else {
        return Ok(0);
    };
    let cfg = Config::load();
    if !cfg.enabled {
        return Ok(0);
    }

    let shell = flag_value(&flags, "--shell").and_then(|s| Shell::parse(&s));
    let kind = match flag_value(&flags, "--kind").as_deref() {
        Some("shell") => match shell {
            Some(shell) => Kind::Shell(shell),
            None => fail!("--kind shell needs --shell"),
        },
        // `auto` and `external` both have to be on PATH to count. This is the
        // single place that decides what may ever be suggested.
        _ if shell::on_path(word) => Kind::External,
        _ => return Ok(0),
    };

    let db = config::db_path();
    let mut db_store = store::edit(&db);
    if db_store.is_ignored(word) {
        return Ok(0);
    }
    db_store.bump(word, kind, 1.0);
    if let Ok(dir) = std::env::current_dir() {
        db_store.bump_in(store::dir_key(&dir), word, 1.0);
    }
    db_store.commit().map_err(at(&db))?;
    Ok(0)
}

fn query(args: &[String]) -> Result<i32, Fail> {
    let (flags, operands) = split_flags(args);
    let Some(word) = operands.first() else {
        fail!("query needs a word")
    };
    let cfg = Config::load();
    let db_store = Store::open(&config::db_path());
    let dir = std::env::current_dir()
        .map(|d| store::dir_key(&d))
        .unwrap_or(0);
    let ctx = Context {
        store: &db_store,
        dir,
        shell: flag_value(&flags, "--shell").and_then(|s| Shell::parse(&s)),
        now: store::now(),
    };

    let limit = flag_value(&flags, "-n")
        .or_else(|| flag_value(&flags, "--limit"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(if flags.iter().any(|f| f == "--score") {
            10
        } else {
            1
        });

    let too_short = db_store.sticky(word).is_none() && word.chars().count() < cfg.min_input;
    let hits = if too_short || !correctable(word) {
        Vec::new()
    } else {
        candidates(word, &ctx, &cfg, db_store.sticky(word))
    };
    if hits.is_empty() {
        if shell::on_path(word) {
            eprintln!("zcomplete: '{word}' is already a command");
        }
        return Ok(NO_MATCH);
    }

    if flags.iter().any(|f| f == "-i" || f == "--interactive") {
        let names: Vec<String> = hits.iter().map(|hit| hit.name.clone()).collect();
        return match pick_one(&names, word, cfg.color)? {
            Some(name) => {
                println!("{name}");
                Ok(FOUND)
            }
            None => Ok(DECLINED),
        };
    }

    if flags.iter().any(|f| f == "--score") {
        for hit in hits.iter().take(limit) {
            println!(
                "{:<24} {:<9} {:>8.1} {:>8.1}",
                hit.name,
                hit.tier.label(),
                hit.score,
                hit.rank
            );
        }
    } else {
        for hit in hits.iter().take(limit) {
            println!("{}", hit.name);
        }
    }
    Ok(FOUND)
}

/// fzf if it is installed, and a plain numbered menu on the terminal if not.
fn pick_one(names: &[String], word: &str, color: Color) -> Result<Option<String>, Fail> {
    match shell::which("fzf") {
        Some(fzf) => pick_with(&fzf, names),
        None => Ok(term::Tty::open(color)
            .and_then(|mut tty| tty.choose(&format!("matches for '{word}':"), names))
            .map(|at| names[at].clone())),
    }
}

fn pick_with(picker: &std::path::Path, options: &[String]) -> Result<Option<String>, Fail> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(picker)
        .arg("--no-multi")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        write!(stdin, "{}", options.join("\n"))?;
    }
    let out = child.wait_with_output()?;
    let picked = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok((!picked.is_empty()).then_some(picked))
}

fn stats(args: &[String]) -> Result<i32, Fail> {
    let (flags, _) = split_flags(args);
    let limit = flag_value(&flags, "-n")
        .and_then(|n| n.parse().ok())
        .unwrap_or(25);

    let db_store = Store::open(&config::db_path());
    let at = store::now();
    let mut ranked: Vec<_> = db_store
        .entries
        .iter()
        .map(|entry| (store::frecency(entry.rank, entry.last, at), entry))
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0));

    if ranked.is_empty() {
        println!("nothing learned yet - run `zcomplete import` to seed from your shell history");
        return Ok(0);
    }

    println!("{:>8}  {:<14} {}", "score", "last used", "command");
    for (score, entry) in ranked.iter().take(limit) {
        let kind = match entry.kind {
            Kind::External => String::new(),
            Kind::Shell(shell) => format!(" ({})", shell.name()),
        };
        println!(
            "{score:>8.1}  {:<14} {}{kind}",
            ago(at.saturating_sub(entry.last)),
            entry.name
        );
    }

    let pinned: Vec<_> = db_store
        .bindings
        .iter()
        .filter(|b| b.weight >= store::STICKY_AT)
        .collect();
    if !pinned.is_empty() {
        println!("\nshortcuts");
        for binding in pinned {
            println!("  {:<12} -> {}", binding.input, binding.target);
        }
    }
    Ok(0)
}

fn import(args: &[String]) -> Result<i32, Fail> {
    let (flags, operands) = split_flags(args);
    if let Some(path) = flag_value(&flags, "--restore") {
        return restore(&PathBuf::from(path));
    }

    let shells: Vec<Shell> = match operands.first() {
        Some(name) => match Shell::parse(name) {
            Some(shell) => vec![shell],
            None => fail!("unknown shell '{name}'"),
        },
        None => vec![Shell::Zsh, Shell::Bash, Shell::Fish],
    };
    let dry = flags.iter().any(|f| f == "--dry-run");

    let db = config::db_path();
    let mut db_store = store::edit(&db);
    let mut added = 0usize;
    let mut skipped = 0usize;

    for shell in shells {
        let Some(path) = shell::history_path(shell) else {
            continue;
        };
        let (seen, missing) = absorb(&mut db_store, shell, &path, dry)?;
        added += seen;
        skipped += missing;
        println!("{}: {seen} commands from {}", shell.name(), path.display());
    }

    if dry {
        println!("(dry run, nothing written)");
        return Ok(0);
    }
    let stamp = store::now().saturating_sub(30 * 24 * 3600);
    for entry in &mut db_store.entries {
        if entry.last == 0 {
            entry.last = stamp;
        }
    }
    db_store.compact();
    db_store.touch();
    db_store.commit().map_err(at(&db))?;
    println!("learned {added} invocations ({skipped} skipped as not installed)");
    Ok(0)
}

/// Read one shell's history into the store. Returns how many invocations were
/// learned and how many named something that is no longer installed.
fn absorb(
    db_store: &mut Store,
    shell: Shell,
    path: &std::path::Path,
    dry: bool,
) -> Result<(usize, usize), Fail> {
    let (mut learned, mut missing) = (0, 0);
    for entry in shell::read_history(shell, path).map_err(at(path))? {
        let Some(word) = shell::command_word(&entry.line) else {
            continue;
        };
        if db_store.is_ignored(word) {
            continue;
        }
        if !shell::on_path(word) {
            missing += 1;
            continue;
        }
        learned += 1;
        if !dry {
            // History is evidence, not activity: weight each occurrence below a
            // live invocation, and keep the timestamp it actually happened at.
            db_store.seed(word, Kind::External, 0.4, entry.at.unwrap_or(0));
        }
    }
    Ok((learned, missing))
}

fn restore(path: &std::path::Path) -> Result<i32, Fail> {
    let text = std::fs::read_to_string(path).map_err(at(path))?;
    let db = config::db_path();
    let mut db_store = store::edit(&db);
    let mut count = 0;
    for line in text.lines() {
        let mut fields = line.split('\t');
        let (Some(name), Some(rank), Some(last)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(rank), Ok(last)) = (rank.parse::<f32>(), last.parse::<u64>()) else {
            continue;
        };
        db_store.seed(name, Kind::External, rank, last);
        count += 1;
    }
    db_store.commit().map_err(at(&db))?;
    println!("restored {count} commands");
    Ok(0)
}

fn export() -> Result<i32, Fail> {
    let db_store = Store::open(&config::db_path());
    let mut out = std::io::stdout().lock();
    for entry in &db_store.entries {
        writeln!(out, "{}\t{:.3}\t{}", entry.name, entry.rank, entry.last)?;
    }
    Ok(0)
}

fn forget(args: &[String]) -> Result<i32, Fail> {
    let db = config::db_path();
    let mut db_store = store::edit(&db);
    if args.iter().any(|a| a == "--all") {
        db_store.clear();
        db_store.commit().map_err(at(&db))?;
        println!("database emptied");
        return Ok(0);
    }
    if args.is_empty() {
        fail!("forget needs a command name, or --all")
    }
    for name in args {
        if db_store.forget(name) {
            println!("forgot {name}");
        } else {
            println!("{name} was not in the database");
        }
    }
    db_store.commit().map_err(at(&db))?;
    Ok(0)
}

fn bind(args: &[String]) -> Result<i32, Fail> {
    let [word, target] = args else {
        fail!("bind needs a word and a command, as in `zcomplete bind gs git`")
    };
    if target.split_whitespace().count() > 1 {
        fail!("a shortcut can only point at one command; make '{target}' a shell alias instead")
    }
    let db = config::db_path();
    let mut db_store = store::edit(&db);
    if !shell::on_path(target) && db_store.get(target).is_none() {
        fail!("'{target}' is not a command on PATH and is not in the database")
    }
    db_store.bump(target, Kind::External, 0.0);
    db_store.nudge_binding(word, target, store::PINNED);
    db_store.commit().map_err(at(&db))?;
    println!("{word} -> {target}");
    Ok(0)
}

fn unbind(args: &[String]) -> Result<i32, Fail> {
    let Some(word) = args.first() else {
        fail!("unbind needs a word")
    };
    let db = config::db_path();
    let mut db_store = store::edit(&db);
    if db_store.unbind(word) {
        db_store.commit().map_err(at(&db))?;
        println!("unbound {word}");
        Ok(0)
    } else {
        println!("{word} was not bound");
        Ok(NO_MATCH)
    }
}

fn ignore(args: &[String]) -> Result<i32, Fail> {
    let db = config::db_path();
    let mut db_store = store::edit(&db);
    let (flags, names) = split_flags(args);

    if names.is_empty() {
        if db_store.ignored.is_empty() {
            println!("nothing is ignored");
        }
        for name in &db_store.ignored {
            println!("{name}");
        }
        return Ok(0);
    }

    let removing = flags.iter().any(|f| f == "--remove" || f == "-r");
    for name in names {
        if removing {
            db_store.unignore(&name);
            println!("no longer ignoring {name}");
        } else {
            db_store.forget(&name);
            db_store.ignore(&name);
            println!("ignoring {name}");
        }
    }
    db_store.commit().map_err(at(&db))?;
    Ok(0)
}

fn mode(args: &[String]) -> Result<i32, Fail> {
    let cfg = Config::load();
    let Some(name) = args.first() else {
        println!("{} - {}", cfg.mode, cfg.mode.describe());
        return Ok(0);
    };
    let Some(mode) = Mode::parse(name) else {
        fail!("unknown mode '{name}' (safe, unsafe or bypass)")
    };
    let path = cfg
        .set("mode", &format!("\"{mode}\""))
        .map_err(at(&config::config_path()))?;
    println!("{mode} - {} ({})", mode.describe(), path.display());
    Ok(0)
}

fn switch(on: bool) -> Result<i32, Fail> {
    let cfg = Config::load();
    cfg.set("enabled", if on { "true" } else { "false" })
        .map_err(at(&config::config_path()))?;
    println!("corrections {}", if on { "enabled" } else { "disabled" });
    Ok(0)
}

fn doctor() -> Result<i32, Fail> {
    let cfg = Config::load();
    let db = config::db_path();
    let db_store = Store::open(&db);
    let mut problems = 0;

    println!("zcomplete {VERSION}");
    if let Some(path) = shell::which("zcomplete") {
        println!("  binary          {}", path.display());
    }
    for (label, path) in config::describe_paths() {
        let mark = if config::exists(&path) {
            ""
        } else {
            "  (not created yet)"
        };
        println!("  {label:<15} {}{mark}", path.display());
    }
    println!("  mode            {} - {}", cfg.mode, cfg.mode.describe());
    println!(
        "  corrections     {}",
        if cfg.enabled { "on" } else { "off" }
    );
    println!("  learned         {} commands", db_store.entries.len());
    if db_store.is_read_only() {
        println!("\n  the database was written by a newer zcomplete and is being left alone;");
        println!("  upgrade, or delete {} to start over", db.display());
        problems += 1;
    }

    if db_store.entries.len() < 10 {
        println!(
            "\n  the database is nearly empty; `zcomplete import` seeds it from shell history"
        );
        problems += 1;
    }

    println!("\nshells");
    for shell in [Shell::Zsh, Shell::Bash, Shell::Fish] {
        if shell::which(shell.name()).is_some() {
            problems += report_shell(shell);
        }
    }

    print!("\nterminal        ");
    match term::Tty::open(Color::Auto) {
        Some(tty) => println!(
            "/dev/tty available, colour {}",
            if tty.color { "on" } else { "off" }
        ),
        None => {
            println!("no /dev/tty - safe mode cannot ask, so corrections will be skipped");
            problems += 1;
        }
    }

    println!(
        "\n{}",
        match problems {
            0 => "no problems found".to_string(),
            1 => "one thing to look at".to_string(),
            n => format!("{n} things to look at"),
        }
    );
    Ok(if problems == 0 { 0 } else { NO_MATCH })
}

/// One line per installed shell, plus what to do about it. Returns how many
/// things need attention.
fn report_shell(shell: Shell) -> i32 {
    let mut problems = 0;
    let hooked = shell::rc_files(shell)
        .iter()
        .any(|rc| std::fs::read_to_string(rc).is_ok_and(|text| text.contains("zcomplete init")));
    let login = std::env::var("SHELL")
        .ok()
        .as_deref()
        .and_then(Shell::parse)
        .is_some_and(|s| s == shell);

    println!(
        "  {:<6} {}",
        shell.name(),
        match (hooked, login) {
            (true, true) => "integrated (your login shell)",
            (true, false) => "integrated",
            (false, true) => "NOT integrated - and this is your login shell",
            (false, false) => "not integrated",
        }
    );
    if !hooked {
        problems += 1;
        for rc in shell::rc_files(shell) {
            println!("         add to {}:  {}", rc.display(), setup_line(shell));
        }
    }
    if shell == Shell::Bash && bash_major().is_some_and(|version| version < 4) {
        println!("         this bash predates command_not_found_handle, so corrections");
        println!("         are offered at the next prompt instead of run in place;");
        println!("         `brew install bash` for the full behaviour");
        problems += 1;
    }
    problems
}

fn setup_line(shell: Shell) -> &'static str {
    match shell {
        Shell::Zsh => "eval \"$(zcomplete init zsh)\"",
        Shell::Bash => "eval \"$(zcomplete init bash)\"",
        Shell::Fish => "zcomplete init fish | source",
    }
}

fn bash_major() -> Option<u32> {
    let output = std::process::Command::new("bash")
        .arg("-c")
        .arg("echo $BASH_VERSINFO")
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn ago(seconds: u64) -> String {
    match seconds {
        0..=90 => "just now".to_string(),
        s if s < 5_400 => format!("{} min ago", s / 60),
        s if s < 172_800 => format!("{} hours ago", s / 3_600),
        s if s < 1_209_600 => format!("{} days ago", s / 86_400),
        s if s < 5_184_000 => format!("{} weeks ago", s / 604_800),
        s => format!("{} months ago", s / 2_592_000),
    }
}

/// Splits `--flag`/`--flag=value`/`-n 5` from operands, honouring a `--`
/// terminator so a user's own arguments are never read as ours.
fn split_flags(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut flags = Vec::new();
    let mut operands = Vec::new();
    let mut iter = args.iter().peekable();

    while let Some(arg) = iter.next() {
        if arg == "--" {
            operands.extend(iter.cloned());
            break;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            flags.push(arg.clone());
            let takes_value = matches!(
                arg.as_str(),
                "--shell" | "--kind" | "-n" | "--limit" | "--restore" | "--only"
            );
            if takes_value {
                if let Some(value) = iter.next() {
                    flags.push(value.clone());
                }
            }
            continue;
        }
        operands.push(arg.clone());
    }
    (flags, operands)
}

fn flag_value(flags: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    flags.iter().enumerate().find_map(|(i, flag)| {
        if let Some(inline) = flag.strip_prefix(&prefix) {
            Some(inline.to_string())
        } else if flag == name {
            flags.get(i + 1).cloned()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn flags_stop_at_the_terminator() {
        let (flags, operands) =
            split_flags(&words(&["--shell", "zsh", "--", "rm", "-rf", "build"]));
        assert_eq!(flags, words(&["--shell", "zsh"]));
        assert_eq!(operands, words(&["rm", "-rf", "build"]));
    }

    #[test]
    fn inline_flag_values_work_too() {
        let (flags, _) = split_flags(&words(&["--shell=fish"]));
        assert_eq!(flag_value(&flags, "--shell").as_deref(), Some("fish"));
    }

    #[test]
    fn a_users_own_flags_are_not_parsed_as_ours() {
        let (_, operands) = split_flags(&words(&["--", "ls", "-n", "3"]));
        assert_eq!(operands, words(&["ls", "-n", "3"]));
    }

    /// The one rule the whole tool rests on: a name only ever leaves here if the
    /// shell could actually run it. Deleting the `runnable` filter in
    /// `candidates` used to pass every other test in the suite.
    #[test]
    fn a_command_that_is_no_longer_installed_is_not_offered() {
        let mut store = Store::default();
        store.bump("definitely-not-installed-xyzzy", Kind::External, 500.0);
        store.bump("sh", Kind::External, 1.0);

        assert!(!runnable(&store, "definitely-not-installed-xyzzy", None));
        assert!(runnable(&store, "sh", None));
        assert!(!runnable(&store, "also-never-installed-xyzzy", None));

        // A shell word is only usable in the shell that defined it.
        store.bump("mygitfn", Kind::Shell(Shell::Zsh), 5.0);
        assert!(runnable(&store, "mygitfn", Some(Shell::Zsh)));
        assert!(!runnable(&store, "mygitfn", Some(Shell::Fish)));
    }

    #[test]
    fn errors_name_the_file_and_drop_the_errno() {
        let err = std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "No such file or directory (os error 2)",
        );
        assert_eq!(plain(&err), "No such file or directory");
        let fail = at(std::path::Path::new("/tmp/seed.tsv"))(err);
        assert_eq!(fail.to_string(), "/tmp/seed.tsv: No such file or directory");
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
        // A separator inside quotes is text, not a pipeline.
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
        // The correction is the second command, so the first is left alone.
        assert_eq!(preview("printf hi | ct", "ct", "cat"), "printf hi | cat");
    }

    #[test]
    fn relative_times_read_like_english() {
        assert_eq!(ago(10), "just now");
        assert_eq!(ago(600), "10 min ago");
        assert_eq!(ago(7_200), "2 hours ago");
        assert_eq!(ago(200_000), "2 days ago");
    }
}
