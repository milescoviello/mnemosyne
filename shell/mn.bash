# bash/zsh equivalent of mn.fish — source this from ~/.bashrc
# The cd must happen in the shell, hence a function rather than a script.

# Map a session's recorded permission mode to claude flags. A session resumes
# under the mode it was started in; an explicit flag on the command line wins,
# and --ask forces normal prompts.
__mn_perms() {
    local mode="$1"; shift
    local no_bypass="$1"; shift
    [ "$no_bypass" = "1" ] && return 0
    for a in "$@"; do
        case "$a" in
            --dangerously-skip-permissions|--permission-mode) return 0 ;;
        esac
    done
    case "$mode" in
        bypassPermissions) printf '%s\n' --dangerously-skip-permissions ;;
        # "default" is deliberately absent below: it is not a valid value for
        # --permission-mode, and it means "behave normally" anyway.
        plan|acceptEdits|auto|manual|dontAsk) printf '%s\n%s\n' --permission-mode "$mode" ;;
        default) return 0 ;;
        # nothing recorded (older transcripts predate the field) — keep the old
        # cs behaviour rather than surprising anyone with prompts
        *) printf '%s\n' --dangerously-skip-permissions ;;
    esac
}

# Run a command in a new terminal window; print which terminal was used.
__mn_term_open() {
    local cwd="$1" inner="$2" term
    local -a cmd
    for term in $MN_TERMINAL alacritty konsole kitty wezterm foot ghostty xterm; do
        command -v "$term" >/dev/null 2>&1 || continue
        case "$term" in
            alacritty) cmd=("$term" --working-directory "$cwd" -e bash -lc "$inner") ;;
            konsole)   cmd=("$term" --workdir "$cwd" -e bash -lc "$inner") ;;
            kitty)     cmd=("$term" --directory "$cwd" bash -lc "$inner") ;;
            wezterm)   cmd=("$term" start --cwd "$cwd" -- bash -lc "$inner") ;;
            foot)      cmd=("$term" --working-directory="$cwd" bash -lc "$inner") ;;
            ghostty)   cmd=("$term" --working-directory="$cwd" -e bash -lc "$inner") ;;
            *)         cmd=("$term" -e bash -lc "$inner") ;;
        esac
        __mn_spawn "${cmd[@]}"
        printf '%s\n' "$term"
        return 0
    done

    # macOS has none of those. Terminal.app is told to run a script rather
    # than a command line, which keeps a shell command out of AppleScript
    # quoting entirely -- and it is spawned by the system, so it is already
    # detached from this shell.
    if command -v osascript >/dev/null 2>&1; then
        local tmp; tmp="$(mktemp -t mn-open)" || return 1
        printf '#!/bin/sh\nrm -f %q\ncd %q\n%s\n' "$tmp" "$cwd" "$inner" > "$tmp"
        chmod +x "$tmp"
        osascript -e "tell application \"Terminal\" to do script \"$tmp\"" \
                  -e 'tell application "Terminal" to activate' >/dev/null
        printf '%s\n' Terminal.app
        return 0
    fi
    return 1
}

# Start a window that outlives the terminal that asked for it.
#
# `disown` only removes the job from this shell's table -- the child keeps our
# process group and session, so closing the window running `mn` sends it
# SIGHUP and every window we just opened disappears with it. setsid gives it a
# session of its own; `-f` always forks, which matters because a backgrounded
# job is already a group leader and plain setsid would refuse.
__mn_spawn() {
    if command -v setsid >/dev/null 2>&1; then
        setsid -f "$@" >/dev/null 2>&1
    else
        "$@" >/dev/null 2>&1 &
        disown 2>/dev/null
    fi
}

# Make sure a tmux session exists for this chat; print its name. Progress goes
# to stderr so the caller can capture just the name.
__mn_tmux_ensure() {
    local cwd="$1" sid="$2" mdl="$3" ttl="$4" want="$5"; shift 5
    [ "$1" = "--" ] && shift
    command -v tmux >/dev/null 2>&1 || { echo "  ✗ tmux is not installed" >&2; return 1; }

    local name="mn-${sid:0:8}"
    [ -n "$want" ] && name="$want"

    # Already there, whatever it ended up called: attach rather than starting
    # a second client on the same transcript. tmux remembers the command each
    # pane was started with, so the chat is found by its session id and not by
    # a name that is now yours to choose.
    local have
    have="$(tmux list-panes -a -F '#{session_name}	#{pane_start_command}' 2>/dev/null \
            | grep -F -- "--resume $sid" | head -1 | cut -f1)"
    if [ -n "$have" ]; then
        echo "  ▶ $have already running — resuming where it left off" >&2
        printf '%s\n' "$have"
        return 0
    fi
    # The name is taken, and not by this chat -- the check above would have
    # found it. Attaching anyway would drop you into someone else's session
    # claiming it was yours. It also happens whenever several chats are
    # opened under one chosen name, which used to collapse them all into a
    # single session running only the first.
    if tmux has-session -t "=$name" 2>/dev/null; then
        local n=2
        while tmux has-session -t "=$name-$n" 2>/dev/null; do n=$((n + 1)); done
        name="$name-$n"
    fi
    [ -d "$cwd" ] || { echo "  ✗ folder gone, skipping: $cwd" >&2; return 1; }

    local margs=(); [ -n "$mdl" ] && margs=(--model "$mdl")
    local wname; wname="$(printf '%s' "$ttl" | tr -c 'a-zA-Z0-9._-' '-')"; wname="${wname:0:18}"
    [ -z "$wname" ] && wname="$name"
    if tmux new-session -d -s "$name" -n "$wname" -c "$cwd" \
           "exec claude --resume $sid ${margs[*]} $*" 2>/dev/null; then
        echo "  ▶ $ttl  (tmux $name)" >&2
        printf '%s\n' "$name"
        return 0
    fi
    echo "  ✗ could not create tmux session $name" >&2
    return 1
}

# attach-session fails when already inside tmux; switch-client is the in-tmux
# equivalent.
__mn_tmux_attach() {
    if [ -n "$TMUX" ]; then tmux switch-client -t "=$1"; else tmux attach-session -t "=$1"; fi
}

# Split one plan line into its six fields.
#
# `read` cannot be given tab as the separator directly: bash treats tab as IFS
# whitespace, so a run of them collapses into one and an empty field -- a
# session with no model recorded, say -- silently shifts every field after it
# along by one. Swapping in a non-whitespace separator first is what stops
# that; unit separator cannot appear in a path or a title.
__mn_split() {
    local line="${1//$'\t'/$'\x1f'}"
    IFS=$'\x1f' read -r "${@:2}" <<< "$line"
}

mn() {
    local mine=() fwd=() no_bypass=0 a take_value=0
    for a in "$@"; do
        if [ "$take_value" = 1 ]; then mine+=("$a"); take_value=0; continue; fi
        case "$a" in
            --no-splash|--subagents|--no-model|--no-update|--update|--check-update|--write-config|--reopen) mine+=("$a") ;;
            # --restore takes a count, which is ours and not claude's
            --restore) mine+=("$a"); take_value=1 ;;
            --ask|--no-bypass) no_bypass=1 ;;
            *) fwd+=("$a") ;;
        esac
    done

    # Read the plan as it is produced, not after the browser exits. A
    # session opened in a window of its own does not need the picker to close
    # first, so mnemosyne hands those over while it is still running and this
    # loop acts on each as it arrives. Only `here` and `tmux` need this
    # terminal, and those arrive last, on the way out.
    #
    # Process substitution, not a pipe: the loop has to run in this shell or
    # the `cd` below would happen in a subshell and be lost.
    # Progress is collected, not printed: the browser is still on screen
    # while these run, and writing over it is what made it look like mn had
    # half-exited. It all comes out once the screen is ours again.
    local finally="" tmux_first="" notes="" line mode cwd sid mdl prm ttl tmx nm term inner name
    # __mn_tmux_ensure prints the session name on stdout and its progress on
    # stderr, so the two have to stay apart: the name is a value, the
    # progress is for you to read afterwards.
    local errf; errf="$(mktemp)" || return 1
    # Read on fd 3, not stdin, and give every command in the body /dev/null
    # for input. Otherwise the loop's stdin is the pipe and anything that
    # reads stdin -- tmux does -- swallows the next plan line, so the second
    # window never opens. It would take keystrokes from the browser too,
    # which is still running and reading the terminal.
    while IFS= read -r line <&3; do
        [ -z "$line" ] && continue
        __mn_split "$line" mode cwd sid mdl prm ttl tmx
        local extra=(); mapfile -t extra < <(__mn_perms "$prm" "$no_bypass" "${fwd[@]}")
        local margs=(); [ -n "$mdl" ] && margs=(--model "$mdl")

        { case "$mode" in
            here)
                finally="$line"
                ;;
            tmux)
                nm="$(__mn_tmux_ensure "$cwd" "$sid" "$mdl" "$ttl" "$tmx" -- "${extra[@]}" "${fwd[@]}" 2>>"$errf")" \
                    && [ -z "$tmux_first" ] && tmux_first="$nm"
                ;;
            wintmux)
                [ -d "$cwd" ] || { notes+="  ✗ folder gone, skipping: $cwd"$'\n'; continue; }
                if command -v tmux >/dev/null 2>&1; then
                    name="$(__mn_tmux_ensure "$cwd" "$sid" "$mdl" "$ttl" "$tmx" -- "${extra[@]}" "${fwd[@]}" 2>>"$errf")" || continue
                    if term="$(__mn_term_open "$cwd" "exec tmux attach-session -t =$name")"; then
                        notes+="  ▶ $ttl  ($term → tmux $name)"$'\n'
                    else
                        notes+="  ▶ $ttl  (tmux $name — no terminal to show it in; ctrl+t attaches)"$'\n'
                    fi
                else
                    inner="cd $(printf %q "$cwd"); exec claude --resume $sid ${margs[*]} ${extra[*]} ${fwd[*]}"
                    term="$(__mn_term_open "$cwd" "$inner")" && notes+="  ▶ $ttl  ($term)"$'\n'
                fi
                ;;
            *)
                [ -d "$cwd" ] || { notes+="  ✗ folder gone, skipping: $cwd"$'\n'; continue; }
                inner="cd $(printf %q "$cwd"); exec claude --resume $sid ${margs[*]} ${extra[*]} ${fwd[*]}"
                if term="$(__mn_term_open "$cwd" "$inner")"; then
                    notes+="  ▶ $ttl  ($term)"$'\n'
                else
                    notes+="  ✗ no terminal emulator found (set \$MN_TERMINAL)"$'\n'
                fi
                ;;
        esac; } </dev/null
    done 3< <(mnemosyne "${mine[@]}")

    notes="$(cat "$errf")"$'\n'"$notes"; rm -f "$errf"
    printf '%s' "$notes" | grep -v '^$'
    if [ -n "$tmux_first" ]; then __mn_tmux_attach "$tmux_first"; return 0; fi
    [ -z "$finally" ] && return 0

    # Landing in this terminal: the cd has to happen here, which is the whole
    # reason this is a function.
    __mn_split "$finally" mode cwd sid mdl prm ttl tmx
    local extra=(); mapfile -t extra < <(__mn_perms "$prm" "$no_bypass" "${fwd[@]}")
    local margs=(); [ -n "$mdl" ] && margs=(--model "$mdl")
    if [ -d "$cwd" ]; then cd "$cwd" || return 1
    else echo "folder is gone: $cwd — resuming from $PWD"; fi
    echo "▶ $ttl"
    claude --resume "$sid" "${margs[@]}" "${extra[@]}" "${fwd[@]}"
}
