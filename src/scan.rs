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
    /// A game installed on the console (installed.rs): /user/app/<TID> or /mnt/extN/user/app/<TID>.
    Installed,
}

/// Duplicate rank, lower wins (claim).
pub fn rank(kind: Kind) -> u8 {
    match kind {
        Kind::Installed => 0,
        Kind::Pkg => 1,
        Kind::Folder(Packing::Zstd) => 2,
        Kind::Folder(Packing::Lz4) => 3,
        Kind::Folder(Packing::Plain) => 4,
    }
}

/// An internal console install run through ng (ampr_installed): a pkg source at
/// /user/app/<TID>/app.pkg. The console's registration stays; only /system_ex/app/<TID> is ours.
pub fn installed_pkg(src: &Source) -> bool {
    src.kind == Kind::Pkg && src.path.starts_with("/user/app")
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
    /// .pkg or image file size; 0 for folders.
    pub size: u64,
    pub backport: Option<PathBuf>,
    /// The .exfat/.ffpkg/.ffpfs/.ffpfsc image holding the game folder `path` (mounted under
    /// image::MOUNT_BASE; a .ffpfsc's game is in its inner image's mount).
    pub image: Option<PathBuf>,
}

impl Source {
    /// What lies in the scan root: an image game's image file, else `path`. `exclude` entries and
    /// deletes go by it.
    pub fn origin(&self) -> &Path {
        self.image.as_deref().unwrap_or(&self.path)
    }
}

pub struct ScanResult {
    pub games: Vec<Source>,
    pub duplicates: Vec<Source>,
    pub errors: Vec<(PathBuf, String)>,
    /// Sources still being copied (stability_wait_seconds): path and title id when known.
    /// Skipped this pass, but not gone: the caller keeps their entry, registration and cache.
    pub unstable: Vec<(PathBuf, Option<String>)>,
    /// Patch and DLC .pkg files in the scan roots (same candidates, exclusions and stability wait).
    pub packages: Vec<Package>,
}

/// Seconds since the youngest max(mtime, ctime) of `paths` when that is under `wait` (SMP
/// sm_stability.c); None = stable. A clock in the future (negative age) counts as stable, as in SMP.
/// A path that cannot be stat'ed is ignored here (the scan reports it).
pub fn still_written(paths: &[PathBuf], wait: u64) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    if wait == 0 {
        return None;
    }
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs() as i64;
    paths.iter().filter_map(|p| fs::metadata(p).ok())
        .map(|m| now - m.mtime().max(m.ctime()))
        .filter(|&age| (0..wait as i64).contains(&age))
        .min()
        .map(|age| age as u64)
}

/// Records an unstable source; logs it once until it is stable again.
fn skip_unstable(result: &mut ScanResult, path: PathBuf, tid: Option<String>, age: u64) {
    let logged = UNSTABLE.get_or_init(|| Mutex::new(HashSet::new()));
    if logged.lock().unwrap_or_else(|e| e.into_inner()).insert(path.clone()) {
        let label = tid.clone().unwrap_or_else(|| path.display().to_string());
        log!("{label}: source still being written (changed {age} s ago), waiting");
    }
    result.unstable.push((path, tid));
}

/// Unstable sources already logged.
static UNSTABLE: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

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

/// Bytes of a CNT header read to classify a package (SMP sm_pkg.c PKG_HEADER_SIZE).
const CNT_LEN: usize = 0xA0;
/// Content flags of a patch (SMP sm_pkg.c PKG_PATCH_FLAG_MASK).
const PKG_PATCH_FLAG_MASK: u32 = 0x4010_0000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PkgKind {
    Base,
    Patch,
    Dlc,
}

/// A patch or DLC .pkg in a scan root: listed on the Install tab, never a game source.
#[derive(Clone, Debug)]
pub struct Package {
    pub path: PathBuf,
    pub kind: PkgKind,
    pub content_id: String,
    pub title_id: String,
    pub title_name: String,
    pub version: String,
    pub size: u64,
    pub drive: String,
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

fn le64(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at.checked_add(8)?)?.try_into().ok()?))
}

/// Offset of the CNT header from a package's first bytes (SMP parse_package): `\x7FFIH` version 3
/// -> LE u64 at 0x58; `\x7FLIH` version 1 -> LE u64 at 0x30 (a delta patch); `\x7FCNT` -> 0.
/// None for any other magic or version, or an offset inside the outer header.
pub fn cnt_offset(outer: &[u8]) -> Option<(u64, bool)> {
    let version = || outer.get(6..8).map(|v| u16::from_le_bytes([v[0], v[1]]));
    let wrapped = |at| le64(outer, at).filter(|&base| base >= CNT_LEN as u64);
    match outer.get(..4)? {
        b"\x7FFIH" if version() == Some(3) => Some((wrapped(0x58)?, false)),
        b"\x7FLIH" if version() == Some(1) => Some((wrapped(0x30)?, true)),
        b"\x7FCNT" => Some((0, false)),
        _ => None,
    }
}

/// SMP sm_pkg_is_media_remaster: content type 0x26 with both remaster flags.
fn is_media_remaster(content_type: u32, flags: u32) -> bool {
    content_type == 0x26 && flags & 0x4040_0000 == 0x4040_0000
}

/// The title id in a content id (`UP0000-PPSA01234_00-...`: chars 7..16).
fn title_of(content_id: &str) -> Option<String> {
    content_id.get(7..16).filter(|t| valid_tid(t)).map(str::to_owned)
}

/// Package kind and content id from a CNT header (SMP sm_pkg.c parse_package): content type 0x1B,
/// 0x1C, 0x21 -> Dlc; a delta (LIH, container type low byte 0x1E or high byte 0x41), category
/// gp/gpd/ps5dp, or the patch flags outside a media remaster -> Patch; else Base. `category` is
/// the pkg's param.sfo CATEGORY, "" when unknown. None without a valid content id holding a game
/// title id. A base CATEGORY (gd, gde, gdc, gdp) that disagrees with a Patch from the flags
/// gives Base: SMP marks that kind not confident and skips the package, here it goes the FPKG
/// path as on main. ponytail: the rest of SMP's confidence check (known content types, other
/// disagreeing categories) is not ported; an unusual base package is still Base.
pub fn classify(cnt: &[u8], delta: bool, category: &str) -> Option<(PkgKind, String)> {
    if cnt.len() < CNT_LEN || &cnt[..4] != b"\x7FCNT" {
        return None;
    }
    let id = &cnt[0x40..0x40 + 36];
    let content_id = std::str::from_utf8(&id[..id.iter().position(|&b| b == 0).unwrap_or(36)]).ok()?;
    if !content_id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
        return None;
    }
    title_of(content_id)?;
    let (content_type, flags, container) = (be32(cnt, 0x74)?, be32(cnt, 0x78)?, be32(cnt, 4)?);
    let delta = delta || container & 0xFF == 0x1E || container & 0xFF00_0000 == 0x4100_0000;
    let patch_category = ["gp", "gpd", "ps5dp"].iter().any(|c| category.eq_ignore_ascii_case(c));
    let kind = if matches!(content_type, 0x1B | 0x1C | 0x21) {
        PkgKind::Dlc
    } else if delta || patch_category || (!is_media_remaster(content_type, flags) && flags & PKG_PATCH_FLAG_MASK != 0
        && !["gd", "gde", "gdc", "gdp"].iter().any(|c| category.eq_ignore_ascii_case(c))) {
        PkgKind::Patch
    } else {
        PkgKind::Base
    };
    Some((kind, content_id.to_owned()))
}

/// A string value (format 0x0004 or 0x0204) of a param.sfo (SMP parse_sfo); None when absent.
fn sfo_string(data: &[u8], key: &str) -> Option<String> {
    let le32 = |at: usize| data.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    if data.get(..4)? != b"\0PSF" {
        return None;
    }
    let (keys, values, count) = (le32(8)?, le32(12)?, le32(16)?);
    let cstr = |b: &[u8]| b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())].to_vec();
    for e in (0..count.min(data.len().saturating_sub(20) / 16)).map(|i| 20 + i * 16) {
        let key_rel = u16::from_le_bytes([data[e], data[e + 1]]) as usize;
        let format = u16::from_le_bytes([data[e + 2], data[e + 3]]);
        let (Some(length), Some(value_rel)) = (le32(e + 4), le32(e + 12)) else { continue };
        let Some(name) = data.get(keys.saturating_add(key_rel)..) else { continue };
        if cstr(name) != key.as_bytes() || !matches!(format, 0x0004 | 0x0204) {
            continue;
        }
        let value = data.get(values.saturating_add(value_rel)..).and_then(|v| v.get(..length));
        return value.map(|v| String::from_utf8_lossy(&cstr(v)).into_owned());
    }
    None
}

/// (kind, content id, title id, name, version) of a classified package.
type PkgClass = (PkgKind, String, String, String, String);
type ClassMemo = HashMap<PathBuf, (u64, SystemTime, Option<PkgClass>)>;
static CLASS_MEMO: OnceLock<Mutex<ClassMemo>> = OnceLock::new();

/// `read_class`, memoized per (size, mtime) as `pkg_param`; an I/O error is not memoized and
/// gives None this pass (the FPKG path then reports it).
fn pkg_class(path: &Path, size: u64, mtime: SystemTime) -> Option<PkgClass> {
    let memo = CLASS_MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    let mut memo = memo.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((old_size, old_mtime, class)) = memo.get(path) {
        if *old_size == size && *old_mtime == mtime {
            return class.clone();
        }
    }
    match read_class(path) {
        Ok(class) => {
            memo.insert(path.to_path_buf(), (size, mtime, class.clone()));
            class
        }
        Err(_) => {
            memo.remove(path);
            None
        }
    }
}

/// Whether the memoized class of `path` (any size and mtime) is a patch or DLC.
fn memo_is_package(path: &Path) -> bool {
    let memo = CLASS_MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    let memo = memo.lock().unwrap_or_else(|e| e.into_inner());
    memo.get(path).is_some_and(|(_, _, c)| c.as_ref().is_some_and(|c| c.0 != PkgKind::Base))
}

/// A package classified from its outer and CNT headers (SMP parse_package); None when it has no
/// CNT header `classify` accepts. Category, name and version come from the CNT entry table
/// (`pkg_param_entries`).
fn read_class(path: &Path) -> io::Result<Option<PkgClass>> {
    use std::os::unix::fs::FileExt;
    let f = fs::File::open(path)?;
    let len = f.metadata()?.len();
    let mut head = [0u8; CNT_LEN];
    if len < CNT_LEN as u64 {
        return Ok(None);
    }
    f.read_exact_at(&mut head, 0)?;
    let Some((base, delta)) = cnt_offset(&head) else { return Ok(None) };
    if base > len - CNT_LEN as u64 {
        return Ok(None);
    }
    if base != 0 {
        f.read_exact_at(&mut head, base)?;
    }
    if &head[..4] != b"\x7FCNT" {
        return Ok(None);
    }
    let (category, param) = pkg_param_entries(&f, len, base, &head);
    let Some((kind, content_id)) = classify(&head, delta, &category) else { return Ok(None) };
    let title_id = title_of(&content_id).unwrap_or_default();
    // ponytail: an unreadable param shows the title id and no version.
    let (name, version) = param.unwrap_or_else(|| (title_id.clone(), String::new()));
    Ok(Some((kind, content_id, title_id, name, version)))
}

/// (param.sfo CATEGORY or "", (name, version)) from the CNT entry table at `base` (SMP
/// read_pkg_sfo): param.json (our `parse_param`) first, else param.sfo TITLE and APP_VER.
/// Unreadable entries give "" and None.
fn pkg_param_entries(f: &fs::File, len: u64, base: u64, cnt: &[u8]) -> (String, Option<(String, String)>) {
    use std::os::unix::fs::FileExt;
    let (Some(count), Some(table)) = (be32(cnt, 0x10), be32(cnt, 0x18)) else { return (String::new(), None) };
    let room = len - base;
    if count == 0 || count > 1024 || table as u64 > room || count as u64 > (room - table as u64) / 0x20 {
        return (String::new(), None);
    }
    let mut entries = vec![0u8; count as usize * 0x20];
    if f.read_exact_at(&mut entries, base + table as u64).is_err() {
        return (String::new(), None);
    }
    let (mut category, mut sfo, mut json) = (String::new(), None, None);
    for e in entries.chunks_exact(0x20) {
        let (Some(id), Some(offset), Some(length)) = (be32(e, 0), be32(e, 0x10), be32(e, 0x14)) else { continue };
        if !matches!(id, 0x1000 | 0x2000) || length == 0 || length > 1 << 20 || offset as u64 > room
            || length as u64 > room - offset as u64 {
            continue;
        }
        let mut data = vec![0u8; length as usize];
        if f.read_exact_at(&mut data, base + offset as u64).is_err() {
            continue;
        }
        if id == 0x1000 {
            category = sfo_string(&data, "CATEGORY").unwrap_or_default();
            sfo = sfo_string(&data, "TITLE").map(|t| (t, sfo_string(&data, "APP_VER").unwrap_or_default()));
        } else {
            let end = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
            json = parse_param(&data[..end]).ok().map(|(_, name, version)| (name, version));
        }
    }
    (category, json.or(sfo))
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

/// Source path (.pkg or folder) -> TID of each cache dir, from the "source" of its `.done` stamp.
fn cached_sources() -> HashMap<PathBuf, String> {
    let Ok(rd) = fs::read_dir(crate::cache::CACHES) else { return HashMap::new() };
    rd.flatten()
        .filter_map(|e| {
            let tid = e.file_name().to_string_lossy().into_owned();
            let stamp: serde_json::Value = serde_json::from_slice(&fs::read(e.path().join(crate::cache::DONE)).ok()?).ok()?;
            Some((PathBuf::from(stamp["source"].as_str()?), tid))
        })
        .collect()
}

/// (title id, name, version) from the cache's sce_sys/param.json when that cache was built from `pkg`
/// after the pkg's last change and its stamp has the pkg's current size (a same-name replacement
/// that kept an older mtime differs in size). ponytail: trusts the cache; the .pkg is opened only
/// for its memoized class (`pkg_class`), not for its param.
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
    parse_param(&read_param(&dir).ok()?).ok()
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

/// Duplicate rule: the lowest `rank` wins; on an equal rank the first source in scan order wins
/// and the others go to `duplicates`. A source still being copied (`source` None) claims its title
/// id the same way, so a mid-copy winner keeps the other copy out of the library (an unstable
/// loser claims nothing); it itself goes only to `unstable`. A `nested` source (one folder below
/// a root, scanned after every root's own entries) never beats a holder: an old copy kept in a
/// subfolder cannot take over a game that already runs from a root.
fn claim<T>(seen: &mut HashMap<String, (u8, Option<usize>)>, games: &mut Vec<Option<T>>,
            duplicates: &mut Vec<T>, tid: &str, rank: u8, nested: bool, source: Option<T>) {
    match seen.get(tid) {
        Some(_) if nested => duplicates.extend(source),
        Some(&(held, _)) if rank >= held => duplicates.extend(source),
        held => {
            if let Some(&(_, Some(i))) = held {
                duplicates.extend(games[i].take());
            }
            let slot = source.map(|s| {
                games.push(Some(s));
                games.len() - 1
            });
            seen.insert(tid.to_string(), (rank, slot));
        }
    }
}

/// Claims before any root entry: the title being installed now is reserved at rank 0 with no
/// source (its FPKG becomes a duplicate and is not registered again mid-install), then each
/// installed game (installed::list, which leaves out titles with a marker) at rank 0.
fn claim_installed<T>(seen: &mut HashMap<String, (u8, Option<usize>)>, games: &mut Vec<Option<T>>,
                      duplicates: &mut Vec<T>, installing: Option<&str>, installed: Vec<(String, T)>) {
    if let Some(tid) = installing {
        seen.insert(tid.to_string(), (0, None));
    }
    for (tid, source) in installed {
        claim(seen, games, duplicates, &tid, 0, false, Some(source));
    }
}

/// Last logged scan error per path.
static LOGGED: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();

/// Sorted direct entries of `dir` without dot entries (macOS ._X.pkg AppleDouble files, as in
/// SMP); None when `dir` is gone.
fn list(dir: &Path, result: &mut ScanResult) -> Option<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return None,
        Err(e) => {
            scan_error(result, dir.to_path_buf(), e);
            return None;
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) if entry.file_name().to_string_lossy().starts_with('.') => {}
            Ok(entry) => paths.push(entry.path()),
            Err(e) => scan_error(result, dir.to_path_buf(), e),
        }
    }
    paths.sort();
    Some(paths)
}

/// `backports` in any case: an exFAT drive finds `Backports` under that name too (backport_for).
fn is_backports(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n.eq_ignore_ascii_case("backports"))
}

/// Candidate sources of one root: (its own entries sorted, the nested ones). Nested = the
/// entries, sorted, of each subfolder that is not a game folder, not `backports` and not itself a
/// root (SMP scan_depth=2). A path already in `listed` (this pass) is left out.
/// ponytail: one level below a root only (SMP's default depth), and none below a drive root
/// (/mnt/usbN, /mnt/extN): its homebrew and etaHEN/games are roots of their own, and walking
/// every top folder of a drive (Movies/...) each pass costs too much on USB. A game two folders
/// down, or in another folder at a drive's root, needs its own scanpath line.
fn candidates(root: &Path, roots: &[String], exclude: &[String], listed: &mut HashSet<PathBuf>, result: &mut ScanResult) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let (mut top, mut nested) = (Vec::new(), Vec::new());
    // Excluded paths are never opened: no listing, no scan error for an unreadable one.
    if excluded(root, exclude) {
        return (top, nested);
    }
    let drive_root = root.parent() == Some(Path::new("/mnt"));
    for path in list(root, result).unwrap_or_default() {
        if is_backports(&path) || excluded(&path, exclude) || !listed.insert(path.clone()) {
            continue;
        }
        let descend = !drive_root && path.is_dir()
            && !(path.join("eboot.bin").is_file() && path.join("sce_sys/param.json").is_file())
            && !roots.iter().any(|r| Path::new(r) == path);
        if descend {
            for child in list(&path, result).unwrap_or_default() {
                if !is_backports(&child) && !excluded(&child, exclude) && listed.insert(child.clone()) {
                    nested.push(child);
                }
            }
        }
        top.push(path);
    }
    (top, nested)
}

/// Whether `path` is an `exclude` entry or lies under one (whole components, so /data/homebrew/A
/// does not cover /data/homebrew/AB; a trailing `/` on an entry does not matter).
pub fn excluded(path: &Path, exclude: &[String]) -> bool {
    exclude.iter().any(|e| path.starts_with(e))
}

/// Finds the sources under `roots`: the .pkg files, image files (`crate::image`) and game folders (eboot.bin and
/// sce_sys/param.json) directly in a root, and those one folder below it (see `candidates`).
/// A source in `exclude` (see `excluded`) is skipped before it can claim its title id.
/// Installed games (installed::list) claim first, at rank 0; `installing` (the title whose base
/// install runs now) is reserved with no source (`claim_installed`). A patch or DLC .pkg goes to
/// `packages`, never to `games` or `errors`.
pub fn scan(roots: &[String], exclude: &[String], backports: bool, stability_wait: u64, installing: Option<&str>, ng_installed: &dyn Fn(&str) -> bool) -> ScanResult {
    let mut result = ScanResult { games: Vec::new(), duplicates: Vec::new(), errors: Vec::new(), unstable: Vec::new(), packages: Vec::new() };
    // Title id -> (rank of the winner, its index in `games`; None while the winner is still being
    // copied or the title is being installed).
    let mut seen: HashMap<String, (u8, Option<usize>)> = HashMap::new();
    // A slot is emptied when a later source beats it.
    let mut games: Vec<Option<Source>> = Vec::new();
    // ponytail: an installed game's backport is looked up from the first scan root (backport_for
    // then tries the other roots and /data/homebrew).
    let owner = Path::new(roots.first().map_or("/data/homebrew", String::as_str));
    let installed = crate::installed::list().into_iter()
        .filter(|g| Some(g.title_id.as_str()) != installing)
        .map(|g| {
            // An internal debug FPKG install with ampr_installed (per title): its app.pkg is a pkg
            // source (ng pkg mode), see installed_pkg. Retail and extended-storage installs stay native.
            // A header that cannot be read this pass keeps the last build's kind (no rebuild flip-flop).
            let as_pkg = g.path.starts_with("/user/app") && ng_installed(&g.title_id)
                && crate::installed::debug_of(&g.path.join("app.pkg"))
                    .unwrap_or_else(|| crate::cache::stamp_kind(&g.title_id).as_deref() == Some("pkg"));
            (g.title_id.clone(), Source {
                drive: crate::storage::drive_of(&g.path),
                backport: if backports { backport_for(owner, roots, &g.title_id) } else { None },
                kind: if as_pkg { Kind::Pkg } else { Kind::Installed }, size: g.size,
                path: if as_pkg { g.path.join("app.pkg") } else { g.path },
                title_id: g.title_id, title_name: g.title_name, version: g.version, image: None,
            })
        })
        .collect();
    claim_installed(&mut seen, &mut games, &mut result.duplicates, installing, installed);
    let cached = cached_sources();
    // Every root's own entries first, in root order, then the nested ones: a nested copy never
    // beats a source found directly in a root (claim).
    let (mut listed, mut order, mut later) = (HashSet::new(), Vec::new(), Vec::new());
    for root in roots {
        let root = Path::new(root);
        let (top, nested) = candidates(root, roots, exclude, &mut listed, &mut result);
        order.extend(top.into_iter().map(|p| (root, p, false)));
        later.extend(nested.into_iter().map(|p| (root, p, true)));
    }
    order.extend(later);
    for (root, mut path, nested) in order {
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) => {
                scan_error(&mut result, path, e);
                continue;
            }
        };
        let mut image = None;
        let (kind, size, param) = if metadata.is_file() && crate::image::is_image(&path) {
            // Before the image is attached: a half-copied image is not mounted. The title id is
            // the one of an earlier cache of a folder in its mount point, if any.
            if let Some(age) = still_written(std::slice::from_ref(&path), stability_wait) {
                let mp = crate::image::mount_point(&path);
                let tid = cached.iter().find(|(s, _)| s.starts_with(&mp)).map(|(_, t)| t.clone());
                if let Some(t) = &tid {
                    claim(&mut seen, &mut games, &mut result.duplicates, t, rank(Kind::Folder(Packing::Plain)), nested, None);
                }
                skip_unstable(&mut result, path, tid, age);
                continue;
            }
            // Read without attaching it (ps5-dump-forge's readers): an image without a game, or
            // with a bad param.json, is never mounted.
            let found = crate::image::probe(&path)
                .and_then(|p| parse_param(&p.param).map(|param| (p, param)));
            let (probe, param) = match found {
                Ok(found) => found,
                Err(e) => {
                    scan_error(&mut result, path, e);
                    continue;
                }
            };
            // A copy that already loses its title id (an installed game, a pkg, an earlier root) is
            // not attached: a duplicate image stays a file until it can win (claim's rule, with the
            // packing read from the image).
            let kind = Kind::Folder(if probe.zstd { Packing::Zstd } else if probe.lz4 { Packing::Lz4 } else { Packing::Plain });
            let (tid, title_name, version) = &param;
            if valid_tid(tid) && seen.get(tid).is_some_and(|&(held, _)| nested || rank(kind) >= held) {
                result.duplicates.push(Source {
                    drive: crate::storage::drive_of(&path),
                    backport: if backports { backport_for(root, roots, tid) } else { None },
                    path: crate::image::dir_of(&path, &probe), kind, title_id: tid.clone(),
                    title_name: title_name.clone(), version: version.clone(), size: metadata.len(), image: Some(path),
                });
                continue;
            }
            let dir = match crate::image::ensure(&path, &probe) {
                Ok(dir) => dir,
                Err(e) => {
                    scan_error(&mut result, path, e);
                    continue;
                }
            };
            let param: Result<_, String> = Ok(param);
            image = Some(std::mem::replace(&mut path, dir));
            (Kind::Folder(packing(&path)), metadata.len(), param)
        } else if metadata.is_file()
            && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pkg")) {
            // Before the pkg is opened: a half-copied header is not parsed. The title id is
            // the one of an earlier cache of this path, if any.
            if let Some(age) = still_written(std::slice::from_ref(&path), stability_wait) {
                // A path last classified as a patch or DLC reserves no title id (an older build
                // may have cached it as its base game).
                let tid = cached.get(&path).cloned().filter(|_| !memo_is_package(&path));
                if let Some(t) = &tid {
                    claim(&mut seen, &mut games, &mut result.duplicates, t, rank(Kind::Pkg), nested, None);
                }
                skip_unstable(&mut result, path, tid, age);
                continue;
            }
            let mtime = match metadata.modified() {
                Ok(mtime) => mtime,
                Err(e) => {
                    scan_error(&mut result, path, e);
                    continue;
                }
            };
            // Classified from its headers first (memoized), with or without a cache: a patch .pkg
            // cached as a game by an older build is a package. ponytail: its old cache and
            // registration stay; auto-remove fires only when the source is gone.
            if let Some((kind @ (PkgKind::Patch | PkgKind::Dlc), content_id, title_id, title_name, version)) =
                pkg_class(&path, metadata.len(), mtime) {
                result.packages.push(Package {
                    drive: crate::storage::drive_of(&path), size: metadata.len(),
                    path, kind, content_id, title_id, title_name, version,
                });
                continue;
            }
            let param = match cached_param(&cached, &path, metadata.len(), mtime) {
                Some(p) => Ok(p),
                None => pkg_param(&path, metadata.len(), mtime),
            };
            (Kind::Pkg, metadata.len(), param)
        } else if metadata.is_dir() && !is_backports(&path)
            && path.join("eboot.bin").is_file() && path.join("sce_sys/param.json").is_file() {
            // ponytail: only the folder itself, sce_sys/, its param.json and eboot.bin are checked, so
            // a big file deep inside can still be in flight; a full walk per pass costs too much on USB.
            let probe = [path.clone(), path.join("sce_sys"), path.join("sce_sys/param.json"), path.join("eboot.bin")];
            if let Some(age) = still_written(&probe, stability_wait) {
                // A param.json mid-copy does not parse: the title id of an earlier cache of this path.
                let tid = read_param(&path).ok()
                    .and_then(|json| parse_param(&json).ok()).map(|(tid, _, _)| tid)
                    .or_else(|| cached.get(&path).cloned());
                if let Some(t) = &tid {
                    claim(&mut seen, &mut games, &mut result.duplicates, t, rank(Kind::Folder(packing(&path))), nested, None);
                }
                skip_unstable(&mut result, path, tid, age);
                continue;
            }
            (Kind::Folder(packing(&path)), 0, read_param(&path)
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
            drive: crate::storage::drive_of(image.as_deref().unwrap_or(&path)),
            backport: if backports { backport_for(root, roots, &title_id) } else { None },
            path, kind, title_id, title_name, version, size, image,
        };
        let (tid, rank) = (source.title_id.clone(), rank(source.kind));
        claim(&mut seen, &mut games, &mut result.duplicates, &tid, rank, nested, Some(source));
    }
    result.games = games.into_iter().flatten().collect();
    result.games.sort_by(|a, b| a.title_name.cmp(&b.title_name));
    // A fixed error that comes back is logged again.
    if let Some(logged) = LOGGED.get() {
        let errors: HashSet<&PathBuf> = result.errors.iter().map(|(p, _)| p).collect();
        logged.lock().unwrap_or_else(|e| e.into_inner()).retain(|p, _| errors.contains(p));
    }
    // A source copied again later is logged again.
    if let Some(logged) = UNSTABLE.get() {
        let unstable: HashSet<&PathBuf> = result.unstable.iter().map(|(p, _)| p).collect();
        logged.lock().unwrap_or_else(|e| e.into_inner()).retain(|p| unstable.contains(p));
    }
    result
}

fn packing(dir: &Path) -> Packing {
    if dir.join("ampr_zstd.index").is_file() {
        Packing::Zstd
    } else if dir.join("ampr_assets.index").is_file() {
        Packing::Lz4
    } else {
        Packing::Plain
    }
}

/// The source at `path` read as the scan reads it (pkg header memoized), no backport; None when
/// it is not a readable game (a patch or DLC package included). For excluded paths, which the scan skips.
pub fn read_source(path: &Path) -> Option<Source> {
    let m = fs::metadata(path).ok()?;
    let image = m.is_file() && crate::image::is_image(path);
    let (kind, size, (title_id, title_name, version)) =
        if m.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pkg")) {
            let mtime = m.modified().ok()?;
            // A patch or DLC package is not a game.
            if pkg_class(path, m.len(), mtime).is_some_and(|c| c.0 != PkgKind::Base) {
                return None;
            }
            (Kind::Pkg, m.len(), pkg_param(path, m.len(), mtime).ok()?)
        } else if m.is_dir() && path.join("eboot.bin").is_file() {
            // ponytail: param.json is read again on every call (a few KiB; the scan does it per pass too).
            (Kind::Folder(packing(path)), 0, parse_param(&read_param(path).ok()?).ok()?)
        } else if image {
            // Read without attaching it: `path` stays the image file (no mount folder here).
            (Kind::Folder(Packing::Plain), m.len(), parse_param(&crate::image::probe(path).ok()?.param).ok()?)
        } else {
            return None;
        };
    Some(Source {
        path: path.to_path_buf(), kind, title_id, title_name, version, size,
        drive: crate::storage::drive_of(path), backport: None,
        image: image.then(|| path.to_path_buf()),
    })
}

/// A whole file of at most `max` bytes: a corrupt, huge file must not abort the payload (panic=abort).
pub fn read_capped(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut b = Vec::new();
    fs::File::open(path)?.take(max + 1).read_to_end(&mut b)?;
    if b.len() as u64 > max {
        return Err(Error::new(ErrorKind::InvalidData, format!("{}: over {max} bytes", path.display())));
    }
    Ok(b)
}

/// A folder's sce_sys/param.json (at most 1 MiB).
pub fn read_param(dir: &Path) -> io::Result<Vec<u8>> {
    read_capped(&dir.join("sce_sys/param.json"), 1 << 20)
}

pub fn parse_param(json: &[u8]) -> io::Result<(String, String, String)> {
    let v: serde_json::Value = serde_json::from_slice(json)
        .map_err(|e| Error::new(ErrorKind::InvalidData, format!("param.json: {e}")))?;
    let tid = v["titleId"].as_str().or_else(|| v["title_id"].as_str()).unwrap_or_default();
    if !valid_tid(tid) {
        return Err(Error::new(ErrorKind::InvalidData, format!("param.json: bad titleId {tid:?}")));
    }
    let lp = &v["localizedParameters"];
    // ponytail: the console language at parse time; an unbuilt pkg's memoized name keeps the
    // language of its first read until the pkg changes or the payload restarts.
    let name = local_title(lp, crate::i18n::title_tag())
        .or_else(|| lp["defaultLanguage"].as_str().and_then(|lang| lp[lang]["titleName"].as_str()))
        .filter(|name| !name.is_empty())
        .or_else(|| lp["en-US"]["titleName"].as_str().filter(|name| !name.is_empty()))
        .or_else(|| lp.as_object().and_then(|langs| langs.values()
            .filter_map(|lang| lang["titleName"].as_str()).find(|name| !name.is_empty())))
        .unwrap_or(tid);
    Ok((tid.to_owned(), name.to_owned(), v["contentVersion"].as_str().unwrap_or_default().to_owned()))
}
/// The `localizedParameters` title for language tag `want`: that tag (any case), else the same
/// language in another region (es-419 -> es-ES, en-GB -> en-US), as Dump Forge's gameTitle; blank
/// names do not count.
fn local_title<'a>(lp: &'a serde_json::Value, want: &str) -> Option<&'a str> {
    let langs = lp.as_object()?;
    let base = |t: &str| t.split('-').next().unwrap_or("").to_ascii_lowercase();
    let name = |v: &'a serde_json::Value| v["titleName"].as_str().filter(|n| !n.trim().is_empty());
    let titles = || langs.iter().filter(|(k, _)| *k != "defaultLanguage");
    titles().find(|(k, _)| k.eq_ignore_ascii_case(want)).and_then(|(_, v)| name(v))
        .or_else(|| titles().filter(|(k, _)| base(k) == base(want)).find_map(|(_, v)| name(v)))
}

/// Game title ids only: NPXS ids are system apps (SMP is_supported_game_title_id).
pub fn valid_tid(t: &str) -> bool {
    let b = t.as_bytes();
    b.len() == 9 && b[..4].iter().all(u8::is_ascii_uppercase) && b[4..].iter().all(u8::is_ascii_digit)
        && !t.starts_with("NPXS")
}

/// A homebrew title id (SMP 1.7beta6): a valid one that is not a PS4 (CUSA) or PS5 (PPSA) game.
pub fn homebrew(t: &str) -> bool {
    valid_tid(t) && !t.starts_with("CUSA") && !t.starts_with("PPSA")
}
