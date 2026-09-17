#!/usr/bin/env bash
# Install the original fzf picker as `cs-classic`. Needs fzf and python3.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
mkdir -p "$HOME/.local/bin"
install -m755 "$here/claude-session-index" "$HOME/.local/bin/claude-session-index"
if [ -d "$HOME/.config/fish" ]; then
    mkdir -p "$HOME/.config/fish/functions"
    install -m644 "$here/cs-classic.fish" "$HOME/.config/fish/functions/cs-classic.fish"
    echo "installed cs-classic"
else
    echo "fish not found; cs-classic is a fish function"
fi
