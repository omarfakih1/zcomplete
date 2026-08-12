# zcomplete

When you type a word that isn't a command, zcomplete works out which command you
meant from the ones you actually run, and offers to run it with your arguments.

```
$ mkd build
zcomplete: run mkdir instead of 'mkd'? [Y/n] y

$ gti status
zcomplete: run git instead of 'gti'? [Y/n] y
On branch main
nothing to commit, working tree clean

$ cargo tset
error: no such command: `tset`
zcomplete: run cargo test? [Y/n] y
```

zsh, bash and fish. One dependency (`libc`), one binary, and a data
directory you can delete at any time.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/omarfakih1/zcomplete/main/install.sh | sh
```

Downloads the binary for your platform, verifies it against the published
`sha256`, and refuses to install if the checksum is missing or wrong. Then it
asks before adding a line to any shell config, and offers to seed the database
from your history. Add `-s -- -y` to answer yes to both.

Platforms: macOS arm64 and x86-64, Linux arm64 and x86-64 (static musl).
Options: `--prefix=DIR` (default `~/.local`), `--version=vX.Y.Z` (default
latest). To read [install.sh](install.sh) first, replace `| sh` with `| less`.
Run from a checkout, it builds instead of downloading.

By hand:

```bash
cargo install --git https://github.com/omarfakih1/zcomplete
```

then add the line for your shell:

| shell | line | file |
|---|---|---|
| zsh | `eval "$(zcomplete init zsh)"` | `~/.zshrc` |
| bash | `eval "$(zcomplete init bash)"` | `~/.bashrc` |
| fish | `zcomplete init fish \| source` | `~/.config/fish/config.fish` |

and seed it:

```bash
zcomplete import
```

`import` reads your history file, and asks the shell for its aliases, functions
and builtins: those are not on `PATH` and would otherwise be discarded as words
that aren't commands. `zcomplete doctor` reports what is missing.

## Update

The install command again. It replaces the binary in place and leaves your
database, mode and shell config alone.

```bash
curl -fsSL https://raw.githubusercontent.com/omarfakih1/zcomplete/main/install.sh | sh
```

The new binary is written beside the old one and renamed over it, so a shell
that is mid-command keeps running the version it started with. Open shells go on
using the old one until the hook next starts a process; `exec $SHELL` picks up
the new one immediately, and is needed anyway if the shell integration itself
changed:

```bash
zcomplete --version && exec $SHELL
```

From a checkout:

```bash
git pull && ./install.sh && exec $SHELL
```

`--version=vX.Y.Z` installs a specific release, which is also how you go back.

## Modes

```bash
zcomplete safe      # confirm every correction (default)
zcomplete unsafe    # run ordinary corrections; confirm dangerous ones
zcomplete bypass    # never confirm
zcomplete off       # stop correcting
```

The mode is read on every correction, so a change applies to shells that are
already open. `ZCOMPLETE_MODE=safe ./script.sh` overrides it for one command.

Dangerous means listed in [src/safety.rs](src/safety.rs): `rm`, `dd`, `mkfs*`,
`git push --force`, `git reset --hard`, `terraform destroy`, `kubectl delete`,
recursive `chmod`, `curl ... | sh`, and about thirty more. Flags are parsed the way
the shell parses them, so `-rf`, `-r -f`, `--force` and a `--` terminator are all
handled, and `git clean -n` is distinguished from `git clean -fd`.

## At the prompt

| key | |
|---|---|
| `y` or Enter | run the correction |
| `n` or `i` | leave it alone |
| `u` | fix the subcommand too, when one is offered |
| `1`-`9` | pick from the list when the match is unclear |

One keypress, no Enter. `u` handles a line where both words are wrong:

```
$ zcom he
zcomplete: run zcomplete instead of 'zcom'?  u: also he -> help [Y/n] u
zcomplete 0.1.1 - run the command you meant
```

`y` would run `zcomplete he` and let it fail; `u` fixes both words.

## How it decides

Each command gets a rank. Ranks decay with age on zoxide's curve:

```
score = rank × 4      used within the hour
        rank × 2      within the day
        rank ÷ 2      within the week
        rank ÷ 4      older
```

Commands run in the current directory carry extra weight, so `ma` gives `make`
inside a project and `man` elsewhere.

A typed word is matched four ways:

| tier | example |
|---|---|
| prefix | `mkd` → `mkdir` |
| initials | `dc` → `docker-compose` |
| subsequence | `dkr` → `docker` |
| typo | `gti` → `git`, `sl` → `ls`, `lls` → `ls` |

Each yields a similarity. The final score is similarity times the *logarithm* of
frecency, so match quality dominates and usage breaks ties. Sorting by tier
instead makes `gti` mean `gtimeout`; sorting by raw frecency makes `rmd` mean
`rm` rather than `rmdir`.

Fixed rules:

- A match that added or dropped letters never displaces one that didn't.
- A typo scores fully only if the first letter is right.
- Words under two letters are never corrected.
- A typo must be within one edit for a short word, two for a long one.

Two typos are exempt from the length rule. A **doubled letter**, because `lls`
contains every letter of `ls` in order; a deletion that is not a doubled letter
still loses to a clean prefix, which keeps `rmd` on `rmdir`. And a **two-letter
swap**: `sl`, `vm`, `pc`. Two-letter words get no edit budget, since one
substitution reaches a dozen real commands, but a two-letter word has exactly one
transposition and it keeps both letters. It scores at the floor, so `dc` still
means `docker-compose` and not `cd`.

Two invariants hold in every mode, including `bypass`:

- **Nothing is suggested that isn't a command.** A word enters the database only
  if the shell could resolve it at the time, and every candidate is rechecked
  against `PATH` before it is offered.
- **Non-interactive shells are untouched.** No controlling terminal means no
  correction, so scripts and CI jobs are never rewritten.

When nothing you have run fits, anything on `PATH` is considered, but only for
prefix and near-miss matches, so a fresh install works before it has learned
anything.

## Subcommands

`git sttaus`, `cargo tset`, `docker psu`. There is no list of tools that take
subcommands; second words are counted instead. A word becomes a subcommand after
coming back twice from use or four times from history, and a command needs two
such words before its second argument is read as a verb at all. So `git status`
and `git commit` qualify git within a day, while the `foo` in `grep foo file.c`
never qualifies grep.

For a word nothing has been learned about, zcomplete reads the command's
`--help`, once, with a deadline, and caches the result. That is why `git sttaus`
works on a fresh install. A tool with no subcommands lists none, so `cat notez`
asks `cat` once and never again.

```bash
zcomplete stats git          # verbs it would offer for one command
zcomplete query git sttaus   # what it would resolve to
```

A subcommand cannot be checked against `PATH` in advance, so it is corrected
*after* the command has failed rather than instead of running it; a command that
failed on its verb did nothing, so rerunning the line costs nothing. A line that
failed for any other reason is left alone: `git push` failing on a missing
upstream is not a typo. And a subcommand is learned only once it has exited zero,
so `git sttaus` can never teach zcomplete that git has a `sttaus`.

Two deliberate limits:

- **One command per line.** `cp a b && git sttaus` is refused, because rerunning
  it would repeat the copy.
- **The first two words only.** `npm run buidl` is out of reach; the typo is in a
  script name, not a command.

## Learning from answers

Confirming `mkd` → `mkdir` three times makes it the direct answer. Refusing the
same suggestion twice retires it. You can also set it directly:

```bash
zcomplete bind gs git        # gs always means git
zcomplete unbind gs
zcomplete ignore sl          # never suggest sl
```

## Commands

```
zcomplete stats [-n N]           what it has learned, strongest first
zcomplete stats <command>        subcommands it would offer for one command
zcomplete query <word>           what a word would resolve to
zcomplete query <cmd> <word>     the same, for a subcommand
zcomplete query <word> --score   with the tier and score
zcomplete import [zsh|bash|fish] seed from history (--dry-run to preview)
zcomplete forget <command>...    unlearn; --all empties the database, shortcuts
                                 and ignore list included
zcomplete ignore [<command>...]  list, add to, or --remove from the ignore list
zcomplete bind <word> <command>  pin a shortcut; unbind removes it
zcomplete mode                   show the current mode
zcomplete safe | unsafe | bypass set it
zcomplete on | off               enable or disable without editing shell config
zcomplete flush                  fold what the shell has buffered (the hooks
                                 do this for you)
zcomplete doctor                 check the installation
```

## Settings

No config file. The mode lives in the database, so `zcomplete safe` applies to
open shells and there is no second file to keep in step.

| variable | |
|---|---|
| `ZCOMPLETE_MODE` | override the mode for one command |
| `ZCOMPLETE_DISABLE` | switch corrections off for one shell |
| `ZCOMPLETE_DATA_DIR` | move the database |
| `NO_COLOR`, `TERM` | colour |
| `HISTFILE`, `XDG_DATA_HOME` | read by `import`, honoured for the data path |

The database defaults to `~/.local/share/zcomplete/commands.bin`, mode 0600. It
holds command names, never arguments. Beside it, in a directory created 0700:

| file | |
|---|---|
| `journal.<pid>` | what one shell has typed since the last fold, one line each |
| `path.<hash>` | the names on one `PATH`, and the directory metadata that dates it |
| `commands.lock` | `flock`ed while the database is being rewritten |

All three are caches of a sort. Deleting any of them costs at most the counts
buffered in a journal, and everything rebuilds.

## Speed

A command that worked needs no correction, so nothing is started for it. The
hook appends one line to a per-session journal using only builtins, and the next
run that takes the write lock folds those in. Measured on an M-series Mac:

```
                        before    after
zsh, bash               2.25 ms   0.067 ms
fish                    1.84 ms   0.15  ms
starting any process    1.4  ms
```

`zcomplete flush` runs every 200 commands to bound the journal. The read paths
look at the journals without taking the lock, so a command typed a moment ago is
still correctable.

fish has no clock builtin and `date` is a process, so its lines carry no time of
their own and are dated by the journal's last write instead, which is at most one
flush out. The decay buckets are hours and days wide.

When a correction is needed, `tests/bench.sh` with 2000 learned commands and 4257
executables on `PATH` puts the whole run at 2.2-2.7 ms, against a 1.9 ms floor
for a Rust binary that does nothing at all. So under a millisecond of it is
zcomplete's own work.

Two things keep it there. Only the scopes a run actually reads are decoded, and
the rest of that table is copied to the new file as the bytes it arrived as. And
the names on `PATH` are cached against one `stat` per directory rather than one
`readdir` per entry, so checking stops scaling with how many programs you have
installed. Installing anything moves a directory's mtime and the cache is
discarded; nothing is cached at all if a directory would not open, or if one
changed within the second the sweep ran, which is the window where a coarse
mtime could not tell the change apart.

## Shell differences

**zsh and bash correct an alias a moment later than a program.** Both run the
not-found handler in a forked child. That is fine for a program, but loses what
an alias or function does to the shell itself (`cd`, `set`, `export`). When the
answer is one of those, zcomplete hands it back for the next prompt to run in
the real shell. The question looks the same; it costs one extra call on a
command that had already failed.

**Your prompt sees the word you typed, not the command that ran.** The fork
cannot tell the shell it corrected anything. Usually invisible, but a prompt that
reacts to specific commands reacts to the typo: under powerlevel10k a corrected
`clea` leaves the blank line a typed `clear` would have suppressed, and
`POWERLEVEL9K_PROMPT_ADD_NEWLINE=false` removes it.

**fish corrects the line before it runs, so zcomplete binds enter there.** fish
calls `fish_command_not_found` only after abandoning the job, with stdin and
stdout back on the terminal; correcting from inside it would hand `cat` the
keyboard in `printf x | ct`. Rewriting the line first keeps pipes, redirections,
job control and `$status` normal, and a corrected function keeps its side
effects. zcomplete chains onto whatever enter was bound to, so load it last: it
cannot survive being rebound afterwards.

**bash 3.2, the version macOS ships, cannot intercept anything.**
`command_not_found_handle` arrived in bash 4.0. On 3.2 zcomplete still learns and
offers the fix at the next prompt:

```
$ mkd build
bash: mkd: command not found
zcomplete: run mkdir build? [Y/n] y
```

That runs in the real shell, so `cd` works. For in-place correction, install
bash 4+.

**A corrected subcommand leaves `$?` alone.** All three shells restore the exit
status around the prompt hook where subcommands are corrected, so the prompt
reports the failure you typed even after the rerun succeeds.

## Uninstall

```bash
zcomplete forget --all
rm -rf ~/.local/share/zcomplete
```

and delete the `zcomplete init` line from your shell config.

`cargo test` covers none of [src/init/](src/init). Those are tested by driving a
real zsh, bash and fish under a pty, which needs all three installed and takes
about a minute:

```bash
cargo test && cargo build --release && python3 tests/shells.py
```

`tests/bench.sh` times the hot path against `/usr/bin/true`.
