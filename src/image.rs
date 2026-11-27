//! Game images (DESIGN.md "Images"): a port of SMP sm_image.c / sm_mount_device.c, LVD backend
//! only. `.exfat` (exfatfs), `.ffpkg` (UFS), `.ffpfs` (PFS) and `.ffpfsc` (a PFS container holding
//! one more image, usually `pfs_image.dat`). The image is attached through /dev/lvdctl as
//! /dev/lvdN and mounted read-only on /mnt/krakenmount/<name>; the game inside is then an ordinary
//! folder source. /data/krakenmount/images/<mount name> records `<unit>\n<image path>` so a later
//! copy of the payload (no reboot) finds, reuses or releases its mounts. The scan reads each image
//! first (`probe`, ps5-dump-forge's readers): an image without a game is never attached.
mod probe;

pub use probe::Probe;

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::io::{self, Error, ErrorKind};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const MOUNT_BASE: &str = "/mnt/krakenmount";
const STATE_DIR: &str = "/data/krakenmount/images";
/// Mount names of `.ffpfsc` containers start with this: an image file inside one of them gets
/// SMP's nested inner profile.
const PFSC_PREFIX: &str = "pfsc_";

const LVD_CTRL: &str = "/dev/lvdctl";
const LVD_IOC_ATTACH_V0: libc::c_ulong = 0xC028_6D00;
const LVD_IOC_DETACH: libc::c_ulong = 0xC028_6D01;
const LVD_ENTRY_TYPE_FILE: u16 = 1;
const LVD_ENTRY_FLAG_NO_BITMAP: u16 = 0x1;
/// SMP LVD_ATTACH_FLAGS_SINGLE_RO: BASE | SINGLE_SAVE_PROFILE | RO_VARIANT.
const LVD_FLAGS_SINGLE_RO: u16 = 0x0004 | 0x0010 | 0x0008;
/// SMP LVD_ATTACH_FLAGS_DD_RO: SINGLE_RO | DOWNLOAD_DATA_PROFILE (legacy UFS).
const LVD_FLAGS_DD_RO: u16 = LVD_FLAGS_SINGLE_RO | 0x0002;
const LVD_TYPE_SINGLE: u16 = 0;
const LVD_TYPE_SAVE_DATA: u16 = 5;
const LVD_TYPE_DOWNLOAD_DATA: u16 = 7;
/// SMP pfs_lvd_game_image_type_from_selector(0x82) and (0x02).
const LVD_TYPE_AC_INNER: u16 = 8;
const LVD_TYPE_AC_OUTER: u16 = 9;
/// SMP LVD_SECONDARY_UNIT_IMAGE_IO.
const SECONDARY_UNIT: u32 = 0x10000;
const MNT_NOATIME: i32 = 0x1000_0000;
/// SMP PFS_ZERO_EKPFS_KEY_HEX.
const ZERO_EKPFS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fs {
    Exfat,
    Ufs,
    Pfs,
    /// The outer `.ffpfsc` container (mounted as pfs).
    Pfsc,
}

impl Fs {
    fn fstype(self) -> &'static str {
        match self {
            Fs::Exfat => "exfatfs",
            Fs::Ufs => "ufs",
            Fs::Pfs | Fs::Pfsc => "pfs",
        }
    }
}

#[repr(C)]
struct LvdLayer {
    source_type: u16,
    flags: u16,
    reserved0: u32,
    path: *const libc::c_char,
    offset: u64,
    size: u64,
    bitmap_path: *const libc::c_char,
    bitmap_offset: u64,
    bitmap_size: u64,
}

#[repr(C)]
struct LvdAttach {
    io_version: u32,
    device_id: i32,
    sector_size: u32,
    secondary_unit: u32,
    flags: u16,
    image_type: u16,
    layer_count: u32,
    device_size: u64,
    layers: *mut LvdLayer,
}

#[repr(C)]
struct LvdDetach {
    reserved0: u32,
    device_id: i32,
    reserved: [u8; 0x20],
}

const _: () = assert!(std::mem::size_of::<LvdAttach>() == 0x28);
const _: () = assert!(std::mem::size_of::<LvdDetach>() == 0x28);

/// The image format of a scan candidate, by extension (any case).
pub fn fs_of(path: &Path) -> Option<Fs> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "exfat" => Some(Fs::Exfat),
        "ffpkg" => Some(Fs::Ufs),
        "ffpfs" => Some(Fs::Pfs),
        "ffpfsc" => Some(Fs::Pfsc),
        _ => None,
    }
}

/// A `.exfat`, `.ffpkg`, `.ffpfs` or `.ffpfsc` file.
pub fn is_image(path: &Path) -> bool {
    fs_of(path).is_some()
}

fn fnv1a32(s: &[u8]) -> u32 {
    s.iter().fold(0x811c_9dc5u32, |h, &b| (h ^ b as u32).wrapping_mul(0x0100_0193))
}

/// /mnt/krakenmount/[pfsc_]<stem>_<hash>. ponytail: the stem is cut to 27 ASCII alnum/-/_
/// characters so the path fits statfs f_mntonname (88 bytes); the hash of the full path keeps it unique.
pub fn mount_point(image: &Path) -> PathBuf {
    let stem: String = image.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        .chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(27).collect();
    let hash = fnv1a32(image.as_os_str().as_encoded_bytes());
    let prefix = if fs_of(image) == Some(Fs::Pfsc) { PFSC_PREFIX } else { "" };
    Path::new(MOUNT_BASE).join(format!("{prefix}{stem}_{hash:08x}"))
}

/// The image mount point holding `path` (a game folder or an image inside an image), if any.
pub fn mount_of(path: &Path) -> Option<PathBuf> {
    let name = path.strip_prefix(MOUNT_BASE).ok()?.components().next()?;
    Some(Path::new(MOUNT_BASE).join(name))
}

/// Whether the game folder `dir` (a cache's stamp source) is in `image`'s mount, or for a `.ffpfsc`
/// in its inner image's mount. Paths only: true after the mounts are released too.
pub fn holds(image: &Path, dir: &Path) -> bool {
    let mp = mount_point(image);
    let inner = probe(image).ok().and_then(|p| p.inner).map(|name| mount_point(&mp.join(name)));
    mount_of(dir).is_some_and(|m| m == mp || inner.as_ref() == Some(&m))
}

/// An image file inside a mounted `.ffpfsc` container: SMP's nested inner profile.
fn nested_inner(image: &Path) -> bool {
    mount_of(image).is_some_and(|mp| mp.file_name().is_some_and(|n| n.to_string_lossy().starts_with(PFSC_PREFIX)))
}

fn state_file(mp: &Path) -> PathBuf {
    Path::new(STATE_DIR).join(mp.file_name().unwrap_or_default())
}

/// (unit, image path) recorded for the mount point `mp`.
fn recorded(mp: &Path) -> Option<(i32, PathBuf)> {
    let text = fs::read_to_string(state_file(mp)).ok()?;
    let (unit, image) = text.split_once('\n')?;
    Some((unit.trim().parse().ok()?, PathBuf::from(image)))
}

fn c_path(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_encoded_bytes()).map_err(|_| Error::from(ErrorKind::InvalidInput))
}

/// statfs of `mp` when it is a mount point: (fstype, from). Err for a dead mount (drive gone).
fn mounted_at(mp: &Path) -> io::Result<Option<(String, String)>> {
    let c = c_path(mp)?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        let e = Error::last_os_error();
        return if e.kind() == ErrorKind::NotFound { Ok(None) } else { Err(e) };
    }
    let s = |b: &[libc::c_char]| unsafe { std::ffi::CStr::from_ptr(b.as_ptr()) }.to_string_lossy().into_owned();
    if s(&st.f_mntonname) != mp.to_string_lossy() {
        return Ok(None);
    }
    Ok(Some((s(&st.f_fstypename), s(&st.f_mntfromname))))
}

/// Reads `image` without attaching it (see `probe`): its game folder and param.json.
pub fn probe(image: &Path) -> io::Result<Probe> {
    let fs = fs_of(image).ok_or_else(|| Error::new(ErrorKind::InvalidInput, "not an image"))?;
    let meta = fs::metadata(image)?;
    probe::probe(image, fs, meta.len(), meta.mtime())
}

/// The format of the image inside a `.ffpfsc`, by its name: `pfs_image.dat` (no known extension)
/// is PFS, as in SMP; `.ufs2` is UFS (ps5-dump-forge's `open_ffpfsc` accepts it too).
fn inner_fs(name: &str) -> Fs {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".ufs2") { Fs::Ufs } else { fs_of(Path::new(&lower)).filter(|f| *f != Fs::Pfsc).unwrap_or(Fs::Pfs) }
}

/// The game folder `ensure` returns for `image`, from the paths alone (nothing is attached).
pub fn dir_of(image: &Path, p: &Probe) -> PathBuf {
    let mp = mount_point(image);
    let mp = match &p.inner {
        Some(name) => mount_point(&mp.join(name)),
        None => mp,
    };
    if p.game.is_empty() { mp } else { mp.join(&p.game) }
}

type Key = (PathBuf, u64, i64);

/// Images whose mount failed, per (path, size, mtime): not attached again every scan pass (a
/// `.ffpfsc` whose inner image does not mount had its container attached and released each pass).
/// A changed file tries again at once, the same file after RETRY_FAILED (a busy LVD unit, a drive
/// replugged mid-attach).
static FAILED: OnceLock<Mutex<HashMap<Key, (Instant, ErrorKind, String)>>> = OnceLock::new();
const RETRY_FAILED: Duration = Duration::from_secs(10 * 60);

/// `ensure`, once per version of the file while it fails (FAILED).
pub fn ensure(image: &Path, p: &Probe) -> io::Result<PathBuf> {
    let meta = fs::metadata(image)?;
    let key = (image.to_path_buf(), meta.len(), meta.mtime());
    let failed = FAILED.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((at, k, m)) = failed.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        if at.elapsed() < RETRY_FAILED {
            return Err(Error::new(*k, m.clone()));
        }
    }
    let r = ensure_mounted(image, p);
    let mut failed = failed.lock().unwrap_or_else(|e| e.into_inner());
    failed.retain(|k, _| k.0 != key.0);
    if let Err(e) = &r {
        // What the failed attempt left (the container of a `.ffpfsc`) is released now, not by
        // release_unused: the error keeps no mount.
        if let Err(r) = release_tree(&mount_point(image), &[]) {
            log!("{}: release: {r}", image.display());
        }
        failed.insert(key, (Instant::now(), e.kind(), e.to_string()));
    }
    r
}

/// Mounts `image` (read by `probe` as `p`) when it is not mounted yet and returns its game folder.
/// An existing mount of this image (this run or an earlier copy of the payload) is reused; a dead
/// one (its drive was unplugged) is released first. A `.ffpfsc` mounts its inner image too.
fn ensure_mounted(image: &Path, p: &Probe) -> io::Result<PathBuf> {
    let fs = fs_of(image).ok_or_else(|| Error::new(ErrorKind::InvalidInput, "not an image"))?;
    let mp = mount_as(image, fs)?;
    let (mp, fs) = match &p.inner {
        Some(name) => {
            let inner = mp.join(name);
            let inner_fs = inner_fs(name);
            (mount_as(&inner, inner_fs)?, inner_fs)
        }
        None => (mp, fs),
    };
    let dir = if p.game.is_empty() { mp.clone() } else { mp.join(&p.game) };
    if dir.join("eboot.bin").is_file() && dir.join("sce_sys/param.json").is_file() {
        return Ok(dir);
    }
    // The reader and the mounted filesystem disagree: nothing of it is kept (ensure releases it).
    Err(Error::new(ErrorKind::InvalidData, format!("{} ({fs:?}): no eboot.bin + sce_sys/param.json in {}", image.display(), dir.display())))
}

/// The mount of `image` as `fs`, mounted when missing (see `ensure`).
fn mount_as(image: &Path, fs: Fs) -> io::Result<PathBuf> {
    let size = fs::metadata(image)?.len();
    let mp = mount_point(image);
    match mounted_at(&mp) {
        Ok(Some((_, from))) if from.starts_with("/dev/lvd") && recorded(&mp).is_some_and(|(_, i)| i == image) => {}
        Ok(None) => mount(image, fs, size, &mp)?,
        Ok(Some((fstype, from))) => {
            log!("{}: {} is a {fstype} of {from}, not ours; releasing it", image.display(), mp.display());
            release(&mp)?;
            mount(image, fs, size, &mp)?;
        }
        Err(e) => {
            log!("{}: {}: {e}; releasing the dead mount", image.display(), mp.display());
            release_tree(&mp, &[])?;
            mount(image, fs, size, &mp)?;
        }
    }
    Ok(mp)
}

/// LVD and nmount parameters, SMP get_image_mount_profile and the helpers after it.
struct Profile {
    legacy: bool,
    nested: bool,
    sector: u32,
    secondary: u32,
    flags: u16,
    image_type: u16,
    mnt_flags: i32,
}

fn profile(image: &Path, fs: Fs) -> Profile {
    let inner = fs == Fs::Pfs && nested_inner(image);
    let nested = fs == Fs::Pfsc || inner;
    let optimized_source = image.starts_with("/data") || image.starts_with("/user");
    let legacy = nested || !optimized_source;
    // SMP get_lvd_sector_size: a backing filesystem block smaller than the default is used.
    let default_sector = if fs == Fs::Exfat { 512 } else { 4096 };
    let sector = if nested {
        default_sector
    } else {
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        let bsize = match c_path(image) {
            Ok(c) if unsafe { libc::statfs(c.as_ptr(), &mut st) } == 0 => {
                if st.f_bsize != 0 { st.f_bsize as u64 } else { st.f_iosize as u64 }
            }
            _ => 0,
        };
        if bsize == 0 || bsize >= default_sector as u64 { default_sector } else { bsize as u32 }
    };
    let secondary = if (legacy && fs != Fs::Exfat) || SECONDARY_UNIT % sector != 0 { sector } else { SECONDARY_UNIT };
    let legacy_ufs = legacy && fs == Fs::Ufs;
    let image_type = match fs {
        Fs::Pfsc => LVD_TYPE_AC_OUTER,
        Fs::Pfs if inner => LVD_TYPE_AC_INNER,
        Fs::Pfs => LVD_TYPE_SAVE_DATA,
        Fs::Ufs if legacy => LVD_TYPE_DOWNLOAD_DATA,
        Fs::Exfat if legacy => LVD_TYPE_SINGLE,
        Fs::Ufs | Fs::Exfat => LVD_TYPE_SAVE_DATA,
    };
    Profile {
        legacy, nested, sector, secondary,
        flags: if legacy_ufs { LVD_FLAGS_DD_RO } else { LVD_FLAGS_SINGLE_RO },
        image_type,
        mnt_flags: libc::MNT_RDONLY as i32 | if legacy_ufs { MNT_NOATIME } else { 0 },
    }
}

fn attach(image: &Path, size: u64, p: &Profile) -> io::Result<i32> {
    let path = c_path(image)?;
    let ctl = CString::new(LVD_CTRL).unwrap();
    let fd = unsafe { libc::open(ctl.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        let e = Error::last_os_error();
        return Err(Error::new(e.kind(), format!("open {LVD_CTRL}: {e}")));
    }
    let mut layer = LvdLayer {
        source_type: LVD_ENTRY_TYPE_FILE, flags: LVD_ENTRY_FLAG_NO_BITMAP, reserved0: 0,
        path: path.as_ptr(), offset: 0, size, bitmap_path: std::ptr::null(), bitmap_offset: 0, bitmap_size: 0,
    };
    let mut req = LvdAttach {
        io_version: 0, device_id: -1, sector_size: p.sector, secondary_unit: p.secondary,
        flags: p.flags, image_type: p.image_type, layer_count: 1, device_size: size, layers: &mut layer,
    };
    let rc = unsafe { libc::ioctl(fd, LVD_IOC_ATTACH_V0, &mut req as *mut LvdAttach) };
    let e = Error::last_os_error();
    unsafe { libc::close(fd) };
    if rc != 0 {
        return Err(Error::new(e.kind(), format!("lvd attach: {e}")));
    }
    if req.device_id < 0 {
        return Err(Error::other(format!("lvd attach returned unit {}", req.device_id)));
    }
    Ok(req.device_id)
}

fn detach(unit: i32) -> io::Result<()> {
    let ctl = CString::new(LVD_CTRL).unwrap();
    let fd = unsafe { libc::open(ctl.as_ptr(), libc::O_RDWR) };
    if fd < 0 {
        return Err(Error::last_os_error());
    }
    let mut req = LvdDetach { reserved0: 0, device_id: unit, reserved: [0; 0x20] };
    let rc = unsafe { libc::ioctl(fd, LVD_IOC_DETACH, &mut req as *mut LvdDetach) };
    let e = Error::last_os_error();
    unsafe { libc::close(fd) };
    if rc != 0 { Err(e) } else { Ok(()) }
}

fn mount(image: &Path, fs: Fs, size: u64, mp: &Path) -> io::Result<()> {
    fs::create_dir_all(mp)?;
    fs::create_dir_all(STATE_DIR)?;
    let p = profile(image, fs);
    let unit = attach(image, size, &p)?;
    let dev = format!("/dev/lvd{unit}");
    // SMP LVD_NODE_WAIT_RETRIES x LVD_NODE_WAIT_US.
    let mut node = false;
    for _ in 0..100 {
        if Path::new(&dev).exists() {
            node = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let fail = |e: Error| {
        if let Err(d) = detach(unit) {
            log!("{}: detach {dev}: {d}", image.display());
        }
        e
    };
    if !node {
        return Err(fail(Error::new(ErrorKind::TimedOut, format!("{dev} did not appear"))));
    }
    let fspath = mp.to_string_lossy();
    // SMP perform_image_nmount iov_ufs / iov_exfat / iov_pfs / iov_nested_pfs (without errmsg/force).
    // The nested PFS profile differs only in `disc` (selector bit 0x40: 0 for both 0x02 and 0x82).
    let opts: Vec<(&str, Option<&str>)> = match fs {
        Fs::Ufs => vec![
            ("fstype", Some("ufs")), ("from", Some(&dev)), ("fspath", Some(&fspath)),
            ("budgetid", Some("game")), ("async", None), ("noatime", None), ("automounted", None),
        ],
        Fs::Exfat => vec![
            ("from", Some(&dev)), ("fspath", Some(&fspath)), ("fstype", Some("exfatfs")),
            ("budgetid", Some("game")), ("large", Some("yes")), ("timezone", Some("static")),
            ("async", None), ("noatime", None), ("ignoreacl", None), ("automounted", None),
        ],
        Fs::Pfs | Fs::Pfsc => vec![
            ("from", Some(&dev)), ("fspath", Some(&fspath)), ("fstype", Some("pfs")),
            ("sigverify", Some("0")), ("mkeymode", Some("AC")), ("budgetid", Some("game")),
            ("playgo", Some("0")), ("disc", Some("0")), ("ekpfs", Some(ZERO_EKPFS)),
            ("async", None), ("noatime", None), ("automounted", None),
        ],
    };
    if let Err(e) = crate::sys::nmount_opt(&opts, p.mnt_flags) {
        return Err(fail(Error::new(e.kind(), format!("nmount {} {dev} -> {fspath}: {e}", fs.fstype()))));
    }
    match mounted_at(mp) {
        Ok(Some((_, from))) if from == dev => {}
        other => {
            let _ = crate::sys::unmount(&fspath, 0);
            return Err(fail(Error::other(format!("{fspath}: not the expected mount of {dev}: {other:?}"))));
        }
    }
    fs::write(state_file(mp), format!("{unit}\n{}", image.display()))?;
    log!("{}: mounted {dev} on {fspath} ({}, read-only, lvd type {} sector {} unit {:#x}{}{})",
        image.display(), fs.fstype(), p.image_type, p.sector, p.secondary,
        if p.legacy { ", legacy" } else { "" }, if p.nested { ", nested" } else { "" });
    Ok(())
}

/// Unmounts `mp` (never MNT_FORCE: EBUSY = a title, a running game or an inner image still uses
/// it), detaches its LVD unit and forgets it. The unit is the live mount's /dev/lvdN, or the recorded
/// one for a dead mount (still in the table, so attached this boot). Nothing mounted at `mp`: the
/// record may be from before a reboot, its unit now another image's or the system's: not detached.
/// ponytail: a unit whose mount went without its detach (a failed detach, the payload killed in
/// between, an unmount from elsewhere) stays attached until reboot: its number alone cannot tell.
pub fn release(mp: &Path) -> io::Result<()> {
    let unit = match mounted_at(mp) {
        Ok(Some((_, from))) => from.strip_prefix("/dev/lvd").and_then(|u| u.parse().ok()),
        Ok(None) => None,
        Err(_) => recorded(mp).map(|(u, _)| u),
    };
    let fspath = mp.to_string_lossy();
    match crate::sys::unmount(&fspath, 0) {
        Ok(()) => {}
        // EINVAL: nothing mounted there; ENOENT: no such dir.
        Err(e) if matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOENT)) => {}
        Err(e) => return Err(Error::new(e.kind(), format!("unmount {fspath}: {e}"))),
    }
    if let Some(unit) = unit {
        // ENXIO/ENOENT/EINVAL: the unit is already gone.
        if let Err(e) = detach(unit) {
            if !matches!(e.raw_os_error(), Some(libc::ENXIO | libc::ENOENT | libc::EINVAL)) {
                log!("{fspath}: detach /dev/lvd{unit}: {e}");
            }
        }
    }
    let _ = fs::remove_file(state_file(mp));
    let _ = fs::remove_dir(mp);
    log!("{fspath}: released");
    Ok(())
}

/// Our image mounts: (mount point, image path) from the state dir.
pub fn mounts() -> Vec<(PathBuf, PathBuf)> {
    let Ok(rd) = fs::read_dir(STATE_DIR) else { return Vec::new() };
    rd.flatten().filter_map(|e| {
        let mp = Path::new(MOUNT_BASE).join(e.file_name());
        recorded(&mp).map(|(_, image)| (mp, image))
    }).collect()
}

/// Releases `mp` with everything on top of it: the images inside it (a `.ffpfsc`'s inner image),
/// then the titles (/system_ex/app/<TID>) mounted from it, then `mp` itself. `table` is the mount
/// table (web::mount_table); an empty one unmounts no title.
pub fn release_tree(mp: &Path, table: &[(String, String, u64)]) -> io::Result<()> {
    for (child, image) in mounts() {
        if child != mp && image.starts_with(mp) {
            release_tree(&child, table)?;
        }
    }
    let prefix = format!("{}/", mp.display());
    let mut tids: Vec<&str> = table.iter().filter_map(|(on, from, _)| {
        let tid = on.strip_prefix("/system_ex/app/").filter(|t| crate::scan::valid_tid(t))?;
        let from = from.strip_prefix("<above>:").unwrap_or(from);
        (from == mp.to_string_lossy() || from.starts_with(&prefix)).then_some(tid)
    }).collect();
    tids.sort_unstable();
    tids.dedup();
    for tid in tids {
        crate::register::unmount_title(tid)?;
    }
    release(mp)
}

/// After a scan: releases every image mount not in `keep` (its image is gone, moved, or no longer
/// a source), with what is on top of it. A failure (busy) is logged and retried next pass. A mount
/// under a `running` title's /system_ex/app stack (or holding such an inner image) is left alone:
/// a running homebrew's title is never touched.
pub fn release_unused(keep: &HashSet<PathBuf>, running: &[String]) {
    let all = mounts();
    // A kept inner image keeps its container (a game folder with a scan error names only the inner).
    let mut keep = keep.clone();
    loop {
        let parents: Vec<PathBuf> = all.iter().filter(|(mp, _)| keep.contains(mp))
            .filter_map(|(_, image)| mount_of(image)).filter(|p| !keep.contains(p)).collect();
        if parents.is_empty() {
            break;
        }
        keep.extend(parents);
    }
    let unused: Vec<PathBuf> = all.iter().map(|(mp, _)| mp.clone()).filter(|mp| !keep.contains(mp)).collect();
    if unused.is_empty() {
        return;
    }
    let table = match crate::web::mount_table() {
        Ok(t) => t,
        Err(e) => {
            log!("images: getfsstat: {e}");
            return;
        }
    };
    let runs_on = |mp: &Path| {
        let prefix = format!("{}/", mp.display());
        table.iter().any(|(on, from, _)| {
            let from = from.strip_prefix("<above>:").unwrap_or(from);
            on.strip_prefix("/system_ex/app/").is_some_and(|t| running.iter().any(|r| r == t))
                && (from == mp.to_string_lossy() || from.starts_with(&prefix))
        })
    };
    for mp in unused {
        // Already gone as the inner image of a container released before it.
        if !state_file(&mp).exists() {
            continue;
        }
        if runs_on(&mp) || all.iter().any(|(child, image)| image.starts_with(&mp) && runs_on(child)) {
            continue;
        }
        if let Err(e) = release_tree(&mp, &table) {
            log!("{}: release: {e}", mp.display());
        }
    }
}
