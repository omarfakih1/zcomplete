# zcomplete: fish integration, loaded by  zcomplete init fish | source
#
# fish calls fish_command_not_found *after* abandoning the job, with stdin and
# stdout back on the terminal: correcting there would hand `cat` the keyboard
# in `echo hi | ct` and hang the shell. So enter rewrites the command line
# instead and fish runs the corrected line itself.
if not set -q __zcomplete_builtins
    set -g __zcomplete_builtins (builtin --names)
end

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

set -g __zcomplete_since 0
if set -q ZCOMPLETE_DATA_DIR; and test -n "$ZCOMPLETE_DATA_DIR"
    set -g __zcomplete_journal $ZCOMPLETE_DATA_DIR/journal.$fish_pid
else if set -q XDG_DATA_HOME; and test -n "$XDG_DATA_HOME"
    set -g __zcomplete_journal $XDG_DATA_HOME/zcomplete/journal.$fish_pid
else
    set -g __zcomplete_journal $HOME/.local/share/zcomplete/journal.$fish_pid
end
# Created 0600 here, once, because a redirect takes the shell's umask and the
# per-command path must not fork to fix it afterwards.
if not test -e $__zcomplete_journal
    # Set and put back, rather than forked into a `fish -c` subshell: that
    # looked the shell up on PATH, and a fish whose own binary is not on PATH
    # printed `Unknown command: fish` into the session and left the journal to
    # be created by the first append instead - at the user's umask, which is
    # usually 0644, on a file listing every command they run and where.
    set -l __zcomplete_umask (umask)
    umask 077
    echo -n '' >>$__zcomplete_journal 2>/dev/null
    umask $__zcomplete_umask
end

function __zcomplete_record --on-event fish_postexec
    set -l ret $status
    # The rerun below is itself a command, and fish would announce it here.
    # Cleared on the way past rather than after the eval: a ctrl-c during the
    # rerun never reaches the line that clears it, and a guard left standing
    # would silence the hook for the rest of the session.
    if set -q __zcomplete_rerunning
        set -e __zcomplete_rerunning
        return
    end
    # Split once, inline: a command substitution is the most expensive thing
    # left on this path, and calling out for the first word and again for the
    # second paid for two.
    set -l words (string split -n ' ' -- $argv[1])
    while set -q words[1]
        switch $words[1]
            case '*=*' sudo doas command builtin nohup exec env time nice stdbuf
                set -e words[1]
            case '*'
                break
        end
    end
    set -l word $words[1]
    test -n "$word"; or return
    if string match -q -- '*/*' $word
        return
    end

    set -l kind auto
    set -l jkind x
    if functions -q -- $word; or contains -- $word $__zcomplete_builtins
        set kind shell
        set jkind fish
    end

    # A command that worked needs no correction, so all that is left is counting
    # it, and an append costs no process where starting zcomplete costs one.
    # fish has no clock builtin and `date` is a process, so the time is left at
    # 0 and the fold dates the line by the journal's own mtime instead.
    if test $ret -eq 0
        set -l verb ''
        for candidate in $words[2..-1]
            if string match -qr '^[A-Za-z0-9_][-_A-Za-z0-9]*$' -- $candidate
                set verb $candidate
                break
            end
        end
        if string match -qr '[|&;()`]' -- $argv[1]
            set verb ''
        end
        # A newline in $PWD would end the record early and let the rest of the
        # directory's name pose as a second one, so such a directory goes
        # uncounted. `2>/dev/null` goes first: a redirection that fails reports
        # it on whatever stderr is at the time.
        if not string match -qr \n -- $PWD
            echo "0 $jkind $word $verb $PWD" 2>/dev/null >>$__zcomplete_journal
        end
        set -g __zcomplete_since (math $__zcomplete_since + 1)
        if test $__zcomplete_since -ge 200
            set -g __zcomplete_since 0
            command zcomplete flush 2>/dev/null
        end
        return
    end

    set -l fixed (command zcomplete record --shell fish --kind $kind --status $ret -- $argv[1] | string collect)
    if test -n "$fixed"
        set -g __zcomplete_rerunning 1
        eval $fixed
        set -e __zcomplete_rerunning
    end
end

function __zcomplete_rewrite
    # `string collect` keeps a multi-line buffer as one string; without it a
    # `for` loop's body ends up spliced onto its header.
    set -l line (commandline | string collect)

    set -l unknown
    for part in (string split -- '|' (string replace -ra '[;&()\n]' '|' -- $line))
        set -l word (__zcomplete_first_word "$part")
        if test -n "$word"; and not type -q -- $word
            set -a unknown $word
        end
    end

    if set -q unknown[1]
        set -l fixed (command zcomplete retry --shell fish --inline --only (string join ',' $unknown) -- $line | string collect)
        set -l answered $status
        if test $answered -eq 0 -a -n "$fixed"
            if type -q -- (__zcomplete_first_word "$fixed")
                commandline --replace -- $fixed
            end
        end
    end
end

# Hand over to whatever enter already did rather than calling `commandline -f
# execute`: fish's own knows about incomplete commands, abbreviations and
# history, and a user's own binding survives. Enter is CR from a terminal
# and LF from anything feeding fish through a pty.
function __zcomplete_bind_enter --argument-names mode key
    # fish prints the preset binding first and any binding of the user's own
    # after it, so the last line is the one in force. The flags between `bind`
    # and the key have to be stepped over rather than assumed away: under vi
    # keys the preset reads `bind --preset -m insert enter execute`, and taking
    # the first word after `--preset` would leave `insert enter execute` as the
    # command. `-m` also has to be carried over, since it is what makes enter
    # leave normal mode.
    set -l lines (bind -M $mode $key 2>/dev/null)
    set -l command
    set -l sets
    if set -q lines[-1]
        set command (string replace -r \
            '^bind\s+(?:(?:--preset|-s|--silent)\s+|(?:-M|--mode|-m|--sets-mode|-k|--key)\s+\S+\s+)*\S+\s+' \
            '' -- $lines[-1])
        set sets (string match -r '(?:^|\s)(?:-m|--sets-mode)\s+(\S+)' -- $lines[-1])
        # `bind` quotes a command that needs it, and passing those quotes back
        # would bind the quoted string rather than what it stands for.
        set command (string unescape --style=script -- $command)
    end
    if test -z "$command"; or string match -q '*__zcomplete_rewrite*' -- "$command"
        set command execute
    end
    # A bare `execute` bound alongside another command goes dead on fish 3.x:
    # `bind` installs it without complaint, but the key then fires nothing at
    # all, not even the other command. `commandline -f execute` is the same
    # action spelled as a normal command instead of that special-cased name.
    if test "$command" = execute
        set command 'commandline -f execute'
    end
    if set -q sets[2]
        bind -M $mode -m $sets[2] $key __zcomplete_rewrite $command
    else
        bind -M $mode $key __zcomplete_rewrite $command
    end
end

for key in \r \n
    __zcomplete_bind_enter default $key
    __zcomplete_bind_enter insert $key
end

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
complete -c zcomplete -n __fish_use_subcommand -a stats -d 'list learned commands, or one command\'s subcommands'
complete -c zcomplete -n __fish_use_subcommand -a import -d 'seed the database from shell history'
complete -c zcomplete -n __fish_use_subcommand -a forget -d 'drop a command'
complete -c zcomplete -n __fish_use_subcommand -a bind -d 'pin a shortcut to a command'
complete -c zcomplete -n __fish_use_subcommand -a unbind -d 'remove a pinned shortcut'
complete -c zcomplete -n __fish_use_subcommand -a ignore -d 'never suggest a command'
complete -c zcomplete -n __fish_use_subcommand -a mode -d 'show the confirmation mode'
complete -c zcomplete -n __fish_use_subcommand -a safe -d 'confirm every correction'
complete -c zcomplete -n __fish_use_subcommand -a unsafe -d 'confirm only dangerous corrections'
complete -c zcomplete -n __fish_use_subcommand -a bypass -d 'never confirm'
complete -c zcomplete -n __fish_use_subcommand -a on -d 'enable corrections'
complete -c zcomplete -n __fish_use_subcommand -a off -d 'disable corrections'
complete -c zcomplete -n __fish_use_subcommand -a flush -d 'fold what the shells have buffered'
complete -c zcomplete -n __fish_use_subcommand -a doctor -d 'check the installation'
