//! The commands you run yourself. Nothing here is on the hot path.

use crate::correct::{call_in, commands_in, is_verb};
use crate::store::{self, Kind, Mode, Shell, Store};
use crate::{at, shell, term};
use crate::{flag_value, reject_unknown, split_flags, Fail, NO_MATCH, VERSION};

pub(crate) fn stats(args: &[String]) -> Result<i32, Fail> {
    let (flags, operands) = split_flags(args);
    reject_unknown(&flags, &["-n", "--limit"])?;
    let limit = flag_value(&flags, "-n")
        .or_else(|| flag_value(&flags, "--limit"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(25);

    // Folded first: what a person asked to see should be what has happened, not
    // what has happened minus whatever this shell has not flushed yet.
    let db = store::db_path();
    let mut writing = store::edit(&db);
    let folded = crate::correct::fold(&mut writing);
    let db_store = writing.commit_taking(folded).map_err(at(&db))?;
    let at = store::now();

    if let Some(parent) = operands.first() {
        let mut ranked = db_store.verbs(parent);
        if ranked.is_empty() {
            println!("nothing learned about {parent} yet");
            return Ok(NO_MATCH);
        }
        // By name on a tie: the scoped table is a HashMap, so equally used verbs
        // would swap places between runs.
        ranked.sort_by(|a, b| {
            store::frecency(b.rank, b.last, at)
                .total_cmp(&store::frecency(a.rank, a.last, at))
                .then_with(|| a.name.cmp(&b.name))
        });
        println!("{:>8}  {:<14} {}", "score", "last used", parent);
        for entry in ranked.iter().take(limit) {
            println!(
                "{:>8}  {:<14} {parent} {}",
                store::tenths(store::frecency(entry.rank, entry.last, at)),
                ago(at.saturating_sub(entry.last)),
                entry.name
            );
        }
        return Ok(0);
    }

    let mut ranked: Vec<_> = db_store
        .entries
        .iter()
        .map(|entry| (store::frecency(entry.rank, entry.last, at), entry))
        .collect();
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));

    if ranked.is_empty() {
        println!("nothing learned yet - run `zcomplete import` to seed from your shell history");
        return Ok(0);
    }

    println!("{:>8}  {:<14} command", "score", "last used");
    for (score, entry) in ranked.iter().take(limit) {
        let kind = match entry.kind {
            Kind::External => String::new(),
            Kind::Shell(shell) => format!(" ({})", shell.name()),
        };
        println!(
            "{:>8}  {:<14} {}{kind}",
            store::tenths(*score),
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

pub(crate) fn import(args: &[String]) -> Result<i32, Fail> {
    let (flags, operands) = split_flags(args);
    reject_unknown(&flags, &["--dry-run"])?;
    let shells: Vec<Shell> = match operands.first() {
        Some(name) => match Shell::parse(name) {
            Some(shell) => vec![shell],
            None => fail!("unknown shell '{name}'"),
        },
        None => vec![Shell::Zsh, Shell::Bash, Shell::Fish],
    };
    let dry = flags.iter().any(|f| f == "--dry-run");

    let db = store::db_path();
    let mut db_store = store::edit(&db);
    let mut added = 0usize;
    let mut skipped = 0usize;
    let mut verbs: std::collections::HashMap<(String, String), usize> = Default::default();

    for shell in shells {
        let Some(path) = shell::history_path(shell) else {
            continue;
        };
        let defined: std::collections::HashSet<String> =
            shell::defined_words(shell).into_iter().collect();
        let (seen, missing) = absorb(&mut db_store, shell, &path, dry, &mut verbs, &defined)?;
        added += seen;
        skipped += missing;
        println!("{}: {seen} commands from {}", shell.name(), path.display());
    }

    // A subcommand you use turns up in history many times; a filename turns up
    // once. History does not record what exited zero, so repetition is all there is.
    let mut learned_verbs = 0usize;
    for ((parent, verb), count) in &verbs {
        if *count >= 2 {
            learned_verbs += 1;
            db_store.bump_in(store::sub_scope(parent), verb, 0.5 * *count as f32);
        }
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
    println!("learned {added} invocations and {learned_verbs} subcommands ({skipped} skipped as not installed)");
    Ok(0)
}

fn absorb(
    db_store: &mut Store,
    shell: Shell,
    path: &std::path::Path,
    dry: bool,
    verbs: &mut std::collections::HashMap<(String, String), usize>,
    defined: &std::collections::HashSet<String>,
) -> Result<(usize, usize), Fail> {
    let (mut learned, mut missing) = (0, 0);
    for entry in shell::read_history(shell, path).map_err(at(path))? {
        let Some(word) = shell::command_word(&entry.line) else {
            continue;
        };
        if db_store.is_ignored(word) {
            continue;
        }
        // An alias is not on PATH and never will be, so the only evidence that
        // `gs` is a command is that the shell says so and history says you ran it.
        let kind = match shell::on_path(word) {
            true => Kind::External,
            false if defined.contains(word) => Kind::Shell(shell),
            false => {
                missing += 1;
                continue;
            }
        };
        learned += 1;
        if !dry {
            db_store.seed(word, kind, 0.4, entry.at.unwrap_or(0));
        }
        for span in commands_in(&entry.line) {
            let Some(call) = call_in(&entry.line, span) else {
                continue;
            };
            let verb = &entry.line[call.verb.0..call.verb.1];
            // No `plausible_verb` here: the directory a history line ran in is gone.
            if is_verb(verb) && verb.len() <= 24 && shell::on_path(call.parent) {
                *verbs
                    .entry((call.parent.to_owned(), verb.to_owned()))
                    .or_default() += 1;
            }
        }
    }
    Ok((learned, missing))
}

pub(crate) fn forget(args: &[String]) -> Result<i32, Fail> {
    let db = store::db_path();
    let mut db_store = store::edit(&db);
    // Buffered lines first, or a shell that has been counting `rm` all morning
    // puts it back at its next flush and the forget looks like it never happened.
    let folded = crate::correct::fold(&mut db_store);
    // On its own, never alongside names: `forget git --all` reads as a careless
    // way of saying `forget git`, and emptying the database on it is not a
    // mistake anyone gets to take back.
    if args.iter().any(|a| a == "--all") {
        if args.len() > 1 {
            fail!("--all empties the database, so it takes no command names")
        }
        db_store.clear();
        db_store.commit_taking(folded).map_err(at(&db))?;
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
    db_store.commit_taking(folded).map_err(at(&db))?;
    Ok(0)
}

pub(crate) fn bind(args: &[String]) -> Result<i32, Fail> {
    let [word, target] = args else {
        fail!("bind needs a word and a command, as in `zcomplete bind gs git`")
    };
    if target.split_whitespace().count() > 1 {
        fail!("a shortcut can only point at one command; make '{target}' a shell alias instead")
    }
    // The same shape a command word has to have to be looked up at all. Without
    // this an empty word, `a b` or `../../etc/passwd` all bound happily and then
    // sat there for good: a pin outranks everything when the table is evicted,
    // so a shortcut that can never be typed pushed out ones that can.
    if !crate::correct::is_plain_name(word) {
        fail!("'{word}' is not a word a shell would read as a command, so it could never be typed")
    }
    let db = store::db_path();
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

pub(crate) fn unbind(args: &[String]) -> Result<i32, Fail> {
    let Some(word) = args.first() else {
        fail!("unbind needs a word")
    };
    let db = store::db_path();
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

pub(crate) fn ignore(args: &[String]) -> Result<i32, Fail> {
    let (flags, names) = split_flags(args);
    reject_unknown(&flags, &["--remove", "-r"])?;
    let db = store::db_path();
    let mut db_store = store::edit(&db);

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

pub(crate) fn mode(args: &[String]) -> Result<i32, Fail> {
    let db = store::db_path();
    let mut db_store = store::edit(&db);
    let Some(name) = args.first() else {
        let mode = db_store.mode();
        println!("{mode} - {}", mode.describe());
        return Ok(0);
    };
    let Some(mode) = Mode::parse(name) else {
        fail!("unknown mode '{name}' (safe, unsafe or bypass)")
    };
    db_store.set_mode(mode);
    db_store.commit().map_err(at(&db))?;
    println!("{mode} - {}", mode.describe());
    // The environment wins over the database, so saying nothing here would be
    // reporting a change that no shell carrying this variable will act on.
    if let Some(forced) = std::env::var_os("ZCOMPLETE_MODE") {
        println!(
            "note: ZCOMPLETE_MODE={} overrides this wherever it is exported",
            forced.to_string_lossy()
        );
    }
    Ok(0)
}

pub(crate) fn switch(on: bool) -> Result<i32, Fail> {
    let db = store::db_path();
    let mut db_store = store::edit(&db);
    db_store.set_enabled(on);
    db_store.commit().map_err(at(&db))?;
    println!("corrections {}", if on { "enabled" } else { "disabled" });
    Ok(0)
}

pub(crate) fn doctor() -> Result<i32, Fail> {
    let db = store::db_path();
    let db_store = Store::open(&db);
    let mut problems = 0;

    println!("zcomplete {VERSION}");
    if let Some(path) = shell::which("zcomplete") {
        println!("  binary          {}", path.display());
    }
    for (label, path) in [
        ("database", db.clone()),
        ("data directory", store::data_dir()),
    ] {
        let mark = if path.exists() {
            ""
        } else {
            "  (not created yet)"
        };
        println!("  {label:<15} {}{mark}", path.display());
    }
    let mode = db_store.mode();
    println!("  mode            {mode} - {}", mode.describe());
    println!(
        "  corrections     {}",
        if db_store.enabled() { "on" } else { "off" }
    );
    let with_verbs = db_store
        .entries
        .iter()
        .filter(|entry| db_store.takes_verbs(&entry.name))
        .count();
    println!(
        "  learned         {} commands, {with_verbs} of them with subcommands",
        db_store.entries.len()
    );
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
    match term::Tty::open() {
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
        println!(
            "         `zcomplete init --{}` adds it to {}",
            shell.name(),
            {
                let files = shell::rc_files(shell);
                files.first().map_or_else(
                    || "your shell config".to_string(),
                    |rc| rc.display().to_string(),
                )
            }
        );
    }
    if shell == Shell::Bash && bash_major().is_some_and(|version| version < 4) {
        println!("         this bash predates command_not_found_handle, so corrections");
        println!("         are offered at the next prompt instead of run in place;");
        println!("         `brew install bash` for the full behaviour");
        problems += 1;
    }
    problems
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
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times_read_like_english() {
        assert_eq!(ago(10), "just now");
        assert_eq!(ago(600), "10 min ago");
        assert_eq!(ago(7_200), "2 hours ago");
        assert_eq!(ago(200_000), "2 days ago");
    }
}
