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

fetch_prebuilt() {
    [ "$(uname -s)" = "Linux" ] && [ "$(uname -m)" = "x86_64" ] || return 1
    have curl || return 1
    local url tmp
    url="https://github.com/$REPO/releases/latest/download/mnemosyne-x86_64-linux.tar.gz"
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    say "fetching the latest release…"
    curl -fsSL "$url" -o "$tmp/m.tar.gz" || return 1
    if curl -fsSL "$url.sha256" -o "$tmp/m.sha256" 2>/dev/null && have sha256sum; then
        ( cd "$tmp" && sed "s|  .*|  m.tar.gz|" m.sha256 | sha256sum -c - >/dev/null ) \
            || { say "checksum did not match — falling back to building"; return 1; }
    fi
    tar -C "$tmp" -xzf "$tmp/m.tar.gz" || return 1
    mkdir -p "$bindir"
    install -m755 "$tmp/mnemosyne" "$bindir/mnemosyne"
    [ -f "$tmp/mn.fish" ] && cp "$tmp/mn.fish" "$tmp/mn.bash" "$here/shell/" 2>/dev/null || true
    return 0
}

build_from_source() {
    have cargo || {
        say "no prebuilt binary for this platform and no cargo to build with."
        say "install rust from https://rustup.rs and re-run."
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
    install -m644 "$here/shell/mn.fish" "$HOME/.config/fish/functions/mn.fish"
    say "installed ~/.config/fish/functions/mn.fish   -> type: mn"
fi

# bash/zsh must source it, because the cd has to happen in your shell
mkdir -p "$HOME/.local/share/mnemosyne"
install -m644 "$here/shell/mn.bash" "$HOME/.local/share/mnemosyne/mn.bash"
line='source ~/.local/share/mnemosyne/mn.bash'
for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
    [ -f "$rc" ] || continue
    if ! grep -qF "$line" "$rc"; then
        say "add to $(basename "$rc"):  $line"
    fi
done

case ":$PATH:" in
    *":$bindir:"*) ;;
    *) say "note: $bindir is not on your PATH" ;;
esac

say ""
say "building the index…"
"$bindir/mnemosyne" --refresh
say ""
say "done — run: mn"
