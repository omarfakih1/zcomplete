# zcomplete — bash integration.
# Loaded by:  eval "$(zcomplete init bash)"
#
# Interception needs bash 4.0 or newer, which is where command_not_found_handle
# arrived. On the bash 3.2 that ships with macOS the recording half still works,
# so a newer bash started later already knows your habits; `zcomplete doctor`
# says so out loud.
#
# Sourcing this twice is harmless; nothing here uses `return`, which inside an
# `eval` would abandon the rest of your .bashrc.

if declare -F command_not_found_handle >/dev/null 2>&1 &&
    ! declare -F __zcomplete_previous >/dev/null 2>&1; then
    case "$(declare -f command_not_found_handle)" in
        *zcomplete*) ;;
        *) eval "__zcomplete_previous() $(declare -f command_not_found_handle | tail -n +2)" ;;
    esac
fi

# Builtins, matched without forking a `type -t`. Anything else is settled by
# zcomplete, which will not record a word that is not on PATH.
__zcomplete_is_builtin() {
    case "$1" in
        .|:|alias|bg|bind|break|builtin|caller|cd|command|compgen|complete|compopt|continue|declare|dirs|disown|echo|enable|eval|exec|exit|export|false|fc|fg|getopts|hash|help|history|jobs|kill|let|local|logout|mapfile|popd|printf|pushd|pwd|read|readarray|readonly|return|set|shift|shopt|source|suspend|test|times|trap|true|type|typeset|ulimit|umask|unalias|unset|wait) return 0 ;;
        *) return 1 ;;
    esac
}

__zcomplete_record() {
    local __zc_exit=$?
    local entry number line typed word kind fixed

    # `history 1` gives "  512  git status"; peel off the event number and use
    # it to avoid recording the same line twice when the prompt redraws.
    entry=$(HISTTIMEFORMAT='' history 1) || return $__zc_exit
    entry=${entry#"${entry%%[![:space:]]*}"}
    number=${entry%%[![:digit:]]*}
    [ -n "$number" ] || return $__zc_exit
    [ "$number" != "$__zcomplete_last_event" ] || return $__zc_exit
    __zcomplete_last_event=$number

    line=${entry#"$number"}
    line=${line#"${line%%[![:space:]]*}"}
    typed=$line

    while :; do
        # Word splitting is what we want here: the first field is the command.
        # shellcheck disable=SC2086
        set -- $line
        word=${1:-}
        case "$word" in
            *=*|sudo|doas|command|builtin|nohup|exec|env|time|nice|stdbuf)
                shift
                line="$*"
                [ -n "$line" ] || return $__zc_exit
                ;;
            *) break ;;
        esac
    done

    case "$word" in
        ''|*/*) return $__zc_exit ;;
    esac

    # bash 3.2 has no command_not_found_handle, so the correction has to happen
    # after the fact: the line already failed with 127 and we offer to run it
    # properly. This half runs in the real shell, not a fork, so a `cd` in the
    # rewritten line sticks.
    if [ "${BASH_VERSINFO[0]}" -lt 4 ] && [ "$__zc_exit" -eq 127 ] &&
        ! type "$word" >/dev/null 2>&1; then
        fixed=$(command zcomplete retry --shell bash -- "$typed")
        if [ $? -eq 0 ] && [ -n "$fixed" ]; then
            eval "$fixed"
            return $?
        fi
        return $__zc_exit
    fi

    if declare -F "$word" >/dev/null 2>&1 || alias "$word" >/dev/null 2>&1 ||
        __zcomplete_is_builtin "$word"; then
        kind=shell
    else
        kind=auto
    fi

    command zcomplete record --shell bash --kind "$kind" -- "$word"
    return $__zc_exit
}

command_not_found_handle() {
    # bash runs this in a forked child (verified: BASHPID differs from $$), so
    # dropping the handler here is child-local and stops a missing zcomplete
    # binary from recursing on every unknown word.
    unset -f command_not_found_handle

    local word=$1 target ret
    case $- in
        *i*)
            target=$(command zcomplete resolve --shell bash -- "$@")
            ret=$?
            ;;
        *) ret=1 ;;
    esac

    if [ $ret -eq 0 ] && [ -n "$target" ]; then
        shift
        if alias "$target" >/dev/null 2>&1; then
            eval -- "$(printf '%q ' "$target" "$@")"
        else
            "$target" "$@"
        fi
        return $?
    fi
    # 130 is a Ctrl-C at the prompt; the user has already said enough.
    [ $ret -eq 130 ] && return 130

    if declare -F __zcomplete_previous >/dev/null 2>&1; then
        __zcomplete_previous "$@"
        return $?
    fi
    printf '%s: command not found\n' "$word" >&2
    return 127
}

case "${PROMPT_COMMAND:-}" in
    *__zcomplete_record*) ;;
    *) PROMPT_COMMAND="__zcomplete_record${PROMPT_COMMAND:+;$PROMPT_COMMAND}" ;;
esac

complete -W 'init query stats import forget bind unbind ignore mode on off doctor export help --safe --unsafe --bypass --version' zcomplete

# Sourcing us should not hand .bashrc a non-zero status.
true
