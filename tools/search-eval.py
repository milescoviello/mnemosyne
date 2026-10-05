#!/usr/bin/env python3
"""Measure how well `--search` finds a session you are thinking of.

    python3 tools/search-eval.py [path-to-binary ...]

There is no list of right answers for a search over somebody's own history,
but there is one kind of question whose answer is known: a session you
remember something about. So this asks, for every session in the corpus
under $HOME, for a few of its most distinctive words, and looks at where
that session comes in the answer:

  title    two of the rarest words of the session's title. The title is
           written by Claude, often in other words than were used: this is
           the question asked in your own words.
  prompt   two of the rarest words of what you first asked in it: the
           question asked in the session's words.

For each it prints how often the session was found at all, how often it was
first, in the top five, and the mean reciprocal rank -- 1 for first, 1/2
for second, 0 for missing -- averaged over every session.

Nothing is printed about any session; only the totals. It reads the index
the binary builds under $HOME, so run the binary once first.
"""
import collections, os, re, sqlite3, subprocess, sys

HOME = os.environ["HOME"]
DB = os.path.join(HOME, ".claude/mnemosyne/index.db")
STOP = set(
    "the a an and or of to in on for with from by at is are be it this that as "
    "into via not no its using use new fix add make get set up out about how "
    "why what when which over after before between more less can you your "
    "please just like want need have has was were will would should could "
    "there here then them they these those some any all also our my me i".split()
)


def corpus():
    c = sqlite3.connect(DB)
    c.execute(
        "CREATE VIRTUAL TABLE IF NOT EXISTS temp.v USING fts5vocab(main, body, 'row')"
    )
    df = dict(c.execute("SELECT term, doc FROM temp.v"))
    rows = c.execute(
        "SELECT s.id, s.custom_title, s.ai_title, s.first_prompt FROM sessions s "
        "JOIN body_ref r ON r.path = s.path WHERE s.is_subagent = 0"
    ).fetchall()
    return df, rows


def rarest(text, df, k=2):
    words = []
    for w in re.findall(r"[a-z0-9]+", text.lower()):
        if len(w) >= 3 and w not in STOP and df.get(w, 0) > 1 and w not in words:
            words.append(w)
    if len(words) < k:
        return None
    keep = sorted(words, key=lambda w: df[w])[:k]
    return " ".join(w for w in words if w in keep)


def ranks(binary, questions):
    out = []
    for q, want in questions:
        r = subprocess.run(
            [binary, "--search", q],
            capture_output=True,
            text=True,
            env={**os.environ, "MNEMOSYNE_NO_UPDATE": "1"},
        )
        ids = [line.split("\t")[3] for line in r.stdout.splitlines() if line.count("\t") >= 4]
        out.append(ids.index(want) + 1 if want in ids else None)
    return out


def report(name, got):
    n = len(got)
    at = lambda k: sum(1 for r in got if r and r <= k) / n
    found = sum(1 for r in got if r) / n
    mrr = sum(1 / r for r in got if r) / n
    print(f"  {name:8} n={n:<4} found {found:4.0%}  first {at(1):4.0%}  "
          f"top 5 {at(5):4.0%}  MRR {mrr:.3f}")


def main():
    binaries = sys.argv[1:] or ["mnemosyne"]
    df, rows = corpus()
    sets = collections.OrderedDict()
    sets["title"] = [
        (q, sid) for sid, ct, at, _ in rows if (q := rarest(ct or at, df))
    ]
    sets["prompt"] = [(q, sid) for sid, _, _, fp in rows if (q := rarest(fp, df))]
    for b in binaries:
        print(b)
        for name, qs in sets.items():
            report(name, ranks(b, qs))


if __name__ == "__main__":
    main()
