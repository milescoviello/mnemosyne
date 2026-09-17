#!/usr/bin/env bash
# Build mnemosyne and install the `mn` shell function.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
bindir="${BINDIR:-$HOME/.local/bin}"

command -v cargo >/dev/null || { echo "need a rust toolchain (cargo)"; exit 1; }

echo "building…"
cargo build --release --manifest-path "$here/Cargo.toml"

mkdir -p "$bindir"
install -m755 "$here/target/release/mnemosyne" "$bindir/mnemosyne"
echo "installed $bindir/mnemosyne"

# fish: functions are autoloaded from this directory
if [ -d "$HOME/.config/fish" ]; then
    mkdir -p "$HOME/.config/fish/functions"
    install -m644 "$here/shell/mn.fish" "$HOME/.config/fish/functions/mn.fish"
    echo "installed ~/.config/fish/functions/mn.fish   -> type: mn"
fi

# bash/zsh: must be sourced, because the cd happens in your shell
mkdir -p "$HOME/.local/share/mnemosyne"
install -m644 "$here/shell/mn.bash" "$HOME/.local/share/mnemosyne/mn.bash"
echo "installed ~/.local/share/mnemosyne/mn.bash"
echo "  for bash/zsh add:  source ~/.local/share/mnemosyne/mn.bash"

case ":$PATH:" in
    *":$bindir:"*) ;;
    *) echo "note: $bindir is not on your PATH" ;;
esac

echo
echo "building the index…"
"$bindir/mnemosyne" --refresh
echo
echo "done — run: mn"
