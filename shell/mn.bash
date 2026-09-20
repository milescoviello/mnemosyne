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
    for term in $MN_TERMINAL alacritty konsole kitty wezterm foot ghostty xterm; do
        command -v "$term" >/dev/null 2>&1 || continue
        case "$term" in
            alacritty) "$term" --working-directory "$cwd" -e bash -lc "$inner" & ;;
            konsole)   "$term" --workdir "$cwd" -e bash -lc "$inner" & ;;
            kitty)     "$term" --directory "$cwd" bash -lc "$inner" & ;;
            wezterm)   "$term" start --cwd "$cwd" -- bash -lc "$inner" & ;;
            foot)      "$term" --working-directory="$cwd" bash -lc "$inner" & ;;
            ghostty)   "$term" --working-directory="$cwd" -e bash -lc "$inner" & ;;
            *)         "$term" -e bash -lc "$inner" & ;;
        esac
        disown 2>/dev/null
        printf '%s\n' "$term"
        return 0
    done

    # macOS has none of those. Terminal.app is told to run a script rather
    # than a command line, which keeps a shell command out of AppleScript
    # quoting entirely.
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

# Make sure a tmux session exists for this chat; print its name. Progress goes
# to stderr so the caller can capture just the name.
__mn_tmux_ensure() {
    local cwd="$1" sid="$2" mdl="$3" ttl="$4"; shift 4
    [ "$1" = "--" ] && shift
    command -v tmux >/dev/null 2>&1 || { echo "  ✗ tmux is not installed" >&2; return 1; }

    local name="mn-${sid:0:8}"
    # Already there: attach to it rather than starting a second client on the
    # same transcript. That is what resuming its latest state means.
    if tmux has-session -t "=$name" 2>/dev/null; then
        echo "  ▶ $name already running — resuming where it left off" >&2
        printf '%s\n' "$name"
        return 0
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

    local plan
    plan="$(mnemosyne "${mine[@]}")" || return $?
    [ -z "$plan" ] && return 0

    local mode cwd sid mdl prm ttl line
    __mn_split "$(printf '%s\n' "$plan" | head -1)" mode cwd sid mdl prm ttl

    if [ "$mode" = "here" ]; then
        local extra=(); mapfile -t extra < <(__mn_perms "$prm" "$no_bypass" "${fwd[@]}")
        if [ -d "$cwd" ]; then cd "$cwd" || return 1
        else echo "folder is gone: $cwd — resuming from $PWD"; fi
        local margs=(); [ -n "$mdl" ] && margs=(--model "$mdl")
        echo "▶ $ttl"
        claude --resume "$sid" "${margs[@]}" "${extra[@]}" "${fwd[@]}"
        return
    fi

    if [ "$mode" = "tmux" ]; then
        # Create every requested session detached first, then attach once —
        # attaching inside the loop would block on the first one.
        local target="" nm
        while IFS= read -r line; do
            __mn_split "$line" mode cwd sid mdl prm ttl
            local extra=(); mapfile -t extra < <(__mn_perms "$prm" "$no_bypass" "${fwd[@]}")
            nm="$(__mn_tmux_ensure "$cwd" "$sid" "$mdl" "$ttl" -- "${extra[@]}" "${fwd[@]}")" || continue
            [ -z "$target" ] && target="$nm"
        done < <(printf '%s\n' "$plan")
        [ -n "$target" ] && __mn_tmux_attach "$target"
        return
    fi

    # window, or wintmux: a window each, with tmux underneath so that closing
    # the window leaves the session running instead of killing it.
    local term inner name
    while IFS= read -r line; do
        __mn_split "$line" mode cwd sid mdl prm ttl
        [ -d "$cwd" ] || { echo "  ✗ folder gone, skipping: $cwd"; continue; }
        local extra=(); mapfile -t extra < <(__mn_perms "$prm" "$no_bypass" "${fwd[@]}")
        local margs=(); [ -n "$mdl" ] && margs=(--model "$mdl")

        if [ "$mode" = "wintmux" ] && command -v tmux >/dev/null 2>&1; then
            name="$(__mn_tmux_ensure "$cwd" "$sid" "$mdl" "$ttl" -- "${extra[@]}" "${fwd[@]}")" || continue
            if term="$(__mn_term_open "$cwd" "exec tmux attach-session -t =$name")"; then
                echo "  ▶ $ttl  ($term → tmux $name)"
            else
                echo "  ▶ $ttl  (tmux $name — no terminal to show it in; ctrl+t attaches)"
            fi
            continue
        fi

        inner="cd $(printf %q "$cwd"); exec claude --resume $sid ${margs[*]} ${extra[*]} ${fwd[*]}"
        if term="$(__mn_term_open "$cwd" "$inner")"; then
            echo "  ▶ $ttl  ($term)"
        else
            echo "  ✗ no terminal emulator found (set \$MN_TERMINAL)"
        fi
    done < <(printf '%s\n' "$plan")
}
