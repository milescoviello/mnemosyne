#!/usr/bin/env bash
# Exercise the shell wrappers without running Claude, opening a window, or
# touching your tmux sessions.
#
# The wrappers are where the interesting failures live: mnemosyne prints a
# plan, and everything after that -- splitting the fields, mapping permission
# modes, creating the tmux session, opening the window -- happens in shell,
# which nothing else here tests. Two field-splitting bugs in mn.bash were
# found by exactly this and would not have shown up in any Rust test.
#
#   tools/shell-selftest.sh
#
# Stubs stand in for mnemosyne, claude and the terminal emulator, and tmux is
# run on a private socket so a test can never disturb real sessions.
set -u

cd "$(dirname "$0")/.." || exit 1
root=$(pwd)
tmp=$(mktemp -d)
bin="$tmp/bin"
log="$tmp/log"
mkdir -p "$bin" "$tmp/work-a" "$tmp/work-b"

fails=0
checks=0
skips=0

cleanup() {
    [ -x "$bin/tmux" ] && "$bin/tmux" kill-server 2>/dev/null
    rm -rf "$tmp"
}
trap cleanup EXIT

ok() {
    checks=$((checks + 1))
    printf '  ok   %s\n' "$1"
}
bad() {
    checks=$((checks + 1))
    fails=$((fails + 1))
    printf '  FAIL %s\n' "$1"
    printf '       %s\n' "$2"
}
skip() {
    skips=$((skips + 1))
    printf '  skip %s (%s)\n' "$1" "$2"
}

# $1 what it is, $2 the text that must appear, $3 where to look
has() { case "$3" in *"$2"*) ok "$1" ;; *) bad "$1" "expected to find: $2" ;; esac; }
hasnt() { case "$3" in *"$2"*) bad "$1" "should not contain: $2" ;; *) ok "$1" ;; esac; }

# ---- stubs -------------------------------------------------------------
# The plan mnemosyne would have printed. Line 2 deliberately has an empty
# model field: an empty field between two tabs is what shifted every later
# field along by one in bash, silently losing the title.
plan_file="$tmp/plan"
cat > "$bin/mnemosyne" <<EOF
#!/bin/sh
cat "$plan_file"
EOF

cat > "$bin/claude" <<'EOF'
#!/bin/sh
echo "claude: $*" >> "$MN_TEST_LOG"
# stay alive so the tmux session it belongs to stays alive with it
sleep 20
EOF

cat > "$bin/faketerm" <<'EOF'
#!/bin/sh
echo "term: $*" >> "$MN_TEST_LOG"
EOF

real_tmux=$(command -v tmux 2>/dev/null)
if [ -n "$real_tmux" ]; then
    cat > "$bin/tmux" <<EOF
#!/bin/sh
# a private server, so this can never touch the user's own sessions
exec "$real_tmux" -L mn-selftest "\$@"
EOF
fi
chmod +x "$bin"/*

export PATH="$bin:$PATH"
export MN_TERMINAL=faketerm
export MN_TEST_LOG="$log"

write_plan() { printf '%b' "$1" > "$plan_file"; }

# tab-separated, as mnemosyne prints it
WINTMUX_PLAN="wintmux\t$tmp/work-a\t026bcdb5-8d88-4ad7-9f23-58649bf4f353\tclaude-opus-5\tbypassPermissions\tthe api gateway timeout\n"
WINTMUX_PLAN="${WINTMUX_PLAN}wintmux\t$tmp/work-b\t11111111-2222-3333-4444-555555555555\t\tdefault\tparser byte offsets\n"
WINTMUX_PLAN="${WINTMUX_PLAN}wintmux\t$tmp/no-such-folder\t22222222-3333-4444-5555-666666666666\t\tdefault\tgone\n"

WINDOW_PLAN="window\t$tmp/work-a\t026bcdb5-8d88-4ad7-9f23-58649bf4f353\t\tdefault\tno model recorded\n"

# ---- one shell's worth of checks ---------------------------------------
run_shell() {
    local shell_name="$1" source_line="$2" runner="$3"
    printf '\n%s\n' "$shell_name"

    [ -n "$real_tmux" ] && "$bin/tmux" kill-server 2>/dev/null

    # --- a window each, with tmux underneath
    write_plan "$WINTMUX_PLAN"
    : > "$log"
    local out
    out=$("$runner" -c "$source_line; mn" 2>&1)
    local seen; seen=$(cat "$log")

    has "$shell_name: the title survives field splitting" "parser byte offsets" "$out"
    hasnt "$shell_name: an empty model is not read as the next field" "--model default" "$seen"
    has "$shell_name: a recorded bypass mode comes back" \
        "claude: --resume 026bcdb5-8d88-4ad7-9f23-58649bf4f353 --model claude-opus-5 --dangerously-skip-permissions" "$seen"
    has "$shell_name: a default mode resumes with prompts on" \
        "claude: --resume 11111111-2222-3333-4444-555555555555" "$seen"
    has "$shell_name: a folder that is gone is skipped" "folder gone" "$out"
    hasnt "$shell_name: and nothing is started for it" "22222222" "$seen"

    if [ -n "$real_tmux" ]; then
        has "$shell_name: a window is opened onto the tmux session" \
            "term: -e $shell_name -lc exec tmux attach-session -t =mn-026bcdb5" "$seen"
        local sessions; sessions=$("$bin/tmux" list-sessions -F '#{session_name}' 2>/dev/null)
        has "$shell_name: the session is named for the chat" "mn-026bcdb5" "$sessions"
        has "$shell_name: and so is the second one" "mn-11111111" "$sessions"

        # running it again must attach, not start a second client on the
        # same transcript
        : > "$log"
        out=$("$runner" -c "$source_line; mn" 2>&1)
        seen=$(cat "$log")
        hasnt "$shell_name: a second run starts no second claude" "claude:" "$seen"
        has "$shell_name: it attaches to what is already there" "already running" "$out"
        "$bin/tmux" kill-server 2>/dev/null
    else
        skip "$shell_name: tmux checks" "tmux is not installed"
    fi

    # --- a plain window, and --ask
    write_plan "$WINDOW_PLAN"
    : > "$log"
    out=$("$runner" -c "$source_line; mn" 2>&1)
    seen=$(cat "$log")
    has "$shell_name: a window runs claude directly" "claude --resume 026bcdb5" "$seen"
    hasnt "$shell_name: an empty model adds no flag" "--model" "$seen"

    : > "$log"
    out=$("$runner" -c "$source_line; mn --ask" 2>&1)
    seen=$(cat "$log")
    hasnt "$shell_name: --ask refuses to skip permissions" "--dangerously-skip-permissions" "$seen"

    # --- our flags are ours, claude's are claude's
    : > "$log"
    out=$("$runner" -c "$source_line; mn --no-splash --verbose" 2>&1)
    seen=$(cat "$log")
    has "$shell_name: unknown flags go through to claude" "--verbose" "$seen"
    hasnt "$shell_name: our own flags do not" "--no-splash" "$seen"
}

printf 'shell wrapper self-test\n'

run_shell bash "source $root/shell/mn.bash" bash

if command -v fish >/dev/null 2>&1; then
    run_shell fish "source $root/shell/mn.fish" fish
else
    printf '\nfish\n'
    skip "fish wrapper" "fish is not installed"
fi

printf '\n%d checks, %d failed, %d skipped\n' "$checks" "$fails" "$skips"
[ "$fails" -eq 0 ]
