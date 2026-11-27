//! Reads a game image without attaching it, through ps5-dump-forge's readers: `ps5-dump-forge-pfs`
//! for `.ffpfs` and `.ffpfsc` (`open_ffpfsc`), `ps5upload-fpkg` (`exfat`, `ufs2_source`) for `.exfat`
//! and `.ffpkg`. The scan uses it to check an image and read its `sce_sys/param.json` before
//! `image::ensure` attaches and mounts it.
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use ps5upload_fpkg::exfat::ExFatSource;
use ps5upload_fpkg::source::{SourceTree, MAX_PARAM_BYTES};
use ps5upload_fpkg::ufs2_source::Ufs2Source;

use super::Fs;

/// What the image holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// The game folder inside the (inner) image: "" for its root, else the name of the first folder
    /// directly inside it (sorted) with `eboot.bin` and `sce_sys/param.json`.
    pub game: String,
    /// `sce_sys/param.json` of that folder.
    pub param: Vec<u8>,
    /// For a `.ffpfsc`: the name of the one image in its root (`PPSA01234.exfat`, `pfs_image.dat`).
    pub inner: Option<String>,
    /// The game folder has `ampr_zstd.index` / `ampr_assets.index` (scan::packing, read from the
    /// image): its rank is known before it is attached.
    pub zstd: bool,
    pub lz4: bool,
}

/// The game folder in a tree's file list: its root, else the first such folder directly inside it.
fn game_of(files: &[String]) -> Option<String> {
    let has = |p: String| files.iter().any(|f| *f == p);
    let is_game = |d: &str| has(format!("{d}eboot.bin")) && has(format!("{d}sce_sys/param.json"));
    if is_game("") {
        return Some(String::new());
    }
    let mut dirs: Vec<&str> = files.iter().filter_map(|f| f.split_once('/').map(|(d, _)| d))
        .filter(|d| !d.starts_with('.')).collect();
    dirs.sort_unstable();
    dirs.dedup();
    dirs.into_iter().find(|d| is_game(&format!("{d}/"))).map(str::to_string)
}

fn open(image: &Path, fs: Fs) -> ps5upload_fpkg::Result<(Box<dyn SourceTree>, Option<String>)> {
    let label = image.display().to_string();
    Ok(match fs {
        Fs::Exfat => (Box::new(ExFatSource::open(image)?), None),
        Fs::Ufs => (Box::new(Ufs2Source::open(image)?), None),
        Fs::Pfs => (Box::new(ps5_dump_forge_pfs::PfsSource::from_reader(Box::new(File::open(image)?), label)?), None),
        Fs::Pfsc => {
            let (tree, info) = ps5_dump_forge_pfs::open_ffpfsc(Box::new(File::open(image)?), &label)?;
            (tree, Some(info.inner_name))
        }
    })
}

fn read(image: &Path, fs: Fs) -> io::Result<Probe> {
    let bad = |m: String| Error::new(ErrorKind::InvalidData, m);
    // The readers start their messages with the path; the scan's log line has it already.
    let label = format!("{}: ", image.display());
    let (mut tree, inner) = open(image, fs).map_err(|e| match e {
        ps5upload_fpkg::Error::Io(io) => io,
        other => {
            let m = other.to_string();
            bad(m.strip_prefix(&label).map(str::to_string).unwrap_or(m))
        }
    })?;
    let files: Vec<String> = tree.files().iter().map(|f| f.path.clone()).collect();
    let game = game_of(&files).ok_or_else(|| bad("no game folder (eboot.bin + sce_sys/param.json) in the image".into()))?;
    let param_path = if game.is_empty() { "sce_sys/param.json".to_string() } else { format!("{game}/sce_sys/param.json") };
    let size = tree.files().iter().find(|f| f.path == param_path).map_or(0, |f| f.size);
    if size > MAX_PARAM_BYTES {
        return Err(bad(format!("{param_path}: {size} bytes")));
    }
    let param = tree.read(&param_path).map_err(|e| bad(format!("{param_path}: {e}")))?;
    let prefix = if game.is_empty() { String::new() } else { format!("{game}/") };
    let has = |name: &str| files.iter().any(|f| *f == format!("{prefix}{name}"));
    let (zstd, lz4) = (has("ampr_zstd.index"), has("ampr_assets.index"));
    Ok(Probe { game, param, inner, zstd, lz4 })
}

type Key = (PathBuf, u64, i64);

/// One probe per (path, size, mtime), errors included: an image is walked again only when it
/// changes, not every scan pass.
static CACHE: OnceLock<Mutex<HashMap<Key, Result<Probe, (ErrorKind, String)>>>> = OnceLock::new();

/// The game in `image` (format `fs`), read without attaching it. `size` and `mtime` are the file's.
pub fn probe(image: &Path, fs: Fs, size: u64, mtime: i64) -> io::Result<Probe> {
    let key = (image.to_path_buf(), size, mtime);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(r) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return r.clone().map_err(|(k, m)| Error::new(k, m));
    }
    let r = read(image, fs);
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    // An older version of the same file is not read again.
    cache.retain(|k, _| k.0 != key.0);
    cache.insert(key, r.as_ref().map(Probe::clone).map_err(|e| (e.kind(), e.to_string())));
    r
}

#[cfg(test)]
mod tests {
    use super::game_of;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn root_first_then_sorted_folder() {
        assert_eq!(game_of(&v(&["eboot.bin", "sce_sys/param.json", "B/eboot.bin", "B/sce_sys/param.json"])), Some(String::new()));
        assert_eq!(game_of(&v(&["B/eboot.bin", "B/sce_sys/param.json", "A/eboot.bin", "A/sce_sys/param.json"])), Some("A".into()));
        assert_eq!(game_of(&v(&["A/eboot.bin", "B/eboot.bin", "B/sce_sys/param.json"])), Some("B".into()));
        assert_eq!(game_of(&v(&["A/B/eboot.bin", "A/B/sce_sys/param.json", ".x/eboot.bin", ".x/sce_sys/param.json"])), None);
    }
}
