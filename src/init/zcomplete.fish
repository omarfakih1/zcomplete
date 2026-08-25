# zcomplete: fish integration, loaded by  zcomplete init fish | source
#
# fish calls fish_command_not_found after abandoning the job, so correcting
# there would hand `cat` the keyboard in `echo hi | ct`. Enter rewrites the
# command line instead and fish runs the corrected line itself.
if not set -q __zcomplete_builtins
    set -g __zcomplete_builtins (builtin --names)
end

if functions -q fish_command_not_found; and not functions -q __zcomplete_previous
    if not string match -q '*zcomplete*' -- (functions fish_command_not_found)
        functions --copy fish_command_not_found __zcomplete_previous
    end
end

# Sets $__zcomplete_words to the line past its wrappers. A global rather than a
# return value: a command substitution is the most expensive thing on the enter
# and postexec paths, and this way the call itself needs none.
function __zcomplete_split --argument-names line
    set -g __zcomplete_words (string match -ra '\S+' -- $line)
    while set -q __zcomplete_words[1]
        switch $__zcomplete_words[1]
            case '*=*' sudo doas command builtin nohup exec env time nice stdbuf
                set -e __zcomplete_words[1]
            case '*'
                break
        end
    end
end

set -g __zcomplete_since
if set -q ZCOMPLETE_DATA_DIR; and test -n "$ZCOMPLETE_DATA_DIR"
    set -g __zcomplete_journal $ZCOMPLETE_DATA_DIR/journal.$fish_pid
else if set -q XDG_DATA_HOME; and test -n "$XDG_DATA_HOME"
    set -g __zcomplete_journal $XDG_DATA_HOME/zcomplete/journal.$fish_pid
else
    set -g __zcomplete_journal $HOME/.local/share/zcomplete/journal.$fish_pid
end
if not test -e $__zcomplete_journal
    # A redirect takes the shell's umask, and this file lists every command you
    # run and where. Set and put back rather than forked into `fish -c`, which
    # looked the shell up on PATH and printed `Unknown command: fish` when it
    # was not there.
    set -l __zcomplete_umask (umask)
    umask 077
    echo -n '' >>$__zcomplete_journal 2>/dev/null
    umask $__zcomplete_umask
end

function __zcomplete_record --on-event fish_postexec
    set -l ret $status
    # The rerun below is a command too, and fish announces it here. Cleared on
    # the way past rather than after the eval: a ctrl-c during the rerun never
    # reaches the line that clears it, and a guard left standing would silence
    # the hook for the rest of the session.
    if set -q __zcomplete_rerunning
        set -e __zcomplete_rerunning
        return
    end
    __zcomplete_split $argv[1]
    set -l word $__zcomplete_words[1]
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

    # An append costs no process where starting zcomplete costs one. fish has no
    # clock builtin, so the time is left at 0 and the fold dates the line by the
    # journal's own mtime.
    if test $ret -eq 0
        set -l verb ''
        for candidate in $__zcomplete_words[2..-1]
            if string match -qr '^[A-Za-z0-9_][-_A-Za-z0-9]*$' -- $candidate
                set verb $candidate
                break
            end
        end
        if string match -qr '[|&;()`\n]' -- $argv[1]
            set verb ''
        end
        # A newline in $PWD would end the record early and let the rest of the
        # name pose as a second directory. Paid for only when there is one.
        set -l here $PWD
        if string match -qr \n -- $here
            set here (string replace -a \n '?' -- $here)
        end
        echo "0 $jkind $word $verb $here" 2>/dev/null >>$__zcomplete_journal
        set -ga __zcomplete_since x
        if set -q __zcomplete_since[200]
            set -g __zcomplete_since
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
        __zcomplete_split $part
        set -l word $__zcomplete_words[1]
        if test -n "$word"; and not type -q -- $word
            set -a unknown $word
        end
    end

    if set -q unknown[1]
        set -l fixed (command zcomplete retry --shell fish --inline --only (string join ',' $unknown) -- $line | string collect)
        if test $status -eq 0 -a -n "$fixed"
            __zcomplete_split $fixed
            if type -q -- $__zcomplete_words[1]
                commandline --replace -- $fixed
            end
        end
    end
end

# Hand over to whatever enter already did rather than calling `commandline -f
# execute`: fish's own knows about incomplete commands, abbreviations and
# history, and a user's own binding survives. Enter is CR from a terminal and
# LF from anything feeding fish through a pty.
function __zcomplete_bind_enter --argument-names mode key
    # The last line is the binding in force. The flags between `bind` and the
    # key are stepped over rather than assumed away: under vi keys the preset
    # reads `bind --preset -m insert enter execute`. `-m` is carried over, since
    # it is what makes enter leave normal mode.
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
    # A bare `execute` bound after another command goes dead before fish 4: the
    # key then fires nothing at all. Queueing it runs there, and the queue is
    # dropped when a command that took the terminal hands it back, which is
    # every correction we make.
    if test "$command" = execute; and string match -qr '^[0-3]\.' -- $FISH_VERSION
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
