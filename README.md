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

## Permissions

A session resumes under the permission mode it was **started** in, read from
the transcript, the same way the model is restored:

| recorded mode | resumed with |
|---|---|
| `bypassPermissions` | `--dangerously-skip-permissions` |
| `plan`, `acceptEdits`, `auto`, `manual`, `dontAsk` | `--permission-mode <mode>` |
| `default` | nothing — it started with prompts on, so prompts stay on |
| *nothing recorded* | `--dangerously-skip-permissions` |

`default` is deliberately absent from the `--permission-mode` column: it is not
one of that flag's accepted values, and it already means "behave normally".
The last row covers older transcripts written before the field existed.

Overrides, both of which win over the recorded mode:

```sh
mn --ask     # resume with permission prompts on, whatever it was started in
mn --dangerously-skip-permissions   # force bypass
```

Anything `mn` does not recognise is forwarded to `claude` untouched, so
`mn --verbose` works. `mn`'s own flags (`--no-splash`, `--subagents`,
`--no-model`, `--ask`) are filtered out before the rest is handed over.

## tmux

`ctrl+t` resumes a session inside tmux, in a session named `mn-<first 8 of the
id>`:

```
  ▶ linux-abi-self-hosting  (tmux mn-026bcdb5)
```

If that tmux session already exists, `ctrl+t` **attaches to it** rather than
starting a second client on the same transcript — so you pick up its latest
state instead of forking it:

```
  ▶ mn-026bcdb5 already running — resuming where it left off
```

The browser knows about this too. A session with a tmux session waiting shows
it in the rail (`tmux mn-026bcdb5`), and pressing `enter` on one refuses and
points you at `ctrl+t` instead. Attaching works from inside tmux as well —
`switch-client` is used there, since `attach-session` cannot nest.

The `mn-` prefix is namespaced and matched exactly (`-t =name`), so tmux
sessions you created yourself are never touched. Selecting several sessions
and pressing `ctrl+t` creates them all detached, then attaches to the first.

## The opening animation

Mnemosyne is the spring of memory in the underworld — the counter-pool to
Lethe, which souls drank in order to forget. So the wordmark surfaces out of a
rippling pool, lit by a gradient running from deep water to pale foam with a
shimmer band that leads the reveal and then keeps sweeping:

```
   ███╗   ███╗███╗   ██╗███████╗███╗   ███╗ ██████╗ ███████╗██╗   ██╗███╗   ██╗███████╗
   ████╗ ████║████╗  ██║██╔════╝████╗ ████║██╔═══██╗██╔════╝╚██╗ ██╔╝████╗  ██║██╔════╝
   ██╔████╔██║██╔██╗ ██║█████╗  ██╔████╔██║██║   ██║███████╗ ╚████╔╝ ██╔██╗ ██║█████╗
   ██║╚██╔╝██║██║╚██╗██║██╔══╝  ██║╚██╔╝██║██║   ██║╚════██║  ╚██╔╝  ██║╚██╗██║██╔══╝
   ██║ ╚═╝ ██║██║ ╚████║███████╗██║ ╚═╝ ██║╚██████╔╝███████║   ██║   ██║ ╚████║███████╗
   ╚═╝     ╚═╝╚═╝  ╚═══╝╚══════╝╚═╝     ╚═╝ ╚═════╝ ╚══════╝   ╚═╝   ╚═══╝  ╚═══╝╚══════╝
      ∼∼~~~~∼∼    ∼~~≈≈~~∼∼∼∼~~≈≈≈≈~∼∼  ∼∼~~~~∼∼    ∼∼~~~~~∼∼∼∼∼~≈≈≈≈~~∼∼∼∼∼~~~~~∼
   ~~~~∼∼   ∼~~≈≈≈~~∼∼∼∼~~≈≈≈~~∼    ∼~~~~∼∼   ∼∼~~≈≈~~∼∼∼∼~~≈≈≈~~∼∼  ∼∼~~~~∼∼    ∼~~~~~

                            1080 transcripts · 2.4G
                ████████████████████████████████████████████████
                                any key to skip
```

Columns that have not surfaced yet show only the wave crests, so the
unrevealed half reads as open water rather than a wall of glyphs. The pool
uses two summed sine frequencies, because a single one falls into long uniform
runs that look like teeth. Roughly 3,500 distinct colours are in play per
frame, from `rgb(10,18,33)` to `rgb(239,251,250)`.

Below about 90 columns it falls back to spaced letters resolving out of noise,
with the same gradient and pool.

It is covering real work — the index builds on a background thread while this
runs — and it lasts until indexing finishes or about 1.5s has passed, whichever
is later. Any key skips straight to the list, and that keypress is swallowed so
it cannot act on the session under the cursor. When there is genuine work left
the bar reports it; on a warm index the bar fills with the reveal and the counts
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
