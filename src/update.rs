//! Keeping itself current.
//!
//! Checks GitHub for a newer release, and by default installs it. Two rules
//! shape how:
//!
//! * It never blocks you. The check runs on a background thread, at most once
//!   a day, and if the network is down or slow nothing waits on it.
//! * It never swaps the binary out from under the process you are using. The
//!   new one is renamed into place, which is atomic and leaves the running
//!   image alone; it takes effect the next time you start.
//!
//! Network and archive work shell out to curl, tar and sha256sum rather than
//! linking a TLS stack and a decompressor into a session browser.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "milescoviello/mnemosyne";
/// The release asset for the platform this binary was built for.
///
/// Hardcoding one of these is a quiet way to hand an aarch64 machine an
/// x86-64 binary, so it is derived from the build target.
pub const fn asset() -> &'static str {
    if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "mnemosyne-aarch64-macos.tar.gz"
        } else {
            "mnemosyne-x86_64-macos.tar.gz"
        }
    } else if cfg!(target_arch = "aarch64") {
        "mnemosyne-aarch64-linux.tar.gz"
    } else {
        "mnemosyne-x86_64-linux.tar.gz"
    }
}

pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn stamp_path() -> PathBuf {
    crate::index::state_dir().join("last-update-check")
}

/// Has enough time passed to be worth asking GitHub again?
///
/// Zero means every start, which is the default: the check is a background
/// thread nothing waits on, so the only cost of asking is a request.
/// The stamp is a path rather than a fixed location so the tests can use a
/// temporary one. They used to read and *write* the real file in whatever
/// home the suite ran in, which made one of them pass only on a machine
/// that had not run mnemosyne lately.
pub fn check_due_at(stamp: &Path, every_hours: u64) -> bool {
    if every_hours == 0 {
        return true;
    }
    let Ok(meta) = std::fs::metadata(stamp) else {
        return true;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    modified
        .elapsed()
        .map(|d| d.as_secs() >= every_hours * 3600)
        .unwrap_or(true)
}

fn touch_stamp() {
    touch_stamp_at(&stamp_path());
}

fn touch_stamp_at(stamp: &Path) {
    if let Some(dir) = stamp.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(stamp, chrono::Utc::now().to_rfc3339());
}

/// `0.2.0` -> `[0, 2, 0]`, ignoring a leading `v` and anything after a dash.
fn parts(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches('v')
        .split('-')
        .next()
        .unwrap_or("")
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

/// Is `candidate` a later version than `have`?
pub fn is_newer(candidate: &str, have: &str) -> bool {
    let (a, b) = (parts(candidate), parts(have));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

fn curl(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "20"])
        .args(args)
        .output()
        .map_err(|e| anyhow!("curl: {e}"))?;
    if !out.status.success() {
        return Err(anyhow!("curl failed"));
    }
    Ok(out.stdout)
}

/// The newest published tag, or None if we could not find out.
pub fn latest_tag() -> Option<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = curl(&["-H", "Accept: application/vnd.github+json", &url]).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&body).ok()?;
    v.get("tag_name")?.as_str().map(|s| s.to_string())
}

/// Hash a file, using whichever tool this platform ships: coreutils calls it
/// sha256sum, macOS calls it shasum.
fn sha256_of(path: &Path) -> Option<String> {
    for (bin, args) in [("sha256sum", vec![]), ("shasum", vec!["-a", "256"])] {
        if let Ok(out) = Command::new(bin).args(&args).arg(path).output() {
            if out.status.success() {
                if let Some(h) = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next()
                {
                    return Some(h.to_string());
                }
            }
        }
    }
    None
}

/// Where this binary lives, resolving symlinks so we replace the real file.
fn own_path() -> Result<PathBuf> {
    let p = std::env::current_exe()?;
    Ok(std::fs::canonicalize(&p).unwrap_or(p))
}

/// Download the latest release and put it in place.
///
/// Returns the version installed. The running process keeps its own image;
/// the new binary is what starts next time.
pub fn install_latest() -> Result<String> {
    let tag = latest_tag().ok_or_else(|| anyhow!("could not reach GitHub"))?;
    if !is_newer(&tag, current()) {
        return Err(anyhow!("already on {}", current()));
    }

    let dir = std::env::temp_dir().join(format!("mnemosyne-update-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let tarball = dir.join(asset());
    let base = format!(
        "https://github.com/{REPO}/releases/latest/download/{}",
        asset()
    );

    let bytes = curl(&[&base])?;
    std::fs::write(&tarball, &bytes)?;

    // Verify before trusting it. A missing checksum file is a reason to stop,
    // not a reason to shrug.
    let sums = curl(&[&format!("{base}.sha256")])?;
    let want = String::from_utf8_lossy(&sums)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();
    let got = sha256_of(&tarball).unwrap_or_default();
    if want.is_empty() || want != got {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(anyhow!("checksum did not match — refusing to install"));
    }

    let status = Command::new("tar")
        .args([
            "-C",
            &dir.to_string_lossy(),
            "-xzf",
            &tarball.to_string_lossy(),
        ])
        .status()?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(anyhow!("could not unpack the release"));
    }

    let me = own_path()?;
    let new = dir.join("mnemosyne");
    if !new.is_file() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(anyhow!("release did not contain a binary"));
    }
    // Stage beside the target so the rename stays on one filesystem, then
    // rename over it: atomic, and safe while the old one is running.
    let staged = me.with_extension("new");
    std::fs::copy(&new, &staged)?;
    let mut perm = std::fs::metadata(&staged)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        perm.set_mode(0o755);
    }
    std::fs::set_permissions(&staged, perm)?;
    std::fs::rename(&staged, &me)?;

    // The shell functions ship in the tarball and can change with it.
    refresh_shell_files(&dir);

    let _ = std::fs::remove_dir_all(&dir);
    touch_stamp();
    Ok(tag.trim_start_matches('v').to_string())
}

/// Update the installed shell functions, but only where one already exists —
/// an update should not start installing things you did not have.
fn refresh_shell_files(from: &Path) {
    let home = std::env::var("HOME").unwrap_or_default();
    for (src, dst) in [
        ("mn.fish", format!("{home}/.config/fish/functions/mn.fish")),
        ("mn.bash", format!("{home}/.local/share/mnemosyne/mn.bash")),
    ] {
        let s = from.join(src);
        let d = PathBuf::from(&dst);
        if s.is_file() && d.is_file() {
            let _ = std::fs::copy(&s, &d);
        }
    }
}

/// What the background check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    /// Downloaded and put in place; it applies on the next start.
    Installed(String),
    /// A newer release exists but could not be installed — no network left
    /// by the time we tried, no permission to write the binary, a checksum
    /// that did not match. Worth saying so rather than silently doing
    /// nothing.
    Available(String),
}

/// The background check.
pub fn auto(every_hours: u64) -> Option<Found> {
    auto_with(every_hours, latest_tag)
}

/// The check, with the network call injectable so the ordering around the
/// stamp can be tested without one.
pub fn auto_with(every_hours: u64, fetch: impl FnOnce() -> Option<String>) -> Option<Found> {
    auto_full(&stamp_path(), every_hours, fetch, install_latest)
}

/// The whole decision, with the stamp, the network and the installer all
/// handed in. Nothing here reaches the network or replaces a binary unless
/// the caller says so, which is what makes the upgrade path testable at all.
pub fn auto_full(
    stamp: &Path,
    every_hours: u64,
    fetch: impl FnOnce() -> Option<String>,
    install: impl FnOnce() -> anyhow::Result<String>,
) -> Option<Found> {
    if !check_due_at(stamp, every_hours) {
        return None;
    }
    // Record the check only once GitHub has actually answered. Stamping
    // first meant a machine with no network marked itself as checked, and a
    // session closed mid-download burned the whole window without having
    // installed anything.
    let tag = fetch()?;
    touch_stamp_at(stamp);
    if !is_newer(&tag, current()) {
        return None;
    }
    let version = tag.trim_start_matches('v').to_string();
    match install() {
        Ok(v) => Some(Found::Installed(v)),
        Err(_) => Some(Found::Available(version)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        // shorter is not automatically older
        assert!(!is_newer("0.2", "0.2.0"));
        assert!(is_newer("0.3", "0.2.9"));
        // Double digits are where a string comparison would go wrong, and
        // this project is about to reach them.
        assert!(is_newer("0.4.10", "0.4.9"), "0.4.10 is newer than 0.4.9");
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(!is_newer("0.4.9", "0.4.10"));
        assert!(is_newer("1.0.0", "0.100.0"));
        // and a tag that is not a version at all must not read as an update
        assert!(!is_newer("nightly", "0.4.8"));
        assert!(!is_newer("", "0.4.8"));
        // a tag we cannot parse must never look like an upgrade
        assert!(!is_newer("nightly", "0.2.0"));
    }

    /// A stamp of our own, so the suite never reads or writes the real one.
    fn stamp(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("last-update-check")
    }

    #[test]
    fn an_unreachable_github_does_not_count_as_a_check() {
        // The stamp is what suppresses the next attempt, so writing it for a
        // check that never reached GitHub would silence retries for the whole
        // window. Also covers a session closed mid-download.
        let d = tempfile::tempdir().unwrap();
        let p = stamp(&d);
        let got = auto_full(&p, 0, || None, || panic!("must not install"));
        assert_eq!(got, None);
        assert!(!p.exists(), "stamped without hearing back from GitHub");
    }

    #[test]
    fn being_current_stamps_but_installs_nothing() {
        let d = tempfile::tempdir().unwrap();
        let p = stamp(&d);
        let v = format!("v{}", current());
        let got = auto_full(&p, 0, move || Some(v), || panic!("must not install"));
        assert_eq!(got, None, "nothing to do");
        assert!(p.exists(), "a real answer from GitHub should be recorded");
    }

    #[test]
    fn a_newer_tag_installs_and_says_so() {
        // The upgrade path itself, which nothing could reach before without
        // actually downloading a release over the top of the binary.
        let d = tempfile::tempdir().unwrap();
        let got = auto_full(
            &stamp(&d),
            0,
            || Some("v99.0.0".into()),
            || Ok("99.0.0".into()),
        );
        assert_eq!(got, Some(Found::Installed("99.0.0".into())));
    }

    #[test]
    fn an_install_that_fails_still_tells_you_there_is_one() {
        let d = tempfile::tempdir().unwrap();
        let got = auto_full(
            &stamp(&d),
            0,
            || Some("v99.0.0".into()),
            || Err(anyhow::anyhow!("no room on device")),
        );
        assert_eq!(
            got,
            Some(Found::Available("99.0.0".into())),
            "a failed install must not pass as up to date"
        );
    }

    #[test]
    fn a_check_is_due_when_there_is_no_record_of_one() {
        // first run on a machine must not skip the check
        let d = tempfile::tempdir().unwrap();
        assert!(check_due_at(&stamp(&d), 24), "no stamp means never checked");
    }

    #[test]
    fn a_fresh_stamp_holds_the_next_check_off() {
        let d = tempfile::tempdir().unwrap();
        let p = stamp(&d);
        touch_stamp_at(&p);
        assert!(!check_due_at(&p, 24), "checked seconds ago, asked again");
        assert!(check_due_at(&p, 0), "zero hours means every start");
    }

    #[test]
    fn an_old_stamp_lets_the_next_check_through() {
        let d = tempfile::tempdir().unwrap();
        let p = stamp(&d);
        touch_stamp_at(&p);
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600);
        f.set_times(std::fs::FileTimes::new().set_modified(long_ago))
            .unwrap();
        assert!(check_due_at(&p, 24), "two days old and still not due");
    }
}

#[cfg(test)]
mod asset_tests {
    use super::*;

    #[test]
    fn asset_matches_the_platform_it_was_built_for() {
        let a = asset();
        assert!(a.ends_with(".tar.gz"));
        if cfg!(target_os = "macos") {
            assert!(a.contains("macos"), "{a}");
        } else {
            assert!(a.contains("linux"), "{a}");
        }
        if cfg!(target_arch = "aarch64") {
            assert!(a.contains("aarch64"), "{a}");
        } else if cfg!(target_arch = "x86_64") {
            assert!(a.contains("x86_64"), "{a}");
        }
    }

    #[test]
    fn asset_name_matches_what_the_release_workflows_publish() {
        // If these drift, the updater 404s on every machine of that shape.
        // Intel macOS lives in its own workflow because its runners queue for
        // an hour and would hold up everyone else's release.
        let main = include_str!("../.github/workflows/release.yml");
        let intel = include_str!("../.github/workflows/release-intel-mac.yml");
        let both = format!("{main}{intel}");
        for name in [
            "mnemosyne-x86_64-linux",
            "mnemosyne-aarch64-linux",
            "mnemosyne-x86_64-macos",
            "mnemosyne-aarch64-macos",
        ] {
            assert!(both.contains(name), "nothing builds {name}");
        }
        assert!(
            both.contains(asset().trim_end_matches(".tar.gz")),
            "this platform's own asset is never built"
        );
    }

    #[test]
    fn the_slow_target_cannot_block_the_others() {
        let main = include_str!("../.github/workflows/release.yml");
        assert!(
            !main.contains("macos-13"),
            "Intel macOS is back in the blocking matrix"
        );
        let intel = include_str!("../.github/workflows/release-intel-mac.yml");
        assert!(intel.contains("continue-on-error: true"));
    }
}
