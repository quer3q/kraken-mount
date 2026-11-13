//! Thin wrappers over the PS5 kernel: nmount/unmount, notifications, and the log.

use std::ffi::CString;
use std::io::{self, Write};

pub const LOG_PATH: &str = "/data/krakenmount/kraken.log";

/// Prints a line to stdout and appends it to /data/krakenmount/kraken.log.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::sys::log_line(&format!($($arg)*))
    };
}

/// kraken.log starts over once it passes this size (no kept copy).
const LOG_MAX: u64 = 1 << 20;
/// Settings `log`: false = no kraken.log writes (stdout still gets every line). Set by main on each
/// config load; true until the first one, so start-up problems are logged.
pub static LOG_FILE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn log_line(line: &str) {
    // Serialized, so two threads never rotate at once. A poisoned lock still logs.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let open = || std::fs::OpenOptions::new().create(true).append(true).open(LOG_PATH);
    let on = LOG_FILE.load(std::sync::atomic::Ordering::Relaxed);
    let mut f = if on { open() } else { Err(io::ErrorKind::Unsupported.into()) };
    if f.as_ref().is_ok_and(|f| f.metadata().is_ok_and(|m| m.len() > LOG_MAX)) {
        f = std::fs::File::create(LOG_PATH);
    }
    if let Ok(mut f) = f {
        let _ = writeln!(f, "{line}");
    }
    drop(guard);
    // stdout is elfldr's socket; once the sender hangs up a write fails. println! would panic
    // (panic=abort ends the payload), so the error is ignored.
    let _ = writeln!(io::stdout().lock(), "{line}");
}

fn cstr(s: &str) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in string"))
}

/// nmount(2) with name/value option pairs. Each string is passed NUL-terminated; an empty value
/// is still passed as "\0" (a flag option such as "rw").
pub fn nmount(opts: &[(&str, &str)], flags: i32) -> io::Result<()> {
    let mut strings = Vec::with_capacity(opts.len() * 2);
    for (k, v) in opts {
        strings.push(cstr(k)?);
        strings.push(cstr(v)?);
    }
    let mut iov: Vec<libc::iovec> = strings
        .iter()
        .map(|s| libc::iovec {
            iov_base: s.as_ptr() as *mut libc::c_void,
            iov_len: s.as_bytes_with_nul().len(),
        })
        .collect();
    let rc = unsafe { libc::nmount(iov.as_mut_ptr(), iov.len() as libc::c_uint, flags) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// nmount(2) like SMP's IOVEC_ENTRY: a None value is passed as (NULL, 0), the form SMP uses for
/// flag options such as "notime" and "fnodup".
pub fn nmount_opt(opts: &[(&str, Option<&str>)], flags: i32) -> io::Result<()> {
    let mut strings = Vec::with_capacity(opts.len() * 2);
    for (k, v) in opts {
        strings.push(Some(cstr(k)?));
        strings.push(v.map(cstr).transpose()?);
    }
    let mut iov: Vec<libc::iovec> = strings
        .iter()
        .map(|s| match s {
            Some(s) => libc::iovec {
                iov_base: s.as_ptr() as *mut libc::c_void,
                iov_len: s.as_bytes_with_nul().len(),
            },
            None => libc::iovec { iov_base: std::ptr::null_mut(), iov_len: 0 },
        })
        .collect();
    let rc = unsafe { libc::nmount(iov.as_mut_ptr(), iov.len() as libc::c_uint, flags) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn unmount(path: &str, flags: i32) -> io::Result<()> {
    let p = cstr(path)?;
    if unsafe { libc::unmount(p.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The SDK / ShadowMountPlus notify_request: 45 reserved bytes, then the message (0xC30 total).
#[repr(C)]
struct NotifyRequest {
    unused: [u8; 45],
    message: [u8; 3075],
}
const _: () = assert!(std::mem::size_of::<NotifyRequest>() == 0xC30);

extern "C" {
    fn sceKernelSendNotificationRequest(
        device: i32,
        req: *mut NotifyRequest,
        size: usize,
        blocking: i32,
    ) -> i32;
}

/// The toast's icon (the tile's), where the notification overlay reads it (SMP and dump-forge
/// keep theirs under /user/data too).
const TOAST_ICON: &str = "/user/data/krakenmount/notify-icon.png";
const ICON: &[u8] = include_bytes!("../ps5/icon0.png");

extern "C" {
    /// libSceNotification: a notification from its JSON payload.
    fn sceNotificationSend(user_id: i32, logged_in: bool, payload: *const std::ffi::c_char) -> i32;
}

/// Writes ICON to TOAST_ICON unless it is there already; false if it can't.
fn toast_icon() -> bool {
    if std::fs::read(TOAST_ICON).is_ok_and(|b| b == ICON) {
        return true;
    }
    let tmp = format!("{TOAST_ICON}.{}.tmp", std::process::id());
    let ok = std::fs::create_dir_all("/user/data/krakenmount").is_ok()
        && std::fs::write(&tmp, ICON).is_ok()
        && std::fs::rename(&tmp, TOAST_ICON).is_ok();
    if !ok {
        let _ = std::fs::remove_file(&tmp);
    }
    ok
}

/// A Unix time as `2000-01-01T12:00:00.000Z` (Howard Hinnant's days-to-civil).
fn utc_time(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let z = days as i64 + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let (h, m, s) = (rem / 3600, rem / 60 % 60, rem % 60);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.000Z")
}

/// The ShadowMountPlus toast shape (its TOASTS.md, via dump-forge notice.rs):
/// InteractiveToastTemplateB with Kraken Mount's icon, no action.
fn toast_json(msg: &str, now: u64) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let id = format!("{now}{:03}", SEQ.fetch_add(1, Ordering::Relaxed) % 1000);
    serde_json::json!({
        "rawData": {
            "viewTemplateType": "InteractiveToastTemplateB",
            "channelType": "ServiceFeedback",
            "bundleName": "kraken-mount",
            "useCaseId": "IDC",
            "soundEffect": "none",
            "toastOverwriteType": "No",
            "isImmediate": true,
            "priority": 100,
            "viewData": {
                "icon": { "type": "Url", "parameters": { "url": TOAST_ICON } },
                "message": { "body": msg },
                "subMessage": { "body": "Kraken Mount" },
            },
            "platformViews": {
                "previewDisabled": {
                    "viewData": {
                        "icon": { "type": "Predefined", "parameters": { "icon": "community" } },
                        "message": { "body": msg },
                    }
                }
            },
        },
        "createdDateTime": utc_time(now),
        "localNotificationId": id,
    })
    .to_string()
}

/// A system toast with `msg` (sceNotificationSend to every logged-in user, 0xFE); when the
/// console refuses it, the old notify_request (cut at 3,074 bytes). Also logged.
pub fn notify(msg: &str) {
    log_line(msg);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    if toast_icon() {
        if let Ok(json) = CString::new(toast_json(msg, now)) {
            if unsafe { sceNotificationSend(0xFE, true, json.as_ptr()) } == 0 {
                return;
            }
        }
    }
    let mut req = NotifyRequest { unused: [0; 45], message: [0; 3075] };
    let n = msg.len().min(req.message.len() - 1);
    req.message[..n].copy_from_slice(&msg.as_bytes()[..n]);
    unsafe {
        sceKernelSendNotificationRequest(0, &mut req, std::mem::size_of::<NotifyRequest>(), 0);
    }
}
