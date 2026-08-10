# zcomplete — fish integration.
# Loaded by:  zcomplete init fish | source
#
# Sourcing this twice is harmless.

if not set -q __zcomplete_builtins
    set -g __zcomplete_builtins (builtin --names)
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

function fish_command_not_found
    set -l target ""
    set -l ret 1
    if status is-interactive
        set target (command zcomplete resolve --shell fish -- $argv)
        set ret $status
    end

    if test $ret -eq 0 -a -n "$target"
        $target $argv[2..-1]
        return $status
    end
    # 130 is a Ctrl-C at the prompt; the user has already said enough.
    if test $ret -eq 130
        return 130
    end

    if functions -q __fish_default_command_not_found_handler
        __fish_default_command_not_found_handler $argv
        return $status
    end
    printf 'fish: Unknown command: %s\n' $argv[1] >&2
    return 127
end

# fish 3.0 and 3.1 dispatch through the older name.
function __fish_command_not_found_handler --on-event fish_command_not_found
    fish_command_not_found $argv
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
