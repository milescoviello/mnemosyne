# Retired tools

Kept for reference, not installed by `install.sh`.

| file | replaced by |
|---|---|
| `cs-classic.fish` + `claude-session-index` | `mn` — the original fzf picker, which read 60 MB on every run and could not search conversations |
| `claude-live.fish` | `mn` shows running sessions inline with a `●` marker, and `L` filters to them |
| `claude-restore.fish` | `mn --restore N`, which also skips sessions already running or already in tmux |

`cs-classic` still works with nothing but `fzf` and `python3`, so it is worth
keeping around as a fallback if the binary is ever missing or broken:

    ./legacy/install-classic.sh
