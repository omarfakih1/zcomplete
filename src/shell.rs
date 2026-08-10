//! PATH lookups, the init snippets, and reading a shell's history file.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::store::Shell;

pub fn init_script(shell: Shell) -> &'static str {
    match shell {
        Shell::Zsh => include_str!("init/zcomplete.zsh"),
        Shell::Bash => include_str!("init/zcomplete.bash"),
        Shell::Fish => include_str!("init/zcomplete.fish"),
    }
}

/// Is `name` an executable on PATH right now. This is what keeps a word the
/// user types often but that is not installed — `clean`, `deploy`, a typo they
/// keep making — from ever becoming a suggestion.
pub fn on_path(name: &str) -> bool {
    if name.contains('/') {
        return false;
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .any(|dir| executable(&dir.join(name)))
}

fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Every name on PATH, as a cold-start fallback for when nothing the user has
/// actually run looks like what they typed. Deliberately does not stat each
/// file — that is a syscall per entry across a few thousand of them, and the
/// handful of names that survive ranking get checked by `on_path` anyway.
pub fn path_commands() -> Vec<String> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    let mut seen = std::collections::HashSet::new();
    std::env::split_paths(&path)
        .filter_map(|dir| fs::read_dir(dir).ok())
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| executable(candidate))
}

/// A command line from a history file, and when it ran if the format says so.
pub struct Recalled {
    pub line: String,
    pub at: Option<u64>,
}

pub fn history_path(shell: Shell) -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let from_env = |var: &str| {
        std::env::var_os(var)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let path = match shell {
        Shell::Zsh => from_env("HISTFILE").unwrap_or_else(|| home.join(".zsh_history")),
        Shell::Bash => from_env("HISTFILE").unwrap_or_else(|| home.join(".bash_history")),
        Shell::Fish => match std::env::var_os("XDG_DATA_HOME") {
            Some(data) if !data.is_empty() => PathBuf::from(data).join("fish/fish_history"),
            _ => home.join(".local/share/fish/fish_history"),
        },
    };
    path.exists().then_some(path)
}

pub fn read_history(shell: Shell, path: &Path) -> std::io::Result<Vec<Recalled>> {
    // History files collect whatever the terminal was fed, including bytes that
    // are not UTF-8. Read lossily rather than refusing to import at all.
    let text = String::from_utf8_lossy(&fs::read(path)?).into_owned();
    Ok(match shell {
        Shell::Zsh => zsh_history(&text),
        Shell::Bash => bash_history(&text),
        Shell::Fish => fish_history(&text),
    })
}

/// Extended format is `: <unix time>:<elapsed>;<command>`; plain format is the
/// command on its own. A trailing backslash continues onto the next line.
fn zsh_history(text: &str) -> Vec<Recalled> {
    let mut out = Vec::new();
    let mut pending: Option<String> = None;

    for raw in text.lines() {
        let joined = match pending.take() {
            Some(mut head) => {
                head.push_str(raw);
                head
            }
            None => raw.to_owned(),
        };
        if let Some(head) = joined.strip_suffix('\\') {
            pending = Some(head.to_owned());
            continue;
        }

        let (at, line) = match joined.strip_prefix(": ") {
            Some(rest) => match rest.split_once(';') {
                Some((meta, command)) => (
                    meta.split(':').next().and_then(|t| t.trim().parse().ok()),
                    command.to_owned(),
                ),
                None => (None, joined.clone()),
            },
            None => (None, joined.clone()),
        };
        if !line.trim().is_empty() {
            out.push(Recalled { line, at });
        }
    }
    out
}

fn bash_history(text: &str) -> Vec<Recalled> {
    let mut out = Vec::new();
    let mut stamp = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('#') {
            if let Ok(seconds) = rest.trim().parse::<u64>() {
                stamp = Some(seconds);
                continue;
            }
        }
        if !line.trim().is_empty() {
            out.push(Recalled {
                line: line.to_owned(),
                at: stamp.take(),
            });
        }
    }
    out
}

/// fish writes a YAML-ish stream: `- cmd: git status` followed by `  when: …`.
fn fish_history(text: &str) -> Vec<Recalled> {
    let mut out: Vec<Recalled> = Vec::new();
    for line in text.lines() {
        if let Some(command) = line.strip_prefix("- cmd: ") {
            out.push(Recalled {
                line: unescape_fish(command),
                at: None,
            });
        } else if let Some(when) = line.trim().strip_prefix("when: ") {
            if let (Some(last), Ok(seconds)) = (out.last_mut(), when.trim().parse()) {
                last.at = Some(seconds);
            }
        }
    }
    out
}

fn unescape_fish(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_owned();
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match (ch, chars.clone().next()) {
            ('\\', Some('n')) => {
                chars.next();
                out.push('\n');
            }
            ('\\', Some('\\')) => {
                chars.next();
                out.push('\\');
            }
            _ => out.push(ch),
        }
    }
    out
}

/// The command word a history line actually invoked, skipping the wrappers and
/// environment assignments that come first.
pub fn command_word(line: &str) -> Option<&str> {
    let mut rest = line.trim_start();
    loop {
        let word = rest.split_whitespace().next()?;
        let wrapper = matches!(
            word,
            "sudo" | "doas" | "command" | "builtin" | "nohup" | "exec" | "env" | "time" | "nice" | "stdbuf"
        );
        if !wrapper && !word.contains('=') {
            return (!word.is_empty()
                && !word.contains('/')
                && !word.starts_with(['#', '-', '$', '(', '"', '\'', '!']))
            .then_some(word);
        }
        rest = rest[word.len()..].trim_start();
        if rest.is_empty() {
            return None;
        }
    }
}

/// Where a shell reads its startup files, for `doctor` and the installer.
pub fn rc_files(shell: Shell) -> Vec<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return Vec::new();
    };
    match shell {
        Shell::Zsh => vec![home.join(".zshrc")],
        Shell::Bash => vec![home.join(".bashrc"), home.join(".bash_profile")],
        Shell::Fish => vec![home.join(".config/fish/config.fish")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_extended_and_plain_zsh_history() {
        let entries = zsh_history(": 1700000000:0;git status\nls -la\n: 1700000005:12;make \\\nall\n");
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].line, "git status");
        assert_eq!(entries[0].at, Some(1_700_000_000));
        assert_eq!(entries[1].at, None);
        assert_eq!(entries[2].line, "make all");
    }

    #[test]
    fn reads_timestamped_bash_history() {
        let entries = bash_history("#1700000000\ngit push\ncargo test\n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].at, Some(1_700_000_000));
        assert_eq!(entries[1].at, None);
    }

    #[test]
    fn reads_fish_history() {
        let entries = fish_history("- cmd: echo hi\\nthere\n  when: 1700000000\n- cmd: ls\n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].line, "echo hi\nthere");
        assert_eq!(entries[0].at, Some(1_700_000_000));
    }

    #[test]
    fn finds_the_command_behind_the_wrappers() {
        assert_eq!(command_word("sudo apt install vim"), Some("apt"));
        assert_eq!(command_word("FOO=1 BAR=2 make -j8"), Some("make"));
        assert_eq!(command_word("env RUST_LOG=debug cargo run"), Some("cargo"));
        assert_eq!(command_word("  ls"), Some("ls"));
    }

    #[test]
    fn skips_lines_that_are_not_commands() {
        assert_eq!(command_word("./configure"), None);
        assert_eq!(command_word("/usr/bin/env python"), None);
        assert_eq!(command_word("# a comment"), None);
        assert_eq!(command_word(""), None);
        assert_eq!(command_word("FOO=1"), None);
    }

    #[test]
    fn path_lookup_agrees_with_the_system() {
        assert!(on_path("sh"));
        assert!(!on_path("definitely-not-a-real-binary-xyzzy"));
        assert!(!on_path("/bin/sh"));
    }
}
