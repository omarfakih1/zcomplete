#!/bin/sh
# Build zcomplete, wire it into your shell, and seed it from your history.

set -eu

prefix=${PREFIX:-$HOME/.local}
bindir=$prefix/bin
assume_yes=0

for arg in "$@"; do
    case $arg in
        -y|--yes) assume_yes=1 ;;
        --prefix=*) prefix=${arg#--prefix=}; bindir=$prefix/bin ;;
        -h|--help)
            printf 'usage: install.sh [-y] [--prefix=DIR]\n'
            exit 0
            ;;
        *)
            printf 'install.sh: unknown option: %s\n' "$arg" >&2
            exit 2
            ;;
    esac
done

confirm() {
    [ "$assume_yes" -eq 1 ] && return 0
    printf '%s [y/N] ' "$1"
    read -r answer </dev/tty || return 1
    case $answer in y|Y|yes) return 0 ;; *) return 1 ;; esac
}

command -v cargo >/dev/null 2>&1 || {
    printf 'install.sh: cargo is required (https://rustup.rs)\n' >&2
    exit 1
}

cd "$(dirname "$0")"
printf 'building\n'
cargo build --release --quiet

mkdir -p "$bindir"
cp target/release/zcomplete "$bindir/zcomplete"
printf 'installed %s\n' "$bindir/zcomplete"

case ":$PATH:" in
    *":$bindir:"*) ;;
    *) printf '\n%s is not on your PATH; add it before the line below.\n' "$bindir" ;;
esac

wire() {
    shell=$1 file=$2 line=$3
    command -v "$shell" >/dev/null 2>&1 || return 0
    if [ -f "$file" ] && grep -q 'zcomplete init' "$file"; then
        printf '%s: already set up\n' "$shell"
        return 0
    fi
    confirm "$shell: add \`$line\` to $file?" || return 0
    mkdir -p "$(dirname "$file")"
    printf '\n# zcomplete\n%s\n' "$line" >>"$file"
    printf '%s: added to %s\n' "$shell" "$file"
}

printf '\n'
wire zsh "$HOME/.zshrc" 'eval "$(zcomplete init zsh)"'
wire bash "$HOME/.bashrc" 'eval "$(zcomplete init bash)"'
wire fish "$HOME/.config/fish/config.fish" 'zcomplete init fish | source'

printf '\n'
if confirm 'seed the database from your shell history?'; then
    "$bindir/zcomplete" import
fi

printf '\nopen a new shell, then try a typo. `zcomplete doctor` if anything looks off.\n'
