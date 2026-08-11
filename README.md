# zcomplete

Your shell already knows which commands you actually run. zcomplete uses that:
when you type a word that isn't a command, it works out which one you meant and
runs it, arguments and all.

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

One keypress, no Enter. It only ever suggests a command that exists right now,
so if you type `clean` fifty times and `clean` isn't installed, `cle` still gets
you `clear`.

It is `zoxide` for commands instead of directories, and it borrows zoxide's
frecency ranking wholesale.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/omarfakih1/zcomplete/main/install.sh | sh
```

It downloads the binary for your platform, checks it against the published
`sha256`, and refuses to install it if either is missing. Then it asks, once per
shell, before adding a line to any file, and offers to seed the database from
the history you already have. Nothing is written without an answer, and `-y`
answers for you:

```bash
curl -fsSL https://raw.githubusercontent.com/omarfakih1/zcomplete/main/install.sh | sh -s -- -y
```

macOS on Apple silicon or Intel, and Linux on x86-64 or arm64. The Linux
binaries are static musl builds, so the distribution and its libc do not matter.
`--prefix=DIR` installs somewhere other than `~/.local`, and `--version=vX.Y.Z`
pins a release rather than taking the newest.

Piping a script from the internet into a shell is a thing worth being uneasy
about. [install.sh](install.sh) is short enough to read first:

```bash
curl -fsSL https://raw.githubusercontent.com/omarfakih1/zcomplete/main/install.sh | less
```

From a checkout it builds what is in front of it instead of downloading
anything, which is the same script and the same wiring:

```bash
git clone https://github.com/omarfakih1/zcomplete && cd zcomplete && ./install.sh
```

To do it by hand:

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

`import` reads your history, and asks each shell for its aliases, functions and
builtins so the ones in that history are kept. An alias is never on `PATH`, so
without that step the `gs` you run twenty times a day looks like a word that
isn't a command and gets dropped.

`zcomplete doctor` checks all of that and says what is missing.

## Modes

```bash
zcomplete safe      # confirm every correction (default)
zcomplete unsafe    # run ordinary corrections; still confirm dangerous ones
zcomplete bypass    # never confirm
zcomplete off       # stop correcting entirely
```

The mode is read on every correction, so changing it takes effect in shells that
are already open. `ZCOMPLETE_MODE=safe ./script.sh` overrides it for one command.

"Dangerous" is decided in [src/safety.rs](src/safety.rs): one file, one list.
`rm`, `dd`, `sudo`, `mkfs*`, `git push --force`, `git reset --hard`,
`terraform destroy`, `kubectl delete`, recursive `chmod`, a `curl … | sh`, and
about thirty more. It knows the difference between `git clean -fd` and
`git clean -n`, and it reads flags the way the shell does, so `-rf`, `-r -f`,
`--force` and a `--` terminator all land correctly.

## At the prompt

| key | |
|---|---|
| `y` or Enter | run the correction |
| `n` or `i` | leave it alone |
| `u` | fix the subcommand too, when one is offered |
| `1`-`9` | pick from the list when the match is unclear |

`u` is for a line where both words are wrong:

```
$ zcom he
zcomplete: run zcomplete instead of 'zcom'?  u: also he -> help [Y/n] u
zcomplete 0.1.0 - run the command you meant
```

`y` there would run `zcomplete he` and let it fail on its own; `u` fixes both.
Turning a suggestion down twice retires it for good.

## How it decides

Each command you run gets a rank, and each rank decays with age, on the same
curve zoxide uses:

```
score = rank × 4      if used within the hour
        rank × 2      within the day
        rank ÷ 2      within the week
        rank ÷ 4      older
```

Commands you run in *this* directory carry extra weight, so `ma` finds `make`
inside a project and `man` everywhere else.

A typed word is matched against that list four ways:

| | example |
|---|---|
| prefix | `mkd` → `mkdir` |
| initials | `dc` → `docker-compose` |
| subsequence | `dkr` → `docker` |
| typo | `gti` → `git`, `gut` → `git`, `sl` → `ls`, `lls` → `ls` |

Each produces a similarity, and the final score is that similarity times the
*logarithm* of frecency. The compression is the point: how well a word matches
is the main signal, and how often you run something can overturn a close call
but not a wide one. Ranking the four kinds absolutely sounds tidier and is wrong
in both directions: with coreutils installed it made `gti` mean `gtimeout`, and
ranking on raw frecency made `rmd` mean `rm` rather than `rmdir`.

Two rules are absolute. A match that had to add or drop letters never displaces
one that did not, and a typo has to keep the first letter to score fully,
because that is the letter people get right. Words under two letters are never
corrected, and a typo has to be within one edit for a short word and two for a
long one; past that zcomplete says nothing and you get the usual error.

Two typos are exempt from the length rule, because neither is really a guess:

- **A doubled letter.** `lls` keeps every letter of `ls` where you put it, so
  frecency decides. A deletion that is *not* a doubled letter still loses to a
  clean prefix, which is what keeps `rmd` on `rmdir` rather than on `rm`.
- **A two-letter swap.** `sl`, `vm`, `pc`. A two-letter word gets no edit budget
  at all, because one substitution reaches a dozen real commands, but its single
  transposition keeps both letters you typed and there is only ever one
  candidate. It is scored at the floor so a deliberate abbreviation still reads
  first: `dc` is `docker-compose`, not `cd`.

Two invariants hold in every mode, including `bypass`:

- **It never runs something that isn't a command.** A word only enters the
  database if the shell could resolve it at the time, and every candidate is
  re-checked against `PATH` before it is offered. Uninstall a tool and it stops
  being suggested.
- **It never touches a non-interactive shell.** No controlling terminal means no
  correction, so nothing gets rewritten inside a script or a CI job.

When nothing you have run fits, it will consider anything on `PATH` as a last
resort, but only for prefix and near-miss matches. That is what makes a fresh
install useful before it has learned anything.

## Subcommands

`git sttaus`, `cargo tset`, `docker psu`: the same treatment one level down, for
any command at all. There is no list of tools that take subcommands, because a
list is always missing the one you use.

Instead the second word is watched. Every command's second word is counted, and
a word only becomes a subcommand once it has come back: twice from use, four
times from history. A command needs two such words before its second argument is
read as a verb at all. So `git status` and `git commit` qualify git within a day,
and the `foo` in `grep foo file.c` never qualifies grep, because the next grep
uses a different pattern.

When a command fails on a word nothing has been learned about, zcomplete asks
the command itself, once, by reading its `--help`. That is the subcommand
version of scanning `PATH`, and it is why `git sttaus` works on a fresh install:

```
$ git sttaus
git: 'sttaus' is not a git command. See 'git --help'.
zcomplete: run git status? [Y/n] y
```

It runs only after a command has already failed, only for a command you just ran
yourself, at most once per command, with a deadline. What it reads is cached
beside what it learned. A tool with no subcommands lists none, so `cat notez`
asks `cat` once, gets nothing, and never asks again.

```bash
zcomplete stats git          # the verbs it would offer for one command
zcomplete query git sttaus   # what it would resolve to
```

One thing works differently down here. A subcommand cannot be checked against
`PATH` before the fact, so it is corrected **after** the command has failed
rather than instead of running it. That ordering is the whole safety argument: a
command that failed on its verb did nothing, so running the line again costs
nothing. A line that failed for any other reason is left alone: `git push`
failing on a missing upstream is not a typo, and zcomplete says nothing.

The rule that keeps `clean` from standing in for `clear` holds here too: a
subcommand is only learned once it has exited zero. `git sttaus` can never teach
zcomplete that git has a `sttaus`, however many times you type it.

Two limits are deliberate:

- **Only one command per line.** `cp a b && git sttaus` is refused, because
  correcting it means re-running the copy that already happened.
- **Only the first two words.** `npm run buidl` is out of reach: the typo is in
  the script name, which is a project's business rather than a command's.

## It learns from your answers

Confirm `mkd` → `mkdir` three times and it stops being a guess: zcomplete jumps
straight there. Turn the same suggestion down twice and it is never offered
again. You can also say so directly:

```bash
zcomplete bind gs git        # gs always means git
zcomplete unbind gs
zcomplete ignore sl          # never suggest sl, ever
```

## Commands

```
zcomplete stats [-n N]           what it has learned, strongest first
zcomplete stats <command>        the subcommands it would offer for one command
zcomplete query <word>           what a word would resolve to
zcomplete query <cmd> <word>     the same question about a subcommand
zcomplete query <word> --score   and why, with the tier and the score
zcomplete import [zsh|bash|fish] seed from shell history (--dry-run to look first)
zcomplete forget <command>...    unlearn commands; --all empties the database,
                                 shortcuts and ignore list included
zcomplete ignore [<command>...]  list, add to, or --remove from the never-suggest list
zcomplete bind <word> <command>  pin a shortcut; unbind removes it
zcomplete mode                   show the current mode
zcomplete safe | unsafe | bypass set it
zcomplete on | off               enable or disable without touching your shell config
zcomplete doctor                 check the installation
```

## Settings

There is no config file. The mode is the only thing there was ever anything to
set, and it lives in the database, so `zcomplete safe` takes effect in shells
that are already open and there is no second file to keep in step.

Environment: `ZCOMPLETE_MODE` overrides the mode for one command or one script,
`ZCOMPLETE_DISABLE` switches corrections off for one shell, and
`ZCOMPLETE_DATA_DIR` moves the database. `NO_COLOR` and `TERM` decide colour,
`HISTFILE` is read by `import`, and `XDG_DATA_HOME` is honoured; otherwise the
database lives in `~/.local/share/zcomplete/commands.bin`, mode 0600, and holds
command names only, never arguments.

## Speed

It runs on every command you type, so it is worth knowing what that costs.
`tests/bench.sh` on an M-series Mac, 2000 learned commands, 4257 executables on
PATH:

```
process spawn (the floor)            1.4 ms
zcomplete --version (does nothing)   1.9 ms
record  (every command)              2.4 ms
resolve (learned hit)                2.3 ms
resolve (subcommand)                 2.1 ms
resolve (cold, scans PATH)           3.8 ms
```

Most of that is not zcomplete. The first line is what any process costs to start
on this machine, and a do-nothing Rust binary measures no faster than the second
one, so the tax on a command you type is under a millisecond of actual work.
Learning subcommands is free: it rides along in the write that was happening
anyway. The only slow paths are the ones that have to read every directory on
`PATH`, and they run when nothing you have used fits. Reading a command's
`--help` is slower still, and happens once per command, ever.

## What it does not do

**In zsh and bash, an alias is corrected a moment later than a program.** Both
run the not-found handler in a forked child, which is fine for a program but
loses anything an alias or a function does to the shell itself: a `cd`, a `set`,
an `export`. So when the answer is one of those, zcomplete declines to run it
there, says nothing, and hands it back for the next prompt to run in the real
shell, where the side effect sticks. You see the same question either way; the
only difference is one extra call, on a command that had already failed.

**Your prompt sees the word you typed, not the command that ran.** Same reason:
the fork cannot tell the shell it corrected anything. Mostly this is invisible,
but a prompt that reacts to specific commands will react to the typo. If you use
powerlevel10k, a corrected `clea` leaves the blank line above the prompt that a
typed `clear` would have suppressed; `POWERLEVEL9K_PROMPT_ADD_NEWLINE=false`
removes it. fish does not have this problem, because there the line is corrected
before it runs.

**fish is corrected a moment earlier, which means zcomplete binds enter there.**
fish calls `fish_command_not_found` only after it has given up on the job, with
stdin and stdout back on the terminal. Running the correction from inside it
would make `printf x | ct` hand `cat` the keyboard and hang the shell, and
`unam > out.txt` print to the screen. So on fish the command line is rewritten
before it runs, and fish executes it itself. Pipes, redirections, command
substitution, job control and `$status` all behave normally, and unlike zsh and
bash a corrected shell function keeps its side effects. zcomplete chains onto
whatever enter was already bound to rather than replacing it, so a binding of
your own still runs; the one thing it cannot survive is being rebound *after* it
loads, so load it last. The confirmation is asked on the alternate screen, the
way fzf does, because fish's line editor is still drawing at that moment and
anything written over it smears the redraw.

**bash 3.2, the bash that ships with macOS, cannot intercept anything.**
`command_not_found_handle` arrived in bash 4.0. On 3.2 zcomplete still learns,
and it offers the fix on the next prompt instead of running it in place:

```
$ mkd build
bash: mkd: command not found
zcomplete: run mkdir build? [Y/n] y
```

That path runs in your real shell rather than a subshell, so `cd` works there.
For in-place correction, `brew install bash`.

**A corrected subcommand leaves `$?` alone.** All three shells restore the exit
status around their prompt hook, which is where subcommands are corrected, so
your prompt goes on reporting the failure you actually typed even after the
rerun succeeds. The rerun itself happens in your real shell rather than a
subshell, so unlike a first-word correction in zsh or bash, its side effects
survive.

## Uninstall

```bash
zcomplete forget --all
rm -rf ~/.local/share/zcomplete
```

and delete the `zcomplete init` line from your shell config.

## Contributing

Eight files, no dependencies beyond `libc`:

| file | |
|---|---|
| [src/main.rs](src/main.rs) | argument dispatch, exit codes, the error type |
| [src/correct.rs](src/correct.rs) | the hot path: everything the shell hooks call |
| [src/admin.rs](src/admin.rs) | the commands you run yourself: stats, import, doctor |
| [src/matcher.rs](src/matcher.rs) | what a typed word could have meant, and how strongly |
| [src/store.rs](src/store.rs) | the on-disk format, ranks, learned answers, the mode |
| [src/safety.rs](src/safety.rs) | what counts as dangerous |
| [src/term.rs](src/term.rs) | the prompt, which talks to `/dev/tty` directly |
| [src/shell.rs](src/shell.rs) | PATH lookups, history parsing, the init snippets |

Unit tests live beside the code they test:

```bash
cargo test
```

The shell integration is tested separately, by driving a real zsh, bash and fish
under a pty. Nothing in [src/init/](src/init) is covered by `cargo test`, so run
this too if you touch it:

```bash
cargo build --release && python3 tests/shells.py
```

It needs the three shells installed and takes about a minute. `tests/bench.sh`
times the hot path against `/usr/bin/true`.

Comments here explain constraints rather than code, so one that looks obvious is
usually load-bearing.

MIT.
