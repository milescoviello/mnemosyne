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

  --restore N      reopen the N most recent sessions, each in its own window,
                   skipping any already running (replaces claude-restore)

  --subagents      start with subagent transcripts revealed
  --no-splash      skip the opening animation (or set MNEMOSYNE_NO_SPLASH=1)
  --no-mouse       start with mouse reporting off (toggle in-app with M)
  --write-config   write a commented config file and exit

  --update         install the latest release now, and exit
  --check-update   say whether a newer release exists, and exit
  --no-update      skip the background update check this run
  --no-model       do not restore each session's original --model
  -h, --help       this text
  -V, --version    version

On exit the browser prints the chosen action to stdout as TSV:
  <here|window|tmux>\\t<cwd>\\t<session-id>\\t<model>\\t<permission-mode>\\t<title>
The shell function (`mn`) turns that into a cd plus `claude --resume`, or into
a tmux attach. It restores the model and the permission mode the session
started in; pass --ask to resume with prompts on instead.
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a == f);

    if has("-h") || has("--help") {
        print!("{HELP}");
        return Ok(());
    }
    if has("-V") || has("--version") {
        println!("mnemosyne {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let include_subagents = true; // always indexed; visibility is a UI toggle
    let restore_model = !has("--no-model");

    if has("--update") {
        println!("current {}", update::current());
        match update::install_latest() {
            Ok(v) => println!("updated to {v} — it takes effect next time you start"),
            Err(e) => println!("not updated: {e}"),
        }
        return Ok(());
    }

    if has("--check-update") {
        match update::latest_tag() {
            Some(tag) if update::is_newer(&tag, update::current()) => {
                println!("{} is available; you have {}", tag, update::current())
            }
            Some(tag) => println!("up to date on {} (latest is {tag})", update::current()),
            None => println!("could not reach GitHub"),
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

    let interactive =
        !(has("--list") || has("--json") || has("--stats") || args.iter().any(|a| a == "--search"));
    let use_splash =
        interactive && !has("--no-splash") && std::env::var_os("MNEMOSYNE_NO_SPLASH").is_none();

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
        println!("running now     {}", live::live_map().count);
        let m = meta::Meta::load();
        println!("favourites      {}", m.favorite_count());
        println!("tags            {}", m.all_tags().len());
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
            _ => search::Mode::Content,
        };
        let pool: Vec<model::Session> = sessions
            .iter()
            .filter(|s| has("--subagents") || !s.is_subagent)
            .cloned()
            .collect();
        let t = Instant::now();
        let (hits, how) = search::run(&pool, q, mode);
        let mut rows: Vec<&model::Session> = pool
            .iter()
            .filter(|s| hits.contains_key(&s.path.to_string_lossy().to_string()))
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
        app.rebuild();
        let mut out = std::io::stdout().lock();
        let mut opened = 0;
        for r in &app.view {
            if opened >= n {
                break;
            }
            let app::Row::Item(idx) = r else { continue };
            let s = &app.all[*idx];
            // already up, so reopening would just duplicate the window
            if s.live_exact || s.has_tmux {
                continue;
            }
            if s.cwd.is_empty() || !std::path::Path::new(&s.cwd).is_dir() {
                continue;
            }
            writeln!(
                out,
                "window\t{}\t{}\t{}\t{}\t{}",
                s.cwd,
                s.id,
                if restore_model {
                    s.model.clone()
                } else {
                    String::new()
                },
                s.permission_mode,
                s.title()
            )?;
            opened += 1;
        }
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
    app.show_subagents = has("--subagents");
    app.rebuild();

    let cfg = config::Config::load();
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

    enable_raw_mode()?;
    stderr().execute(EnterAlternateScreen)?;
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

    if use_splash {
        let p = index::Progress::default();
        let p2 = p.clone();
        let handle = std::thread::spawn(move || index::refresh_with_progress(true, Some(p2)));
        let _ = splash::run(&mut term, &p);
        if let Ok(Ok(fresh)) = handle.join() {
            let mut fresh = fresh;
            fresh.sort_by_key(|s| std::cmp::Reverse(s.mtime));
            app.all = fresh;
            app.live = live::live_map();
            app.recompute_totals();
            app.apply_overlay();
            app.rebuild();
        }
    }

    let res = run(&mut term, &mut app, &updated);
    disable_raw_mode()?;
    let _ = execute!(term.backend_mut(), DisableMouseCapture);
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    res?;

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
            writeln!(
                out,
                "{mode}\t{}\t{}\t{}\t{}\t{}",
                t.cwd, t.id, t.model, t.perms, t.title
            )?;
        }
    }
    Ok(())
}

fn run<B: ratatui::backend::Backend>(
    term: &mut Terminal<B>,
    app: &mut App,
    updated: &std::sync::Arc<std::sync::Mutex<Option<update::Found>>>,
) -> Result<()> {
    let mut last_live = Instant::now();
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

        app.absorb_deep();

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
            let sessions = index::refresh(true)?;
            let mut fresh = sessions;
            fresh.sort_by_key(|s| std::cmp::Reverse(s.mtime));
            app.all = fresh;
            app.recompute_totals();
            app.meta = meta::Meta::load();
            app.live = live::live_map();
            app.apply_overlay();
            app.rebuild();
            app.status = format!("reindexed — {} sessions", app.item_count());
        }

        // keep the running/not-running markers honest without re-reading disk
        if last_live.elapsed() > Duration::from_secs(3) {
            last_live = Instant::now();
            let lm = live::live_map();
            if lm.count != app.live.count {
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
