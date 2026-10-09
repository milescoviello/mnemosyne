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

# Unpack a .tar.gz into a directory. AlmaLinux and RHEL minimal images come
# without tar, and every install there ended in "tar: command not found".
# dnf brings python with it, and python reads tarballs.
unpack() {
    if have tar; then
        tar -C "$2" -xzf "$1"
    elif have python3; then
        python3 -c 'import sys, tarfile
f = {"filter": "data"} if hasattr(tarfile, "data_filter") else {}
with tarfile.open(sys.argv[1]) as t:
    t.extractall(sys.argv[2], **f)' "$1" "$2"
    else
        return 1
    fi
}

# Why there is no prebuilt binary, for whoever has to say so. Anything going
# wrong in here -- no tar, a download cut short -- used to come out as "no
# prebuilt binary to install", which sent you off to clone and build for a
# missing tar.
why=""

fetch_prebuilt() {
    have curl || { why="there is no curl to download it with"; return 1; }
    local url tmp name tag
    name="$(asset_for_platform)" || { why="there is no prebuilt binary for $(uname -s) $(uname -m)"; return 1; }
    # Which release is the latest, asked once, and both files from that one.
    # Each asked of releases/latest, a release published in between answered
    # the second: "its checksum did not match", on Fedora 43 as v0.6.1 went
    # out. releases/latest redirects to its tag's page, no API call needed.
    tag="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" 2>/dev/null)" || tag=""
    tag="${tag##*/releases/tag/}"
    case "$tag" in
        v[0-9]*) url="https://github.com/$REPO/releases/download/$tag/$name" ;;
        *) tag=""; url="https://github.com/$REPO/releases/latest/download/$name" ;;
    esac
    tmp="$(mktemp -d)" || { why="could not make a temporary directory to download into"; return 1; }
    KEEP="$tmp"
    say "fetching ${tag:-the latest release} for $(uname -s) $(uname -m)…"
    curl -fsSL "$url" -o "$tmp/m.tar.gz" || { why="could not download $url"; return 1; }
    # Verified, or not installed. A checksum that was not published, or no
    # tool to check one with, used to skip the check and install whatever
    # arrived -- where the updater has always refused both.
    if ! curl -fsSL "$url.sha256" -o "$tmp/m.sha256" 2>/dev/null; then
        why="no checksum was published for it, so it was not installed"
        return 1
    fi
    want="$(awk '{print $1}' "$tmp/m.sha256")"
    got="$(sha256_of "$tmp/m.tar.gz")"
    if [ -z "$got" ]; then
        why="there is no sha256sum or shasum to check it with, so it was not installed"
        return 1
    fi
    if [ -z "$want" ] || [ "$want" != "$got" ]; then
        why="its checksum did not match, so it was not installed"
        return 1
    fi
    if ! have tar && ! have python3; then
        why="there is no tar to unpack it with (install tar, or python3)"
        return 1
    fi
    unpack "$tmp/m.tar.gz" "$tmp" || { why="could not unpack it"; return 1; }
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

# Is version $1 at least $2? Dotted numbers; anything after them ignored.
version_ge() {
    local a b i
    IFS=. read -r -a a <<< "${1%%[!0-9.]*}"
    IFS=. read -r -a b <<< "${2%%[!0-9.]*}"
    for i in 0 1 2; do
        [ "${a[i]:-0}" -gt "${b[i]:-0}" ] && return 0
        [ "${a[i]:-0}" -lt "${b[i]:-0}" ] && return 1
    done
    return 0
}

build_from_source() {
    # Only a checkout of this project. Piped from curl, `here` is the
    # current directory, and whatever Cargo.toml was in it got built --
    # build scripts and all.
    [ -n "$why" ] && say "$why."
    is_checkout || {
        say "building it from source${why:+ instead} needs a clone:"
        say "  git clone https://github.com/$REPO && cd mnemosyne && ./install.sh --build"
        exit 1
    }
    have cargo || {
        say "building it from source${why:+ instead} needs cargo: install rust from"
        say "https://rustup.rs and re-run, or open an issue asking for this"
        say "platform to be added to the release build."
        exit 1
    }
    # The oldest Rust it builds with, from Cargo.toml. A distribution's own
    # can be older -- Debian 12's 1.63, Ubuntu 24.04's 1.75 -- and cargo then
    # stopped at "failed to parse lock file" or a list of crates, and said
    # nothing about what to do.
    local need found
    need="$(sed -n 's/^rust-version *= *"\(.*\)"/\1/p' "$here/Cargo.toml" | head -1)"
    found="$(rustc -V 2>/dev/null | awk '{print $2}')"
    if [ -n "$need" ] && [ -n "$found" ] && ! version_ge "$found" "$need"; then
        say "building it needs Rust $need or newer, and this rustc is $found:"
        say "install a current one from https://rustup.rs and re-run."
        exit 1
    fi
    say "building…"
    cargo build --release --manifest-path "$here/Cargo.toml"
    mkdir -p "$bindir"
    install -m755 "$here/target/release/mnemosyne" "$bindir/mnemosyne"
}

if [ "$force_build" = 1 ] || ! fetch_prebuilt; then
    build_from_source
fi
say "installed $bindir/mnemosyne"

# Where each shell reads its config. zsh reads $ZDOTDIR/.zshrc when that is
# set, and fish $XDG_CONFIG_HOME/fish: ~/.zshrc and ~/.config/fish were
# wired up regardless, the install said "done", and the next shell had no mn.
zshrc="${ZDOTDIR:-$HOME}/.zshrc"
fishdir="${XDG_CONFIG_HOME:-$HOME/.config}/fish"
# A path as it is shown: under ~ when it is in the home directory.
tilde() { case "$1" in "$HOME"/*) printf '~/%s' "${1#"$HOME"/}" ;; *) printf '%s' "$1" ;; esac; }

# A shell nobody has configured yet has nothing to add to. A fresh Mac runs
# zsh and has no ~/.zshrc; fish that has never been started has no
# ~/.config/fish. Only touching what already existed left those machines
# with nothing wired up at all, so make it for the shell you actually use.
case "${SHELL##*/}" in
    zsh) [ -f "$zshrc" ] || { mkdir -p "${zshrc%/*}" && : > "$zshrc"; } 2>/dev/null || true ;;
    bash) [ -f "$HOME/.bashrc" ] || { : > "$HOME/.bashrc"; } 2>/dev/null || true ;;
    fish) mkdir -p "$fishdir" 2>/dev/null || true ;;
esac

# Config that cannot be written to, and what to add to it instead.
# home-manager (NixOS) makes these links into its read-only store, and the
# install died at the first one on a bare "Permission denied": the binary
# in place, no index built, nothing said about what to add by hand.
readonly_rc=""
# $1 the file, $2 the text to append. Fails, quietly, where it may not.
append() { { printf '%s' "$2" >> "$1"; } 2>/dev/null; }

mkdir -p "$HOME/.local/share/mnemosyne"
# fish autoloads functions from this directory. Where it may not be
# written, a copy beside mn.bash, for your own config to source.
if [ -d "$fishdir" ]; then
    if { mkdir -p "$fishdir/functions" && install -m644 "$SHELLSRC/mn.fish" "$fishdir/functions/mn.fish"; } 2>/dev/null; then
        say "installed $(tilde "$fishdir")/functions/mn.fish   -> type: mn"
    else
        readonly_rc="$readonly_rc $(tilde "$fishdir")/functions"
        install -m644 "$SHELLSRC/mn.fish" "$HOME/.local/share/mnemosyne/mn.fish"
    fi
fi

# bash/zsh must source it, because the cd has to happen in your shell.
# Wiring it up is the install; printing homework and then announcing success
# leaves you with a command that does not exist.
install -m644 "$SHELLSRC/mn.bash" "$HOME/.local/share/mnemosyne/mn.bash"
line='source ~/.local/share/mnemosyne/mn.bash'
wired=""
for rc in "$HOME/.bashrc" "$zshrc"; do
    [ -f "$rc" ] || continue
    if grep -qF "$line" "$rc"; then
        wired="yes"
        continue
    fi
    if append "$rc" "$(printf '\n# mnemosyne: the mn function, which has to run in your shell to cd\n%s\n' "$line")
"; then
        say "added to $(tilde "$rc"):  $line"
        wired="yes"
    else
        readonly_rc="$readonly_rc $(tilde "$rc")"
    fi
done

# A login bash -- an SSH session, a Mac's Terminal -- reads a profile and not
# .bashrc, unless the profile sources it. Most distros give a new user one
# that does; Alpine and NixOS give them none at all, and `mn` was missing
# from every SSH login there. Only when there is none: a profile that is
# already there is somebody's own, and bash reads just the first it finds.
if [ "${SHELL##*/}" = bash ] && [ -f "$HOME/.bashrc" ] && grep -qF "$line" "$HOME/.bashrc" \
    && [ ! -e "$HOME/.bash_profile" ] && [ ! -e "$HOME/.bash_login" ] && [ ! -e "$HOME/.profile" ]; then
    if append "$HOME/.profile" "$(printf '%s\n' '# mnemosyne: a login bash reads this file and not ~/.bashrc, so read that too' \
        'if [ -n "$BASH_VERSION" ] && [ -f "$HOME/.bashrc" ]; then . "$HOME/.bashrc"; fi')
"; then
        say "made ~/.profile, so a login bash reads ~/.bashrc too"
    fi
fi

# A path as fish reads it: in single quotes, where only \\ and \' mean
# anything. Unquoted, a folder with a space in its name went to
# fish_add_path as two folders, neither of them the one it meant.
fish_quote() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/[\\\\']/\\\\&/g")"
}

# An installed binary that is not on PATH is not installed. Warning about it
# and carrying on leaves `mn` calling a command the shell cannot find.
path_added="" on_path=yes
case ":$PATH:" in
    *":$bindir:"*) ;;
    *)
        on_path=""
        pathline="export PATH=\"$bindir:\$PATH\""
        added=""
        for rc in "$HOME/.bashrc" "$zshrc"; do
            [ -f "$rc" ] || continue
            grep -qF "$bindir" "$rc" && continue
            if append "$rc" "$(printf '\n# mnemosyne: so the binary it installed can be found\n%s\n' "$pathline")
"; then
                say "added to $(tilde "$rc"):  $pathline"
                added="yes"
            fi
        done
        if [ -d "$fishdir" ]; then
            fishpath="$fishdir/conf.d/mnemosyne-path.fish"
            if [ ! -f "$fishpath" ] && { mkdir -p "$fishdir/conf.d"; } 2>/dev/null \
                && append "$fishpath" "$(printf '# mnemosyne: so the binary it installed can be found\nfish_add_path %s\n' "$(fish_quote "$bindir")")
"; then
                say "added $bindir to fish's PATH"
                added="yes"
            fi
        fi
        [ -z "$added" ] && [ -z "$readonly_rc" ] && say "note: $bindir is not on your PATH and no shell config was found"
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
if [ -n "$readonly_rc" ]; then
    say "done, but this could not be written to (read-only):$readonly_rc"
    say "add these to wherever it comes from (home-manager, say):"
    case "${SHELL##*/}" in
        fish)
            [ -n "$on_path" ] || say "    fish_add_path $(fish_quote "$bindir")"
            say "    source ~/.local/share/mnemosyne/mn.fish"
            ;;
        *)
            [ -n "$on_path" ] || say "    export PATH=\"$bindir:\$PATH\""
            say "    $line"
            ;;
    esac
    exit 0
fi
case "${SHELL##*/}" in
    fish)
        if [ ! -f "$fishdir/functions/mn.fish" ]; then
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
        say "    fish: $(tilde "$fishdir")/functions/mn.fish"
        say "    bash/zsh: $line"
        ;;
esac
