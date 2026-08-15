# zcomplete

You mistype a command. Instead of `command not found`, zcomplete figures out
which command you meant from the ones you actually run, and offers to run it
with your arguments.

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

zsh, bash and fish. One binary, one dependency (`libc`), about 550 KB.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/omarfakih1/zcomplete/main/install.sh | sh
```

That downloads the binary, sets up each shell it finds, and offers to seed the
database from your history. It asks before touching any config file. `-s -- -y`
answers yes to everything.

Then restart your shell, or `exec $SHELL`. Done.

The installer checks the binary against its published `sha256` and refuses to
install if the checksum is missing or wrong. macOS and Linux, arm64 and x86-64.
`--prefix=DIR` defaults to `~/.local`. Replace `| sh` with `| less` to read
[install.sh](install.sh) first.

### From cargo

```bash
cargo install --git https://github.com/omarfakih1/zcomplete
zcomplete init --all && zcomplete import
```

`init --all` adds one line to the config of every shell you have. Run it twice
and the second run does nothing. It appends and never rewrites, so the rest of
your config is untouched. `--zsh`, `--bash` or `--fish` picks one.

`import` reads your history file and asks the shell for its aliases, functions
and builtins. Those aren't on `PATH`, so without it they'd be discarded as words
that aren't commands.

The line `init` writes, if you'd rather add it yourself:

| shell | line | file |
|---|---|---|
| zsh | `eval "$(zcomplete init zsh)"` | `~/.zshrc` |
| bash | `eval "$(zcomplete init bash)"` | `~/.bashrc` |
| fish | `zcomplete init fish \| source` | `~/.config/fish/config.fish` |

(`zcomplete init zsh` without the dashes prints the integration script. That's
what the line above runs. You don't type it yourself.)

`zcomplete doctor` says what's set up and what isn't.

## At the prompt

| key | |
|---|---|
| `y` or Enter | run it |
| `n` or `i` | leave it alone |
| `u` | fix the subcommand too, when one is offered |
| `1`-`9` | pick from the list when the match is unclear |

One keypress, no Enter. `u` is for a line where both words are wrong:

```
$ zcom he
zcomplete: run zcomplete instead of 'zcom'?  u: also he -> help [Y/n] u
zcomplete 0.1.2 - run the command you meant
```

`y` there would run `zcomplete he` and let it fail.

## Modes

```bash
zcomplete safe      # confirm every correction (default)
zcomplete unsafe    # run ordinary corrections, confirm dangerous ones
zcomplete bypass    # never confirm
zcomplete off       # stop correcting
```

The mode is read on every correction, so a change reaches shells that are
already open. `ZCOMPLETE_MODE=safe ./script.sh` overrides it for one command.

Dangerous means listed in [src/safety.rs](src/safety.rs): `rm`, `dd`, `mkfs*`,
`git push --force`, `git reset --hard`, `terraform destroy`, `kubectl delete`,
recursive `chmod`, `curl ... | sh`, and about thirty more. Flags are read the way
the shell reads them, so `-rf`, `-r -f`, `--force` and a `--` terminator all
count, and `git clean -n` is not treated like `git clean -fd`.

## What it learns

Every command you run gets a rank, and ranks decay with age:

```
score = rank × 4      used within the hour
        rank × 2      within the day
        rank ÷ 2      within the week
        rank ÷ 4      older
```

Commands you ran in the current directory count for more, so `ma` gives `make`
inside a project and `man` everywhere else.

A typed word is matched four ways: prefix (`mkd` → `mkdir`), initials (`dc` →
`docker-compose`), subsequence (`dkr` → `docker`) and typo (`gti` → `git`,
`sl` → `ls`). Match quality decides the winner and usage only breaks ties.
Sorting the other way round makes `gti` mean `gtimeout`.

Two things hold in every mode, `bypass` included:

- **Nothing is suggested that isn't a command.** A word enters the database only
  if the shell could resolve it at the time, and every candidate is checked
  against `PATH` again before you're offered it.
- **Non-interactive shells are left alone.** No controlling terminal means no
  correction, so scripts and CI never get rewritten.

### Subcommands

`git sttaus`, `cargo tset`, `docker psu`. No list of tools is hardcoded. Second
words get counted, and a command needs two of them to come back before its
second argument is read as a verb at all. So `git status` and `git commit`
qualify git within a day, and the `foo` in `grep foo file.c` never qualifies
grep. For a command it knows nothing about, zcomplete reads its `--help` once
and caches the answer, which is why `git sttaus` works on a fresh install.

A subcommand can't be checked against `PATH` ahead of time, so it's corrected
after the command fails rather than instead of running it. That's safe because a
command that failed on its verb did nothing. A line that failed for any other
reason is left alone, and a subcommand is only learned once it has exited zero,
so `git sttaus` can never teach zcomplete that git has a `sttaus`.

Two limits: one command per line (`cp a b && git sttaus` is refused, since
rerunning would repeat the copy), and the first two words only (`npm run buidl`
is out of reach).

### Answers

Confirm `mkd` → `mkdir` three times and it becomes the direct answer. Refuse the
same suggestion twice and it retires. Or set it yourself:

```bash
zcomplete bind gs git        # gs always means git
zcomplete unbind gs
zcomplete ignore sl          # never suggest sl
```

## Commands

```
zcomplete init --all             set up every shell you have (--zsh, --bash, --fish)
zcomplete stats [-n N]           what it has learned, strongest first
zcomplete stats <command>        subcommands it would offer for one command
zcomplete query <word>           what a word would resolve to (--score for detail)
zcomplete query <cmd> <word>     the same, for a subcommand
zcomplete import [zsh|bash|fish] seed from history (--dry-run to preview)
zcomplete forget <command>...    unlearn; --all empties everything
zcomplete ignore [<command>...]  list, add to, or --remove from the ignore list
zcomplete bind <word> <command>  pin a shortcut; unbind removes it
zcomplete mode                   show the current mode
zcomplete safe | unsafe | bypass set it
zcomplete on | off               enable or disable without editing shell config
zcomplete doctor                 check the installation
```

## Speed

A command that worked needs no correction, so nothing starts for it. The hook
appends one line to a per-session file using only shell builtins. Per command,
on an M-series Mac:

```
zsh, bash               0.067 ms
fish                    0.15  ms
starting any process    1.4   ms   (for comparison)
```

When a correction is actually needed, 2000 learned commands and 4257 executables
on `PATH` put the whole run at 2.2-2.7 ms, against a 1.9 ms floor for a Rust
binary that does nothing. Under a millisecond of that is zcomplete's own work.

## Disk

Everything sits in one directory you can delete at any time
(`~/.local/share/zcomplete`, or `$XDG_DATA_HOME/zcomplete`). Every file in it
has a ceiling:

| file | what it is | bound |
|---|---|---|
| `commands.bin` | the database | ~160 KB, 4096 scoped rows and 512 shortcuts, weakest evicted |
| `path.<hash>` | the names on one `PATH` | the 4 most recently used |
| `journal.<pid>` | what one shell hasn't folded yet | one per live shell, emptied at each fold |
| `commands.corrupt.*` | a database that wouldn't read, kept in case you want it | the 2 most recent |

Twelve different `PATH`s and 2000 learned commands come to 48 KB.

The `PATH` listings used to have no ceiling, and every distinct `PATH` left one
behind for good. Ten venvs meant ten copies, kept for ever. Fixed.

The database holds command names. Never arguments. It's mode 0600 in a directory
created 0700.

## Settings

No config file. The mode lives in the database, so `zcomplete safe` reaches open
shells and there's no second file to keep in step.

| variable | |
|---|---|
| `ZCOMPLETE_MODE` | override the mode for one command |
| `ZCOMPLETE_DISABLE` | switch corrections off for one shell |
| `ZCOMPLETE_DATA_DIR` | move the database |
| `NO_COLOR`, `TERM` | colour |
| `HISTFILE`, `XDG_DATA_HOME` | read by `import`, honoured for the data path |

## Shell differences

**zsh and bash correct an alias a moment later than a program.** Both run the
not-found handler in a forked child, which loses what an alias or function does
to the shell itself (`cd`, `set`, `export`). When the answer is one of those,
zcomplete hands it back for the next prompt to run in the real shell. Looks the
same, costs one extra call on a command that had already failed.

**Your prompt sees the word you typed, not the command that ran.** The fork
can't tell the shell it corrected anything. Usually invisible, but a prompt that
reacts to specific commands reacts to the typo. Under powerlevel10k a corrected
`clea` leaves the blank line a typed `clear` would have suppressed;
`POWERLEVEL9K_PROMPT_ADD_NEWLINE=false` removes it.

**fish corrects the line before it runs, so zcomplete binds enter there.** fish
calls `fish_command_not_found` only after abandoning the job, so correcting from
inside it would hand `cat` the keyboard in `printf x | ct`. Rewriting the line
first keeps pipes, redirections, job control and `$status` normal. zcomplete
chains onto whatever enter was already bound to, so load it last.

**bash 3.2, the version macOS ships, can't intercept anything.**
`command_not_found_handle` arrived in bash 4.0. On 3.2 zcomplete still learns,
and offers the fix at the next prompt instead:

```
$ mkd build
bash: mkd: command not found
zcomplete: run mkdir build? [Y/n] y
```

That runs in the real shell, so `cd` works. Install bash 4+ for in-place
correction.

## Update

Run the install command again. It replaces the binary and leaves your database,
mode and shell config alone.

```bash
curl -fsSL https://raw.githubusercontent.com/omarfakih1/zcomplete/main/install.sh | sh
```

The new binary is written beside the old one and renamed over it, so a shell
mid-command keeps running the version it started with. Open shells use the old
one until the hook next starts a process. `exec $SHELL` picks up the new one
straight away, and is needed anyway if the integration script itself changed:

```bash
zcomplete --version && exec $SHELL
```

From a checkout: `git pull && ./install.sh && exec $SHELL`. `--version=vX.Y.Z`
installs a specific release, which is also how you go back.

## Uninstall

```bash
curl -fsSL https://raw.githubusercontent.com/omarfakih1/zcomplete/main/uninstall.sh | sh
```

Takes the lines back out of your shell config (leaving a `.zcomplete.bak` beside
each), deletes the database, removes the binary. `--keep-data` keeps what it
learned. Shells you already have open keep the hook until you `exec $SHELL`.

## Tests

`cargo test` doesn't cover [src/init/](src/init). Those are tested by driving a
real zsh, bash and fish under a pty, which needs all three installed and takes
about a minute:

```bash
cargo test && cargo build --release && python3 .github/tests/shells.py
```

`.github/tests/bench.sh` times the hot path against `/usr/bin/true`.

## License

MIT.
