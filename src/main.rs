//! mnemosyne — browse, search, tag and resume Claude Code sessions.
//!
//! The interface is drawn on stderr and the chosen action is printed on stdout,
//! so a shell wrapper can capture the decision with a command substitution
//! while the TUI still owns the terminal. That wrapper exists because changing
//! the calling shell's working directory is something only the shell can do.

mod app;
mod art;
mod index;
mod live;
mod meta;
mod model;
mod preview;
mod scan;
mod search;
mod splash;
mod ui;

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
  --search-mode M  content (default) | file | tool

  --subagents      start with subagent transcripts revealed
  --no-splash      skip the opening animation (or set MNEMOSYNE_NO_SPLASH=1)
  --no-mouse       start with mouse reporting off (toggle in-app with M)
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
            _ => search::Mode::Content,
        };
        let pool: Vec<model::Session> = sessions
            .iter()
            .filter(|s| has("--subagents") || !s.is_subagent)
            .cloned()
            .collect();
        let t = Instant::now();
        let hits = search::run(&pool, q, mode);
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
            "{} of {} sessions matched \"{}\" ({}) in {:.2}s",
            rows.len(),
            pool.len(),
            q,
            mode.label(),
            t.elapsed().as_secs_f64()
        );
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

    app.mouse_on = !has("--no-mouse");

    enable_raw_mode()?;
    stderr().execute(EnterAlternateScreen)?;
    if app.mouse_on {
        stderr().execute(EnableMouseCapture)?;
    }
    let mut term = Terminal::new(CrosstermBackend::new(stderr()))?;

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
            app.apply_overlay();
            app.rebuild();
        }
    }

    let res = run(&mut term, &mut app);
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

fn run<B: ratatui::backend::Backend>(term: &mut Terminal<B>, app: &mut App) -> Result<()> {
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
