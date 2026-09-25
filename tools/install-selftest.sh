#!/usr/bin/env bash
# Run install.sh against empty home directories and check that `mn` works
# afterwards, in a new shell, for each shell it claims to set up.
#
#   tools/install-selftest.sh [path-to-binary]
#
# The installer is the first thing anyone runs and nothing else tested it.
# On a fresh Mac -- zsh by default, no ~/.zshrc, ~/.local/bin not on PATH --
# it wired up nothing and then gave advice that, followed exactly, left `mn`
# saying "command not found".
#
# Nothing is downloaded: a stub curl hands over a tarball packaged from the
# local build, the same way the release workflow packages it. The real HOME
# is never touched.
set -u

cd "$(dirname "$0")/.." || exit 1
root=$(pwd)
binary="${1:-$root/target/release/mnemosyne}"
[ -x "$binary" ] || { echo "no binary at $binary — cargo build --release first"; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fails=0
checks=0
skips=0
ok() { checks=$((checks + 1)); printf '  ok   %s\n' "$1"; }
bad() {
    checks=$((checks + 1)); fails=$((fails + 1))
    printf '  FAIL %s\n' "$1"; printf '       %s\n' "$2"
}
skip() { skips=$((skips + 1)); printf '  skip %s (%s)\n' "$1" "$2"; }
has() { case "$3" in *"$2"*) ok "$1" ;; *) bad "$1" "expected to find: $2" ;; esac; }

# ---- what the release would have served --------------------------------
mkdir -p "$tmp/dist" "$tmp/stub"
cp "$binary" "$tmp/dist/mnemosyne"
cp shell/mn.fish shell/mn.bash README.md LICENSE "$tmp/dist/"
tar -C "$tmp/dist" -czf "$tmp/asset.tar.gz" .
# coreutils calls it sha256sum; macOS calls it shasum
if command -v sha256sum >/dev/null 2>&1; then sum() { sha256sum "$1"; }; else sum() { shasum -a 256 "$1"; }; fi
sum "$tmp/asset.tar.gz" | awk '{print $1 "  asset.tar.gz"}' > "$tmp/asset.sha256"

# curl, as the installer calls it: `curl -fsSL <url> -o <file>`
cat > "$tmp/stub/curl" <<EOF
#!/bin/sh
url= out=
while [ \$# -gt 0 ]; do
    case "\$1" in
        -o) out=\$2; shift ;;
        http*) url=\$1 ;;
    esac
    shift
done
# MN_STUB_NO_SUM and MN_STUB_NO_ASSET stand for a release missing one or
# the other.
case "\$url" in
    *.tar.gz) [ -n "\${MN_STUB_NO_ASSET:-}" ] && exit 22; cp "$tmp/asset.tar.gz" "\$out" ;;
    *.tar.gz.sha256) [ -n "\${MN_STUB_NO_SUM:-}" ] && exit 22; cp "$tmp/asset.sha256" "\$out" ;;
    *) exit 22 ;;
esac
EOF
chmod +x "$tmp/stub/curl"
# cargo, so a build that should not happen shows if it does
cat > "$tmp/stub/cargo" <<EOF
#!/bin/sh
echo "cargo \$*" >> "$tmp/cargo.log"
exit 1
EOF
chmod +x "$tmp/stub/cargo"

# A minimal PATH, like a fresh machine's: no ~/.local/bin on it.
base_path="$tmp/stub:/usr/local/bin:/usr/bin:/bin"
# the shells themselves, wherever they live on this machine
for s in zsh fish; do
    p=$(command -v "$s" 2>/dev/null) && ln -sf "$p" "$tmp/stub/$s"
done

# $1 name, $2 the login shell to claim; prints what the installer said
install_into() {
    local home="$tmp/home-$1"
    mkdir -p "$home"
    env -i HOME="$home" SHELL="/bin/$2" PATH="$base_path" TERM=dumb \
        bash "$root/install.sh" 2>&1
}

# ---- zsh on a fresh Mac: no rc file at all ----------------------------
printf '\nzsh, never configured\n'
if command -v zsh >/dev/null 2>&1; then
    said=$(install_into zsh zsh)
    home="$tmp/home-zsh"
    if [ -f "$home/.zshrc" ]; then ok "a ~/.zshrc is made for it"; else bad "a ~/.zshrc is made for it" "none was"; fi
    works=$(env -i HOME="$home" PATH="$base_path" TERM=dumb zsh -c 'source ~/.zshrc; mn -V' 2>&1)
    has "zsh: mn works in a new shell" "mnemosyne " "$works"
    now=$(printf '%s\n' "$said" | sed -n '/use it now/,$p' | tail -n +2 | sed 's/^ *//')
    works=$(env -i HOME="$home" PATH="$base_path" TERM=dumb zsh -c "$now
mn -V" 2>&1)
    has "zsh: and in this one, doing what it said" "mnemosyne " "$works"

    # a second run adds nothing more
    install_into zsh zsh >/dev/null
    n=$(grep -c 'mnemosyne/mn.bash' "$home/.zshrc")
    [ "$n" = 1 ] && ok "zsh: running it twice wires it once" || bad "zsh: running it twice wires it once" "$n source lines"
    n=$(grep -c 'export PATH=' "$home/.zshrc")
    [ "$n" = 1 ] && ok "zsh: and adds one PATH line" || bad "zsh: and adds one PATH line" "$n PATH lines"
else
    skip "zsh" "zsh is not installed"
fi

# ---- bash, with an ordinary existing .bashrc --------------------------
printf '\nbash, with a .bashrc\n'
home="$tmp/home-bash"
mkdir -p "$home"
printf '# an existing config\nalias ll="ls -l"\n' > "$home/.bashrc"
said=$(install_into bash bash)
has "bash: what was there is kept" 'alias ll="ls -l"' "$(cat "$home/.bashrc")"
works=$(env -i HOME="$home" PATH="$base_path" TERM=dumb bash -c 'source ~/.bashrc; mn -V' 2>&1)
has "bash: mn works in a new shell" "mnemosyne " "$works"
now=$(printf '%s\n' "$said" | sed -n '/use it now/,$p' | tail -n +2 | sed 's/^ *//')
works=$(env -i HOME="$home" PATH="$base_path" TERM=dumb bash -c "$now
mn -V" 2>&1)
has "bash: and in this one, doing what it said" "mnemosyne " "$works"

# ---- fish, before fish has ever been started --------------------------
printf '\nfish, never started\n'
if command -v fish >/dev/null 2>&1; then
    said=$(install_into fish fish)
    home="$tmp/home-fish"
    if [ -f "$home/.config/fish/functions/mn.fish" ]; then
        ok "fish: the function is installed"
    else
        bad "fish: the function is installed" "no ~/.config/fish/functions/mn.fish"
    fi
    works=$(env -i HOME="$home" PATH="$base_path" TERM=dumb fish -c 'mn -V' 2>&1)
    has "fish: mn works in a new shell" "mnemosyne " "$works"
    now=$(printf '%s\n' "$said" | sed -n '/use it now/,$p' | tail -n +2 | sed 's/^ *//')
    # a shell that was already running: config was read before the install
    works=$(env -i HOME="$home" PATH="$base_path" TERM=dumb fish --no-config -c \
        "source $home/.config/fish/functions/mn.fish; $now
mn -V" 2>&1)
    has "fish: and in this one, doing what it said" "mnemosyne " "$works"
else
    skip "fish" "fish is not installed"
fi

# ---- a release it cannot verify ---------------------------------------
printf '\nwhat it must refuse\n'
home="$tmp/home-nosum"
mkdir -p "$home"
said=$(env -i HOME="$home" SHELL=/bin/bash PATH="$base_path" TERM=dumb MN_STUB_NO_SUM=1 \
    bash "$root/install.sh" 2>&1)
code=$?
if [ "$code" -ne 0 ] && [ ! -e "$home/.local/bin/mnemosyne" ]; then
    ok "a release with no checksum is not installed"
else
    bad "a release with no checksum is not installed" "exit $code: $said"
fi

# ---- somewhere it cannot write ----------------------------------------
home="$tmp/home-ro"
mkdir -p "$home/ro"
chmod 555 "$home/ro"
said=$(env -i HOME="$home" SHELL=/bin/bash PATH="$base_path" TERM=dumb BINDIR="$home/ro/bin" \
    bash "$root/install.sh" 2>&1)
code=$?
if [ "$code" -ne 0 ]; then ok "a BINDIR it cannot write fails"; else bad "a BINDIR it cannot write fails" "exit 0"; fi
case "$said" in
    *installed*) bad "and it does not say it installed anything" "$said" ;;
    *) ok "and it does not say it installed anything" ;;
esac
[ -e "$home/.bashrc" ] && bad "nor wires up a binary that is not there" "$(cat "$home/.bashrc")" \
    || ok "nor wires up a binary that is not there"
chmod 755 "$home/ro"

# ---- piped from curl, standing in some other project ------------------
home="$tmp/home-pipe"
mkdir -p "$home/elsewhere"
printf '[package]\nname = "someone-else"\n' > "$home/elsewhere/Cargo.toml"
: > "$tmp/cargo.log"
said=$(cd "$home/elsewhere" && env -i HOME="$home" SHELL=/bin/bash PATH="$base_path" TERM=dumb \
    MN_STUB_NO_ASSET=1 bash < "$root/install.sh" 2>&1)
if [ -s "$tmp/cargo.log" ]; then
    bad "piped in, it never builds the project it was run from" "$(cat "$tmp/cargo.log")"
else
    ok "piped in, it never builds the project it was run from"
fi
has "and says a clone is what it needs" "git clone" "$said"

printf '\n%d checks, %d failed, %d skipped\n' "$checks" "$fails" "$skips"
[ "$fails" -eq 0 ]
