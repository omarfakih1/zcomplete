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

| Area | Verdict | Gap |
|---|---|---|
| Ranking | NOT YET | A learned command two edits away beat the installed one a single edit away, and the right answer was not even offered: `chwon` resolved to a twelve-times-used `chmod` rather than `chown`. Distance two is a different word, not a slip, so it is now a guess — and the installed-but-unlearned pool feeds the same ranking instead of only filling in when nothing was learned. |
| fish | NOT YET | `commandline` splits on newlines, so a multi-line buffer arrived as separate arguments and was joined with spaces. Every correction inside a `for` block was dead, and Alt-Enter turned the second command into an argument to the first. |
| Safety | (fixed in the same round) | The subcommand check matched a word anywhere in the arguments, so `git commit -m restore` prompted while `git -C /tmp restore file` did not. |
| Whole | see commits | Preview showed the wrong word replaced when the correction was not first on the line; fish clobbered a user's own enter binding. |

The ranking round also produced the most useful criticism of the tests: the
golden table's fixture contained no competitor for most of its rows, so 19 of
26 cases had exactly one candidate and proved nothing. The fixture now carries
a `gtimeout` beside `git`, a `chown` beside `chmod`, a `ctags` beside `cat`.
Three rows changed answer when it did, and two turned out to have no honest
single answer at all — `gitt` is one keystroke from both `git` and `gitk` — so
those are now asserted as "both are offered" rather than given a winner the
code does not deserve.
