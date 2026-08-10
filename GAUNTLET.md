# Gauntlet log

zcomplete was built as a gauntlet loop: a builder produces the work, and
separate agents with no knowledge of how it was built grade the finished
artifact against a bar, each naming the single biggest gap. This is what the
rounds found. It is kept because the failures are more instructive than the
code.

**The bar.** zoxide, which the goal names directly ("a zoxide for commands"),
plus a 60-criterion rubric generated for the parts zoxide does not cover:
safety modes, multi-shell interception, the never-run-a-non-command invariant.

## Round 1

Six critics against the first complete build.

| Area | Verdict | Gap |
|---|---|---|
| Resolution | NOT YET | Tier ranked ahead of frecency as an absolute veto. With coreutils installed, `gti` resolved to `gtimeout`, and no amount of use could overturn it. |
| Shells | NOT YET | On fish the correction ran inside `fish_command_not_found`, which fish calls after abandoning the job. `printf x \| ct` handed `cat` the keyboard and hung the shell; `unam > out.txt` printed to the screen. |
| Safety | NOT YET | `git restore work.txt` auto-ran in unsafe mode and discarded uncommitted work. No argument-level rule at all, so `--force` on a command with no rule of its own passed silently. |
| Durability | NOT YET | The write lock was taken before the confirmation prompt and held across the keypress: every other shell's per-command hook blocked 2007ms against a 2.4ms baseline. |
| Craft | NOT YET | Every I/O failure reached the user as a bare errno with no object, including from the hook that runs on every command. |
| UX and testing | NOT YET | The executability invariant — the one hard rule — had no test. Deleting the check left all 46 unit tests and the whole pty suite green. Proven by mutation. |

The ranking fix is the one worth recording. The critic's suggested repair,
ranking on score alone, introduces a worse bug: `rmd` resolves to a much-used
`rm` instead of `rmdir`. Frecency now enters logarithmically, so it turns a
close call and not a wide one, and one absolute rule survives: a match that had
to add or drop letters never displaces one that did not.

The same round turned up a bug nobody was looking for. zsh evaluates subscripts
inside `(( ))` as arithmetic, so `$+commands[docker-compose]` asked about a key
named "docker minus compose" and answered no. Every hyphenated command, and
every alias and function, had gone unlearned since the first commit.

## Round 2

Four critics against the repaired build: ranking, fish, safety, and one
generalist told to find whatever would most embarrass the author.

Verdicts and fixes are recorded in the commit history.
