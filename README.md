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

$ cle
zcomplete: run clear instead of 'cle'? [Y/n] y
```

One keypress, no Enter. It only ever suggests a command that exists right now —
if you type `clean` fifty times and `clean` isn't installed, `cle` still gets you
`clear`.

It is `zoxide` for commands instead of directories, and it borrows zoxide's
frecency ranking wholesale.

## Install

```bash
git clone https://github.com/omarfakih1/zcomplete && cd zcomplete && ./install.sh
```

The script builds the binary, adds one line to your shell's config, and seeds
the database from the shell history you already have. To do it by hand:

```bash
cargo install --path .
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

`zcomplete doctor` checks all of that and says what is missing.

## Modes

```bash
zcomplete --safe      # confirm every correction (default)
zcomplete --unsafe    # run ordinary corrections; still confirm dangerous ones
zcomplete --bypass    # never confirm
```

The mode is read on every correction, so changing it takes effect in shells that
are already open. `ZCOMPLETE_MODE=safe ./script.sh` overrides it for one command.

"Dangerous" is decided in [src/safety.rs](src/safety.rs) — one file, one list.
`rm`, `dd`, `sudo`, `mkfs*`, `git push --force`, `git reset --hard`,
`terraform destroy`, `kubectl delete`, recursive `chmod`, a `curl … | sh`, and
about thirty more. It knows the difference between `git clean -fd` and
`git clean -n`, and it reads flags the way the shell does, so `-rf`, `-r -f`,
`--force` and a `--` terminator all land correctly. Add your own:

```toml
always_confirm = ["deploy", "terraform"]
```

## How it decides

Each command you run gets a rank, and each rank decays with age — the same
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
| typo | `gti` → `git`, `gut` → `git` |

Each produces a similarity, and the final score is that similarity times the
*logarithm* of frecency. The compression is the point: how well a word matches
is the main signal, and how often you run something can overturn a close call
but not a wide one. Ranking the four kinds absolutely instead sounds tidier and
is wrong in both directions — with coreutils installed it made `gti` mean
`gtimeout` rather than `git`; ranking on raw frecency instead made `rmd` mean
`rm` rather than `rmdir`.

One rule is absolute: a match that had to add or drop letters never displaces
one that did not. If you typed a genuine prefix of something, you were spelling
that word. Words under two letters are never corrected, and a typo has to be
within one edit for a short word, two for a long one — past that zcomplete says
nothing and you get the usual error.

Two invariants hold in every mode, including `--bypass`:

- **It never runs something that isn't a command.** A word only enters the
  database if the shell could resolve it at the time, and every candidate is
  re-checked against `PATH` before it is offered. Uninstall a tool and it stops
  being suggested.
- **It never touches a non-interactive shell.** No controlling terminal means no
  correction, so nothing gets rewritten inside a script or a CI job.

When nothing you have run fits, it will consider anything on `PATH` as a last
resort, but only for prefix and near-miss matches. That is what makes a fresh
install useful before it has learned anything. Turn it off with
`path_fallback = false`.

### It learns from your answers

Confirm `mkd` → `mkdir` three times and it stops being a guess: zcomplete jumps
straight there. Turn the same suggestion down twice and it is never offered
again. You can also say so directly:

```bash
zcomplete bind gs "git"      # gs always means git
zcomplete unbind gs
zcomplete ignore sl          # never suggest sl, ever
```

## Commands

```
zcomplete stats [-n N]           what it has learned, strongest first
zcomplete query <word>           what a word would resolve to
zcomplete query <word> --score   and why, with the tier and the score
zcomplete query <word> -i        pick from the matches (uses fzf if installed)
zcomplete import [zsh|bash|fish] seed from shell history (--dry-run to look first)
zcomplete forget <command>...    unlearn commands, or --all
zcomplete ignore [<command>...]  list, add to, or --remove from the never-suggest list
zcomplete bind <word> <command>  pin a shortcut; unbind removes it
zcomplete mode [safe|unsafe|bypass]
zcomplete on | off               without touching your shell config
zcomplete export                 the database as text; import --restore reads it back
zcomplete doctor                 check the installation
```

## Configuration

`~/.config/zcomplete/config.toml`, all optional:

```toml
mode = "safe"            # safe | unsafe | bypass
enabled = true
min_input = 2            # never correct a word shorter than this
context_weight = 4.0     # how much a use in this directory counts for
typo_limit = 2           # maximum edit distance, 0 disables typo matching
max_candidates = 5       # size of the picker when the match is unclear
ambiguity = 0.75         # runner-up this close to the winner means "ask"
path_fallback = true     # consider installed-but-never-used commands
color = "auto"           # auto | always | never
always_confirm = []      # extra commands to prompt for in unsafe mode
```

Environment: `ZCOMPLETE_MODE` overrides the mode for one command,
`ZCOMPLETE_DISABLE` switches it off, `ZCOMPLETE_CONFIG` and `ZCOMPLETE_DATA_DIR`
move the files. `NO_COLOR` is respected, `HISTFILE` is read by `import`, and
`XDG_CONFIG_HOME` and `XDG_DATA_HOME` are honoured;
otherwise the database lives in `~/.local/share/zcomplete/commands.bin`, mode
0600, and holds command names only — never arguments.

## Speed

It runs on every command you type, so it is worth knowing what that costs.
On an M-series Mac, 2000 learned commands, 2254 executables on PATH:

```
process spawn (the floor)            1.39 ms
record  (every command)              2.33 ms
resolve (learned hit)                2.59 ms
resolve (cold, scans PATH)           3.95 ms
```

The first line is what any process costs to start, so the tax on a command you
type is about a millisecond. Reproduce with `./tests/bench.sh`.

## What it does not do

**In zsh and bash, corrections run in a subshell.** Both run the not-found
handler in a forked child, so if the command you meant is a shell function that
does `cd` or sets a variable, the correction runs but the side effect does not
survive. External commands, which is nearly all of them, are unaffected.

**fish is corrected a moment earlier, which means zcomplete binds enter there.**
fish calls `fish_command_not_found` only after it has given up on the job, with
stdin and stdout back on the terminal — running the correction from inside it
would make `printf x | ct` hand `cat` the keyboard and hang the shell, and
`unam > out.txt` print to the screen. So on fish the command line is rewritten
before it runs, and fish executes it itself. Pipes, redirections, command
substitution, job control and `$status` all behave normally, and unlike zsh and
bash a corrected shell function keeps its side effects. The cost is the key
binding: if something else in your config rebinds enter after zcomplete loads,
load zcomplete last.

**bash 3.2 — the bash that ships with macOS — cannot intercept anything.**
`command_not_found_handle` arrived in bash 4.0. On 3.2 zcomplete still learns,
and it offers the fix on the next prompt instead of running it in place:

```
$ mkd build
bash: mkd: command not found
zcomplete: run mkdir build? [Y/n] y
```

That path runs in your real shell rather than a subshell, so `cd` works there.
For in-place correction, `brew install bash`.

**Only the first word.** `git ci` is git's business, not ours — `git` exists, so
the shell never asks.

## Uninstall

```bash
zcomplete forget --all
rm -rf ~/.local/share/zcomplete ~/.config/zcomplete
```

and delete the `zcomplete init` line from your shell config.

## Development

```bash
cargo test               # unit tests, including the resolution table
python3 tests/shells.py  # drives real zsh, bash and fish through a pty
```

The pty suite is the one that matters: preexec only fires in an interactive
shell and the prompt reads `/dev/tty` directly, so nothing less proves it works.

MIT.
