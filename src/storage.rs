//! Drives (internal, USB, extended) and default scan roots (DESIGN.md).
use std::ffi::CString;
use std::path::Path;

pub struct Drive {
    pub mount: String,
    pub label: String,
    /// "internal" | "usb" | "ext"
    pub kind: &'static str,
    pub total: u64,
    pub free: u64,
}

const USB: usize = 8;
const EXT: usize = 2;

fn statfs(path: &str) -> Option<libc::statfs> {
    let p = CString::new(path).ok()?;
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statfs(p.as_ptr(), &mut fs) } == 0).then_some(fs)
}

fn on_name(fs: &libc::statfs) -> String {
    let b: Vec<u8> = fs.f_mntonname.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&b).into_owned()
}

/// A statfs that reports `path` itself as the mount point (an empty /mnt/usbN reports /mnt).
fn mounted(path: &str) -> Option<libc::statfs> {
    statfs(path).filter(|fs| on_name(fs) == path)
}

pub fn drives() -> Vec<Drive> {
    let mk = |mount: String, label: String, kind, fs: Option<libc::statfs>| {
        let (total, free) = fs.map_or((0, 0), |f| {
            (f.f_blocks as u64 * f.f_bsize as u64, f.f_bavail.max(0) as u64 * f.f_bsize as u64)
        });
        Drive { mount, label, kind, total, free }
    };
    let mut v = vec![mk("/data".into(), "Internal".into(), "internal", statfs("/data"))];
    for i in 0..USB {
        let m = format!("/mnt/usb{i}");
        if let Some(fs) = mounted(&m) {
            v.push(mk(m, format!("USB {}", i + 1), "usb", Some(fs)));
        }
    }
    for i in 0..EXT {
        let m = format!("/mnt/ext{i}");
        if let Some(fs) = mounted(&m) {
            v.push(mk(m, format!("Extended {}", i + 1), "ext", Some(fs)));
        }
    }
    v
}

pub fn drive_of(path: &Path) -> String {
    let p = path.to_string_lossy();
    mounted_aliases()
        .into_iter()
        .filter(|m| p == m.as_str() || p.starts_with(&format!("{m}/")))
        .max_by_key(|m| m.len())
        .unwrap_or_else(|| "/data".into())
}

/// SMP SM_DEFAULT_SCAN_PATHS_INITIALIZER (include/sm_paths.h) order, present drives only.
pub fn default_scan_roots() -> Vec<String> {
    let present = mounted_aliases();
    let ext: Vec<String> = (0..EXT).map(|i| format!("/mnt/ext{i}")).collect();
    let usb: Vec<String> = (0..USB).map(|i| format!("/mnt/usb{i}")).collect();
    let mut all = vec!["/data/homebrew".to_string(), "/data/etaHEN/games".to_string()];
    for m in &ext {
        all.push(format!("{m}/homebrew"));
        all.push(format!("{m}/etaHEN/games"));
    }
    all.extend(usb.iter().map(|m| format!("{m}/homebrew")));
    all.extend(usb.iter().map(|m| format!("{m}/etaHEN/games")));
    all.extend(usb.iter().cloned());
    all.extend(ext.iter().cloned());
    all.retain(|r| r.starts_with("/data/") || present.iter().any(|m| r == m || r.starts_with(&format!("{m}/"))));
    all
}

pub fn mounted_aliases() -> Vec<String> {
    (0..USB)
        .map(|i| format!("/mnt/usb{i}"))
        .chain((0..EXT).map(|i| format!("/mnt/ext{i}")))
        .filter(|m| mounted(m).is_some())
        .collect()
}
