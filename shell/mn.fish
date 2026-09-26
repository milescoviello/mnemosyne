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
    set -l report 0
    for a in $argv
        if test "$take_value" = 1
            set -a mine $a
            set take_value 0
            continue
        end
        # `--restore` may stand alone, meaning five: the word after it is its
        # count only when it is not an option. Taken regardless, `--ask` went
        # as the count, and every session came back with its prompts off.
        if test "$take_value" = count
            set take_value 0
            if not string match -q -- '-*' $a
                set -a mine $a
                continue
            end
        end
        # Every flag mnemosyne has, sorted by what it does. The tags are read
        # by a test that holds these lists to the binary's own (src/main.rs);
        # a list kept by hand here fell behind, and `mn --stats` opened the
        # browser and passed --stats on to claude.
        switch $a
            case -h --help -V --version --list --json --refresh --stats --update --check-update --write-config  # flags: report
                set -a mine $a
                set report 1
            case --search  # flags: report value
                set -a mine $a
                set report 1
                set take_value 1
            case --reopen --subagents --no-splash --no-mouse --no-model --no-update  # flags: plan
                set -a mine $a
            case --search-mode  # flags: plan value
                set -a mine $a
                set take_value 1
            case --restore  # flags: plan value
                set -a mine $a
                set take_value count
            case --ask --no-bypass
                set no_bypass 1
            case '*'
                set -a fwd $a
        end
    end

    # An answer rather than a choice: there is no plan to read back, so let
    # mnemosyne have the terminal and keep its exit status. Read as a plan,
    # "up to date on 0.4.14" came out as "folder gone, skipping:". Anything
    # that looked like claude's goes along too, so that a typo is refused
    # instead of quietly dropped.
    if test $report -eq 1
        command mnemosyne $mine $fwd
        return
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
                    set -l res (__mn_tmux_ensure "$p[2]" "$p[3]" "$p[4]" "$p[6]" "$p[7]" -- $extra $fwd 2>&1)
                    if test $status -eq 0
                        test -z "$tmux_first"; and set tmux_first $res[-2]
                        set -a notes (__mn_opened "$p[6]" "tmux $res[-2]" $res[-1])
                    else
                        set -a notes $res
                    end
                case wintmux
                    set -a notes (__mn_wintmux "$p[2]" "$p[3]" "$p[4]" "$p[6]" "$p[7]" -- $extra $fwd 2>&1)
                case window
                    set -a notes (__mn_window "$p[2]" "$p[3]" "$p[4]" "$p[6]" -- $extra $fwd 2>&1)
                case '*'
                    # Not a plan this wrapper knows how to carry out -- from
                    # a newer mnemosyne, say. Show it; do not guess.
                    set -a notes $line
            end
        end </dev/null
    end
    # mnemosyne's own status, not the loop's: a refused flag has to fail
    # here too, or `mn --restore abc && ...` carries on regardless.
    set -l st $pipestatus[1]

    for n in $notes
        echo $n
    end
    if test -n "$tmux_first"
        __mn_tmux_attach $tmux_first
        return 0
    end
    test -z "$finally"; and return $st

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
    # Prints the name, then `new` or `running`, and nothing else: saying what
    # happened is the caller's job, once, in the notes it prints after the
    # browser has closed. Printing here as well said everything twice -- and
    # its stderr went straight onto the browser, which was still open.
    # Errors go to stderr, for the caller to collect.
    set -l cwd $argv[1]
    set -l sid $argv[2]
    set -l mdl $argv[3]
    set -l ttl $argv[4]
    # A name you chose, or empty for the generated one.
    set -l want $argv[5]
    # Everything after the separator goes to claude, and the separator is
    # always the sixth argument. Looking for the first `--` found a title,
    # model or name that was `--` instead, and claude's flags went in as a
    # prompt -- a bypass resumed with every prompt on.
    set -l extra
    test "$argv[6]" = --; and set extra $argv[7..-1]

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
        string split \t -- $running | head -1
        echo running
        return 0
    end
    # The name is taken, and not by this chat -- the check above would have
    # found it. Attaching anyway would drop you into someone else's session
    # claiming it was yours. It also happens whenever several chats are
    # opened under one chosen name, which used to collapse them all into a
    # single session running only the first.
    if tmux has-session -t "="$name 2>/dev/null
        set -l n 2
        while tmux has-session -t "="$name-$n 2>/dev/null
            set n (math $n + 1)
        end
        set name $name-$n
    end

    if not test -d "$cwd"
        echo "  ✗ folder gone, skipping: $cwd" >&2
        return 1
    end
    set -l margs
    test -n "$mdl"; and set margs --model $mdl
    set -l wname (string sub -l 18 -- (string replace -ra '[^a-zA-Z0-9._-]' '-' -- $ttl))
    test -z "$wname"; and set wname $name

    # Separate words, which tmux runs as they are. Given one string it hands
    # that to your default shell to parse -- whichever shell that is -- and
    # an argument with a space in it came out as two.
    if tmux new-session -d -s $name -n "$wname" -c "$cwd" (__mn_claude) --resume $sid $margs $extra 2>/dev/null
        echo $name
        echo new
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
    # Its errors are captured here, not left on stderr: this runs while the
    # browser is still on screen, and stderr is where the browser is drawn.
    set -l res (__mn_tmux_ensure $argv 2>&1)
    or begin
        printf '%s\n' $res
        return 1
    end
    set -l name $res[-2]
    set -l state $res[-1]
    set -l ttl $argv[4]
    # Quoted: the name is whatever the chat already runs under, and tmux
    # allows spaces and `(...)` in one.
    set -l term (__mn_term_open "$argv[1]" "exec tmux attach-session -t "(string escape -- "=$name"))
    or begin
        echo (__mn_opened "$ttl" "tmux $name" $state)" — no terminal to show it in; ctrl+t attaches"
        return 0
    end
    __mn_opened "$ttl" "$term → tmux $name" $state
end

function __mn_opened --description 'One line saying where a chat went'
    # title, where, and `running` if it was open already
    if test "$argv[3]" = running
        echo "  ▶ $argv[1]  ($argv[2], already running)"
    else
        echo "  ▶ $argv[1]  ($argv[2])"
    end
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
    # Either spelling: with an `=` it went unseen, and the recorded bypass
    # was added next to the plan mode you asked for.
    if contains -- --dangerously-skip-permissions $fwd; or contains -- --permission-mode $fwd
        or string match -q -- '--permission-mode=*' $fwd
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
        case ''
            # nothing recorded (older transcripts predate the field) — keep the
            # old cs behaviour rather than surprising anyone with prompts
            echo --dangerously-skip-permissions
        case '*'
            # A mode this wrapper does not know -- one a newer Claude Code
            # added, say. It used to fall in with "nothing recorded" and
            # resume with every prompt switched off. Prompts on is the answer
            # that is never a surprise.
            return 0
    end
end

function __mn_window --description 'Open one resumed session in its own terminal window'
    set -l cwd $argv[1]
    set -l sid $argv[2]
    set -l mdl $argv[3]
    set -l ttl $argv[4]
    # Everything after the separator is passed through to claude. It is
    # always the fifth argument: the first `--` could be the title.
    set -l extra
    test "$argv[5]" = --; and set extra $argv[6..-1]

    if not test -d "$cwd"
        echo "  ✗ folder gone, skipping: $cwd"
        return 1
    end
    set -l margs
    test -n "$mdl"; and set margs --model $mdl
    # Each word escaped on its own: pasted in bare, `--add-dir "/my projects"`
    # arrived as two arguments, and a `$(...)` inside one was run.
    set -l inner "cd "(string escape -- $cwd)"; exec "(string join ' ' -- (string escape -- (__mn_claude) --resume $sid $margs $extra))

    set -l term (__mn_term_open "$cwd" "$inner")
    or begin
        echo "  ✗ no terminal emulator found (set \$MN_TERMINAL)"
        return 1
    end
    echo "  ▶ $ttl  ($term)"
end

function __mn_claude --description 'The claude this shell would run, as a path'
    # A new window's login shell and a tmux server started from somewhere
    # else need not have the same PATH as you, and "claude: command not
    # found" in a window that then closes is a poor way to find that out.
    command -s claude; or echo claude
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
        # The script is read by /bin/sh, and what this function is handed is
        # quoted for fish: `string escape` writes `'John\'s'`, which sh cannot
        # read, so a folder with an apostrophe never opened. sh is given only
        # words it can read, and the command goes to fish as one of them.
        printf '#!/bin/sh\nrm -f %s\ncd %s\nexec fish -lc %s\n' (__mn_sh_quote $tmp) (__mn_sh_quote $cwd) (__mn_sh_quote "$inner") >$tmp
        chmod +x $tmp
        osascript -e "tell application \"Terminal\" to do script \"$tmp\"" \
            -e 'tell application "Terminal" to activate' >/dev/null
        echo Terminal.app
        return 0
    end
    return 1
end

function __mn_sh_quote --description 'One word, quoted for /bin/sh'
    printf "'%s'" (string replace -a "'" "'\\''" -- $argv[1] | string collect)
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
