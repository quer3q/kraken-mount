//! Kraken Mount (kraken-mount): finds FPKG .pkg files and game folders on the internal SSD and USB/extended
//! drives, keeps a per-game cache in /data/krakenmount/caches/<TID>, registers each game on the
//! home screen and overlays its fakelib at launch (DESIGN.md). Web UI on port web::PORT.

// ponytail: ampr_index keeps two unused format constants (FILE_STREAMING, CHUNK_STREAMING).
#![allow(dead_code)]

#[macro_use]
mod sys;
mod ampr_index;
mod cache;
mod config;
mod launch;
mod register;
mod scan;
mod storage;
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
/// backport, ampr dir) appearing, vanishing or changing mtime.
fn inputs_key(src: &Source, s: &Settings) -> String {
    let mut k = cache::stamp(src, s).to_string();
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
        toast(s, &format!("Kraken Mount: /data has only {} GB free", free >> 30));
    }
}

fn backport_of(src: &Source) -> Option<&std::path::Path> {
    match src.kind {
        Kind::Folder(_) => src.backport.as_deref(),
        Kind::Pkg => None,
    }
}

/// Builds when stale or forced, then registers. Ok(true) when the title was (re)registered.
fn build_and_register(state: &State, s: &Settings, src: &Source, fresh: &Fresh, forced: bool) -> std::io::Result<bool> {
    let tid = &src.title_id;
    if let (false, Fresh::Fakelib(why)) = (forced, fresh) {
        // Only fakelib/ is behind: recompose it, then go on as a fresh cache (mount or register).
        log!("{tid}: {why}; refreshing fakelib only");
        cache::refresh_fakelib(src, s)?;
    }
    let stale = forced || !matches!(fresh, Fresh::Fresh | Fresh::Fakelib(_));
    let app0 = cache::app0(src);
    if !stale && register::is_registered(tid) {
        // ponytail: registered by an earlier run; only the nullfs is redone (gone after a
        // reboot; a no-op when mounted): no sce_sys copy, no install call.
        register::mount(tid, &app0, backport_of(src))?;
        return Ok(false);
    }
    if stale {
        match fresh {
            Fresh::Stale(why) => log!("{tid}: cache stale ({why}); rebuilding"),
            Fresh::Missing => log!("{tid}: no cache; building"),
            Fresh::Fresh | Fresh::Fakelib(_) => log!("{tid}: rebuild forced"),
        }
        state.update(&src.path, |g| g.status = "building");
        check_space(state, s);
        let r = cache::build(src, s);
        check_space(state, s);
        r?;
    }
    register::register(tid, &app0, &cache::sce_sys(src), backport_of(src))?;
    Ok(true)
}

/// One game of the scan: check, build, register, status. A failed game is retried only when
/// its inputs_key changes or a rebuild is forced.
fn process(state: &State, s: &Settings, src: &Source, registered: &mut HashSet<String>, failed: &mut HashMap<PathBuf, String>) {
    let tid = &src.title_id;
    let forced = state.forced.lock().unwrap().remove(tid);
    let fresh = cache::check(src, s);
    let r = if !forced && failed.get(&src.path) == Some(&inputs_key(src, s)) {
        state.update(&src.path, |g| g.status = "error");
        return;
    } else if !forced && matches!(fresh, Fresh::Fresh) && registered.contains(tid) {
        // Each scan: redo /system_ex/app/<TID> when it is gone or stale (USB replug, rest
        // mode); a no-op when app0 is mounted there. Status stays "ready" (no UI flicker).
        match register::mount(tid, &cache::app0(src), backport_of(src)) {
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
    let (size, built) = (cache::size(tid), cache::built_at(tid));
    match r {
        Ok(new) => {
            failed.remove(&src.path);
            registered.insert(tid.clone());
            state.titles.lock().unwrap().insert(tid.clone());
            state.update(&src.path, |g| {
                (g.status, g.error, g.cache_size, g.cache_built) = ("ready", None, size, built);
            });
            if new {
                toast(s, &format!("{} registered", src.title_name));
            }
        }
        Err(e) => {
            let e = format!("{tid} ({}): {e}", src.path.display());
            log!("{e}");
            failed.insert(src.path.clone(), inputs_key(src, s));
            state.update(&src.path, |g| {
                (g.status, g.error, g.cache_size, g.cache_built) = ("error", Some(e.clone()), size, built);
            });
            toast(s, &format!("{}: failed, see the log", src.title_name));
        }
    }
}

/// Replaces the UI's game list with the scan: an entry keeps its status while its source
/// stays listed, new ones are "pending", duplicates "duplicate".
fn publish(state: &State, r: &scan::ScanResult) {
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
/// "<len> <crc32>" of the bundled copy last written. Outside AMPR_DIR: every file there goes into fakelib/.
const BUNDLED_MARK: &str = "/data/krakenmount/ampr.bundled";

/// Puts the bundled libSceAmpr.sprx into AMPR_DIR when it is missing or is a bundled copy we wrote
/// before (an older release). A file put there by hand (a debug build, say) is kept.
fn install_bundled_ampr() {
    let path = format!("{}/libSceAmpr.sprx", cache::AMPR_DIR);
    let mark = |b: &[u8]| format!("{} {:08x}", b.len(), ampr_index::crc32(&[b]));
    let write_mark = || {
        if let Err(e) = std::fs::write(BUNDLED_MARK, mark(BUNDLED_AMPR)) {
            log!("ampr: write {BUNDLED_MARK}: {e}");
        }
    };
    match std::fs::read(&path) {
        Ok(cur) if cur == BUNDLED_AMPR => {
            if std::fs::read_to_string(BUNDLED_MARK).ok() != Some(mark(BUNDLED_AMPR)) {
                write_mark();
            }
            return;
        }
        Ok(cur) if std::fs::read_to_string(BUNDLED_MARK).is_ok_and(|m| m == mark(&cur)) => {}
        Ok(_) => {
            log!("ampr: {path} is not a bundled ampr-emu-ng: kept (delete it to get {BUNDLED_AMPR_VERSION})");
            return;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            log!("ampr: read {path}: {e}");
            return;
        }
    }
    // Temp file beside AMPR_DIR, not in it, so a scan never copies a half-written one.
    let tmp = format!("{DATA_DIR}/libSceAmpr.sprx.tmp");
    let r = std::fs::File::create(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, BUNDLED_AMPR).and_then(|()| f.sync_all()))
        .and_then(|()| std::fs::rename(&tmp, &path));
    match r {
        Ok(()) => {
            write_mark();
            log!("ampr: installed bundled ampr-emu-ng {BUNDLED_AMPR_VERSION} ({} bytes) to {path}", BUNDLED_AMPR.len());
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            log!("ampr: install bundled ampr-emu-ng to {path}: {e}");
        }
    }
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

fn main() {
    // panic=abort: record the panic in kraken.log before the payload dies.
    std::panic::set_hook(Box::new(|info| sys::log_line(&format!("PANIC: {info}"))));
    for dir in [DATA_DIR, cache::CACHES, cache::AMPR_DIR] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            sys::notify(&format!("Kraken Mount: cannot create {dir}: {e}"));
            return;
        }
    }
    let s = config::load();
    sys::LOG_FILE.store(s.log, Ordering::Relaxed);
    let Some(listener) = web::take_port() else {
        toast(&s, &format!("Kraken Mount: port {} busy (an older copy that has a game running, or another app?); not started", web::PORT));
        return;
    };
    toast(&s, "Kraken Mount started");
    install_bundled_ampr();
    prepare_ampr();
    std::thread::spawn(save_self_copy);
    // ponytail: debug turned on later takes effect at the next start.
    if s.debug {
        std::thread::spawn(klog_capture);
    }
    let state = Arc::new(State::new(s));
    let web_state = state.clone();
    std::thread::spawn(move || web::serve(web_state, listener));
    let (titles, running) = (state.titles.clone(), state.running.clone());
    // Built caches are launchable before the first scan ends: watch them from the start.
    for e in std::fs::read_dir(cache::CACHES).into_iter().flatten().flatten() {
        if e.path().join(cache::DONE).is_file() {
            titles.lock().unwrap().insert(e.file_name().to_string_lossy().into_owned());
        }
    }
    std::thread::spawn(move || launch::watch(titles, running));
    let (mut registered, mut failed) = (HashSet::new(), HashMap::new());
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
        let s = config::load();
        sys::LOG_FILE.store(s.log, Ordering::Relaxed);
        *state.settings.lock().unwrap() = s.clone();
        state.busy.store(true, Ordering::SeqCst);
        let roots = if s.scan_paths.is_empty() { storage::default_scan_roots() } else { s.scan_paths.clone() };
        let r = scan::scan(&roots, s.backports);
        if state.paused() {
            continue;
        }
        // Purge after the scan, so `keep` is this scan's library and every kept title rebuilds below.
        if state.purge.swap(false, Ordering::SeqCst) {
            let keep: Vec<String> = r.games.iter().map(|g| g.title_id.clone()).collect();
            match cache::purge(&keep) {
                Ok(()) => log!("caches purged; {} kept for rebuild", keep.len()),
                Err(e) => log!("purge {}: {e}", cache::CACHES),
            }
            state.forced.lock().unwrap().extend(keep);
        }
        publish(&state, &r);
        // A source that left the scan (USB unplugged) goes through register::mount again when back.
        registered.retain(|t| r.games.iter().any(|g| &g.title_id == t));
        // Built caches are overlaid at launch from the start, not only once the scan reaches them.
        state.titles.lock().unwrap().extend(
            r.games.iter().filter(|g| cache::built_at(&g.title_id).is_some()).map(|g| g.title_id.clone()),
        );
        check_space(&state, &s);
        for src in &r.games {
            if state.paused() {
                log!("a game started; scan paused until it exits");
                break;
            }
            process(&state, &s, src, &mut registered, &mut failed);
        }
        *state.cache_total.lock().unwrap() = cache::total_size();
        check_space(&state, &s);
        state.busy.store(false, Ordering::SeqCst);
        drop(work);
        state.wait(Duration::from_secs(s.scan_interval_secs));
    }
}
