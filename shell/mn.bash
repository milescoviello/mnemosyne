# bash/zsh equivalent of mn.fish — source this from ~/.bashrc or ~/.zshrc
# The cd must happen in the shell, hence a function rather than a script.
#
# Everything here has to mean the same thing to both shells, and several
# obvious spellings do not: zsh has no `mapfile`, does not expand $'\x1f' in
# the replacement half of ${var//a/b}, and its `echo` turns a `\n` in a title
# into a line break. tools/shell-selftest.sh runs this file under both.

# Map a session's recorded permission mode to claude flags. A session resumes
# under the mode it was started in; an explicit flag on the command line wins,
# and --ask forces normal prompts.
__mn_perms() {
    local mode="$1"; shift
    local no_bypass="$1"; shift
    [ "$no_bypass" = "1" ] && return 0
    for a in "$@"; do
        case "$a" in
            # either spelling: with an `=` it went unseen, and the recorded
            # bypass was added next to the plan mode you asked for
            --dangerously-skip-permissions|--permission-mode|--permission-mode=*) return 0 ;;
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

# The claude this shell would run, as a path. A new window's login shell and
# a tmux server started from somewhere else need not have the same PATH as
# you, and "claude: command not found" in a window that then closes is a
# poor way to find that out.
__mn_claude() {
    local c; c="$(command -v claude 2>/dev/null)"
    case "$c" in /*) printf '%s\n' "$c" ;; *) printf '%s\n' claude ;; esac
}

# Words quoted for a bash command line, for a window to run. Each one is
# escaped on its own: pasted in bare, `--add-dir "/my projects"` arrived as
# two arguments, and a `$(...)` inside one was run.
__mn_quote() {
    local w out=""
    for w in "$@"; do out+="$(printf '%q' "$w") "; done
    printf '%s' "${out% }"
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

# Make sure a tmux session exists for this chat. Prints its name, then `new`
# or `running`, and nothing else: saying what happened is the caller's job,
# once, in the notes it prints after the browser has closed. Printing here as
# well said everything twice. Errors go to stderr.
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
        printf '%s\nrunning\n' "$have"
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
    [ -d "$cwd" ] || { printf '  ✗ folder gone, skipping: %s\n' "$cwd" >&2; return 1; }

    local margs=(); [ -n "$mdl" ] && margs=(--model "$mdl")
    local wname; wname="$(printf '%s' "$ttl" | tr -c 'a-zA-Z0-9._-' '-')"; wname="${wname:0:18}"
    [ -z "$wname" ] && wname="$name"
    # Separate words, which tmux runs as they are. Given one string it hands
    # that to your default shell to parse -- whichever shell that is -- and
    # an argument with a space in it came out as two.
    if tmux new-session -d -s "$name" -n "$wname" -c "$cwd" \
           "$(__mn_claude)" --resume "$sid" "${margs[@]}" "$@" 2>/dev/null; then
        printf '%s\nnew\n' "$name"
        return 0
    fi
    printf '  ✗ could not create tmux session %s\n' "$name" >&2
    return 1
}

# One line saying where a chat went: $1 title, $2 where, $3 `running` if it
# was open already.
__mn_opened() {
    if [ "$3" = running ]; then
        printf '  ▶ %s  (%s, already running)\n' "$1" "$2"
    else
        printf '  ▶ %s  (%s)\n' "$1" "$2"
    fi
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
    local tab=$'\t' us=$'\x1f'
    local line="${1//$tab/$us}"
    IFS=$us read -r "${@:2}" <<< "$line"
}

mn() {
    local mine=() fwd=() no_bypass=0 a take_value=0 report=0
    for a in "$@"; do
        if [ "$take_value" = 1 ]; then mine+=("$a"); take_value=0; continue; fi
        # Every flag mnemosyne has, sorted by what it does. The tags are read
        # by a test that holds these lists to the binary's own (src/main.rs);
        # a list kept by hand here fell behind, and `mn --stats` opened the
        # browser and passed --stats on to claude.
        case "$a" in
            -h|--help|-V|--version|--list|--json|--refresh|--stats|--update|--check-update|--write-config) mine+=("$a"); report=1 ;;  # flags: report
            --search) mine+=("$a"); report=1; take_value=1 ;;  # flags: report value
            --reopen|--subagents|--no-splash|--no-mouse|--no-model|--no-update) mine+=("$a") ;;  # flags: plan
            --search-mode|--restore) mine+=("$a"); take_value=1 ;;  # flags: plan value
            --ask|--no-bypass) no_bypass=1 ;;
            *) fwd+=("$a") ;;
        esac
    done

    # An answer rather than a choice: there is no plan to read back, so let
    # mnemosyne have the terminal and keep its exit status. Read as a plan,
    # "up to date on 0.4.14" came out as "folder gone, skipping:". Anything
    # that looked like claude's goes along too, so that a typo is refused
    # instead of quietly dropped.
    if [ "$report" = 1 ]; then
        command mnemosyne "${mine[@]}" "${fwd[@]}"
        return
    fi

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
    local finally="" tmux_first="" notes="" line mode cwd sid mdl prm ttl tmx term inner name state res flag
    # Errors from __mn_tmux_ensure, kept apart from the name it prints and
    # shown with everything else once the browser has closed.
    local errf; errf="$(mktemp)" || return 1
    # mnemosyne's own status, which a process substitution does not hand
    # back on its own. A refused flag has to fail here too, or
    # `mn --restore abc && ...` carries on regardless. It is written before
    # the pipe closes, so it is there by the time the loop sees the end.
    local stf; stf="$(mktemp)" || { rm -f "$errf"; return 1; }
    # Read on fd 3, not stdin, and give every command in the body /dev/null
    # for input. Otherwise the loop's stdin is the pipe and anything that
    # reads stdin -- tmux does -- swallows the next plan line, so the second
    # window never opens. It would take keystrokes from the browser too,
    # which is still running and reading the terminal.
    while IFS= read -r line <&3; do
        [ -z "$line" ] && continue
        __mn_split "$line" mode cwd sid mdl prm ttl tmx
        local extra=(); while IFS= read -r flag; do extra+=("$flag"); done < <(__mn_perms "$prm" "$no_bypass" "${fwd[@]}")
        local margs=(); [ -n "$mdl" ] && margs=(--model "$mdl")

        { case "$mode" in
            here)
                finally="$line"
                ;;
            tmux)
                res="$(__mn_tmux_ensure "$cwd" "$sid" "$mdl" "$ttl" "$tmx" -- "${extra[@]}" "${fwd[@]}" 2>>"$errf")" || continue
                { IFS= read -r name; IFS= read -r state; } <<< "$res"
                [ -z "$tmux_first" ] && tmux_first="$name"
                notes+="$(__mn_opened "$ttl" "tmux $name" "$state")"$'\n'
                ;;
            wintmux)
                [ -d "$cwd" ] || { notes+="  ✗ folder gone, skipping: $cwd"$'\n'; continue; }
                if command -v tmux >/dev/null 2>&1; then
                    res="$(__mn_tmux_ensure "$cwd" "$sid" "$mdl" "$ttl" "$tmx" -- "${extra[@]}" "${fwd[@]}" 2>>"$errf")" || continue
                    { IFS= read -r name; IFS= read -r state; } <<< "$res"
                    if term="$(__mn_term_open "$cwd" "exec tmux attach-session -t =$name")"; then
                        notes+="$(__mn_opened "$ttl" "$term → tmux $name" "$state")"$'\n'
                    else
                        notes+="$(__mn_opened "$ttl" "tmux $name" "$state") — no terminal to show it in; ctrl+t attaches"$'\n'
                    fi
                else
                    inner="cd $(printf %q "$cwd"); exec $(__mn_quote "$(__mn_claude)" --resume "$sid" "${margs[@]}" "${extra[@]}" "${fwd[@]}")"
                    term="$(__mn_term_open "$cwd" "$inner")" && notes+="  ▶ $ttl  ($term)"$'\n'
                fi
                ;;
            window)
                [ -d "$cwd" ] || { notes+="  ✗ folder gone, skipping: $cwd"$'\n'; continue; }
                inner="cd $(printf %q "$cwd"); exec $(__mn_quote "$(__mn_claude)" --resume "$sid" "${margs[@]}" "${extra[@]}" "${fwd[@]}")"
                if term="$(__mn_term_open "$cwd" "$inner")"; then
                    notes+="  ▶ $ttl  ($term)"$'\n'
                else
                    notes+="  ✗ no terminal emulator found (set \$MN_TERMINAL)"$'\n'
                fi
                ;;
            *)
                # Not a plan this wrapper knows how to carry out -- from a
                # newer mnemosyne, say. Show it; do not guess.
                notes+="$line"$'\n'
                ;;
        esac; } </dev/null
    done 3< <(mnemosyne "${mine[@]}"; echo "$?" > "$stf")

    local st; st="$(cat "$stf")"; rm -f "$stf"
    notes="$(cat "$errf")"$'\n'"$notes"; rm -f "$errf"
    printf '%s' "$notes" | grep -v '^$'
    if [ -n "$tmux_first" ]; then __mn_tmux_attach "$tmux_first"; return 0; fi
    [ -z "$finally" ] && return "${st:-0}"

    # Landing in this terminal: the cd has to happen here, which is the whole
    # reason this is a function.
    __mn_split "$finally" mode cwd sid mdl prm ttl tmx
    local extra=(); while IFS= read -r flag; do extra+=("$flag"); done < <(__mn_perms "$prm" "$no_bypass" "${fwd[@]}")
    local margs=(); [ -n "$mdl" ] && margs=(--model "$mdl")
    if [ -d "$cwd" ]; then cd "$cwd" || return 1
    else printf 'folder is gone: %s — resuming from %s\n' "$cwd" "$PWD"; fi
    printf '▶ %s\n' "$ttl"
    claude --resume "$sid" "${margs[@]}" "${extra[@]}" "${fwd[@]}"
}
