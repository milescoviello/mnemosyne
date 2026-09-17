# bash/zsh equivalent of mn.fish — source this from ~/.bashrc
# The cd must happen in the shell, hence a function rather than a script.
mn() {
    local plan
    plan="$(mnemosyne "$@")" || return $?
    [ -z "$plan" ] && return 0

    local extra=(--dangerously-skip-permissions)
    for a in "$@"; do
        [ "$a" = "--dangerously-skip-permissions" ] && extra=()
    done

    local mode cwd sid mdl ttl
    IFS=$'\t' read -r mode cwd sid mdl ttl <<< "$(printf '%s\n' "$plan" | head -1)"

    if [ "$mode" = "here" ]; then
        if [ -d "$cwd" ]; then cd "$cwd" || return 1
        else echo "folder is gone: $cwd — resuming from $PWD"; fi
        local margs=(); [ -n "$mdl" ] && margs=(--model "$mdl")
        echo "▶ $ttl"
        claude --resume "$sid" "${margs[@]}" "${extra[@]}"
    else
        printf '%s\n' "$plan" | while IFS=$'\t' read -r mode cwd sid mdl ttl; do
            [ -d "$cwd" ] || { echo "  ✗ folder gone, skipping: $cwd"; continue; }
            local margs=(); [ -n "$mdl" ] && margs=(--model "$mdl")
            local inner="cd $(printf %q "$cwd"); exec claude --resume $sid ${margs[*]} ${extra[*]}"
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
