//! Dispatch, exit codes, and the error type. 2 means a real error.

use std::fmt;

use store::Shell;

macro_rules! fail {
    ($($arg:tt)*) => { return Err(Fail(format!($($arg)*))) };
}

// After the macro: `fail!` is only in scope for modules declared below it.
mod admin;
mod correct;
mod matcher;
mod safety;
mod shell;
mod store;
mod term;

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

pub(crate) const FOUND: i32 = 0;
pub(crate) const NO_MATCH: i32 = 1;
pub(crate) const DECLINED: i32 = 3;
pub(crate) const DISABLED: i32 = 4;
/// The hook found the answer in a fork that cannot run it. Passed back out so
/// the real shell does the correction at the next prompt.
pub(crate) const DEFERRED: i32 = 5;

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

pub(crate) fn at(path: &std::path::Path) -> impl Fn(std::io::Error) -> Fail + '_ {
    move |err| Fail(format!("{}: {}", path.display(), plain(&err)))
}

pub(crate) fn plain(err: &std::io::Error) -> String {
    let text = err.to_string();
    match text.rfind(" (os error ") {
        Some(cut) => text[..cut].to_owned(),
        None => text,
    }
}

fn main() {
    // Rust ignores SIGPIPE, so `stats | head` panics instead of ending quietly.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = run(&args).unwrap_or_else(|err| {
        eprintln!("zcomplete: {err}");
        2
    });
    std::process::exit(code);
}

fn run(args: &[String]) -> Result<i32, Fail> {
    let Some((verb, rest)) = args.split_first() else {
        print!("{}", usage());
        return Ok(0);
    };

    match verb.as_str() {
        "init" => init(rest),
        "resolve" => correct::resolve(rest),
        "retry" => correct::retry(rest),
        "record" => correct::record(rest),
        "query" => correct::query(rest),
        "stats" | "list" => admin::stats(rest),
        "import" => admin::import(rest),
        "forget" | "remove" => admin::forget(rest),
        "bind" => admin::bind(rest),
        "unbind" => admin::unbind(rest),
        "ignore" => admin::ignore(rest),
        "mode" => admin::mode(rest),
        "on" => admin::switch(true),
        "off" => admin::switch(false),
        "doctor" => admin::doctor(),
        "safe" | "unsafe" | "bypass" | "--safe" | "--unsafe" | "--bypass" => {
            admin::mode(&[verb.trim_start_matches("--").to_string()])
        }
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

Safety
  mode                     show the current confirmation mode
  safe                     confirm every correction (the default)
  unsafe                   run ordinary corrections, confirm dangerous ones
  bypass                   never confirm
  on | off                 enable or disable corrections entirely

At the prompt
  y or enter               run the correction
  n or i                   leave it alone
  u                        also fix the subcommand, when one is offered
  1-9                      pick from the list when the match is unclear

Everything else
  init <zsh|bash|fish>     print the shell integration
  query [cmd] <word>       show what a word would resolve to
  stats [cmd] [-n N]       learned commands, strongest first
  import [zsh|bash|fish]   seed the database from shell history
  bind <word> <command>    always resolve <word> to <command>
  unbind <word>            drop a pinned or learned shortcut
  forget <command>...      remove commands from the database
  ignore <command>...      keep a command out of every suggestion
  doctor                   check the installation

Setup
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

pub(crate) const VALUED: &[&str] = &["--shell", "--kind", "--status", "-n", "--limit", "--only"];

pub(crate) fn split_flags(args: &[String]) -> (Vec<String>, Vec<String>) {
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
            if VALUED.contains(&arg.as_str()) {
                flags.extend(iter.next().cloned());
            }
            continue;
        }
        operands.push(arg.clone());
    }
    (flags, operands)
}

pub(crate) fn reject_unknown(flags: &[String], known: &[&str]) -> Result<(), Fail> {
    let mut expecting_value = false;
    for flag in flags {
        if expecting_value {
            expecting_value = false;
            continue;
        }
        let name = flag.split('=').next().unwrap_or(flag);
        if !name.starts_with('-') {
            continue;
        }
        if !known.contains(&name) {
            fail!("unknown option '{name}' (try `zcomplete help`)")
        }
        expecting_value = !flag.contains('=') && VALUED.contains(&name);
    }
    Ok(())
}

pub(crate) fn flag_value(flags: &[String], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    flags
        .iter()
        .enumerate()
        .find_map(|(i, flag)| match flag.strip_prefix(&prefix) {
            Some(inline) => Some(inline.to_owned()),
            None if flag == name => flags.get(i + 1).cloned(),
            None => None,
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

        let (flags, _) = split_flags(&words(&["--shell=fish"]));
        assert_eq!(flag_value(&flags, "--shell").as_deref(), Some("fish"));

        let (_, operands) = split_flags(&words(&["--", "ls", "-n", "3"]));
        assert_eq!(operands, words(&["ls", "-n", "3"]));
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
}
