# zcomplete — zsh integration.
# Loaded by:  eval "$(zcomplete init zsh)"
#
# Sourcing this twice is harmless; nothing here uses `return`, which inside an
# `eval` would abandon the rest of your .zshrc.

# Keep any handler that was already installed (Homebrew's, a distro's) so a word
# zcomplete cannot place still reaches whoever asked for it. Guarded so a second
# load does not capture our own handler and recurse forever.
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

__zcomplete_record() {
    emulate -L zsh
    local REPLY kind
    __zcomplete_first_word "$1"
    [[ -n $REPLY && $REPLY != */* ]] || return 0

    # Subscripts inside (( )) are evaluated as arithmetic, so $+commands[git-lfs]
    # asks about a key named "git minus lfs" and quietly answers no. Every
    # hyphenated command and every alias went unlearned that way.
    if [[ -n ${aliases[$REPLY]+x}${functions[$REPLY]+x}${builtins[$REPLY]+x} ]]; then
        kind=shell
    else
        # $commands is a hash zsh fills at startup, so a tool installed since
        # then is missing from it. Hand the word over and let zcomplete look at
        # PATH itself; it refuses to record anything it cannot find.
        kind=auto
    fi

    \command zcomplete record --shell zsh --kind $kind -- $REPLY
}

command_not_found_handler() {
    emulate -L zsh
    # zsh runs this in the fork it made to exec the missing command, so removing
    # ourselves here is child-local. Without it, a zcomplete that has been
    # uninstalled while this snippet is still in .zshrc recurses until zsh gives
    # up on FUNCNEST.
    unfunction command_not_found_handler

    local word=$1 target ret
    if [[ -o interactive ]]; then
        target=$(\command zcomplete resolve --shell zsh -- "$@")
        ret=$?
    else
        ret=1
    fi

    # The store can name a function from a .zshrc that no longer defines it.
    # `whence` searches for real, unlike the $commands hash, which zsh fills at
    # startup and which therefore misses anything installed since.
    if (( ret == 0 )) && [[ -n $target ]] && whence -- "$target" >/dev/null 2>&1; then
        shift
        if [[ -n ${aliases[$target]+x} ]]; then
            eval -- ${(q)target} ${(q)@}
        else
            "$target" "$@"
        fi
        return $?
    fi
    # 130 is a Ctrl-C at the prompt; the user has already said enough.
    (( ret == 130 )) && return 130

    if (( $+functions[__zcomplete_previous] )); then
        __zcomplete_previous "$@"
        return $?
    fi
    print -ru2 -- "zsh: command not found: $word"
    return 127
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec __zcomplete_record

_zcomplete() {
    local -a subcommands
    subcommands=(
        'init:print shell integration'
        'query:show what a word would resolve to'
        'stats:list learned commands by score'
        'import:seed the database from shell history'
        'forget:drop a command'
        'bind:pin a shortcut to a command'
        'unbind:remove a pinned shortcut'
        'ignore:never suggest a command'
        'mode:show or set the confirmation mode'
        'on:enable corrections'
        'off:disable corrections'
        'doctor:check the installation'
        'export:dump the database as text'
    )
    if (( CURRENT == 2 )); then
        _describe 'command' subcommands
        _values 'flag' '--safe' '--unsafe' '--bypass' '--version' '--help'
    else
        _default
    fi
}
if (( $+functions[compdef] )); then
    compdef _zcomplete zcomplete 2>/dev/null
fi

# compdef can be an autoload stub that compinit has not filled in yet, and a
# failed completion registration is not a reason for `eval "$(zcomplete init
# zsh)"` to hand your .zshrc a non-zero status.
true
