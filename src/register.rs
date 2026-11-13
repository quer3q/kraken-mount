//! Registers a mounted game on the home screen. Port of ShadowMountPlus mount_and_install
//! (src/sm_install.c): stage sce_sys and appmeta, copy trophy metadata, nullfs-mount the game at
//! /system_ex/app/<TID>, then call sceAppInstUtilAppInstallTitleDir.
//!
//! SMP also writes /user/app/<TID>/mount.lnk, but only for its own remount/cleanup tracking;
//! nothing here reads it, so it is skipped.

use crate::sys::nmount;
use std::ffi::{c_char, c_void, CStr, CString};
use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;

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

/// Recursive copy, like SMP copy_dir: existing files are overwritten.
fn copy_dir(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let (s, d) = (e.path(), dst.join(e.file_name()));
        if e.file_type()?.is_dir() {
            copy_dir(&s, &d)?;
        } else {
            fs::copy(&s, &d)?;
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
fn copy_appmeta(src_sce_sys: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src_sce_sys)? {
        let e = e?;
        let name = e.file_name();
        let Some(name_str) = name.to_str() else { continue };
        if !is_appmeta_file(name_str) || !e.file_type()?.is_file() {
            continue;
        }
        fs::copy(e.path(), dst.join(&name))?;
    }
    Ok(())
}

/// SMP update_trophy_metadata: copy npbind.dat files and param.json to
/// /system_data/priv/appmeta/<TID> when present in the source and absent at the destination.
fn update_trophy_metadata(title_id: &str, src_sce_sys: &Path) -> io::Result<()> {
    let base = Path::new(TROPHY_BASE).join(title_id);
    fs::create_dir_all(base.join("trophy2"))?;
    fs::create_dir_all(base.join("uds"))?;
    for rel in ["trophy2/npbind.dat", "uds/npbind.dat", "param.json"] {
        let (s, d) = (src_sce_sys.join(rel), base.join(rel));
        if s.exists() && !d.exists() {
            ctx(fs::copy(&s, &d), || format!("copy {} -> {}", s.display(), d.display()))?;
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

/// SMP mount_title_nullfs: `src` must be a dir with eboot.bin or eboot2.bin; mkdir
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
    match fs::DirBuilder::new().mode(0o755).create(&dst) {
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
        let rc = unsafe { sceAppInstUtilInitialize() };
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
    let rc = unsafe { f(title_id.as_ptr()) } as u32;
    if rc == 0 { Ok(()) } else { Err(err(format!("sceAppInstUtilAppUnInstall({tid}): {rc:#x}"))) }
}

/// sceAppInstUtilAppInstallTitleDir(tid, "/user/app/", NULL); 0 and 0x80990002 are success.
fn install_title_dir(title_id: &str) -> io::Result<()> {
    let f = match INSTALL_FN.get_or_init(resolve_install) {
        Ok(f) => *f,
        Err(e) => {
            // dump-forge's last resort: registers every folder under /user/app, ours included.
            extern "C" {
                fn sceAppInstUtilAppInstallAll(reserved: *mut c_void) -> i32;
            }
            let rc = unsafe { sceAppInstUtilAppInstallAll(ptr::null_mut()) } as u32;
            log!("{title_id}: {e}; sceAppInstUtilAppInstallAll: {rc:#x}");
            return if rc == 0 { Ok(()) } else { Err(err(format!("sceAppInstUtilAppInstallAll: {rc:#x}"))) };
        }
    };
    let tid = CString::new(title_id).map_err(|_| err(format!("NUL in {title_id}")))?;
    let base = CString::new(format!("{APP_BASE}/")).expect("no NUL");
    let rc = unsafe { f(tid.as_ptr(), base.as_ptr(), ptr::null_mut()) } as u32;
    if rc == 0 || rc == ALREADY_INSTALLED {
        return Ok(());
    }
    Err(err(format!("sceAppInstUtilAppInstallTitleDir({title_id}): {rc:#x}")))
}

/// SMP mount_and_install: stage `sce_sys` into /user/app/<TID> (sce_sys + icon0.png) and
/// /user/appmeta/<TID> (param.*, *.png incl. pic0/pic1, *.dds, *.at9), trophy metadata, nullfs
/// `app0` at /system_ex/app/<TID>, the optional backport unionfs on top, then register.
pub fn register(title_id: &str, app0: &Path, sce_sys: &Path, backport: Option<&Path>) -> io::Result<()> {
    ctx(register_inner(title_id, app0, sce_sys, backport), || format!("register {title_id} ({})", app0.display()))
}

fn register_inner(title_id: &str, app0: &Path, sce_sys: &Path, backport: Option<&Path>) -> io::Result<()> {
    let app_dir = Path::new(APP_BASE).join(title_id);
    let user_sce_sys = app_dir.join("sce_sys");
    ctx(copy_dir(sce_sys, &user_sce_sys), || {
        format!("copy {} -> {}", sce_sys.display(), user_sce_sys.display())
    })?;
    let (icon_src, icon_dst) = (sce_sys.join("icon0.png"), app_dir.join("icon0.png"));
    ctx(fs::copy(&icon_src, &icon_dst), || {
        format!("copy {} -> {}", icon_src.display(), icon_dst.display())
    })?;
    let appmeta = Path::new(APPMETA_BASE).join(title_id);
    ctx(copy_appmeta(sce_sys, &appmeta), || format!("appmeta -> {}", appmeta.display()))?;
    ctx(update_trophy_metadata(title_id, sce_sys), || "trophy metadata".into())?;
    mount_inner(title_id, app0, backport, false)?;
    install_title_dir(title_id)
}

/// True when the title is on the home screen already: /user/app/<TID>/sce_sys/param.json,
/// /user/appmeta/<TID>/param.json, and the title id in app.db (SMP checks app.db + is_installed).
/// ponytail: app.db is searched for the TID bytes, no sqlite; a deleted row still in a free page
/// reads as registered (a rebuild re-registers). Unreadable app.db = not registered.
pub fn is_registered(title_id: &str) -> bool {
    if !Path::new(APP_BASE).join(title_id).join("sce_sys/param.json").is_file()
        || !Path::new(APPMETA_BASE).join(title_id).join("param.json").is_file()
    {
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

/// Only the nullfs `app0` -> /system_ex/app/<TID> (+ backport unionfs) of `register`, for a
/// title already registered; a no-op when it is mounted (it is gone after a reboot).
pub fn mount(title_id: &str, app0: &Path, backport: Option<&Path>) -> io::Result<()> {
    ctx(mount_inner(title_id, app0, backport, true), || format!("mount {title_id} ({})", app0.display()))
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
/// Marks /user/app/KRKN00001 as ours: "pending" while not yet registered, else the param.json
/// hash of the last successful registration.
const TILE_MARKER: &str = "/data/krakenmount/tile.ok";
const TILE_ICON: &[u8] = include_bytes!("../ps5/icon0.png");

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
fn fnv1a(bytes: &[u8]) -> String {
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

fn ensure_tile(port: u16) -> io::Result<String> {
    let app_dir = Path::new(APP_BASE).join(TILE_ID);
    let marker = fs::read_to_string(TILE_MARKER).ok().map(|s| s.trim().to_string());
    if marker.is_none() && fs::symlink_metadata(&app_dir).is_ok() {
        return Ok(format!("{} exists without {TILE_MARKER}; left alone", app_dir.display()));
    }
    if marker.is_none() {
        // Claim the folder before creating it, so a failed registration is retried.
        ctx(fs::write(TILE_MARKER, "pending\n"), || format!("write {TILE_MARKER}"))?;
    }
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
    if written.is_empty() && marker.as_deref() == Some(hash.as_str()) {
        return Ok(format!("up to date (port {port})"));
    }
    ctx(fs::write(TILE_MARKER, "pending\n"), || format!("write {TILE_MARKER}"))?;
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
