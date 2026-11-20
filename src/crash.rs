//! Crash watcher: when an app crashes, the PS5 writes /user/devlog/system/sce_coredumps.0/<TID>_<n>/
//! (the dump `prosperocore-<n>-0x<pid>-<process>.prosperodmp` inside) and deletes it seconds later.
//! For our titles this records the last crash, so the web UI can tell a game crash from a Kraken
//! Mount problem. Read-only, no klog needed. See DESIGN.md "Crash watcher".
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DIR: &str = "/user/devlog/system/sce_coredumps.0";
/// The dump file can appear just after its folder: looked for this long.
const DUMP_WAIT_MS: u64 = 2000;
/// Without a vnode watch: the folder lives a few seconds, so this still sees it.
const POLL_MS: u64 = 500;

#[derive(Clone)]
pub struct Crash {
    /// Unix seconds.
    pub at: u64,
    /// From the dump's file name ("eboot.bin"); None when the dump did not show up in time.
    pub process: Option<String>,
    /// The console's crash folder name ("PPSA22327_1795166239").
    pub folder: String,
}

/// "PPSA22327_1795166239" -> "PPSA22327".
pub fn folder_tid(name: &str) -> Option<&str> {
    let (tid, n) = name.split_once('_')?;
    let digits = !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    (digits && crate::scan::valid_tid(tid)).then_some(tid)
}

/// "prosperocore-1795166239-0x000000e9-eboot.bin.prosperodmp" -> "eboot.bin".
pub fn dump_process(file: &str) -> Option<&str> {
    let rest = file.strip_prefix("prosperocore-")?.strip_suffix(".prosperodmp")?;
    let mut parts = rest.splitn(3, '-');
    let (_, _, process) = (parts.next()?, parts.next()?, parts.next()?);
    (!process.is_empty()).then_some(process)
}

fn list(dir: &str) -> io::Result<HashSet<String>> {
    Ok(std::fs::read_dir(dir)?.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
}

fn find_process(folder: &str) -> Option<String> {
    let path = format!("{DIR}/{folder}");
    for _ in 0..DUMP_WAIT_MS / 100 {
        let found = list(&path).ok()?.iter().find_map(|f| dump_process(f).map(str::to_string));
        if found.is_some() {
            return found;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn record(name: &str, titles: &Mutex<HashSet<String>>, crashes: &Mutex<HashMap<String, Crash>>) {
    let Some(tid) = folder_tid(name) else { return };
    if !titles.lock().unwrap().contains(tid) {
        return;
    }
    let process = find_process(name);
    log!("{tid}: crashed ({}); the console's crash dump: {DIR}/{name}", process.as_deref().unwrap_or("process unknown"));
    let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    crashes.lock().unwrap().insert(tid.to_string(), Crash { at, process, folder: name.to_string() });
}

/// A vnode watch for writes to DIR (a folder added), or None to poll.
fn vnode_watch() -> Option<i32> {
    let kq = unsafe { libc::kqueue() };
    let path = CString::new(DIR).unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
    if kq < 0 || fd < 0 {
        return None;
    }
    let mut k: libc::kevent = unsafe { std::mem::zeroed() };
    k.ident = fd as usize;
    k.filter = libc::EVFILT_VNODE;
    k.flags = libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR;
    k.fflags = libc::NOTE_WRITE as u32;
    let ok = unsafe { libc::kevent(kq, &k, 1, std::ptr::null_mut(), 0, std::ptr::null()) } == 0;
    ok.then_some(kq)
}

/// Blocking; run on its own thread. Records the crashes of the titles in `titles`.
pub fn watch(titles: Arc<Mutex<HashSet<String>>>, crashes: Arc<Mutex<HashMap<String, Crash>>>) {
    let Ok(mut known) = list(DIR) else {
        log!("crash watcher: {DIR} not found; game crashes are not reported");
        return;
    };
    let mut kq = vnode_watch();
    log!("crash watcher: watching {DIR} ({})", if kq.is_some() { "EVFILT_VNODE" } else { "polling" });
    loop {
        match kq {
            Some(q) => {
                let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
                let n = unsafe { libc::kevent(q, std::ptr::null(), 0, &mut ev, 1, std::ptr::null()) };
                if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    log!("crash watcher: kevent failed: {}; polling", io::Error::last_os_error());
                    kq = None;
                }
            }
            None => std::thread::sleep(Duration::from_millis(POLL_MS)),
        }
        let Ok(now) = list(DIR) else { continue };
        for name in now.difference(&known) {
            record(name, &titles, &crashes);
        }
        known = now;
    }
}
