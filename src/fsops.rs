//! The filesystem and process edge: sizing, last use, moving to Trash, removing, tool lookup, free space.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use crate::model::Mode;

pub fn is_real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

pub fn exists_plain(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| !m.file_type().is_symlink())
}

/// Disk use in KB as `du -sk` counts it: allocated blocks, each hard-linked file once, symlinks not followed.
pub fn size_kb(path: &Path) -> u64 {
    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&p) else { continue };
        if meta.nlink() > 1 && !meta.is_dir() && !seen.insert((meta.dev(), meta.ino())) {
            continue;
        }
        total = total.saturating_add(meta.blocks().saturating_mul(512) / 1024);
        if meta.is_dir()
            && let Ok(entries) = fs::read_dir(&p)
        {
            stack.extend(entries.flatten().map(|e| e.path()));
        }
    }
    total
}

/// Sizes of `paths`, measured on parallel threads, in the same order.
pub fn sizes_kb(paths: &[PathBuf]) -> Vec<u64> {
    let workers = thread::available_parallelism().map_or(4, usize::from).clamp(2, 12);
    let chunk = paths.len().div_ceil(workers).max(1);
    thread::scope(|s| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|part| s.spawn(move || part.iter().map(|p| size_kb(p)).collect::<Vec<_>>()))
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    })
}

/// Seconds since the epoch of the newest change to `path` or anything directly in it.
pub fn newest_mtime(path: &Path) -> Option<i64> {
    let own = fs::symlink_metadata(path).ok().map(|m| m.mtime());
    let children =
        fs::read_dir(path).into_iter().flatten().flatten().filter_map(|e| e.metadata().ok()).map(|m| m.mtime());
    own.into_iter().chain(children).max()
}

pub fn days_since(now: i64, then: Option<i64>) -> Option<u64> {
    then.map(|t| u64::try_from(now.saturating_sub(t) / 86_400).unwrap_or(0))
}

/// Move `path` (or, in contents mode, each thing inside it) to the Trash.
pub fn move_to_trash(mode: Mode, path: &Path) -> Result<(), String> {
    targets(mode, path)?.iter().try_for_each(|p| trash::delete(p).map_err(|e| e.to_string()))
}

/// Remove `path` (or, in contents mode, each thing inside it) for good.
pub fn remove(mode: Mode, path: &Path) -> Result<(), String> {
    targets(mode, path)?.iter().try_for_each(|p| {
        let meta = fs::symlink_metadata(p).map_err(|e| e.to_string())?;
        if meta.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) }.map_err(|e| e.to_string())
    })
}

fn targets(mode: Mode, path: &Path) -> Result<Vec<PathBuf>, String> {
    match mode {
        Mode::Whole => Ok(vec![path.to_path_buf()]),
        Mode::Contents => {
            let entries = fs::read_dir(path).map_err(|e| e.to_string())?;
            Ok(entries.flatten().map(|e| e.path()).collect())
        }
    }
}

/// Whether this process may read the folders macOS guards behind Full Disk Access. Without it, listing them fails
/// with "permission denied" and a scan silently misses what is inside.
pub fn has_full_disk_access(home: &Path) -> bool {
    ["Library/Safari", "Library/Mail", ".Trash"]
        .iter()
        .map(|rel| fs::read_dir(home.join(rel)))
        .all(|r| !matches!(r, Err(e) if e.kind() == io::ErrorKind::PermissionDenied))
}

/// `PATH` plus the Homebrew and Docker folders an app opened from Finder does not inherit.
pub fn search_path(home: &Path) -> OsString {
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    let extra = [
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".orbstack/bin"),
        home.join(".docker/bin"),
        home.join("go/bin"),
        home.join(".cargo/bin"),
        home.join("Library/pnpm"),
        PathBuf::from("/usr/local/go/bin"),
        PathBuf::from("/Applications/Docker.app/Contents/Resources/bin"),
    ];
    for dir in extra {
        if !dirs.contains(&dir) && dir.is_dir() {
            dirs.push(dir);
        }
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

/// Whether `program` is installed on the search path.
pub fn has_program(program: &str, path: &OsString) -> bool {
    std::env::split_paths(path).any(|dir| dir.join(program).is_file())
}

/// Whether a Docker daemon is up (a socket exists), without starting `docker`.
pub fn docker_running(home: &Path) -> bool {
    [
        PathBuf::from("/var/run/docker.sock"),
        home.join(".docker/run/docker.sock"),
        home.join(".orbstack/run/docker.sock"),
    ]
    .iter()
    .any(|p| p.exists())
}

/// Stdout of a command that exited 0.
pub fn capture(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Free space on the data volume, in KB.
pub fn free_kb() -> Option<u64> {
    let out = capture("df", &["-k", "/System/Volumes/Data"])?;
    let line = out.lines().nth(1)?;
    line.split_whitespace().nth(3)?.parse().ok()
}

/// Where Cargo puts every workspace's build output, when `~/.cargo/config.toml` sets `build.build-dir` to a shared
/// template such as `/Volumes/Data/rust-build/{workspace-path-hash}`: the part before the first placeholder.
pub fn cargo_build_root(home: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(home.join(".cargo/config.toml")).ok()?;
    let value: toml::Table = toml::from_str(&text).ok()?;
    let template = value.get("build")?.get("build-dir")?.as_str()?;
    shared_build_root(template, home)
}

/// The fixed prefix of a `build-dir` template that names a placeholder; `None` for a per-workspace path.
pub fn shared_build_root(template: &str, home: &Path) -> Option<PathBuf> {
    let (prefix, _) = template.split_once('{')?;
    let prefix = prefix.trim_end_matches('/');
    let root = crate::rules::expand(prefix, home)?;
    root.is_absolute().then_some(root)
}

/// Lock every `.cargo-build-lock` in a build dir as Cargo does; `None` when a running cargo holds one. The locks are
/// released when the returned files drop.
pub fn lock_build_dir(dir: &Path) -> io::Result<Option<Vec<File>>> {
    let mut held = Vec::new();
    for entry in fs::read_dir(dir)?.flatten() {
        let lock = entry.path().join(".cargo-build-lock");
        if !lock.is_file() {
            continue;
        }
        let file = File::options().read(true).write(true).open(&lock)?;
        match file.try_lock() {
            Ok(()) => held.push(file),
            Err(fs::TryLockError::WouldBlock) => return Ok(None),
            Err(fs::TryLockError::Error(e)) => return Err(e),
        }
    }
    Ok(Some(held))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_build_root_is_the_prefix_before_the_placeholder() {
        let home = Path::new("/Users/me");
        assert_eq!(
            shared_build_root("/Volumes/Data/rust-build/{workspace-path-hash}", home),
            Some(PathBuf::from("/Volumes/Data/rust-build"))
        );
        assert_eq!(shared_build_root("~/builds/{workspace-path-hash}", home), Some(PathBuf::from("/Users/me/builds")));
        assert_eq!(shared_build_root("target-dir", home), None);
    }

    #[test]
    fn contents_mode_empties_and_keeps_the_folder() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("cleanup-mac-test-{}", std::process::id()));
        fs::create_dir_all(dir.join("sub"))?;
        fs::write(dir.join("a.log"), "x")?;
        fs::write(dir.join("sub/b.log"), "y")?;
        assert!(size_kb(&dir) > 0);
        remove(Mode::Contents, &dir)?;
        assert!(dir.is_dir());
        assert_eq!(fs::read_dir(&dir)?.count(), 0);
        remove(Mode::Whole, &dir)?;
        assert!(!dir.exists());
        Ok(())
    }
}
