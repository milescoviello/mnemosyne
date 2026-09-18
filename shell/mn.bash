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

mn() {
    local mine=() fwd=() no_bypass=0 a take_value=0
    for a in "$@"; do
        if [ "$take_value" = 1 ]; then mine+=("$a"); take_value=0; continue; fi
        case "$a" in
            --no-splash|--subagents|--no-model) mine+=("$a") ;;
            # --restore takes a count, which is ours and not claude's
            --restore) mine+=("$a"); take_value=1 ;;
            --ask|--no-bypass) no_bypass=1 ;;
            *) fwd+=("$a") ;;
        esac
    done

    local plan
    plan="$(mnemosyne "${mine[@]}")" || return $?
    [ -z "$plan" ] && return 0

    local mode cwd sid mdl prm ttl
    IFS=$'\t' read -r mode cwd sid mdl prm ttl <<< "$(printf '%s\n' "$plan" | head -1)"

    if [ "$mode" = "here" ]; then
        local extra=(); mapfile -t extra < <(__mn_perms "$prm" "$no_bypass" "${fwd[@]}")
        if [ -d "$cwd" ]; then cd "$cwd" || return 1
        else echo "folder is gone: $cwd — resuming from $PWD"; fi
        local margs=(); [ -n "$mdl" ] && margs=(--model "$mdl")
        echo "▶ $ttl"
        claude --resume "$sid" "${margs[@]}" "${extra[@]}" "${fwd[@]}"
    else
        printf '%s\n' "$plan" | while IFS=$'\t' read -r mode cwd sid mdl prm ttl; do
            [ -d "$cwd" ] || { echo "  ✗ folder gone, skipping: $cwd"; continue; }
            local extra=(); mapfile -t extra < <(__mn_perms "$prm" "$no_bypass" "${fwd[@]}")
            local margs=(); [ -n "$mdl" ] && margs=(--model "$mdl")
            local inner="cd $(printf %q "$cwd"); exec claude --resume $sid ${margs[*]} ${extra[*]} ${fwd[*]}"
            for term in $MN_TERMINAL alacritty konsole kitty wezterm foot xterm; do
                command -v "$term" >/dev/null 2>&1 || continue
                case "$term" in
                    alacritty) "$term" --working-directory "$cwd" -e bash -lc "$inner" & ;;
                    konsole)   "$term" --workdir "$cwd" -e bash -lc "$inner" & ;;
                    kitty)     "$term" --directory "$cwd" bash -lc "$inner" & ;;
                    wezterm)   "$term" start --cwd "$cwd" -- bash -lc "$inner" & ;;
                    foot)      "$term" --working-directory="$cwd" bash -lc "$inner" & ;;
                    *)         "$term" -e bash -lc "$inner" & ;;
                esac
                disown 2>/dev/null
                echo "  ▶ $ttl  ($term)"
                break
            done
        done
    fi
}
