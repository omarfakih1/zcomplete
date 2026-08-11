# zcomplete: bash integration, loaded by  eval "$(zcomplete init bash)"
# Interception needs bash 4.0. On the 3.2 macOS ships, the recording half
# still works and the fix is offered at the next prompt instead.
if declare -F command_not_found_handle >/dev/null 2>&1 &&
    ! declare -F __zcomplete_previous >/dev/null 2>&1; then
    case "$(declare -f command_not_found_handle)" in
        *zcomplete*) ;;
        *) eval "__zcomplete_previous() $(declare -f command_not_found_handle | tail -n +2)" ;;
    esac
fi

__zcomplete_is_builtin() {
    case "$1" in
        .|:|alias|bg|bind|break|builtin|caller|cd|command|compgen|complete|compopt|continue|declare|dirs|disown|echo|enable|eval|exec|exit|export|false|fc|fg|getopts|hash|help|history|jobs|kill|let|local|logout|mapfile|popd|printf|pushd|pwd|read|readarray|readonly|return|set|shift|shopt|source|suspend|test|times|trap|true|type|typeset|ulimit|umask|unalias|unset|wait) return 0 ;;
        *) return 1 ;;
    esac
}

__zcomplete_record() {
    local __zc_exit=$?
    local entry number line typed word kind fixed

    # `history 1` gives "  512  git status". The event number is what stops a
    # prompt redraw from recording the same line twice.
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

    # Two ways to arrive here with the line still unrun: bash 3.2 has no
    # interception at all, and on bash 4 the handler hands back an alias or a
    # function rather than running it in a fork that cannot keep what it does.
    # Either way this is the real shell, so a `cd` in the rewritten line sticks.
    if { [ "$__zc_exit" -eq 5 ] ||
        { [ "${BASH_VERSINFO[0]}" -lt 4 ] && [ "$__zc_exit" -eq 127 ]; }; } &&
        ! type "$word" >/dev/null 2>&1; then
        fixed=$(command zcomplete retry --shell bash --only "$word" -- "$typed")
        if [ $? -eq 0 ] && [ -n "$fixed" ]; then
            eval "$fixed"
            return $?
        fi
        # Nothing was printed when the handler stepped aside.
        [ "$__zc_exit" -eq 5 ] && printf '%s: command not found\n' "$word" >&2
        return 127
    fi

    if declare -F "$word" >/dev/null 2>&1 || alias "$word" >/dev/null 2>&1 ||
        __zcomplete_is_builtin "$word"; then
        kind=shell
    else
        kind=auto
    fi

    fixed=$(command zcomplete record --shell bash --kind "$kind" --status "$__zc_exit" -- "$typed")
    if [ -n "$fixed" ]; then
        eval "$fixed"
        return $?
    fi
    return $__zc_exit
}

command_not_found_handle() {
    # Child-local: bash runs this in a forked child, so dropping the handler
    # stops a missing zcomplete binary recursing on every unknown word.
    unset -f command_not_found_handle

    local word=$1 reply target second ret
    case $- in
        *i*)
            reply=$(command zcomplete resolve --shell bash --subshell -- "$@")
            ret=$?
            target=${reply%%$'\n'*}
            case $reply in *$'\n'*) second=${reply#*$'\n'} ;; *) second= ;; esac
            ;;
        *) ret=1 ;;
    esac

    if [ $ret -eq 0 ] && [ -n "$target" ] && type "$target" >/dev/null 2>&1; then
        shift
        case $second in 'verb '*) set -- "${second#verb }" "${@:2}" ;; esac
        if alias "$target" >/dev/null 2>&1; then
            eval -- "$(printf '%q ' "$target" "$@")"
        else
            "$target" "$@"
        fi
        return $?
    fi
    # Left for the next prompt, which is the shell and not a fork of it.
    [ $ret -eq 5 ] && return 5
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

complete -W 'init query stats import forget bind unbind ignore mode safe unsafe bypass on off doctor help --version' zcomplete

true
