# mnemosyne

A terminal browser for your Claude Code history. Every session you have ever
run, across every folder, in one searchable list — with favourites, tags,
full-text search inside the conversations, and one keypress to reattach.

Claude Code writes every session to `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`
and nothing is lost on reboot. But `claude --resume` only lists sessions for the
directory you happen to be standing in, so there is no way to see the whole
picture. `mnemosyne` is that missing view.

```
  ⌇ mnemosyne                                                    330 sessions · ★12 · ●6 live · recency

         AGE  FOLDER              TITLE                        LEFT OFF                    MODEL     MSGS  TAGS
  ≈ today  ~~≈≈≈~~∼∼∼∼~~≈≈≈~~      ~~~~        ~~~~
  ▌ ❯ ●   6s  ~/proj              fix the auth flow            the redirect drops state    opus-5    6.3k
  ▌      26s  ~/proj         ⌁27  migrate the schema           run it against staging      opus-5     412  #api
  ▌       4m  ~                   update the deps              check the lockfile diff     opus-5    1.2k
  ≈ yesterday  ~~≈≈≈~~∼∼∼∼~~≈≈≈~~      ~~~~
  ▌ ★    1d   ~/site              redesign the landing page    make the hero smaller       opus-5    8.1k
  ▌       1d  ~/notes             tidy the vault               merge the daily notes       sonnet-5  3.4k

  ~~∼∼   ∼∼~≈≈≈~~~∼ ∼~~~≈≈~~       ~~~         ≈≈
  fix the auth flow                    ~/proj · main · 686K · 187 entries · 35m · bypass · tmux mn-a3f21c04

  claude    Found it — the callback rebuilds the URL and loses the query string…
  you       does that break the mobile flow too?

  ↑↓ move   ↵ resume   / filter   F search   ^t tmux   f ★   t tag   s sort   ? keys                    1/330
```

## The look

One idea runs through it: **the pool**. Mnemosyne is the spring of memory, so
the list is a water surface and older sessions sink.

A **depth gutter** runs down the left edge, coloured on the water ramp by each
session's age — today's are pale foam at the surface, last year's fade into
deep indigo. Age becomes something you see rather than read, and the age text
sits on the same ramp. Subagents hang off their parent on a thinner `│`.

**Date bands are ripples**, not rules, and they dissolve toward the right
instead of stretching a hard line across the terminal. They appear only when
sorting by recency, where time order makes them mean something.

The **wordmark is lit letter by letter** along the same ramp, and every piece
of chrome — cursor `❯`, band `≈`, prompt `⌇`, selection `◆` — comes from one
small water alphabet. No borders anywhere; structure comes from alignment.

At a wide terminal a title column alone leaves a sixty-column void in every
row, so the leftover space becomes a **LEFT OFF** column carrying the last
thing you said. When a session has no AI title its opening prompt becomes the
title, and that is often also the last prompt, so the duplicate is suppressed
rather than printed twice. Below about 80 columns the column is dropped and
the rail carries the cue instead.

## Mouse

It is a pointer-driven list as much as a keyboard one.

| | |
|---|---|
| click a row | select it |
| click it again | resume it |
| right-click | favourite it |
| wheel | scroll |
| click a column heading | sort by that column |
| click a `⌁n` count | open that session's subagents |
| `M` | mouse off, so the terminal can select text again |

## Tags

Tags are the organising axis, not folders — most sessions tend to share one
working directory, so the folder column is nearly constant while tags are not.

`t` tags the session under the cursor, or **every session in the selection**
if you have picked several with `space`. Inside that prompt, `-name` removes a
tag and `old>new` renames one everywhere it appears, merging if the
destination already exists. `T` filters to a single tag, with completion over
the tags you already use.

Everything clickable records its screen span as it draws, and the mouse
handler tests against what was actually rendered — so the hit areas cannot
drift out of step with the layout. Clicks and keys both dispatch through one
`Action` enum, so the two can never disagree about what a command does.

Mouse reporting takes over the terminal's own text selection, which is why
`M` (or `--no-mouse`) exists. In most terminals holding shift while dragging
also bypasses it.


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

```sh
curl -fsSL https://raw.githubusercontent.com/milescoviello/mnemosyne/main/install.sh | bash
```

That fetches a prebuilt static binary (checksum verified), installs it to
`~/.local/bin/mnemosyne`, and adds the `mn` shell function. No Rust needed.

From a clone, or to build it yourself:

```sh
git clone https://github.com/milescoviello/mnemosyne
cd mnemosyne
./install.sh            # prebuilt if available, otherwise builds
./install.sh --build    # always build from source
```

Then:

```sh
mn
```

The shell function exists because `mnemosyne` cannot change your shell's
working directory — no child process can. The binary draws the interface on
**stderr** and prints its decision to **stdout**; the function reads that and
performs the `cd` plus `claude --resume` itself. It is a few lines long and
you can read all of it in `shell/mn.fish`.

## Searching

`/` filters the list. `F` searches *inside* the conversations.

Content search runs against a full-text index, so it answers in
**milliseconds** rather than re-reading the corpus:

| query over 1,084 transcripts | indexed | exhaustive scan |
|---|---|---|
| `checkpatch` | 0.002 s | 0.195 s |
| `page fault` | 0.006 s | 0.69 s |
| `the` | 0.03 s | 4.5 s |

The index is small because a transcript is mostly not prose. Measured over a
random sample here, **2.2%** of 2.5 GB is anything a person said — the rest is
tool output, base64 and JSON. So the index holds what was said, thought, and
run (text blocks, thinking blocks, and shell commands) and comes to about a
tenth of the corpus.

That is a deliberate trade: it does not cover tool *output*, which is the
other 98%. `m` cycles the search between **content** (indexed), **file
touched**, **tool used**, and **everything** — the last reads every byte and
is the slow, complete one. A content search that finds nothing falls back to
the exhaustive scan automatically rather than claiming there is nothing
there.

Two exclusions make the results worth trusting. Memory files and
`<system-reminder>` blocks are injected into every session, so indexing them
would make every session match any word in yours; they are stripped. And
base64 image payloads spell short words by chance, so the exhaustive scan
rejects matches inside them.

## Non-interactive use## Non-interactive use

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
mnemosyne --restore 5                 # reopen the 5 most recent, each in a window
```

`--restore` skips anything already running or already in a tmux session, and
anything whose directory has since been deleted.

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

## Tokens

Transcripts record their own token usage, so `mn` reads it. The header
carries the machine-wide total, there is a **TOKENS** column at wide
terminals that `s` will sort by, and each session's own count sits in the
rail:

```
  ⌇ mnemosyne                      331 sessions · 90.72b tokens · ●2 live · recency
```

The header total is deliberately **not** filtered — it is a property of the
corpus rather than of whatever you are looking at, and a figure that moved
while you typed would be hard to read. Narrowing to one tag changes the
session count beside it, not the total.

As the terminal narrows the header drops whole facts rather than cutting one
in half, worst-first: the sort label goes, then the token total, then the live
count. Anything explaining *why* the list looks the way it does — an active
filter, a tag, a selection — outlives them, since without it the view is
inexplicable.

The same numbers, broken down:

```
$ mnemosyne --stats
tokens, every session on this machine
  input            19.8m
  output          334.7m
  cache read      89.03b
  cache write      1.30b
  total           90.68b
```

Counts only, and only local transcripts. Turning usage into money, and doing
it across more than one machine, is what `ccusage` is for.

## Configuration

Entirely optional — with no file the defaults are what the tool has always
used. `mnemosyne --write-config` drops a commented one at
`~/.claude/mnemosyne/config.toml`:

```toml
# The water ramp, deep to pale. Every gradient samples it.
ramp = ["#0e2042", "#154884", "#1a7aa8", "#26b2b0", "#6ce2d6", "#e2f8f6"]
accent = "cyan"
favorite = "yellow"

[splash]
enabled = true
floor_warm_ms = 420     # how long it lingers when there was no work to cover

[start]
mouse = true
preview = true
```

Command-line flags always beat the file. A malformed config is reported once
and then ignored — losing your colours is not a reason to refuse to start.

## Failure modes

The index is derived data, so it is treated that way. A corrupt database is
deleted and rebuilt rather than reported; if the location cannot be written to
at all — read-only home, full disk — it falls back to an in-memory index,
which is slower but works. Neither case stops the tool starting, and both used
to.

Your favourites, tags and notes are the only thing here that cannot be
rebuilt, so `meta.json` is written atomically and keeps three generations
behind it (`meta.json.1` … `.3`). The rotation skips identical saves, so
toggling one favourite repeatedly cannot push real history out of the window.

Live-session detection reads `/proc`, so it only works on Linux. Elsewhere it
says so rather than reporting that nothing is running, which looks identical
to a broken feature.

## legacy/

`legacy/` holds the tools this replaced — the original `fzf` + Python picker,
and the two small fish functions for listing and reopening sessions. See
`legacy/README.md`. The picker needs nothing but `fzf` and `python3`, so it is
worth keeping as a fallback if the binary is ever missing:

    ./legacy/install-classic.sh

## Development

```sh
cargo test          # 36 tests, no network or fixtures on disk
cargo clippy --all-targets -- -D warnings
python3 tools/gen-wordmark.py     # regenerate the logo (needs Pillow)
python3 tools/tui-drive.py '["./target/release/mnemosyne","--no-splash"]' '["DOWN","v"]'
```

`tools/tui-drive.py` runs the interface in a pseudo-terminal and rebuilds what
it drew, so the TUI can be exercised in CI or from a script. It speaks
synthetic mouse events too, and `CLICKFIND:<glyph>` locates a glyph and clicks
it in the same pass — finding coordinates in one run and clicking in another
races against a transcript corpus that is being appended to while you look at
it.

The tests deliberately pin the findings that were expensive to discover: that
a line cannot be classified by its first `"type":"`, that injected context and
base64 have to be excluded from search, that permission mode is read from the
start of a session, and that a scanner change must invalidate the cache.

## License

MIT.
