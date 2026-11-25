//! Per-game cache /data/krakenmount/caches/<TID> (DESIGN.md "Cache"). Folder: fakelib/ only.
//! Installed (the console's own install): fakelib/ only, from the backport's lib dir, the emulators
//! and the ampr files (ampr_installed); never registered or mounted by us.
//! FPKG: the cache is /app0: sce_sys, eboot.bin and every PRX and ELF extracted, fakelib/, kraken/ (launch.rs
//! mounts the pkg's folder there), the two AMPR indexes; `.done` (JSON stamp, mtime = build start) last.
//! fakelib/ (both kinds) = the game's or backport's libs, then /data/krakenmount/ampr/* copied on
//! top (inject_ampr; always for a pkg); in between, the bundled DLC (dlc_emu) and PlayGo (pgo_stub)
//! emulators when the game needs one and ships none (add_emulators; no pgo_stub for playgo_off). For a title in
//! overlay_off fakelib/ stays empty (no overlay at launch). A change of the ampr files,
//! inject_ampr, overlay_off or the emulator inputs only recomposes fakelib/ (refresh_fakelib), no full rebuild.
//! /app0 is writable: files the game writes here stay across rebuilds (`.built` lists what a build
//! made, a rebuild removes only that); purge() deletes them too.
//! /data/krakenmount/ampr is never copied here: launch.rs mounts it as its own layer.
use crate::ampr_index::{self, Chunk, IndexFile, Kraken};
use crate::config::Settings;
use crate::scan::{Kind, Packing, Source};
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
/// What fakelib/ was composed from (JSON): {"ampr": ampr_state, "emus": emu_state}.
const AMPR_STATE: &str = ".ampr";
/// Bundled third-party emulators (GPL-3.0, THIRD_PARTY_NOTICES), released files (ps5/build.sh checks
/// SHA256SUMS): drakmor/dlc_emu (DLC; its 3 files go together) and drakmor/pgo_stub (PlayGo).
const DLC_EMU_VERSION: &str = include_str!("../vendor/dlc_emu/VERSION");
const PGO_STUB_VERSION: &str = include_str!("../vendor/pgo_stub/VERSION");
/// (file name, contents, project, is the DLC set).
const EMUS: [(&str, &[u8], &str, bool); 4] = [
    ("libSceAppContent.sprx", include_bytes!("../vendor/dlc_emu/libSceAppContent.sprx"), "dlc_emu", true),
    ("libSceNpEntitlementAccess.sprx", include_bytes!("../vendor/dlc_emu/libSceNpEntitlementAccess.sprx"), "dlc_emu", true),
    ("libSceGameUpdate.sprx", include_bytes!("../vendor/dlc_emu/libSceGameUpdate.sprx"), "dlc_emu", true),
    ("libScePlayGo.sprx", include_bytes!("../vendor/pgo_stub/libScePlayGo.sprx"), "pgo_stub", false),
];
/// dlc_emu's config at the game root: its presence asks for the DLC set.
const DLC_INI: &str = "dlc_emu.ini";
/// An import of libScePlayGo in a decrypted eboot.bin: its module name as a plain string.
const PLAYGO_NEEDLE: &[u8] = b"libScePlayGo";
/// pkg caches: the mount point for the pkg's folder at /app0/kraken (launch.rs).
pub const PKG_MOUNT_DIR: &str = "kraken";
/// pkg caches: one line, no newline, the .pkg relative to /app0 (`kraken/<file name>`); ng opens /app0/ + it.
const IMAGE_FILE: &str = "kraken.image";
const CHUNK: usize = 4 << 20;

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

/// Files copied out of the pkg: sce_sys/**, eboot.bin, any *.prx / *.sprx, any *.elf outside
/// decrypted/ (an app can exit-spawn an ELF from a subfolder, which SceSysCore loads from disk;
/// dump-forge's decrypted/ copy is never run), and the root-level
/// dlc_emu.ini / playgo_stub.dat (the emulators fopen /app0/<name>).
/// ponytail: no STAMP_V bump for the last two: an older cache lacks them loose, ng still serves them
/// from the pkg through the index.
fn extracted(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("sce_sys/")
        || lower == "eboot.bin"
        || lower.ends_with(".prx")
        || lower.ends_with(".sprx")
        || (lower.ends_with(".elf") && !lower.starts_with("decrypted/"))
        || lower == DLC_INI
        || lower == "playgo_stub.dat"
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
    crate::scan::parse_param(&read_all(&p, &io, &param, 1 << 20)?)
}

/// One file of the pkg, e.g. "sce_sys/icon0.png" (images of a pkg whose cache is not built).
pub fn pkg_file(pkg: &Path, name: &str) -> Result<Vec<u8>> {
    let (io, p) = open(pkg)?;
    let e = p.lookup(name).ok_or_else(|| Error::new(ErrorKind::NotFound, format!("{}: no {name}", pkg.display())))?;
    read_all(&p, &io, &e, 32 << 20)
}

/// A corrupt entry size must not abort the payload (panic=abort) on every scan of that pkg.
fn read_all<R: PRead>(p: &Pkg, io: &R, e: &Entry, max: u64) -> Result<Vec<u8>> {
    if e.size > max {
        return Err(bad(format!("{}: {} bytes, over the {max} byte limit", e.path, e.size)));
    }
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

/// `e`'s Kraken blocks as index chunks (a sparse hole stays a zero chunk: stored_len 0).
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
    // `.done` first: a clear cut off midway must not leave a half-cleared cache that check() calls fresh.
    match fs::remove_file(dir.join(DONE)) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    // Only a missing `.built` means an older build; a read error must not empty the game's files.
    let list = match fs::read(dir.join(BUILT)) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            empty_dir(dir)?;
            return fs::write(dir.join(BUILT), "");
        }
        Err(e) => return Err(e),
    };
    // Recomposed after the build too (refresh_fakelib), so .built may not list all of it: always ours.
    match fs::remove_dir_all(dir.join("fakelib")) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut dirs = Vec::new();
    // AMPR_STATE stays: compose_fakelib reads its playgo memo and the unmounted emus fallback, and
    // rewrites it.
    for rel in list.lines().filter(|l| !l.is_empty() && *l != AMPR_STATE) {
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
    // Emptied, not removed: a build that fails or is cut off must not leave a cache without `.built`,
    // which the next build would empty (game-written files included).
    // ponytail: files a cut-off build wrote stay unlisted (kept like game files) until Purge.
    fs::write(dir.join(BUILT), "")
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

/// Where the game's /app0 files are. Installed: the system's own /system_ex/app/<TID> (read-only
/// here, used for the eboot.bin and dlc_emu.ini lookups; it may not be mounted outside a launch).
pub fn app0(src: &Source) -> PathBuf {
    match src.kind {
        Kind::Pkg => dir(&src.title_id),
        Kind::Folder(_) => src.path.clone(),
        Kind::Installed => Path::new(SYSTEM_APP).join(&src.title_id),
    }
}

/// Where the system mounts a title's /app0.
const SYSTEM_APP: &str = "/system_ex/app";

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
            // Never for an installed title: that stack is the system's (an installed title can miss
            // from `keep`, e.g. one installed::list skips). ponytail: a layer of ours left over it
            // is dropped by main's drop_old_mount once the title is listed again.
            // An install run through ng has a pkg stamp whose source is its /user/app app.pkg (installed
            // even while that file is briefly gone during a console update).
            let installed = crate::installed::copy_in(Path::new("/"), &name).is_some()
                || stamp_kind(&name).as_deref() == Some("installed")
                || stamp_source(&name).is_some_and(|p| Path::new(&p).starts_with("/user/app"));
            if installed {
                // An install run through ng has our cache nullfs on top: that layer only, else the cache stays.
                if let Err(err) = crate::register::drop_cache_layer(&name) {
                    log!("{name}: purge skipped {}: unmount our layer: {err}", path.display());
                    continue;
                }
                log!("{name}: purge: installed, the system's /system_ex/app/{name} left mounted; deleting {}", path.display());
            } else if let Err(err) = crate::register::unmount_title(&name) {
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

/// `mode` with the `want` bits added, or None when none is missing (fix_access only adds bits).
fn add_bits(mode: u32, want: u32) -> Option<u32> {
    let mode = mode & 0o7777;
    (mode & want != want).then_some(mode | want)
}

/// fix_access's tally: modes fixed, chmods failed and the first failure.
#[derive(Default)]
struct Access {
    fixed: u64,
    failed: u64,
    first: Option<(PathBuf, Error)>,
}

impl Access {
    /// Adds `dirs` to a dir, `files` to a regular file; anything else (symlinks: never followed) is skipped.
    fn entry(&mut self, path: &Path, m: &fs::Metadata, dirs: u32, files: u32) {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let want = if m.is_dir() { dirs } else if m.is_file() { files } else { return };
        let Some(mode) = add_bits(m.mode(), want) else { return };
        match fs::set_permissions(path, fs::Permissions::from_mode(mode)) {
            Ok(()) => self.fixed += 1,
            Err(e) => {
                self.failed += 1;
                self.first.get_or_insert((path.to_path_buf(), e));
            }
        }
    }

    /// `path` itself (lstat), then everything under it when it is a dir.
    fn tree(&mut self, path: &Path, dirs: u32, files: u32) {
        let Ok(m) = fs::symlink_metadata(path) else { return };
        self.entry(path, &m, dirs, files);
        if m.is_dir() {
            for (p, m) in walk(path) {
                self.entry(&p, &m, dirs, files);
            }
        }
    }
}

/// Opens up `src`'s files for the game and the system services (save data, trophies), which read
/// them with another uid than the payload. Only adds bits, only chmods when some are missing,
/// never follows symlinks: the cache dir (dirs 0777: /app0 is writable; files +0444); a pkg's
/// .pkg (+0444) and its folder (+0555, the /app0/kraken nullfs; ng falls back to LZ4 mode when it
/// cannot open the .pkg); a folder game's tree (dirs +0555, files +0444). Never fails: one log
/// line per title when something was fixed or failed (exFAT USB, read-only), with the first failure.
/// ponytail: one lstat walk of the cache and the game folder per run (main.rs calls it once per
/// title); a 150k-file folder costs that walk, and files added later wait for the next run.
pub fn fix_access(src: &Source) {
    let mut a = Access::default();
    a.tree(&dir(&src.title_id), 0o777, 0o444);
    match src.kind {
        Kind::Pkg => {
            if let Ok(m) = fs::symlink_metadata(&src.path) {
                a.entry(&src.path, &m, 0, 0o444);
            }
            if let Some(parent) = src.path.parent() {
                if let Ok(m) = fs::symlink_metadata(parent) {
                    a.entry(parent, &m, 0o555, 0);
                }
            }
        }
        // A read-only image mount: chmod would only fail (EROFS) on every file.
        Kind::Folder(_) if src.image.is_some() => {}
        Kind::Folder(_) => a.tree(&src.path, 0o555, 0o444),
        // The system's install: never chmod'ed from here.
        Kind::Installed => {}
    }
    if a.fixed + a.failed == 0 {
        return;
    }
    let first = a.first.map_or(String::new(), |(p, e)| format!("; first failure {}: {e}", p.display()));
    log!("{}: access ({}): {} modes fixed, {} failed{first}", src.title_id, src.path.display(), a.fixed, a.failed);
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
        // Installed: only a backport's lib dir (the install's own libs are already in /app0).
        Kind::Pkg | Kind::Installed => None,
        Kind::Folder(_) => ["fakelib2", "fakelib"].iter().map(|d| src.path.join(d)).find(|p| p.is_dir()),
    })
}

/// The cache layout version in the stamp. 2: the pkg reader also extracts container-only sce_sys
/// files (save_data.png, pic0/pic1, snd0.at9, trophy00.ucp, nptitle.dat, npbind.dat), and files the
/// index cannot map are extracted; every v1 cache rebuilds once. 3: *.elf outside decrypted/ extracted too.
const STAMP_V: u64 = 3;

pub fn stamp(src: &Source, _s: &Settings) -> serde_json::Value {
    serde_json::json!({
        "v": STAMP_V,
        "kind": kind_name(src.kind),
        "source": src.path.to_string_lossy(),
        "size": src.size,
        "backport": src.backport.as_ref().map(|b| b.to_string_lossy()),
        // Which lib dir was used: a removed or newly added fakelib/fakelib2 changes it.
        "fakelib": lib_dir(src).map(|p| p.to_string_lossy().into_owned()),
    })
}

/// The stamp's "kind".
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Pkg => "pkg",
        Kind::Folder(_) => "folder",
        Kind::Installed => "installed",
    }
}

/// The `.done` stamp's "source" of title `tid` (the pkg, folder or install dir); None without a cache.
pub fn stamp_source(tid: &str) -> Option<String> {
    let stamp: serde_json::Value = serde_json::from_slice(&fs::read(dir(tid).join(DONE)).ok()?).ok()?;
    Some(stamp["source"].as_str()?.to_string())
}

/// The `.done` stamp's "kind" of title `tid` ("pkg", "folder", "installed"); None without a cache.
pub fn stamp_kind(tid: &str) -> Option<String> {
    let stamp: serde_json::Value = serde_json::from_slice(&fs::read(dir(tid).join(DONE)).ok()?).ok()?;
    Some(stamp["kind"].as_str()?.to_string())
}

/// The title's fakelib was composed with drops_game_ng (an installed game, ampr_installed off): launch
/// then also hides the install's own libSceAmpr.sprx in /app0 (launch/nsfs.rs).
pub fn ng_dropped(tid: &str) -> bool {
    fs::read(dir(tid).join(AMPR_STATE)).ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|v| v["ampr"] == "drop")
}

/// The `.done` stamp's "backport" of title `tid` (the backport dir the cache was built with); None
/// without a cache or a backport (the `backports` setting off leaves it out).
pub fn stamp_backport(tid: &str) -> Option<String> {
    let stamp: serde_json::Value = serde_json::from_slice(&fs::read(dir(tid).join(DONE)).ok()?).ok()?;
    Some(stamp["backport"].as_str()?.to_string())
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
        // The install itself (app.pkg; an ext copy's dir when it has none) and the backport's lib dir.
        // ponytail: not /system_ex/app/<TID> (mounted by the system, maybe not now); a patch
        // installed over it changes the eboot only, seen through emu_state.
        Kind::Installed => {
            let pkg = src.path.join("app.pkg");
            v.push(if pkg.exists() { pkg } else { src.path.clone() });
            if let Some(lib) = bp_lib(src) {
                v.extend(walk(&lib).into_iter().map(|e| e.0));
                v.push(lib);
            }
        }
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
    /// Only fakelib/ is out of date (the ampr files, inject_ampr or emu_state changed, a legacy game_fakelib
    /// stamp): refresh_fakelib.
    Fakelib(String),
    /// The same game moved from this old source path (the old path is gone): relink, no rebuild.
    Moved(PathBuf),
}

/// `.done` key (pkg only, not part of `stamp`): `pkg_id` of the .pkg the cache was built from.
const PKG_ID: &str = "pkg_id";
const ID_SPAN: usize = 0x10000;

/// A .pkg's identity: FNV-1a 64 of its size, first 64 KiB (the FIH header), the 4 KiB CNT header
/// (content id, digests) and last 64 KiB. ponytail: a sample, not a full hash (128 KiB read, not the
/// whole pkg); together with the old path being gone it tells a moved pkg from another build.
pub fn pkg_id(pkg: &Path) -> Result<String> {
    let f = File::open(pkg)?;
    let len = f.metadata()?.len();
    let mut v = len.to_le_bytes().to_vec();
    let mut feed = |b: &[u8]| v.extend_from_slice(b);
    let mut buf = vec![0u8; ID_SPAN];
    let head = ID_SPAN.min(len as usize);
    f.read_exact_at(&mut buf[..head], 0)?;
    feed(&buf[..head]);
    let cnt = buf.get(0x58..0x60).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
    if cnt.checked_add(0x1000).is_some_and(|e| e <= len) {
        f.read_exact_at(&mut buf[..0x1000], cnt)?;
        feed(&buf[..0x1000]);
    }
    let tail = ID_SPAN.min(len as usize);
    f.read_exact_at(&mut buf[..tail], len - tail as u64)?;
    feed(&buf[..tail]);
    Ok(crate::register::fnv1a(&v))
}

/// `.done` key (folder only, not part of `stamp`): `folder_id` of the folder the cache was built from.
const FOLDER_ID: &str = "folder_id";

/// A folder game's identity: FNV-1a 64 of each input file's path (relative to the folder) and
/// size, plus the bytes of every input but eboot.bin (param.json and the lib dir's files, which
/// the cache's fakelib/ is a copy of). ponytail: eboot.bin by size only (it can be large) and no
/// mtimes (a copy to exFAT rounds them); another dump differs in eboot.bin's size, a lib or param.json.
fn folder_id(src: &Source, s: &Settings) -> String {
    let mut v = Vec::new();
    let mut feed = |b: &[u8]| v.extend_from_slice(b);
    let eboot = src.path.join("eboot.bin");
    // ponytail: an installed app.pkg (many GB) feeds its size and mtime, not its bytes; an install
    // over it rewrites the file, so the mtime changes.
    let app_pkg = (src.kind == Kind::Installed).then(|| src.path.join("app.pkg"));
    let mut files = inputs(src, s);
    files.sort();
    for p in files {
        let Ok(m) = fs::symlink_metadata(&p) else { continue };
        if m.is_file() {
            feed(p.strip_prefix(&src.path).unwrap_or(&p).to_string_lossy().as_bytes());
            feed(&m.len().to_le_bytes());
            if app_pkg.as_ref() == Some(&p) {
                feed(&std::os::unix::fs::MetadataExt::mtime(&m).to_le_bytes());
            } else if p != eboot {
                feed(&fs::read(&p).unwrap_or_default());
            }
        }
    }
    crate::register::fnv1a(&v)
}

/// Whether stamp `new` is `old` with only the source's location changed: every key equal but
/// "source" (and "fakelib" when it is the game's own lib dir under the moved source).
fn only_moved(old: &serde_json::Value, new: &serde_json::Value) -> bool {
    let (Some(from), Some(to)) = (old["source"].as_str(), new["source"].as_str()) else { return false };
    if from == to {
        return false;
    }
    new.as_object().into_iter().flatten().all(|(k, v)| match k.as_str() {
        "source" => true,
        "fakelib" if old.get(k) != Some(v) => {
            let rebased = old[k].as_str().and_then(|p| p.strip_prefix(from)).map(|rest| format!("{to}{rest}"));
            rebased.is_some() && rebased.as_deref() == v.as_str()
        }
        _ => old.get(k) == Some(v),
    })
}

/// The move decision: a relink only when the stamps differ by location alone, the old source is
/// gone (a copy while the old one still exists is a duplicate), and the files are the same (pkg:
/// `pkg_id` equal; folder: every input older than `.done`, as after a plain move).
fn is_move(old: &serde_json::Value, new: &serde_json::Value, old_exists: bool, same_files: impl FnOnce() -> bool) -> bool {
    only_moved(old, new) && !old_exists && same_files()
}

/// `is_move`'s file check. ponytail: a cache built before `pkg_id` was stamped is rebuilt once
/// when its pkg moves.
fn same_files(src: &Source, s: &Settings, old: &serde_json::Value, done_mtime: SystemTime) -> bool {
    if src.kind != Kind::Pkg {
        // folder_id covers the bytes of everything a folder cache is made from (param.json, the
        // lib dir), so a copy with new mtimes (copy, then delete the old one) still relinks.
        return old[FOLDER_ID].as_str() == Some(folder_id(src, s).as_str());
    }
    // The moved .pkg itself may be newer than `.done` (a copy to another drive): its id decides;
    // the other inputs (the backport, copied into the cache) must be older than `.done`.
    let older = inputs(src, s).iter().filter(|p| **p != src.path).all(|p| {
        fs::symlink_metadata(p).and_then(|m| m.modified()).is_ok_and(|m| m < done_mtime)
    });
    older && old[PKG_ID].as_str().is_some_and(|id| pkg_id(&src.path).ok().as_deref() == Some(id))
}

/// The ampr files fakelib/ should carry: {name: [size, mtime]} of /data/krakenmount/ampr's
/// regular files (libkernel.sprx never copied), or null when `injects_ampr` is false.
fn ampr_state(src: &Source, s: &Settings) -> serde_json::Value {
    if drops_game_ng(src, s) {
        // Not null: a cache composed before the drop existed refreshes once.
        return "drop".into();
    }
    if !injects_ampr(src, s) {
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

/// Whether fakelib/ gets the ampr dir's files: always for a pkg (#73: pkg mode exists only in
/// ampr-emu-ng, the package's own libSceAmpr.sprx cannot read the .pkg named in kraken.image);
/// a zstd or LZ4 folder with inject_ampr; a plain folder with inject_ampr unless the title is in ampr_off
/// (Settings::ng_folder); an installed game with ampr_installed (per title, Settings::ng_installed).
fn injects_ampr(src: &Source, s: &Settings) -> bool {
    match src.kind {
        Kind::Pkg => true,
        Kind::Folder(Packing::Plain) => s.ng_folder(&src.title_id),
        Kind::Folder(_) => s.inject_ampr,
        Kind::Installed => s.ng_installed(&src.title_id),
    }
}

/// An installed game without ampr_installed: no libSceAmpr.sprx in its fakelib at all, the backport's
/// included (SMP removes it from an installed PPR package's backport fakelib), so the system's runs.
fn drops_game_ng(src: &Source, s: &Settings) -> bool {
    src.kind == Kind::Installed && !s.ng_installed(&src.title_id)
}

/// libkernel.sprx from the ampr dir would replace the system one for every game: never copied.
fn ampr_copied(name: &str) -> bool {
    !name.eq_ignore_ascii_case("libkernel.sprx")
}

/// The entry of `dir` named `name`, compared case-insensitively. Missing dir: None.
fn find_ci(dir: &Path, name: &str) -> Option<PathBuf> {
    fs::read_dir(dir).into_iter().flatten().flatten()
        .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(name))
        .map(|e| e.path())
}

/// Known older releases of the bundled emulators, (file name, len, crc32), each in its two builds:
/// dlc_emu 0.1a and 0.2, pgo_stub 0.4 and 0.5. A fakelib copy matching one is upgraded.
/// ponytail: the binaries carry no version string, so only these known releases are upgraded; any
/// other copy (custom, the bundled release, a log build of it) is left alone: no downgrade.
const OLD_EMUS: [(&str, u64, u32); 14] = [
    ("libSceAppContent.sprx", 39366, 0xcb58_8874),
    ("libSceAppContent.sprx", 31174, 0x641c_afda),
    ("libSceNpEntitlementAccess.sprx", 37414, 0x548e_8426),
    ("libSceNpEntitlementAccess.sprx", 33206, 0x4b1d_e94a),
    ("libSceAppContent.sprx", 35558, 0x518e_6f93),
    ("libSceAppContent.sprx", 28966, 0x1c74_5846),
    ("libSceGameUpdate.sprx", 10326, 0x4820_0ddd),
    ("libSceGameUpdate.sprx", 9110, 0x226b_c515),
    ("libSceNpEntitlementAccess.sprx", 37750, 0xaf0f_f584),
    ("libSceNpEntitlementAccess.sprx", 33222, 0xebad_2e15),
    ("libScePlayGo.sprx", 17430, 0x9bdb_d12e),
    ("libScePlayGo.sprx", 11590, 0x3163_6f06),
    ("libScePlayGo.sprx", 20582, 0x46bc_f017),
    ("libScePlayGo.sprx", 14390, 0xb801_64e7),
];

/// Whether `data`, a file named `name` (case-insensitive), is a known older release (OLD_EMUS).
fn old_emu(name: &str, data: &[u8]) -> bool {
    OLD_EMUS.iter().any(|&(n, len, crc)| {
        n.eq_ignore_ascii_case(name) && data.len() as u64 == len && ampr_index::crc32(&[data]) == crc
    })
}

/// old_emu for the file at `path`, read only when its size is a known one.
fn old_emu_file(path: &Path, name: &str) -> bool {
    let Ok(len) = fs::metadata(path).map(|m| m.len()) else { return false };
    OLD_EMUS.iter().any(|&(n, l, _)| l == len && n.eq_ignore_ascii_case(name))
        && fs::read(path).is_ok_and(|d| old_emu(name, &d))
}

/// ng's library file name, in the ampr dir and in a game's fakelib.
const AMPR_LIB: &str = "libSceAmpr.sprx";
/// ng's embedded identity, followed by its X.Y.Z version (plain bytes).
const NG_ID: &[u8] = b"ampr-emu-ng ";

/// The version text of the first `ampr-emu-ng X.Y.Z` identity of `data` that parses
/// (crate::parse_version); None for no identity (upstream ampr_emu, anything else).
fn ng_version(data: &[u8]) -> Option<&str> {
    data.windows(NG_ID.len()).enumerate().filter(|(_, w)| *w == NG_ID).find_map(|(i, _)| {
        let rest = &data[i + NG_ID.len()..];
        let end = rest.iter().position(|b| !(b.is_ascii_digit() || *b == b'.' || *b == b'v')).unwrap_or(rest.len());
        std::str::from_utf8(&rest[..end]).ok().filter(|v| crate::parse_version(v).is_some())
    })
}

/// Whether the game's own ng is newer than ours (crate::newer): `game` must be an ampr-emu-ng, an
/// `ours` without identity is older than any.
fn game_ng_newer(game: Option<&str>, ours: Option<&str>) -> bool {
    crate::newer(game, ours)
}

/// Whether to keep the game's or backport's libSceAmpr.sprx in `fakelib` over AMPR_DIR's: only an
/// ampr-emu-ng strictly newer than AMPR_DIR's (logged). ponytail: a hand-placed ampr build in
/// AMPR_DIR also loses to a newer game copy.
fn keep_game_ng(tid: &str, fakelib: &Path) -> bool {
    let (Some(game), Some(ours)) = (find_ci(fakelib, AMPR_LIB), find_ci(Path::new(AMPR_DIR), AMPR_LIB)) else {
        return false;
    };
    let v = |p: &Path| fs::read(p).ok().and_then(|d| ng_version(&d).map(str::to_owned));
    let (g, o) = (v(&game), v(&ours));
    let keep = game_ng_newer(g.as_deref(), o.as_deref());
    if keep {
        let fmt = |v: &Option<String>| v.clone().unwrap_or_else(|| "an unknown build".into());
        log!("{tid}: fakelib: kept the game's ampr-emu-ng {} (newer than {})", fmt(&g), fmt(&o));
    }
    keep
}

/// dlc_emu.ini at a folder game's root, an installed game's /system_ex/app/<TID> or the backport's
/// root (a pkg's own is checked in compose_fakelib).
/// Exact name: dlc_emu opens /app0/dlc_emu.ini, and the internal SSD is case-sensitive.
fn dlc_ini_on_disk(src: &Source) -> bool {
    (src.kind != Kind::Pkg && app0(src).join(DLC_INI).is_file())
        || src.backport.as_ref().is_some_and(|b| b.join(DLC_INI).is_file())
}

/// The eboot.bin searched for a PlayGo import: the backport's if it has one, else the folder's,
/// else (pkg) the cache's extracted copy, else (installed) /system_ex/app/<TID>/eboot.bin when it
/// is a file now. None (installed, not mounted or unreadable): the PlayGo import is unknown.
fn eboot(src: &Source) -> Option<PathBuf> {
    eboot_in(src, Path::new(SYSTEM_APP))
}

fn eboot_in(src: &Source, system_app: &Path) -> Option<PathBuf> {
    match &src.backport {
        Some(b) if b.join("eboot.bin").is_file() => Some(b.join("eboot.bin")),
        _ => match src.kind {
            Kind::Pkg => Some(dir(&src.title_id).join("eboot.bin")),
            Kind::Folder(_) => Some(src.path.join("eboot.bin")),
            Kind::Installed => Some(system_app.join(&src.title_id).join("eboot.bin")).filter(|p| p.is_file()),
        },
    }
}

/// Title ids whose missing installed eboot.bin was logged in this run.
static NO_EBOOT_LOGGED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// What the emulator choice depends on, stat only (no eboot read): null without add_emulators.
/// ponytail: a pkg's own dlc_emu.ini is not here; a pkg change rebuilds through the stamp.
fn emu_state(src: &Source, s: &Settings) -> serde_json::Value {
    if !s.add_emulators {
        return serde_json::Value::Null;
    }
    let eboot = eboot(src).and_then(|p| fs::metadata(p).ok()).map(|m| (m.len(), m.modified().map(secs).unwrap_or(0)));
    // A backport's eboot.bin hides the mount state from `eboot`: look at the install's own.
    let unmounted = src.kind == Kind::Installed && !app0(src).join("eboot.bin").is_file();
    let mut v = serde_json::json!({
        "dlc_emu": DLC_EMU_VERSION.trim(),
        "pgo_stub": PGO_STUB_VERSION.trim(),
        "dlc_ini": dlc_ini_on_disk(src),
        "eboot": eboot,
        "playgo_off": playgo_off(src, s),
    });
    // The fallbacks below hold only while the backport is the same one (it may have fed them).
    if let (Kind::Installed, Some(b)) = (&src.kind, &src.backport) {
        v["backport"] = b.to_string_lossy().into();
        // The system mount's part of dlc_ini alone, for the fallback below.
        v["app0_dlc_ini"] = app0(src).join(DLC_INI).is_file().into();
    }
    // ponytail: an installed game's /system_ex/app/<TID> may be mounted only around a launch: while
    // it is not, the last seen eboot and dlc_ini stay, so the fakelib is not recomposed every pass.
    if unmounted {
        let old = fs::read(dir(&src.title_id).join(AMPR_STATE)).ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .filter(|o| o["emus"]["backport"] == v["backport"]);
        if let Some(o) = old.as_ref().map(|o| &o["emus"]["eboot"]).filter(|o| eboot.is_none() && !o.is_null()) {
            v["eboot"] = o.clone();
        }
        // The backport's dlc_emu.ini stays live; only the system mount's part falls back (without a
        // backport, dlc_ini is that part alone).
        let key = if src.backport.is_some() { "app0_dlc_ini" } else { "dlc_ini" };
        if old.as_ref().is_some_and(|o| o["emus"][key] == true) {
            v[key] = true.into();
            v["dlc_ini"] = true.into();
        }
    }
    v
}

/// Whether the PlayGo stub is turned off for this title (the playgo_off setting).
fn playgo_off(src: &Source, s: &Settings) -> bool {
    s.playgo_off.iter().any(|t| *t == src.title_id)
}

/// The memoized "the eboot imports PlayGo" (AMPR_STATE "playgo"[2]) of title `tid`; None when unknown.
/// ponytail: the memo as last written, not checked against the eboot now.
pub fn playgo_import(tid: &str) -> Option<bool> {
    let b = fs::read(dir(tid).join(AMPR_STATE)).ok()?;
    serde_json::from_slice::<serde_json::Value>(&b).ok()?["playgo"][2].as_bool()
}

/// Whether this title gets an empty fakelib/, so no overlay at launch (the overlay_off setting).
/// Folder games only: a pkg needs ampr-emu-ng's pkg mode, so a pkg source of a title turned off as
/// a folder keeps its fakelib.
fn overlay_off(src: &Source, s: &Settings) -> bool {
    !matches!(src.kind, Kind::Pkg) && s.overlay_off.iter().any(|t| *t == src.title_id)
}

/// The AMPR_STATE contents for `src` now. "overlay_off" only when set, so a cache written before
/// the setting existed stays fresh.
fn fakelib_state(src: &Source, s: &Settings) -> serde_json::Value {
    let mut state = serde_json::json!({"ampr": ampr_state(src, s), "emus": emu_state(src, s)});
    if overlay_off(src, s) {
        state["overlay_off"] = true.into();
    }
    state
}

/// Whether `r` contains `needle` (non-empty), read in `chunk`-sized pieces that overlap by
/// needle.len() - 1 bytes, so a match across two reads is found.
fn contains(r: &mut impl std::io::Read, needle: &[u8], chunk: usize) -> Result<bool> {
    let keep = needle.len().saturating_sub(1);
    let mut buf = vec![0u8; chunk + keep];
    let mut have = 0;
    loop {
        let n = match r.read(&mut buf[have..]) {
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            r => r?,
        };
        if n == 0 {
            return Ok(false);
        }
        let end = have + n;
        if buf[..end].windows(needle.len()).any(|w| w == needle) {
            return Ok(true);
        }
        let k = keep.min(end);
        buf.copy_within(end - k..end, 0);
        have = k;
    }
}

/// Whether the game imports libScePlayGo. ponytail: only eboot.bin is searched, a PlayGo import in
/// another prx module is missed; an encrypted eboot never matches (no stub, the safe side).
/// A missing eboot: false. A read error: logged with the title id and path, None (taken as false,
/// not memoized).
fn wants_playgo(src: &Source) -> Option<bool> {
    let Some(path) = eboot(src) else {
        let mut logged = NO_EBOOT_LOGGED.lock().unwrap_or_else(|e| e.into_inner());
        if !logged.contains(&src.title_id) {
            logged.push(src.title_id.clone());
            log!("{}: {}/{}/eboot.bin not readable (not mounted?), no eboot.bin in the backport: PlayGo import unknown, no pgo_stub",
                src.title_id, SYSTEM_APP, src.title_id);
        }
        return None;
    };
    let found = File::open(&path).and_then(|mut f| contains(&mut f, PLAYGO_NEEDLE, CHUNK));
    match found {
        Ok(b) => Some(b),
        Err(e) if e.kind() == ErrorKind::NotFound => Some(false),
        Err(e) => {
            log!("{}: {}: PlayGo check: {e}", src.title_id, path.display());
            None
        }
    }
}

/// wants_playgo, memoized in AMPR_STATE's "playgo" as [len, mtime secs, result] of the eboot:
/// `had` is the previous AMPR_STATE, `state` the one being written (gets the memo).
/// ponytail: an eboot rewritten with the same size and second keeps the old result.
fn wants_playgo_memo(src: &Source, had: &serde_json::Value, state: &mut serde_json::Value) -> bool {
    let Some((len, mtime)) = eboot(src).and_then(|p| fs::metadata(p).ok()).map(|m| (m.len(), m.modified().map(secs).unwrap_or(0))) else {
        // emu_state's fallback: an installed title's unmounted /system_ex/app/<TID> keeps the last
        // result, so the fakelib agrees with the eboot AMPR_STATE records (same test as emu_state).
        let same_bp = had["emus"]["backport"] == state["emus"]["backport"];
        if let (Kind::Installed, true, Some(b)) = (&src.kind, same_bp, had["playgo"][2].as_bool()) {
            return b;
        }
        return wants_playgo(src).unwrap_or(false);
    };
    let old = &had["playgo"];
    if old[0].as_u64() == Some(len) && old[1].as_u64() == Some(mtime) {
        if let Some(b) = old[2].as_bool() {
            state["playgo"] = old.clone();
            return b;
        }
    }
    let b = wants_playgo(src);
    if let Some(b) = b {
        state["playgo"] = serde_json::json!([len, mtime, b]);
    }
    b.unwrap_or(false)
}

pub fn check(src: &Source, s: &Settings) -> Fresh {
    let done = dir(&src.title_id).join(DONE);
    let Ok(done_mtime) = fs::metadata(&done).and_then(|m| m.modified()) else { return Fresh::Missing };
    let old: serde_json::Value =
        fs::read(&done).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let new = stamp(src, s);
    // An older layout rebuilds, moved or not (only_moved would refuse it too: "v" differs).
    if old.get("v") != new.get("v") {
        return Fresh::Stale(format!("cache layout v{} -> v{STAMP_V}", old.get("v").unwrap_or(&serde_json::Value::Null)));
    }
    if old.get("source") != new.get("source") {
        if let Some(from) = old["source"].as_str().map(PathBuf::from) {
            let exists = fs::symlink_metadata(&from).is_ok();
            if is_move(&old, &new, exists, || same_files(src, s, &old, done_mtime)) {
                return Fresh::Moved(from);
            }
        }
    }
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
    // A `.done` from before the game_fakelib setting was removed: emulators/off filtered fakelib/.
    // refresh_fakelib recomposes it in full and rewrites `.done` without the key; "full" is fresh.
    if old.get("game_fakelib").is_some_and(|m| m != "full") {
        return Fresh::Fakelib("game_fakelib setting removed".into());
    }
    let had: serde_json::Value = fs::read(dir(&src.title_id).join(AMPR_STATE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    // An older `.ampr` (ampr files only, no "emus") differs too: every cache refreshes once, which
    // also drops a libSceSaveData.native.sprx that an older build copied (keep_game_lib).
    // Only "ampr", "emus" and "overlay_off" count: the "playgo" memo never triggers a refresh.
    let new = fakelib_state(src, s);
    if ["ampr", "emus", "overlay_off"].iter().any(|&k| had.get(k) != new.get(k)) {
        return Fresh::Fakelib(format!("{AMPR_DIR}, inject_ampr, ampr_installed, the bundled emulators, playgo_off, overlay_off or ampr_off changed"));
    }
    Fresh::Fresh
}

/// Whether fakelib file `name` goes into the cache's fakelib/: everything but libkernel.sprx.
fn keep_fakelib(name: &str) -> bool {
    !name.eq_ignore_ascii_case("libkernel.sprx")
}

/// keep_fakelib for the game's own lib dir, also without libSceSaveData.native.sprx: it breaks
/// saves (#68: CE-107098-5 until removed; #64: ShadowMount+ "full" fails, "emulators only" works).
/// ponytail: a backport's lib dir and a game's fakelib2/ keep it (keep_fakelib): both are deliberate
/// overrides, used as-is.
fn keep_game_lib(name: &str) -> bool {
    keep_fakelib(name) && !name.eq_ignore_ascii_case("libSceSaveData.native.sprx")
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

/// The bundled emulators (EMUS) into `fakelib`: a file of that name the game or backport ships is
/// upgraded when it is a known older release (OLD_EMUS), else left alone; a missing one is added
/// when wanted (dlc_emu: `dlc`; pgo_stub: `playgo()`, called only then). The dlc_emu set is one
/// unit: if any of its files is not a known older release, none of the three is touched (no mixed
/// set). pgo_stub goes per file; with `pgo_off` it is skipped entirely (not added, not upgraded),
/// so a libScePlayGo.sprx the game or backport ships stays as shipped.
fn add_emus(tid: &str, fakelib: &Path, dlc: bool, pgo_off: bool, mut playgo: impl FnMut() -> bool) -> Result<()> {
    // Per entry: None = missing, Some((path, known old, the bundled file itself)).
    let found: Vec<_> = EMUS.iter().map(|&(name, data, ..)| find_ci(fakelib, name).map(|p| {
        let old = old_emu_file(&p, name);
        let current = !old && fs::read(&p).is_ok_and(|b| b == data);
        (p, old, current)
    })).collect();
    // A copy of the bundled release fits the set too: only other content keeps the game's set.
    let dlc_kept = EMUS.iter().zip(&found).any(|(e, f)| e.3 && f.as_ref().is_some_and(|(_, old, cur)| !old && !cur));
    if dlc_kept {
        log!("{tid}: fakelib: kept the game's dlc_emu files (not a known release)");
    }
    for (&(name, data, from, is_dlc), f) in EMUS.iter().zip(found) {
        if is_dlc && dlc_kept {
            continue;
        }
        if !is_dlc && pgo_off {
            log!("{tid}: fakelib: PlayGo stub off (playgo_off)");
            continue;
        }
        let version = if is_dlc { DLC_EMU_VERSION } else { PGO_STUB_VERSION }.trim();
        let (dest, what) = match f {
            // pgo_stub: playgo() only fills the import memo here (an old stub is upgraded either
            // way), so the UI offers the per-game switch for this game too.
            Some((have, true, _)) => {
                if !is_dlc {
                    playgo();
                }
                (have, "upgraded")
            }
            Some(_) => continue,
            // The eboot is read only when still needed.
            None if (if is_dlc { dlc } else { playgo() }) => (fakelib.join(name), "added"),
            None => continue,
        };
        fs::write(&dest, data)?;
        exec_mode(&dest)?;
        log!("{tid}: fakelib: {what} {name} ({from} {version})");
    }
    Ok(())
}

/// fakelib/ from scratch: the backport's lib dir (all files) if any, else the game's own (pkg:
/// extracted from `pkg`; folder: copied), all of it but libSceSaveData.native.sprx; libkernel.sprx
/// never; then each bundled emulator (EMUS) the game needs and fakelib/ lacks (add_emulators: the
/// DLC set for a dlc_emu.ini at the game or backport root, pgo_stub for a PlayGo import in
/// eboot.bin, memoized in AMPR_STATE, none for a title in playgo_off), never over a file of that name unless it is a known older
/// release (add_emus: OLD_EMUS, then upgraded; the dlc_emu set as one unit); then the ampr files on
/// top (inject_ampr, always for a pkg), except over the game's own newer ampr-emu-ng. All 0777.
/// Writes AMPR_STATE. Freshness needs no new state: the game's libs
/// are inputs (pkg: stamp), AMPR_DIR is ampr_state, the bundled versions are emu_state.
fn compose_fakelib(src: &Source, s: &Settings, pkg: Option<(&Io, &Pkg)>) -> Result<()> {
    let dir = dir(&src.title_id);
    let fakelib = dir.join("fakelib");
    match fs::remove_dir_all(&fakelib) {
        Err(e) if e.kind() != ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    fs::create_dir_all(&fakelib)?;
    if overlay_off(src, s) {
        // Empty fakelib/: launch.rs then mounts no overlay. The playgo memo is kept (keyed by the eboot).
        log!("{}: fakelib: overlay off (overlay_off)", src.title_id);
        let mut state = fakelib_state(src, s);
        if let Some(memo) = fs::read(dir.join(AMPR_STATE)).ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("playgo").cloned()) {
            state["playgo"] = memo;
        }
        return fs::write(dir.join(AMPR_STATE), state.to_string());
    }
    if let Some(bp) = bp_lib(src) {
        copy_flat(&bp, &fakelib, keep_fakelib)?;
    } else if let Some((io, p)) = pkg {
        // SMP order inside the game: fakelib2/ (used as-is) wins over fakelib/.
        let game_lib = if p.files().any(|e| e.path.to_ascii_lowercase().starts_with(LIB_DIRS[0])) {
            LIB_DIRS[0]
        } else {
            LIB_DIRS[1]
        };
        let mut buf = vec![0u8; CHUNK];
        for e in p.files().filter(|e| !e.is_dir) {
            let lower = e.path.to_ascii_lowercase();
            let Some(name) = lower.strip_prefix(game_lib) else { continue };
            let name = &e.path[e.path.len() - name.len()..];
            if !name.contains('/') && (if game_lib == LIB_DIRS[0] { keep_fakelib(name) } else { keep_game_lib(name) }) {
                extract(p, io, &e, &fakelib.join(name), &mut buf)?;
            }
        }
    } else if let Some(lib) = lib_dir(src) {
        let keep = if lib.ends_with("fakelib2") { keep_fakelib } else { keep_game_lib };
        copy_flat(&lib, &fakelib, keep)?;
    }
    if drops_game_ng(src, s) {
        if let Some(p) = find_ci(&fakelib, AMPR_LIB) {
            fs::remove_file(&p)?;
            log!("{}: fakelib: dropped the backport's {} (ampr_installed off): {}", src.title_id, AMPR_LIB, p.display());
        }
    }
    // Our ampr-emu-ng on top of whatever the game or backport ships (its own libSceAmpr.sprx included).
    // State read before the copy: a file replaced mid-copy (ampr auto-update) then differs at the
    // next check and refreshes fakelib again, instead of being recorded as already copied.
    let mut state = fakelib_state(src, s);
    let had: serde_json::Value =
        fs::read(dir.join(AMPR_STATE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    // An unused memo is kept: it is keyed by the eboot's size and mtime.
    if had.get("playgo").is_some() {
        state["playgo"] = had["playgo"].clone();
    }
    if s.add_emulators {
        // emus.dlc_ini is emu_state's: live, or the last recorded one while an installed title's
        // /system_ex/app/<TID> is not mounted (the backport's stays live).
        let dlc = state["emus"]["dlc_ini"] == true
            || pkg.is_some_and(|(_, p)| p.files().any(|e| !e.is_dir && e.path == DLC_INI));
        add_emus(&src.title_id, &fakelib, dlc, playgo_off(src, s), || wants_playgo_memo(src, &had, &mut state))?;
    }
    if injects_ampr(src, s) {
        // No ng downgrade: the game's own newer ampr-emu-ng stays.
        let keep = keep_game_ng(&src.title_id, &fakelib);
        copy_flat(Path::new(AMPR_DIR), &fakelib, |n| ampr_copied(n) && !(keep && n.eq_ignore_ascii_case(AMPR_LIB)))?;
    }
    fs::write(dir.join(AMPR_STATE), state.to_string())
}

/// Recomposes only fakelib/ (Fresh::Fakelib): no extraction besides a pkg's own fakelib, no
/// index, no re-registration. ponytail: a launch reads fakelib/ while this runs only if a game
/// starts mid-refresh (the scan loop is paused while one runs).
pub fn refresh_fakelib(src: &Source, s: &Settings) -> Result<()> {
    log!("{}: refreshing {}", src.title_id, dir(&src.title_id).join("fakelib").display());
    let pkg = if src.kind == Kind::Pkg { Some(open(&src.path)?) } else { None };
    compose_fakelib(src, s, pkg.as_ref().map(|(io, p)| (io, p)))
        .and_then(|()| {
            // check() matched the stamp and inputs: the current stamp with `.done`'s own mtime
            // (drops a legacy game_fakelib key, keeps the input checks).
            let d = dir(&src.title_id);
            let mtime = fs::metadata(d.join(DONE))?.modified()?;
            write_done(&d, src, s, mtime)
        })
        .map_err(|e| {
            let e = Error::new(e.kind(), format!("{} ({}): fakelib refresh: {e}", src.title_id, src.path.display()));
            log!("{e}");
            e
        })
}

/// Always rebuilds `src`'s cache. Errors carry the title id and source path and are logged.
pub fn build(src: &Source, s: &Settings) -> Result<()> {
    let d = dir(&src.title_id);
    log!("{}: building cache {} from {}", src.title_id, d.display(), src.path.display());
    let t0 = std::time::Instant::now();
    let mut before = None;
    let r = (|| {
        // Keep the dir: /system_ex/app/<TID> is a nullfs of this exact directory, and deleting it
        // would leave that mount on a dead inode (launch fails with 0x80940033).
        fs::create_dir_all(&d)?;
        clear_build(&d)?;
        // What is left is the game's own files: never part of the build, never removed by it.
        let b = before.insert(walk(&d).into_iter().map(|e| e.0).collect::<HashSet<_>>());
        build_inner(src, s, b)
    })();
    if r.is_ok() {
        log!("{}: cache built in {:.1} s", src.title_id, t0.elapsed().as_secs_f64());
    }
    r.map_err(|e| {
        let e = Error::new(e.kind(), format!("{} ({}): cache build: {e}", src.title_id, src.path.display()));
        log!("{e}");
        // Without `.done` the cache is unusable: free what this build wrote. A failed clear_build
        // has no `before`: nothing is removed then.
        if let Some(b) = &before {
            remove_created(&d, b);
        }
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
/// A pkg's stamp also gets its `pkg_id` (for `is_move`; check() ignores the extra key).
fn write_done(dir: &Path, src: &Source, s: &Settings, start: SystemTime) -> Result<()> {
    let done = dir.join(DONE);
    let mut st = stamp(src, s);
    st[if src.kind == Kind::Pkg { PKG_ID } else { FOLDER_ID }] =
        if src.kind == Kind::Pkg { pkg_id(&src.path)? } else { folder_id(src, s) }.into();
    fs::write(&done, st.to_string())?;
    File::options().write(true).open(&done)?.set_modified(start)
}

/// Fresh::Moved: points the cache at `src`, the same game moved from `from`, without a rebuild.
/// pkg: new stamp source, kraken.image rewritten when the file name changed, `.done` mtime now
/// (the moved .pkg may be newer than the old `.done`; its pkg_id matched); extracted files and the
/// indexes stay (ng checks the pkg's size only, not its mtime). Folder: new stamp, `.done` mtime
/// now; main.rs remounts /system_ex/app/<TID>. Errors carry the title id and both paths.
pub fn relink(src: &Source, s: &Settings, from: &Path) -> Result<()> {
    let d = dir(&src.title_id);
    log!("{}: moved {} -> {}; relinking {}", src.title_id, from.display(), src.path.display(), d.display());
    let r = (|| {
        if src.kind == Kind::Pkg {
            let name = src.path.file_name().and_then(|n| n.to_str());
            let name = name.ok_or_else(|| bad(format!("{}: file name is not UTF-8", src.path.display())))?;
            let image = format!("{PKG_MOUNT_DIR}/{name}");
            if fs::read_to_string(d.join(IMAGE_FILE)).ok().as_deref() != Some(image.as_str()) {
                fs::write(d.join(IMAGE_FILE), image)?;
            }
        }
        // `.done` mtime now: the moved inputs may be newer than the old one (a copy). A legacy
        // game_fakelib `.done` never gets here: it has no pkg_id/folder_id, so it is never a move.
        write_done(&d, src, s, SystemTime::now())
    })();
    r.map_err(|e| {
        let e = Error::new(e.kind(), format!("{} ({} -> {}): relink: {e}", src.title_id, from.display(), src.path.display()));
        log!("{e}");
        e
    })
}

fn build_inner(src: &Source, s: &Settings, before: &HashSet<PathBuf>) -> Result<()> {
    let start = SystemTime::now();
    let is_pkg = src.kind == Kind::Pkg;
    let dir = dir(&src.title_id);
    let mut files: BTreeMap<String, IndexFile> = BTreeMap::new();
    let pkg = if is_pkg { Some(open(&src.path)?) } else { None };
    if let Some((io, p)) = &pkg {
        let mut buf = vec![0u8; CHUNK];
        // (files, bytes) extracted because the index cannot map them, sparse ones aside (logged each).
        let mut unpackable = (0u64, 0u64);
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
            // A file stays in the pkg when the index can describe its blocks (a sparse hole is a
            // zero chunk, ampr_index). ponytail: any other file is extracted whole to the internal
            // SSD (its size, holes included): sparse holes off its 256 KiB block grid, or a short
            // compressed block running past its end (packers such as ps5-dump-forge put several
            // small files in one block; issue #52: Astro Bot, 4668 of 156246 files, 19 MB).
            let packed = if extracted(e.path) {
                None
            } else {
                // blocks() refuses (Unsupported only) a file whose data starts inside a block
                // shared with the file before it: extracted too, read_at decodes it.
                let c = if p.blocks(&e).is_ok() { Some(chunks(p, &e)?) } else { None };
                if let Some(c) = c.filter(|c| ampr_index::packable(e.size, c)) {
                    Some(c)
                } else if p.has_sparse(&e) {
                    log!("{}: {}: sparse holes off the 256 KiB grid, extracting {} bytes", src.title_id, e.path, e.size);
                    None
                } else {
                    unpackable.0 += 1;
                    unpackable.1 += e.size;
                    None
                }
            };
            if packed.is_none() {
                extract(p, io, &e, &dir.join(e.path), &mut buf)?;
            }
            files.insert(lower, IndexFile { path: e.path.to_string(), size: e.size, chunks: packed });
        }
        if unpackable.0 > 0 {
            log!(
                "{}: {} files in compressed blocks the index cannot map (shared or short), extracted {} bytes from {}",
                src.title_id,
                unpackable.0,
                unpackable.1,
                src.path.display()
            );
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
