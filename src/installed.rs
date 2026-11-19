//! Packages installed by the system, `<app root>/<TID>/app.pkg` (DESIGN.md "Scan roots").
//! They are ordinary pkg sources: the package stays where the system put it, Kraken Mount only
//! builds the cache and registers the tile from it. Nothing here copies, moves or deletes one.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The system's app roots, internal first: /user/app and the extended-storage layouts (SMP's
/// installed checks use the same four drive paths).
const ROOTS: [&str; 5] = [
    "/user/app",
    "/mnt/ext0/user/app",
    "/mnt/ext0/ps5/user/app",
    "/mnt/ext1/user/app",
    "/mnt/ext1/ps5/user/app",
];

/// The read bits the cache build and the game's services (another uid) need.
const FILE_BITS: u32 = 0o444;
/// The same for the package's own title directory (no traversal without it).
const DIR_BITS: u32 = 0o555;

/// True for exactly `<app root>/<TID>/app.pkg` with a valid game title id: the layout of an
/// installed package, never a .pkg file picked up from a scan root.
pub fn package_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    ROOTS.iter().any(|root| {
        let Some(rest) = text.strip_prefix(root).and_then(|r| r.strip_prefix('/')) else { return false };
        let mut parts = rest.split('/');
        matches!((parts.next(), parts.next(), parts.next()), (Some(tid), Some("app.pkg"), None)
            if crate::scan::valid_tid(tid))
    })
}

/// Every installed package, in title-id order: (path, title id). A regular `app.pkg` only:
/// a symlink, a missing entry or another file name is left to the system.
pub fn paths() -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    for root in ROOTS {
        let Ok(entries) = fs::read_dir(root) else { continue };
        for entry in entries.flatten() {
            let tid = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path().join("app.pkg");
            if !crate::scan::valid_tid(&tid)
                || !entry.file_type().is_ok_and(|t| t.is_dir())
                || !fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
            {
                continue;
            }
            out.push((path, tid));
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// Adds the missing read bits to the package and its title dir, so the pkg can be opened (an
/// install can be 0700/0600 for another uid). Only bits are added, never taken away, like
/// cache::fix_access does for a scanned pkg. A path/title/size mismatch, a symlink or a
/// non-regular file is refused and left untouched.
pub fn prepare(path: &Path, tid: &str, size: u64) -> io::Result<()> {
    if !package_path(path)
        || path.file_name().is_none_or(|n| n != "app.pkg")
        || path.parent().and_then(Path::file_name).is_none_or(|n| n != tid)
    {
        return Err(io::Error::other("not an installed package path for this title"));
    }
    let file = fs::symlink_metadata(path)?;
    let parent = path.parent().ok_or_else(|| io::Error::other("installed package has no directory"))?;
    let dir = fs::symlink_metadata(parent)?;
    if !file.is_file() || file.file_type().is_symlink() || !dir.is_dir() || dir.file_type().is_symlink() {
        return Err(io::Error::other("installed package is not a regular file and directory"));
    }
    if file.len() != size {
        return Err(io::Error::other("installed package changed since it was read"));
    }
    add_bits(parent, &dir, DIR_BITS)?;
    add_bits(path, &file, FILE_BITS)
}

/// Adds `want` to the mode of `path` when any of those bits is missing; a chmod error is
/// returned with the path. The entry is used as stat'ed above (never followed).
fn add_bits(path: &Path, meta: &fs::Metadata, want: u32) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mode = meta.mode() & 0o7777;
    if mode & want == want {
        return Ok(());
    }
    fs::set_permissions(path, fs::Permissions::from_mode(mode | want))
        .map_err(|e| io::Error::new(e.kind(), format!("chmod {}: {e}", path.display())))
}

