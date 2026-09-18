#!/usr/bin/env bash
# Install mnemosyne.
#
# Downloads a prebuilt static binary when there is one, and builds from source
# otherwise. Either way you end up with `mnemosyne` on PATH and an `mn`
# function in your shell.
#
#   ./install.sh                 # prebuilt if possible, else build
#   ./install.sh --build         # always build from source
#   BINDIR=~/bin ./install.sh    # somewhere else
set -euo pipefail

REPO="milescoviello/mnemosyne"
bindir="${BINDIR:-$HOME/.local/bin}"
here="$(cd "$(dirname "$0")" && pwd)"
force_build=0
[ "${1:-}" = "--build" ] && force_build=1

say() { printf '%s\n' "$*"; }
have() { command -v "$1" >/dev/null 2>&1; }

# Where the shell functions come from. Next to this script when run from a
# clone; from the downloaded tarball when run via curl, where there is no
# checkout to read them out of.
SHELLSRC="$here/shell"
KEEP=""

# Which release asset matches this machine, if any.
asset_for_platform() {
    local os arch
    case "$(uname -s)" in
        Linux) os=linux ;;
        Darwin) os=macos ;;
        *) return 1 ;;
    esac
    case "$(uname -m)" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) return 1 ;;
    esac
    printf 'mnemosyne-%s-%s.tar.gz' "$arch" "$os"
}

# coreutils calls it sha256sum; macOS calls it shasum.
sha256_of() {
    if have sha256sum; then
        sha256sum "$1" | awk '{print $1}'
    elif have shasum; then
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

fetch_prebuilt() {
    have curl || return 1
    local url tmp name
    name="$(asset_for_platform)" || return 1
    url="https://github.com/$REPO/releases/latest/download/$name"
    tmp="$(mktemp -d)"
    KEEP="$tmp"
    say "fetching the latest release for $(uname -s) $(uname -m)…"
    curl -fsSL "$url" -o "$tmp/m.tar.gz" || return 1
    if curl -fsSL "$url.sha256" -o "$tmp/m.sha256" 2>/dev/null; then
        want="$(awk '{print $1}' "$tmp/m.sha256")"
        got="$(sha256_of "$tmp/m.tar.gz")"
        if [ -n "$want" ] && [ -n "$got" ] && [ "$want" != "$got" ]; then
            say "checksum did not match — falling back to building"
            return 1
        fi
    fi
    tar -C "$tmp" -xzf "$tmp/m.tar.gz" || return 1
    mkdir -p "$bindir"
    install -m755 "$tmp/mnemosyne" "$bindir/mnemosyne"
    # the tarball carries the shell functions, so a curl install has them too
    [ -f "$tmp/mn.fish" ] && SHELLSRC="$tmp"
    return 0
}

build_from_source() {
    have cargo || {
        say "no prebuilt binary for $(uname -s) $(uname -m), and no cargo to build with."
        say "install rust from https://rustup.rs and re-run, or open an issue"
        say "asking for this platform to be added to the release build."
        exit 1
    }
    say "building…"
    cargo build --release --manifest-path "$here/Cargo.toml"
    mkdir -p "$bindir"
    install -m755 "$here/target/release/mnemosyne" "$bindir/mnemosyne"
}

if [ "$force_build" = 1 ] || ! fetch_prebuilt; then
    build_from_source
fi
say "installed $bindir/mnemosyne"

# fish autoloads functions from this directory
if [ -d "$HOME/.config/fish" ]; then
    mkdir -p "$HOME/.config/fish/functions"
    install -m644 "$SHELLSRC/mn.fish" "$HOME/.config/fish/functions/mn.fish"
    say "installed ~/.config/fish/functions/mn.fish   -> type: mn"
fi

# bash/zsh must source it, because the cd has to happen in your shell.
# Wiring it up is the install; printing homework and then announcing success
# leaves you with a command that does not exist.
mkdir -p "$HOME/.local/share/mnemosyne"
install -m644 "$SHELLSRC/mn.bash" "$HOME/.local/share/mnemosyne/mn.bash"
line='source ~/.local/share/mnemosyne/mn.bash'
wired=""
for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
    [ -f "$rc" ] || continue
    if grep -qF "$line" "$rc"; then
        wired="yes"
        continue
    fi
    printf '\n# mnemosyne: the mn function, which has to run in your shell to cd\n%s\n' \
        "$line" >> "$rc"
    say "added to $(basename "$rc"):  $line"
    wired="yes"
done

# An installed binary that is not on PATH is not installed. Warning about it
# and carrying on leaves `mn` calling a command the shell cannot find.
case ":$PATH:" in
    *":$bindir:"*) ;;
    *)
        pathline="export PATH=\"$bindir:\$PATH\""
        added=""
        for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
            [ -f "$rc" ] || continue
            grep -qF "$bindir" "$rc" && continue
            printf '\n# mnemosyne: so the binary it installed can be found\n%s\n' \
                "$pathline" >> "$rc"
            say "added to $(basename "$rc"):  $pathline"
            added="yes"
        done
        if [ -d "$HOME/.config/fish" ]; then
            mkdir -p "$HOME/.config/fish/conf.d"
            fishpath="$HOME/.config/fish/conf.d/mnemosyne-path.fish"
            if [ ! -f "$fishpath" ]; then
                printf '# mnemosyne: so the binary it installed can be found\nfish_add_path %s\n' \
                    "$bindir" > "$fishpath"
                say "added $bindir to fish's PATH"
                added="yes"
            fi
        fi
        [ -z "$added" ] && say "note: $bindir is not on your PATH and no shell config was found"
        ;;
esac

say ""
say "building the index…"
"$bindir/mnemosyne" --refresh
[ -n "$KEEP" ] && rm -rf "$KEEP"

say ""
# Be honest about whether `mn` works in the shell you are standing in.
case "${SHELL##*/}" in
    fish)
        if [ -f "$HOME/.config/fish/functions/mn.fish" ]; then
            say "done — run: mn"
        else
            say "done, but no fish config was found; see shell/mn.fish"
        fi
        ;;
    bash | zsh)
        if [ -n "$wired" ]; then
            say "done — open a new shell, or run this once to use it now:"
            say "    $line"
        else
            say "done, but nothing was wired up: no .bashrc or .zshrc found."
            say "add this to your shell config:  $line"
        fi
        ;;
    *)
        say "done. mn is a shell function; source the one for your shell:"
        say "    fish: ~/.config/fish/functions/mn.fish"
        say "    bash/zsh: $line"
        ;;
esac
