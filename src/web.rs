//! Tiny HTTP/1.1 server for the web UI opened in the PS5 browser (Connection: close, no keep-alive).
//! Routes: DESIGN.md "Web API".
use crate::config::{self, Settings};
use crate::scan::{Kind, Packing, Source};
use crate::{cache, register, storage, sys};
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub const PORT: u16 = 8097;
const MAX_REQUEST: usize = 16 << 10;
const LOG_LINES: usize = 100;
const UI: &str = include_str!("ui.html");
const APP_ICON: &[u8] = include_bytes!("../ps5/icon0.png");

#[derive(Clone)]
pub struct Game {
    pub src: Source,
    /// "pending" | "checking" | "building" | "ready" | "error" | "duplicate"
    pub status: &'static str,
    pub error: Option<String>,
    pub cache_size: u64,
    pub cache_built: Option<u64>,
}

pub struct State {
    /// The last scan: library games first, then duplicates.
    pub games: Mutex<Vec<Game>>,
    pub settings: Mutex<Settings>,
    /// Title ids to rebuild on the next scan.
    pub forced: Mutex<HashSet<String>>,
    /// Purge requested: done by the scan loop, so it never races a build.
    pub purge: AtomicBool,
    pub busy: AtomicBool,
    /// Start-up is waiting for this ("kstuff" or "shadowmount", main::wait_ready); None once ready.
    pub waiting: Mutex<Option<&'static str>>,
    /// Held by the scan loop for a whole pass (scan, purge, build, register); delete takes it.
    pub work: Mutex<()>,
    pub low_space: AtomicBool,
    pub cache_total: Mutex<u64>,
    /// Installed packages that are not homebrew PKGs (retail or signed): title ids, for the UI.
    pub skipped: Mutex<Vec<String>>,
    /// Title ids registered by this run or with a built cache; launch::watch overlays only these.
    pub titles: Arc<Mutex<HashSet<String>>>,
    /// Title ids with a live game session (launch::watch); the scan loop pauses while non-empty.
    pub running: Arc<Mutex<HashSet<String>>>,
    rescan: Mutex<bool>,
    wake: Condvar,
}

impl State {
    pub fn new(settings: Settings) -> State {
        State {
            games: Mutex::new(Vec::new()),
            settings: Mutex::new(settings),
            forced: Mutex::new(HashSet::new()),
            purge: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            waiting: Mutex::new(None),
            work: Mutex::new(()),
            low_space: AtomicBool::new(false),
            cache_total: Mutex::new(0),
            skipped: Mutex::new(Vec::new()),
            titles: Arc::new(Mutex::new(HashSet::new())),
            running: Arc::new(Mutex::new(HashSet::new())),
            rescan: Mutex::new(false),
            wake: Condvar::new(),
        }
    }

    /// Updates the entry of the source at `path` (if still listed).
    pub fn update(&self, path: &Path, f: impl FnOnce(&mut Game)) {
        if let Some(g) = self.games.lock().unwrap().iter_mut().find(|g| g.src.path == path) {
            f(g);
        }
    }

    /// Wakes the scan loop now.
    pub fn wake(&self) {
        *self.rescan.lock().unwrap() = true;
        self.wake.notify_all();
    }

    /// Blocks until `wake` or `timeout`, then clears the flag.
    pub fn wait(&self, timeout: Duration) {
        let flag = self.rescan.lock().unwrap();
        let (mut flag, _) = self.wake.wait_timeout_while(flag, timeout, |f| !*f).unwrap();
        *flag = false;
    }

    /// A game runs: no scan/build/register/purge until it exits (requests stay queued).
    pub fn paused(&self) -> bool {
        !self.running.lock().unwrap().is_empty()
    }

    /// Title ids of the library (duplicates excluded).
    pub fn library(&self) -> Vec<String> {
        let games = self.games.lock().unwrap();
        games.iter().filter(|g| g.status != "duplicate").map(|g| g.src.title_id.clone()).collect()
    }

    fn source(&self, tid: &str) -> Option<Source> {
        let games = self.games.lock().unwrap();
        games.iter().find(|g| g.src.title_id == tid && g.status != "duplicate").map(|g| g.src.clone())
    }
}

fn game_json(g: &Game) -> serde_json::Value {
    let (kind, packing) = match g.src.kind {
        Kind::Pkg => ("pkg", None),
        Kind::Folder(Packing::Plain) => ("folder", Some("plain")),
        Kind::Folder(Packing::Zstd) => ("folder", Some("zstd")),
        Kind::Folder(Packing::Lz4) => ("folder", Some("lz4")),
    };
    serde_json::json!({
        "title_id": g.src.title_id,
        "title_name": g.src.title_name,
        "version": g.src.version,
        "path": g.src.path.to_string_lossy(),
        "kind": kind,
        "packing": packing,
        "installed_pkg": crate::installed::package_path(&g.src.path),
        "drive": g.src.drive,
        "size": g.src.size,
        "status": g.status,
        "error": g.error,
        "cache_size": g.cache_size,
        "cache_built": g.cache_built,
        "backport": g.src.backport.as_ref().map(|b| b.to_string_lossy()),
    })
}

/// Single instance: binds PORT, asking an older copy on it to quit first (POST /api/quit),
/// retrying for 5 s. None: the port stays taken (an older build without /api/quit, or another
/// app), so this copy must not run.
pub fn take_port() -> Option<TcpListener> {
    if let Ok(l) = TcpListener::bind(("0.0.0.0", PORT)) {
        return Some(l);
    }
    if let Ok(mut s) = TcpStream::connect(("127.0.0.1", PORT)) {
        let _ = s.write_all(b"POST /api/quit HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n");
    }
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(250));
        if let Ok(l) = TcpListener::bind(("0.0.0.0", PORT)) {
            return Some(l);
        }
    }
    None
}

/// Listens on 0.0.0.0:PORT forever, one short-lived thread per connection. Once bound, registers
/// the home-screen tile that opens the UI (once, in its own thread).
pub fn serve(state: Arc<State>, listener: TcpListener) {
    log!("web UI on port {PORT}");
    std::thread::spawn(|| crate::register::register_tile(PORT));
    for conn in listener.incoming().flatten() {
        let state = state.clone();
        std::thread::spawn(move || {
            let _ = handle(&state, conn);
        });
    }
}

struct Reply {
    code: u16,
    ctype: &'static str,
    body: Vec<u8>,
    /// "no-cache" for what the browser's AppCache keeps (page, manifest, images), else "no-store".
    cache: &'static str,
}

fn reply(code: u16, ctype: &'static str, body: impl Into<Vec<u8>>) -> Reply {
    Reply { code, ctype, body: body.into(), cache: "no-store" }
}

/// For the AppCache: revalidated on every load, so a new payload's page replaces the kept one.
fn cached(ctype: &'static str, body: impl Into<Vec<u8>>) -> Reply {
    Reply { cache: "no-cache", ..reply(200, ctype, body) }
}

/// The AppCache manifest: the tile's page opens offline (no payload yet) and starts the saved ELF.
/// Its comment changes with the version and the page, so a new build's page is fetched again.
fn appcache() -> String {
    let hash = UI.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("CACHE MANIFEST\n# {} {hash:016x}\nCACHE:\n/\n/app-icon.png\nNETWORK:\n*\n", env!("CARGO_PKG_VERSION"))
}

fn no_content() -> Reply {
    reply(204, "", "")
}

fn not_found() -> Reply {
    reply(404, "text/plain", "not found")
}

fn handle(state: &State, mut conn: TcpStream) -> std::io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(5)))?;
    conn.set_write_timeout(Some(Duration::from_secs(10)))?;
    let loopback = conn.peer_addr().map(|a| a.ip().is_loopback()).unwrap_or(false);
    let r = match read_request(&mut conn)? {
        Ok((head, body)) => route(state, &head, &body, loopback),
        Err(code) => reply(code, "text/plain", "bad or too large request"),
    };
    let reason = match r.code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    };
    let mut head = format!("HTTP/1.1 {} {reason}\r\nConnection: close\r\n", r.code);
    if r.code != 204 {
        head += &format!("Content-Type: {}\r\nContent-Length: {}\r\n", r.ctype, r.body.len());
    }
    head += &format!("Cache-Control: {}\r\n\r\n", r.cache);
    conn.write_all(head.as_bytes())?;
    if r.code != 204 {
        conn.write_all(&r.body)?;
    }
    conn.flush()
}

/// Request line + headers, and the body by Content-Length; Err(status) when the head exceeds
/// MAX_REQUEST (431), the whole request would (413), or Content-Length is not a number (400).
fn read_request(conn: &mut TcpStream) -> std::io::Result<Result<(String, Vec<u8>), u16>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    // (end of the head, end of the body)
    let mut need: Option<(usize, usize)> = None;
    loop {
        if need.is_none() {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                let len = head.lines().find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>())
                });
                let len = match len {
                    None => 0,
                    Some(Ok(n)) => n,
                    Some(Err(_)) => return Ok(Err(400)),
                };
                // checked: a huge Content-Length must not wrap (panic=abort would end the payload).
                match (end + 4).checked_add(len) {
                    Some(n) if n <= MAX_REQUEST => need = Some((end, n)),
                    _ => return Ok(Err(413)),
                }
            }
        }
        match need {
            Some((end, n)) if buf.len() >= n => {
                let head = String::from_utf8_lossy(&buf[..end]).into_owned();
                return Ok(Ok((head, buf[end + 4..n].to_vec())));
            }
            None if buf.len() > MAX_REQUEST => return Ok(Err(431)),
            _ => {}
        }
        let n = conn.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn route(state: &State, head: &str, body: &[u8], loopback: bool) -> Reply {
    let mut parts = head.lines().next().unwrap_or("").split(' ');
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match (method, path) {
        ("GET", "/") => cached("text/html; charset=utf-8", UI.replace("__VERSION__", env!("CARGO_PKG_VERSION"))),
        ("GET", "/kraken.appcache") => cached("text/cache-manifest", appcache()),
        ("GET", "/app-icon.png") => cached("image/png", APP_ICON),
        ("GET", "/api/state") => reply(200, "application/json", api_state(state)),
        ("GET", p) if p.starts_with("/api/sizes/") => sizes(state, &p["/api/sizes/".len()..]),
        ("GET", "/api/settings") => {
            let v = config::to_json(&state.settings.lock().unwrap());
            reply(200, "application/json", v.to_string())
        }
        ("POST", "/api/settings") => post_settings(state, body),
        ("GET", "/api/ampr") => reply(200, "application/json", crate::ampr_status().to_string()),
        // The UI's "Update now": forced, one attempt, answered when done (one thread per request).
        ("POST", "/api/ampr/update") => match crate::ampr_update(state, true, 1) {
            Ok(m) => reply(200, "text/plain", m),
            Err(e) if e.contains("already running") => reply(409, "text/plain", e),
            Err(e) => reply(502, "text/plain", e),
        },
        // A newer copy taking over the port (loopback, empty body) or the UI's Shut down (body
        // "shutdown", from any host, as Dump Forge's Stop).
        ("POST", "/api/quit") if (loopback || body == b"shutdown") && state.paused() => {
            log!("quit refused: a game is running (its overlay would stay mounted)");
            reply(409, "text/plain", "a game is running")
        }
        ("POST", "/api/quit") if loopback || body == b"shutdown" => {
            let user = body == b"shutdown";
            let toast = user && state.settings.lock().unwrap().notify;
            log!("{}", if user { "shut down from the web UI" } else { "quit requested by a newer copy" });
            std::thread::spawn(move || {
                if toast {
                    sys::notify(crate::i18n::stopped(crate::i18n::lang()));
                }
                std::thread::sleep(Duration::from_millis(200));
                std::process::exit(0)
            });
            no_content()
        }
        ("POST", "/api/rescan") => {
            state.wake();
            no_content()
        }
        ("POST", "/api/rebuild-all") => {
            let tids = state.library();
            log!("rebuild of all {} games requested", tids.len());
            state.forced.lock().unwrap().extend(tids);
            state.wake();
            no_content()
        }
        ("POST", p) if p.starts_with("/api/rebuild/") => rebuild(state, &p["/api/rebuild/".len()..]),
        ("POST", p) if p.starts_with("/api/delete/") => delete(state, &p["/api/delete/".len()..], body),
        ("POST", "/api/eject") => eject(state, body),
        ("POST", "/api/purge") => {
            log!("cache purge requested");
            state.purge.store(true, Ordering::SeqCst);
            state.wake();
            no_content()
        }
        ("GET", p) if p.starts_with("/icon/") => image(state, &p["/icon/".len()..], &["icon0.png"]),
        ("GET", p) if p.starts_with("/pic/") => image(state, &p["/pic/".len()..], &["pic0.png", "pic1.png"]),
        // Debug: read a file under /data (logs, klog.txt). Read-only.
        ("GET", p) if p.starts_with("/file/data/") && state.settings.lock().unwrap().debug => {
            dbg_file(&p["/file".len()..], query)
        }
        _ => not_found(),
    }
}

fn post_settings(state: &State, body: &[u8]) -> Reply {
    let parsed = serde_json::from_slice(body).map_err(|e| format!("settings: {e}")).and_then(|v| config::from_json(&v));
    let s = match parsed {
        Ok(s) => s,
        Err(e) => {
            log!("settings rejected: {e}");
            return reply(400, "text/plain", e);
        }
    };
    if let Err(e) = config::save(&s) {
        log!("settings: save {}: {e}", config::CONFIG_PATH);
        return reply(500, "text/plain", e.to_string());
    }
    log!("settings saved");
    *state.settings.lock().unwrap() = s;
    state.wake();
    no_content()
}

/// Serves an absolute path under /data (no traversal above /data). Whole file: first 256 KiB as
/// text. With ?off=N&len=M: those bytes (M <= 16 MiB) as octet-stream, read in place, so large
/// files can be pulled in pieces.
fn dbg_file(path: &str, query: &str) -> Reply {
    use std::os::unix::fs::FileExt;
    if path.contains("..") {
        return not_found();
    }
    let arg = |k: &str| {
        query.split('&').find_map(|kv| kv.strip_prefix(k)?.strip_prefix('=')?.parse::<u64>().ok())
    };
    if let (Some(off), Some(len)) = (arg("off"), arg("len")) {
        let Ok(f) = std::fs::File::open(path) else { return not_found() };
        let mut buf = vec![0u8; len.min(16 << 20) as usize];
        let mut n = 0;
        while n < buf.len() {
            match f.read_at(&mut buf[n..], off + n as u64) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(_) => return not_found(),
            }
        }
        buf.truncate(n);
        return reply(200, "application/octet-stream", buf);
    }
    match std::fs::read(path) {
        Ok(mut b) => {
            b.truncate(256 * 1024);
            reply(200, "text/plain; charset=utf-8", b)
        }
        Err(_) => not_found(),
    }
}

fn api_state(state: &State) -> Vec<u8> {
    let games: Vec<_> = state.games.lock().unwrap().iter().map(game_json).collect();
    let all = storage::drives();
    let drives: Vec<_> = all
        .iter()
        .map(|d| serde_json::json!({"mount": d.mount, "label": d.label, "kind": d.kind, "total": d.total, "free": d.free,
            "ejectable": storage::ejectable(&d.mount)}))
        .collect();
    let free = all.iter().find(|d| d.mount == "/data").map_or(0, |d| d.free);
    let log = log_tail();
    let log = String::from_utf8_lossy(&log);
    let lines: Vec<&str> = log.lines().collect();
    let tail = &lines[lines.len().saturating_sub(LOG_LINES)..];
    let mut running: Vec<String> = state.running.lock().unwrap().iter().cloned().collect();
    running.sort();
    let v = serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "busy": state.busy.load(Ordering::SeqCst),
        "paused": !running.is_empty(),
        "waiting": *state.waiting.lock().unwrap(),
        "running": running,
        "games": games,
        "installed_skipped": *state.skipped.lock().unwrap(),
        "drives": drives,
        "cache": {
            "path": cache::CACHES,
            "size": *state.cache_total.lock().unwrap(),
            "free": free,
            "low_space": state.low_space.load(Ordering::SeqCst),
            "ampr_present": Path::new(cache::AMPR_DIR).join("libSceAmpr.sprx").is_file(),
        },
        "log": tail,
    });
    v.to_string().into_bytes()
}

/// The last LOG_TAIL bytes of kraken.log (sys.rs rotates it at ~4 MiB), from a line start.
fn log_tail() -> Vec<u8> {
    use std::io::{Seek, SeekFrom};
    const LOG_TAIL: u64 = 64 << 10;
    let Ok(mut f) = std::fs::File::open(sys::LOG_PATH) else { return Vec::new() };
    let len = f.metadata().map_or(0, |m| m.len());
    let off = len.saturating_sub(LOG_TAIL);
    let mut b = Vec::new();
    if f.seek(SeekFrom::Start(off)).is_err() || f.take(LOG_TAIL).read_to_end(&mut b).is_err() {
        return Vec::new();
    }
    if off > 0 {
        let cut = b.iter().position(|&c| c == b'\n').map_or(b.len(), |i| i + 1);
        b.drain(..cut);
    }
    b
}

fn rebuild(state: &State, tid: &str) -> Reply {
    if !crate::scan::valid_tid(tid) || state.source(tid).is_none() {
        return not_found();
    }
    log!("{tid}: rebuild requested");
    state.forced.lock().unwrap().insert(tid.to_string());
    state.wake();
    no_content()
}

fn sizes(state: &State, tid: &str) -> Reply {
    if !crate::scan::valid_tid(tid) {
        return not_found();
    }
    let Some(src) = state.source(tid) else { return not_found() };
    let game = match src.kind {
        Kind::Pkg => fs::metadata(&src.path).map(|m| m.len()).unwrap_or_else(|e| {
            log!("{tid}: stat {}: {e}", src.path.display());
            src.size
        }),
        Kind::Folder(_) => cache::du(&src.path),
    };
    let dlc: u64 = [
        "/user/addcont", "/mnt/ext0/user/addcont", "/mnt/ext0/ps5/user/addcont",
        "/mnt/ext1/user/addcont", "/mnt/ext1/ps5/user/addcont",
    ].iter().map(|root| cache::du(&Path::new(root).join(tid))).sum();
    let family = if tid.starts_with("PPSA") { "savedata_prospero" } else { "savedata" };
    let save = match fs::read_dir("/user/home") {
        Ok(entries) => {
            let mut total = 0;
            let mut failed = false;
            for entry in entries {
                match entry.and_then(|e| Ok((e.file_name(), e.file_type()?, e.path()))) {
                    Ok((name, ty, home)) => {
                        let name = name.to_string_lossy();
                        if ty.is_dir() && name.len() == 8 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
                            total += cache::du(&home.join(family).join(tid));
                        }
                    }
                    Err(e) => {
                        log!("{tid}: read /user/home: {e}");
                        failed = true;
                        break;
                    }
                }
            }
            if failed { None } else { Some(total) }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(0),
        Err(e) => {
            log!("{tid}: read /user/home: {e}");
            None
        }
    };
    reply(200, "application/json", serde_json::json!({
        "game": game, "dlc": dlc, "save": save, "cache": cache::size(tid),
    }).to_string())
}

fn safe_delete_source(src: &Source, roots: &[String]) -> Result<(), &'static str> {
    let path = &src.path;
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return Err("source path is not a safe absolute path");
    }
    if ["/", "/data", "/user", "/mnt", "/system", "/system_ex"].iter().any(|p| path == Path::new(p))
        || path.parent() == Some(Path::new("/mnt"))
        || storage::drives().iter().any(|d| path == Path::new(&d.mount)) {
        return Err("source path is a protected root");
    }
    let roots: Vec<&Path> = roots.iter().map(|r| {
        let trimmed = r.trim_end_matches('/');
        Path::new(if trimmed.is_empty() { "/" } else { trimmed })
    }).collect();
    if roots.iter().any(|root| path == *root || root.starts_with(path)) {
        return Err("source path is a scan root or its parent");
    }
    let Some(root) = roots.iter().filter(|root| path.starts_with(root)).max_by_key(|r| r.as_os_str().len()) else {
        return Err("source path is outside scan roots");
    };
    // No symlink from the scan root down to the source: the path text must be where it lives.
    // ponytail: links above the root are not checked (system paths may be links; the root is the user's choice).
    // ponytail: checked once; a link swapped in before remove_dir_all is not caught (std does not follow it).
    if path.ancestors().skip(1).take_while(|a| a.starts_with(root))
        .any(|a| fs::symlink_metadata(a).map(|m| m.file_type().is_symlink()).unwrap_or(true)) {
        return Err("source path goes through a symlink");
    }
    let meta = fs::symlink_metadata(path).map_err(|_| "source path is missing or inaccessible")?;
    match src.kind {
        Kind::Pkg if meta.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pkg")) => Ok(()),
        Kind::Folder(_) if meta.is_dir() => Ok(()),
        Kind::Pkg => Err("source is not a .pkg file"),
        Kind::Folder(_) => Err("source is not a directory"),
    }
}

fn delete(state: &State, tid: &str, body: &[u8]) -> Reply {
    if !crate::scan::valid_tid(tid) {
        return not_found();
    }
    // The scan loop holds `work` while it scans, builds or registers; never wait a build out here.
    let Ok(_work) = state.work.try_lock() else {
        return reply(409, "text/plain", "a scan or build is in progress; try again in a moment");
    };
    let Some(src) = state.source(tid) else { return not_found() };
    // A package the system installed belongs to the system: never delete it from here.
    if crate::installed::package_path(&src.path) {
        log!("{tid}: delete refused: {} is an installed package", src.path.display());
        return reply(409, "text/plain", "installed games are managed by the system; this only deletes scan sources");
    }
    // The UI sends the path it showed in the confirmation; a scan may have picked another copy since.
    if body != src.path.as_os_str().as_encoded_bytes() {
        log!("{tid}: delete refused: asked for {:?}, library source is {}", String::from_utf8_lossy(body), src.path.display());
        return reply(409, "text/plain", "the library changed; reopen the game and try again");
    }
    if state.paused() || state.running.lock().unwrap().contains(tid) {
        return reply(409, "text/plain", "a game is running");
    }
    if state.games.lock().unwrap().iter().any(|g| g.src.title_id == tid && matches!(g.status, "building" | "checking")) {
        return reply(409, "text/plain", "the game is being built");
    }
    let roots = {
        let settings = state.settings.lock().unwrap();
        if settings.scan_paths.is_empty() { storage::default_scan_roots() } else { settings.scan_paths.clone() }
    };
    if let Err(reason) = safe_delete_source(&src, &roots) {
        log!("{tid}: delete rejected {}: {reason}", src.path.display());
        return reply(409, "text/plain", reason);
    }
    if let Err(e) = register::unmount_title(tid) {
        log!("{tid}: unmount {}: {e}", src.path.display());
        return reply(409, "text/plain", e.to_string());
    }
    // Source first: if it cannot be removed, the cache (game-written files) and the tile stay.
    let result = match src.kind {
        Kind::Pkg => fs::remove_file(&src.path),
        Kind::Folder(_) => fs::remove_dir_all(&src.path),
    };
    if let Err(e) = result {
        log!("{tid}: delete source {}: {e}", src.path.display());
        // Unmounted above: the next pass remounts it (process: register::mount on a registered title).
        // ponytail: a folder remove_dir_all that failed midway leaves a partial source; not restored.
        state.wake();
        return reply(500, "text/plain", e.to_string());
    }
    if let Err(e) = register::uninstall(tid) {
        // ponytail: the tile may stay; the next scan does not re-register a deleted source.
        log!("{tid}: uninstall {}: {e}", src.path.display());
    }
    state.titles.lock().unwrap().remove(tid);
    state.games.lock().unwrap().retain(|g| g.src.path != src.path);
    state.wake();
    let cache_dir = cache::dir(tid);
    if cache_dir.exists() {
        if let Err(e) = fs::remove_dir_all(&cache_dir) {
            log!("{tid}: delete cache {}: {e}", cache_dir.display());
            return reply(500, "text/plain", format!("source deleted; cache not: {e}"));
        }
    }
    log!("{tid}: deleted {}", src.path.display());
    if state.settings.lock().unwrap().notify {
        sys::notify(&crate::i18n::deleted(crate::i18n::lang(), &src.title_name));
    }
    no_content()
}

// Lives in ps5/usb.c (IPMI to SceAutoMounterIpc, SMP sm_usb.c port).
extern "C" {
    fn km_usb_eject(path: *const libc::c_char, source: *const libc::c_char, fsid: u64, native_status: *mut i32) -> i32;
}

/// (mount point, source, fsid) of every mount. getfsstat into our own buffer: ps5/compat.c's
/// getmntinfo shares one static buffer with the launch watcher's thread.
fn mount_table() -> std::io::Result<Vec<(String, String, u64)>> {
    let mut extra = 8; // room for mounts added between the two calls
    for _ in 0..4 {
        let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buf: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; n as usize + extra];
        let bytes = (buf.len() * std::mem::size_of::<libc::statfs>()) as libc::c_long;
        let n = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // A full buffer may be a cut table: the dependency checks need every mount.
        if (n as usize) < buf.len() {
            return Ok(buf[..n as usize].iter().map(mount_entry).collect());
        }
        extra *= 4;
    }
    Err(std::io::Error::other("the mount table kept growing"))
}

fn mount_entry(fs: &libc::statfs) -> (String, String, u64) {
    // SMP sm_usb_filesystem_id: the 8-byte fsid as one u64.
    let fsid = unsafe { std::mem::transmute::<libc::fsid_t, u64>(fs.f_fsid) };
    // Bounded: a name that fills its field has no NUL.
    (crate::launch::c_field(&fs.f_mntonname), crate::launch::c_field(&fs.f_mntfromname), fsid)
}

/// The mount exactly at `path`: Ok(None) when the path resolves to its parent FS (an unmounted
/// /mnt/usbN reports /mnt) or is gone (ENOENT); any other statfs error is Err (SMP sm_usb_check_mount).
fn mount_at(path: &str) -> std::io::Result<Option<(String, String, u64)>> {
    let c = std::ffi::CString::new(path).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut fs) } != 0 {
        let e = std::io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::ENOENT) { Ok(None) } else { Err(e) };
    }
    Ok(Some(mount_entry(&fs)).filter(|m| m.0 == path))
}

/// SMP usb_disk_source_length: the length of "/dev/daN" when followed by the end, 'p' or 's'.
fn disk_len(source: &str) -> usize {
    let Some(rest) = source.strip_prefix("/dev/da") else { return 0 };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return 0;
    }
    match rest.as_bytes().get(digits) {
        None | Some(b'p' | b's') => 7 + digits,
        _ => 0,
    }
}

/// POST /api/eject: unmounts our titles on the drive, then asks the system to eject it (SMP
/// run_usb_eject_job). `work` is held throughout so the scan loop cannot remount meanwhile.
fn eject(state: &State, body: &[u8]) -> Reply {
    let path = String::from_utf8_lossy(body).into_owned();
    if !storage::ejectable(&path) {
        return reply(400, "text/plain", "not an ejectable drive");
    }
    let (source, fsid) = match mount_at(&path) {
        Ok(Some((_, source, fsid))) => (source, fsid),
        Ok(None) => return reply(404, "text/plain", "the drive is not mounted"),
        Err(e) => {
            log!("eject {path}: statfs: {e}");
            return reply(500, "text/plain", format!("cannot read the drive: {e}"));
        }
    };
    let Ok(work) = state.work.try_lock() else {
        return reply(409, "text/plain", "a scan or build is in progress; try again in a moment");
    };
    if state.paused() {
        return reply(409, "text/plain", "a game is running");
    }
    // ponytail: the drive's identity is taken now, not when the page was drawn; a drive swapped in
    // between is ejected. Harmless: it is only an unmount.
    // An unreadable table fails closed: the dependency checks below need it.
    let mounts = match mount_table() {
        Ok(m) => m,
        Err(e) => {
            log!("eject {path}: getfsstat: {e}");
            return reply(500, "text/plain", format!("cannot read the mount table: {e}"));
        }
    };
    let disk = disk_len(&source);
    let other = mounts.iter().find(|(on, from, _)| {
        *on != path && disk != 0 && disk_len(from) == disk && from[..disk] == source[..disk]
    });
    if let Some((on, from, _)) = other {
        log!("eject {path}: another partition of the drive is mounted: {on} ({from})");
        return reply(409, "text/plain", format!("another partition of this drive is mounted: {on}"));
    }
    let mut tids: Vec<&str> = mounts
        .iter()
        .filter_map(|(on, from, _)| {
            let tid = on.strip_prefix("/system_ex/app/").filter(|t| crate::scan::valid_tid(t))?;
            let from = from.strip_prefix("<above>:").unwrap_or(from);
            (from == path || from.starts_with(&format!("{path}/"))).then_some(tid)
        })
        .collect();
    tids.sort_unstable();
    tids.dedup();
    for tid in tids {
        if let Err(e) = register::unmount_title(tid) {
            log!("eject {path}: {tid}: unmount: {e}");
            drop(work);
            state.wake();
            return reply(409, "text/plain", e.to_string());
        }
    }
    // The launch watcher does not take `work`: a game may have started since the check above (SMP
    // rechecks too). ponytail: a game starting between here and the IPMI call is not caught; its
    // sandbox's /mnt/usbN alias then keeps the drive busy or goes with it.
    let result = if state.paused() {
        log!("eject {path}: a game started; not ejected");
        reply(409, "text/plain", "a game is running")
    } else {
        eject_and_confirm(&path, &source, fsid)
    };
    drop(work);
    // The next pass forgets the gone drive's games; on failure it remounts the titles unmounted above.
    state.wake();
    result
}

fn eject_and_confirm(path: &str, source: &str, fsid: u64) -> Reply {
    let (Ok(cpath), Ok(csource)) = (std::ffi::CString::new(path), std::ffi::CString::new(source)) else {
        return reply(400, "text/plain", "NUL in the path");
    };
    let mut native = 0i32;
    let rc = unsafe { km_usb_eject(cpath.as_ptr(), csource.as_ptr(), fsid, &mut native) };
    if rc == libc::ENOENT || rc == libc::ESTALE {
        log!("eject {path}: the mount changed (errno {rc})");
        return reply(409, "text/plain", "the drive changed; refresh and try again");
    }
    if rc != 0 {
        log!("eject {path}: failed: errno {rc}, native status {:#010x}", native as u32);
        return reply(502, "text/plain", format!("eject failed: errno {rc}, native status {:#010x}", native as u32));
    }
    for _ in 0..50 {
        // Only a seen unmount counts; a statfs error keeps polling (SMP: unconfirmed).
        if let Ok(None) = mount_at(path) {
            log!("eject {path}: done ({source})");
            return no_content();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    log!("eject {path}: requested, but still mounted after 5 s");
    reply(502, "text/plain", "unmount not confirmed; keep the drive connected and refresh")
}

/// The first of `names` in the source's sce_sys, else in /user/appmeta/<TID>/, else (a pkg) in the .pkg.
fn image(state: &State, tid: &str, names: &[&str]) -> Reply {
    if !crate::scan::valid_tid(tid) {
        return not_found();
    }
    let src = state.source(tid);
    let mut dirs: Vec<PathBuf> = src.iter().map(cache::sce_sys).collect();
    dirs.push(Path::new("/user/appmeta").join(tid));
    for dir in &dirs {
        for name in names {
            if let Ok(png) = std::fs::read(dir.join(name)) {
                return reply(200, "image/png", png);
            }
        }
    }
    // A pkg whose cache is not built (yet, or it failed): straight from the .pkg.
    if let Some(src) = src.filter(|s| s.kind == Kind::Pkg) {
        for name in names {
            match cache::pkg_file(&src.path, &format!("sce_sys/{name}")) {
                Ok(png) => return reply(200, "image/png", png),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log!("{tid}: image {name} from {}: {e}", src.path.display()),
            }
        }
    }
    not_found()
}
