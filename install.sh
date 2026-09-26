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
# Where this script is: a checkout of mnemosyne when run as ./install.sh.
# Piped from curl there is no script file -- $0 is "bash" -- and this is
# just wherever you happened to be standing.
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
# However it ends. Removed only at the very bottom, every way out before
# that left the download behind.
trap '[ -n "$KEEP" ] && rm -rf "$KEEP"' EXIT

# Is `here` a checkout of this project, and so something to build?
is_checkout() {
    [ -f "$here/Cargo.toml" ] && grep -q '^name = "mnemosyne"' "$here/Cargo.toml"
}

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
    # Verified, or not installed. A checksum that was not published, or no
    # tool to check one with, used to skip the check and install whatever
    # arrived -- where the updater has always refused both.
    if ! curl -fsSL "$url.sha256" -o "$tmp/m.sha256" 2>/dev/null; then
        say "no checksum was published for it — not installing it"
        return 1
    fi
    want="$(awk '{print $1}' "$tmp/m.sha256")"
    got="$(sha256_of "$tmp/m.tar.gz")"
    if [ -z "$got" ]; then
        say "there is no sha256sum or shasum to check it with — not installing it"
        return 1
    fi
    if [ -z "$want" ] || [ "$want" != "$got" ]; then
        say "checksum did not match — not installing it"
        return 1
    fi
    tar -C "$tmp" -xzf "$tmp/m.tar.gz" || return 1
    # From here a failure is the destination's, which building would not
    # get round. Called as the condition of an `if`, this function runs
    # without `set -e`: a failed install went unnoticed, and the script
    # said "installed" and wired up a binary that was not there.
    if ! { mkdir -p "$bindir" && install -m755 "$tmp/mnemosyne" "$bindir/mnemosyne"; }; then
        say "could not write $bindir/mnemosyne"
        exit 1
    fi
    # the tarball carries the shell functions, so a curl install has them too
    [ -f "$tmp/mn.fish" ] && SHELLSRC="$tmp"
    return 0
}

build_from_source() {
    # Only a checkout of this project. Piped from curl, `here` is the
    # current directory, and whatever Cargo.toml was in it got built --
    # build scripts and all.
    is_checkout || {
        say "no prebuilt binary to install, and building one needs a clone:"
        say "  git clone https://github.com/$REPO && cd mnemosyne && ./install.sh --build"
        exit 1
    }
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

# A shell nobody has configured yet has nothing to add to. A fresh Mac runs
# zsh and has no ~/.zshrc; fish that has never been started has no
# ~/.config/fish. Only touching what already existed left those machines
# with nothing wired up at all, so make it for the shell you actually use.
case "${SHELL##*/}" in
    zsh) [ -f "$HOME/.zshrc" ] || : > "$HOME/.zshrc" ;;
    bash) [ -f "$HOME/.bashrc" ] || : > "$HOME/.bashrc" ;;
    fish) mkdir -p "$HOME/.config/fish" ;;
esac

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

# A path as fish reads it: in single quotes, where only \\ and \' mean
# anything. Unquoted, a folder with a space in its name went to
# fish_add_path as two folders, neither of them the one it meant.
fish_quote() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/[\\\\']/\\\\&/g")"
}

# An installed binary that is not on PATH is not installed. Warning about it
# and carrying on leaves `mn` calling a command the shell cannot find.
path_added=""
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
                    "$(fish_quote "$bindir")" > "$fishpath"
                say "added $bindir to fish's PATH"
                added="yes"
            fi
        fi
        [ -z "$added" ] && say "note: $bindir is not on your PATH and no shell config was found"
        path_added="$added"
        ;;
esac

say ""
say "building the index…"
"$bindir/mnemosyne" --refresh
[ -n "$KEEP" ] && rm -rf "$KEEP"

say ""
# Be honest about whether `mn` works in the shell you are standing in. It
# read its config before any of this happened, so a PATH added above is not
# in it yet: "run this once" has to include that, or following it exactly
# ends in "command not found".
case "${SHELL##*/}" in
    fish)
        if [ ! -f "$HOME/.config/fish/functions/mn.fish" ]; then
            say "done, but the fish function could not be installed"
        elif [ -n "$path_added" ]; then
            say "done — open a new shell, or run this once to use it now:"
            say "    fish_add_path $(fish_quote "$bindir")"
        else
            say "done — run: mn"
        fi
        ;;
    bash | zsh)
        if [ -n "$wired" ]; then
            say "done — open a new shell, or run this once to use it now:"
            [ -n "$path_added" ] && say "    export PATH=\"$bindir:\$PATH\""
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
