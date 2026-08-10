# zcomplete — fish integration.
# Loaded by:  zcomplete init fish | source
#
# fish is corrected differently from zsh and bash. Their not-found handlers run
# in the fork that was already going to exec the command, so running something
# else there inherits the job's pipes and redirections. fish calls
# fish_command_not_found *after* abandoning the job, with stdin and stdout on the
# terminal — `echo hi | ct` would hand `cat` the keyboard and hang the shell, and
# `unam > out.txt` would print to the screen and leave out.txt empty. So the fix
# happens a moment earlier: enter rewrites the command line and fish runs the
# corrected line itself, which keeps pipes, redirections, job control and $status
# behaving exactly as if the right word had been typed.
#
# Sourcing this twice is harmless.

if not set -q __zcomplete_builtins
    set -g __zcomplete_builtins (builtin --names)
end

# Preserve a handler the user already had, unless it is ours from a second load.
if functions -q fish_command_not_found; and not functions -q __zcomplete_previous
    if not string match -q '*zcomplete*' -- (functions fish_command_not_found)
        functions --copy fish_command_not_found __zcomplete_previous
    end
end

function __zcomplete_first_word
    set -l words (string split -n ' ' -- $argv[1])
    while set -q words[1]
        switch $words[1]
            case '*=*' sudo doas command builtin nohup exec env time nice stdbuf
                set -e words[1]
            case '*'
                break
        end
    end
    if set -q words[1]
        echo -- $words[1]
    end
end

function __zcomplete_record --on-event fish_preexec
    set -l word (__zcomplete_first_word $argv[1])
    test -n "$word"; or return 0
    if string match -q -- '*/*' $word
        return 0
    end

    set -l kind auto
    if functions -q -- $word; or contains -- $word $__zcomplete_builtins
        set kind shell
    end

    command zcomplete record --shell fish --kind $kind -- $word
end

function __zcomplete_rewrite
    # `string collect` keeps a multi-line buffer as one string. Without it fish
    # splits the command substitution on newlines, the lines arrive as separate
    # arguments, and a `for` loop's body ends up spliced onto its header.
    set -l line (commandline | string collect)

    # Cheap gate first: split on the separators a command can follow and see
    # whether any of them starts with a word this shell cannot run. Only then is
    # it worth starting a process, so pressing enter on a line that is already
    # fine costs nothing. zcomplete does the quote-aware scan properly.
    set -l unknown
    for part in (string split -- '|' (string replace -ra '[;&()\n]' '|' -- $line))
        set -l word (__zcomplete_first_word "$part")
        if test -n "$word"; and not type -q -- $word
            set -a unknown $word
        end
    end

    if set -q unknown[1]
        # zcomplete writes its question straight to the terminal, while fish's
        # reader still believes it owns that line and knows where the cursor
        # sits. Save the cursor, give the question the line to itself, then put
        # the cursor back and wipe what we drew, so fish redraws from exactly
        # the state it left. This is how any full-screen fish binding behaves;
        # skip it and the question lands on top of what you typed and the
        # redraw smears the rest across the screen.
        set -l fixed (command zcomplete retry --shell fish --inline --only (string join ',' $unknown) -- $line | string collect)
        set -l answered $status
        if test $answered -eq 0 -a -n "$fixed"
            # The store can name a function a config no longer defines.
            if type -q -- (__zcomplete_first_word "$fixed")
                commandline --replace -- $fixed
            end
        end
        commandline -f repaint
    end
end

# We rewrite the command line and then hand over to whatever enter already did,
# rather than calling `commandline -f execute` ourselves. fish's own `execute`
# knows about incomplete commands, abbreviations and history; and a user who has
# bound enter to something of their own keeps it. Enter arrives as CR from a
# terminal and as LF from anything feeding fish a script through a pty.
function __zcomplete_bind_enter --argument-names mode key
    set -l existing (bind -M $mode $key 2>/dev/null | string replace -r '^bind\s+(--preset\s+)?\S+\s+' '')
    if test -z "$existing"; or string match -q '*__zcomplete_rewrite*' -- "$existing"
        set existing execute
    end
    bind -M $mode $key __zcomplete_rewrite $existing
end

for key in \r \n
    __zcomplete_bind_enter default $key
    __zcomplete_bind_enter insert $key
end

# Still worth defining: the binding only sees the first word of a line the user
# typed, so anything reaching fish another way lands here. It deliberately does
# not run the correction - see the note at the top.
function fish_command_not_found
    if functions -q __zcomplete_previous
        __zcomplete_previous $argv
        return $status
    end
    if functions -q __fish_default_command_not_found_handler
        __fish_default_command_not_found_handler $argv
        return $status
    end
    printf 'fish: Unknown command: %s\n' $argv[1] >&2
    return 127
end

complete -c zcomplete -f
complete -c zcomplete -n __fish_use_subcommand -a init -d 'print shell integration'
complete -c zcomplete -n __fish_use_subcommand -a query -d 'show what a word resolves to'
complete -c zcomplete -n __fish_use_subcommand -a stats -d 'list learned commands by score'
complete -c zcomplete -n __fish_use_subcommand -a import -d 'seed the database from shell history'
complete -c zcomplete -n __fish_use_subcommand -a forget -d 'drop a command'
complete -c zcomplete -n __fish_use_subcommand -a bind -d 'pin a shortcut to a command'
complete -c zcomplete -n __fish_use_subcommand -a unbind -d 'remove a pinned shortcut'
complete -c zcomplete -n __fish_use_subcommand -a ignore -d 'never suggest a command'
complete -c zcomplete -n __fish_use_subcommand -a mode -d 'show or set the confirmation mode'
complete -c zcomplete -n __fish_use_subcommand -a doctor -d 'check the installation'
complete -c zcomplete -n __fish_use_subcommand -a export -d 'dump the database as text'
complete -c zcomplete -l safe -d 'confirm every correction'
complete -c zcomplete -l unsafe -d 'confirm only dangerous corrections'
complete -c zcomplete -l bypass -d 'never confirm'
