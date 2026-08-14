#!/bin/sh
# Take zcomplete back out: the shell lines, the binary, and the database.
#
#   ./uninstall.sh
#   curl -fsSL https://raw.githubusercontent.com/omarfakih1/zcomplete/main/uninstall.sh | sh
#
# It asks before it touches anything, and answers itself with -y. Every shell
# config it edits is copied to <file>.zcomplete.bak first.

set -eu

prefix=${PREFIX:-$HOME/.local}
bindir=$prefix/bin
assume_yes=0
keep_data=0

for arg in "$@"; do
    case $arg in
        -y|--yes) assume_yes=1 ;;
        --prefix=*) prefix=${arg#--prefix=}; bindir=$prefix/bin ;;
        --keep-data) keep_data=1 ;;
        -h|--help)
            printf 'usage: uninstall.sh [-y] [--prefix=DIR] [--keep-data]\n'
            exit 0
            ;;
        *)
            printf 'uninstall.sh: unknown option: %s\n' "$arg" >&2
            exit 2
            ;;
    esac
done

# Same shape as install.sh: piped from curl, stdin is the script, so questions
# go to the terminal and there is nothing to ask without one.
confirm() {
    [ "$assume_yes" -eq 1 ] && return 0
    { exec 3</dev/tty; } 2>/dev/null || return 1
    exec 3<&-
    printf '%s [y/N] ' "$1" >/dev/tty
    read -r answer </dev/tty || return 1
    case $answer in y|Y|yes) return 0 ;; *) return 1 ;; esac
}

unwire() {
    shell=$1 file=$2
    [ -f "$file" ] || return 0
    grep -q 'zcomplete init' "$file" 2>/dev/null || return 0
    confirm "$shell: remove the zcomplete lines from $file?" || return 0
    cp "$file" "$file.zcomplete.bak"
    # install.sh appends exactly a blank line, `# zcomplete`, and the init line,
    # so exactly those three go. Read whole and marked by index rather than
    # filtered as it streams: a shell config is the one file here that is not
    # ours, and a blank line somebody else put in stays where they put it.
    awk '
        { line[NR] = $0 }
        END {
            for (i = 1; i <= NR; i++) {
                if (line[i] !~ /zcomplete init/) continue
                drop[i] = 1
                if (line[i - 1] != "# zcomplete") continue
                drop[i - 1] = 1
                if (line[i - 2] == "") drop[i - 2] = 1
            }
            for (i = 1; i <= NR; i++) if (!(i in drop)) print line[i]
        }
    ' "$file.zcomplete.bak" >"$file"
    printf '%s: removed from %s (kept %s)\n' "$shell" "$file" "$file.zcomplete.bak"
}

# Shell config first. Whatever happens to the binary after this, a shell started
# from here on is a shell with no hook in it.
unwire zsh "$HOME/.zshrc"
unwire bash "$HOME/.bashrc"
unwire fish "$HOME/.config/fish/config.fish"

data=${ZCOMPLETE_DATA_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/zcomplete}
if [ "$keep_data" -eq 1 ]; then
    printf 'kept the database at %s\n' "$data"
elif [ -d "$data" ]; then
    if confirm "delete everything zcomplete learned, at $data?"; then
        rm -rf "$data"
        printf 'deleted %s\n' "$data"
    else
        printf 'kept the database at %s\n' "$data"
    fi
fi

# Last, because until it goes the hooks in shells that are still open keep
# working. Once it is gone they report a command not found after every line,
# which is what the closing note is about.
if [ -e "$bindir/zcomplete" ]; then
    if confirm "delete $bindir/zcomplete?"; then
        rm -f "$bindir/zcomplete"
        printf 'deleted %s\n' "$bindir/zcomplete"
    fi
elif command -v zcomplete >/dev/null 2>&1; then
    printf 'not at %s; the one on your PATH is %s\n' \
        "$bindir/zcomplete" "$(command -v zcomplete)"
fi

cat <<'EOF'

done. Shells you already have open still have the hook loaded: run

  exec $SHELL

in each of them, or just open a new one.
EOF
