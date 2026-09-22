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
            case --no-splash --subagents --no-model --no-update --update --check-update --write-config --reopen
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

    # Read the plan as it is produced, not after the browser exits.
    # A session opened in a window of its own does not need the picker to
    # close first, so mnemosyne hands those over while it is still running
    # and this loop acts on each one as it arrives. Only `here` and `tmux`
    # need this terminal, and those arrive last, on the way out.
    set -l finally ""
    set -l tmux_first ""
    # Progress is collected, not printed. The browser is still on screen
    # while these run, and writing over it is what made it look like mn had
    # half-exited. It all comes out once the screen is ours again.
    set -l notes
    mnemosyne $mine | while read -l line
        test -z "$line"; and continue
        set -l p (string split \t -- $line)
        # Everything in here gets /dev/null for input. Inside a `while read`
        # the loop's stdin *is* the pipe, so any command that reads stdin
        # swallows the next plan line -- tmux does exactly that, and the
        # second window never opened. It would take keystrokes from the
        # browser too, which is still running and reading the terminal.
        begin
            set -l extra (__mn_perms "$p[5]" $no_bypass $fwd)
            switch $p[1]
                case here
                    set finally $line
                case tmux
                    # Create each detached as it arrives; attach once, after.
                    set -l nm (__mn_tmux_ensure "$p[2]" "$p[3]" "$p[4]" "$p[6]" "$p[7]" -- $extra $fwd)
                    test -z "$tmux_first"; and set tmux_first $nm
                case wintmux
                    set -a notes (__mn_wintmux "$p[2]" "$p[3]" "$p[4]" "$p[6]" "$p[7]" -- $extra $fwd 2>&1)
                case '*'
                    set -a notes (__mn_window "$p[2]" "$p[3]" "$p[4]" "$p[6]" -- $extra $fwd 2>&1)
            end
        end </dev/null
    end

    for n in $notes
        echo $n
    end
    if test -n "$tmux_first"
        __mn_tmux_attach $tmux_first
        return 0
    end
    test -z "$finally"; and return 0

    # Landing in this terminal: the cd has to happen here, which is the whole
    # reason this is a function.
    set -l p (string split \t -- $finally)
    set -l cwd $p[2]
    set -l sid $p[3]
    set -l mdl $p[4]
    set -l ttl $p[6]
    set -l extra (__mn_perms "$p[5]" $no_bypass $fwd)
    if test -d "$cwd"
        cd "$cwd"
    else
        echo "folder is gone: $cwd — resuming from "(pwd)
    end
    set -l margs
    test -n "$mdl"; and set margs --model $mdl
    echo "▶ $ttl"
    claude --resume $sid $margs $extra $fwd
end

function __mn_tmux_ensure --description 'Make sure a tmux session exists for this chat; echo its name'
    # Progress messages go to stderr so the caller can capture just the name.
    set -l cwd $argv[1]
    set -l sid $argv[2]
    set -l mdl $argv[3]
    set -l ttl $argv[4]
    # A name you chose, or empty for the generated one.
    set -l want $argv[5]
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
    test -n "$want"; and set name $want

    # Already there, whatever it ended up called: attach rather than starting
    # a second client on the same transcript. tmux remembers the command each
    # pane was started with, so the chat is found by its session id and not
    # by a name that is now yours to choose.
    set -l running (tmux list-panes -a -F '#{session_name}	#{pane_start_command}' 2>/dev/null \
        | string match -r '^[^\t]+\t.*--resume[ =]'$sid'.*$' | head -1)
    if test -n "$running"
        set -l have (string split \t -- $running)[1]
        echo "  ▶ $have already running — resuming where it left off" >&2
        echo $have
        return 0
    end
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

function __mn_wintmux --description 'Open a resumed session in its own window, running under tmux'
    # Without tmux this is just a window, which is the next best thing rather
    # than an error: the session still opens.
    if not command -q tmux
        # __mn_window does not take the tmux name, so drop it
        __mn_window $argv[1..4] $argv[6..-1]
        return $status
    end
    set -l name (__mn_tmux_ensure $argv)
    or return 1
    set -l ttl $argv[4]
    set -l term (__mn_term_open "$argv[1]" "exec tmux attach-session -t ="$name)
    or begin
        echo "  ▶ $ttl  (tmux $name — no terminal to show it in; ctrl+t attaches)"
        return 0
    end
    echo "  ▶ $ttl  ($term → tmux $name)"
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

    set -l term (__mn_term_open "$cwd" "$inner")
    or begin
        echo "  ✗ no terminal emulator found (set \$MN_TERMINAL)"
        return 1
    end
    echo "  ▶ $ttl  ($term)"
end

function __mn_term_open --description 'Run a command in a new terminal window; echo the terminal used'
    set -l cwd $argv[1]
    set -l inner $argv[2]

    set -l cmd
    for term in $MN_TERMINAL alacritty konsole kitty wezterm foot ghostty xterm
        test -z "$term"; and continue
        command -q $term; or continue
        switch $term
            case alacritty
                set cmd $term --working-directory "$cwd" -e fish -lc "$inner"
            case konsole
                set cmd $term --workdir "$cwd" -e fish -lc "$inner"
            case kitty
                set cmd $term --directory "$cwd" fish -lc "$inner"
            case wezterm
                set cmd $term start --cwd "$cwd" -- fish -lc "$inner"
            case foot
                set cmd $term --working-directory="$cwd" fish -lc "$inner"
            case ghostty
                set cmd $term --working-directory="$cwd" -e fish -lc "$inner"
            case '*'
                set cmd $term -e fish -lc "$inner"
        end
        __mn_spawn $cmd
        echo $term
        return 0
    end

    # macOS has none of those. Terminal.app is told to run a script rather
    # than a command line, which keeps a shell command out of AppleScript
    # quoting entirely -- and it is spawned by the system, so it is already
    # detached from this shell.
    if command -q osascript
        set -l tmp (mktemp -t mn-open)
        printf '#!/bin/sh\nrm -f %s\ncd %s\n%s\n' (string escape -- $tmp) (string escape -- $cwd) "$inner" >$tmp
        chmod +x $tmp
        osascript -e "tell application \"Terminal\" to do script \"$tmp\"" \
            -e 'tell application "Terminal" to activate' >/dev/null
        echo Terminal.app
        return 0
    end
    return 1
end

function __mn_spawn --description 'Start a window that outlives the terminal that asked for it'
    # `disown` only removes the job from this shell's table -- the child keeps
    # our process group and session, so closing the window running `mn` sends
    # it SIGHUP and every window we just opened disappears with it. setsid
    # gives it a session of its own. `-f` always forks, which matters because
    # a backgrounded job is already a group leader and plain setsid would
    # refuse.
    if command -q setsid
        command setsid -f $argv >/dev/null 2>&1
    else
        command $argv >/dev/null 2>&1 &
        disown
    end
end
