function claude-restore --description 'Reopen the N most recent Claude Code sessions, each in its own window'
    # One-shot recovery after a reboot: the N most recently active sessions
    # each come back in their own alacritty, cd'd to the right folder and
    # already resumed. Recency is the proxy for "what I had open" — Claude
    # doesn't hold its transcript open, so there's no live pid→session map.
    # Sessions already running are skipped, so re-running is safe.
    argparse n/dry-run h/help -- $argv
    or return

    if set -q _flag_help
        echo "claude-restore [N] [-n|--dry-run]     (default N = 5)"
        echo "  reopens the N most recently used sessions, newest first"
        return 0
    end

    set -l count 5
    if set -q argv[1]
        if string match -qr '^[0-9]+$' -- $argv[1]
            set count $argv[1]
        else
            echo "usage: claude-restore [N] [-n]"
            return 1
        end
    end

    set -l rows (claude-session-index list | head -n $count)
    if test (count $rows) -eq 0
        echo "No saved sessions found."
        return 1
    end

    # session ids already up, so a second run doesn't duplicate windows
    set -l running
    for pid in (pgrep -x claude)
        set -l cmd (tr '\0' ' ' <"/proc/$pid/cmdline" 2>/dev/null)
        set -l id (string match -rg -- '--resume[= ]([0-9a-f-]{36})' -- "$cmd")
        test -n "$id"; and set -a running $id
    end

    set -l opened 0
    set -l skipped 0
    for row in $rows
        set -l parts (string split \t -- $row)
        set -l label (string trim -- $parts[1])
        set -l sid $parts[3]
        set -l cwd $parts[4]

        if contains -- $sid $running
            set skipped (math $skipped + 1)
            continue
        end
        if not test -d "$cwd"
            echo "  ✗ folder gone, skipping: $cwd"
            continue
        end

        if set -q _flag_dry_run
            echo "  → $label"
        else
            echo "  ▶ $label"
            alacritty --working-directory "$cwd" \
                -e fish -C "claude --resume $sid --dangerously-skip-permissions" \
                >/dev/null 2>&1 &
            disown
            set opened (math $opened + 1)
            sleep 0.4
        end
    end

    if set -q _flag_dry_run
        echo "(dry run — $skipped of $count already running)"
    else
        echo "opened $opened window(s), skipped $skipped already running."
    end
end
