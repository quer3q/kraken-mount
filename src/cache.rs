//! Per-game cache /data/krakenmount/caches/<TID> (DESIGN.md "Cache"). Folder: fakelib/ only.
//! FPKG: the cache is /app0: sce_sys, eboot.bin and every PRX extracted, fakelib/, kraken/ (launch.rs
//! mounts the pkg's folder there), the two AMPR indexes; `.done` (JSON stamp, mtime = build start) last.
//! fakelib/ (both kinds) = the game's or backport's libs, then /data/krakenmount/ampr/* copied on
//! top; an ampr or inject_ampr change only recomposes fakelib/ (refresh_fakelib), no full rebuild.
//! /app0 is writable: files the game writes here stay across rebuilds (`.built` lists what a build
//! made, a rebuild removes only that); purge() deletes them too.
//! /data/krakenmount/ampr is never copied here: launch.rs mounts it as its own layer.
use crate::ampr_index::{self, Chunk, IndexFile, Kraken};
use crate::config::{FakelibMode, Settings};
use crate::scan::{Kind, Source};
use kraken_mount_pkg::{Entry, Pkg, PRead};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{Error, ErrorKind, Result, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

pub const CACHES: &str = "/data/krakenmount/caches";
pub const AMPR_DIR: &str = "/data/krakenmount/ampr";
pub const DONE: &str = ".done";
/// The game lib dirs, SMP priority first. fakelib2/ is never a game file.
const LIB_DIRS: [&str; 2] = ["fakelib2/", "fakelib/"];
/// What the last build created, one path per line relative to the cache dir (written before `.done`).
const BUILT: &str = ".built";
/// What fakelib/'s ampr copies came from: `ampr_state` when composed (JSON).
const AMPR_STATE: &str = ".ampr";
/// pkg caches: the mount point for the pkg's folder at /app0/kraken (launch.rs).
pub const PKG_MOUNT_DIR: &str = "kraken";
/// pkg caches: one line, no newline, the .pkg relative to /app0 (`kraken/<file name>`); ng opens /app0/ + it.
const IMAGE_FILE: &str = "kraken.image";
const CHUNK: usize = 4 << 20;
/// SMP sm_fakelib.c fakelib_emulator_names.
const EMULATORS: [&str; 5] = [
    "libSceAppContent.sprx",
    "libSceGameUpdate.sprx",
    "libSceNpEntitlementAccess.sprx",
    "libScePlayGo.sprx",
    "libSceAmpr.sprx",
];

/// The .pkg file as a kraken_mount_pkg reader (newtype: the orphan rule forbids `impl PRead for File`).
struct Io(File);

impl PRead for Io {
    fn pread(&self, off: u64, buf: &mut [u8]) -> std::result::Result<usize, kraken_mount_pkg::Error> {
        loop {
            match self.0.read_at(buf, off) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                r => return r.map_err(|e| kraken_mount_pkg::Error::Io(e.raw_os_error().unwrap_or(-1))),
            }
        }
    }
}

fn bad(msg: String) -> Error {
    Error::new(ErrorKind::InvalidData, msg)
}

fn vfs(e: kraken_mount_pkg::Error) -> Error {
    match e {
        kraken_mount_pkg::Error::Io(code) if code > 0 => Error::from_raw_os_error(code),
        e => bad(e.to_string()),
    }
}

/// Files copied out of the pkg: sce_sys/**, eboot.bin, any *.prx / *.sprx.
fn extracted(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("sce_sys/") || lower == "eboot.bin" || lower.ends_with(".prx") || lower.ends_with(".sprx")
}

fn open(pkg: &Path) -> Result<(Io, Pkg)> {
    let file = File::open(pkg)?;
    let len = file.metadata()?.len();
    let io = Io(file);
    let p = Pkg::open(&io, len).map_err(vfs)?;
    Ok((io, p))
}

/// titleId, name and version from the pkg's sce_sys/param.json.
pub fn peek_pkg(pkg: &Path) -> Result<(String, String, String)> {
    let (io, p) = open(pkg)?;
    let param = p.lookup("sce_sys/param.json").ok_or_else(|| bad("no sce_sys/param.json".into()))?;
    crate::scan::parse_param(&read_all(&p, &io, &param)?)
}

/// One file of the pkg, e.g. "sce_sys/icon0.png" (images of a pkg whose cache is not built).
pub fn pkg_file(pkg: &Path, name: &str) -> Result<Vec<u8>> {
    let (io, p) = open(pkg)?;
    let e = p.lookup(name).ok_or_else(|| Error::new(ErrorKind::NotFound, format!("{}: no {name}", pkg.display())))?;
    read_all(&p, &io, &e)
}

fn read_all<R: PRead>(p: &Pkg, io: &R, e: &Entry) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; e.size as usize];
    let n = p.read_at(io, e, 0, &mut buf).map_err(vfs)?;
    buf.truncate(n);
    Ok(buf)
}

/// Copies `e` to `dest` in CHUNK-sized reads.
fn extract<R: PRead>(p: &Pkg, io: &R, e: &Entry, dest: &Path, buf: &mut [u8]) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut out = File::create(dest)?;
    let mut off = 0u64;
    while off < e.size {
        let n = p.read_at(io, e, off, buf).map_err(vfs)?;
        if n == 0 {
            return Err(bad(format!("{}: short read at {off}", e.path)));
        }
        out.write_all(&buf[..n])?;
        off += n as u64;
    }
    drop(out);
    exec_mode(dest)
}

/// 0777, like SMP's fakelib copies: the PS5 loader refuses a module whose file has no
/// execute bits ("mount flag / attribute error", va_mode 0666).
fn exec_mode(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o777))
}

/// `e`'s Kraken blocks as index chunks.
fn chunks(p: &Pkg, e: &Entry) -> Result<Vec<Chunk>> {
    let blocks = p.blocks(e).map_err(vfs)?;
    let mut out = Vec::with_capacity(blocks.len());
    for b in blocks {
        let kraken = b.compressed.then_some(Kraken { even_len: b.desc.even_len, mode: b.desc.mode });
        out.push(Chunk { pkg_off: b.pkg_off, stored_len: b.stored_len, raw_len: b.raw_len, kraken });
    }
    Ok(out)
}

/// Every entry under `dir` (dirs included), recursive, symlinks not followed. Missing dir: empty.
fn walk(dir: &Path) -> Vec<(PathBuf, fs::Metadata)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let Ok(m) = fs::symlink_metadata(e.path()) else { continue };
            if m.is_dir() {
                todo.push(e.path());
            }
            out.push((e.path(), m));
        }
    }
    out
}

/// Removes everything inside `dir`, keeping `dir` (it may be a nullfs source).
fn empty_dir(dir: &Path) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if fs::symlink_metadata(&path)?.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Removes what the last build listed in `.built`, keeping files the game wrote; a dir goes only
/// when it ends up empty. A cache without `.built` (an older build) is emptied.
fn clear_build(dir: &Path) -> Result<()> {
    let Ok(list) = fs::read_to_string(dir.join(BUILT)) else { return empty_dir(dir) };
    // Recomposed after the build too (refresh_fakelib), so .built may not list all of it: always ours.
    match fs::remove_dir_all(dir.join("fakelib")) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut dirs = Vec::new();
    for rel in list.lines().filter(|l| !l.is_empty()) {
        // Our own file, but never step outside the cache.
        if Path::new(rel).components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            continue;
        }
        let path = dir.join(rel);
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => dirs.push(path),
            Ok(_) => fs::remove_file(&path)?,
            Err(_) => {}
        }
    }
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let _ = fs::remove_dir(&d); // fails while it still holds game files: kept
    }
    for name in [BUILT, DONE] {
        match fs::remove_file(dir.join(name)) {
            Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Paths under `dir` that were not there before the build (`before`), deepest dirs last.
fn created(dir: &Path, before: &HashSet<PathBuf>) -> Vec<(PathBuf, bool)> {
    walk(dir).into_iter().filter(|(p, _)| !before.contains(p)).map(|(p, m)| (p, m.is_dir())).collect()
}

/// Removes what a failed build made, keeping the game's files.
fn remove_created(dir: &Path, before: &HashSet<PathBuf>) {
    let mut new = created(dir, before);
    new.sort_by_key(|(p, _)| std::cmp::Reverse(p.components().count()));
    for (p, is_dir) in new {
        let _ = if is_dir { fs::remove_dir(&p) } else { fs::remove_file(&p) };
    }
}

/// The .pkg of a built pkg cache (its `.done` stamp's "source"), else None.
pub fn pkg_source(tid: &str) -> Option<String> {
    let stamp: serde_json::Value = serde_json::from_slice(&fs::read(dir(tid).join(DONE)).ok()?).ok()?;
    if stamp["kind"] != "pkg" {
        return None;
    }
    Some(stamp["source"].as_str()?.to_string())
}

pub fn dir(tid: &str) -> PathBuf {
    Path::new(CACHES).join(tid)
}

pub fn app0(src: &Source) -> PathBuf {
    match src.kind {
        Kind::Pkg => dir(&src.title_id),
        Kind::Folder(_) => src.path.clone(),
    }
}

pub fn sce_sys(src: &Source) -> PathBuf {
    app0(src).join("sce_sys")
}

fn secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn built_at(tid: &str) -> Option<u64> {
    fs::metadata(dir(tid).join(DONE)).and_then(|m| m.modified()).ok().map(secs)
}

pub fn du(path: &Path) -> u64 {
    walk(path).iter().filter(|(_, m)| m.is_file()).map(|(_, m)| m.len()).sum()
}

pub fn size(tid: &str) -> u64 {
    du(&dir(tid))
}

pub fn total_size() -> u64 {
    du(Path::new(CACHES))
}

/// Deletes caches/<TID> not in `keep`; empties the kept ones so they rebuild.
pub fn purge(keep: &[String]) -> Result<()> {
    let Ok(rd) = fs::read_dir(CACHES) else { return Ok(()) };
    for e in rd.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        let r = if !keep.contains(&name) {
            // A pkg cache is app0: drop its /system_ex/app stack first, or it keeps the deleted inode.
            if let Err(err) = crate::register::unmount_title(&name) {
                log!("{name}: purge skipped {}: unmount: {err}", path.display());
                continue;
            }
            fs::remove_dir_all(&path)
        } else {
            empty_dir(&path)
        };
        if let Err(err) = r {
            log!("purge {}: {err}", path.display());
            return Err(err);
        }
    }
    Ok(())
}

fn mode_name(m: FakelibMode) -> &'static str {
    match m {
        FakelibMode::Emulators => "emulators",
        FakelibMode::Full => "full",
        FakelibMode::Off => "off",
    }
}

/// The backport's lib dir in SMP order (sm_fakelib.c resolve_game_fakelib_source_for_path):
/// fakelib2 (exclusive), then fakelib.
fn bp_lib(src: &Source) -> Option<PathBuf> {
    let b = src.backport.as_ref()?;
    ["fakelib2", "fakelib"].iter().map(|d| b.join(d)).find(|p| p.is_dir())
}

/// The fakelib dir on disk the cache's fakelib/ is built from: backport fakelib2/fakelib, then
/// a folder game's fakelib2/fakelib. None for a pkg without one (its libs are inside the pkg).
fn lib_dir(src: &Source) -> Option<PathBuf> {
    bp_lib(src).or_else(|| match src.kind {
        Kind::Pkg => None,
        Kind::Folder(_) => ["fakelib2", "fakelib"].iter().map(|d| src.path.join(d)).find(|p| p.is_dir()),
    })
}

pub fn stamp(src: &Source, s: &Settings) -> serde_json::Value {
    serde_json::json!({
        "v": 1,
        "kind": if src.kind == Kind::Pkg { "pkg" } else { "folder" },
        "source": src.path.to_string_lossy(),
        "size": src.size,
        "game_fakelib": mode_name(s.game_fakelib),
        "backport": src.backport.as_ref().map(|b| b.to_string_lossy()),
        // Which lib dir was used: a removed or newly added fakelib/fakelib2 changes it.
        "fakelib": lib_dir(src).map(|p| p.to_string_lossy().into_owned()),
    })
}

/// Existing paths whose mtime must be older than `.done`. The ampr dir is not one: launch mounts it.
pub fn inputs(src: &Source, _s: &Settings) -> Vec<PathBuf> {
    let mut v = Vec::new();
    match src.kind {
        Kind::Pkg => {
            v.push(src.path.clone());
            // The whole backport is copied into a pkg cache.
            if let Some(b) = &src.backport {
                v.push(b.clone());
                v.extend(walk(b).into_iter().map(|e| e.0));
            }
        }
        // ponytail: only the lib dir of a folder's backport feeds its cache; the rest is a live unionfs.
        // Not the game folder itself: the game writes there (/app0 is writable), which must not rebuild.
        Kind::Folder(_) => {
            v.extend([src.path.join("eboot.bin"), src.path.join("sce_sys/param.json")]);
            v.retain(|p| p.exists());
            if let Some(lib) = lib_dir(src) {
                v.extend(walk(&lib).into_iter().map(|e| e.0));
                v.push(lib);
            }
        }
    }
    v
}

pub enum Fresh {
    Fresh,
    Missing,
    Stale(String),
    /// Only fakelib/ is out of date (the ampr files or inject_ampr changed): refresh_fakelib.
    Fakelib(String),
}

/// The ampr files fakelib/ should carry: {name: [size, mtime]} of /data/krakenmount/ampr's
/// regular files (libkernel.sprx never copied), or null with inject_ampr off.
fn ampr_state(s: &Settings) -> serde_json::Value {
    if !s.inject_ampr {
        return serde_json::Value::Null;
    }
    let mut m = serde_json::Map::new();
    for e in fs::read_dir(AMPR_DIR).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(md) = fs::metadata(e.path()) else { continue };
        if md.is_file() && ampr_copied(&name) {
            m.insert(name, serde_json::json!([md.len(), md.modified().map(secs).unwrap_or(0)]));
        }
    }
    serde_json::Value::Object(m)
}

/// libkernel.sprx from the ampr dir would replace the system one for every game: never copied.
fn ampr_copied(name: &str) -> bool {
    !name.eq_ignore_ascii_case("libkernel.sprx")
}

pub fn check(src: &Source, s: &Settings) -> Fresh {
    let done = dir(&src.title_id).join(DONE);
    let Ok(done_mtime) = fs::metadata(&done).and_then(|m| m.modified()) else { return Fresh::Missing };
    let old: serde_json::Value =
        fs::read(&done).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let new = stamp(src, s);
    for (k, v) in new.as_object().into_iter().flatten() {
        if old.get(k) != Some(v) {
            return Fresh::Stale(format!("{k} changed"));
        }
    }
    // A cache built before kraken.image existed (or with it deleted): ng could not find the pkg.
    if src.kind == Kind::Pkg && !dir(&src.title_id).join(IMAGE_FILE).is_file() {
        return Fresh::Stale(format!("{IMAGE_FILE} missing"));
    }
    // ponytail: an input with a future mtime (wrong clock on the PC that wrote the USB) rebuilds every scan.
    for p in inputs(src, s) {
        // inputs() lists existing paths only, and the stamp records the lib dir used, so a missing one is a removal.
        let Ok(m) = fs::symlink_metadata(&p).and_then(|m| m.modified()) else {
            return Fresh::Stale(format!("{} missing", p.display()));
        };
        if m >= done_mtime {
            return Fresh::Stale(format!("{} changed", p.display()));
        }
    }
    let had: serde_json::Value = fs::read(dir(&src.title_id).join(AMPR_STATE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if had != ampr_state(s) {
        return Fresh::Fakelib(format!("{AMPR_DIR} or inject_ampr changed"));
    }
    Fresh::Fresh
}

/// Whether fakelib file `name` goes into the cache's fakelib/ (`all`: a backport lib dir or a game's
/// fakelib2, which SMP uses as-is).
fn keep_fakelib(name: &str, s: &Settings, all: bool) -> bool {
    !name.eq_ignore_ascii_case("libkernel.sprx")
        && (all
            || match s.game_fakelib {
                FakelibMode::Full => true,
                FakelibMode::Off => false,
                FakelibMode::Emulators => EMULATORS.iter().any(|k| name.eq_ignore_ascii_case(k)),
            })
}

/// Copies the regular files directly in `from` that pass `keep` into `to`, 0777.
fn copy_flat(from: &Path, to: &Path, keep: impl Fn(&str) -> bool) -> Result<()> {
    let Ok(rd) = fs::read_dir(from) else { return Ok(()) };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !fs::metadata(e.path()).is_ok_and(|m| m.is_file()) || !keep(&name) {
            continue;
        }
        let dest = to.join(&name);
        fs::copy(e.path(), &dest)?;
        exec_mode(&dest)?;
    }
    Ok(())
}

fn loose(path: String, size: u64) -> IndexFile {
    IndexFile { path, size, chunks: None }
}

/// fakelib/ from scratch: the backport's lib dir (all files) if any, else the game's own (pkg:
/// extracted from `pkg`; folder: copied), filtered by game_fakelib; then the ampr files on top
/// (inject_ampr). All 0777. Writes AMPR_STATE.
fn compose_fakelib(src: &Source, s: &Settings, pkg: Option<(&Io, &Pkg)>) -> Result<()> {
    let dir = dir(&src.title_id);
    let fakelib = dir.join("fakelib");
    match fs::remove_dir_all(&fakelib) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    fs::create_dir_all(&fakelib)?;
    let all_game_libs = |two: bool| two && s.game_fakelib != FakelibMode::Off;
    if let Some(bp) = bp_lib(src) {
        copy_flat(&bp, &fakelib, |n| keep_fakelib(n, s, true))?;
    } else if let Some((io, p)) = pkg {
        // SMP order inside the game: fakelib2/ (used as-is) wins over fakelib/.
        let game_lib = if p.files().any(|e| e.path.to_ascii_lowercase().starts_with(LIB_DIRS[0])) {
            LIB_DIRS[0]
        } else {
            LIB_DIRS[1]
        };
        let two = game_lib == LIB_DIRS[0];
        let mut buf = vec![0u8; CHUNK];
        for e in p.files().filter(|e| !e.is_dir) {
            let lower = e.path.to_ascii_lowercase();
            let Some(name) = lower.strip_prefix(game_lib) else { continue };
            let name = &e.path[e.path.len() - name.len()..];
            if !name.contains('/') && keep_fakelib(name, s, all_game_libs(two)) {
                extract(p, io, &e, &fakelib.join(name), &mut buf)?;
            }
        }
    } else if let Some(lib) = lib_dir(src) {
        let two = lib.file_name().is_some_and(|n| n == "fakelib2");
        copy_flat(&lib, &fakelib, |n| keep_fakelib(n, s, all_game_libs(two)))?;
    }
    // Our ampr-emu-ng on top of whatever the game or backport ships (its own libSceAmpr.sprx included).
    if s.inject_ampr {
        copy_flat(Path::new(AMPR_DIR), &fakelib, ampr_copied)?;
    }
    fs::write(dir.join(AMPR_STATE), ampr_state(s).to_string())
}

/// Recomposes only fakelib/ (Fresh::Fakelib): no extraction besides a pkg's own fakelib, no
/// index, no re-registration. ponytail: a launch reads fakelib/ while this runs only if a game
/// starts mid-refresh (the scan loop is paused while one runs).
pub fn refresh_fakelib(src: &Source, s: &Settings) -> Result<()> {
    log!("{}: refreshing {}", src.title_id, dir(&src.title_id).join("fakelib").display());
    let pkg = if src.kind == Kind::Pkg { Some(open(&src.path)?) } else { None };
    compose_fakelib(src, s, pkg.as_ref().map(|(io, p)| (io, p))).map_err(|e| {
        let e = Error::new(e.kind(), format!("{} ({}): fakelib refresh: {e}", src.title_id, src.path.display()));
        log!("{e}");
        e
    })
}

/// Always rebuilds `src`'s cache. Errors carry the title id and source path and are logged.
pub fn build(src: &Source, s: &Settings) -> Result<()> {
    let d = dir(&src.title_id);
    log!("{}: building cache {} from {}", src.title_id, d.display(), src.path.display());
    let mut before = HashSet::new();
    let r = (|| {
        // Keep the dir: /system_ex/app/<TID> is a nullfs of this exact directory, and deleting it
        // would leave that mount on a dead inode (launch fails with 0x80940033).
        fs::create_dir_all(&d)?;
        clear_build(&d)?;
        // What is left is the game's own files: never part of the build, never removed by it.
        before = walk(&d).into_iter().map(|e| e.0).collect();
        build_inner(src, s, &before)
    })();
    r.map_err(|e| {
        let e = Error::new(e.kind(), format!("{} ({}): cache build: {e}", src.title_id, src.path.display()));
        log!("{e}");
        // Without `.done` the cache is unusable: free what this build wrote.
        remove_created(&d, &before);
        e
    })
}

/// Writes `.built` (what this build created) and then `.done`.
fn finish(dir: &Path, src: &Source, s: &Settings, start: SystemTime, before: &HashSet<PathBuf>) -> Result<()> {
    let mut list = String::new();
    for (p, _) in created(dir, before) {
        if let Ok(rel) = p.strip_prefix(dir) {
            list += &rel.to_string_lossy();
            list.push('\n');
        }
    }
    fs::write(dir.join(BUILT), list)?;
    write_done(dir, src, s, start)
}

/// Writes `.done` with mtime `start`, so an input changed during the build is newer than it.
fn write_done(dir: &Path, src: &Source, s: &Settings, start: SystemTime) -> Result<()> {
    let done = dir.join(DONE);
    fs::write(&done, stamp(src, s).to_string())?;
    File::options().write(true).open(&done)?.set_modified(start)
}

fn build_inner(src: &Source, s: &Settings, before: &HashSet<PathBuf>) -> Result<()> {
    let start = SystemTime::now();
    let is_pkg = src.kind == Kind::Pkg;
    let dir = dir(&src.title_id);
    let mut files: BTreeMap<String, IndexFile> = BTreeMap::new();
    let pkg = if is_pkg { Some(open(&src.path)?) } else { None };
    if let Some((io, p)) = &pkg {
        let mut buf = vec![0u8; CHUNK];
        for e in p.files().filter(|e| !e.is_dir) {
            let lower = e.path.to_ascii_lowercase();
            // dump-forge always puts a fresh ampr_emu.index (loose files only) into a pkg: ours replaces it.
            if lower == "ampr_emu.index" {
                continue;
            }
            // ponytail: a pkg made from an ampr-packed folder is refused; serving packs from inside a pkg is not supported.
            if lower == "ampr_assets.index" || lower == "ampr_zstd.index" {
                return Err(bad(format!(
                    "the pkg contains {} (ampr packs); make the pkg from the unpacked game folder",
                    e.path
                )));
            }
            // The lib dirs are compose_fakelib's; never game files.
            if LIB_DIRS.iter().any(|d| lower.starts_with(d)) {
                continue;
            }
            let chunks = if extracted(e.path) || p.has_sparse(&e) {
                extract(p, io, &e, &dir.join(e.path), &mut buf)?;
                None
            } else {
                Some(chunks(p, &e)?)
            };
            files.insert(lower, IndexFile { path: e.path.to_string(), size: e.size, chunks });
        }
    }
    compose_fakelib(src, s, pkg.as_ref().map(|(io, p)| (io, p)))?;
    if !is_pkg {
        return finish(&dir, src, s, start, before);
    }
    // Backport files (except its fakelib/fakelib2) go loose over the pkg's.
    if let Some(bp) = &src.backport {
        for (path, m) in walk(bp).into_iter().filter(|(_, m)| m.is_file()) {
            let rel = path.strip_prefix(bp).unwrap_or(&path);
            let rel = rel.to_str().ok_or_else(|| bad(format!("{}: path is not UTF-8", path.display())))?;
            let lower = rel.to_ascii_lowercase();
            if LIB_DIRS.iter().any(|d| lower.starts_with(d)) {
                continue;
            }
            let dest = dir.join(rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&path, &dest)?;
            exec_mode(&dest)?;
            files.insert(lower, loose(rel.to_string(), m.len()));
        }
    }
    // fakelib/ is not indexed: it reaches the game as the common/lib overlay, and with a writable
    // /app0 an index miss goes to the real file (ng), so a refresh needs no index rewrite.
    // launch.rs mounts the pkg's folder on /app0/kraken; kraken.image names the pkg in it for ng.
    fs::create_dir_all(dir.join(PKG_MOUNT_DIR))?;
    let name = src.path.file_name().and_then(|n| n.to_str()).ok_or_else(|| bad(format!("{}: file name is not UTF-8", src.path.display())))?;
    fs::write(dir.join(IMAGE_FILE), format!("{PKG_MOUNT_DIR}/{name}"))?;
    let files: Vec<IndexFile> = files.into_values().collect();
    ampr_index::write(&dir, &src.path, &files)?;
    finish(&dir, src, s, start, before)
}
