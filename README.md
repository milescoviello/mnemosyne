# mnemosyne

A terminal browser for your Claude Code history. Every session you have ever
run, across every folder, in one searchable list — with favourites, tags,
full-text search inside the conversations, and one keypress to reattach.

Claude Code writes every session to `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`
and nothing is lost on reboot. But `claude --resume` only lists sessions for the
directory you happen to be standing in, so there is no way to see the whole
picture. `mnemosyne` is that missing view.

```
  mnemosyne                                328 sessions · ★12 · 6 live · recency

       AGE  FOLDER           TITLE                              MODEL    MSGS  TAGS
   →    3s  ~                fix the auth flow                  opus-5   6.3k
       26s  ~/proj           add rate limiting                  opus-5    412  #api
    ●  30s  ~/proj           migrate the schema                 opus-5   1.2k
    ★   4m  ~/site       ⌁27 redesign the landing page          opus-5   8.1k
        5h  ~/notes          tidy the vault                     sonnet-5  3.4k

  ───────────────────────────────────────────────────────────────────────────────
  fix the auth flow                  ~/proj · main · 686K · 187 entries · 35m

  left off   the redirect still drops the state param
  claude     Found it — the callback rebuilds the URL and loses the query…

  ↑↓ move   enter resume   / filter   F search   f ★   t tag   s sort   ? keys  1/328
```

No borders, aligned columns, one footer line. The full key list lives behind
`?` rather than permanently on screen.


## What it does

**Finds things.** `/` fuzzy-filters titles, folders, branches and tags. `F`
searches *inside* the conversations — the actual text of what you and Claude
said — across the whole corpus in about a fifth of a second. `m` switches that
between three questions: what was *said*, which *files* a session actually
edited, and which *tools* it used.

**Remembers what matters to you.** `f` favourites a session and pins it to the
top. `t` tags it; `T` filters to one tag. `N` attaches a private note. All of
this lives in one small JSON file, separate from the disposable index.

**Shows you the real titles.** Claude Code generates a title for each session
and records it in the transcript. Most tools ignore it and fall back to the
first user message, which turns a pasted multi-paragraph prompt into a useless
list entry. `mnemosyne` uses the real title, plus the *last* prompt — the best
single cue for "where was I".

**Knows what's already running.** Live sessions are marked, and pressing enter
on one tells you its pid instead of silently attaching a second client to the
same transcript.

**Restores the model.** If a session ran on a specific `--model`, resuming
brings it back on that model rather than quietly dropping to the default.

**Surfaces subagents.** Subagent transcripts live in a nested directory and are
normally invisible. `a` reveals them; `→` expands a session's children.

## Install

Needs a Rust toolchain, and `fish` or `bash`.

```sh
git clone https://github.com/milescoviello/mnemosyne
cd mnemosyne
./install.sh
```

That builds the binary to `~/.local/bin/mnemosyne` and installs the `mn` shell
function. Then just:

```sh
mn
```

The shell function exists because `mnemosyne` cannot change your shell's
working directory — no child process can. So the binary draws the interface on
**stderr** and prints its decision to **stdout**, and the function reads that
and performs the `cd` plus `claude --resume` itself. It is a few lines long and
you can read all of it in `shell/mn.fish`.

## The opening animation

The name resolves out of noise while the index is being built, so the animation
is covering real work rather than stalling on purpose:

```
                            m n e m ~ y 7 0 %
                             ────────
                          recalling 1078 of 1080
                          ██████████████████████
                              any key to skip
```

It runs until indexing finishes or about a second has passed, whichever is
later. Any key skips straight to the list, and that keypress is swallowed so it
cannot act on the session under the cursor. When there is genuine work left the
bar reports it; on a warm index the bar fills with the reveal and the counts
below state the real totals. The bar never steps backwards when the source
changes under it.

Turn it off with `--no-splash` or `MNEMOSYNE_NO_SPLASH=1`.

## Non-interactive use

Handy from scripts, and from inside a Claude session that wants to find its own
past work.

```sh
mnemosyne --list                      # TSV of every session
mnemosyne --json                      # same, as JSON
mnemosyne --search "connection reset" # which sessions discussed this
mnemosyne --search Cargo.toml --search-mode file   # which sessions edited it
mnemosyne --search WebSearch --search-mode tool    # which sessions used it
mnemosyne --stats                     # corpus summary
mnemosyne --refresh                   # rebuild the index and exit
```

## How it stays fast

The corpus this was built against is 2.5 GB across ~1,100 transcripts, with
individual sessions over 400 MB. Measured on that corpus:

| operation | time |
|---|---|
| first index, cold page cache | 0.50 s |
| re-index, nothing changed | 0.02 s |
| full-text search, all sessions | ~0.2 s |
| preview any session | constant, regardless of size |

Four things get it there:

**No JSON parsing in the hot path.** A byte-level test classifies each line and
only the handful that actually carry a title or a prompt are handed to a real
JSON parser.

**The right byte-level test.** Looking for the first `"type":"` in a line is
wrong: message lines embed a nested `"type":"text"` content block *before* their
own top-level `type`, which misclassifies about half of all lines. Metadata
lines instead begin literally with `{"type":"`, and message lines carry
`"role":"user"` / `"role":"assistant"` near their front. Those two tests
together were verified exact over thousands of lines — no misses, no false
positives.

**Append-only incremental scanning.** Transcripts only ever grow, so the index
records how many bytes it has consumed and later reads just the new tail. An
untouched session costs one `stat()`.

**Previews read backwards.** Showing the last few messages of a 400 MB session
means seeking to the end, not reading 400 MB. Preview cost is independent of
session size.

## Search precision

Naive substring search over raw transcripts is nearly useless, for two reasons
that are easy to miss:

- **Injected context.** Memory files and `<system-reminder>` blocks are pasted
  into every session's context. Search for any word that appears in yours and
  you match *every session you have ever run*. Matches inside injected spans,
  and inside `attachment` records, are excluded.
- **Base64.** Pasted images arrive as long unbroken base64, whose alphabet
  cheerfully spells short words by chance. Prose contains whitespace and blobs
  do not, so matches in a wide window with no whitespace are rejected.

Together these cut a representative query from 58 hits to 36 real ones.

## Keys

Arrow keys, `enter`, `esc` and `tab` are the documented path and are always on
screen; `?` shows everything. Vim motions (`j k g G h l`) work as silent
aliases if you want them, and are never required.

## Files it touches

| path | what |
|---|---|
| `~/.claude/projects/**/*.jsonl` | read only, never modified |
| `~/.claude/mnemosyne/index.db` | disposable cache; delete it any time |
| `~/.claude/mnemosyne/meta.json` | your favourites, tags and notes |

Only `meta.json` cannot be regenerated, so it is written via a temp file and
rename and kept deliberately small and readable.

## Retention warning

Claude Code deletes local transcripts after `cleanupPeriodDays`, which
**defaults to 30 days** by last activity and runs at startup. If you want a
long history to browse, raise it in `~/.claude/settings.json`:

```json
{ "cleanupPeriodDays": 3650 }
```

There is no literal "never". Already-deleted transcripts are unrecoverable.

## legacy/

`legacy/` holds the small `fzf` + Python picker this replaced, kept because it
has no dependencies beyond `fzf` and `python3` and so still works if the binary
is missing. Install it as `cs-classic` if you want the simple version around.

## License

MIT.
