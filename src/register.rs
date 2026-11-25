//! Registers a mounted game on the home screen. Port of ShadowMountPlus mount_and_install
//! (src/sm_install.c): stage sce_sys and appmeta, copy trophy metadata, nullfs-mount the game at
//! /system_ex/app/<TID>, then call sceAppInstUtilAppInstallTitleDir.
//!
//! SMP also writes /user/app/<TID>/mount.lnk, but only for its own remount/cleanup tracking;
//! nothing here reads it, so it is skipped.

use crate::sys::nmount;
use std::collections::HashSet;
use std::ffi::{c_char, c_void, CStr, CString};
use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

const APP_BASE: &str = "/user/app";
const SYSTEM_EX_APP: &str = "/system_ex/app";
const APPMETA_BASE: &str = "/user/appmeta";
const TROPHY_BASE: &str = "/system_data/priv/appmeta";
/// The system's title list (SMP APP_DB_PATH).
const APP_DB: &str = "/system_data/priv/mms/app.db";
const APP_INST_UTIL: &str = "/system/common/lib/libSceAppInstUtil.sprx";
/// sceAppInstUtilAppInstallTitleDir: the title is already in app.db (SMP logs "Restored").
const ALREADY_INSTALLED: u32 = 0x8099_0002;

fn err(msg: String) -> io::Error {
    io::Error::new(ErrorKind::Other, msg)
}

fn ctx<T>(r: io::Result<T>, what: impl FnOnce() -> String) -> io::Result<T> {
    r.map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", what())))
}

/// SMP copy_param_json_rewrite: the first `upgradable` becomes `standard` (applicationDrmType),
/// so the system does not treat the title as an upgradable (disc/patch) app. True when patched.
fn patch_param_json(buf: &mut Vec<u8>) -> bool {
    const FROM: &[u8] = b"upgradable";
    let Some(i) = buf.windows(FROM.len()).position(|w| w == FROM) else { return false };
    buf.splice(i..i + FROM.len(), b"standard".iter().copied());
    true
}

/// One staged file: overwritten when present, then 0666, since a 0600 file from a game folder
/// would stay unreadable for the trophy and save services.
/// A file named param.json gets patch_param_json (only these staged copies, never the game's).
fn copy_file(title_id: &str, src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if src.file_name().is_some_and(|n| n == "param.json") {
        let mut buf = crate::scan::read_capped(src, 1 << 20)?;
        let patched = patch_param_json(&mut buf);
        fs::write(dst, &buf)?;
        if patched {
            log!("{title_id}: param.json patched: {}", dst.display());
        }
    } else {
        // 1 MiB at a time: on FreeBSD std's fs::copy is a read/write loop with an 8 KiB buffer,
        // and staging sce_sys and appmeta (pic*.png/pic*.dds, ~80 MB) took 16-24 s per game.
        // io::copy reads straight into a BufReader's own buffer.
        io::copy(&mut io::BufReader::with_capacity(1 << 20, fs::File::open(src)?), &mut fs::File::create(dst)?)?;
    }
    fs::set_permissions(dst, fs::Permissions::from_mode(0o666))
}

/// Recursive copy, like SMP copy_dir: existing files are overwritten.
fn copy_dir(title_id: &str, src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let (s, d) = (e.path(), dst.join(e.file_name()));
        if e.file_type()?.is_dir() {
            copy_dir(title_id, &s, &d)?;
        } else {
            copy_file(title_id, &s, &d)?;
        }
    }
    Ok(())
}

fn is_appmeta_file(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "param.json"
        || n == "param.sfo"
        || n.ends_with(".png")
        || n.ends_with(".dds")
        || n.ends_with(".at9")
}

/// Top-level param.json, param.sfo, *.png, *.dds, *.at9 from sce_sys into /user/appmeta/<TID>.
fn copy_appmeta(title_id: &str, src_sce_sys: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src_sce_sys)? {
        let e = e?;
        let name = e.file_name();
        let Some(name_str) = name.to_str() else { continue };
        if !is_appmeta_file(name_str) || !e.file_type()?.is_file() {
            continue;
        }
        copy_file(title_id, &e.path(), &dst.join(&name))?;
    }
    Ok(())
}

/// SMP update_trophy_metadata: copy npbind.dat files and param.json to
/// /system_data/priv/appmeta/<TID> when present in the source and absent at the destination.
/// One already there is not overwritten, but gets +0444 when a read bit is missing, so the
/// trophy service (another uid) can read it.
fn update_trophy_metadata(title_id: &str, src_sce_sys: &Path) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let base = Path::new(TROPHY_BASE).join(title_id);
    fs::create_dir_all(base.join("trophy2"))?;
    fs::create_dir_all(base.join("uds"))?;
    for rel in ["trophy2/npbind.dat", "uds/npbind.dat", "param.json"] {
        let (s, d) = (src_sce_sys.join(rel), base.join(rel));
        if !s.exists() {
            continue;
        }
        match fs::metadata(&d) {
            Err(_) => ctx(copy_file(title_id, &s, &d), || format!("copy {} -> {}", s.display(), d.display()))?,
            Ok(m) if m.is_file() && m.mode() & 0o444 != 0o444 => {
                let mode = fs::Permissions::from_mode((m.mode() & 0o7777) | 0o444);
                ctx(fs::set_permissions(&d, mode), || format!("chmod {}", d.display()))?;
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

/// True when `a` and `b` are the same directory by inode. A nullfs root passes its lower
/// directory's inode through but reports its own device (FreeBSD null_getattr), so dev is
/// not compared. ponytail: an inode reused for a new cache dir would look current.
fn same_inode(a: &str, b: &str) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.ino() == y.ino(),
        _ => false,
    }
}

/// The top mount at a path, from statfs.
struct Mnt {
    fstype: String,
    /// f_mntfromname, "<above>:" (unionfs upper dir) stripped.
    from: String,
    flags: u64,
    /// f_mntfromname filled its buffer (88 bytes on the FreeBSD 11 layout), so `from` may be cut.
    cut: bool,
}

impl Mnt {
    /// `from` is `want`, or a cut-off prefix of it.
    fn from_is(&self, want: &str) -> bool {
        self.from == want || (self.cut && !self.from.is_empty() && want.starts_with(&self.from))
    }
}

/// The top mount exactly at `dst`, or None when `dst` is not a mount point.
fn mount_source(dst: &str) -> io::Result<Option<Mnt>> {
    let c = CString::new(dst).map_err(|_| err(format!("NUL in {dst}")))?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let fstype = unsafe { CStr::from_ptr(st.f_fstypename.as_ptr()) };
    let on = unsafe { CStr::from_ptr(st.f_mntonname.as_ptr()) };
    if on.to_bytes() != dst.as_bytes() {
        return Ok(None);
    }
    let raw = unsafe { CStr::from_ptr(st.f_mntfromname.as_ptr()) };
    let cut = raw.to_bytes().len() + 1 >= st.f_mntfromname.len();
    let from = raw.to_string_lossy();
    let from = from.strip_prefix("<above>:").unwrap_or(&from).to_string();
    Ok(Some(Mnt { fstype: fstype.to_string_lossy().into_owned(), from, flags: st.f_flags as u64, cut }))
}

/// Unmounts every layer at /system_ex/app/<TID>, newest first, never MNT_FORCE (a running game
/// keeps its stack: EBUSY is returned). A statfs failure there (removed USB under a nullfs:
/// EPIPE/EAGAIN/EIO/ENOENT) counts as a stale mount; unmount(2) finds it by path string.
pub fn unmount_title(tid: &str) -> io::Result<()> {
    let dst = format!("{SYSTEM_EX_APP}/{tid}");
    for _ in 0..4 {
        match mount_source(&dst) {
            Ok(None) => return Ok(()),
            Ok(Some(m)) => log!("{tid}: unmounting {dst} ({} of {})", m.fstype, m.from),
            Err(e) => log!("{tid}: statfs {dst}: {e}; unmounting it as stale"),
        }
        match crate::sys::unmount(&dst, 0) {
            Ok(()) => {}
            // EINVAL: no mount at that path (FreeBSD kern_unmount by path), e.g. dst is absent.
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => return Ok(()),
            Err(e) => return ctx(Err(e), || format!("unmount {dst}")),
        }
    }
    match mount_source(&dst) {
        Ok(None) => Ok(()),
        _ => Err(err(format!("{dst}: still mounted after 4 unmounts"))),
    }
}

/// SMP remount_system_ex: nmount(MNT_UPDATE) of /system_ex without MNT_RDONLY makes it writable.
/// ponytail: the device and options are SMP's (also etaHEN's, kstuff-lite's); no other layout is tried.
fn remount_system_ex() -> io::Result<()> {
    let opts = [
        ("from", Some("/dev/ssd0.system_ex")), ("fspath", Some("/system_ex")), ("fstype", Some("exfatfs")),
        ("large", Some("yes")), ("timezone", Some("static")), ("async", None), ("ignoreacl", None),
    ];
    crate::sys::nmount_opt(&opts, libc::MNT_UPDATE as i32)
}

/// SMP mount_title_nullfs: `src` must be a dir with eboot.bin or eboot2.bin; mkdir -p
/// /system_ex/app/<TID> 0755, then nmount(fstype=nullfs, from, fspath, flags 0). Any other
/// mount at the target (another tool's copy, a stale nullfs of a rebuilt cache, an old
/// backport unionfs) is unmounted first.
fn mount_title_nullfs(title_id: &str, src: &str) -> io::Result<()> {
    let src_path = Path::new(src);
    if !src_path.is_dir() {
        return Err(err(format!("nullfs source {src} is not a directory")));
    }
    if !src_path.join("eboot.bin").is_file() && !src_path.join("eboot2.bin").is_file() {
        return Err(err(format!("nullfs source {src} has no eboot.bin/eboot2.bin")));
    }
    let dst = format!("{SYSTEM_EX_APP}/{title_id}");
    // Recursive: SMP mkdirs /system_ex/app at start-up. Still EEXIST for a file or a dead mount.
    let mk = || fs::DirBuilder::new().mode(0o755).recursive(true).create(&dst);
    let made = match mk() {
        // /system_ex is read-only until a tool remounts it (etaHEN, SMP and kstuff-lite do at
        // start-up, elfldr does not). An existing title dir never gets here: mkdir says EEXIST first.
        Err(e) if e.raw_os_error() == Some(libc::EROFS) => {
            log!("{title_id}: mkdir {dst}: {e}; remounting /system_ex read-write");
            ctx(remount_system_ex(), || "remount /system_ex".to_string())?;
            mk()
        }
        r => r,
    };
    match made {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::AlreadyExists => match fs::metadata(&dst) {
            Ok(m) if !m.is_dir() => return Err(err(format!("mount target {dst} is not a directory"))),
            Ok(_) => {}
            // A dead nullfs over a removed USB folder; the loop below unmounts it.
            Err(e) => log!("{title_id}: stat {dst}: {e}; treating it as a stale mount"),
        },
        Err(e) => return ctx(Err(e), || format!("mkdir {dst}")),
    }
    // ponytail: at most 4 stacked layers, then give up (SMP has a full stack reset).
    for _ in 0..4 {
        match mount_source(&dst) {
            Ok(None) => {
                return ctx(nmount(&[("fstype", "nullfs"), ("from", src), ("fspath", &dst)], 0), || {
                    format!("nullfs {src} -> {dst}")
                });
            }
            // Same source path is not enough: a rebuilt (deleted, recreated) cache dir leaves this
            // mount on the old inode, and ShellCore then sees an empty app (0x80940033).
            Ok(Some(m)) if m.fstype == "nullfs" && m.from_is(src) && same_inode(&dst, src) => return Ok(()),
            Ok(Some(m)) => {
                log!("{title_id}: {dst} is a {} of {} (not ours, or stale); unmounting it", m.fstype, m.from)
            }
            Err(e) => log!("{title_id}: statfs {dst}: {e}; unmounting it as stale"),
        }
        // Never MNT_FORCE: EBUSY means a running game holds the stack; leave it alone.
        ctx(crate::sys::unmount(&dst, 0), || format!("unmount {dst} (title running?)"))?;
    }
    Err(err(format!("{dst}: still mounted after 4 unmounts")))
}

/// SMP mount_backport_overlay: unionfs of the backport dir on top of the nullfs at
/// /system_ex/app/<TID> (copymode=transparent, noatime, fnodup; read-only if the nullfs is).
fn mount_backport_overlay(title_id: &str, backport: &Path) -> io::Result<()> {
    let src = backport.to_str().ok_or_else(|| err(format!("non-UTF-8 backport {}", backport.display())))?;
    if !backport.is_dir() {
        return Err(err(format!("backport {src} is not a directory")));
    }
    let dst = format!("{SYSTEM_EX_APP}/{title_id}");
    let Some(m) = ctx(mount_source(&dst), || format!("statfs {dst}"))? else {
        return Err(err(format!("{dst} is not mounted; no backport overlay")));
    };
    if m.fstype != "nullfs" {
        return Err(err(format!("{dst} is a {}, not our nullfs; no backport overlay", m.fstype)));
    }
    let flags = m.flags;
    let opts = [
        ("fstype", Some("unionfs")), ("from", Some(src)), ("fspath", Some(dst.as_str())),
        ("copymode", Some("transparent")), ("noatime", None), ("fnodup", None),
    ];
    let ro = if flags & libc::MNT_RDONLY as u64 != 0 { libc::MNT_RDONLY } else { 0 };
    ctx(crate::sys::nmount_opt(&opts, ro), || format!("unionfs {src} -> {dst}"))
}

type InstallTitleDir =
    unsafe extern "C" fn(title_id: *const c_char, path: *const c_char, reserved: *mut c_void) -> i32;
type UninstallTitle = unsafe extern "C" fn(title_id: *const c_char) -> i32;

extern "C" {
    fn sceUserServiceInitialize(params: *const c_void) -> i32;
    fn sceAppInstUtilInitialize() -> i32;
}

/// Once per process: the two service inits (SMP main.c).
static APP_INST_INIT: OnceLock<()> = OnceLock::new();
static INSTALL_FN: OnceLock<Result<InstallTitleDir, String>> = OnceLock::new();
static UNINSTALL_FN: OnceLock<Result<UninstallTitle, String>> = OnceLock::new();

fn dlerror_str() -> String {
    let e = unsafe { libc::dlerror() };
    if e.is_null() {
        "unknown".into()
    } else {
        unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
    }
}

fn resolve_symbol(nid: &[u8], name: &[u8], label: &str) -> Result<*mut c_void, String> {
    APP_INST_INIT.get_or_init(|| {
        let rc = unsafe { sceUserServiceInitialize(ptr::null()) };
        log!("sceUserServiceInitialize: {:#x}", rc as u32);
        let rc = {
            let _appinst = crate::install::AppInst::lock();
            unsafe { sceAppInstUtilInitialize() }
        };
        log!("sceAppInstUtilInitialize: {:#x}", rc as u32);
    });
    let lib = CString::new(APP_INST_UTIL).expect("no NUL");
    let h = unsafe { libc::dlopen(lib.as_ptr(), libc::RTLD_LAZY) };
    if h.is_null() {
        return Err(format!("dlopen {APP_INST_UTIL}: {}", dlerror_str()));
    }
    // The PS5 dynlib exports by NID (base64 of SHA1(name + suffix), first 8 bytes):
    // Wudg3Xe3heE = sceAppInstUtilAppInstallTitleDir, Sx4TTyrQccE = sceAppInstUtilAppUnInstall.
    // First through the SDK's module table (dump-forge ps5/launcher.c; -1 = this process).
    extern "C" {
        fn kernel_dynlib_handle(pid: libc::pid_t, basename: *const c_char, handle: *mut u32) -> i32;
        fn kernel_dynlib_resolve(pid: libc::pid_t, handle: u32, nid: *const c_char) -> isize;
    }
    let mut handle = 0u32;
    let rc = unsafe { kernel_dynlib_handle(-1, b"libSceAppInstUtil.sprx\0".as_ptr() as *const c_char, &mut handle) };
    let addr = if rc == 0 {
        unsafe { kernel_dynlib_resolve(-1, handle, nid.as_ptr() as *const c_char) }
    } else {
        0
    };
    log!("kernel_dynlib_handle: {rc:#x} handle={handle:#x} resolve={addr:#x}");
    if addr != 0 {
        return Ok(addr as *mut c_void);
    }
    let mut f = ptr::null_mut();
    for (handle, sym) in [
        (h, nid),
        (libc::RTLD_DEFAULT, nid),
        (h, name),
    ] {
        f = unsafe { libc::dlsym(handle, sym.as_ptr() as *const c_char) };
        if !f.is_null() {
            break;
        }
    }
    if f.is_null() {
        return Err(format!("dlsym {label}: {}", dlerror_str()));
    }
    Ok(f)
}

fn resolve_install() -> Result<InstallTitleDir, String> {
    let f = resolve_symbol(b"Wudg3Xe3heE\0", b"sceAppInstUtilAppInstallTitleDir\0", "sceAppInstUtilAppInstallTitleDir")?;
    Ok(unsafe { std::mem::transmute::<*mut c_void, InstallTitleDir>(f) })
}

fn resolve_uninstall() -> Result<UninstallTitle, String> {
    let f = resolve_symbol(b"Sx4TTyrQccE\0", b"sceAppInstUtilAppUnInstall\0", "sceAppInstUtilAppUnInstall")?;
    Ok(unsafe { std::mem::transmute::<*mut c_void, UninstallTitle>(f) })
}

pub fn uninstall(tid: &str) -> io::Result<()> {
    let f = UNINSTALL_FN.get_or_init(resolve_uninstall).as_ref().map_err(|e| err(e.clone()))?;
    let title_id = CString::new(tid).map_err(|_| err(format!("NUL in {tid}")))?;
    let rc = {
        let _appinst = crate::install::AppInst::lock();
        (unsafe { f(title_id.as_ptr()) }) as u32
    };
    if rc == 0 { Ok(()) } else { Err(err(format!("sceAppInstUtilAppUnInstall({tid}): {rc:#x}"))) }
}

/// After an uninstall the title must count as unregistered: is_registered searches app.db as
/// bytes, and a deleted row can survive in a free page. Removing the staged
/// /user/app/<TID>/sce_sys/param.json makes it false, so a returning source registers in full.
pub fn forget(tid: &str) {
    let p = Path::new(APP_BASE).join(tid).join("sce_sys/param.json");
    match fs::remove_file(&p) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => log!("{tid}: remove {}: {e}", p.display()),
        _ => {}
    }
}

/// sceAppInstUtilAppInstallTitleDir(tid, "/user/app/", NULL); 0 and 0x80990002 are success.
fn install_title_dir(title_id: &str) -> io::Result<()> {
    let f = *INSTALL_FN.get_or_init(resolve_install).as_ref().map_err(|e| err(e.clone()))?;
    let tid = CString::new(title_id).map_err(|_| err(format!("NUL in {title_id}")))?;
    let base = CString::new(format!("{APP_BASE}/")).expect("no NUL");
    let rc = {
        let _appinst = crate::install::AppInst::lock();
        (unsafe { f(tid.as_ptr(), base.as_ptr(), ptr::null_mut()) }) as u32
    };
    if rc == 0 || rc == ALREADY_INSTALLED {
        return Ok(());
    }
    Err(err(format!("sceAppInstUtilAppInstallTitleDir({title_id}): {rc:#x}")))
}

/// dump-forge's last resort when sceAppInstUtilAppInstallTitleDir does not resolve (FW 12+):
/// registers every folder under /user/app, so one call (~20 s) covers every title `register`
/// staged in a scan pass, like SMP's install queue. The get_or_init runs the AppInstUtil init.
pub fn install_all(title_ids: &[String]) -> io::Result<()> {
    extern "C" {
        fn sceAppInstUtilAppInstallAll(reserved: *mut c_void) -> i32;
    }
    let why = INSTALL_FN.get_or_init(resolve_install).as_ref().err().cloned().unwrap_or_default();
    // Taken before the call: a tile staged while it runs (the tile thread) waits for the next pass
    // instead of being marked registered by a call that never saw it.
    // ponytail: taken either way: a failed tile keeps tile.ok "pending" and gets no retry until the
    // next start, so a failing call does not repeat (~20 s) every pass.
    let tile = TILE_STAGED.lock().unwrap().take();
    let t = std::time::Instant::now();
    let rc = {
        let _appinst = crate::install::AppInst::lock();
        (unsafe { sceAppInstUtilAppInstallAll(ptr::null_mut()) }) as u32
    };
    let ms = t.elapsed().as_millis();
    let ids: Vec<&str> = title_ids.iter().map(String::as_str).chain(tile.as_ref().map(|_| TILE_ID)).collect();
    log!("{}: {why}; sceAppInstUtilAppInstallAll: {rc:#x} ({ms} ms)", ids.join(", "));
    if rc != 0 {
        return Err(err(format!("sceAppInstUtilAppInstallAll: {rc:#x}")));
    }
    if let Some(hash) = tile {
        match fs::write(TILE_MARKER, format!("{hash}\n")) {
            Ok(()) => log!("tile {TILE_ID}: registered"),
            Err(e) => log!("tile {TILE_ID}: registered, but write {TILE_MARKER} failed: {e}"),
        }
    }
    for tid in title_ids {
        spawn_snd0info(tid, 200);
    }
    Ok(())
}

/// SMP mount_and_install: stage `sce_sys` into /user/app/<TID> (sce_sys + icon0.png) and
/// /user/appmeta/<TID> (param.*, *.png incl. pic0/pic1, *.dds, *.at9), trophy metadata, nullfs
/// `app0` at /system_ex/app/<TID>, the optional backport unionfs on top, then register.
/// Ok(false): staged and mounted only, waiting for the pass's `install_all` (no TitleDir).
pub fn register(title_id: &str, app0: &Path, sce_sys: &Path, backport: Option<&Path>) -> io::Result<bool> {
    ctx(register_inner(title_id, app0, sce_sys, backport), || format!("register {title_id} ({})", app0.display()))
}

fn register_inner(title_id: &str, app0: &Path, sce_sys: &Path, backport: Option<&Path>) -> io::Result<bool> {
    let app_dir = Path::new(APP_BASE).join(title_id);
    let user_sce_sys = app_dir.join("sce_sys");
    ctx(copy_dir(title_id, sce_sys, &user_sce_sys), || {
        format!("copy {} -> {}", sce_sys.display(), user_sce_sys.display())
    })?;
    let (icon_src, icon_dst) = (sce_sys.join("icon0.png"), app_dir.join("icon0.png"));
    ctx(copy_file(title_id, &icon_src, &icon_dst), || {
        format!("copy {} -> {}", icon_src.display(), icon_dst.display())
    })?;
    let appmeta = Path::new(APPMETA_BASE).join(title_id);
    ctx(copy_appmeta(title_id, sce_sys, &appmeta), || format!("appmeta -> {}", appmeta.display()))?;
    ctx(update_trophy_metadata(title_id, sce_sys), || "trophy metadata".into())?;
    mount_inner(title_id, app0, backport, false)?;
    if INSTALL_FN.get_or_init(resolve_install).is_err() {
        return Ok(false);
    }
    install_title_dir(title_id)?;
    // ponytail: SMP's fixed 200 ms (sm_install.c) for the install's app.db row to land; a row
    // not there yet is caught by the next run's already-registered pass or the next game exit.
    spawn_snd0info(title_id, 200);
    Ok(true)
}

/// `set_snd0info` on its own thread after `delay_ms`, so a busy app.db (up to 3 s) does not hold
/// the caller (scan thread, launch watcher); APP_DB_LOCK still serializes the writes.
pub fn spawn_snd0info(title_id: &str, delay_ms: u64) {
    let tid = title_id.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        set_snd0info(&tid);
    });
}

/// Ok when the home-screen title looks like one Kraken Mount staged; Err(path) when the path
/// shows a real install: /user/app/<TID>/app.pkg, a copy on an M.2 drive
/// (/mnt/ext{0,1}/user/app/<TID>), or no /user/app/<TID>/sce_sys/param.json (we write one).
/// ponytail: a layout heuristic, not an app.db lookup; an install laid out like ours reads as ours.
pub fn is_ours(title_id: &str) -> Result<(), PathBuf> {
    is_ours_in(Path::new("/"), title_id)
}

/// `is_ours` under `root` (a fake tree in tests).
fn is_ours_in(root: &Path, title_id: &str) -> Result<(), PathBuf> {
    let app = root.join("user/app").join(title_id);
    if let Some(p) = crate::installed::copy_in(root, title_id) {
        return Err(p);
    }
    if !app.join("sce_sys/param.json").is_file() {
        return Err(app);
    }
    Ok(())
}

extern "C" {
    /// ps5/appdb.c: one statement with ?1 = `arg`; rows changed, or -1 with the error in `err`.
    fn km_sqlite_exec1(path: *const c_char, sql: *const c_char, arg: *const c_char, err: *mut c_char, errlen: usize) -> i32;
}

/// The scan thread and the launch watcher both write app.db.
static APP_DB_LOCK: Mutex<()> = Mutex::new(());

const APP_DB_WAL: &str = "/system_data/priv/mms/app.db-wal";
/// ?1 = comma-joined title ids: one statement, so one write transaction for any number of titles
/// (a no-op UPDATE still takes the write lock). ponytail: instr() scans tbl_contentinfo (a few
/// hundred rows); title ids are [A-Z0-9], never a comma.
const SND0_SQL: &str = "UPDATE tbl_contentinfo SET snd0info = '/user/appmeta/' || titleId || '/snd0.at9' \
    WHERE instr(',' || ?1 || ',', ',' || titleId || ',') > 0 \
    AND snd0info IS NOT '/user/appmeta/' || titleId || '/snd0.at9'";

type DbStamp = (Option<(SystemTime, u64)>, Option<(SystemTime, u64)>);
static LAST_APP_DB_STAMP: Mutex<Option<DbStamp>> = Mutex::new(None);
static SND0_SYNCING: AtomicBool = AtomicBool::new(false);

fn file_stamp(p: &str) -> Option<(SystemTime, u64)> {
    let m = fs::metadata(p).ok()?;
    Some((m.modified().ok()?, m.len()))
}

fn app_db_stamp() -> DbStamp {
    (file_stamp(APP_DB), file_stamp(APP_DB_WAL))
}

fn has_snd0(tid: &str) -> bool {
    Path::new(APPMETA_BASE).join(tid).join("snd0.at9").is_file()
}

/// One UPDATE for every title in `tids` with a snd0.at9. False when it failed (logged).
fn exec_snd0info(tids: &[&str]) -> bool {
    let ids = tids.iter().copied().filter(|t| has_snd0(t)).collect::<Vec<_>>().join(",");
    if ids.is_empty() {
        return true;
    }
    let (Ok(path), Ok(sql), Ok(arg)) = (CString::new(APP_DB), CString::new(SND0_SQL), CString::new(ids.as_str())) else {
        return false;
    };
    let mut msg = [0 as c_char; 256];
    let rows = {
        let _guard = APP_DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { km_sqlite_exec1(path.as_ptr(), sql.as_ptr(), arg.as_ptr(), msg.as_mut_ptr(), msg.len()) }
    };
    if rows > 0 {
        log!("{ids}: snd0info set ({rows} row(s))");
    } else if rows < 0 {
        let e = unsafe { CStr::from_ptr(msg.as_ptr()) }.to_string_lossy();
        log!("{ids}: snd0info update in {APP_DB} failed: {e}");
    }
    rows >= 0
}

/// SMP update_snd0info (sm_appdb.c): point app.db's snd0info at /user/appmeta/<TID>/snd0.at9 so
/// the home screen plays the title sound. Only when that file exists; errors are logged only.
pub fn set_snd0info(title_id: &str) {
    exec_snd0info(&[title_id]);
}

/// When app.db or its WAL changed (mtime/size) since the last good sync, one background UPDATE
/// for the registered titles. Installed games are skipped: their row is the system's. A failed
/// sync keeps the old stamp, so the next pass retries it.
pub fn sync_snd0info_if_changed(tids: &HashSet<String>) {
    let stamp = app_db_stamp();
    if *LAST_APP_DB_STAMP.lock().unwrap_or_else(|e| e.into_inner()) == Some(stamp) {
        return;
    }
    if SND0_SYNCING.swap(true, Ordering::SeqCst) {
        return;
    }
    let list: Vec<String> =
        tids.iter().filter(|t| has_snd0(t) && !crate::installed::is_installed(t)).cloned().collect();
    std::thread::spawn(move || {
        let refs: Vec<&str> = list.iter().map(String::as_str).collect();
        if exec_snd0info(&refs) {
            *LAST_APP_DB_STAMP.lock().unwrap_or_else(|e| e.into_inner()) = Some(app_db_stamp());
        }
        SND0_SYNCING.store(false, Ordering::SeqCst);
    });
}

/// True when the title is on the home screen already: /user/app/<TID>/sce_sys/param.json,
/// /user/appmeta/<TID>/param.json, and the title id in app.db (SMP checks app.db + is_installed).
/// ponytail: app.db is searched for the TID bytes, no SELECT; a deleted row still in a free page
/// reads as registered (a rebuild re-registers). Unreadable app.db = not registered.
pub fn is_registered(title_id: &str) -> bool {
    if !staged(title_id) {
        return false;
    }
    match fs::read(APP_DB) {
        Ok(db) => db.windows(title_id.len()).any(|w| w == title_id.as_bytes()),
        Err(e) => {
            log!("{title_id}: read {APP_DB}: {e}; registering again");
            false
        }
    }
}

/// The two staged files of a registration (/user/app/<TID>/sce_sys/param.json and
/// /user/appmeta/<TID>/param.json): two stats, cheap enough for every scan pass.
pub fn staged(title_id: &str) -> bool {
    Path::new(APP_BASE).join(title_id).join("sce_sys/param.json").is_file()
        && Path::new(APPMETA_BASE).join(title_id).join("param.json").is_file()
}

/// is_registered for many titles (web API status) with one pass over one app.db read, in `tids` order.

static DB_READ_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// ponytail: read on every call (each /api/state poll); app.db is a few MB at most.
pub fn registered(tids: &[&str]) -> Vec<bool> {
    let staged: Vec<bool> = tids.iter().map(|t| staged(t)).collect();
    if !staged.contains(&true) {
        return staged;
    }
    let db = match fs::read(APP_DB) {
        Ok(db) => db,
        Err(e) => {
            // Once until it reads again: /api/state polls this every second or two per open UI.
            if !DB_READ_FAILED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                log!("read {APP_DB}: {e}; titles shown as not registered");
            }
            return vec![false; tids.len()];
        }
    };
    DB_READ_FAILED.store(false, std::sync::atomic::Ordering::Relaxed);
    let wanted: HashSet<&[u8]> = tids.iter().zip(&staged).filter(|(_, s)| **s).map(|(t, _)| t.as_bytes()).collect();
    // Every TID-shaped 9-byte run of app.db (a TID is 9 bytes, crate::scan::valid_tid).
    let mut found: HashSet<&[u8]> = HashSet::new();
    for w in db.windows(9) {
        if w[0].is_ascii_uppercase() && wanted.contains(w) {
            found.insert(w);
        }
    }
    tids.iter().zip(staged).map(|(t, s)| s && found.contains(t.as_bytes())).collect()
}

/// Only the nullfs `app0` -> /system_ex/app/<TID> (+ backport unionfs) of `register`, for a
/// title already registered; a no-op when it is mounted (it is gone after a reboot).
pub fn mount(title_id: &str, app0: &Path, backport: Option<&Path>) -> io::Result<()> {
    ctx(mount_inner(title_id, app0, backport, true), || format!("mount {title_id} ({})", app0.display()))
}

/// Takes our cache nullfs off the top of /system_ex/app/<TID> (an installed title run through ng), never
/// MNT_FORCE and never a layer below it; Ok when ours is not on top (nothing of ours to take).
pub fn drop_cache_layer(title_id: &str) -> io::Result<()> {
    let dst = format!("{SYSTEM_EX_APP}/{title_id}");
    let cache = crate::cache::dir(title_id);
    match mount_source(&dst) {
        Ok(Some(m)) if m.fstype == "nullfs" && Path::new(&m.from).starts_with(&cache) => {
            ctx(crate::sys::unmount(&dst, 0), || format!("unmount {dst}"))
        }
        Ok(_) => Ok(()),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// `mount` for an installed title run through ng (scan::installed_pkg): our cache nullfs at
/// /system_ex/app/<TID>, the console's registration untouched. A layer of the system's there is never
/// unmounted: ours goes on top of it (main's drop_old_mount takes ours off, top first, when ng is
/// switched off). An unreadable statfs is refused.
pub fn mount_over_install(title_id: &str, app0: &Path) -> io::Result<()> {
    let dst = format!("{SYSTEM_EX_APP}/{title_id}");
    let src = app0.to_str().ok_or_else(|| err(format!("non-UTF-8 path {}", app0.display())))?;
    match mount_source(&dst) {
        // Nothing mounted there (or no mount point yet): the usual mount (mkdir, /system_ex remount).
        Ok(None) => mount(title_id, app0, None),
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => mount(title_id, app0, None),
        Err(e) => Err(err(format!("statfs {dst}: {e}; not mounted"))),
        Ok(Some(m)) if m.fstype == "nullfs" && m.from_is(src) && same_inode(&dst, src) => Ok(()),
        Ok(Some(m)) => {
            // Ours on an old inode (a rebuilt cache): off first, never MNT_FORCE. Anything else stays below.
            if m.fstype == "nullfs" && m.from_is(src) {
                log!("{title_id}: {dst} is a stale nullfs of {}; unmounting it", m.from);
                ctx(crate::sys::unmount(&dst, 0), || format!("unmount {dst}"))?;
                if mount_source(&dst).ok().flatten().is_none() {
                    return mount(title_id, app0, None);
                }
            }
            ctx(crate::sys::nmount(&[("fstype", "nullfs"), ("from", src), ("fspath", &dst)], 0), || {
                format!("nullfs {src} -> {dst} (over the system's layer)")
            })
        }
    }
}

/// `mount` for a moved folder game: the whole stack is redone on the new `app0` even when the
/// backport overlay on top is current (the nullfs under it still shows the old folder). Never
/// MNT_FORCE: a running game's stack fails with EBUSY.
pub fn remount(title_id: &str, app0: &Path, backport: Option<&Path>) -> io::Result<()> {
    ctx(mount_inner(title_id, app0, backport, false), || format!("remount {title_id} ({})", app0.display()))
}

/// The stack at /system_ex/app/<TID> without its backport unionfs, before the backport is deleted:
/// redone on `app0` alone when something is mounted there. Never MNT_FORCE: busy = EBUSY.
pub fn drop_backport(title_id: &str, app0: &Path) -> io::Result<()> {
    match mount_source(&format!("{SYSTEM_EX_APP}/{title_id}")) {
        Ok(None) => Ok(()),
        // Never registered (no mount point yet): no stack to redo. A stale stack (its drive gone)
        // answers ENOENT too, but its name is still listed in /system_ex/app.
        Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR))
            && fs::read_dir(SYSTEM_EX_APP)
                .and_then(|d| d.map(|x| x.map(|x| x.file_name())).collect::<io::Result<Vec<_>>>())
                .is_ok_and(|names| !names.iter().any(|n| n == title_id)) => Ok(()),
        Err(e) => Err(e),
        Ok(Some(_)) => remount(title_id, app0, None),
    }
}

/// `keep_active`: keep a live overlay of the same backport (fresh cache). A full register
/// (after a build) remounts the stack, since a recreated game folder leaves the old lower nullfs.
fn mount_inner(title_id: &str, app0: &Path, backport: Option<&Path>, keep_active: bool) -> io::Result<()> {
    let src = app0.to_str().ok_or_else(|| err(format!("non-UTF-8 path {}", app0.display())))?;
    let dst = format!("{SYSTEM_EX_APP}/{title_id}");
    // SMP keeps an active overlay of the same backport (BACKPORT_OVERLAY_ACTIVE). Only folder
    // sources get one; only the fresh-cache path (keep_active) trusts the nullfs below it.
    let active = keep_active && match (backport.and_then(|b| b.to_str()), mount_source(&dst)) {
        (Some(b), Ok(Some(m))) => m.fstype == "unionfs" && m.from_is(b),
        _ => false,
    };
    if !active {
        mount_title_nullfs(title_id, src)?;
        if let Some(backport) = backport {
            mount_backport_overlay(title_id, backport)?;
        }
    }
    Ok(())
}

/// The home-screen tile: /user/app/KRKN00001/sce_sys/{param.json,icon0.png}, a web shortcut
/// whose deeplinkUri opens the payload's web UI in the console's browser (shape from
/// ps5-dump-forge tile.rs). Only sce_sys is written, as dump-forge does: no appmeta, no
/// /system_ex, no eboot.
const TILE_ID: &str = "KRKN00001";
/// "pending" while not yet registered, else the param.json hash of the last successful registration.
const TILE_MARKER: &str = "/data/krakenmount/tile.ok";
const TILE_ICON: &[u8] = include_bytes!("../ps5/icon0.png");
/// FW 12+ (no TitleDir): the param.json hash of a tile staged in /user/app, waiting for the scan
/// pass's `install_all` (InstallAll registers it with every other /user/app folder).
static TILE_STAGED: Mutex<Option<String>> = Mutex::new(None);

/// A staged tile waits for `install_all`: the scan pass calls it even with no game staged.
pub fn tile_staged() -> bool {
    TILE_STAGED.lock().unwrap().is_some()
}

fn tile_param_json(port: u16) -> Vec<u8> {
    let param = serde_json::json!({
        "applicationCategoryType": 65536,
        "titleId": TILE_ID,
        "localizedParameters": {
            "defaultLanguage": "en-US",
            "en-US": { "titleName": "Kraken Mount" },
        },
        "deeplinkUri": format!("http://127.0.0.1:{port}/"),
    });
    (serde_json::to_string_pretty(&param).expect("json") + "\n").into_bytes()
}

/// FNV-1a 64, hex.
pub(crate) fn fnv1a(bytes: &[u8]) -> String {
    let h = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{h:016x}")
}

/// Replaces `path` (temp file + rename) only when its bytes differ; true when written.
fn write_if_changed(path: &Path, bytes: &[u8]) -> io::Result<bool> {
    if fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(false);
    }
    let tmp = path.with_extension("krkn-tmp");
    ctx(fs::write(&tmp, bytes), || format!("write {}", tmp.display()))?;
    ctx(fs::rename(&tmp, path), || format!("rename -> {}", path.display()))?;
    Ok(true)
}

/// Rows of the tile in app.db's tbl_contentinfo (SMP's title list table); None when app.db
/// cannot be queried (logged).
fn tile_in_app_db() -> Option<bool> {
    const SQL: &str = "SELECT COUNT(*) FROM tbl_contentinfo WHERE titleId = ?1";
    let (path, sql, tid) = (CString::new(APP_DB).ok()?, CString::new(SQL).ok()?, CString::new(TILE_ID).ok()?);
    let mut msg = [0 as c_char; 256];
    let _guard = APP_DB_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let rows = unsafe { km_sqlite_exec1(path.as_ptr(), sql.as_ptr(), tid.as_ptr(), msg.as_mut_ptr(), msg.len()) };
    if rows < 0 {
        let e = unsafe { CStr::from_ptr(msg.as_ptr()) }.to_string_lossy();
        log!("tile {TILE_ID}: query {APP_DB} failed: {e}; trusting {TILE_MARKER}");
        return None;
    }
    Some(rows > 0)
}

/// Registers the tile when tile.ok lacks the current param.json hash, a file changed, or app.db
/// has no row for it (a wiped /data/krakenmount, or the system dropped it).
fn ensure_tile(port: u16) -> io::Result<String> {
    let app_dir = Path::new(APP_BASE).join(TILE_ID);
    let marker = fs::read_to_string(TILE_MARKER).ok().map(|s| s.trim().to_string());
    let sys = app_dir.join("sce_sys");
    ctx(fs::create_dir_all(&sys), || format!("mkdir {}", sys.display()))?;
    let param = tile_param_json(port);
    let hash = fnv1a(&param);
    let mut written = Vec::new();
    for (name, bytes) in [("param.json", &param[..]), ("icon0.png", TILE_ICON)] {
        if write_if_changed(&sys.join(name), bytes)? {
            written.push(name);
        }
    }
    // ponytail: app.db unreadable = trust the marker, so a FW 12+ start does not pay ~20 s for nothing.
    let in_db = tile_in_app_db();
    if written.is_empty() && marker.as_deref() == Some(hash.as_str()) && in_db != Some(false) {
        return Ok(format!("up to date (port {port})"));
    }
    if in_db == Some(false) {
        log!("tile {TILE_ID}: not in {APP_DB}; registering it");
    }
    ctx(fs::write(TILE_MARKER, "pending\n"), || format!("write {TILE_MARKER}"))?;
    if INSTALL_FN.get_or_init(resolve_install).is_err() {
        *TILE_STAGED.lock().unwrap() = Some(hash);
        return Ok(format!("staged for the next sceAppInstUtilAppInstallAll, wrote {written:?}"));
    }
    install_title_dir(TILE_ID)?;
    ctx(fs::write(TILE_MARKER, format!("{hash}\n")), || format!("write {TILE_MARKER}"))?;
    Ok(format!("registered, deeplink http://127.0.0.1:{port}/, wrote {written:?}"))
}

/// Installs or refreshes the home-screen tile for the web UI on `port`. Never fails the caller:
/// the outcome goes to the log, and a failed registration is retried on the next start.
pub fn register_tile(port: u16) {
    match ensure_tile(port) {
        Ok(what) => log!("tile {TILE_ID}: {what}"),
        Err(e) => log!("tile {TILE_ID}: not registered: {e}"),
    }
}
