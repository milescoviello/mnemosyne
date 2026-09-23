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
# what it was asked, one bracket per argument so a split value shows
{ printf 'mnemosyne:'; printf ' [%s]' "\$@"; echo; } >> "\$MN_TEST_LOG.mn"
cat "$plan_file"
# Stay "open" a moment after handing the plan over, the way the browser
# does, then say when it closed: whatever the wrapper prints before this
# line would have been drawn on top of the browser.
if [ -n "\${MN_STUB_LINGER:-}" ]; then
    sleep 1
    echo "-- browser closed --" >&2
fi
exit \${MN_STUB_EXIT:-0}
EOF

cat > "$bin/claude" <<'EOF'
#!/bin/sh
echo "claude: $*" >> "$MN_TEST_LOG"
# the same again, one bracket per argument, so a split one shows
{ printf 'claude-args:'; printf ' [%s]' "$@"; echo; } >> "$MN_TEST_LOG"
echo "claude-pwd: $(pwd)" >> "$MN_TEST_LOG"
# Stay alive so the tmux session it belongs to stays alive with it. Only
# there: anywhere else it is waited for, and twenty seconds a time adds up.
[ -n "$TMUX" ] && sleep 20
exit 0
EOF

cat > "$bin/faketerm" <<'EOF'
#!/bin/sh
echo "term: $*" >> "$MN_TEST_LOG"
# Which session did we land in? If it is the caller's, closing the window
# that ran mn takes this one down with it.
echo "term-sid: $(ps -o sid= -p $$ | tr -d ' ')" >> "$MN_TEST_LOG"
# Run what the window would have run, when asked: `-e <shell> -lc <command>`.
# Not as a login shell, though -- that rereads /etc/profile, which on some
# systems resets PATH, and then `claude` would be the real one rather than
# the stub. fish without its config, for the same reason.
[ "${MN_TERM_RUN:-}" = 1 ] || exit 0
while [ $# -gt 0 ] && [ "$1" != -e ]; do shift; done
[ $# -ge 4 ] || exit 0
sh_=$2; cmd_=$4
case "$sh_" in
    fish) exec fish --no-config -c "$cmd_" ;;
    *) exec "$sh_" -c "$cmd_" ;;
esac
EOF

# Prints the session id of whatever shell invoked it, so the check below
# reads the same in fish and bash.
cat > "$bin/mysid" <<'EOF'
#!/bin/sh
echo "my-sid: $(ps -o sid= -p $PPID | tr -d ' ')"
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
# Run from inside tmux, these would say so, and the wrappers read them: it
# picks switch-client over attach-session. The result should not depend on
# where the test happens to be run from.
unset TMUX TMUX_PANE
export MN_TEST_LOG="$log"

write_plan() { printf '%b' "$1" > "$plan_file"; }

# Windows are started detached, and tmux starts its pane on its own time,
# so what they did shows up in the log a moment later. $1 text, $2 file.
wait_for() {
    local i=0
    while [ $i -lt 50 ]; do
        grep -qF -- "$1" "$2" 2>/dev/null && return 0
        sleep 0.1
        i=$((i + 1))
    done
    return 1
}

# tab-separated, as mnemosyne prints it
WINTMUX_PLAN="wintmux\t$tmp/work-a\t026bcdb5-8d88-4ad7-9f23-58649bf4f353\tclaude-opus-5\tbypassPermissions\tthe api gateway timeout\n"
WINTMUX_PLAN="${WINTMUX_PLAN}wintmux\t$tmp/work-b\t11111111-2222-3333-4444-555555555555\t\tdefault\tparser byte offsets\n"
WINTMUX_PLAN="${WINTMUX_PLAN}wintmux\t$tmp/no-such-folder\t22222222-3333-4444-5555-666666666666\t\tdefault\tgone\n"

WINDOW_PLAN="window\t$tmp/work-a\t026bcdb5-8d88-4ad7-9f23-58649bf4f353\t\tdefault\tno model recorded\n"

# landing in this terminal: the one case that needs the shell to cd
HERE_PLAN="here\t$tmp/work-b\t33333333-4444-5555-6666-777777777777\tclaude-opus-5\tplan\tright here\n"

# ctrl+t: into tmux, in this terminal. The attach at the end cannot work
# here, with no terminal to attach, but everything before it can be checked.
TMUX_PLAN="tmux\t$tmp/work-b\t66666666-7777-8888-9999-000000000000\t\tdefault\tstraight into tmux\t\n"

# a seventh field: the tmux session name chosen at the prompt
NAMED_PLAN="wintmux\t$tmp/work-a\t026bcdb5-8d88-4ad7-9f23-58649bf4f353\t\tdefault\tnamed one\tmy-own-name\n"

# two different chats asking for the same name, which is what selecting
# several and naming them once produces
CLASH_PLAN="wintmux\t$tmp/work-a\t026bcdb5-8d88-4ad7-9f23-58649bf4f353\t\tdefault\tfirst chat\tbatch\n"
CLASH_PLAN="${CLASH_PLAN}wintmux\t$tmp/work-b\t11111111-2222-3333-4444-555555555555\t\tdefault\tsecond chat\tbatch\n"

# ---- one shell's worth of checks ---------------------------------------
run_shell() {
    local shell_name="$1" source_line="$2" runner="$3"
    # what the new window runs its command with, which for zsh is bash:
    # mn.bash builds a bash command line for it
    local inner="${4:-$1}"
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
            "term: -e $inner -lc exec tmux attach-session -t =mn-026bcdb5" "$seen"
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

    # --- nothing is drawn over the browser, and each window is said once
    # Window lines arrive while the browser is still on screen. Anything
    # printed then lands on top of it and vanishes with it -- which fish did
    # with every tmux session it created, and with "folder gone".
    if [ -n "$real_tmux" ]; then
        "$bin/tmux" kill-server 2>/dev/null
        write_plan "$WINTMUX_PLAN"
        out=$(MN_STUB_LINGER=1 "$runner" -c "$source_line; mn" 2>&1)
        local early="${out%%"-- browser closed --"*}"
        if [ "$early" = "$out" ]; then
            bad "$shell_name: nothing is printed over the browser" "the stub never said it closed"
        elif [ -n "$early" ]; then
            bad "$shell_name: nothing is printed over the browser" "printed while it was open: $early"
        else
            ok "$shell_name: nothing is printed over the browser"
        fi
        local times; times=$(grep -c "parser byte offsets" <<< "$out")
        if [ "$times" = 1 ]; then
            ok "$shell_name: each window is reported once"
        else
            bad "$shell_name: each window is reported once" "reported $times times"
        fi
        "$bin/tmux" kill-server 2>/dev/null
    else
        skip "$shell_name: nothing printed over the browser" "tmux is not installed"
    fi

    # --- ctrl+t, into tmux in this terminal
    if [ -n "$real_tmux" ]; then
        "$bin/tmux" kill-server 2>/dev/null
        write_plan "$TMUX_PLAN"
        : > "$log"
        out=$("$runner" -c "$source_line; mn" 2>&1)
        local tsess; tsess=$("$bin/tmux" list-sessions -F '#{session_name}' 2>/dev/null)
        has "$shell_name: ctrl+t makes the session" "mn-66666666" "$tsess"
        has "$shell_name: and says so" "straight into tmux  (tmux mn-66666666)" "$out"
        local tn; tn=$(grep -c "straight into tmux" <<< "$out")
        if [ "$tn" = 1 ]; then ok "$shell_name: once"; else bad "$shell_name: once" "said $tn times"; fi
        "$bin/tmux" kill-server 2>/dev/null
    else
        skip "$shell_name: ctrl+t" "tmux is not installed"
    fi

    # --- a tmux session named at the prompt
    if [ -n "$real_tmux" ]; then
        "$bin/tmux" kill-server 2>/dev/null
        write_plan "$NAMED_PLAN"
        : > "$log"
        out=$("$runner" -c "$source_line; mn" 2>&1)
        local named; named=$("$bin/tmux" list-sessions -F '#{session_name}' 2>/dev/null)
        has "$shell_name: the session takes the name you gave it" "my-own-name" "$named"
        hasnt "$shell_name: and not the generated one" "mn-026bcdb5" "$named"

        # the double-attach guard has to survive a name it did not choose:
        # the chat is found by the command the pane was started with
        : > "$log"
        out=$("$runner" -c "$source_line; mn" 2>&1)
        seen=$(cat "$log")
        hasnt "$shell_name: a named session is still found again" "claude:" "$seen"
        has "$shell_name: and attached to by its real name" "tmux my-own-name, already running" "$out"
        "$bin/tmux" kill-server 2>/dev/null
    else
        skip "$shell_name: named tmux session" "tmux is not installed"
    fi

    # --- the window has to outlive the shell that opened it
    write_plan "$WINDOW_PLAN"
    : > "$log"
    out=$("$runner" -c "$source_line; mn; mysid" 2>&1)
    seen=$(cat "$log")
    local mine theirs
    mine=$(printf '%s' "$out" | sed -n 's/^my-sid: //p' | head -1)
    theirs=$(printf '%s' "$seen" | sed -n 's/^term-sid: //p' | head -1)
    if [ -z "$mine" ] || [ -z "$theirs" ]; then
        skip "$shell_name: window outlives its parent" "could not read session ids"
    elif [ "$mine" = "$theirs" ]; then
        bad "$shell_name: window outlives its parent" \
            "opened in the caller's session ($mine) — closing the terminal would kill it"
    else
        ok "$shell_name: window outlives its parent"
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

    # --- one name, several chats
    if [ -n "$real_tmux" ]; then
        "$bin/tmux" kill-server 2>/dev/null
        write_plan "$CLASH_PLAN"
        : > "$log"
        out=$("$runner" -c "$source_line; mn" 2>&1)
        seen=$(cat "$log")
        local names started
        names=$("$bin/tmux" list-sessions -F '#{session_name}' 2>/dev/null | sort | tr '\n' ' ')
        started=$(grep -c '^claude: --resume' <<< "$seen")
        if [ "$started" = "2" ]; then
            ok "$shell_name: both chats actually started"
        else
            bad "$shell_name: both chats actually started" \
                "started $started; one name for several chats used to collapse them into one"
        fi
        has "$shell_name: the first takes the name" "batch" "$names"
        has "$shell_name: the second gets its own" "batch-2" "$names"
        "$bin/tmux" kill-server 2>/dev/null
    else
        skip "$shell_name: one name several chats" "tmux is not installed"
    fi

    # --- landing in this terminal
    write_plan "$HERE_PLAN"
    : > "$log"
    out=$("$runner" -c "$source_line; mn; pwd" 2>&1)
    seen=$(cat "$log")
    has "$shell_name: resuming here runs claude" \
        "claude: --resume 33333333-4444-5555-6666-777777777777 --model claude-opus-5" "$seen"
    has "$shell_name: and maps the recorded permission mode" "--permission-mode plan" "$seen"
    has "$shell_name: and the shell cd'd there" "$tmp/work-b" "$out"
    has "$shell_name: and says which session" "right here" "$out"

    # zsh's echo reads backslashes, so a title with a Windows path in it
    # came out broken over two lines. The four backslashes below are one by
    # the time the plan is written: halved by the quotes, then by printf %b.
    write_plan "here\t$tmp/work-b\t33333333-4444-5555-6666-777777777777\t\tplan\tfix C:\\\\new folder\n"
    out=$("$runner" -c "$source_line; mn" 2>&1)
    has "$shell_name: a backslash in a title is printed as one" 'fix C:\new folder' "$out"

    # --- claude's arguments arrive as you typed them
    # They were pasted into a command line unquoted, so in a window or under
    # tmux `--add-dir "/my projects"` arrived as two arguments -- and a `;`
    # or `$(...)` inside one was run as a command. The folder is awkward on
    # purpose: a quote, a space and a dollar sign.
    local odd="$tmp/it's a \$dir"
    mkdir -p "$odd"
    # The substitution leads: after a `;`, the exec before it would have
    # replaced the shell first and hidden the problem.
    local fwd_args="--add-dir '/my projects' --append-system-prompt '\$(touch $tmp/ran-it);x'"
    local want_args='[--add-dir] [/my projects] [--append-system-prompt] [$(touch '"$tmp"'/ran-it);x]'
    rm -f "$tmp/ran-it"

    if [ -n "$real_tmux" ]; then
        "$bin/tmux" kill-server 2>/dev/null
        write_plan "wintmux\t$odd\t44444444-5555-6666-7777-888888888888\t\tdefault\todd one\n"
        : > "$log"
        "$runner" -c "$source_line; mn $fwd_args" >/dev/null 2>&1
        wait_for "claude-pwd:" "$log"
        seen=$(cat "$log")
        has "$shell_name: under tmux, an argument with a space stays one" "$want_args" "$seen"
        has "$shell_name: under tmux, the odd folder is where it starts" "claude-pwd: $odd" "$seen"
        "$bin/tmux" kill-server 2>/dev/null
    else
        skip "$shell_name: arguments under tmux" "tmux is not installed"
    fi

    write_plan "window\t$odd\t55555555-6666-7777-8888-999999999999\t\tdefault\todd one\n"
    : > "$log"
    MN_TERM_RUN=1 "$runner" -c "$source_line; mn $fwd_args" >/dev/null 2>&1
    wait_for "claude-pwd:" "$log"
    seen=$(cat "$log")
    has "$shell_name: in a window, an argument with a space stays one" "$want_args" "$seen"
    has "$shell_name: in a window, the odd folder is where it starts" "claude-pwd: $odd" "$seen"
    if [ -e "$tmp/ran-it" ]; then
        bad "$shell_name: nothing in an argument is run" "the \$(touch) inside an argument ran"
    else
        ok "$shell_name: nothing in an argument is run"
    fi

    # --- our flags are ours, claude's are claude's
    write_plan "$HERE_PLAN"
    : > "$log"
    out=$("$runner" -c "$source_line; mn --no-splash --verbose" 2>&1)
    seen=$(cat "$log")
    has "$shell_name: unknown flags go through to claude" "--verbose" "$seen"
    hasnt "$shell_name: our own flags do not" "--no-splash" "$seen"

    # one the wrapper's own list once left out
    : > "$log"; : > "$log.mn"
    out=$("$runner" -c "$source_line; mn --no-mouse --verbose" 2>&1)
    seen=$(cat "$log")
    has "$shell_name: --no-mouse reaches mnemosyne" "[--no-mouse]" "$(cat "$log.mn")"
    hasnt "$shell_name: and is not handed to claude" "--no-mouse" "$seen"

    # --- flags that answer and exit
    # Their reply is for you to read. It used to go through the plan loop,
    # which took "up to date on 0.4.14" for a session in a folder called ""
    # and said so: "folder gone, skipping:".
    write_plan "up to date on 0.4.14 (latest is v0.4.14)\n"
    : > "$log"; : > "$log.mn"
    out=$("$runner" -c "$source_line; mn --check-update" 2>&1)
    has "$shell_name: an answer is shown to you" "up to date on 0.4.14 (latest is v0.4.14)" "$out"
    hasnt "$shell_name: and not read as a plan" "folder gone" "$out"

    write_plan "sessions  4\n"
    : > "$log"; : > "$log.mn"
    out=$("$runner" -c "$source_line; mn --stats" 2>&1)
    seen=$(cat "$log")
    has "$shell_name: --stats reaches mnemosyne" "[--stats]" "$(cat "$log.mn")"
    has "$shell_name: and its answer is shown" "sessions  4" "$out"
    hasnt "$shell_name: and claude is not started" "claude" "$seen"

    : > "$log"; : > "$log.mn"
    out=$("$runner" -c "$source_line; mn --search 'connection reset' --search-mode tool" 2>&1)
    has "$shell_name: a search keeps its words together" \
        "[--search] [connection reset] [--search-mode] [tool]" "$(cat "$log.mn")"
    hasnt "$shell_name: and starts nothing" "claude" "$(cat "$log")"

    # a claude flag next to a report flag goes to mnemosyne, which refuses
    # it, rather than being dropped as though it had worked
    : > "$log.mn"
    "$runner" -c "$source_line; mn --list --verbose" >/dev/null 2>&1
    has "$shell_name: nothing is quietly dropped from a report" "[--list] [--verbose]" "$(cat "$log.mn")"

    # older fish reads `case --help` as asking for help on `case`
    : > "$log.mn"
    "$runner" -c "$source_line; mn --help" >/dev/null 2>&1
    has "$shell_name: --help is mnemosyne's" "[--help]" "$(cat "$log.mn")"

    out=$(MN_STUB_EXIT=2 "$runner" -c "$source_line; mn --stats || echo failure-came-through" 2>&1)
    has "$shell_name: a report that fails, fails" "failure-came-through" "$out"

    # the same through the plan loop, which used to end in success whatever
    # mnemosyne said: `mn --restore abc && ...` carried on regardless
    write_plan ""
    out=$(MN_STUB_EXIT=2 "$runner" -c "$source_line; mn --restore abc || echo failure-came-through" 2>&1)
    has "$shell_name: a refused plan fails too" "failure-came-through" "$out"
    out=$("$runner" -c "$source_line; mn && echo success-came-through" 2>&1)
    has "$shell_name: and quitting the browser is still success" "success-came-through" "$out"

    # --- a line that is not a plan is shown, not acted on
    # A newer mnemosyne could say something this wrapper does not know
    # about; treating it as a window to open is the one thing not to do.
    write_plan "something the wrapper has never seen\n"
    : > "$log"
    out=$("$runner" -c "$source_line; mn" 2>&1)
    has "$shell_name: an unknown line is printed" "something the wrapper has never seen" "$out"
    hasnt "$shell_name: and not taken for a session" "folder gone" "$out"
    hasnt "$shell_name: nor opened" "term:" "$(cat "$log")"
}

printf 'shell wrapper self-test\n'

run_shell bash "source $root/shell/mn.bash" bash

# The installer wires mn.bash into ~/.zshrc as well, and zsh is what a Mac
# opens by default, so it has to run there too.
if command -v zsh >/dev/null 2>&1; then
    run_shell zsh "source $root/shell/mn.bash" zsh bash
else
    printf '\nzsh\n'
    skip "zsh with mn.bash" "zsh is not installed"
fi

if command -v fish >/dev/null 2>&1; then
    run_shell fish "source $root/shell/mn.fish" fish
else
    printf '\nfish\n'
    skip "fish wrapper" "fish is not installed"
fi

printf '\n%d checks, %d failed, %d skipped\n' "$checks" "$fails" "$skips"
[ "$fails" -eq 0 ]
