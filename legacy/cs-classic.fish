function cs-classic --description 'The original fzf session picker, kept as-is'
    # Cross-folder session picker. Lists every saved Claude Code session
    # (newest first), previews the conversation, and on Enter jumps into
    # that session's folder and resumes it. Resumes with
    # --dangerously-skip-permissions by default; extra args pass through.
    set -l line (claude-session-index list | fzf \
        --ansi --delimiter=\t --with-nth=1 \
        --preview 'claude-session-index preview {2}' \
        --preview-window='right,55%,wrap' \
        --header 'Enter = resume in its folder   |   Esc = cancel' \
        --prompt 'claude session > ')
    or return
    test -z "$line"; and return

    set -l parts (string split \t -- $line)
    set -l sid $parts[3]
    set -l cwd $parts[4]

    if test -d "$cwd"
        cd "$cwd"
    else
        echo "Folder no longer exists: $cwd — resuming from here."
    end
    # default to skip-permissions, but don't duplicate if the user passed it
    set -l flags --dangerously-skip-permissions
    if contains -- --dangerously-skip-permissions $argv
        set flags
    end

    echo "▶ resuming $sid  in  "(pwd)
    claude --resume $sid $flags $argv
end

# Frozen on 2026-09-17 when mnemosyne took over the `cs` name.
# This is the original 32-line fzf picker: one fzf call, a python backend that
# reads the head of every transcript on every run, and no state of its own.
# Kept because it has no dependencies beyond fzf and python, so it still works
# if the Rust binary is missing or broken.
