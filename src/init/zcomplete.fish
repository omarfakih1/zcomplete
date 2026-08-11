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

function __zcomplete_record --on-event fish_postexec
    set -l ret $status
    # The rerun below is itself a command, and fish would announce it here.
    if set -q __zcomplete_rerunning
        return
    end
    set -l word (__zcomplete_first_word $argv[1])
    test -n "$word"; or return
    if string match -q -- '*/*' $word
        return
    end

    set -l kind auto
    if functions -q -- $word; or contains -- $word $__zcomplete_builtins
        set kind shell
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
complete -c zcomplete -n __fish_use_subcommand -a doctor -d 'check the installation'
