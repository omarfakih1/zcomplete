//! Configuration: where things live on disk, and the knobs.
//!
//! The file is a deliberately small subset of TOML — flat `key = value` lines
//! with strings, numbers, booleans and string arrays. Editing it by hand is the
//! point, so `zcomplete mode` rewrites only the line it owns and leaves every
//! comment where the user put it.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    /// Confirm every correction before running it.
    #[default]
    Safe,
    /// Run ordinary corrections straight away; still confirm dangerous ones.
    Unsafe,
    /// Never ask.
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
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Mode::Safe => "safe",
            Mode::Unsafe => "unsafe",
            Mode::Bypass => "bypass",
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Color {
    #[default]
    Auto,
    Always,
    Never,
}

pub struct Config {
    pub mode: Mode,
    pub enabled: bool,
    /// Words shorter than this are never corrected; two letters of evidence is
    /// the floor at which a guess is worth making.
    pub min_input: usize,
    /// How much a use in the current directory counts for, relative to a use
    /// anywhere. Frecency is compressed logarithmically, so this needs to be
    /// well above 1 to move anything.
    pub context_weight: f32,
    pub typo_limit: usize,
    /// How many alternatives the picker offers when the top match is not clear.
    pub max_candidates: usize,
    /// Runner-up within this fraction of the winner means "ask, don't assume".
    pub ambiguity: f32,
    pub color: Color,
    /// When nothing you have run matches, consider anything on PATH. Keeps a
    /// fresh install from being useless before it has learned anything.
    pub path_fallback: bool,
    /// Extra commands that always require confirmation, whatever the mode.
    pub always_confirm: Vec<String>,
    pub source: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            mode: Mode::Safe,
            enabled: true,
            min_input: 2,
            context_weight: 4.0,
            typo_limit: 2,
            max_candidates: 5,
            ambiguity: 0.75,
            color: Color::Auto,
            path_fallback: true,
            always_confirm: Vec::new(),
            source: None,
        }
    }
}

impl Config {
    pub fn load() -> Config {
        let path = config_path();
        let mut cfg = Config::default();
        let Ok(text) = fs::read_to_string(&path) else {
            return cfg;
        };
        cfg.source = Some(path);

        for (key, value) in parse(&text) {
            match key.as_str() {
                "mode" => cfg.mode = value.str().and_then(Mode::parse).unwrap_or(cfg.mode),
                "enabled" => cfg.enabled = value.bool().unwrap_or(cfg.enabled),
                "min_input" => cfg.min_input = value.int().unwrap_or(cfg.min_input as i64).max(1) as usize,
                "context_weight" => cfg.context_weight = value.num().unwrap_or(cfg.context_weight).max(0.0),
                "typo_limit" => cfg.typo_limit = value.int().unwrap_or(2).clamp(0, 3) as usize,
                "max_candidates" => {
                    cfg.max_candidates = value.int().unwrap_or(5).clamp(1, 9) as usize
                }
                "ambiguity" => cfg.ambiguity = value.num().unwrap_or(cfg.ambiguity).clamp(0.0, 1.0),
                "path_fallback" => cfg.path_fallback = value.bool().unwrap_or(cfg.path_fallback),
                "color" => {
                    cfg.color = match value.str().unwrap_or_default() {
                        "always" => Color::Always,
                        "never" => Color::Never,
                        _ => Color::Auto,
                    }
                }
                "always_confirm" => cfg.always_confirm = value.list(),
                _ => {}
            }
        }

        // One-shot overrides, for `ZCOMPLETE_MODE=safe ./deploy.sh` and for
        // switching the whole thing off in a single shell.
        if let Some(mode) = std::env::var("ZCOMPLETE_MODE").ok().as_deref().and_then(Mode::parse) {
            cfg.mode = mode;
        }
        if std::env::var_os("ZCOMPLETE_DISABLE").is_some() {
            cfg.enabled = false;
        }
        cfg
    }

    /// How far off a word of `len` letters is allowed to be before we stop
    /// treating it as a typo of something. Short words are left alone: at three
    /// letters, one edit reaches too much of the dictionary.
    pub fn max_typo_distance(&self, len: usize) -> usize {
        let by_length = match len {
            0..=2 => 0,
            3..=4 => 1,
            _ => 2,
        };
        by_length.min(self.typo_limit)
    }

    pub fn set(&self, key: &str, value: &str) -> std::io::Result<PathBuf> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let existing = fs::read_to_string(&path).unwrap_or_default();
        fs::write(&path, rewrite(&existing, key, value))?;
        Ok(path)
    }
}

/// Replace the assignment for `key`, or append it, without disturbing anything
/// else in the file.
fn rewrite(text: &str, key: &str, value: &str) -> String {
    let mut out = String::with_capacity(text.len() + key.len() + value.len() + 8);
    let mut replaced = false;
    for line in text.lines() {
        if line.split('=').next().map(str::trim) == Some(key) && !line.trim_start().starts_with('#')
        {
            out.push_str(&format!("{key} = {value}\n"));
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        if out.is_empty() {
            out.push_str("# zcomplete configuration - see `zcomplete help config`\n");
        }
        out.push_str(&format!("{key} = {value}\n"));
    }
    out
}

pub enum Value {
    Text(String),
    Num(f64),
    Bool(bool),
    List(Vec<String>),
}

impl Value {
    fn str(&self) -> Option<&str> {
        match self {
            Value::Text(text) => Some(text),
            _ => None,
        }
    }

    fn num(&self) -> Option<f32> {
        match self {
            Value::Num(n) => Some(*n as f32),
            _ => None,
        }
    }

    fn int(&self) -> Option<i64> {
        match self {
            Value::Num(n) => Some(*n as i64),
            _ => None,
        }
    }

    fn bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    fn list(&self) -> Vec<String> {
        match self {
            Value::List(items) => items.clone(),
            Value::Text(text) => vec![text.clone()],
            _ => Vec::new(),
        }
    }
}

fn parse(text: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let raw = raw.split(" #").next().unwrap_or(raw).trim();
        let value = if let Some(items) = raw.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
            Value::List(
                items
                    .split(',')
                    .map(unquote)
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        } else if raw == "true" || raw == "false" {
            Value::Bool(raw == "true")
        } else if let Ok(number) = raw.parse::<f64>() {
            Value::Num(number)
        } else {
            Value::Text(unquote(raw))
        };
        out.push((key.trim().to_owned(), value));
    }
    out
}

fn unquote(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .to_owned()
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home().join(fallback),
    }
}

pub fn config_path() -> PathBuf {
    match std::env::var_os("ZCOMPLETE_CONFIG") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => xdg("XDG_CONFIG_HOME", ".config")
            .join("zcomplete")
            .join("config.toml"),
    }
}

pub fn data_dir() -> PathBuf {
    match std::env::var_os("ZCOMPLETE_DATA_DIR") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => xdg("XDG_DATA_HOME", ".local/share").join("zcomplete"),
    }
}

pub fn db_path() -> PathBuf {
    data_dir().join("commands.bin")
}

/// Where a shell should look for its own init snippet when debugging.
pub fn describe_paths() -> [(&'static str, PathBuf); 3] {
    [
        ("config", config_path()),
        ("database", db_path()),
        ("data directory", data_dir()),
    ]
}

pub fn exists(path: &Path) -> bool {
    path.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_subset_we_document() {
        let values = parse(
            r#"
            # a comment
            mode = "unsafe"
            enabled = false
            context_weight = 2.5
            max_candidates = 7   # trailing comment
            always_confirm = ["rm", 'dd', "kubectl"]
            [ignored-table]
            "#,
        );
        let get = |key: &str| values.iter().find(|(k, _)| k == key).map(|(_, v)| v);
        assert_eq!(get("mode").unwrap().str(), Some("unsafe"));
        assert_eq!(get("enabled").unwrap().bool(), Some(false));
        assert_eq!(get("context_weight").unwrap().num(), Some(2.5));
        assert_eq!(get("max_candidates").unwrap().int(), Some(7));
        assert_eq!(get("always_confirm").unwrap().list(), ["rm", "dd", "kubectl"]);
    }

    #[test]
    fn rewriting_preserves_the_rest_of_the_file() {
        let before = "# keep me\nmode = \"safe\"\nmin_input = 3\n";
        let after = rewrite(before, "mode", "\"bypass\"");
        assert_eq!(after, "# keep me\nmode = \"bypass\"\nmin_input = 3\n");
    }

    #[test]
    fn rewriting_appends_when_the_key_is_absent() {
        let after = rewrite("min_input = 3\n", "mode", "\"unsafe\"");
        assert!(after.starts_with("min_input = 3\n"));
        assert!(after.ends_with("mode = \"unsafe\"\n"));
    }

    #[test]
    fn a_commented_out_key_is_not_mistaken_for_the_real_one() {
        let after = rewrite("# mode = \"safe\"\n", "mode", "\"bypass\"");
        assert_eq!(after, "# mode = \"safe\"\nmode = \"bypass\"\n");
    }

    #[test]
    fn two_letter_words_are_not_treated_as_typos() {
        let cfg = Config::default();
        assert_eq!(cfg.max_typo_distance(2), 0);
        assert_eq!(cfg.max_typo_distance(4), 1);
        assert_eq!(cfg.max_typo_distance(9), 2);
    }
}
