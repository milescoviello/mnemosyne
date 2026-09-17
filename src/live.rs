//! Which sessions are running right now.
//!
//! There is no session id in a Claude process's environment, so there is no
//! general pid -> session map. Two signals, in descending order of trust:
//!
//!   exact  -- the process has `--resume <uuid>` / `-r <uuid>` on its command
//!             line, which names the session outright.
//!   guess  -- the process's cwd matches a session's cwd and that session is
//!             the most recently touched one there. Plain `claude` with no
//!             `--resume` can't do better than this.
//!
//! Also recovers the `--model` a live process was started with, so resuming a
//! session doesn't silently drop it back to the default model.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: i32,
    pub cwd: String,
    pub resume_id: Option<String>,
    pub model: Option<String>,
}

fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.as_bytes()
            .iter()
            .enumerate()
            .all(|(i, c)| match i {
                8 | 13 | 18 | 23 => *c == b'-',
                _ => c.is_ascii_hexdigit(),
            })
}

pub fn scan_procs() -> Vec<Proc> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/proc") else { return out };
    let me = std::process::id() as i32;
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
        if pid == me {
            continue;
        }
        let base = format!("/proc/{pid}");
        let Ok(raw) = std::fs::read(format!("{base}/cmdline")) else { continue };
        if raw.is_empty() {
            continue;
        }
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if args.is_empty() {
            continue;
        }
        let exe = args[0].rsplit('/').next().unwrap_or("");
        if exe != "claude" {
            continue;
        }

        let mut resume_id = None;
        let mut model = None;
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            let next = args.get(i + 1).map(|s| s.as_str());
            match a {
                "--resume" | "-r" => {
                    if let Some(v) = next {
                        if looks_like_uuid(v) {
                            resume_id = Some(v.to_string());
                        }
                    }
                }
                "--model" => {
                    if let Some(v) = next {
                        model = Some(v.to_string());
                    }
                }
                _ => {
                    if let Some(v) = a.strip_prefix("--resume=") {
                        if looks_like_uuid(v) {
                            resume_id = Some(v.to_string());
                        }
                    } else if let Some(v) = a.strip_prefix("--model=") {
                        model = Some(v.to_string());
                    }
                }
            }
            i += 1;
        }

        let cwd = std::fs::read_link(format!("{base}/cwd"))
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        out.push(Proc { pid, cwd, resume_id, model });
    }
    out
}

pub struct LiveMap {
    pub by_id: HashMap<String, Proc>,
    pub by_cwd: HashMap<String, Proc>,
    pub count: usize,
}

pub fn live_map() -> LiveMap {
    let procs = scan_procs();
    let count = procs.len();
    let mut by_id = HashMap::new();
    let mut by_cwd = HashMap::new();
    for p in procs {
        match &p.resume_id {
            Some(id) => {
                by_id.insert(id.clone(), p);
            }
            None => {
                if !p.cwd.is_empty() {
                    by_cwd.insert(p.cwd.clone(), p);
                }
            }
        }
    }
    LiveMap { by_id, by_cwd, count }
}
