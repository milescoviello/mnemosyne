//! Where things are on this machine: your home directory, and the folder
//! Claude Code keeps its sessions in. Everything that needs either asks
//! here, so the two can never disagree.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Your home directory: `$HOME`, or the password database's where that is
/// unset or empty -- a service, a container, `env -i`.
///
/// The fallbacks used to be `/tmp` for the index and `/` for the
/// transcripts, which disagreed with each other, and the first put a copy
/// of your conversations, and a config anyone could plant ahead of you, in
/// the machine-wide `/tmp`.
pub fn home() -> PathBuf {
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| home_from(std::env::var_os("HOME"), passwd_home))
        .clone()
}

fn home_from(var: Option<OsString>, passwd: impl FnOnce() -> Option<PathBuf>) -> PathBuf {
    match var {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => passwd().unwrap_or_else(|| PathBuf::from("/")),
    }
}

/// This user's home in the password database.
fn passwd_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0u8; 16 * 1024];
    // SAFETY: getpwuid_r writes only into `pw` and `buf`, both ours and big
    // enough for what it says it wrote; `out` is null unless it found one.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut out: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(
            libc::getuid(),
            &mut pw,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut out,
        );
        if rc != 0 || out.is_null() || pw.pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr(pw.pw_dir).to_bytes();
        (!dir.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(dir)))
    }
}

/// Where Claude Code keeps what it writes: `$CLAUDE_CONFIG_DIR`, which it
/// reads to move all of it elsewhere -- one per account, say -- or
/// `~/.claude`. Not read, the browser came up empty for anyone who had
/// moved it ("transcripts 0").
pub fn claude_dir() -> PathBuf {
    claude_dir_from(std::env::var_os("CLAUDE_CONFIG_DIR"), home())
}

fn claude_dir_from(var: Option<OsString>, home: PathBuf) -> PathBuf {
    match var {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => home.join(".claude"),
    }
}

/// Make `dir`, and keep it to you: 0700, and an existing one tightened to
/// that. Everything mn keeps goes in one -- the index, which holds the prose
/// of every conversation; your notes; what was open -- and it was 0755,
/// with the files in it 0644: readable by every account on the machine,
/// wherever home is 0755 too (Debian 11 and older, Ubuntu 20.04 and older,
/// many servers), although Claude Code keeps the transcripts themselves to
/// you alone. The folder is enough: nothing in it can be reached past it.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let mode = std::fs::metadata(dir)?.permissions().mode();
    if mode & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode & 0o7700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn what_mn_keeps_is_kept_to_you() {
        let t = tempfile::tempdir().unwrap();
        let fresh = t.path().join("a/mnemosyne");
        private_dir(&fresh).unwrap();
        assert_eq!(mode(&fresh), 0o700);
        // one an older mn made, open to everyone, is closed
        let old = t.path().join("old");
        std::fs::create_dir(&old).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        private_dir(&old).unwrap();
        assert_eq!(mode(&old), 0o700);
    }

    #[test]
    fn home_is_the_variable_when_there_is_one() {
        let h = home_from(Some("/home/x".into()), || {
            panic!("asked the password database")
        });
        assert_eq!(h, PathBuf::from("/home/x"));
    }

    #[test]
    fn with_home_unset_or_empty_it_is_the_password_databases_never_tmp() {
        let db = || Some(PathBuf::from("/home/from-passwd"));
        assert_eq!(home_from(None, db), PathBuf::from("/home/from-passwd"));
        assert_eq!(
            home_from(Some("".into()), db),
            PathBuf::from("/home/from-passwd")
        );
        // nothing anywhere: somewhere no other user can write, not /tmp
        assert_eq!(home_from(None, || None), PathBuf::from("/"));
    }

    #[test]
    fn this_user_is_in_the_password_database() {
        // whoever runs the suite has an entry, and a home in it
        let h = passwd_home().expect("no password entry for this user");
        assert!(h.is_absolute(), "{h:?}");
    }

    #[test]
    fn claude_config_dir_moves_claudes_folder() {
        let home = PathBuf::from("/home/x");
        assert_eq!(
            claude_dir_from(Some("/srv/work-claude".into()), home.clone()),
            PathBuf::from("/srv/work-claude")
        );
        assert_eq!(claude_dir_from(None, home.clone()), home.join(".claude"));
        assert_eq!(
            claude_dir_from(Some("".into()), home.clone()),
            home.join(".claude")
        );
    }
}
