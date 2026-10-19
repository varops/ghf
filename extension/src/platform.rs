//! The few places ghf differs between Unix and Windows.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Modification time in nanoseconds since the Unix epoch.
pub fn mtime_ns(md: &fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as i64)
}

/// A stable file identity where the platform has one (the inode on Unix).
/// Windows returns 0; size and mtime still guard the stat cache.
pub fn file_id(md: &fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        std::os::unix::fs::MetadataExt::ino(md)
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        0
    }
}

/// Is the file executable? Windows has no executable bit.
pub fn is_executable(md: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::PermissionsExt::mode(&md.permissions()) & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        false
    }
}

/// Set or clear the executable bit (no-op on Windows).
pub fn set_executable(path: &Path, exec: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }),
        )
    }
    #[cfg(not(unix))]
    {
        let _ = (path, exec);
        Ok(())
    }
}

/// A symlink's target as bytes, the form ghf stores.
pub fn read_link_bytes(path: &Path) -> io::Result<Vec<u8>> {
    let target = fs::read_link(path)?;
    Ok(path_bytes(&target))
}

pub fn path_bytes(p: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        p.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        p.to_string_lossy().replace('\\', "/").into_bytes()
    }
}

/// Create a symlink at `link` pointing at `target`. Where Windows refuses
/// (no privilege or developer mode), write a plain file holding the target,
/// as Git for Windows does with `core.symlinks=false`.
pub fn create_symlink(target: &[u8], link: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), link)
    }
    #[cfg(windows)]
    {
        let t = String::from_utf8_lossy(target).into_owned();
        match std::os::windows::fs::symlink_file(&t, link) {
            Ok(()) => Ok(()),
            Err(_) => fs::write(link, target),
        }
    }
}

/// The user's home directory.
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .or_else(|| std::env::var_os("HOME"))
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The user's login name.
pub fn user_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into())
}

/// Canonical path without Windows' `\\?\` verbatim prefix, which Git and
/// many tools do not accept.
pub fn canonicalize(p: impl AsRef<Path>) -> io::Result<PathBuf> {
    let c = fs::canonicalize(p)?;
    #[cfg(windows)]
    {
        let s = c.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return Ok(PathBuf::from(format!(r"\\{rest}")));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return Ok(PathBuf::from(rest));
        }
    }
    Ok(c)
}

/// Detach a child so it outlives us and is not killed with our process group.
pub fn detach(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
}

/// Create (or truncate) a file readable only by its owner where the
/// platform supports that.
pub fn create_private(path: &Path) -> io::Result<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

/// Set a file's or directory's modification and access time. Windows can
/// only open a directory handle with backup semantics.
pub fn set_times(path: &Path, t: std::time::SystemTime) -> io::Result<()> {
    let times = fs::FileTimes::new().set_modified(t).set_accessed(t);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        let f = fs::OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        f.set_times(times)
    }
    #[cfg(not(windows))]
    {
        fs::File::open(path)?.set_times(times)
    }
}
