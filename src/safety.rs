//! Which corrections deserve a second look.
//!
//! The premise is unusual: the user typed a word that does not exist, and we are
//! about to run a *different* word on their behalf. So the bar is not "is this
//! command dangerous in the abstract" but "would running this by accident cost
//! the user something they cannot get back". That makes plain `rm` a prompt even
//! without `-rf`, while `rmdir` — which refuses to touch a non-empty directory —
//! is not.

/// Commands whose ordinary use is already irreversible.
const ALWAYS: &[(&str, &str)] = &[
    ("rm", "deletes files"),
    ("shred", "overwrites files in place"),
    ("srm", "overwrites files in place"),
    ("dd", "writes raw blocks"),
    ("fdisk", "edits partition tables"),
    ("sfdisk", "edits partition tables"),
    ("parted", "edits partition tables"),
    ("diskutil", "edits disks and volumes"),
    ("newfs", "creates a filesystem over existing data"),
    ("sudo", "runs as root"),
    ("doas", "runs as root"),
    ("su", "switches user"),
    ("shutdown", "powers the machine down"),
    ("reboot", "restarts the machine"),
    ("halt", "stops the machine"),
    ("poweroff", "powers the machine down"),
    ("truncate", "resizes files, discarding the tail"),
    ("killall", "signals every process by that name"),
    ("mkswap", "reformats a device"),
];

pub struct Concern {
    pub reason: &'static str,
}

/// `None` means the correction can run unprompted in `unsafe` mode.
pub fn inspect(command: &str, args: &[String], extra_always: &[String]) -> Option<Concern> {
    let bare = command.rsplit('/').next().unwrap_or(command);

    if extra_always.iter().any(|name| name == bare) {
        return Some(Concern {
            reason: "listed in always_confirm",
        });
    }
    if bare.starts_with("mkfs") {
        return Some(Concern {
            reason: "formats a filesystem",
        });
    }
    if let Some((_, reason)) = ALWAYS.iter().find(|(name, _)| *name == bare) {
        return Some(Concern { reason });
    }
    if let Some(reason) = reckless_flag(args) {
        return Some(Concern { reason });
    }

    let reason = match bare {
        "git" => git(args),
        "chmod" | "chown" | "chgrp" => permissions(args),
        "kill" | "pkill" => {
            (flag(args, &['9'], &["KILL", "-signal=KILL"]) || args.iter().any(|a| a == "-KILL"))
                .then_some("sends SIGKILL")
        }
        "docker" | "podman" | "nerdctl" => containers(args),
        "kubectl" | "oc" => sub(args, "delete").then_some("deletes cluster resources"),
        "terraform" | "tofu" | "pulumi" => infra(args),
        "npm" | "pnpm" | "yarn" | "bun" => packages(args),
        "pip" | "pip3" | "brew" | "apt" | "apt-get" | "dnf" | "yum" | "pacman" => {
            (sub(args, "uninstall") || sub(args, "remove") || sub(args, "purge") || sub(args, "autoremove"))
                .then_some("removes installed packages")
        }
        "redis-cli" => args
            .iter()
            .any(|a| a.eq_ignore_ascii_case("flushall") || a.eq_ignore_ascii_case("flushdb"))
            .then_some("empties the datastore"),
        "psql" | "mysql" | "mariadb" | "sqlite3" | "mongosh" => sql(args),
        "systemctl" | "launchctl" | "service" => {
            (sub(args, "disable") || sub(args, "remove") || sub(args, "unload") || sub(args, "mask"))
                .then_some("changes service state persistently")
        }
        "defaults" => sub(args, "delete").then_some("deletes macOS preferences"),
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" => piped_installer(args),
        "curl" | "wget" => piped_installer(args),
        "chflags" => args
            .iter()
            .any(|a| a.contains("nouchg") || a.contains("noschg"))
            .then_some("clears immutable flags"),
        "mv" => args
            .last()
            .is_some_and(|target| target == "/dev/null")
            .then_some("moves files into /dev/null"),
        _ => None,
    };

    reason.map(|reason| Concern { reason })
}

/// Flags that mean "I know this destroys something" on whatever command they
/// are handed to. Checked before the per-command rules, because the word that
/// got corrected may not be one we have a rule for.
fn reckless_flag(args: &[String]) -> Option<&'static str> {
    if flag(args, &[], &["dry-run"]) {
        return None;
    }
    for arg in args.iter().take_while(|a| *a != "--") {
        if let Some(long) = arg.strip_prefix("--") {
            if matches!(
                long.split('=').next().unwrap_or(long),
                "force" | "hard" | "no-preserve-root" | "purge" | "destroy" | "wipe"
            ) {
                return Some("was given a flag that destroys something");
            }
        } else if let Some(cluster) = arg.strip_prefix('-') {
            // -rf, -fr, -rvf: recursive and forced together is never gentle.
            if cluster.contains('r') && cluster.contains('f') {
                return Some("was given -r and -f together");
            }
        }
    }
    None
}

fn git(args: &[String]) -> Option<&'static str> {
    let dry_run = flag(args, &['n'], &["dry-run"]);
    if sub(args, "push") && flag(args, &['f'], &["force"]) && !dry_run {
        return Some("force-pushes, rewriting remote history");
    }
    if sub(args, "reset") && (flag(args, &[], &["hard"]) || flag(args, &[], &["merge"])) {
        return Some("discards uncommitted work");
    }
    if sub(args, "clean") && flag(args, &['f'], &["force"]) && !dry_run {
        return Some("deletes untracked files");
    }
    if sub(args, "branch") && args.iter().any(|a| a == "-D") {
        return Some("deletes an unmerged branch");
    }
    if sub(args, "filter-branch") || sub(args, "filter-repo") {
        return Some("rewrites history");
    }
    if sub(args, "stash") && (sub(args, "drop") || sub(args, "clear")) {
        return Some("throws away stashed work");
    }
    // `git restore` exists to throw changes away. `git checkout` only does when
    // it is pointed at a path rather than a branch, which is worth telling
    // apart: switching branches all day with a prompt each time is useless.
    if sub(args, "restore") {
        return Some("discards changes to the files it names");
    }
    if sub(args, "checkout") && names_a_path(args) {
        return Some("overwrites the files it names");
    }
    None
}

fn names_a_path(args: &[String]) -> bool {
    args.iter()
        .skip_while(|a| *a != "checkout")
        .skip(1)
        .any(|a| a == "." || a == "--" || (!a.starts_with('-') && std::path::Path::new(a).exists()))
}

fn permissions(args: &[String]) -> Option<&'static str> {
    if flag(args, &['R'], &["recursive"]) {
        return Some("changes ownership or mode recursively");
    }
    args.iter()
        .any(|a| a == "777" || a == "0777" || a == "a+rwx")
        .then_some("makes files world-writable")
}

fn containers(args: &[String]) -> Option<&'static str> {
    if sub(args, "prune") || (sub(args, "system") && sub(args, "prune")) {
        return Some("deletes unused containers, images and volumes");
    }
    if sub(args, "volume") && sub(args, "rm") {
        return Some("deletes container volumes");
    }
    if sub(args, "rm") && flag(args, &['f'], &["force"]) {
        return Some("force-removes running containers");
    }
    None
}

fn infra(args: &[String]) -> Option<&'static str> {
    if sub(args, "destroy") {
        return Some("tears down live infrastructure");
    }
    (sub(args, "apply") && flag(args, &[], &["auto-approve"]))
        .then_some("applies infrastructure changes without review")
}

fn packages(args: &[String]) -> Option<&'static str> {
    if sub(args, "publish") || sub(args, "unpublish") {
        return Some("publishes to a public registry");
    }
    (sub(args, "uninstall") || sub(args, "remove")).then_some("removes installed packages")
}

fn sql(args: &[String]) -> Option<&'static str> {
    let inline = args.iter().enumerate().filter_map(|(i, arg)| {
        matches!(arg.as_str(), "-c" | "-e" | "--command" | "--eval")
            .then(|| args.get(i + 1))
            .flatten()
    });
    inline
        .flat_map(|stmt| stmt.split_whitespace())
        .any(|word| {
            matches!(
                word.to_ascii_uppercase().as_str(),
                "DROP" | "TRUNCATE" | "DELETE"
            )
        })
        .then_some("runs a destructive statement")
}

/// `sh -c "$(curl …)"`, `curl … | sh` reassembled by the caller, and friends.
fn piped_installer(args: &[String]) -> Option<&'static str> {
    args.iter()
        .any(|arg| {
            (arg.contains("curl ") || arg.contains("wget ")) && (arg.contains("| sh") || arg.contains("|sh") || arg.contains("| bash"))
        })
        .then_some("downloads and executes a remote script")
}

/// Does `name` appear as a subcommand — a bare word, not an option value.
fn sub(args: &[String], name: &str) -> bool {
    args.iter()
        .take_while(|a| *a != "--")
        .any(|a| a == name)
}

/// Handles `-rf`, `-r -f`, `--force`, and stops at a `--` terminator.
fn flag(args: &[String], shorts: &[char], longs: &[&str]) -> bool {
    for arg in args.iter().take_while(|a| *a != "--") {
        if let Some(long) = arg.strip_prefix("--") {
            let long = long.split('=').next().unwrap_or(long);
            if longs.contains(&long) {
                return true;
            }
        } else if let Some(cluster) = arg.strip_prefix('-') {
            if !cluster.is_empty() && cluster.chars().all(|c| c.is_ascii_alphanumeric()) {
                if cluster.chars().any(|c| shorts.contains(&c)) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(str::to_owned).collect()
    }

    fn concern(command: &str, line: &str) -> Option<&'static str> {
        inspect(command, &argv(line), &[]).map(|c| c.reason)
    }

    #[test]
    fn removal_always_asks_however_it_is_spelled() {
        assert!(concern("rm", "-rf build").is_some());
        assert!(concern("rm", "notes.txt").is_some());
        assert!(concern("/bin/rm", "-fr /").is_some());
    }

    #[test]
    fn harmless_neighbours_do_not_ask() {
        assert!(concern("ls", "-la").is_none());
        assert!(concern("rmdir", "empty").is_none());
        assert!(concern("git", "status").is_none());
        assert!(concern("docker", "ps -a").is_none());
        assert!(concern("kubectl", "get pods").is_none());
    }

    #[test]
    fn discarding_local_work_asks_even_without_a_scary_flag() {
        assert!(concern("git", "restore src/main.rs").is_some());
        assert!(concern("git", "reset --hard HEAD~1").is_some());
        assert!(concern("git", "stash drop").is_some());
        // Switching branches is not destructive and must not nag.
        assert!(concern("git", "checkout main").is_none());
        assert!(concern("git", "switch main").is_none());
        assert!(concern("git", "checkout -b feature").is_none());
    }

    #[test]
    fn checkout_of_an_actual_path_is_destructive() {
        let here = std::env::current_dir().unwrap();
        let name = std::fs::read_dir(&here)
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .find(|n| !n.starts_with('.'))
            .expect("the working directory has at least one visible entry");
        assert!(concern("git", &format!("checkout {name}")).is_some());
        assert!(concern("git", "checkout no-such-file-here-xyzzy").is_none());
    }

    #[test]
    fn a_destructive_flag_is_caught_on_a_command_we_have_no_rule_for() {
        assert!(concern("some-deploy-tool", "--force").is_some());
        assert!(concern("frobnicate", "-rf /tmp/x").is_some());
        assert!(concern("frobnicate", "-rv /tmp/x").is_none());
        assert!(concern("some-deploy-tool", "--force-with-lease").is_none());
    }

    #[test]
    fn a_dry_run_is_not_a_destructive_run() {
        assert!(concern("git", "clean -n").is_none());
        assert!(concern("git", "clean -fd").is_some());
        assert!(concern("git", "push --dry-run --force").is_none());
    }

    #[test]
    fn clustered_and_long_flags_read_the_same() {
        assert!(concern("chmod", "-R 755 .").is_some());
        assert!(concern("chmod", "--recursive 755 .").is_some());
        assert!(concern("chmod", "755 file").is_none());
        assert!(concern("chmod", "777 file").is_some());
    }

    #[test]
    fn operands_after_the_terminator_are_not_flags() {
        assert!(concern("git", "clean -- -f").is_none());
        assert!(concern("chown", "me -- -R").is_none());
    }

    #[test]
    fn destructive_sql_is_caught_only_when_inline() {
        assert!(concern("psql", "-c DROP TABLE users").is_some());
        assert!(concern("psql", "mydb").is_none());
    }

    #[test]
    fn remote_scripts_piped_into_a_shell_are_flagged() {
        let args = vec!["-c".into(), "curl -fsSL https://x.sh | sh".into()];
        assert!(inspect("sh", &args, &[]).is_some());
    }

    #[test]
    fn the_user_can_add_their_own() {
        let extra = vec!["deploy".to_string()];
        assert!(inspect("deploy", &[], &extra).is_some());
        assert!(inspect("deploy", &[], &[]).is_none());
    }
}
