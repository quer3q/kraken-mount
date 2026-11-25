//! Installs packages through the console installer (Install tab). See DESIGN.md "Install".
//! The native layer is `ps5/installer.c`: every sceAppInstUtil call there takes one lock, which
//! `register.rs` shares through `AppInst`.

use std::collections::VecDeque;
use std::ffi::{c_char, c_int, CStr, CString};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::scan::{self, Kind, PkgKind, Source};
use crate::web::State;

extern "C" {
    fn km_appinst_begin();
    fn km_appinst_end();
    fn km_pkg_install(url: *const c_char, name: *const c_char, content_id: *mut c_char,
        task_type: *mut c_int, code: *mut u32, note: *mut c_char, note_len: usize) -> c_int;
    fn km_pkg_status(content_id: *const c_char, patch: c_int, status: *mut c_char,
        downloaded: *mut u64, total: *mut u64, error_code: *mut c_int) -> c_int;
    fn km_addcont_installed(content_id: *const c_char, state: *mut c_int) -> c_int;
    fn km_pkg_cancel(content_id: *const c_char) -> c_int;
}

/// SCE kernel EINVAL: a string with a NUL never reaches the installer.
const BAD_ARG: u32 = 0x8002_0016;

/// Holds the AppInstUtil lock (installer.c `km_appinst_begin/end`) while alive: register.rs's
/// sceAppInstUtil calls run under it, so they never overlap an install's. Not reentrant: never
/// call a `km_*` wrapper below while holding one.
pub struct AppInst(());

/// Threads in or waiting for a long sceAppInstUtil call: an AppInst holder (register's
/// InstallAll, about 20 s) or `submit`. `appinst_busy` reads it.
static LONG: AtomicUsize = AtomicUsize::new(0);

impl AppInst {
    pub fn lock() -> AppInst {
        LONG.fetch_add(1, Ordering::SeqCst);
        unsafe { km_appinst_begin() };
        AppInst(())
    }
}

impl Drop for AppInst {
    fn drop(&mut self) {
        unsafe { km_appinst_end() };
        LONG.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether a long sceAppInstUtil call runs or waits: a web handler skips its native call then
/// instead of waiting on the lock. ponytail: a check, not a try-lock (installer.c has none); a
/// long call that starts right after it still blocks that one request.
pub fn appinst_busy() -> bool {
    LONG.load(Ordering::SeqCst) > 0
}

/// One status poll (sceAppInstUtilGet[Patch]InstallStatus).
pub struct Native {
    pub status: String,
    pub downloaded: u64,
    pub total: u64,
    pub error: i32,
}

/// Hands `url` to sceAppInstUtilInstallByPackage (installer.c `km_pkg_install`). Ok: the content
/// id and task type (0 base, 1 addcont, 2 patch) Sony reported; Err: its error code. The native
/// return codes and any credential problem are logged with the URL either way.
pub fn submit(url: &str, name: &str) -> Result<(String, u8), u32> {
    let (Ok(c_url), Ok(c_name)) = (CString::new(url), CString::new(name)) else {
        log!("install {name}: NUL in the URL or name ({url})");
        return Err(BAD_ARG);
    };
    let mut cid = [0 as c_char; 48];
    let mut task = -1;
    let mut code = 0u32;
    let mut note = [0 as c_char; 256];
    LONG.fetch_add(1, Ordering::SeqCst);
    let rc = unsafe {
        km_pkg_install(c_url.as_ptr(), c_name.as_ptr(), cid.as_mut_ptr(), &mut task, &mut code,
            note.as_mut_ptr(), note.len())
    };
    LONG.fetch_sub(1, Ordering::SeqCst);
    let note = unsafe { CStr::from_ptr(note.as_ptr()) }.to_string_lossy().into_owned();
    log!("install {name}: {url}: {}", note.trim_end_matches("; "));
    if rc != 0 {
        return Err(code);
    }
    let cid = unsafe { CStr::from_ptr(cid.as_ptr()) }.to_string_lossy().into_owned();
    // ponytail: an out-of-range type (never seen: Sony fills 0..2) reads as 255, "not a base".
    Ok((cid, u8::try_from(task).unwrap_or(u8::MAX)))
}

/// The install status of `content_id` (the patch call when `patch`); Err: the native rc.
pub fn status(content_id: &str, patch: bool) -> Result<Native, i32> {
    let cid = CString::new(content_id).map_err(|_| BAD_ARG as i32)?;
    let mut status = [0 as c_char; 16];
    let (mut downloaded, mut total, mut error) = (0u64, 0u64, 0 as c_int);
    let rc = unsafe {
        km_pkg_status(cid.as_ptr(), patch as c_int, status.as_mut_ptr(), &mut downloaded,
            &mut total, &mut error)
    };
    if rc != 0 {
        return Err(rc);
    }
    let status = unsafe { CStr::from_ptr(status.as_ptr()) }.to_string_lossy().into_owned();
    Ok(Native { status, downloaded, total, error })
}

/// sceAppInstUtilGetAddcontInstalledStatus: Ok(true) when the DLC's state is 4 (installed);
/// Err: the native rc (0x80E90004 = not found, until the install is promoted).
pub fn addcont_installed(content_id: &str) -> Result<bool, i32> {
    let cid = CString::new(content_id).map_err(|_| BAD_ARG as i32)?;
    let mut state = 0 as c_int;
    let rc = unsafe { km_addcont_installed(cid.as_ptr(), &mut state) };
    if rc != 0 { Err(rc) } else { Ok(state == 4) }
}

/// sceAppInstUtilCancelInstall: the native rc. It addresses the base application, so only for a
/// fresh base install (SMP), never a patch, a DLC or an update of an installed game.
pub fn cancel_native(content_id: &str) -> i32 {
    match CString::new(content_id) {
        Ok(cid) => unsafe { km_pkg_cancel(cid.as_ptr()) },
        Err(_) => BAD_ARG as i32,
    }
}

// ---- the install job (one at a time, a queue in memory) ----

/// Sony's MetaInfo holds the URL in 128 bytes.
const URL_MAX: usize = 127;
/// No request and no byte served for this long fails the install (SMP).
const IDLE: Duration = Duration::from_secs(900);
/// A task not visible yet (status "none", DLC record missing) is waited for this long (SMP
/// `SM_API_PKG_START_TIMEOUT_US`).
const START_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(500);
/// sceAppInstUtilGetAddcontInstalledStatus: no installed-content record (yet).
const NOT_FOUND: i32 = 0x80E9_0004_u32 as i32;
/// The bytes a response body is sent in.
const SEND_CHUNK: usize = 1 << 20;
/// The longest request head read.
const HEAD_MAX: usize = 8 << 10;
/// A kept-alive connection with no request for this long is closed.
const CONN_IDLE: Duration = Duration::from_secs(60);
/// The served file's stamp is checked again once this has passed.
const RECHECK: Duration = Duration::from_secs(5);
/// Fixed and in the past (ps5upload found a fresh one refused on some firmware; dump-forge).
const LAST_MODIFIED: &str = "Wed, 01 Jan 2025 00:00:00 GMT";

/// What can be installed: a library FPKG game, or a patch/DLC package of a scan root.
pub enum Item {
    Game(Source),
    Pkg(scan::Package),
}

/// One install, as the UI sees it.
#[derive(Clone, Debug)]
pub struct Job {
    /// The source path.
    pub id: String,
    pub title_id: String,
    pub name: String,
    /// game | update | patch | dlc
    pub kind: &'static str,
    pub done_bytes: u64,
    pub total_bytes: u64,
    /// wait | download | install
    pub phase: &'static str,
    pub error: Option<String>,
}

/// A queued or running install.
#[derive(Clone)]
struct Entry {
    job: Job,
    /// The .pkg, as a source (path checks of the delete-after-install option).
    src: Source,
}

#[derive(Default)]
struct Jobs {
    current: Option<Entry>,
    queue: VecDeque<Entry>,
    /// The last result per id: "done", "cancelled" or the error text.
    results: Vec<(String, String)>,
}

/// Records `result` for the current entry and clears it.
fn finish(j: &mut Jobs, result: String) {
    if let Some(cur) = j.current.take() {
        j.results.retain(|(id, _)| *id != cur.job.id);
        j.results.push((cur.job.id, result));
    }
}

/// Makes the next queued entry current when none is.
fn pick_next(j: &mut Jobs) -> Option<&Entry> {
    if j.current.is_none() {
        j.current = j.queue.pop_front();
    }
    j.current.as_ref()
}

/// What a `Range` header asks of `total` bytes (dump-forge `parse_range`).
#[derive(Debug, PartialEq)]
enum Range {
    /// No usable range (none, malformed, several): the whole file.
    Full,
    /// Bytes `start..=end`.
    Part(u64, u64),
    /// Nothing of the file: 416.
    Bad,
}

fn parse_range(value: &str, total: u64) -> Range {
    let Some(spec) = value.trim().strip_prefix("bytes=") else { return Range::Full };
    let Some((a, b)) = spec.split_once('-').filter(|_| !spec.contains(',')) else { return Range::Full };
    let (a, b) = (a.trim(), b.trim());
    let (start, end) = if a.is_empty() {
        // `bytes=-n`: the last n bytes.
        let Ok(n) = b.parse::<u64>() else { return Range::Full };
        if n == 0 || total == 0 {
            return Range::Bad;
        }
        (total - n.min(total), total - 1)
    } else {
        let Ok(start) = a.parse::<u64>() else { return Range::Full };
        let end = match b.parse::<u64>() {
            Ok(end) => end,
            Err(_) if b.is_empty() => u64::MAX,
            Err(_) => return Range::Full,
        };
        (start, end)
    };
    if start >= total || start > end {
        return Range::Bad;
    }
    Range::Part(start, end.min(total - 1))
}

/// `http://127.0.0.1:8097/pkg/<token>/<name>`, the name percent-encoded as SMP does (alnum `.`
/// `-` `_` kept); None when longer than the installer takes.
fn url_for(token: &str, file_name: &str) -> Option<String> {
    let mut name = String::new();
    for b in file_name.bytes() {
        if b.is_ascii_alphanumeric() || b".-_".contains(&b) {
            name.push(b as char);
        } else {
            name += &format!("%{b:02X}");
        }
    }
    let url = format!("http://127.0.0.1:{}/pkg/{token}/{name}", crate::web::PORT);
    (url.len() <= URL_MAX).then_some(url)
}

/// The byte ranges served at least once: sorted, disjoint, half-open (dump-forge `Coverage`).
#[derive(Default)]
struct Coverage(Vec<(u64, u64)>);

impl Coverage {
    fn add(&mut self, start: u64, end: u64) {
        if start >= end {
            return;
        }
        let (mut s, mut e) = (start, end);
        self.0.retain(|&(a, b)| {
            let apart = b < s || a > e;
            if !apart {
                (s, e) = (s.min(a), e.max(b));
            }
            apart
        });
        let at = self.0.partition_point(|&(a, _)| a < s);
        self.0.insert(at, (s, e));
    }

    fn covered(&self) -> u64 {
        self.0.iter().map(|(a, b)| b - a).sum()
    }
}

/// One poll's verdict (SMP `package_install_thread`).
#[derive(Debug, PartialEq)]
enum Verdict {
    Done,
    /// The task moves: the start timeout is reset.
    Working,
    /// The task is not visible (yet): bounded by START_TIMEOUT.
    Waiting,
    Failed(String),
}

/// What the polls remember across calls.
#[derive(Default)]
struct Track {
    /// A patch task's total once it was seen fully downloaded (0: not yet).
    patch_total: u64,
    /// The installer reported a total at least once (then bytes served no longer count).
    native_progress: bool,
    /// A task of type 0 for an update/patch/DLC: the shared record changed since the submit.
    replaced: bool,
    /// That record before the submit, when it was terminal: (total, downloaded).
    before: Option<(u64, u64)>,
}

fn terminal(status: &str) -> bool {
    status == "playable" || status == "completed"
}

/// SMP's success rules for one status poll. `task`: Sony's task type (0 base, 1 addcont, 2
/// patch); `fresh`: a base game not installed before; `addcont`: the DLC's installed record
/// (task 1 only); `served_all`: every byte of the .pkg was served; `patch_seen`: for a patch
/// task retired to "none", the base reads playable and a new installed patch/app file showed up.
fn done(task: u8, fresh: bool, st: &Native, addcont: bool, served_all: bool, patch_seen: bool,
    t: &mut Track) -> Verdict {
    if st.error != 0 || st.status == "error" {
        return Verdict::Failed(native_error(st.error as u32, &st.status));
    }
    if st.status.is_empty() {
        return Verdict::Failed("the installer reported no status".into());
    }
    if st.status == "none" {
        // Entitlement-only DLC can have no download record; a patch task retires to "none".
        // A small patch can finish between two polls (no full-download status seen): then every
        // byte served stands in for it.
        if (task == 1 && addcont) || (task == 2 && patch_seen && (t.patch_total > 0 || served_all)) {
            return Verdict::Done;
        }
        return Verdict::Waiting;
    }
    if !fresh && task == 0 && !t.replaced {
        // An unchanged terminal record may still describe the previous base.
        t.replaced = !terminal(&st.status) || t.before.is_some_and(|b| b != (st.total, st.downloaded));
        if !t.replaced {
            return Verdict::Waiting;
        }
    }
    if task == 2 && st.total > 0 {
        if st.downloaded >= st.total {
            t.patch_total = st.total;
        } else if t.patch_total != st.total {
            t.patch_total = 0;
        }
    }
    t.native_progress |= st.total > 0;
    // PlayGo can skip unused ranges: the bytes served count only until the installer reports.
    let downloaded = if st.total > 0 { st.downloaded >= st.total } else { task == 0 && !t.native_progress && served_all };
    let completed = st.status == "completed" || (st.status == "playable" && downloaded);
    match (completed, task == 1) {
        // Completion can precede the installed DLC record.
        (true, true) if !addcont => Verdict::Waiting,
        (true, _) => Verdict::Done,
        _ => Verdict::Working,
    }
}

/// What Sony's error code means for the user, where it is known (dump-forge `hint`).
fn hint(code: u32) -> Option<&'static str> {
    Some(match code {
        0x80A3_0004 | 0x80A3_000A => "install the base game first",
        0x80A3_000F => "this patch is for a different game",
        0x80A3_0011 => "the base game is the wrong version for this patch",
        0x80A3_0019 => "the patch package is invalid or corrupt",
        0x80A3_0017 => "this DLC needs its base content installed",
        0x80A3_000D => "the console firmware is too old for this package",
        0x80A3_000C => "close the game before installing",
        0x80A3_0002 => "not enough free space",
        0x80A3_0008 | 0x80A3_0009 => "the package is broken or the wrong type",
        _ => return None,
    })
}

/// The error text for Sony's refusal of the package.
fn refused(code: u32) -> String {
    let fpkg = "; a debug package needs kstuff with fpkg enabled";
    match hint(code) {
        Some(h) => format!("the PS5 refused the package: 0x{code:08X} ({h})"),
        None => format!("the PS5 refused the package: 0x{code:08X}{fpkg}"),
    }
}

/// The error text for a failed status poll.
fn native_error(code: u32, status: &str) -> String {
    match hint(code) {
        Some(h) => format!("the install failed: 0x{code:08X} ({h})"),
        None => format!("the install failed: 0x{code:08X} (status {status:?})"),
    }
}

/// Whether `id` of `kind` may be queued: 409 when queued or running, 400 for a patch whose game
/// is not installed.
fn admit(j: &Jobs, id: &str, kind: &str, game_installed: bool) -> Result<(), (u16, String)> {
    if j.current.iter().chain(j.queue.iter()).any(|e| e.job.id == id) {
        return Err((409, "already queued".into()));
    }
    if kind == "patch" && !game_installed {
        return Err((400, "install the game first".into()));
    }
    Ok(())
}

static JOBS: OnceLock<(Mutex<Jobs>, Condvar)> = OnceLock::new();
static STATE: OnceLock<Arc<State>> = OnceLock::new();
static STARTED: std::sync::Once = std::sync::Once::new();
/// Set by `cancel`, cleared when the next job starts.
static CANCEL: AtomicBool = AtomicBool::new(false);
/// The title id of the running base install (`busy_tid`).
static BUSY: Mutex<Option<String>> = Mutex::new(None);
/// The package served now, for `serve_conn`.
static SERVING: Mutex<Option<Arc<Serving>>> = Mutex::new(None);

fn jobs() -> (MutexGuard<'static, Jobs>, &'static Condvar) {
    let (m, cv) = JOBS.get_or_init(Default::default);
    (m.lock().unwrap_or_else(|e| e.into_inner()), cv)
}

/// Gives the job thread the shared state (main, once at start-up); nothing starts here.
pub fn init(state: Arc<State>) {
    let _ = STATE.set(state);
}

/// Queues `item`; the job thread starts at the first call.
pub fn enqueue(item: Item) -> Result<(), (u16, String)> {
    let (src, kind) = match item {
        // An install run through ng: its source is the console's own app.pkg (installing it over itself).
        Item::Game(src) if crate::scan::installed_pkg(&src) => return Err((409, "already installed".into())),
        Item::Game(src) if src.kind == Kind::Pkg => {
            let kind = if crate::installed::is_installed(&src.title_id) { "update" } else { "game" };
            (src, kind)
        }
        Item::Game(_) => return Err((400, "only a .pkg game installs".into())),
        Item::Pkg(p) => {
            let kind = match p.kind {
                PkgKind::Base if crate::installed::is_installed(&p.title_id) => "update",
                PkgKind::Base => "game",
                PkgKind::Patch => "patch",
                PkgKind::Dlc => "dlc",
            };
            let src = Source { path: p.path, kind: Kind::Pkg, title_id: p.title_id, title_name: p.title_name,
                version: p.version, drive: p.drive, size: p.size, backport: None, image: None };
            (src, kind)
        }
    };
    let Some(state) = STATE.get().cloned() else { return Err((503, "not ready".into())) };
    let id = src.path.to_string_lossy().into_owned();
    let (mut j, cv) = jobs();
    admit(&j, &id, kind, crate::installed::is_installed(&src.title_id))?;
    let job = Job { id: id.clone(), title_id: src.title_id.clone(), name: src.title_name.clone(), kind,
        done_bytes: 0, total_bytes: src.size, phase: "wait", error: None };
    log!("install {}: queued {kind} {}", src.title_id, src.path.display());
    j.results.retain(|(r, _)| *r != id);
    j.queue.push_back(Entry { job, src });
    cv.notify_all();
    drop(j);
    STARTED.call_once(|| {
        std::thread::spawn(move || worker(state));
    });
    Ok(())
}

/// Cancels the running install; false when none runs.
pub fn cancel() -> bool {
    let (j, _) = jobs();
    let Some(cur) = &j.current else { return false };
    log!("install {}: cancel asked ({})", cur.job.title_id, cur.job.id);
    CANCEL.store(true, Ordering::SeqCst);
    if let Some(s) = SERVING.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        s.stop.store(true, Ordering::SeqCst);
    }
    true
}

/// The running job, the queue and the last result per id.
pub fn snapshot() -> (Option<Job>, Vec<Job>, Vec<(String, String)>) {
    let (j, _) = jobs();
    (j.current.as_ref().map(|e| e.job.clone()), j.queue.iter().map(|e| e.job.clone()).collect(), j.results.clone())
}

/// Whether an install runs or waits in the queue: quit and self-update refuse then (the loopback
/// server would go mid-download and leave a base install's marker).
pub fn active() -> bool {
    let (j, _) = jobs();
    j.current.is_some() || !j.queue.is_empty()
}

/// The title id of the running base install: the scan reserves it (scan::scan `installing`).
pub fn busy_tid() -> Option<String> {
    BUSY.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Updates the running job's row.
fn set(f: impl FnOnce(&mut Job)) {
    if let Some(e) = jobs().0.current.as_mut() {
        f(&mut e.job);
    }
}

// ---- the loopback Range server (dump-forge `serve`/`connection`/`answer`) ----

/// The file's identity: a same-size replacement changes it (dump-forge `changed`).
#[derive(PartialEq)]
struct Stamp(u64, u64, u64, i64, i64);

fn stamp(path: &Path) -> std::io::Result<Stamp> {
    let m = std::fs::symlink_metadata(path)?;
    if !m.is_file() {
        return Err(std::io::Error::other("not a regular file (a link is never followed)"));
    }
    Ok(Stamp(m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec()))
}

/// The package served for the running job.
struct Serving {
    token: String,
    /// The last part of the URL, as sent (percent-encoded).
    url_name: String,
    path: PathBuf,
    total: u64,
    stamp: Stamp,
    stop: AtomicBool,
    live: Mutex<Live>,
}

struct Live {
    coverage: Coverage,
    /// The last request or body chunk: the idle clock.
    last: Instant,
    /// The package changed or failed to read: the job's error.
    failed: Option<String>,
    requests: u64,
}

impl Serving {
    fn live(&self) -> MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fail(&self, e: String) -> std::io::Error {
        log!("install: serving {}: {e}", self.path.display());
        self.live().failed.get_or_insert_with(|| e.clone());
        self.stop.store(true, Ordering::SeqCst);
        std::io::Error::other(e)
    }
}

/// One request's head, as far as serving needs it.
struct Head {
    method: String,
    target: String,
    range: Option<String>,
    /// HTTP/1.1 unless `Connection: close`.
    keep: bool,
}

fn parse_head(text: &str) -> Option<Head> {
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target, version) = (first.next()?, first.next()?, first.next()?);
    let (mut range, mut connection) = (None, None);
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            if name.eq_ignore_ascii_case("range") {
                range = Some(value.to_string());
            } else if name.eq_ignore_ascii_case("connection") {
                connection = Some(value.to_ascii_lowercase());
            }
        }
    }
    let keep = match connection.as_deref() {
        Some(c) if c.contains("close") => false,
        Some(c) if c.contains("keep-alive") => true,
        _ => version == "HTTP/1.1",
    };
    Some(Head { method: method.to_string(), target: target.to_string(), range, keep })
}

fn response(status: &str, len: u64, extra: &str, keep: bool) -> String {
    format!("HTTP/1.1 {status}\r\nLast-Modified: {LAST_MODIFIED}\r\nContent-Type: application/octet-stream\r\n\
        Content-Length: {len}\r\nAccept-Ranges: bytes\r\n{extra}Connection: {}\r\n\r\n",
        if keep { "keep-alive" } else { "close" })
}

fn not_found(conn: &mut TcpStream, keep: bool) -> std::io::Result<bool> {
    let c = if keep { "keep-alive" } else { "close" };
    conn.write_all(format!("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: {c}\r\n\r\n").as_bytes())?;
    Ok(keep)
}

/// A loopback `GET|HEAD /pkg/<token>/<name>` connection from web.rs `handle`, `head` its first
/// request (without the blank line). Answers it and the requests that follow on the connection
/// (keep-alive) until it closes, idles, fails or the job stops serving. Only the running job's
/// token is served; anything else is 404.
// ponytail: bytes web.rs read past the first head (a pipelined request) are lost; the installer
// waits for each response before the next request.
pub fn serve_conn(mut conn: TcpStream, head: &str) {
    let srv = SERVING.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let Some(srv) = srv else {
        let _ = not_found(&mut conn, false);
        return;
    };
    let _ = conn.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = conn.set_write_timeout(Some(Duration::from_secs(30)));
    let _ = conn.set_nodelay(true);
    let (mut head, mut buf, mut file) = (head.to_string(), Vec::new(), None);
    let mut idle;
    loop {
        match answer(&mut conn, &srv, &head, &mut file) {
            Ok(true) => idle = Instant::now(),
            _ => return,
        }
        // Requests have no body: what follows one head is the next.
        loop {
            if srv.stop.load(Ordering::Relaxed) || buf.len() > HEAD_MAX {
                return;
            }
            if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                head = String::from_utf8_lossy(&buf[..at]).into_owned();
                buf.drain(..at + 4);
                break;
            }
            let mut chunk = [0u8; 4096];
            match conn.read(&mut chunk) {
                Ok(0) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    if idle.elapsed() >= CONN_IDLE {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }
}

/// Answers one request; Ok(true) when the connection may carry another.
fn answer(conn: &mut TcpStream, srv: &Serving, text: &str, file: &mut Option<(File, Instant)>)
    -> std::io::Result<bool> {
    let Some(h) = parse_head(text) else { return not_found(conn, false) };
    let head_only = h.method == "HEAD";
    if !head_only && h.method != "GET" {
        return not_found(conn, h.keep);
    }
    // The PS5 adds a query (`?product=..&serverIpAddr=..`): the path decides.
    let path = h.target.split_once('?').map_or(h.target.as_str(), |(p, _)| p);
    if path != format!("/pkg/{}/{}", srv.token, srv.url_name) || srv.stop.load(Ordering::Relaxed) {
        return not_found(conn, h.keep);
    }
    {
        let mut live = srv.live();
        live.last = Instant::now();
        live.requests += 1;
        if live.requests <= 8 {
            log!("install: served {} range={}", text.lines().next().unwrap_or(""), h.range.as_deref().unwrap_or("-"));
        }
    }
    let total = srv.total;
    let (start, end) = match h.range.as_deref().map_or(Range::Full, |r| parse_range(r, total)) {
        Range::Bad => {
            let head = response("416 Range Not Satisfiable", 0, &format!("Content-Range: bytes */{total}\r\n"), h.keep);
            conn.write_all(head.as_bytes())?;
            return Ok(h.keep);
        }
        Range::Part(s, e) => {
            let extra = format!("Content-Range: bytes {s}-{e}/{total}\r\n");
            conn.write_all(response("206 Partial Content", e - s + 1, &extra, h.keep).as_bytes())?;
            (s, e + 1)
        }
        Range::Full => {
            conn.write_all(response("200 OK", total, "", h.keep).as_bytes())?;
            (0, total)
        }
    };
    if !head_only && start < end {
        body(conn, srv, file, start, end)?;
    }
    Ok(h.keep)
}

/// Sends bytes `start..end` of the package, marking each chunk sent as covered. The file is
/// opened once per connection and checked against the job's stamp then and every RECHECK.
fn body(conn: &mut TcpStream, srv: &Serving, file: &mut Option<(File, Instant)>, start: u64, end: u64)
    -> std::io::Result<()> {
    let changed = || format!("{} changed while it was being installed", srv.path.display());
    match file {
        Some(_) => {}
        None => {
            let f = File::open(&srv.path).map_err(|e| srv.fail(format!("open: {e}")))?;
            let m = f.metadata().map_err(|e| srv.fail(format!("stat: {e}")))?;
            let same = Stamp(m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec()) == srv.stamp;
            if !same || stamp(&srv.path).ok().as_ref() != Some(&srv.stamp) {
                return Err(srv.fail(changed()));
            }
            *file = Some((f, Instant::now()));
        }
    }
    let (f, checked) = file.as_mut().expect("opened above");
    f.seek(SeekFrom::Start(start)).map_err(|e| srv.fail(format!("seek: {e}")))?;
    let mut buf = vec![0u8; SEND_CHUNK];
    let mut at = start;
    while at < end {
        if srv.stop.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("stopped"));
        }
        // Rechecked inside the transfer too: one long range must not stream a replaced file.
        if checked.elapsed() >= RECHECK {
            if stamp(&srv.path).ok().as_ref() != Some(&srv.stamp) {
                return Err(srv.fail(changed()));
            }
            *checked = Instant::now();
        }
        let want = usize::try_from(end - at).map_or(SEND_CHUNK, |n| n.min(SEND_CHUNK));
        f.read_exact(&mut buf[..want]).map_err(|e| srv.fail(format!("read at {at}: {e}")))?;
        conn.write_all(&buf[..want])?;
        let mut live = srv.live();
        live.coverage.add(at, at + want as u64);
        live.last = Instant::now();
        at += want as u64;
    }
    Ok(())
}

// ---- the job thread ----

/// Runs the queue, one install at a time, for the payload's life.
fn worker(state: Arc<State>) {
    loop {
        let e = {
            let (mut j, cv) = jobs();
            loop {
                if let Some(e) = pick_next(&mut j) {
                    // Under the lock: `cancel` sees this entry current only after the reset.
                    CANCEL.store(false, Ordering::SeqCst);
                    break e.clone();
                }
                j = cv.wait(j).unwrap_or_else(|e| e.into_inner());
            }
        };
        let r = run(&state, &e);
        if r != "done" {
            log!("install {}: {} ({})", e.job.title_id, r, e.src.path.display());
        }
        // Under the lock, before the wake: a later cancel finds no running job instead of
        // reporting a finished one as cancelled.
        finish(&mut jobs().0, r);
        state.wake();
    }
}

/// Clears `BUSY` when dropped.
struct Busy;

impl Busy {
    fn set(tid: &str) -> Busy {
        *BUSY.lock().unwrap_or_else(|e| e.into_inner()) = Some(tid.to_string());
        Busy
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        *BUSY.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// One install: "done", "cancelled" or the error text.
fn run(state: &State, e: &Entry) -> String {
    let (tid, path) = (e.job.title_id.as_str(), &e.src.path);
    let mut said = false;
    // No install while a game runs.
    while state.paused() {
        if CANCEL.load(Ordering::SeqCst) {
            return "cancelled".into();
        }
        if !said {
            log!("install {tid}: waiting for the running game to exit");
            said = true;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    if CANCEL.load(Ordering::SeqCst) {
        return "cancelled".into();
    }
    let st = match stamp(path) {
        Ok(s) => s,
        Err(err) => return format!("{}: {err}", path.display()),
    };
    let total = st.2;
    let need = total + total / 10;
    match free_bytes("/user") {
        Some(free) if free < need => {
            let gb = |b: u64| b as f64 / 1e9;
            return format!("not enough space on the internal SSD ({:.1} GB, {:.1} GB free)", gb(need), gb(free));
        }
        Some(_) => {}
        None => log!("install {tid}: statfs /user failed; free space not checked"),
    }
    // Re-checked here, not only at enqueue: a second copy queued behind the first is an update now.
    let fresh = e.job.kind == "game" && !crate::installed::is_installed(tid);
    if e.job.kind == "game" && !fresh {
        set(|j| j.kind = "update");
    }
    let _busy = fresh.then(|| Busy::set(tid));
    let mut new_marker = false;
    if fresh {
        match prepare_base(state, tid) {
            Ok(n) => new_marker = n,
            Err(err) => return err,
        }
    }
    if CANCEL.load(Ordering::SeqCst) {
        // Nothing was submitted: no marker left for an install that never started. A marker of
        // an earlier failed install stays ("Install failed" + Reinstall).
        if new_marker {
            crate::installed::unmark(tid);
        }
        return "cancelled".into();
    }
    // A marker over a console copy (an earlier install killed or failed mid-way) is no new base
    // for the record checks: Sony's old terminal record may still be there (SMP general_snapshot).
    let new_base = fresh && crate::installed::copy_in(Path::new("/"), tid).is_none();
    let token = token();
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let Some(url) = url_for(&token, &file_name).or_else(|| url_for(&token, "game.pkg")) else {
        return "the install URL is too long".into();
    };
    let url_name = url.rsplit('/').next().unwrap_or_default().to_string();
    let live = Live { coverage: Coverage::default(), last: Instant::now(), failed: None, requests: 0 };
    let srv = Arc::new(Serving { token, url_name, path: path.clone(), total, stamp: st, stop: AtomicBool::new(false),
        live: Mutex::new(live) });
    *SERVING.lock().unwrap_or_else(|e| e.into_inner()) = Some(srv.clone());
    set(|j| (j.phase, j.done_bytes, j.total_bytes) = ("download", 0, total));
    let r = install(e, &srv, &url, new_base);
    srv.stop.store(true, Ordering::SeqCst);
    *SERVING.lock().unwrap_or_else(|e| e.into_inner()) = None;
    if let Err(err) = r {
        // A confirmed completion wins over a late cancel; a cancel over the error it caused.
        return if CANCEL.load(Ordering::SeqCst) { "cancelled".into() } else { err };
    }
    log!("install {tid}: done ({})", path.display());
    if fresh {
        crate::installed::unmark(tid);
    }
    let s = state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if s.install_delete_source {
        let roots = if s.scan_paths.is_empty() { crate::storage::default_scan_roots() } else { s.scan_paths.clone() };
        match crate::web::safe_delete_source(&e.src, &roots).map_err(String::from)
            .and_then(|()| std::fs::remove_file(path).map_err(|e| e.to_string())) {
            Ok(()) => log!("install {tid}: source deleted ({})", path.display()),
            Err(err) => log!("install {tid}: source not deleted ({}): {err}", path.display()),
        }
    }
    "done".into()
}

/// A base game: the marker first, then Kraken's own home-screen entry goes, so the real install
/// gets a clean title id. Under `work`: no scan pass registers or mounts it meanwhile.
/// Ok(true) when this call wrote the marker (false: an earlier failed install left it).
fn prepare_base(state: &State, tid: &str) -> Result<bool, String> {
    let _work = state.work.lock().unwrap_or_else(|e| e.into_inner());
    let new = !crate::installed::marked(tid);
    crate::installed::mark(tid).map_err(|e| format!("{}/{tid}: {e}", crate::installed::MARKERS))?;
    if !crate::register::is_registered(tid) {
        return Ok(new);
    }
    // A copy of the console's own (an earlier, failed install) is never unmounted or uninstalled
    // here: checked before unmount_title, which removes every layer at /system_ex/app/<TID>.
    if let Err(p) = crate::register::is_ours(tid) {
        log!("{tid}: not Kraken's entry ({}): left for the installer", p.display());
        return Ok(new);
    }
    if let Err(err) = crate::register::unmount_title(tid) {
        if new {
            crate::installed::unmark(tid);
        }
        return Err(format!("/system_ex/app/{tid}: {err}"));
    }
    if let Err(err) = crate::register::uninstall(tid) {
        if new {
            crate::installed::unmark(tid);
        }
        return Err(format!("{tid}: Kraken's entry not removed: {err}"));
    }
    crate::register::forget(tid);
    log!("{tid}: Kraken's entry removed for the install");
    Ok(new)
}

/// Hands the URL to the installer and polls it until the install is done (Ok), fails, idles or
/// is cancelled (Err). Cancel/failure of a fresh base task cancels it natively (SMP).
fn install(e: &Entry, srv: &Serving, url: &str, fresh: bool) -> Result<(), String> {
    let tid = e.job.title_id.as_str();
    let mut t = Track::default();
    // The shared record an update's task 0 reuses, before the submit (SMP general_snapshot).
    if !fresh {
        if let Some(n) = content_id_of(&e.src.path).and_then(|c| status(&c, false).ok()) {
            if n.error == 0 && terminal(&n.status) {
                t.before = Some((n.total, n.downloaded));
            }
        }
    }
    let patch_before = patch_files(tid);
    let (cid, task) = submit(url, &e.job.name.replace('\0', "")).map_err(refused)?;
    log!("install {tid}: accepted, content id {cid}, task type {task}");
    let r = poll(srv, tid, &cid, task, fresh, &patch_before, &mut t);
    if r.is_err() && fresh && task == 0 {
        // ponytail: CancelInstall addresses the base application: only a fresh base task here.
        let rc = cancel_native(&cid);
        if rc != 0 {
            log!("install {tid}: native cancel of {cid} failed: 0x{:08X}", rc as u32);
        }
    }
    r
}

fn poll(srv: &Serving, tid: &str, cid: &str, task: u8, fresh: bool, patch_before: &[Option<Stamp>], t: &mut Track)
    -> Result<(), String> {
    let mut deadline = Some(Instant::now() + START_TIMEOUT);
    // The task's record is not queryable for a moment after the submit: a status error before
    // the first good poll waits, bounded by START_TIMEOUT.
    let mut polled = false;
    // The last native (status, downloaded) and when it changed: progress the installer reports
    // without fetching (its post-download step) also resets the idle limit.
    let mut native: (String, u64, Instant) = (String::new(), 0, Instant::now());
    loop {
        if CANCEL.load(Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        let (idle, covered, failed) = {
            let l = srv.live();
            (l.last.elapsed(), l.coverage.covered(), l.failed.clone())
        };
        if let Some(f) = failed {
            return Err(f);
        }
        if idle.min(native.2.elapsed()) >= IDLE {
            return Err(format!("the installer asked for no byte and showed no progress in {} s", IDLE.as_secs()));
        }
        let n = match status(cid, task == 2) {
            Ok(n) => n,
            Err(rc) if !polled => {
                let d = *deadline.get_or_insert_with(|| Instant::now() + START_TIMEOUT);
                if Instant::now() >= d {
                    return Err(format!("the installer showed no task for {cid} in {} s (status rc 0x{:08X})",
                        START_TIMEOUT.as_secs(), rc as u32));
                }
                std::thread::sleep(POLL);
                continue;
            }
            Err(rc) => return Err(native_error(rc as u32, "-")),
        };
        polled = true;
        if (n.status.as_str(), n.downloaded) != (native.0.as_str(), native.1) {
            native = (n.status.clone(), n.downloaded, Instant::now());
        }
        let mut addcont = false;
        if task == 1 && matches!(n.status.as_str(), "none" | "completed" | "playable") {
            match addcont_installed(cid) {
                Ok(b) => addcont = b,
                // A new DLC need not have an installed-content record yet.
                Err(NOT_FOUND) => {}
                Err(rc) => return Err(native_error(rc as u32, "addcont")),
            }
        }
        let served_all = covered >= srv.total;
        let mut patch_seen = false;
        // A small patch can retire to "none" before any poll saw it fully downloaded: every byte
        // served stands in for that (see `done`).
        if task == 2 && n.status == "none" && (t.patch_total > 0 || served_all) {
            let g = status(cid, false).map_err(|rc| native_error(rc as u32, "-"))?;
            if g.error != 0 || g.status == "error" {
                return Err(native_error(g.error as u32, &g.status));
            }
            // ponytail: SMP also parses the new file's content id and version; a changed stamp
            // of the installed app.pkg or patch.pkg is taken as the patch result here.
            patch_seen = terminal(&g.status) && patch_files(tid).iter().zip(patch_before)
                .any(|(now, was)| now.is_some() && now != was);
        }
        let phase = if served_all || (n.total > 0 && n.downloaded >= n.total) { "install" } else { "download" };
        let (d, all) = if n.total > 0 { (n.downloaded, n.total) } else { (covered, srv.total) };
        set(|j| (j.phase, j.done_bytes, j.total_bytes) = (phase, d.min(all), all));
        match done(task, fresh, &n, addcont, served_all, patch_seen, t) {
            Verdict::Done => return Ok(()),
            Verdict::Failed(err) => return Err(err),
            Verdict::Working => deadline = None,
            Verdict::Waiting => {
                let d = *deadline.get_or_insert_with(|| Instant::now() + START_TIMEOUT);
                if Instant::now() >= d {
                    return Err(format!("the installer showed no task for {cid} in {} s (status {:?})",
                        START_TIMEOUT.as_secs(), n.status));
                }
            }
        }
        std::thread::sleep(POLL);
    }
}

/// Available bytes of the filesystem at `path` (statfs: adapted for the PS5 libc in compat.c).
fn free_bytes(path: &str) -> Option<u64> {
    let c = CString::new(path).ok()?;
    let mut f: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut f) } != 0 {
        return None;
    }
    Some(f.f_bavail.max(0) as u64 * f.f_bsize as u64)
}

/// 16 random hex digits for the URL.
fn token() -> String {
    let mut b = [0u8; 8];
    if File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b)).is_err() {
        // ponytail: without /dev/urandom the clock and pid make the token (loopback only anyway).
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        b = (t.as_nanos() as u64 ^ u64::from(std::process::id()) << 32).to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// The content id of the package at `path` (the CNT header's 0x40, 36 ASCII bytes).
fn content_id_of(path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let mut outer = [0u8; 0x100];
    f.read_exact(&mut outer).ok()?;
    let (at, _) = scan::cnt_offset(&outer)?;
    f.seek(SeekFrom::Start(at.checked_add(0x40)?)).ok()?;
    let mut id = [0u8; 36];
    f.read_exact(&mut id).ok()?;
    let id = std::str::from_utf8(&id).ok()?;
    id.bytes().all(|b| b.is_ascii_graphic()).then(|| id.to_string())
}

/// Stamps of the installed app.pkg and every patch.pkg root SMP checks for `tid`.
fn patch_files(tid: &str) -> Vec<Option<Stamp>> {
    let mut paths = vec![crate::installed::copy_in(Path::new("/"), tid).unwrap_or_default()];
    for root in ["/user/patch", "/mnt/ext0/user/patch", "/mnt/ext0/ps5/user/patch", "/mnt/ext1/user/patch",
        "/mnt/ext1/ps5/user/patch"] {
        paths.push(Path::new(root).join(tid).join("patch.pkg"));
    }
    paths.iter().map(|p| stamp(p).ok().filter(|s| s.2 > 0)).collect()
}
