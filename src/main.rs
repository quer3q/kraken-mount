//! Kraken Mount (kraken-mount): finds FPKG .pkg files and game folders on the internal SSD and USB/extended
//! drives, keeps a per-game cache in /data/krakenmount/caches/<TID>, registers each game on the
//! home screen and overlays its fakelib at launch (DESIGN.md). Web UI on port web::PORT.

// ponytail: ampr_index keeps two unused format constants (FILE_STREAMING, CHUNK_STREAMING).
#![allow(dead_code)]

#[macro_use]
mod sys;
mod i18n;
mod install;
mod installed;
mod ampr_index;
mod cache;
mod config;
mod crash;
mod launch;
mod net;
mod register;
mod scan;
mod storage;
mod update;
mod web;

use cache::Fresh;
use config::Settings;
use scan::{Kind, Source};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use web::State;

pub const DATA_DIR: &str = "/data/krakenmount";

/// A system toast when `notify=1`; always logged.
fn toast(s: &Settings, msg: &str) {
    if s.notify {
        sys::notify(msg);
    } else {
        log!("{msg}");
    }
}

/// What a failed game's retry waits for: another stamp, or any build input (pkg/folder files,
/// backport, ampr dir) appearing, vanishing or changing mtime, or the game's PlayGo, overlay or ng switch.
fn inputs_key(src: &Source, s: &Settings) -> String {
    let mut k = cache::stamp(src, s).to_string();
    k += &format!("|playgo_off={}", s.playgo_off.contains(&src.title_id));
    k += &format!("|overlay_off={}", s.overlay_off.contains(&src.title_id));
    k += &format!("|ng={}", s.ng_installed(&src.title_id));
    for p in cache::inputs(src, s) {
        let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
        k += &format!("|{}={mtime:?}", p.display());
    }
    k
}

/// Low /data space: the UI banner, and a toast once per run. Checked before and after builds.
fn check_space(state: &State, s: &Settings) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let free = storage::drives().into_iter().find(|d| d.mount == "/data").map_or(u64::MAX, |d| d.free);
    let low = free < s.low_space_gb << 30;
    state.low_space.store(low, Ordering::SeqCst);
    if low && !WARNED.swap(true, Ordering::SeqCst) {
        toast(s, &i18n::low_space(i18n::lang(), free >> 30));
    }
}

fn backport_of(src: &Source) -> Option<&std::path::Path> {
    match src.kind {
        Kind::Folder(_) => src.backport.as_deref(),
        // A pkg's backport is copied into its cache; an installed title is never mounted by us.
        Kind::Pkg | Kind::Installed => None,
    }
}

/// What build_and_register did with a title.
#[derive(PartialEq)]
enum Reg {
    /// Registered by an earlier run: only mounted.
    Kept,
    /// Registered now.
    New,
    /// Staged and mounted, registered by the pass's `install_staged` (FW 12+).
    Staged,
}

/// Builds when stale or forced, then registers. An installed title: build only (never registered
/// or mounted by us), then Kraken's old /system_ex/app/<TID> stack dropped (drop_old_mount).
fn build_and_register(state: &State, s: &Settings, src: &Source, fresh: &Fresh, forced: bool) -> std::io::Result<Reg> {
    let tid = &src.title_id;
    if let (false, Fresh::Fakelib(why)) = (forced, fresh) {
        // Only fakelib/ is behind: recompose it, then go on as a fresh cache (mount or register).
        log!("{tid}: {why}; refreshing fakelib only");
        cache::refresh_fakelib(src, s)?;
    }
    let stale = forced || !matches!(fresh, Fresh::Fresh | Fresh::Fakelib(_));
    let app0 = cache::app0(src);
    if src.kind == Kind::Installed {
        if stale {
            // Run through ng until now: our nullfs over the cache goes before the build empties that cache.
            if cache::stamp_kind(tid).as_deref() == Some("pkg") {
                register::drop_cache_layer(tid)?;
            }
            remember_old_source(tid);
            log_rebuild(tid, fresh);
            state.update(&src.path, |g| g.status = "building");
            check_space(state, s);
            let r = cache::build(src, s);
            check_space(state, s);
            r?;
        }
        cache::fix_access(src);
        drop_old_mount(s, tid);
        return Ok(Reg::Kept);
    }
    if scan::installed_pkg(src) {
        // The console registered it: build when stale, then only our /system_ex/app/<TID> mount.
        if stale {
            log_rebuild(tid, fresh);
            state.update(&src.path, |g| g.status = "building");
            check_space(state, s);
            let r = cache::build(src, s);
            check_space(state, s);
            r?;
        }
        cache::fix_access(src);
        register::mount_over_install(tid, &app0)?;
        return Ok(Reg::Kept);
    }
    if !stale && register::is_registered(tid) {
        // ponytail: registered by an earlier run; only the nullfs is redone (gone after a
        // reboot; a no-op when mounted): no sce_sys copy, no install call.
        cache::fix_access(src);
        register::mount(tid, &app0, backport_of(src))?;
        return Ok(Reg::Kept);
    }
    if stale {
        log_rebuild(tid, fresh);
        state.update(&src.path, |g| g.status = "building");
        check_space(state, s);
        let r = cache::build(src, s);
        check_space(state, s);
        r?;
    }
    // Once per run per title: build_and_register runs only for titles not registered in this run.
    cache::fix_access(src);
    let t0 = std::time::Instant::now();
    let new = register::register(tid, &app0, &cache::sce_sys(src), backport_of(src))?;
    log!("{tid}: {} in {} ms", if new { "registered" } else { "staged" }, t0.elapsed().as_millis());
    Ok(if new { Reg::New } else { Reg::Staged })
}

fn log_rebuild(tid: &str, fresh: &Fresh) {
    match fresh {
        Fresh::Stale(why) => log!("{tid}: cache stale ({why}); rebuilding"),
        Fresh::Missing => log!("{tid}: no cache; building"),
        Fresh::Fresh | Fresh::Fakelib(_) | Fresh::Moved(_) => log!("{tid}: rebuild forced"),
    }
}

/// Fresh::Moved: a folder game's /system_ex/app/<TID> stack (nullfs + backport unionfs) is redone
/// on the new folder first, so a failed relink is retried next scan; then the cache is relinked
/// (a pkg's app0 is its cache dir: no remount; launch.rs mounts the pkg's folder from the stamp).
/// Only reached from the scan loop, which never runs while a game does.
fn relink_moved(s: &Settings, src: &Source, from: &std::path::Path) -> std::io::Result<()> {
    if matches!(src.kind, Kind::Folder(_)) {
        register::remount(&src.title_id, &cache::app0(src), backport_of(src))?;
    }
    cache::relink(src, s, from)?;
    // The moved copy may have other modes, and a registered title skips build_and_register.
    cache::fix_access(src);
    // An install run through ng (a cache from its scan-root FPKG): not a move the user made.
    if !scan::installed_pkg(src) {
        toast(s, &i18n::moved(i18n::lang(), &src.title_name, &src.path.to_string_lossy()));
    }
    Ok(())
}

/// One game of the scan: check, build, register, status. A failed game is retried only when
/// its inputs_key changes or a rebuild is forced.
/// `staged` collects the titles waiting for the pass's `install_staged`.
fn process(
    state: &State, s: &Settings, src: &Source, registered: &mut HashSet<String>, failed: &mut HashMap<PathBuf, String>,
    staged: &mut Vec<Source>,
) {
    let tid = &src.title_id;
    let forced = state.forced.lock().unwrap().remove(tid);
    if src.kind != Kind::Installed {
        // Kraken may mount it now: a later install of it gets its stack checked again.
        NO_OLD_MOUNT.lock().unwrap().retain(|t| t != tid);
    }
    // Uninstalled outside Kraken Mount since this run registered it (a launcher's own remover, or
    // the home screen's Delete): its staged files are gone, so it registers in full again.
    // ponytail: two stats per title per pass; a deleted app.db row with the files left is not seen.
    if src.kind != Kind::Installed && registered.contains(tid) && !register::staged(tid) {
        log!("{tid}: no longer on the home screen; registering again");
        registered.remove(tid);
    }
    let mut fresh = cache::check(src, s);
    if let (false, Fresh::Moved(from)) = (forced, &fresh) {
        fresh = match relink_moved(s, src, from) {
            Ok(()) => cache::check(src, s),
            // The rebuild path below logs and reports what still fails.
            Err(e) => Fresh::Stale(format!("relink failed: {e}")),
        };
    }
    let r = if !forced && failed.get(&src.path).is_some_and(|k| *k == inputs_key(src, s)) {
        state.update(&src.path, |g| g.status = "error");
        return;
    } else if !forced && matches!(fresh, Fresh::Fresh) && registered.contains(tid) && src.kind == Kind::Installed {
        // The system mounts an installed title: only a leftover stack of ours is dropped (retried).
        drop_old_mount(s, tid);
        state.update(&src.path, |g| g.status = "ready");
        return;
    } else if !forced && matches!(fresh, Fresh::Fresh) && registered.contains(tid) {
        // Each scan: redo /system_ex/app/<TID> when it is gone or stale (USB replug, rest
        // mode); a no-op when app0 is mounted there. Status stays "ready" (no UI flicker).
        let r = if scan::installed_pkg(src) {
            register::mount_over_install(tid, &cache::app0(src))
        } else {
            register::mount(tid, &cache::app0(src), backport_of(src))
        };
        match r {
            Ok(()) => {
                state.update(&src.path, |g| g.status = "ready");
                return;
            }
            Err(e) => Err(e),
        }
    } else {
        state.update(&src.path, |g| g.status = "checking");
        build_and_register(state, s, src, &fresh, forced)
    };
    match r {
        Ok(Reg::Staged) => {
            state.update(&src.path, |g| g.status = "checking");
            staged.push(src.clone());
        }
        r => finish(state, s, src, registered, failed, r.map(|reg| reg == Reg::New), true),
    }
}

/// Installed titles whose /system_ex/app/<TID> holds no stack of ours any more (checked once per run).
static NO_OLD_MOUNT: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// (title id, folder or backport dir) of a cache stamp the installed rebuild replaced: drop_old_mount
/// also takes a mount of these as ours (a folder from a since-removed scan root).
/// ponytail: per run only; a payload restarted before the drop falls back to the root rules.
static OLD_SOURCES: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Before an installed title's cache is rebuilt: keeps the source and backport of a non-installed stamp.
fn remember_old_source(tid: &str) {
    if matches!(cache::stamp_kind(tid).as_deref(), None | Some("installed")) {
        return;
    }
    let mut old = OLD_SOURCES.lock().unwrap();
    for dir in [cache::stamp_source(tid), cache::stamp_backport(tid)].into_iter().flatten() {
        old.push((tid.to_string(), dir));
    }
}

/// The top mount at /system_ex/app/<TID> as (fstype, source without "<above>:"); None when
/// nothing is mounted on that exact path.
fn top_at(dst: &str) -> std::io::Result<Option<(String, String)>> {
    let c = std::ffi::CString::new(dst).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let (on, fstype, from) = (launch::c_field(&st.f_mntonname), launch::c_field(&st.f_fstypename), launch::c_field(&st.f_mntfromname));
    let from = from.strip_prefix("<above>:").unwrap_or(&from).to_string();
    Ok((on == dst).then_some((fstype, from)))
}

/// Whether a mount (`fstype` of `from`) is one Kraken Mount made for `tid`: a nullfs/unionfs of its
/// cache dir (pkg), of a path under a scan root (a folder game) or of a backport dir. Never one of
/// an installed copy (`.../user/app/...`, the system's own) or anything else (pfs, ...), so a scan
/// root like "/mnt/ext1" or "/" never claims the system's mount.
/// ponytail: path rules, not a record of what register mounted; a scan root holding a "user/app"
/// tree is not a supported layout.
fn ours(fstype: &str, from: &str, tid: &str, roots: &[String]) -> bool {
    if !matches!(fstype, "nullfs" | "unionfs") || from.contains("/user/app/") {
        return false;
    }
    let from = std::path::Path::new(from);
    from.starts_with(cache::dir(tid)) || from.starts_with("/data/homebrew/backports")
        || roots.iter().any(|r| r != "/" && from.starts_with(r))
}

/// A title Kraken registered (FPKG or folder) that is now installed: its /system_ex/app/<TID>
/// stack is dropped, top layer first, while the top layer is ours (never MNT_FORCE, never the
/// system's own mount below). ponytail: not register::unmount_title, which would also unmount the
/// system's layer under ours. Failure: logged with the title id and path, retried next pass.
fn drop_old_mount(s: &Settings, tid: &str) {
    if NO_OLD_MOUNT.lock().unwrap().iter().any(|t| t == tid) {
        return;
    }
    let dst = format!("/system_ex/app/{tid}");
    let mut roots = if s.scan_paths.is_empty() { storage::default_scan_roots() } else { s.scan_paths.clone() };
    roots.extend(OLD_SOURCES.lock().unwrap().iter().filter(|(t, _)| t == tid).map(|(_, d)| d.clone()));
    for _ in 0..4 {
        match top_at(&dst) {
            Ok(Some((fstype, from))) if ours(&fstype, &from, tid, &roots) => {
                log!("{tid}: installed: unmounting Kraken Mount's {dst} ({fstype} of {from})");
                if let Err(e) = sys::unmount(&dst, 0) {
                    log!("{tid}: installed: unmount {dst} ({from}): {e}; retried next pass");
                    return;
                }
            }
            Ok(_) => {
                NO_OLD_MOUNT.lock().unwrap().push(tid.to_string());
                return;
            }
            // No /system_ex/app/<TID> (yet): nothing of ours can be mounted there.
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => {
                NO_OLD_MOUNT.lock().unwrap().push(tid.to_string());
                return;
            }
            Err(e) => {
                log!("{tid}: installed: statfs {dst}: {e}; retried next pass");
                return;
            }
        }
    }
    log!("{tid}: installed: {dst} still has a mount of ours after 4 unmounts; retried next pass");
}

/// FW 12+: one sceAppInstUtilAppInstallAll for every title staged this pass, then each gets
/// what `process` gives a title registered on its own (status, toast, or the error).
fn install_staged(
    state: &State, s: &Settings, staged: &mut Vec<Source>, registered: &mut HashSet<String>,
    failed: &mut HashMap<PathBuf, String>,
) {
    if staged.is_empty() && !register::tile_staged() {
        return;
    }
    let tids: Vec<String> = staged.iter().map(|g| g.title_id.clone()).collect();
    // InstallAll walks every /user/app folder, also the one a base install is still writing.
    // Deferred like a running game defers it: the staged titles are not in `registered`, so the
    // next pass stages them again (and a staged tile stays staged). Logged once per wait.
    static DEFERRED_LOGGED: AtomicBool = AtomicBool::new(false);
    if let Some(busy) = install::busy_tid() {
        if !DEFERRED_LOGGED.swap(true, Ordering::SeqCst) {
            log!("register: InstallAll for [{}] waits for the install of {busy}; retried each pass", tids.join(", "));
        }
        staged.clear();
        return;
    }
    DEFERRED_LOGGED.store(false, Ordering::SeqCst);
    let r = register::install_all(&tids);
    let one = tids.len() == 1;
    for src in staged.drain(..) {
        let r = r.as_ref().map(|_| true).map_err(|e| std::io::Error::new(e.kind(), e.to_string()));
        finish(state, s, &src, registered, failed, r, one);
    }
    // One toast for a batch: the console shows queued toasts one by one and the PS button opens
    // the one on screen, so a toast per game (32 at once on a rebuild of all) blocked leaving a game.
    if tids.len() > 1 {
        let l = i18n::lang();
        toast(s, &if r.is_ok() { i18n::registered_many(l, tids.len()) } else { i18n::failed_many(l, tids.len()) });
    }
}

/// Status, toast (`notify`) and failure record of a processed title. Ok(true): registered now.
fn finish(
    state: &State, s: &Settings, src: &Source, registered: &mut HashSet<String>, failed: &mut HashMap<PathBuf, String>,
    r: std::io::Result<bool>, notify: bool,
) {
    let tid = &src.title_id;
    let (size, built) = (cache::size(tid), cache::built_at(tid));
    match r {
        Ok(new) => {
            failed.remove(&src.path);
            registered.insert(tid.clone());
            state.titles.lock().unwrap().insert(tid.clone());
            state.update(&src.path, |g| {
                (g.status, g.error, g.cache_size, g.cache_built) = ("ready", None, size, built);
            });
            if new && notify {
                toast(s, &i18n::registered(i18n::lang(), &src.title_name));
            }
        }
        Err(e) => {
            let e = format!("{tid} ({}): {e}", src.path.display());
            log!("{e}");
            failed.insert(src.path.clone(), inputs_key(src, s));
            state.update(&src.path, |g| {
                (g.status, g.error, g.cache_size, g.cache_built) = ("error", Some(e.clone()), size, built);
            });
            if notify {
                toast(s, &i18n::failed(i18n::lang(), &src.title_name));
            }
        }
    }
}

/// Replaces the UI's game list with the scan: an entry keeps its status while its source
/// stays listed, new ones are "pending", duplicates "duplicate". A source still being copied
/// keeps its old entry (it is not gone), a new one shows up once it is stable.
fn publish(state: &State, r: &scan::ScanResult) {
    *state.packages.lock().unwrap() = r.packages.clone();
    let mut games = state.games.lock().unwrap();
    let old = std::mem::take(&mut *games);
    for src in &r.games {
        let prev = old.iter().find(|g| g.src.path == src.path && g.src.title_id == src.title_id && g.status != "duplicate");
        games.push(match prev {
            Some(g) => web::Game { src: src.clone(), ..g.clone() },
            None => web::Game { src: src.clone(), status: "pending", error: None, cache_size: 0, cache_built: None },
        });
    }
    for src in &r.duplicates {
        games.push(web::Game { src: src.clone(), status: "duplicate", error: None, cache_size: 0, cache_built: None });
    }
    for (path, _) in &r.unstable {
        if let Some(g) = old.iter().find(|g| &g.src.path == path && g.status != "duplicate") {
            if !games.iter().any(|n| n.src.title_id == g.src.title_id && n.status != "duplicate") {
                games.push(g.clone());
            }
        }
    }
}
/// Debug: copies the kernel log (/dev/klog: module load failures, process crashes) to
/// /data/krakenmount/klog.txt, read through the web UI's /file route.
/// ponytail: restarts the file at 4 MiB; only one reader of /dev/klog can exist.
fn klog_capture() {
    use std::io::{Read, Write};
    const OUT: &str = "/data/krakenmount/klog.txt";
    let mut klog = match std::fs::File::open("/dev/klog") {
        Ok(f) => f,
        Err(e) => {
            log!("klog: cannot open /dev/klog: {e}");
            return;
        }
    };
    log!("klog: capturing to {OUT}");
    let mut buf = vec![0u8; 64 * 1024];
    let mut out = std::fs::File::create(OUT).ok();
    let mut written = 0usize;
    loop {
        let n = match klog.read(&mut buf) {
            Ok(0) => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            Ok(n) => n,
            Err(e) => {
                log!("klog: read failed: {e}");
                return;
            }
        };
        if written > 4 << 20 {
            out = std::fs::File::create(OUT).ok();
            written = 0;
        }
        if let Some(f) = out.as_mut() {
            let _ = f.write_all(&buf[..n]);
        }
        written += n;
    }
}

/// The ELF the tile's page asks elfldr to start (ui.html `ELF`), as Dump Forge saves itself.
pub const SELF_COPY: &str = "/data/krakenmount/kraken-mount.elf";
/// Stage 1 of ps5/build.sh, embedded only in the pushed (stage 2) ELF; the saved copy has none.
#[cfg(feature = "self-copy")]
const SELF_ELF: &[u8] = include_bytes!(env!("KRAKEN_SELF_ELF"));
#[cfg(not(feature = "self-copy"))]
const SELF_ELF: &[u8] = &[];

/// Saves SELF_ELF to SELF_COPY unless it is there already: temp file, fsync, rename.
/// ponytail: the last pushed ELF wins, no version order (Dump Forge keeps the newer one).
fn save_self_copy() {
    if SELF_ELF.is_empty() || std::fs::read(SELF_COPY).is_ok_and(|b| b == SELF_ELF) {
        return;
    }
    let tmp = format!("{SELF_COPY}.tmp");
    let r = std::fs::File::create(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, SELF_ELF).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, SELF_COPY));
    match r {
        Ok(()) => log!("saved {SELF_COPY} ({} bytes): the tile starts it", SELF_ELF.len()),
        Err(e) => log!("save {SELF_COPY}: {e}"),
    }
}

/// The ampr-emu-ng release bundled in the ELF (vendor/ampr-emu-ng; ps5/build.sh checks its SHA-256).
const BUNDLED_AMPR: &[u8] = include_bytes!("../vendor/ampr-emu-ng/libSceAmpr.sprx");
const BUNDLED_AMPR_VERSION: &str = include_str!("../vendor/ampr-emu-ng/VERSION");
/// `<version> <len> <crc32>` of the copy we last wrote (bundled or downloaded); v0.4.0 wrote
/// `<len> <crc32>` (version unknown). Outside AMPR_DIR: every file there goes into fakelib/.
const BUNDLED_MARK: &str = "/data/krakenmount/ampr.bundled";
const AMPR_RELEASE_URL: &str = "https://api.github.com/repos/quer3q/ampr-emu-ng/releases/latest";
const AMPR_ASSET: &str = "libSceAmpr.sprx";

fn mark_format(version: &str, len: usize, crc: u32) -> String {
    format!("{} {len} {crc:08x}", version.trim())
}

/// The mark's version when it records exactly (len, crc): Some(None) for the old format.
fn mark_version(mark: &str, len: usize, crc: u32) -> Option<Option<&str>> {
    let f: Vec<&str> = mark.split_whitespace().collect();
    let (v, l, c) = match f[..] {
        [l, c] => (None, l, c),
        [v, l, c] => (Some(v), l, c),
        _ => return None,
    };
    (l.parse::<usize>().ok()? == len && u32::from_str_radix(c, 16).ok()? == crc).then_some(v)
}

/// `vX.Y.Z` (leading v optional); None = unknown.
pub(crate) fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let v = v.trim();
    let mut it = v.strip_prefix('v').unwrap_or(v).split('.').map(|p| p.parse::<u32>().ok());
    let r = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(r)
}

/// `a` newer than `b`; an unknown version is older than any.
pub(crate) fn newer(a: Option<&str>, b: Option<&str>) -> bool {
    a.and_then(parse_version) > b.and_then(parse_version)
}

#[derive(Debug, PartialEq)]
enum Installed<'a> {
    Missing,
    /// Put there by hand: never replaced.
    Hand,
    /// A copy we wrote, with its version (None = unknown).
    Ours(Option<&'a str>),
}

/// `cur`: (len, crc32, equals the bundled copy) of AMPR_DIR/libSceAmpr.sprx, None when missing.
fn installed<'a>(cur: Option<(usize, u32, bool)>, bundled_version: &'a str, mark: Option<&'a str>) -> Installed<'a> {
    match cur {
        None => Installed::Missing,
        Some((_, _, true)) => Installed::Ours(Some(bundled_version)),
        Some((len, crc, false)) => match mark.and_then(|m| mark_version(m, len, crc)) {
            Some(v) => Installed::Ours(v),
            None => Installed::Hand,
        },
    }
}

/// The updater downloads `tag` when the file is missing or ours and older.
fn should_update(inst: &Installed, tag: &str) -> bool {
    match inst {
        Installed::Missing => true,
        Installed::Hand => false,
        Installed::Ours(v) => newer(Some(tag), *v),
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct Release {
    pub(crate) tag: String,
    pub(crate) url: String,
    pub(crate) size: usize,
}

/// tag_name and the `asset` asset of a GitHub releases/latest answer.
pub(crate) fn parse_release(body: &[u8], asset: &str) -> Result<Release, String> {
    let j: serde_json::Value = serde_json::from_slice(body).map_err(|e| format!("bad JSON: {e}"))?;
    let tag = j["tag_name"].as_str().ok_or("no tag_name")?.to_string();
    let assets = j["assets"].as_array().ok_or("no assets")?;
    let a = assets.iter().find(|a| a["name"].as_str() == Some(asset)).ok_or(format!("{tag}: no {asset} asset"))?;
    let url = a["browser_download_url"].as_str().ok_or("no browser_download_url")?.to_string();
    let size = a["size"].as_u64().and_then(|n| usize::try_from(n).ok()).ok_or("no asset size")?;
    Ok(Release { tag, url, size })
}

/// ELF or SELF (the signed form the PS5 loader also takes).
pub(crate) fn is_module(b: &[u8]) -> bool {
    b.starts_with(&[0x7f, 0x45, 0x4c, 0x46]) || b.starts_with(&[0x4f, 0x15, 0x3d, 0x1d])
}

/// Writes `b` to AMPR_DIR/libSceAmpr.sprx (0777) and records it in BUNDLED_MARK as `version`.
/// Temp file beside AMPR_DIR, not in it, so a scan never copies a half-written one.
fn write_ampr(path: &str, b: &[u8], version: &str) -> std::io::Result<()> {
    let tmp = format!("{DATA_DIR}/libSceAmpr.sprx.tmp");
    let r = std::fs::File::create(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, b).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o777)))
        .and_then(|()| std::fs::rename(&tmp, path));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r?;
    // ponytail: no mark = the file reads as hand-placed and is never replaced again (by the bundled
    // copy or the updater) until deleted; an error here is reported, not undone.
    write_mark(version, b)
}

fn write_mark(version: &str, b: &[u8]) -> std::io::Result<()> {
    std::fs::write(BUNDLED_MARK, mark_format(version, b.len(), ampr_index::crc32(&[b])))
        .map_err(|e| std::io::Error::new(e.kind(), format!("write {BUNDLED_MARK}: {e}")))
}

/// What AMPR_DIR/libSceAmpr.sprx (`path`) is now given BUNDLED_MARK's text, and whether it equals
/// the bundled copy. Err: a read error other than NotFound.
fn read_installed<'a>(path: &str, mark: Option<&'a str>) -> std::io::Result<(Installed<'a>, bool)> {
    let cur = match std::fs::read(path) {
        Ok(cur) => Some((cur.len(), ampr_index::crc32(&[&cur]), cur == BUNDLED_AMPR)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let bundled = cur.is_some_and(|c| c.2);
    Ok((installed(cur, BUNDLED_AMPR_VERSION.trim(), mark), bundled))
}

/// Puts the bundled libSceAmpr.sprx into AMPR_DIR when it is missing or is a copy we wrote
/// (bundled or downloaded) that is not newer. A file put there by hand (a debug build, say) is kept.
fn install_bundled_ampr() {
    let path = format!("{}/{AMPR_ASSET}", cache::AMPR_DIR);
    let bv = BUNDLED_AMPR_VERSION.trim();
    let mark = std::fs::read_to_string(BUNDLED_MARK).ok();
    match read_installed(&path, mark.as_deref()) {
        Ok((_, true)) => {
            let want = mark_format(bv, BUNDLED_AMPR.len(), ampr_index::crc32(&[BUNDLED_AMPR]));
            if mark.as_deref() != Some(want.as_str()) {
                if let Err(e) = write_mark(bv, BUNDLED_AMPR) {
                    log!("ampr: {e}");
                }
            }
            return;
        }
        Ok((Installed::Ours(v), _)) if newer(v, Some(bv)) => {
            log!("ampr: {path} is ampr-emu-ng {} (newer than the bundled {bv}): kept", v.unwrap_or("?"));
            return;
        }
        Ok((Installed::Ours(_) | Installed::Missing, _)) => {}
        Ok((Installed::Hand, _)) => {
            log!("ampr: {path} is not a bundled ampr-emu-ng: kept (delete it to get {bv})");
            return;
        }
        Err(e) => {
            log!("ampr: read {path}: {e}");
            return;
        }
    }
    match write_ampr(&path, BUNDLED_AMPR, bv) {
        Ok(()) => log!("ampr: installed bundled ampr-emu-ng {bv} ({} bytes) to {path}", BUNDLED_AMPR.len()),
        Err(e) => log!("ampr: install bundled ampr-emu-ng to {path}: {e}"),
    }
}

/// net::https_get with up to `attempts` tries 20 s apart; each failure logged after `prefix`.
pub(crate) fn get_retry(prefix: &str, url: &str, max: usize, attempts: u32) -> Result<Vec<u8>, String> {
    let mut last = String::new();
    for n in 1..=attempts {
        match net::https_get(url, max) {
            Ok(b) => return Ok(b),
            Err(e) => {
                log!("{prefix}: GET {url} (attempt {n}/{attempts}): {e}");
                last = e.to_string();
            }
        }
        if n < attempts {
            std::thread::sleep(Duration::from_secs(20));
        }
    }
    Err(last)
}

/// A hand-placed libSceAmpr.sprx moved aside by a forced update (outside AMPR_DIR: never copied).
/// ponytail: one backup; a second forced update over another hand-placed file replaces it.
const HAND_BACKUP: &str = "/data/krakenmount/libSceAmpr.sprx.hand";
const AMPR_MAX: usize = 16 << 20;
/// One update at a time: the start-up check and the web UI's "Update now" share it.
static AMPR_UPDATING: AtomicBool = AtomicBool::new(false);

fn hand_placed(path: &str) -> Result<bool, String> {
    let mark = std::fs::read_to_string(BUNDLED_MARK).ok();
    let (inst, _) = read_installed(path, mark.as_deref()).map_err(|e| format!("read {path}: {e}"))?;
    Ok(inst == Installed::Hand)
}

/// Installs the latest stable ampr-emu-ng release from GitHub. Without `force` (start-up check):
/// only when the installed file is ours (or missing) and older; a hand-placed file is kept. With
/// `force` (web UI): whatever is installed, a hand-placed file moved to HAND_BACKUP first. Returns a
/// one-line result for the UI (also logged); an install wakes the scan loop to refresh fakelibs.
fn ampr_update(state: &State, force: bool, attempts: u32) -> Result<String, String> {
    if AMPR_UPDATING.swap(true, Ordering::SeqCst) {
        return Err("an update is already running".into());
    }
    let r = ampr_update_inner(state, force, attempts);
    AMPR_UPDATING.store(false, Ordering::SeqCst);
    match &r {
        Ok(m) => log!("ampr: update: {m}"),
        Err(e) => log!("ampr: update: {e}"),
    }
    r
}

fn ampr_update_inner(state: &State, force: bool, attempts: u32) -> Result<String, String> {
    let path = format!("{}/{AMPR_ASSET}", cache::AMPR_DIR);
    let body = get_retry("ampr: update", AMPR_RELEASE_URL, 1 << 20, attempts).map_err(|e| format!("GET {AMPR_RELEASE_URL}: {e}"))?;
    let rel = parse_release(&body, AMPR_ASSET).map_err(|e| format!("{AMPR_RELEASE_URL}: {e}"))?;
    let mark = std::fs::read_to_string(BUNDLED_MARK).ok();
    let (inst, _) = read_installed(&path, mark.as_deref()).map_err(|e| format!("read {path}: {e}"))?;
    if !force && !should_update(&inst, &rel.tag) {
        let why = match inst {
            Installed::Hand => "hand-placed, kept".to_string(),
            Installed::Ours(v) => format!("installed {} is not older", v.unwrap_or("?")),
            Installed::Missing => String::new(),
        };
        return Ok(format!("latest {} skipped ({path}: {why})", rel.tag));
    }
    if rel.size > AMPR_MAX {
        return Err(format!("{} asset is {} bytes, over {AMPR_MAX}", rel.tag, rel.size));
    }
    let b = get_retry("ampr: update", &rel.url, AMPR_MAX, attempts).map_err(|e| format!("GET {}: {e}", rel.url))?;
    if b.len() != rel.size || !is_module(&b) {
        let magic = &b[..b.len().min(4)];
        return Err(format!(
            "{} from {}: {} bytes (asset size {}), magic {magic:02x?}: not installed",
            rel.tag, rel.url, b.len(), rel.size
        ));
    }
    // Checked again: a file put there by hand during the download is kept (forced: moved aside).
    if hand_placed(&path)? {
        if !force {
            return Ok(format!("latest {} skipped ({path}: hand-placed meanwhile, kept)", rel.tag));
        }
        std::fs::rename(&path, HAND_BACKUP).map_err(|e| format!("move {path} -> {HAND_BACKUP}: {e}"))?;
        log!("ampr: update: hand-placed {path} moved to {HAND_BACKUP}");
    }
    write_ampr(&path, &b, &rel.tag).map_err(|e| format!("install {} to {path}: {e}", rel.tag))?;
    let s = state.settings.lock().unwrap().clone();
    toast(&s, &i18n::ampr_updated(i18n::lang(), &rel.tag));
    state.wake();
    Ok(format!("installed ampr-emu-ng {} ({} bytes)", rel.tag, b.len()))
}

/// GET /api/ampr: what AMPR_DIR/libSceAmpr.sprx is now, and whether an update runs.
fn ampr_status() -> serde_json::Value {
    let path = format!("{}/{AMPR_ASSET}", cache::AMPR_DIR);
    let mark = std::fs::read_to_string(BUNDLED_MARK).ok();
    let (kind, version) = match read_installed(&path, mark.as_deref()) {
        Ok((_, true)) => ("bundled", Some(BUNDLED_AMPR_VERSION.trim())),
        Ok((Installed::Ours(Some(v)), _)) => ("downloaded", Some(v)),
        // v0.4.0 builds' mark: an older bundled copy.
        Ok((Installed::Ours(None), _)) => ("bundled", None),
        Ok((Installed::Hand, _)) => ("hand", None),
        Ok((Installed::Missing, _)) => ("missing", None),
        Err(_) => ("error", None),
    };
    serde_json::json!({"kind": kind, "version": version, "updating": AMPR_UPDATING.load(Ordering::SeqCst)})
}

/// ampr files are loaded by every game's sandbox: 0777 like the caches. A libkernel.sprx there
/// replaces the system one for every game, which is rarely wanted.
fn prepare_ampr() {
    let Ok(rd) = std::fs::read_dir(cache::AMPR_DIR) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if !path.is_file() {
            continue;
        }
        if let Err(e) = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o777)) {
            log!("ampr: chmod 0777 {}: {e}", path.display());
        }
        if e.file_name().eq_ignore_ascii_case("libkernel.sprx") {
            log!("ampr: WARNING {} overrides the system libkernel in every game; remove it unless intended", path.display());
        }
    }
}

/// A scan pass that takes at least this long is logged with its timing.
const SLOW_PASS_MS: u128 = 1000;

/// ShadowMountPlus's process name (SMP main.c PAYLOAD_NAME): it registers and mounts the same titles.
const SMP_PROCESS: &str = "shadowmountplus.elf";

/// ShadowMount+ not running: polls every 2 s, one toast when it starts waiting. kstuff is not waited
/// for: nothing here needs it, and some kstuff builds do not answer its probe (launch.rs judges kstuff
/// by whether a fake-signed game starts). The probe's answer is logged once.
fn wait_ready(state: &State) {
    match sys::kstuff_probe() {
        (0, false) => log!("kstuff probe: answered"),
        (rax, carry) => log!("kstuff probe: no answer (rax={rax:#x}, carry={carry})"),
    }
    let s = state.settings.lock().unwrap().clone();
    let mut waiting = false;
    loop {
        let why = launch::find_pid(SMP_PROCESS).is_some().then_some("shadowmount");
        *state.waiting.lock().unwrap() = why;
        match why {
            None => {
                if waiting {
                    log!("shadowmount: ready; continuing start-up");
                }
                return;
            }
            Some(w) if !waiting => {
                log!("waiting before start-up: {w}");
                toast(&s, i18n::smp_running(i18n::lang()));
                waiting = true;
            }
            Some(_) => {}
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// SMP auto_remove_missing_games timers (sm_game_cache.c), in memory only: a restart starts over.
#[derive(Default)]
struct MissingTimers {
    /// Title id -> second (since start) its source was first seen missing.
    since: HashMap<String, u64>,
    /// Removed from the home screen in this run while the source stays missing.
    removed: HashSet<String>,
    /// Skipped as a real install (register::is_ours), logged once per run.
    skipped: HashSet<String>,
}

impl MissingTimers {
    /// `missing`: this pass's unprotected titles with a gone source. Any other title's timer
    /// resets (source back, DLC, running). Returns the titles missing for `delay` s, not yet removed.
    fn due(&mut self, missing: &HashSet<String>, now: u64, delay: u64) -> Vec<String> {
        self.since.retain(|t, _| missing.contains(t));
        self.removed.retain(|t| missing.contains(t));
        let mut due = Vec::new();
        for t in missing {
            let since = *self.since.entry(t.clone()).or_insert(now);
            if !self.removed.contains(t) && now.saturating_sub(since) >= delay {
                due.push(t.clone());
            }
        }
        due.sort();
        due
    }
}

/// ENOENT/ENOTDIR only: any other stat error (a drive that answers badly) is not "gone".
fn gone(path: &std::path::Path) -> bool {
    matches!(std::fs::symlink_metadata(path),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)))
}

/// Built caches (a `.done` stamp) whose stamp source is gone, as title id -> source. Not a title
/// this scan lists (moved, or still being copied), a running one, or one with DLC in
/// /user/addcont/<TID> (SMP keeps those; a stat error there protects too).
fn missing_sources(r: &scan::ScanResult, running: &HashSet<String>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for e in std::fs::read_dir(cache::CACHES).into_iter().flatten().flatten() {
        let tid = e.file_name().to_string_lossy().into_owned();
        let Ok(raw) = std::fs::read(e.path().join(cache::DONE)) else { continue };
        let Ok(stamp) = serde_json::from_slice::<serde_json::Value>(&raw) else { continue };
        let Some(source) = stamp["source"].as_str() else { continue };
        if !auto_removable(&stamp)
            || running.contains(&tid)
            || r.games.iter().any(|g| g.title_id == tid)
            || r.unstable.iter().any(|(p, t)| t.as_ref() == Some(&tid) || p.as_os_str() == source)
            || !gone(source.as_ref())
            || !gone(format!("/user/addcont/{tid}").as_ref())
        {
            continue;
        }
        out.insert(tid, source.to_string());
    }
    out
}

/// An installed title's cache (stamp kind "installed") is never auto-removed: we did not register
/// it, and uninstalling would remove the real install.
fn auto_removable(stamp: &serde_json::Value) -> bool {
    stamp["kind"] != "installed"
}

/// One auto-remove step after a full scan pass: start/reset timers, then unregister the due
/// titles as /api/delete does (unmount without force, uninstall); the cache stays for a return.
fn auto_remove(state: &State, s: &Settings, r: &scan::ScanResult, timers: &mut MissingTimers, now: u64) {
    if !s.auto_remove_missing {
        *timers = MissingTimers { skipped: std::mem::take(&mut timers.skipped), ..Default::default() };
        return;
    }
    let running = state.running.lock().unwrap().clone();
    let missing = missing_sources(r, &running);
    let keys: HashSet<String> = missing.keys().cloned().collect();
    for tid in timers.due(&keys, now, s.auto_remove_delay_secs) {
        // A game may have started since `running` was read: stop, the next full pass retries.
        if state.paused() {
            return;
        }
        let source = &missing[&tid];
        // Never uninstall a real installed game that shares the title id; rechecked every pass. A console
        // install run through ng (scan::installed_pkg) has a pkg stamp whose source is under /user/app.
        let console = std::path::Path::new(source.as_str()).starts_with("/user/app");
        if console {
            // Uninstalled from the console's menu: the console's entry is gone; only our layer can be left.
            if let Err(e) = register::drop_cache_layer(&tid) {
                log!("{tid}: console install {source} gone: unmount our layer: {e}; retried next pass");
            } else if timers.skipped.insert(tid.clone()) {
                log!("{tid}: console install {source} gone; nothing of ours left mounted");
            }
            continue;
        }
        if let Err(p) = register::is_ours(&tid) {
            if timers.skipped.insert(tid.clone()) {
                log!("{tid}: auto-remove skipped: an installed copy exists ({})", p.display());
            }
            continue;
        }
        if let Err(e) = register::unmount_title(&tid) {
            // Retried next pass: a busy mount means the title is in use.
            log!("{tid}: auto-remove: unmount ({source}): {e}");
            continue;
        }
        // ponytail: a failed uninstall is logged once and not retried until the source returns or a restart.
        timers.removed.insert(tid.clone());
        match register::uninstall(&tid) {
            Ok(()) => {
                register::forget(&tid);
                let n = now.saturating_sub(timers.since[&tid]);
                log!("{tid}: source {source} missing for {n} s, removed from the home screen");
                state.titles.lock().unwrap().remove(&tid);
            }
            Err(e) => log!("{tid}: auto-remove: uninstall ({source}): {e}"),
        }
    }
}

fn main() {
    // ponytail: umask 0 for the whole payload, so every dir/file it creates is 0777/0666 (as SMP,
    // which mkdirs 0777) whatever elfldr passes: the game and the system services (save data,
    // trophies) read them with another uid. Code that wants a narrower mode sets it explicitly.
    unsafe { libc::umask(0) };
    // panic=abort: record the panic in kraken.log before the payload dies.
    std::panic::set_hook(Box::new(|info| sys::log_line(&format!("PANIC: {info}"))));
    for dir in [DATA_DIR, cache::CACHES, cache::AMPR_DIR] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            sys::notify(&i18n::cannot_create(i18n::lang(), dir, &e.to_string()));
            return;
        }
    }
    let s = config::load();
    sys::LOG_FILE.store(s.log, Ordering::Relaxed);
    let Some(listener) = web::take_port() else {
        toast(&s, &i18n::port_busy(i18n::lang(), web::PORT));
        return;
    };
    toast(&s, i18n::started(i18n::lang()));
    install_bundled_ampr();
    prepare_ampr();
    std::thread::spawn(save_self_copy);
    update::spawn_check();
    // ponytail: debug turned on later takes effect at the next start.
    if s.debug {
        std::thread::spawn(klog_capture);
    }
    let state = Arc::new(State::new(s));
    install::init(state.clone());
    let st = state.settings.lock().unwrap().clone();
    // Not gated on inject_ampr: pkg games get the ampr dir regardless.
    if st.ampr_auto_update {
        let state = state.clone();
        std::thread::spawn(move || ampr_update(&state, false, 3));
    }
    let web_state = state.clone();
    std::thread::spawn(move || web::serve(web_state, listener));
    let smp_state = state.clone();
    std::thread::spawn(move || web::serve_smp(smp_state));
    // The web UI is up; nothing mounts until ShadowMount+ is gone.
    wait_ready(&state);
    let (titles, running) = (state.titles.clone(), state.running.clone());
    // Built caches are launchable before the first scan ends: watch them from the start.
    for e in std::fs::read_dir(cache::CACHES).into_iter().flatten().flatten() {
        if e.path().join(cache::DONE).is_file() {
            titles.lock().unwrap().insert(e.file_name().to_string_lossy().into_owned());
        }
    }
    std::thread::spawn(move || {
        let mut wait = 1;
        loop {
            let t0 = std::time::Instant::now();
            launch::watch(titles.clone(), running.clone());
            // A watcher that fails again at once backs off up to a minute instead of filling the log.
            wait = if t0.elapsed().as_secs() < 5 { (wait * 2).min(60) } else { 2 };
            std::thread::sleep(std::time::Duration::from_secs(wait));
        }
    });
    let (mut registered, mut failed) = (HashSet::new(), HashMap::new());
    // Auto-remove timers start only after the first full pass: a drive not mounted yet at start
    // looks like a missing source until then (the delay covers the rest).
    let (mut timers, mut full_pass, t0) = (MissingTimers::default(), false, std::time::Instant::now());
    loop {
        // A game runs: nothing that could touch its app0 or /system_ex mount. Rescan, rebuild
        // and purge requests stay queued (forced / purge) for after it exits.
        // ponytail: polls every 2 s instead of a wake from the watcher.
        if state.paused() {
            state.busy.store(false, Ordering::SeqCst);
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        // Held to the end of the pass (dropped at `continue` and before the wait): /api/delete waits on it.
        let work = state.work.lock().unwrap();
        // A self-update is under way: the new copy quits this one soon, not in the middle of a build.
        if update::holding() {
            drop(work);
            state.busy.store(false, Ordering::SeqCst);
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        let pass = std::time::Instant::now();
        // Loaded under the settings lock: web.rs change_settings (an exclusion) loads, saves and
        // applies under it too, so this pass never puts an older copy back over a newer one.
        let s = {
            let mut cur = state.settings.lock().unwrap();
            *cur = config::load();
            cur.clone()
        };
        sys::LOG_FILE.store(s.log, Ordering::Relaxed);
        state.busy.store(true, Ordering::SeqCst);
        let roots = if s.scan_paths.is_empty() { storage::default_scan_roots() } else { s.scan_paths.clone() };
        let r = scan::scan(&roots, &s.exclude, s.backports, s.stability_wait_secs, install::busy_tid().as_deref(), &|t| s.ng_installed(t));
        let scan_ms = pass.elapsed().as_millis();
        if state.paused() {
            continue;
        }
        // Purge after the scan, so `keep` is this scan's library and every kept title rebuilds below.
        // purge() empties kept caches too, and a source still being copied is not rebuilt this pass:
        // the request waits until no source is in flight.
        static PURGE_WAIT_LOGGED: AtomicBool = AtomicBool::new(false);
        if !r.unstable.is_empty() && state.purge.load(Ordering::SeqCst) {
            if !PURGE_WAIT_LOGGED.swap(true, Ordering::SeqCst) {
                log!("purge waits: {} source(s) still being copied", r.unstable.len());
            }
        } else if state.purge.swap(false, Ordering::SeqCst) {
            PURGE_WAIT_LOGGED.store(false, Ordering::SeqCst);
            let keep: Vec<String> = r.games.iter().map(|g| g.title_id.clone()).collect();
            match cache::purge(&keep) {
                Ok(()) => log!("caches purged; {} kept for rebuild", keep.len()),
                Err(e) => log!("purge {}: {e}", cache::CACHES),
            }
            state.forced.lock().unwrap().extend(keep);
        }
        publish(&state, &r);
        // A source that left the scan (USB unplugged) goes through register::mount again when back.
        // A source still being copied stays registered (skipped this pass, not gone).
        registered.retain(|t| r.games.iter().any(|g| &g.title_id == t)
            || r.unstable.iter().any(|(_, u)| u.as_ref() == Some(t)));
        // Built caches are overlaid at launch from the start, not only once the scan reaches them.
        state.titles.lock().unwrap().extend(
            r.games.iter().filter(|g| cache::built_at(&g.title_id).is_some()).map(|g| g.title_id.clone()),
        );
        check_space(&state, &s);
        let (mut full, mut staged) = (true, Vec::new());
        for src in &r.games {
            if state.paused() {
                log!("a game started; scan paused until it exits");
                full = false;
                break;
            }
            // Excluded since this pass loaded the settings (/api/v1/manual/remove, uninstall).
            // ponytail: a title staged before the change is still installed by install_staged below.
            if scan::excluded(&src.path, &state.settings.lock().unwrap().exclude) {
                continue;
            }
            process(&state, &s, src, &mut registered, &mut failed, &mut staged);
        }
        // Not once a game has started (nothing registers while a game runs): the staged titles are
        // not in `registered`, so the pass after the game exits stages them again and installs them.
        // ponytail: a title already on the home screen (a rebuild) is only remounted then; its
        // app.db row keeps the old version and name until its next full register.
        if !state.paused() {
            install_staged(&state, &s, &mut staged, &mut registered, &mut failed);
            register::sync_snd0info_if_changed(&registered);
        }
        if full_pass && full && !state.paused() {
            auto_remove(&state, &s, &r, &mut timers, t0.elapsed().as_secs());
        }
        full_pass |= full;
        *state.cache_total.lock().unwrap() = cache::total_size();
        check_space(&state, &s);
        // A quick pass every scan_interval would flood the log: only a slow one is logged.
        let pass_ms = pass.elapsed().as_millis();
        if pass_ms >= SLOW_PASS_MS {
            log!("scan pass: {} games in {pass_ms} ms (scan {scan_ms} ms)", r.games.len());
        }
        state.busy.store(false, Ordering::SeqCst);
        drop(work);
        state.wait(Duration::from_secs(s.scan_interval_secs));
    }
}
