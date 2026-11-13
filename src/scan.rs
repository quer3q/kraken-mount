//! Game discovery over the scan roots (DESIGN.md).
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Error, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Packing {
    Plain,
    Zstd,
    Lz4,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Pkg,
    Folder(Packing),
}

#[derive(Clone, Debug)]
pub struct Source {
    pub path: PathBuf,
    pub kind: Kind,
    pub title_id: String,
    pub title_name: String,
    pub version: String,
    /// Mount of the drive holding `path` ("/data", "/mnt/usb0").
    pub drive: String,
    /// .pkg file size; 0 for folders.
    pub size: u64,
    pub backport: Option<PathBuf>,
}

pub struct ScanResult {
    pub games: Vec<Source>,
    pub duplicates: Vec<Source>,
    pub errors: Vec<(PathBuf, String)>,
}

type PkgParam = Result<(String, String, String), String>;
type PkgMemo = HashMap<PathBuf, (u64, SystemTime, PkgParam)>;
static PKG_MEMO: OnceLock<Mutex<PkgMemo>> = OnceLock::new();

fn pkg_param(path: &Path, size: u64, mtime: SystemTime) -> PkgParam {
    let memo = PKG_MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    let mut memo = memo.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((old_size, old_mtime, result)) = memo.get(path) {
        if *old_size == size && *old_mtime == mtime {
            return result.clone();
        }
    }
    let result = crate::cache::peek_pkg(path);
    // Only deterministic results are memoized; a transient I/O error (USB waking up) retries next scan.
    let deterministic = match &result {
        Ok(_) => true,
        Err(e) => matches!(e.kind(), ErrorKind::InvalidData | ErrorKind::UnexpectedEof),
    };
    let result = result.map_err(|e| e.to_string());
    if deterministic {
        memo.insert(path.to_path_buf(), (size, mtime, result.clone()));
    } else {
        memo.remove(path);
    }
    result
}

/// SMP order (resolve_backport_path_for_title): the game's own scan root, the other roots,
/// then /data/homebrew (SMP's installed backports) even when custom scanpath lines replace it.
fn backport_for(owner: &Path, roots: &[String], tid: &str) -> Option<PathBuf> {
    let mut order: Vec<&Path> = vec![owner];
    for root in roots.iter().map(Path::new).chain([Path::new("/data/homebrew")]) {
        if !order.contains(&root) {
            order.push(root);
        }
    }
    for root in order {
        let dir = root.join("backports");
        let exact = dir.join(tid);
        if exact.is_dir() {
            return Some(exact);
        }
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        let mut matches: Vec<_> = entries.flatten().map(|e| e.path())
            .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(tid)))
            .collect();
        matches.sort();
        if let Some(path) = matches.into_iter().next() {
            return Some(path);
        }
    }
    None
}

/// .pkg path -> TID of each pkg cache dir, from the "source" of its `.done` stamp.
fn cached_pkgs() -> HashMap<PathBuf, String> {
    let Ok(rd) = fs::read_dir(crate::cache::CACHES) else { return HashMap::new() };
    rd.flatten()
        .filter_map(|e| {
            let tid = e.file_name().to_string_lossy().into_owned();
            let stamp: serde_json::Value = serde_json::from_slice(&fs::read(e.path().join(crate::cache::DONE)).ok()?).ok()?;
            if stamp["kind"] != "pkg" {
                return None;
            }
            Some((PathBuf::from(stamp["source"].as_str()?), tid))
        })
        .collect()
}

/// (title id, name, version) from the cache's sce_sys/param.json when that cache was built from `pkg`
/// after the pkg's last change and its stamp has the pkg's current size (a same-name replacement
/// that kept an older mtime differs in size). ponytail: trusts the cache; the .pkg is not opened then.
fn cached_param(cached: &HashMap<PathBuf, String>, pkg: &Path, size: u64, mtime: SystemTime) -> Option<(String, String, String)> {
    let dir = crate::cache::dir(cached.get(pkg)?);
    let done = dir.join(crate::cache::DONE);
    if fs::metadata(&done).and_then(|m| m.modified()).ok()? <= mtime {
        return None;
    }
    let stamp: serde_json::Value = serde_json::from_slice(&fs::read(&done).ok()?).ok()?;
    if stamp["size"].as_u64() != Some(size) {
        return None;
    }
    parse_param(&fs::read(dir.join("sce_sys/param.json")).ok()?).ok()
}

fn scan_error(result: &mut ScanResult, path: PathBuf, error: impl std::fmt::Display) {
    let message = error.to_string();
    // Log only new or changed errors per path: a bad entry would otherwise log every scan.
    let logged = LOGGED.get_or_init(|| Mutex::new(HashMap::new()));
    let mut logged = logged.lock().unwrap_or_else(|e| e.into_inner());
    if logged.get(&path) != Some(&message) {
        log!("{}: {message}", path.display());
        logged.insert(path.clone(), message.clone());
    }
    result.errors.push((path, message));
}

/// Last logged scan error per path.
static LOGGED: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();

pub fn scan(roots: &[String], backports: bool) -> ScanResult {
    let mut result = ScanResult { games: Vec::new(), duplicates: Vec::new(), errors: Vec::new() };
    let mut seen = HashSet::new();
    let cached = cached_pkgs();
    for root in roots {
        let root = Path::new(root);
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => {
                scan_error(&mut result, root.to_path_buf(), e);
                continue;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                // Dot entries (macOS ._X.pkg AppleDouble files) are skipped, as in SMP.
                Ok(entry) if entry.file_name().to_string_lossy().starts_with('.') => {}
                Ok(entry) => paths.push(entry.path()),
                Err(e) => scan_error(&mut result, root.to_path_buf(), e),
            }
        }
        paths.sort();
        for path in paths {
            let metadata = match fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(e) => {
                    scan_error(&mut result, path, e);
                    continue;
                }
            };
            let (kind, size, param) = if metadata.is_file()
                && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pkg")) {
                let mtime = match metadata.modified() {
                    Ok(mtime) => mtime,
                    Err(e) => {
                        scan_error(&mut result, path, e);
                        continue;
                    }
                };
                let param = match cached_param(&cached, &path, metadata.len(), mtime) {
                    Some(p) => Ok(p),
                    None => pkg_param(&path, metadata.len(), mtime),
                };
                (Kind::Pkg, metadata.len(), param)
            } else if metadata.is_dir() && path.file_name().is_some_and(|n| n != "backports")
                && path.join("eboot.bin").is_file() && path.join("sce_sys/param.json").is_file() {
                let packing = if path.join("ampr_zstd.index").is_file() {
                    Packing::Zstd
                } else if path.join("ampr_assets.index").is_file() {
                    Packing::Lz4
                } else {
                    Packing::Plain
                };
                (Kind::Folder(packing), 0, fs::read(path.join("sce_sys/param.json"))
                    .and_then(|json| parse_param(&json)).map_err(|e| e.to_string()))
            } else {
                continue;
            };
            let (title_id, title_name, version) = match param {
                Ok(param) => param,
                Err(e) => {
                    scan_error(&mut result, path, e);
                    continue;
                }
            };
            if !valid_tid(&title_id) {
                scan_error(&mut result, path, format!("invalid title id {title_id:?}"));
                continue;
            }
            let source = Source {
                drive: crate::storage::drive_of(&path),
                backport: if backports { backport_for(root, roots, &title_id) } else { None },
                path, kind, title_id, title_name, version, size,
            };
            if seen.insert(source.title_id.clone()) {
                result.games.push(source);
            } else {
                result.duplicates.push(source);
            }
        }
    }
    result.games.sort_by(|a, b| a.title_name.cmp(&b.title_name));
    // A fixed error that comes back is logged again.
    if let Some(logged) = LOGGED.get() {
        let errors: HashSet<&PathBuf> = result.errors.iter().map(|(p, _)| p).collect();
        logged.lock().unwrap_or_else(|e| e.into_inner()).retain(|p, _| errors.contains(p));
    }
    result
}
pub fn parse_param(json: &[u8]) -> io::Result<(String, String, String)> {
    let v: serde_json::Value = serde_json::from_slice(json)
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("param.json: {e}")))?;
    let tid = v["titleId"].as_str().or_else(|| v["title_id"].as_str()).unwrap_or_default();
    if !valid_tid(tid) {
        return Err(Error::new(ErrorKind::InvalidData, format!("param.json: bad titleId {tid:?}")));
    }
    let lp = &v["localizedParameters"];
    let name = lp["defaultLanguage"].as_str()
        .and_then(|lang| lp[lang]["titleName"].as_str())
        .filter(|name| !name.is_empty())
        .or_else(|| lp["en-US"]["titleName"].as_str().filter(|name| !name.is_empty()))
        .or_else(|| lp.as_object().and_then(|langs| langs.values()
            .filter_map(|lang| lang["titleName"].as_str()).find(|name| !name.is_empty())))
        .unwrap_or(tid);
    Ok((tid.to_owned(), name.to_owned(), v["contentVersion"].as_str().unwrap_or_default().to_owned()))
}
/// Game title ids only: NPXS ids are system apps (SMP is_supported_game_title_id).
pub fn valid_tid(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() == 9 && b[..4].iter().all(u8::is_ascii_uppercase) && b[4..].iter().all(u8::is_ascii_digit)
        && !t.starts_with("NPXS")
}
