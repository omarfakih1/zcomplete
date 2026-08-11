#!/bin/sh
# Install zcomplete, wire it into your shell, and seed it from your history.
#
#   curl -fsSL https://raw.githubusercontent.com/omarfakih1/zcomplete/main/install.sh | sh
#
# In a checkout it builds what is there instead. Either way it asks before it
# edits a file, and answers itself with -y.

set -eu

repo=omarfakih1/zcomplete
prefix=${PREFIX:-$HOME/.local}
bindir=$prefix/bin
version=${ZCOMPLETE_VERSION:-latest}
assume_yes=0

for arg in "$@"; do
    case $arg in
        -y|--yes) assume_yes=1 ;;
        --prefix=*) prefix=${arg#--prefix=}; bindir=$prefix/bin ;;
        --version=*) version=${arg#--version=} ;;
        -h|--help)
            printf 'usage: install.sh [-y] [--prefix=DIR] [--version=vX.Y.Z]\n'
            exit 0
            ;;
        *)
            printf 'install.sh: unknown option: %s\n' "$arg" >&2
            exit 2
            ;;
    esac
done

die() { printf 'install.sh: %s\n' "$1" >&2; exit 1; }

# Piped from curl, stdin is the script itself, so every question goes to the
# terminal. Without one there is nothing to ask, and nothing gets edited.
confirm() {
    [ "$assume_yes" -eq 1 ] && return 0
    [ -r /dev/tty ] || return 1
    printf '%s [y/N] ' "$1" >/dev/tty
    read -r answer </dev/tty || return 1
    case $answer in y|Y|yes) return 0 ;; *) return 1 ;; esac
}

target() {
    machine=$(uname -m)
    case $(uname -s) in
        Darwin)
            case $machine in
                arm64|aarch64) printf 'aarch64-apple-darwin' ;;
                x86_64) printf 'x86_64-apple-darwin' ;;
                *) return 1 ;;
            esac
            ;;
        Linux)
            # Static musl, so one binary runs on whatever libc the distro has.
            case $machine in
                x86_64) printf 'x86_64-unknown-linux-musl' ;;
                aarch64|arm64) printf 'aarch64-unknown-linux-musl' ;;
                *) return 1 ;;
            esac
            ;;
        *) return 1 ;;
    esac
}

checksum() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        printf 'skip'
    fi
}

download() {
    triple=$1 into=$2
    command -v curl >/dev/null 2>&1 || return 1
    case $version in
        latest) base="https://github.com/$repo/releases/latest/download" ;;
        *) base="https://github.com/$repo/releases/download/$version" ;;
    esac
    file=zcomplete-$triple.tar.gz
    # No -S: a 404 here just means no release for this platform, and the
    # message for that is further down and better than curl's.
    curl -fsL "$base/$file" -o "$into/$file" || return 1

    # A published checksum or nothing: a tarball that arrived over the network
    # is the one thing here that nobody in this script wrote.
    want=$(curl -fsL "$base/$file.sha256" 2>/dev/null | cut -d' ' -f1) || want=
    [ -n "$want" ] || die "no checksum published for $file, refusing to install it"
    got=$(checksum "$into/$file")
    [ "$got" = skip ] && die 'no shasum or sha256sum to verify the download with'
    [ "$got" = "$want" ] || die "checksum mismatch for $file"

    tar -xzf "$into/$file" -C "$into" || return 1
    [ -f "$into/zcomplete" ] || return 1
}

here=$(cd "$(dirname "$0")" 2>/dev/null && pwd || printf '.')
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Piped from curl, `dirname $0` is the working directory, which is somebody
# else's Rust project often enough to be worth checking whose Cargo.toml it is.
if grep -q '^name = "zcomplete"' "$here/Cargo.toml" 2>/dev/null &&
    command -v cargo >/dev/null 2>&1; then
    printf 'building\n'
    ( cd "$here" && cargo build --release --quiet )
    cp "$here/target/release/zcomplete" "$work/zcomplete"
elif triple=$(target) && download "$triple" "$work"; then
    printf 'downloaded zcomplete %s for %s\n' "$version" "$triple"
elif command -v cargo >/dev/null 2>&1; then
    printf 'no binary for this platform, building from source\n'
    cargo install --quiet --git "https://github.com/$repo" --root "$work/cargo" zcomplete
    cp "$work/cargo/bin/zcomplete" "$work/zcomplete"
else
    die 'no release for this platform and no cargo to build one (https://rustup.rs)'
fi

mkdir -p "$bindir"
# Written beside the target and renamed over it. Copying onto a running binary
# is ETXTBSY on Linux, and reinstalling while a shell hook is mid-command is the
# normal case rather than a rare one.
cp "$work/zcomplete" "$bindir/.zcomplete.new"
chmod 755 "$bindir/.zcomplete.new"
mv -f "$bindir/.zcomplete.new" "$bindir/zcomplete"
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
# Seeding matters more than it looks: subcommand correction has nothing to go on
# until zcomplete has seen `git status` a few times, and history already has.
if confirm 'seed the database from your shell history?'; then
    "$bindir/zcomplete" import
fi

cat <<'EOF'

open a new shell, then try a typo.

  zcomplete safe | unsafe | bypass   how much say you get before a correction
  zcomplete doctor                   if anything looks off
EOF
