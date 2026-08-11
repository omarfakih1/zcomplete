# zcomplete: zsh integration, loaded by  eval "$(zcomplete init zsh)"
# Nothing here uses `return`, which inside an `eval` would abandon the rest
# of your .zshrc. Sourcing twice is harmless.

# Keep a handler already installed, guarded against capturing our own.
if (( $+functions[command_not_found_handler] )) && (( ! $+functions[__zcomplete_previous] )); then
    if [[ $functions[command_not_found_handler] != *zcomplete* ]]; then
        functions[__zcomplete_previous]=$functions[command_not_found_handler]
    fi
fi

__zcomplete_first_word() {
    local -a words
    words=(${(z)1})
    while (( $#words )); do
        case $words[1] in
            *=*|sudo|doas|command|builtin|nohup|exec|env|time|nice|stdbuf) shift words ;;
            *) break ;;
        esac
    done
    REPLY=${words[1]:-}
}

__zcomplete_preexec() {
    __zcomplete_typed=$1
}

# After the command, not before: the exit status is half the lesson, and
# `git sttaus` must never teach us that git has a `sttaus`.
__zcomplete_precmd() {
    local ret=$?
    emulate -L zsh
    local line=$__zcomplete_typed
    __zcomplete_typed=
    [[ -n $line ]] || return $ret

    local REPLY kind fixed
    __zcomplete_first_word "$line"
    [[ -n $REPLY && $REPLY != */* ]] || return $ret

    # The not-found hook ran in a fork, found an alias or a function there, and
    # handed it back rather than running it: a fork is exactly what cannot keep
    # what those do. Here we are the shell itself, so a `cd` in one sticks.
    if (( ret == 5 )); then
        fixed=$(\command zcomplete retry --shell zsh --only "$REPLY" -- "$line")
        if [[ -n $fixed ]]; then
            eval -- "$fixed"
            return $?
        fi
        print -ru2 -- "zsh: command not found: $REPLY"
        return 127
    fi

    # Not (( $+commands[...] )): subscripts there are arithmetic, so `git-lfs`
    # asks about "git minus lfs" and quietly answers no.
    if [[ -n ${aliases[$REPLY]+x}${functions[$REPLY]+x}${builtins[$REPLY]+x} ]]; then
        kind=shell
    else
        kind=auto
    fi

    fixed=$(\command zcomplete record --shell zsh --kind $kind --status $ret -- "$line")
    if [[ -n $fixed ]]; then
        eval -- "$fixed"
        return $?
    fi
    return $ret
}

command_not_found_handler() {
    emulate -L zsh
    # Child-local: zsh runs this in the fork it made to exec the missing
    # command. Without it, an uninstalled zcomplete recurses until FUNCNEST.
    unfunction command_not_found_handler

    local word=$1 target ret
    local -a reply
    if [[ -o interactive ]]; then
        reply=(${(f)"$(\command zcomplete resolve --shell zsh --subshell -- "$@")"})
        ret=$?
        target=${reply[1]:-}
    else
        ret=1
    fi

    if (( ret == 0 )) && [[ -n $target ]] && whence -- "$target" >/dev/null 2>&1; then
        shift
        [[ ${reply[2]:-} == verb\ * ]] && set -- "${reply[2]#verb }" "${@:2}"
        if [[ -n ${aliases[$target]+x} ]]; then
            eval -- ${(q)target} ${(q)@}
        else
            "$target" "$@"
        fi
        return $?
    fi
    # Left for precmd, which is the shell and not a fork of it. Silent, because
    # the correction has not been offered yet.
    (( ret == 5 )) && return 5
    (( ret == 130 )) && return 130

    if (( $+functions[__zcomplete_previous] )); then
        __zcomplete_previous "$@"
        return $?
    fi
    print -ru2 -- "zsh: command not found: $word"
    return 127
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec __zcomplete_preexec
add-zsh-hook precmd __zcomplete_precmd

_zcomplete() {
    local -a subcommands
    subcommands=(
        'init:print shell integration'
        'query:show what a word would resolve to'
        'stats:list learned commands, or one command'"'"'s subcommands'
        'import:seed the database from shell history'
        'forget:drop a command'
        'bind:pin a shortcut to a command'
        'unbind:remove a pinned shortcut'
        'ignore:never suggest a command'
        'mode:show the confirmation mode'
        'safe:confirm every correction'
        'unsafe:confirm only dangerous corrections'
        'bypass:never confirm'
        'on:enable corrections'
        'off:disable corrections'
        'doctor:check the installation'
    )
    if (( CURRENT == 2 )); then
        _describe 'command' subcommands
        _values 'flag' '--version' '--help'
    else
        _default
    fi
}
if (( $+functions[compdef] )); then
    compdef _zcomplete zcomplete 2>/dev/null
fi

# compdef can be an autoload stub compinit has not filled in yet, and a failed
# registration must not hand your .zshrc a non-zero status.
true
