//! Launch watcher: puts /data/krakenmount/caches/<TID>/fakelib (our ng libSceAmpr.sprx) on top of the
//! game sandbox's system library dir /mnt/sandbox/<TID>_<n>/<x>/common/lib with a unionfs, so the
//! game loads ng instead of the system libSceAmpr (cache.rs copies /data/krakenmount/ampr into that
//! fakelib; there is no mount of the ampr dir). Port of ShadowMountPlus sm_fakelib.c (sandbox
//! lookup, unionfs layer, cleanup) and sm_game_lifecycle.c (SceSysCore kqueue), without the
//! ShellCore hooks. Order: /mnt/sandbox vnode event (before spawn), NOTE_EXEC fallback, then
//! unmount on NOTE_EXIT or when the sandbox dir goes. See DESIGN.md "Launch: fakelib overlay".
use crate::sys;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const SANDBOX_ROOT: &str = "/mnt/sandbox";
/// Fast path: how long to wait for ShellCore's common/lib mount after the sandbox dir appears.
const READY_WAIT_MS: u64 = 3000;
/// SMP GAME_LIFECYCLE_POLL_INTERVAL_US: busy unmount retry interval.
const RETRY_MS: u64 = 250;
/// SMP GAME_SANDBOX_CLEANUP_TIMEOUT_US: busy unmount retries stop here; then one MNT_FORCE.
const CLEANUP_TIMEOUT_MS: u64 = 10_000;
/// SMP: unresolved exec pids are kept up to 32 and repolled for up to 11 s.
const PENDING_MAX: usize = 32;
const PENDING_TIMEOUT_MS: u64 = 11_000;
/// Repoll interval for exec pids without a title id yet (SMP: 250 ms; shorter here, the race).
const PENDING_POLL_MS: u64 = 10;
/// SMP main.c KINFO_PID_OFFSET / KINFO_TDNAME_OFFSET in the PS5 kinfo_proc.
const KINFO_PID_OFFSET: usize = 72;
const KINFO_TDNAME_OFFSET: usize = 447;

/// SMP sm_platform.h app_info_t.
#[repr(C)]
struct AppInfo {
    app_id: u32,
    unknown1: u64,
    title_id: [u8; 14],
    unknown2: [u8; 0x3c],
}

extern "C" {
    fn sceKernelGetAppInfo(pid: libc::pid_t, info: *mut AppInfo) -> i32;
}

/// Monotonic milliseconds (CLOCK_MONOTONIC, since boot): the timestamp on every log line here.
fn now_ms() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

macro_rules! tlog {
    ($($arg:tt)*) => {
        log!("[launch {}] {}", now_ms(), format!($($arg)*))
    };
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn c_field(buf: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = buf.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The mount on top at `path` as statfs reports it.
struct Top {
    on: String,
    fstype: String,
    from: String,
    fsid: libc::fsid_t,
    rdonly: bool,
}

fn top_mount(path: &str) -> io::Result<Top> {
    let p = CString::new(path).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(p.as_ptr(), &mut fs) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(top_of(&fs))
}

fn top_of(fs: &libc::statfs) -> Top {
    Top {
        on: c_field(&fs.f_mntonname),
        fstype: c_field(&fs.f_fstypename),
        from: c_field(&fs.f_mntfromname),
        fsid: fs.f_fsid,
        rdonly: fs.f_flags & libc::MNT_RDONLY as u64 != 0,
    }
}

/// Every mount, oldest first (getmntinfo walks the mount list in mount order).
fn all_mounts() -> Vec<Top> {
    let mut p: *mut libc::statfs = std::ptr::null_mut();
    let n = unsafe { libc::getmntinfo(&mut p, libc::MNT_NOWAIT) };
    if n <= 0 || p.is_null() {
        tlog!("getmntinfo failed: {}", io::Error::last_os_error());
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(p, n as usize) }.iter().map(top_of).collect()
}

/// SMP sandbox_mount_matches(overlay = true): a unionfs whose source (minus "<above>:") is `src`.
fn is_our_overlay(t: &Top, src: &str) -> bool {
    t.fstype == "unionfs" && t.from.strip_prefix("<above>:").unwrap_or(&t.from) == src
}

fn same_fsid(a: &libc::fsid_t, b: &libc::fsid_t) -> bool {
    unsafe {
        std::slice::from_raw_parts(a as *const _ as *const u8, std::mem::size_of::<libc::fsid_t>())
            == std::slice::from_raw_parts(b as *const _ as *const u8, std::mem::size_of::<libc::fsid_t>())
    }
}

/// SMP find_pid_by_name.
fn find_pid(name: &str) -> Option<libc::pid_t> {
    proc_find(|_, td| td == name.as_bytes())
}

/// The first process whose app info title id is `tid` (a game started before this watcher).
fn find_title_pid(tid: &str) -> Option<libc::pid_t> {
    proc_find(|pid, _| title_of(pid).as_deref() == Some(tid))
}

/// Walks sysctl KERN_PROC_PROC with the PS5 kinfo_proc offsets: first pid where `f(pid, tdname)`.
fn proc_find(mut f: impl FnMut(libc::pid_t, &[u8]) -> bool) -> Option<libc::pid_t> {
    let mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PROC, 0];
    let mut len = 0usize;
    let null = std::ptr::null_mut();
    if unsafe { libc::sysctl(mib.as_ptr(), 4, null, &mut len, std::ptr::null(), 0) } != 0 {
        return None;
    }
    let mut buf = vec![0u8; len + len / 4];
    len = buf.len();
    let out = buf.as_mut_ptr() as *mut libc::c_void;
    if unsafe { libc::sysctl(mib.as_ptr(), 4, out, &mut len, std::ptr::null(), 0) } != 0 {
        return None;
    }
    let buf = &buf[..len];
    let mut off = 0;
    while buf.len() - off >= 4 {
        let size = i32::from_ne_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        if size <= KINFO_TDNAME_OFFSET || size > buf.len() - off {
            return None;
        }
        let ki = &buf[off..off + size];
        let pid = i32::from_ne_bytes(ki[KINFO_PID_OFFSET..KINFO_PID_OFFSET + 4].try_into().unwrap());
        let td = &ki[KINFO_TDNAME_OFFSET..];
        if let Some(n) = td.iter().position(|&b| b == 0) {
            if f(pid, &td[..n]) {
                return Some(pid);
            }
        }
        off += size;
    }
    None
}

/// SMP resolve_game_title_id: sceKernelGetAppInfo(pid).title_id, None while it is empty.
fn title_of(pid: libc::pid_t) -> Option<String> {
    let mut info: AppInfo = unsafe { std::mem::zeroed() };
    if unsafe { sceKernelGetAppInfo(pid, &mut info) } != 0 {
        return None;
    }
    let n = info.title_id.iter().position(|&b| b == 0).unwrap_or(info.title_id.len());
    if n == 0 || n >= info.title_id.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&info.title_id[..n]).into_owned())
}

/// "<TID>_<digits>" -> TID (SMP resolve_sandbox_paths name filter).
fn sandbox_tid(name: &str) -> Option<&str> {
    let (tid, n) = name.rsplit_once('_')?;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then_some(tid)
}

/// sandbox_tid, only for a TID in `titles`.
fn sandbox_title(name: &str, titles: &HashSet<String>) -> Option<String> {
    sandbox_tid(name).filter(|tid| titles.contains(*tid)).map(str::to_string)
}

/// The cache's fakelib/ exists and is not empty; else there is nothing to overlay.
fn has_files(dir: &str) -> bool {
    std::fs::read_dir(dir).map(|mut d| d.next().is_some()).unwrap_or(false)
}

fn is_dir(path: &str) -> bool {
    std::fs::metadata(path).map(|m| m.is_dir()).unwrap_or(false)
}

fn list(dir: &str) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let name = e?.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') {
            names.push(name);
        }
    }
    Ok(names)
}

/// SMP resolve_sandbox_paths, second half: the first <x> (not app0) of the sandbox with a
/// <x>/common/lib directory.
fn find_lib(sandbox: &str) -> Option<String> {
    let root = format!("{SANDBOX_ROOT}/{sandbox}");
    list(&root).ok()?.into_iter().filter(|x| x != "app0").find_map(|x| {
        let lib = format!("{root}/{x}/common/lib");
        is_dir(&lib).then_some(lib)
    })
}

/// SMP resolve_sandbox_paths, first half: the one live "<tid>_<n>" sandbox with an app0 dir.
/// Two live sandboxes are EBUSY (SMP: "need an explicit owner").
fn resolve_sandbox(tid: &str) -> io::Result<String> {
    let mut found = None;
    for name in list(SANDBOX_ROOT)? {
        let Some(n) = name.strip_prefix(tid).and_then(|r| r.strip_prefix('_')) else { continue };
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if !is_dir(&format!("{SANDBOX_ROOT}/{name}/app0")) {
            continue;
        }
        if found.is_some() {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        found = Some(name);
    }
    found.ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
}

/// Our layer on common/lib: the cache's fakelib (the game's libs with ampr copied on top, cache.rs).
fn our_srcs(tid: &str) -> [String; 1] {
    [crate::cache::dir(tid).join("fakelib").to_string_lossy().into_owned()]
}

/// One launch of one of our titles: its sandbox, our unionfs layers on its common/lib, the game pid.
struct Session {
    tid: String,
    lib: Option<String>,
    /// The layers this launch wants on common/lib, bottom first (subset of our_srcs, may be empty).
    srcs: Vec<String>,
    /// fsids of the unionfs mounts we made, oldest first.
    layers: Vec<libc::fsid_t>,
    /// Our nullfs mounts of /data and the USB/ext drives inside the sandbox (path, fsid).
    data: Vec<(String, libc::fsid_t)>,
    pid: Option<libc::pid_t>,
    /// (deadline, next try) of a busy cleanup, in now_ms.
    cleanup: Option<(u64, u64)>,
}

impl Session {
    /// The one layer: the cache fakelib when non-empty (ampr is copied into it, no mount of its own).
    fn new(tid: &str) -> Session {
        let [fakelib] = our_srcs(tid);
        let mut srcs = Vec::new();
        if has_files(&fakelib) {
            srcs.push(fakelib);
        }
        Session { tid: tid.to_string(), lib: None, srcs, layers: Vec::new(), data: Vec::new(), pid: None, cleanup: None }
    }

    /// Index in `srcs` of our overlay on top of common/lib now, if one is.
    fn top_layer(&self) -> Option<usize> {
        let lib = self.lib.as_ref()?;
        let t = top_mount(lib).ok().filter(|t| t.on == *lib)?;
        self.srcs.iter().position(|s| is_our_overlay(&t, s))
    }

    /// Mounts srcs[start..] in order (newest on top), then the sandbox /data and USB nullfs.
    fn mount(&mut self, start: usize, why: &str) -> bool {
        let Some(lib) = self.lib.clone() else { return false };
        let mut any = false;
        for src in self.srcs[start.min(self.srcs.len())..].to_vec() {
            // ponytail: a failed layer does not stop the next one; each is useful alone.
            any |= self.mount_layer(&lib, &src, why);
        }
        if any {
            self.mount_data(&lib);
        }
        any
    }

    /// SMP mount_sandbox_layer(overlay) with SMP's exact options and flags 0, then the
    /// track_sandbox_layer identity check (top mount at lib must be a unionfs now).
    fn mount_layer(&mut self, lib: &str, src: &str, why: &str) -> bool {
        let t0 = now_ms();
        let opts = [
            ("fstype", Some("unionfs")),
            ("from", Some(src)),
            ("fspath", Some(lib)),
            ("copymode", Some("transparent")),
            ("notime", None),
            ("fnodup", None),
        ];
        if let Err(e) = sys::nmount_opt(&opts, 0) {
            tlog!("{} overlay mount FAILED ({why}) errno={} ({e}): {src} -> {lib}",
                self.tid, e.raw_os_error().unwrap_or(0));
            return false;
        }
        match top_mount(lib) {
            Ok(t) if t.on == lib && t.fstype == "unionfs" => {
                self.layers.push(t.fsid);
                tlog!("{} overlay MOUNTED ({why}, nmount {} ms): {src} -> {lib}", self.tid, now_ms() - t0);
                true
            }
            Ok(t) => {
                tlog!("{} overlay mounted but top at {lib} is {} on {}", self.tid, t.fstype, t.on);
                false
            }
            Err(e) => {
                tlog!("{} overlay mounted but statfs {lib} failed: {e}", self.tid);
                false
            }
        }
    }

    /// SMP mount_sandbox_directory(app0, "/data"): a nullfs of /data at <sandbox>/data, so the
    /// game (ng) can reach /data/homebrew/<pkg> and /data/ampr_*; without ShadowMountPlus the
    /// sandbox has no /data. The same for every mounted /mnt/usbN and /mnt/extN (a pkg or packs
    /// on USB). An existing nullfs of the same source there (/user/data for /data) is kept.
    fn mount_data(&mut self, lib: &str) {
        // lib = <sandbox>/<x>/common/lib
        let Some(root) = std::path::Path::new(lib).parent().and_then(|p| p.parent()).and_then(|p| p.parent())
        else {
            return;
        };
        for from in std::iter::once("/data".to_string()).chain(crate::storage::mounted_aliases()) {
            self.mount_dir(&format!("{}{from}", root.display()), &from, 0);
        }
        // Writable /app0 (the game's files: the dump folder, or the pkg cache), then the pkg's folder
        // at /app0/kraken (ng finds the .pkg there). Both on top of the system's /app0.
        let app0 = format!("{}/app0", root.display());
        if !self.mount_app0(&app0) {
            return;
        }
        if let Some(pkg) = crate::cache::pkg_source(&self.tid) {
            // Read-only: the game must never write into the folder holding its .pkg.
            // A single-file nullfs of the .pkg fails here with ENOTDIR.
            if let Some(pkg_dir) = std::path::Path::new(&pkg).parent().and_then(|p| p.to_str()) {
                self.mount_dir(&format!("{app0}/{}", crate::cache::PKG_MOUNT_DIR), pkg_dir, libc::MNT_RDONLY as i32);
            }
        }
    }

    /// A read-write nullfs of /system_ex/app/<TID> (the registered app0 stack, backport unionfs
    /// included) over the sandbox's /app0, only when the system's one is read-only. Not yet mounted
    /// at the sandbox's first sight: the exec pass (mount_data again) gets it.
    /// ponytail: not adopted by a later copy of the payload (adopt_existing only knows /data and USB).
    /// False while the system's /app0 is not mounted (then /app0/kraken must wait too).
    fn mount_app0(&mut self, app0: &str) -> bool {
        let from = format!("/system_ex/app/{}", self.tid);
        match top_mount(app0) {
            Ok(t) if t.on == app0 && !t.rdonly => {
                tlog!("{} sandbox /app0 already writable ({} from {})", self.tid, t.fstype, t.from);
                return true;
            }
            Ok(t) if t.on == app0 => {}
            _ => {
                tlog!("{} sandbox /app0 not mounted yet: {app0}", self.tid);
                return false;
            }
        }
        let opts = [("fstype", Some("nullfs")), ("from", Some(from.as_str())), ("fspath", Some(app0))];
        if let Err(e) = sys::nmount_opt(&opts, 0) {
            // Still the system's /app0: /app0/kraken can go on it, the game just can't write.
            tlog!("{} sandbox /app0 rw mount FAILED errno={} ({e}): {from} -> {app0}", self.tid, e.raw_os_error().unwrap_or(0));
            return true;
        }
        match top_mount(app0) {
            Ok(t) if t.on == app0 && t.fstype == "nullfs" && !t.rdonly => {
                self.data.push((app0.to_string(), t.fsid));
                tlog!("{} sandbox /app0 MOUNTED writable: {from} -> {app0}", self.tid);
            }
            _ => tlog!("{} sandbox /app0 rw mount not visible at {app0}", self.tid),
        }
        true
    }

    fn mount_dir(&mut self, path: &str, from: &str, flags: i32) {
        if let Ok(t) = top_mount(path) {
            if t.on == path && t.fstype == "nullfs" && (t.from == from || (from == "/data" && t.from == "/user/data")) {
                tlog!("{} sandbox {from} already mounted from {}", self.tid, t.from);
                return;
            }
        }
        if let Err(e) = std::fs::create_dir_all(path) {
            tlog!("{} sandbox {from} mkdir {path} FAILED: {e}", self.tid);
            return;
        }
        let opts = [("fstype", Some("nullfs")), ("from", Some(from)), ("fspath", Some(path))];
        if let Err(e) = sys::nmount_opt(&opts, flags) {
            tlog!("{} sandbox {from} mount FAILED errno={} ({e}): {path}", self.tid, e.raw_os_error().unwrap_or(0));
            return;
        }
        match top_mount(path) {
            Ok(t) if t.on == path && t.fstype == "nullfs" => {
                self.data.push((path.to_string(), t.fsid));
                tlog!("{} sandbox {from} MOUNTED: {from} -> {path}", self.tid);
            }
            _ => tlog!("{} sandbox {from} mounted but not visible at {path}", self.tid),
        }
    }

    /// Unmounts our sandbox nullfs layers, newest first. Errno of the first that stays (EBUSY), else None.
    fn unmount_data(&mut self, flags: i32) -> Option<i32> {
        let mut first_err = None;
        let mut kept = Vec::new();
        while let Some((path, fsid)) = self.data.pop() {
            let rc = match top_mount(&path) {
                Ok(t) if t.on == path && same_fsid(&t.fsid, &fsid) => sys::unmount(&path, flags),
                _ => unmount_fsid(&fsid, flags),
            };
            match rc {
                Ok(()) => tlog!("{} sandbox nullfs UNMOUNTED: {path}", self.tid),
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::EINVAL)) => {}
                Err(e) => {
                    first_err.get_or_insert(e.raw_os_error().unwrap_or(libc::EIO));
                    kept.push((path, fsid));
                }
            }
        }
        kept.reverse();
        self.data = kept;
        first_err
    }

    /// One cleanup pass over our layers, newest first (SMP cleanup_fakelib_mount +
    /// unmount_sandbox_layer). Our layer on top: unmount(lib); covered by a newer mount: never by
    /// path (SMP), by its fsid instead. ENOENT/EINVAL/gone count as done. Returns the errno of
    /// the first layer that stays (EBUSY retries), or None when all are gone.
    fn unmount_pass(&mut self, force: bool) -> Option<i32> {
        let Some(lib) = self.lib.clone() else {
            self.layers.clear();
            return None;
        };
        let flags = if force { libc::MNT_FORCE } else { 0 };
        let mut first_err = self.unmount_data(flags);
        while let Some(fsid) = self.layers.pop() {
            let rc = match top_mount(&lib) {
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) => {
                    // The sandbox is gone; only the fsid can still name a lingering layer.
                    unmount_fsid(&fsid, flags)
                }
                Err(e) => Err(e),
                Ok(t) if t.on == lib && same_fsid(&t.fsid, &fsid) => sys::unmount(&lib, flags),
                Ok(_) => unmount_fsid(&fsid, flags),
            };
            match rc {
                Ok(()) => tlog!("{} overlay UNMOUNTED{}: {lib}", self.tid, if force { " (MNT_FORCE)" } else { "" }),
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::EINVAL)) => {
                    tlog!("{} overlay already gone: {lib}", self.tid)
                }
                Err(e) => {
                    // Busy: keep it and every layer below it, so the order stays newest first.
                    first_err.get_or_insert(e.raw_os_error().unwrap_or(libc::EIO));
                    self.layers.push(fsid);
                    break;
                }
            }
        }
        first_err
    }
}

/// SMP unmount_unreachable_usb_layer: unmount("FSID:<a>:<b>", MNT_BYFSID); selects only that mount.
fn unmount_fsid(fsid: &libc::fsid_t, flags: i32) -> io::Result<()> {
    let ids: [i32; 2] = unsafe { std::mem::transmute_copy(fsid) };
    sys::unmount(&format!("FSID:{}:{}", ids[0], ids[1]), libc::MNT_BYFSID | flags)
}

fn kev(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
    let mut k: libc::kevent = unsafe { std::mem::zeroed() };
    k.ident = ident;
    k.filter = filter;
    k.flags = flags;
    k.fflags = fflags;
    k
}

fn kevent_add(kq: i32, k: libc::kevent) -> io::Result<()> {
    if unsafe { libc::kevent(kq, &k, 1, std::ptr::null_mut(), 0, std::ptr::null()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn alive(pid: libc::pid_t) -> bool {
    unsafe { libc::kill(pid, 0) == 0 || errno() == libc::EPERM }
}

struct Watcher {
    kq: i32,
    titles: Arc<Mutex<HashSet<String>>>,
    /// Title ids with a live session; the scan loop pauses while it is non-empty.
    running: Arc<Mutex<HashSet<String>>>,
    /// Entries of /mnt/sandbox at the last listing.
    known: HashSet<String>,
    /// Keyed by sandbox dir name ("PPSA00000_000").
    sessions: HashMap<String, Session>,
    /// SysCore descendants that exec'd without a title id yet: (pid, give-up time).
    pending: Vec<(libc::pid_t, u64)>,
}

/// Blocking; run on its own thread. Overlays the cache fakelib for launches of the titles in
/// `titles`; keeps `running` = the title ids with a live session.
pub fn watch(titles: Arc<Mutex<HashSet<String>>>, running: Arc<Mutex<HashSet<String>>>) {
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        tlog!("kqueue failed: {}", io::Error::last_os_error());
        return;
    }
    match find_pid("SceSysCore.elf") {
        Some(pid) => {
            let fl = (libc::NOTE_FORK | libc::NOTE_EXEC | libc::NOTE_TRACK) as u32;
            let k = kev(pid as usize, libc::EVFILT_PROC, libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR, fl);
            match kevent_add(kq, k) {
                Ok(()) => tlog!("watching SceSysCore pid={pid} (NOTE_FORK|NOTE_EXEC|NOTE_TRACK)"),
                Err(e) => tlog!("SceSysCore pid={pid} proc watch failed: {e}"),
            }
        }
        None => tlog!("SceSysCore.elf not found; no exec fallback"),
    }
    let root = CString::new(SANDBOX_ROOT).unwrap();
    let fd = unsafe { libc::open(root.as_ptr(), libc::O_RDONLY) };
    let fl = (libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE) as u32;
    let vnode_ok = fd >= 0
        && kevent_add(kq, kev(fd as usize, libc::EVFILT_VNODE, libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR, fl))
            .map_err(|e| tlog!("{SANDBOX_ROOT} vnode watch failed: {e}"))
            .is_ok();
    if fd < 0 {
        tlog!("open {SANDBOX_ROOT} failed: {}", io::Error::last_os_error());
    }
    tlog!("watching {SANDBOX_ROOT} ({})", if vnode_ok { "EVFILT_VNODE" } else { "polling every 250 ms" });
    let known = list(SANDBOX_ROOT).unwrap_or_default().into_iter().collect();
    let mut w = Watcher { kq, titles, running, known, sessions: HashMap::new(), pending: Vec::new() };
    w.adopt();
    w.poll_cleanups();
    w.publish();
    let mut events: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
    loop {
        let now = now_ms();
        let mut wait: Option<u64> = (!vnode_ok).then_some(RETRY_MS);
        if !w.pending.is_empty() {
            wait = Some(wait.map_or(PENDING_POLL_MS, |x| x.min(PENDING_POLL_MS)));
        }
        for s in w.sessions.values() {
            if let Some((_, next)) = s.cleanup {
                let d = next.saturating_sub(now);
                wait = Some(wait.map_or(d, |x| x.min(d)));
            }
        }
        let ts = wait.map(|ms| libc::timespec { tv_sec: (ms / 1000) as _, tv_nsec: ((ms % 1000) * 1_000_000) as _ });
        let tsp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const _);
        let n = unsafe { libc::kevent(kq, std::ptr::null(), 0, events.as_mut_ptr(), events.len() as i32, tsp) };
        if n < 0 {
            if errno() == libc::EINTR {
                continue;
            }
            tlog!("kevent wait failed: {}; launch watcher stopped", io::Error::last_os_error());
            return;
        }
        // The sandbox dir is the earlier signal: handle it before execs from the same batch.
        let events = &events[..n as usize];
        if !vnode_ok || events.iter().any(|e| e.filter == libc::EVFILT_VNODE) {
            w.on_sandbox_change();
        }
        for e in events {
            if e.filter == libc::EVFILT_PROC {
                let pid = e.ident as libc::pid_t;
                if e.fflags & libc::NOTE_TRACKERR as u32 != 0 {
                    tlog!("NOTE_TRACKERR pid={pid}");
                }
                if e.fflags & libc::NOTE_EXEC as u32 != 0 {
                    w.on_exec(pid);
                }
                if e.fflags & libc::NOTE_EXIT as u32 != 0 {
                    w.on_exit(pid);
                }
            }
        }
        w.poll_pending();
        w.poll_cleanups();
        w.publish();
    }
}

impl Watcher {
    fn titles(&self) -> HashSet<String> {
        self.titles.lock().unwrap().clone()
    }

    /// running = the title ids of the live sessions (a session ends after its cleanup).
    fn publish(&self) {
        let now: HashSet<String> = self.sessions.values().map(|s| s.tid.clone()).collect();
        let mut r = self.running.lock().unwrap();
        if *r != now {
            tlog!("running: {:?}", now);
            *r = now;
        }
    }

    /// At start: take over the sandboxes a previous copy left our layers in (it quit or crashed
    /// mid-game), so their NOTE_EXIT still unmounts them. Any TID: `titles` is empty this early.
    fn adopt(&mut self) {
        let names: Vec<String> = self.known.iter().cloned().collect();
        let mut all = None;
        for name in names {
            let Some(tid) = sandbox_tid(&name).map(str::to_string) else { continue };
            let Some(lib) = find_lib(&name) else { continue };
            let all = all.get_or_insert_with(all_mounts);
            let ours = our_srcs(&tid);
            let mut s = Session::new(&tid);
            s.srcs.clear();
            for m in all.iter().filter(|m| m.on == lib) {
                if let Some(src) = ours.iter().find(|x| is_our_overlay(m, x)) {
                    s.layers.push(m.fsid);
                    s.srcs.push(src.clone());
                }
            }
            if s.layers.is_empty() {
                continue;
            }
            // Ours only: from == the alias itself (ShellCore's own /data is from /user/data).
            let root = format!("{SANDBOX_ROOT}/{name}");
            for from in std::iter::once("/data".to_string()).chain(crate::storage::mounted_aliases()) {
                let path = format!("{root}{from}");
                for m in all.iter().filter(|m| m.on == path && m.fstype == "nullfs" && m.from == from) {
                    s.data.push((path.clone(), m.fsid));
                }
            }
            s.lib = Some(lib.clone());
            match find_title_pid(&tid) {
                Some(pid) => {
                    let k = kev(pid as usize, libc::EVFILT_PROC, libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR, libc::NOTE_EXIT as u32);
                    match kevent_add(self.kq, k) {
                        Ok(()) => s.pid = Some(pid),
                        Err(e) => {
                            tlog!("{tid} adopted exit watch pid={pid} failed: {e}; cleaning up");
                            s.cleanup = Some((now_ms() + CLEANUP_TIMEOUT_MS, 0));
                        }
                    }
                }
                None if std::path::Path::new(&root).is_dir() => {
                    // ponytail: the pid lookup may miss a game that is still starting; its layers stay
                    // until the sandbox dir goes (on_sandbox_change). A stale dir keeps the scan paused.
                    tlog!("{tid} adopted {name}: no game process found; keeping until the sandbox goes");
                }
                None => {
                    tlog!("{tid} adopted {name}: sandbox dir gone; cleaning up");
                    s.cleanup = Some((now_ms() + CLEANUP_TIMEOUT_MS, 0));
                }
            }
            tlog!("{tid} adopted {name}: {} layer(s), {} data mount(s) on {lib}, pid={:?}",
                s.layers.len(), s.data.len(), s.pid);
            self.sessions.insert(name, s);
        }
    }

    /// New "<TID>_<n>" of ours -> fast path; a vanished one -> unmount its overlay.
    fn on_sandbox_change(&mut self) {
        let Ok(names) = list(SANDBOX_ROOT) else { return };
        let names: HashSet<String> = names.into_iter().collect();
        let titles = self.titles();
        let new: Vec<String> = names.difference(&self.known).cloned().collect();
        let gone: Vec<String> = self.known.difference(&names).cloned().collect();
        self.known = names;
        for name in gone {
            if let Some(s) = self.sessions.get_mut(&name) {
                tlog!("{} sandbox gone: {name}", s.tid);
                s.cleanup.get_or_insert((now_ms() + CLEANUP_TIMEOUT_MS, 0));
            }
        }
        for name in new {
            if let Some(tid) = sandbox_title(&name, &titles) {
                self.fast_mount(&name, &tid);
            }
        }
    }

    /// Strategy 1: poll every 1 ms, up to 3 s, until <x>/common/lib exists and is a mount point
    /// (ShellCore's), then put our unionfs on it.
    fn fast_mount(&mut self, name: &str, tid: &str) {
        let t0 = now_ms();
        tlog!("{tid} sandbox seen: {SANDBOX_ROOT}/{name}");
        // Pause the scan now, not after the up-to-3 s common/lib wait (a queued purge could run in it).
        self.running.lock().unwrap().insert(tid.to_string());
        if let Some(mut old) = self.sessions.remove(name) {
            tlog!("{tid} stale session on {name}, unmounting it first");
            old.unmount_pass(false);
        }
        let mut s = Session::new(tid);
        if s.srcs.is_empty() {
            // Still a session: `running` must hold the title while its game runs.
            tlog!("{tid} empty fakelib, nothing to overlay; not mounting");
            self.sessions.insert(name.to_string(), s);
            return;
        }
        let root = format!("{SANDBOX_ROOT}/{name}");
        loop {
            let waited = now_ms() - t0;
            if let Some(lib) = s.lib.clone().or_else(|| find_lib(name)) {
                s.lib = Some(lib.clone());
                if let Ok(t) = top_mount(&lib) {
                    if t.on == lib {
                        match s.srcs.iter().position(|x| is_our_overlay(&t, x)) {
                            Some(i) if i + 1 == s.srcs.len() => tlog!("{tid} common/lib already has our overlay: {lib}"),
                            Some(i) => {
                                tlog!("{tid} common/lib has our layer {}; mounting the rest", s.srcs[i]);
                                s.mount(i + 1, "sandbox");
                            }
                            // SMP track_sandbox_layer: another unionfs owns it; leave it alone.
                            None if t.fstype == "unionfs" => {
                                tlog!("{tid} common/lib occupied by unionfs from {}; not mounting", t.from)
                            }
                            None => {
                                tlog!("{tid} common/lib ready after {waited} ms ({} from {}): {lib}", t.fstype, t.from);
                                s.mount(0, "sandbox");
                            }
                        }
                        break;
                    }
                }
            }
            if waited >= READY_WAIT_MS {
                tlog!("{tid} common/lib not a mount point after {waited} ms (lib={:?}); waiting for exec", s.lib);
                break;
            }
            if !is_dir(&root) {
                tlog!("{tid} sandbox vanished while waiting: {root}");
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        self.sessions.insert(name.to_string(), s);
    }

    fn on_exec(&mut self, pid: libc::pid_t) {
        if self.pending.iter().any(|p| p.0 == pid) {
            return;
        }
        match title_of(pid) {
            Some(tid) => {
                if self.titles().contains(&tid) {
                    self.attach(pid, &tid);
                }
            }
            None if alive(pid) && self.pending.len() < PENDING_MAX => {
                self.pending.push((pid, now_ms() + PENDING_TIMEOUT_MS));
            }
            None => {}
        }
    }

    /// SMP: exec pids whose title id was not known yet are repolled until they get one.
    fn poll_pending(&mut self) {
        let now = now_ms();
        let mut found = Vec::new();
        self.pending.retain(|&(pid, deadline)| match title_of(pid) {
            Some(tid) => {
                found.push((pid, tid));
                false
            }
            None => now < deadline && alive(pid),
        });
        let titles = self.titles();
        for (pid, tid) in found {
            if titles.contains(&tid) {
                self.attach(pid, &tid);
            }
        }
    }

    /// Strategy 2 (SMP sm_fakelib_game_on_exec): an exec of one of our titles. Mounts the overlay
    /// if it is not on top of common/lib yet, and watches the pid for NOTE_EXIT.
    fn attach(&mut self, pid: libc::pid_t, tid: &str) {
        let name = match self.sessions.iter().find(|(_, s)| s.pid == Some(pid)) {
            Some((name, _)) => name.clone(),
            None => match resolve_sandbox(tid) {
                Ok(name) => name,
                Err(e) => {
                    tlog!("{tid} exec pid={pid}: no sandbox ({e})");
                    return;
                }
            },
        };
        let s = self.sessions.entry(name.clone()).or_insert_with(|| Session::new(tid));
        if s.lib.is_none() {
            s.lib = find_lib(&name);
        }
        let top = s.top_layer();
        let on_top = top.is_some_and(|i| i + 1 == s.srcs.len());
        tlog!("{tid} exec pid={pid} sandbox={name}: overlay {}", if on_top { "already there" } else { "MISSING" });
        if s.cleanup.take().is_some() {
            tlog!("{tid} pending cleanup cancelled by exec");
        }
        match s.lib.clone() {
            None => tlog!("{tid} exec pid={pid}: no <x>/common/lib in {name}"),
            // A reused session: a partial cleanup may have taken /data and the USB mounts already
            // (mount_dir keeps the ones still in place).
            Some(lib) if on_top => s.mount_data(&lib),
            _ if s.srcs.is_empty() => tlog!("{tid} empty fakelib, nothing to overlay; not mounting"),
            Some(_) if top.is_some() => {
                s.mount(top.map_or(0, |i| i + 1), "exec fallback");
            }
            Some(lib) => match top_mount(&lib) {
                Ok(t) if t.on == lib && t.fstype == "unionfs" => {
                    tlog!("{tid} common/lib occupied by unionfs from {}; not mounting", t.from)
                }
                _ => {
                    s.mount(0, "exec fallback");
                }
            },
        }
        if s.pid != Some(pid) {
            s.pid = Some(pid);
            // SMP register_game_exit_watch: EV_ADD replaces the tracked knote's fflags.
            let k = kev(pid as usize, libc::EVFILT_PROC, libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR, libc::NOTE_EXIT as u32);
            if let Err(e) = kevent_add(self.kq, k) {
                tlog!("{tid} exit watch pid={pid} failed: {e}");
            }
        }
    }

    /// Strategy 3: the game exited, unmount so ShellCore can tear the sandbox down.
    fn on_exit(&mut self, pid: libc::pid_t) {
        self.pending.retain(|p| p.0 != pid);
        for (name, s) in self.sessions.iter_mut().filter(|(_, s)| s.pid == Some(pid)) {
            tlog!("{} exit pid={pid} sandbox={name}", s.tid);
            s.cleanup.get_or_insert((now_ms() + CLEANUP_TIMEOUT_MS, 0));
        }
    }

    /// SMP schedule_fakelib_cleanup: EBUSY retries every 250 ms for 10 s; past that one try
    /// with MNT_FORCE. A session is dropped once its layers are gone or the retries end.
    fn poll_cleanups(&mut self) {
        let now = now_ms();
        let mut done = Vec::new();
        for (name, s) in self.sessions.iter_mut() {
            let Some((deadline, next)) = s.cleanup else { continue };
            if now < next {
                continue;
            }
            let first = next == 0;
            match s.unmount_pass(false) {
                None => {
                    tlog!("{} cleanup done: {name}", s.tid);
                    done.push(name.clone());
                }
                Some(libc::EBUSY) if now < deadline => {
                    if first {
                        tlog!("{} overlay busy, retrying for {} ms: {name}", s.tid, CLEANUP_TIMEOUT_MS);
                    }
                    s.cleanup = Some((deadline, now + RETRY_MS));
                }
                Some(code) => {
                    let forced = if code == libc::EBUSY { s.unmount_pass(true) } else { Some(code) };
                    match forced {
                        None => tlog!("{} cleanup done after MNT_FORCE: {name}", s.tid),
                        Some(code) => tlog!("{} cleanup FAILED errno={code}; {} layer(s) left on {:?}",
                            s.tid, s.layers.len(), s.lib),
                    }
                    done.push(name.clone());
                }
            }
        }
        for name in done {
            self.sessions.remove(&name);
        }
    }
}
