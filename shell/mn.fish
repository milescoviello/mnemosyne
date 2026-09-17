function mn --description 'Browse, search, tag and resume Claude Code sessions (mnemosyne)'
    # The TUI draws on stderr and prints its decision on stdout, so this
    # function can capture the choice while mnemosyne still owns the terminal.
    # The cd has to happen here: a child process cannot move its parent shell.
    set -l plan (mnemosyne $argv)
    or return $status
    test -z "$plan"; and return 0

    # default to skip-permissions like the old cs did, without duplicating it
    set -l extra --dangerously-skip-permissions
    if contains -- --dangerously-skip-permissions $argv
        set extra
    end

    set -l first (string split \t -- $plan[1])
    if test "$first[1]" = here
        set -l cwd $first[2]
        set -l sid $first[3]
        set -l mdl $first[4]
        set -l ttl $first[5]
        if test -d "$cwd"
            cd "$cwd"
        else
            echo "folder is gone: $cwd — resuming from "(pwd)
        end
        set -l margs
        test -n "$mdl"; and set margs --model $mdl
        echo "▶ $ttl"
        claude --resume $sid $margs $extra
    else
        for line in $plan
            set -l p (string split \t -- $line)
            __mn_window "$p[2]" "$p[3]" "$p[4]" "$p[5]" $extra
        end
    end
end

function __mn_window --description 'Open one resumed session in its own terminal window'
    set -l cwd $argv[1]
    set -l sid $argv[2]
    set -l mdl $argv[3]
    set -l ttl $argv[4]
    set -l extra $argv[5..-1]

    if not test -d "$cwd"
        echo "  ✗ folder gone, skipping: $cwd"
        return 1
    end
    set -l margs
    test -n "$mdl"; and set margs --model $mdl
    set -l inner "cd "(string escape -- $cwd)"; exec claude --resume $sid $margs $extra"

    for term in $MN_TERMINAL alacritty konsole kitty wezterm foot xterm
        test -z "$term"; and continue
        command -q $term; or continue
        switch $term
            case alacritty
                command $term --working-directory "$cwd" -e fish -lc "$inner" &
            case konsole
                command $term --workdir "$cwd" -e fish -lc "$inner" &
            case kitty
                command $term --directory "$cwd" fish -lc "$inner" &
            case wezterm
                command $term start --cwd "$cwd" -- fish -lc "$inner" &
            case foot
                command $term --working-directory="$cwd" fish -lc "$inner" &
            case '*'
                command $term -e fish -lc "$inner" &
        end
        disown
        echo "  ▶ $ttl  ($term)"
        return 0
    end
    echo "  ✗ no terminal emulator found (set \$MN_TERMINAL)"
    return 1
end
