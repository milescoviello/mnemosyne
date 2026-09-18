function claude-live --description 'Show Claude Code sessions running RIGHT NOW (folder + uptime)'
    set -l pids (pgrep -x claude)
    if test -z "$pids"
        echo "No Claude Code sessions currently running."
        return
    end
    printf '%-8s  %-34s  %s\n' PID FOLDER UPTIME
    printf '%-8s  %-34s  %s\n' ------- --------------------------------- ----------
    for pid in $pids
        set -l cwd (readlink /proc/$pid/cwd 2>/dev/null; or echo '?')
        set -l et (ps -o etime= -p $pid 2>/dev/null | string trim)
        printf '%-8s  %-34s  %s\n' $pid $cwd $et
    end
    echo
    echo (count $pids)" live session(s).  After a reboot: 'claude-restore "(count $pids)"' reopens them all, 'cs' picks one."
end
