function mn --description 'Browse, search, tag and resume Claude Code sessions (mnemosyne)'
    # mnemosyne draws on stderr and prints its decision on stdout, so this
    # function can capture the choice while the TUI still owns the terminal.
    # The cd has to happen here: a child process cannot move its parent shell.

    # Split our own flags from ones meant for `claude`, so that neither tool
    # is handed an option it does not understand.
    set -l mine
    set -l fwd
    set -l no_bypass 0
    set -l take_value 0
    for a in $argv
        if test $take_value -eq 1
            set -a mine $a
            set take_value 0
            continue
        end
        switch $a
            case --no-splash --subagents --no-model
                set -a mine $a
            case --restore
                # takes a count, which belongs to mnemosyne and not to claude
                set -a mine $a
                set take_value 1
            case --ask --no-bypass
                set no_bypass 1
            case '*'
                set -a fwd $a
        end
    end

    set -l plan (mnemosyne $mine)
    or return $status
    test -z "$plan"; and return 0


    set -l first (string split \t -- $plan[1])
    if test "$first[1]" = here
        set -l cwd $first[2]
        set -l sid $first[3]
        set -l mdl $first[4]
        set -l prm $first[5]
        set -l ttl $first[6]
        set -l extra (__mn_perms "$prm" $no_bypass $fwd)
        if test -d "$cwd"
            cd "$cwd"
        else
            echo "folder is gone: $cwd — resuming from "(pwd)
        end
        set -l margs
        test -n "$mdl"; and set margs --model $mdl
        echo "▶ $ttl"
        claude --resume $sid $margs $extra $fwd
    else if test "$first[1]" = tmux
        # Create every requested session detached first, then attach once --
        # attaching inside the loop would block on the first one.
        set -l target
        for line in $plan
            set -l p (string split \t -- $line)
            set -l extra (__mn_perms "$p[5]" $no_bypass $fwd)
            set -l nm (__mn_tmux_ensure "$p[2]" "$p[3]" "$p[4]" "$p[6]" -- $extra $fwd)
            test -z "$target"; and set target $nm
        end
        test -n "$target"; and __mn_tmux_attach $target
    else
        for line in $plan
            set -l p (string split \t -- $line)
            set -l extra (__mn_perms "$p[5]" $no_bypass $fwd)
            __mn_window "$p[2]" "$p[3]" "$p[4]" "$p[6]" -- $extra $fwd
        end
    end
end

function __mn_tmux_ensure --description 'Make sure a tmux session exists for this chat; echo its name'
    # Progress messages go to stderr so the caller can capture just the name.
    set -l cwd $argv[1]
    set -l sid $argv[2]
    set -l mdl $argv[3]
    set -l ttl $argv[4]
    set -l sep (contains -i -- -- $argv)
    set -l extra
    if test -n "$sep"; and test (count $argv) -gt $sep
        set extra $argv[(math $sep + 1)..-1]
    end

    if not command -q tmux
        echo "  ✗ tmux is not installed" >&2
        return 1
    end
    set -l name "mn-"(string sub -l 8 -- $sid)

    # Already there: attach to it rather than starting a second client on the
    # same transcript. That is what resuming its latest state means.
    if tmux has-session -t "="$name 2>/dev/null
        echo "  ▶ $name already running — resuming where it left off" >&2
        echo $name
        return 0
    end

    if not test -d "$cwd"
        echo "  ✗ folder gone, skipping: $cwd" >&2
        return 1
    end
    set -l margs
    test -n "$mdl"; and set margs --model $mdl
    set -l wname (string sub -l 18 -- (string replace -ra '[^a-zA-Z0-9._-]' '-' -- $ttl))
    test -z "$wname"; and set wname $name

    if tmux new-session -d -s $name -n "$wname" -c "$cwd" "exec claude --resume $sid $margs $extra" 2>/dev/null
        echo "  ▶ $ttl  (tmux $name)" >&2
        echo $name
        return 0
    end
    echo "  ✗ could not create tmux session $name" >&2
    return 1
end

function __mn_tmux_attach --description 'Attach to a tmux session, from inside or outside tmux'
    # attach-session fails when already inside tmux; switch-client is the
    # in-tmux equivalent.
    if set -q TMUX
        tmux switch-client -t "="$argv[1]
    else
        tmux attach-session -t "="$argv[1]
    end
end

function __mn_perms --description 'Map a session\'s recorded permission mode to claude flags'
    # Resume a session under the mode it was started in. An explicit flag on
    # the command line always wins; --ask forces normal prompts.
    set -l mode $argv[1]
    set -l no_bypass $argv[2]
    set -l fwd $argv[3..-1]

    if test "$no_bypass" = 1
        return 0
    end
    if contains -- --dangerously-skip-permissions $fwd; or contains -- --permission-mode $fwd
        return 0
    end

    switch $mode
        case bypassPermissions
            echo --dangerously-skip-permissions
        case plan acceptEdits auto manual dontAsk
            # "default" is deliberately absent: it is not a valid value for
            # --permission-mode, and it means "just behave normally" anyway.
            echo --permission-mode
            echo $mode
        case default
            # started with prompts on, so resume with prompts on
            return 0
        case '*'
            # nothing recorded (older transcripts predate the field) — keep the
            # old cs behaviour rather than surprising anyone with prompts
            echo --dangerously-skip-permissions
    end
end

function __mn_window --description 'Open one resumed session in its own terminal window'
    set -l cwd $argv[1]
    set -l sid $argv[2]
    set -l mdl $argv[3]
    set -l ttl $argv[4]
    # everything after the -- separator is passed through to claude
    set -l sep (contains -i -- -- $argv)
    set -l extra
    if test -n "$sep"; and test (count $argv) -gt $sep
        set extra $argv[(math $sep + 1)..-1]
    end

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
