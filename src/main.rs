//! mnemosyne — browse, search, tag and resume Claude Code sessions.
//!
//! The interface is drawn on stderr and the chosen action is printed on stdout,
//! so a shell wrapper can capture the decision with a command substitution
//! while the TUI still owns the terminal. That wrapper exists because changing
//! the calling shell's working directory is something only the shell can do.

mod app;
mod art;
mod config;
mod index;
mod live;
mod meta;
mod model;
mod preview;
mod scan;
mod search;
mod splash;
mod ui;
mod update;
mod workspace;
mod wsx;

use anyhow::Result;
use app::{App, Outcome};
use crossterm::event::{self, Event};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{stderr, Write};
use std::time::{Duration, Instant};

const HELP: &str = "\
mnemosyne — browse, search, tag and resume Claude Code sessions

usage: mnemosyne [options]

  (no options)     open the interactive browser
  --list           print a TSV of every session and exit
  --json           print every session as JSON and exit
  --refresh        rebuild the index cache and exit
  --stats          show corpus statistics and exit

  --search TEXT    non-interactively find sessions whose conversations
                   contain TEXT, print matches as TSV, and exit
  --search-mode M  content (default, indexed) | file | tool | everything
                   the last one also reads tool output, which the index
                   leaves out: slower, but complete

  --restore N      reopen the N most recent sessions, each in its own window
                   under tmux, skipping any already running
  --reopen         put back the sessions that were open before the machine
                   last rebooted, each in a window backed by tmux

  --subagents      include subagent transcripts (in --list and --json they
                   are listed under their parent; in the browser, revealed
                   so → can expand one)
  --no-splash      skip the opening animation (or set MNEMOSYNE_NO_SPLASH=1)
  --no-mouse       start with mouse reporting off (toggle in-app with M)
  --write-config   write a commented config file and exit

  --update         install the latest release now, and exit
  --check-update   say whether a newer release exists, and exit
  --no-update      skip the background update check this run
  --no-model       do not restore each session's original --model
  -h, --help       this text
  -V, --version    version

Which sessions are open is recorded every time the browser runs, so that a
reboot can be undone. After one, the browser offers to put them back.

On exit the browser prints the chosen action to stdout as TSV:
  <here|window|tmux|wintmux>\\t<cwd>\\t<session-id>\\t<model>
      \\t<permission-mode>\\t<title>\\t<tmux-session-name, may be empty>
The shell function (`mn`) turns that into a cd plus `claude --resume`, or into
a tmux attach. It restores the model and the permission mode the session
started in; pass --ask to resume with prompts on instead.
";

/// Write one line of the plan, or explain why it cannot be written.
///
/// The plan is tab-separated and the shell splits it back apart positionally,
/// so a tab anywhere inside a field silently shifts every field after it: the
/// session id becomes half a path, and resuming fails with a message about
/// something unrelated. A folder can legally contain a tab on Unix. Titles
/// cannot -- the scanner collapses all whitespace -- but they are checked too
/// rather than trusted, since that is one refactor away from being untrue.
#[allow(clippy::too_many_arguments)]
fn plan_line(
    out: &mut impl Write,
    mode: &str,
    cwd: &str,
    id: &str,
    model: &str,
    perms: &str,
    title: &str,
    tmux: &str,
) -> Result<bool> {
    let fields = [cwd, id, model, perms, title, tmux];
    // \x1f too: mn.bash splits on it, having swapped the tabs for it so that
    // an empty field survives `read`.
    if let Some(bad) = fields.iter().find(|f| f.contains(['\t', '\n', '\x1f'])) {
        eprintln!(
            "skipping {id}: a tab or newline in {bad:?} cannot be carried \
             by the plan the shell reads"
        );
        return Ok(false);
    }
    writeln!(
        out,
        "{mode}\t{cwd}\t{id}\t{model}\t{perms}\t{title}\t{tmux}"
    )?;
    Ok(true)
}

/// Every flag, sorted by what the shell wrappers have to do with it.
///
/// `mn` sits between you and this binary and decides, flag by flag, whether
/// an option is ours or `claude`'s, and whether to read a plan back at all.
/// It used to keep its own list, which fell behind: `mn --stats` opened the
/// browser and later handed `--stats` to claude, and `mn --check-update`
/// printed "folder gone" because its answer was read as a plan line. The
/// wrappers now tag their lists, and a test holds them to these.
///
/// Answer and exit. No browser, and nothing on stdout for a shell to act on,
/// so the wrapper hands these to the binary and gets out of the way.
const REPORT_FLAGS: &[&str] = &[
    "-h",
    "--help",
    "-V",
    "--version",
    "--list",
    "--json",
    "--refresh",
    "--stats",
    "--update",
    "--check-update",
    "--write-config",
];
/// ...and the one of those that takes a value.
const REPORT_VALUE_FLAGS: &[&str] = &["--search"];
/// Shape the browser, or skip it, but still end in a plan.
const PLAN_FLAGS: &[&str] = &[
    "--reopen",
    "--subagents",
    "--no-splash",
    "--no-mouse",
    "--no-model",
    "--no-update",
];
/// ...and the ones of those that take a value. `--search-mode` is here
/// rather than with `--search` because on its own it answers nothing.
const PLAN_VALUE_FLAGS: &[&str] = &["--search-mode", "--restore"];

fn takes_value(a: &str) -> bool {
    REPORT_VALUE_FLAGS.contains(&a) || PLAN_VALUE_FLAGS.contains(&a)
}

fn is_flag(a: &str) -> bool {
    REPORT_FLAGS.contains(&a) || PLAN_FLAGS.contains(&a) || takes_value(a)
}

/// Refuse what we do not understand.
///
/// A mistyped flag used to be ignored, which opened the browser as though
/// nothing had happened -- the one outcome that looks like success. Values
/// were no better: `--restore abc` quietly restored five.
fn check_args(args: &[String]) -> std::result::Result<(), String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if takes_value(a) {
            i += 2;
            continue;
        }
        if !is_flag(a) {
            return Err(format!(
                "unknown option {a:?}\ntry --help for the ones that exist"
            ));
        }
        i += 1;
    }

    if args.iter().any(|a| a == "--search-mode") && !args.iter().any(|a| a == "--search") {
        return Err(
            "--search-mode picks how --search looks; it needs a --search to go with".into(),
        );
    }
    if let Some(v) = value_of(args, "--search-mode") {
        if !matches!(v, "content" | "file" | "tool" | "everything" | "all") {
            return Err(format!(
                "unknown --search-mode {v:?}\nit is one of: content, file, tool, everything"
            ));
        }
    }
    if let Some(v) = value_of(args, "--restore") {
        match v.parse::<usize>() {
            Ok(0) => return Err("--restore 0 would reopen nothing".into()),
            Ok(_) => {}
            Err(_) => return Err(format!("--restore wants a count, not {v:?}")),
        }
    }
    Ok(())
}

/// Sessions for `--restore N` to take from, newest first.
///
/// The "N most recent", as the help says -- not the list's order, which
/// floats favourites to the top, so an old favourite was reopened ahead of
/// what you were working on an hour ago. Nor a conversation a live wsx
/// workspace will put back itself when it starts: restored here too, it
/// ran twice.
fn most_recent_first(app: &App) -> Vec<&model::Session> {
    let mut v: Vec<&model::Session> = app
        .all
        .iter()
        .filter(|s| !s.is_subagent && !app.belongs_to_wsx(s))
        .collect();
    v.sort_by_key(|s| std::cmp::Reverse(s.mtime));
    v
}

/// The flags on the command line, leaving out the value of one that takes a
/// value. Looking for a flag anywhere in the arguments found it in a value
/// too: `--search --update` installed an update, and `--search --help`
/// printed the help, instead of searching for those words.
fn flags_given(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        out.push(a);
        i += if takes_value(a) { 2 } else { 1 };
    }
    out
}

/// The argument after `flag`, unless that is itself a flag.
fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let i = args.iter().position(|a| a == flag)?;
    let v = args.get(i + 1)?.as_str();
    if v.starts_with("--") {
        None
    } else {
        Some(v)
    }
}

fn main() -> Result<()> {
    // Rust ignores SIGPIPE, so a closed pipe surfaces as a write error and
    // `println!` panics. `mnemosyne --stats | head` printing a backtrace is
    // not how a command-line tool should behave; restore the default and let
    // the process die quietly like every other one in the pipeline.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let given = flags_given(&args);
    let has = |f: &str| given.contains(&f);

    if has("-h") || has("--help") {
        print!("{HELP}");
        return Ok(());
    }
    if let Err(e) = check_args(&args) {
        eprintln!("{e}");
        std::process::exit(2);
    }
    if has("-V") || has("--version") {
        println!("mnemosyne {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let include_subagents = true; // always indexed; visibility is a UI toggle
    let restore_model = !has("--no-model");

    if has("--update") {
        println!("current {}", update::current());
        // An answer that it failed has to fail, as every report flag's does:
        // `mn --update && …` carried on after a refused checksum.
        match update::install_latest() {
            Ok(v) => println!("updated to {v} — it takes effect next time you start"),
            Err(e) if e.is::<update::UpToDate>() => println!("{e}"),
            Err(e) => {
                println!("not updated: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if has("--check-update") {
        match update::latest_tag() {
            Some(tag) if update::is_newer(&tag, update::current()) => {
                println!("{} is available; you have {}", tag, update::current())
            }
            Some(tag) => println!("up to date on {} (latest is {tag})", update::current()),
            None => {
                println!("could not reach GitHub");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    if has("--write-config") {
        let p = config::path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        if p.exists() {
            println!("{} already exists — leaving it alone", p.display());
        } else {
            std::fs::write(&p, config::EXAMPLE)?;
            println!("wrote {}", p.display());
        }
        return Ok(());
    }

    if has("--refresh") {
        let t = Instant::now();
        let s = index::refresh(include_subagents)?;
        println!(
            "indexed {} transcripts ({} subagent) in {:.2}s",
            s.len(),
            s.iter().filter(|x| x.is_subagent).count(),
            t.elapsed().as_secs_f64()
        );
        return Ok(());
    }

    // The rest of the report flags have answered and returned by now.
    let interactive = !given
        .iter()
        .any(|a| REPORT_FLAGS.contains(a) || REPORT_VALUE_FLAGS.contains(a));
    // --restore and --reopen are not reports, but they draw nothing either:
    // no splash runs for them, so no rescan behind it, and the cache they
    // started from was all they ever saw. A first run restored nothing and
    // said nothing; later ones missed every session since the last browser.
    let answers_and_exits = has("--restore") || has("--reopen");
    // Read now, before anything takes the screen: whatever it has to say
    // about the file lands where you can read it.
    let cfg = config::get();
    let use_splash = interactive
        && !answers_and_exits
        && cfg.splash.enabled
        && !has("--no-splash")
        && std::env::var_os("MNEMOSYNE_NO_SPLASH").is_none();

    // With the splash on, the real scan happens on a thread behind the
    // animation, so the bar reports actual work instead of finishing before
    // the first frame. Start from the cache, which costs one query.
    let sessions = if use_splash {
        index::Index::open()?
            .load()?
            .into_values()
            .collect::<Vec<_>>()
    } else {
        index::refresh(include_subagents)?
    };
    // Nothing cached means the list has nothing to draw, so the opening
    // animation has to cover the scan. Otherwise it does not: the cached
    // rows go up straight away and the rescan lands underneath them.
    let cold_start = sessions.is_empty();

    if has("--stats") {
        let main: Vec<_> = sessions.iter().filter(|s| !s.is_subagent).collect();
        let bytes: u64 = sessions.iter().map(|s| s.size).sum();
        println!("transcripts     {}", main.len());
        println!("subagents       {}", sessions.len() - main.len());
        println!("total size      {}", model::human_size(bytes));
        println!(
            "with ai title   {}",
            main.iter().filter(|s| !s.ai_title.is_empty()).count()
        );
        println!(
            "with git branch {}",
            main.iter().filter(|s| !s.git_branch.is_empty()).count()
        );
        // Two numbers, because they are two different things and reporting
        // only the second made this disagree with the header and the list.
        // A claude with no --resume on its command line cannot be tied to a
        // transcript, so it is running without being a session we can name.
        let lm = live::live_map();
        let named = {
            let app = App::new(
                sessions.clone(),
                meta::Meta::load(),
                live::live_map(),
                false,
            );
            app.live_shown()
        };
        println!("running now     {named}");
        if lm.count != named {
            println!("claude processes {}", lm.count);
        }
        let m = meta::Meta::load();
        // Counted against the sessions that exist, so this agrees with the
        // list rather than with a file that outlives it.
        let known: std::collections::HashSet<String> =
            sessions.iter().map(|s| s.id.clone()).collect();
        let sum = m.summary(&known);
        println!("favourites      {}", sum.favourites);
        println!("tags            {}", sum.tags);
        if sum.orphans > 0 {
            println!(
                "orphaned marks  {} (favourites, tags or notes on transcripts that are gone)",
                sum.orphans
            );
        }
        if let Ok(i) = index::Index::open() {
            println!("indexed bodies  {}", i.text_rows().unwrap_or(0));
        }

        // Tokens read straight off the usage records in the transcripts.
        // Counts only, and only this machine -- `claude-spend` already turns
        // usage into money, and across the fleet.
        let (mut tin, mut tout, mut tcr, mut tcw) = (0u64, 0u64, 0u64, 0u64);
        for s in &sessions {
            tin += s.in_tokens;
            tout += s.out_tokens;
            tcr += s.cache_read;
            tcw += s.cache_write;
        }
        let total = tin + tout + tcr + tcw;
        if total > 0 {
            println!();
            println!("tokens, every session on this machine");
            println!("  input        {:>9}", model::human_count(tin));
            println!("  output       {:>9}", model::human_count(tout));
            println!("  cache read   {:>9}", model::human_count(tcr));
            println!("  cache write  {:>9}", model::human_count(tcw));
            println!("  total        {:>9}", model::human_count(total));
        }
        return Ok(());
    }

    // scriptable deep search: useful from a shell, or from inside a Claude
    // session that wants to find its own past work.
    if let Some(i) = args.iter().position(|a| a == "--search") {
        let Some(q) = args.get(i + 1) else {
            eprintln!("--search needs a query");
            std::process::exit(2);
        };
        let mode = match args
            .iter()
            .position(|a| a == "--search-mode")
            .and_then(|j| args.get(j + 1))
            .map(|s| s.as_str())
        {
            Some("file") => search::Mode::File,
            Some("tool") => search::Mode::Tool,
            Some("everything") | Some("all") => search::Mode::Everything,
            // anything else was rejected by check_args
            _ => search::Mode::Content,
        };
        // Always search everything, including subagents. What is *listed*
        // depends on the flag, but a match hiding in a child should never
        // make the session it belongs to invisible.
        let pool: Vec<model::Session> = sessions.to_vec();
        let t = Instant::now();
        let (mut hits, how) = search::run(&pool, q, mode);
        // The indexed path returns paths and fetches excerpts per visible
        // row, which is right for the browser and wrong here: this prints
        // every hit once and then exits, so the column that says *why* a
        // session matched came out empty on the default search.
        // Only when there is one to fill: a hit the prose lookup found came
        // with its excerpt, and for `c++` this pass would match nearly every
        // document just to throw the answer away.
        if how == search::How::Indexed && hits.values().any(|s| s.is_empty()) {
            let expr = search::fts_expr(q);
            if let Ok(idx) = index::Index::open() {
                if let Ok(all) = idx.excerpts(&expr, q) {
                    for (path, snip) in hits.iter_mut() {
                        if snip.is_empty() {
                            if let Some(s) = all.get(path) {
                                snip.clone_from(s);
                            }
                        }
                    }
                }
            }
        }
        // A subagent is not something you resume; its parent is. Without
        // --subagents the children are not listed, so the sessions they
        // belong to stand in for them -- the same rule the browser uses.
        let parents = search::parents_of_hits(&pool, &hits);
        let show_subs = has("--subagents");
        let mut rows: Vec<&model::Session> = pool
            .iter()
            .filter(|s| {
                if s.is_subagent {
                    return show_subs && hits.contains_key(&s.path.to_string_lossy().to_string());
                }
                hits.contains_key(&s.path.to_string_lossy().to_string()) || parents.contains(&s.id)
            })
            .collect();
        rows.sort_by_key(|s| std::cmp::Reverse(s.mtime));
        for s in &rows {
            println!(
                "{:>4}\t{}\t{}\t{}\t{}",
                model::reltime(s.mtime),
                model::short_cwd(&s.cwd),
                s.title(),
                s.id,
                hits.get(&s.path.to_string_lossy().to_string())
                    .map(|x| x.as_str())
                    .unwrap_or("")
            );
        }
        eprintln!(
            "{} of {} sessions matched \"{}\" ({}, {}) in {:.3}s",
            rows.len(),
            pool.len(),
            q,
            mode.label(),
            if how == search::How::Indexed {
                "indexed"
            } else {
                "scanned"
            },
            t.elapsed().as_secs_f64()
        );
        return Ok(());
    }

    // Reopen the N most recent sessions without opening the picker. Recency
    // is the proxy for "what I had open": Claude holds no handle on its
    // transcript, so there is no general pid-to-session map to consult.
    if let Some(i) = args.iter().position(|a| a == "--restore") {
        let n: usize = args.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(5);
        let live = live::live_map();
        let mut app = App::new(sessions, meta::Meta::load(), live, restore_model);
        app.set_wsx(wsx::load());
        let mut out = std::io::stdout().lock();
        let mut opened = 0;
        for s in most_recent_first(&app) {
            if opened >= n {
                break;
            }
            // already up, so reopening would just duplicate the window
            if s.live_exact || s.has_tmux {
                continue;
            }
            if s.cwd.is_empty() || !std::path::Path::new(&s.cwd).is_dir() {
                continue;
            }
            // Same landing as the post-reboot offer: a window each, with
            // tmux underneath, so closing one leaves the session running.
            // An older shell wrapper treats this as a plain window.
            let model = if restore_model { s.model.as_str() } else { "" };
            if plan_line(
                &mut out,
                "wintmux",
                &s.cwd,
                &s.id,
                model,
                &s.permission_mode,
                s.title(),
                "",
            )? {
                opened += 1;
            }
        }
        return Ok(());
    }

    // Put back what the last reboot took. The offer is normally made in the
    // browser; this is the same thing without opening it, for a login script
    // or for when it has already been waved away.
    if has("--reopen") {
        let live = live::live_map();
        let mut app = App::new(sessions, meta::Meta::load(), live, restore_model);
        // Which running claudes are wsx's agents -- neither recorded nor
        // offered, since wsx puts them back -- is wsx's to say.
        app.set_wsx(wsx::load());
        let boot = workspace::boot_id();
        // Fold first: on the first run after a reboot the set from before it
        // is still filed under "current", and rolling it over is what makes
        // it available to offer.
        let w = workspace::fold(
            workspace::load(),
            app.open_sessions(),
            &boot,
            live::detection_supported(),
        );
        let _ = workspace::save(&w);

        let running = app.running_ids();
        // A dismissed offer is included here: asking for this by name is a
        // clear enough statement of intent.
        let pending = workspace::pending(&w, &boot, true, &|id| running.contains(id));
        if pending.is_empty() {
            eprintln!(
                "nothing to reopen — no sessions were recorded as open before the last reboot"
            );
            return Ok(());
        }
        let mut out = std::io::stdout().lock();
        for e in &pending {
            plan_line(
                &mut out,
                "wintmux",
                &e.cwd,
                &e.id,
                if restore_model { &e.model } else { "" },
                &e.perms,
                &e.title,
                "",
            )?;
        }
        // Taken, so never offered again; what was just opened becomes the
        // current set instead.
        let mut after = workspace::load();
        after.previous = None;
        let _ = workspace::save(&after);
        // What is open now is what was just reopened *and* what was running
        // already. Recording only the first, as the whole truth, forgot the
        // rest: a second reboot before the next browser would not offer them.
        let mut open = app.open_sessions();
        for e in pending {
            if !open.iter().any(|o| o.id == e.id) {
                open.push(e);
            }
        }
        workspace::record(open, live::detection_supported());
        return Ok(());
    }

    if has("--list") || has("--json") {
        let mut app = App::new(
            sessions,
            meta::Meta::load(),
            live::live_map(),
            restore_model,
        );
        app.show_subagents = has("--subagents");
        if app.show_subagents {
            // Nothing here can expand a parent, so revealing subagents has
            // to mean showing them. Without this the flag changed nothing.
            app.expand_all();
        }
        if has("--json") {
            app.set_wsx(wsx::load());
        }
        app.rebuild();
        if has("--json") {
            let mut out = Vec::new();
            for r in &app.view {
                if let app::Row::Item(i) | app::Row::Sub(i) = r {
                    let s = &app.all[*i];
                    out.push(serde_json::json!({
                        "id": s.id,
                        "path": s.path.to_string_lossy(),
                        "cwd": s.cwd,
                        "title": s.title(),
                        "last_prompt": s.last_prompt,
                        "branch": s.git_branch,
                        "model": s.model,
                        "mtime": s.mtime,
                        "size": s.size,
                        "entries": s.entries,
                        "favorite": s.favorite,
                        "tags": s.tags,
                        "note": s.note,
                        "live_pid": s.live_pid,
                        "subagents": s.subagent_count,
                        "is_subagent": s.is_subagent,
                        "wsx": s.wsx.as_ref().map(|w| serde_json::json!({
                            "repo": w.repo,
                            "slug": w.slug,
                            "tag": w.tag(),
                            "state": w.status.word(),
                            "worktree": match &w.status {
                                wsx::Status::Live { worktree } => Some(worktree),
                                _ => None,
                            },
                            "checkout": match &w.status {
                                wsx::Status::Archived { checkout } => checkout.as_ref(),
                                _ => None,
                            },
                        })),
                    }));
                }
            }
            println!("{}", serde_json::to_string_pretty(&out)?);
        } else {
            for r in &app.view {
                if let app::Row::Item(i) | app::Row::Sub(i) = r {
                    let s = &app.all[*i];
                    println!(
                        "{:>4} {:<24.24} {}\t{}\t{}\t{}",
                        model::reltime(s.mtime),
                        model::short_cwd(&s.cwd),
                        s.title(),
                        s.path.to_string_lossy(),
                        s.id,
                        s.cwd
                    );
                }
            }
        }
        return Ok(());
    }

    // ---------------- interactive ----------------
    let mut app = App::new(
        sessions,
        meta::Meta::load(),
        live::live_map(),
        restore_model,
    );
    // What wsx said last time, to draw the first frame with; it is asked
    // again behind the list, and on every rescan. Waiting for its answer
    // before drawing anything was sixty milliseconds -- three quarters of a
    // start -- and workspaces seldom change between two of them.
    app.ask_wsx = true;
    app.set_wsx(wsx::remembered());
    app.want_wsx = true;
    app.show_subagents = has("--subagents");
    app.rebuild();

    if let Some(stops) = &cfg.ramp {
        let parsed: Vec<(u8, u8, u8)> = stops
            .iter()
            .filter_map(|s| config::parse_color(s))
            .collect();
        art::set_ramp(parsed);
    }
    // A flag always beats the config.
    app.mouse_on = cfg.start.mouse && !has("--no-mouse");
    app.show_preview = cfg.start.preview;
    app.show_subagents = cfg.start.subagents || has("--subagents");
    app.rebuild();

    // The browser draws on stderr and reads keys from stdin, so without a
    // terminal there is nothing to draw on. Saying so beats the errno that
    // used to come back from the tty: "No such device or address".
    {
        use std::io::IsTerminal;
        if !stderr().is_terminal() {
            eprintln!(
                "mnemosyne: no terminal to draw on.\n\
                 For a script, use --list, --json or --search."
            );
            std::process::exit(2);
        }
    }

    // Put the terminal back if anything panics from here on. Without this a
    // panic left raw mode on, the alternate screen up, mouse reporting on and
    // the keyboard in the protocol we asked for -- an unusable shell, with
    // the message explaining it painted onto a screen you cannot see.
    //
    // Only for a panic on this thread. The hook is the whole process's, and
    // a panic behind the browser -- in the rescan, or the update check --
    // put the terminal back while the browser went on drawing: onto the
    // normal screen, in cooked mode, under the panic message. That thread's
    // message waits for the browser to close instead.
    {
        let main_thread = std::thread::current().id();
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().id() != main_thread {
                if let Ok(mut held) = BACKGROUND_PANICS.lock() {
                    held.push(info.to_string());
                }
                return;
            }
            let _ = disable_raw_mode();
            let _ = execute!(
                stderr(),
                event::PopKeyboardEnhancementFlags,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            default_hook(info);
        }));
    }

    enable_raw_mode()?;
    stderr().execute(EnterAlternateScreen)?;
    // Put back on the way out, however the way out goes. The panic hook
    // only runs where a panic starts: one on a rayon worker -- a scan -- is
    // carried to this thread by `resume_unwind`, which skips the hook, so
    // `R` hitting a transcript that crashed the scanner left the terminal
    // in raw mode on the alternate screen, with no word about why.
    let restore = Restore;
    // Ask the terminal to tell shift and ctrl apart, so ctrl+shift+t can be
    // its own key rather than arriving as ctrl+t. Terminals that do not
    // understand the request ignore it, and inside tmux it additionally
    // needs `set -s extended-keys on` -- so this is asked for and not relied
    // on. `W` does the same job everywhere.
    //
    // Deliberately not crossterm's supports_keyboard_enhancement(): that
    // probes by writing to *stdout* and reading the reply, and stdout is
    // where the chosen session is printed. The probe ended up in the plan,
    // and the shell tried to resume a session called `^[[?u^[[ctmux`.
    let _ = execute!(
        stderr(),
        event::PushKeyboardEnhancementFlags(
            event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        )
    );
    if app.mouse_on {
        stderr().execute(EnableMouseCapture)?;
    }
    let mut term = Terminal::new(CrosstermBackend::new(stderr()))?;

    // Started before the splash so the network round-trip overlaps the
    // animation rather than following it. Off the main path entirely: a slow
    // or missing network delays nothing, and the result is only ever a line
    // of text the header shows.
    let updated: std::sync::Arc<std::sync::Mutex<Option<update::Found>>> = Default::default();
    if !has("--no-update") && cfg.update.auto && std::env::var_os("MNEMOSYNE_NO_UPDATE").is_none() {
        let slot = updated.clone();
        // 0 is meaningful here: check on every start.
        let every = cfg.update.check_every_hours;
        std::thread::spawn(move || {
            if let Some(found) = update::auto(every) {
                if let Ok(mut g) = slot.lock() {
                    *g = Some(found);
                }
            }
        });
    }

    // The rescan, if it is still running when the list goes up.
    let mut indexing: Option<std::thread::JoinHandle<Result<Vec<model::Session>>>> = None;
    if use_splash {
        let p = index::Progress::default();
        let p2 = p.clone();
        let handle = std::thread::spawn(move || index::refresh_with_progress(true, Some(p2)));
        // Only when there is work to cover. With the index already there the
        // list can be drawn at once, and the animation was six hundred
        // milliseconds of waiting on nothing -- nine tenths of every start.
        // `[splash] warm = true` plays it anyway.
        let animate = cold_start || cfg.splash.warm;
        // ctrl+c during the animation has to leave immediately. Joining
        // first would have blocked on the very scan the user was trying to
        // escape, on a screen that could no longer change.
        if animate
            && matches!(
                splash::run(&mut term, &p, cold_start),
                Ok(splash::End::Aborted)
            )
        {
            let _ = disable_raw_mode();
            let _ = execute!(
                term.backend_mut(),
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            let _ = term.show_cursor();
            std::process::exit(130);
        }
        if cold_start {
            // With nothing cached there is no list to show without it, so
            // this is the one case that waits.
            if let Ok(Ok(fresh)) = handle.join() {
                app.absorb_rescan(fresh);
            }
        } else {
            app.indexing = true;
            indexing = Some(handle);
        }
    }

    // Write down what is open, every run. There is no daemon to do it at
    // shutdown, so the record is kept fresh by the thing you actually use.
    // This also rolls the pre-reboot set over into the offer, which has to
    // happen before the offer can be read.
    workspace::record(app.open_sessions(), live::detection_supported());
    app.load_reopen();

    let res = run(&mut term, &mut app, &updated, &mut indexing);
    drop(restore);
    res?;

    // What was handed over counts as open: it will be running moments from
    // now and nothing else is watching when it happens. This has to include
    // the windows opened while the browser stayed up, not just a final
    // choice -- they are the ones a reboot would otherwise forget.
    {
        let mut open = app.open_sessions();
        let outcome_targets = match &app.outcome {
            Some(Outcome::Resume { targets, .. }) => targets.clone(),
            None => Vec::new(),
        };
        for t in app.launched.iter().chain(outcome_targets.iter()) {
            if !open.iter().any(|e| e.id == t.id) {
                open.push(workspace::Entry {
                    id: t.id.clone(),
                    cwd: t.cwd.clone(),
                    model: t.model.clone(),
                    perms: t.perms.clone(),
                    title: t.title.clone(),
                });
            }
        }
        workspace::record(open, live::detection_supported());
    }

    // What the status line would have said, had the screen not just gone.
    for n in &app.notes {
        eprintln!("{n}");
    }

    if let Some(Outcome::Resume { targets, target }) = &app.outcome {
        // Several selections cannot share this terminal, so they become
        // windows unless tmux was asked for explicitly.
        let mode = if *target == app::Target::Here && targets.len() > 1 {
            "window"
        } else {
            target.tag()
        };
        let mut out = std::io::stdout().lock();
        for t in targets {
            plan_line(
                &mut out,
                mode,
                &t.cwd,
                &t.id,
                &t.model,
                &t.perms,
                &t.title,
                &app.tmux_name,
            )?;
        }
    }
    Ok(())
}

/// Panics on threads behind the browser, said once the browser has closed.
static BACKGROUND_PANICS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The terminal as the browser found it, put back when this is dropped --
/// at the end of the browser, or by any unwinding past it.
struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        let mut err = stderr();
        let _ = execute!(err, event::PopKeyboardEnhancementFlags);
        let _ = disable_raw_mode();
        let _ = execute!(
            err,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        if let Ok(held) = BACKGROUND_PANICS.lock() {
            for m in held.iter() {
                eprintln!("mnemosyne: a background task failed: {m}");
            }
        }
    }
}

/// The rescan running behind the list, if there is one.
type Indexing = Option<std::thread::JoinHandle<Result<Vec<model::Session>>>>;

fn run<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    app: &mut App,
    updated: &std::sync::Arc<std::sync::Mutex<Option<update::Found>>>,
    indexing: &mut Indexing,
) -> Result<()> {
    let mut last_live = Instant::now();
    let mut injected = false;
    let mut asking_wsx: Option<std::thread::JoinHandle<wsx::State>> = None;
    loop {
        term.draw(|f| ui::draw(f, app))?;

        if event::poll(Duration::from_millis(120))? {
            match event::read()? {
                Event::Key(k) if k.kind == event::KeyEventKind::Press => app.on_key(k),
                Event::Mouse(m) => app.on_mouse(m),
                Event::Resize(_, _) => {}
                _ => {}
            }
        }

        // Fault injection, so the terminal-restoring panic hook can be
        // tested for real rather than reasoned about. `background` panics a
        // thread of its own instead, once, which must leave the browser up.
        match std::env::var("MNEMOSYNE_PANIC_TEST").as_deref() {
            Ok("background") if !injected => {
                injected = true;
                std::thread::spawn(|| panic!("deliberate background panic"));
            }
            // A panic on a rayon worker, carried here by resume_unwind:
            // what a scanner panic during `R` does.
            Ok("rayon") => {
                use rayon::prelude::*;
                (0..2)
                    .into_par_iter()
                    .for_each(|_| panic!("deliberate rayon panic"));
            }
            Ok("background") | Err(_) => {}
            Ok(_) => panic!("deliberate panic for the terminal-restore test"),
        }

        app.absorb_deep();

        // Sessions chosen for a window of their own go to the shell straight
        // away, while the browser stays up. Flushed line by line, because
        // the shell is reading them as they arrive rather than waiting for
        // this process to exit.
        if !app.to_open.is_empty() {
            let mut out = std::io::stdout().lock();
            for (target, targets) in std::mem::take(&mut app.to_open) {
                for t in &targets {
                    let _ = plan_line(
                        &mut out,
                        target.tag(),
                        &t.cwd,
                        &t.id,
                        &t.model,
                        &t.perms,
                        &t.title,
                        &app.tmux_name,
                    );
                }
            }
            let _ = out.flush();
        }

        // A live wsx workspace goes back to wsx. The picker stays up, as it
        // does for a window: nothing about it needs this terminal.
        if !app.to_jump.is_empty() {
            app.finish_jumps(wsx::load(), wsx::jump);
        }

        // wsx, asked off this thread and folded in when it answers.
        if app.want_wsx && asking_wsx.is_none() {
            app.want_wsx = false;
            asking_wsx = Some(std::thread::spawn(wsx::load));
        }
        if asking_wsx.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(Ok(fresh)) = asking_wsx.take().map(|h| h.join()) {
                app.set_wsx(fresh);
            }
        }

        // The rescan started behind the list; fold it in the moment it
        // lands, rather than making anyone wait for it up front.
        if indexing.as_ref().is_some_and(|h| h.is_finished()) {
            if let Some(h) = indexing.take() {
                app.indexing = false;
                if let Ok(Ok(fresh)) = h.join() {
                    app.absorb_rescan(fresh);
                }
            }
        }

        if app.update_notice.is_none() {
            if let Ok(g) = updated.try_lock() {
                if let Some(found) = g.clone() {
                    // Say it once in the status line as well, so it is not
                    // just a chip in the corner you might never look at.
                    app.status = match &found {
                        update::Found::Installed(v) => {
                            format!("mnemosyne v{v} installed — restart mn to start using it")
                        }
                        update::Found::Available(v) => {
                            format!("mnemosyne v{v} is available — run: mn --update")
                        }
                    };
                    app.update_notice = Some(found);
                }
            }
        }

        // `M` flips mouse reporting, so the terminal can do its own text
        // selection again when you need to copy something off the screen.
        if app.mouse_toggled {
            app.mouse_toggled = false;
            if app.mouse_on {
                let _ = stderr().execute(EnableMouseCapture);
            } else {
                let _ = stderr().execute(DisableMouseCapture);
            }
        }

        if app.want_refresh {
            app.want_refresh = false;
            let fresh = index::refresh(true)?;
            // Said in the status line and again once the browser has
            // closed: printed, it went on the browser, and the next frame
            // wiped it before anyone learned where the file went.
            let (meta, said) = meta::Meta::load_quietly();
            app.meta = meta;
            app.absorb_rescan(fresh);
            app.status = app.reindex_message();
            if let Some(said) = said {
                app.status = said.clone();
                app.notes.push(said);
            }
        }

        // keep the running/not-running markers honest without re-reading disk
        if last_live.elapsed() > Duration::from_secs(3) {
            last_live = Instant::now();
            let lm = live::live_map();
            if lm.fingerprint() != app.live.fingerprint() {
                app.live = lm;
                app.apply_overlay();
                app.rebuild();
            }
        }

        if app.quit {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod arg_tests {
    use super::{check_args, flags_given};

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn the_real_options_are_accepted() {
        for line in [
            "--list",
            "--json --subagents",
            "--search zpool",
            "--search zpool --search-mode everything",
            "--restore 5",
            "--reopen --no-model",
            "--no-splash --no-mouse --no-update",
        ] {
            assert!(check_args(&args(line)).is_ok(), "rejected {line:?}");
        }
    }

    #[test]
    fn a_search_for_something_that_looks_like_a_flag_is_a_search() {
        // `--search --update` installed an update.
        let a = args("--search --update --search-mode everything");
        assert!(check_args(&a).is_ok());
        let given = flags_given(&a);
        assert!(given.contains(&"--search"));
        assert!(!given.contains(&"--update"), "{given:?}");
        assert!(!given.contains(&"everything"), "{given:?}");
    }

    #[test]
    fn a_mistyped_option_is_refused_rather_than_ignored() {
        // It used to be skipped, which opened the browser as though nothing
        // had happened -- the one outcome that looks like success.
        let e = check_args(&args("--nosplash")).unwrap_err();
        assert!(e.contains("--nosplash"), "{e}");
        assert!(e.contains("--help"), "should point somewhere useful: {e}");
    }

    #[test]
    fn a_count_has_to_be_a_count() {
        assert!(check_args(&args("--restore abc")).is_err());
        assert!(check_args(&args("--restore -3")).is_err());
        assert!(check_args(&args("--restore 0")).is_err());
        assert!(check_args(&args("--restore 12")).is_ok());
        // omitted entirely is fine; it has a default
        assert!(check_args(&args("--restore")).is_ok());
    }

    #[test]
    fn an_unknown_search_mode_does_not_silently_become_content() {
        let e = check_args(&args("--search x --search-mode bogus")).unwrap_err();
        assert!(e.contains("content"), "should list the real ones: {e}");
        assert!(check_args(&args("--search x --search-mode tool")).is_ok());
    }

    #[test]
    fn a_search_mode_with_nothing_to_search_is_refused() {
        // It opened the browser as though it had not been typed, which is
        // what an ignored flag looks like -- `--search-mode file` on its own
        // reads like it should be a file search.
        let e = check_args(&args("--search-mode file")).unwrap_err();
        assert!(e.contains("--search"), "should say what it goes with: {e}");
        assert!(check_args(&args("--search-mode file --search x")).is_ok());
    }

    #[test]
    fn a_query_that_looks_like_a_flag_is_still_a_query() {
        // The value after --search is whatever you typed, even if it starts
        // with a dash; it must not be checked as an option.
        assert!(check_args(&args("--search --weird-thing")).is_ok());
    }
}

#[cfg(test)]
mod wrapper_tests {
    use super::{PLAN_FLAGS, PLAN_VALUE_FLAGS, REPORT_FLAGS, REPORT_VALUE_FLAGS};
    use std::collections::BTreeSet;

    const WRAPPERS: [(&str, &str); 2] = [
        ("shell/mn.fish", include_str!("../shell/mn.fish")),
        ("shell/mn.bash", include_str!("../shell/mn.bash")),
    ];

    /// The flags on the wrapper lines tagged `# flags: <kind>`.
    fn tagged(src: &str, kind: &str) -> BTreeSet<String> {
        src.lines()
            .filter_map(|l| {
                let (code, tag) = l.split_once("# flags:")?;
                (tag.trim() == kind).then_some(code)
            })
            .flat_map(|code| code.split(|c: char| c.is_whitespace() || c == '|' || c == ')'))
            .filter(|t| t.starts_with('-'))
            .map(str::to_string)
            .collect()
    }

    fn set(flags: &[&str]) -> BTreeSet<String> {
        flags.iter().map(|f| f.to_string()).collect()
    }

    #[test]
    fn the_wrappers_route_every_flag_the_way_the_binary_means_it() {
        // Each wrapper decides for itself which options are mnemosyne's and
        // which are claude's, and whether to read a plan back. Its list fell
        // behind this one: `mn --stats` opened the browser and passed
        // --stats on to claude, and `mn --check-update` answered "folder
        // gone" because its reply was taken for a plan line.
        for (file, src) in WRAPPERS {
            for (kind, want) in [
                ("report", REPORT_FLAGS),
                ("report value", REPORT_VALUE_FLAGS),
                ("plan", PLAN_FLAGS),
                ("plan value", PLAN_VALUE_FLAGS),
            ] {
                let have = tagged(src, kind);
                let want = set(want);
                let missing: Vec<_> = want.difference(&have).collect();
                let extra: Vec<_> = have.difference(&want).collect();
                assert!(
                    missing.is_empty() && extra.is_empty(),
                    "{file}, `# flags: {kind}`: missing {missing:?}, not the binary's {extra:?}"
                );
            }
        }
    }

    #[test]
    fn the_tag_reader_sees_both_shells_syntax() {
        // A test that reads nothing passes; make sure this one reads.
        let fish = "            case --a --b-c  # flags: plan";
        let bash = "            --a|--b-c) mine+=(\"$a\") ;;  # flags: plan";
        for src in [fish, bash] {
            assert_eq!(tagged(src, "plan"), set(&["--a", "--b-c"]), "{src}");
            assert!(tagged(src, "report").is_empty());
        }
    }
}

#[cfg(test)]
mod restore_tests {
    use super::most_recent_first;

    #[test]
    fn restore_takes_the_newest_not_the_favourites() {
        // aaaaaaaa-1 is the fixture's favourite and its newest; make the
        // favourite old and check it no longer comes first.
        let mut a = crate::app::fixtures::app();
        let fav = a.all.iter().position(|s| s.id == "aaaaaaaa-1").unwrap();
        a.all[fav].mtime -= 400 * 86_400;
        let order: Vec<&str> = most_recent_first(&a)
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(order[0], "bbbbbbbb-2", "{order:?}");
        assert_eq!(*order.last().unwrap(), "aaaaaaaa-1");
        assert!(!order.iter().any(|id| id.starts_with("agent-")));
    }

    #[test]
    fn restore_leaves_a_live_wsx_workspace_to_wsx() {
        // gggggggg-7 is the newest conversation at the top of the live
        // shy-daffodil worktree: wsx carries it on when it starts.
        let a = crate::app::fixtures::app();
        let order: Vec<&str> = most_recent_first(&a)
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert!(!order.contains(&"gggggggg-7"), "{order:?}");
        // an archived workspace's is not wsx's any more
        assert!(order.contains(&"hhhhhhhh-8"), "{order:?}");
    }
}

#[cfg(test)]
mod plan_tests {
    use super::plan_line;

    fn line(cwd: &str, title: &str) -> (bool, String) {
        let mut out: Vec<u8> = Vec::new();
        let wrote = plan_line(
            &mut out,
            "wintmux",
            cwd,
            "026bcdb5-8d88-4ad7-9f23-58649bf4f353",
            "claude-opus-5",
            "bypassPermissions",
            title,
            "",
        )
        .unwrap();
        (wrote, String::from_utf8(out).unwrap())
    }

    #[test]
    fn an_ordinary_session_becomes_six_fields() {
        let (wrote, text) = line("/home/u/proj", "some title");
        assert!(wrote);
        assert_eq!(text.matches('\t').count(), 6, "wrong shape: {text:?}");
        assert!(text.ends_with("some title\t\n"), "{text:?}");
    }

    #[test]
    fn a_folder_with_a_tab_in_it_is_refused_rather_than_mangled() {
        // The shell splits this back apart positionally, so a tab inside a
        // field shifts every field after it: the session id becomes half a
        // path and the failure surfaces as something unrelated.
        let (wrote, text) = line("/home/u/tab\there", "fine");
        assert!(!wrote);
        assert!(text.is_empty(), "wrote a line that cannot be parsed back");
    }

    #[test]
    fn the_separator_mn_bash_splits_on_is_refused_too() {
        let (wrote, text) = line("/home/u/odd\u{1f}folder", "fine");
        assert!(!wrote);
        assert!(text.is_empty());
    }

    #[test]
    fn a_newline_is_refused_too() {
        let (wrote, text) = line("/home/u/proj", "first line\nsecond line");
        assert!(!wrote);
        assert!(text.is_empty());
    }

    #[test]
    fn a_space_in_a_path_is_perfectly_fine() {
        // Spaces are ordinary, especially on macOS. Only tabs break the shape.
        let (wrote, text) = line("/home/u/My Documents/thing", "a title");
        assert!(wrote);
        assert!(text.contains("/home/u/My Documents/thing"));
    }
}
