//! Tiny HTTP/1.1 server for the web UI opened in the PS5 browser (Connection: close, no keep-alive).
//! Routes: DESIGN.md "Web API".
use crate::config::{self, Settings};
use crate::scan::{Kind, Package, Packing, PkgKind, Source};
use crate::{cache, register, storage, sys};
use std::collections::{HashMap, HashSet};
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
    /// The last scan's patch and DLC packages (the Install screen).
    pub packages: Mutex<Vec<Package>>,
    pub settings: Mutex<Settings>,
    /// Title ids to rebuild on the next scan.
    pub forced: Mutex<HashSet<String>>,
    /// Purge requested: done by the scan loop, so it never races a build.
    pub purge: AtomicBool,
    pub busy: AtomicBool,
    /// Start-up is waiting for this ("shadowmount", main::wait_ready); None once ready.
    pub waiting: Mutex<Option<&'static str>>,
    /// Held by the scan loop for a whole pass (scan, purge, build, register); delete takes it.
    pub work: Mutex<()>,
    pub low_space: AtomicBool,
    pub cache_total: Mutex<u64>,
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
            packages: Mutex::new(Vec::new()),
            settings: Mutex::new(settings),
            forced: Mutex::new(HashSet::new()),
            purge: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            waiting: Mutex::new(None),
            work: Mutex::new(()),
            low_space: AtomicBool::new(false),
            cache_total: Mutex::new(0),
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

/// `playgo_off`: the setting's title ids; None while add_emulators is off (no stub at all, so the
/// UI shows no switch).
fn game_json(g: &Game, playgo_off: Option<&[String]>, crash: Option<&crate::crash::Crash>) -> serde_json::Value {
    // An install run through ng is still the console's install to the UI.
    let (kind, packing) = if crate::scan::installed_pkg(&g.src) { ("installed", None) } else { kind_json(g.src.kind) };
    serde_json::json!({
        "title_id": g.src.title_id,
        "title_name": g.src.title_name,
        "version": g.src.version,
        "path": g.src.path.to_string_lossy(),
        "kind": kind,
        "packing": packing,
        "drive": g.src.drive,
        "size": g.src.size,
        "status": g.status,
        "error": g.error,
        "cache_size": g.cache_size,
        "cache_built": g.cache_built,
        "backport": g.src.backport.as_ref().map(|b| b.to_string_lossy()),
        // ponytail: reads the game's .ampr on every state poll (a few hundred bytes per game).
        "playgo": playgo_off.and_then(|_| crate::cache::playgo_import(&g.src.title_id)),
        "playgo_off": playgo_off.is_some_and(|l| l.iter().any(|t| *t == g.src.title_id)),
        "crash": crash.map(|c| serde_json::json!({"at": c.at, "process": c.process, "status": c.status})),
    })
}

fn kind_json(kind: Kind) -> (&'static str, Option<&'static str>) {
    match kind {
        Kind::Pkg => ("pkg", None),
        Kind::Folder(Packing::Plain) => ("folder", Some("plain")),
        Kind::Folder(Packing::Zstd) => ("folder", Some("zstd")),
        Kind::Folder(Packing::Lz4) => ("folder", Some("lz4")),
        Kind::Installed => ("installed", None),
    }
}

/// One entry per `exclude` path; a path that is not a readable game has nulls.
fn hidden_json(exclude: &[String]) -> Vec<serde_json::Value> {
    exclude.iter().map(|p| match crate::scan::read_source(Path::new(p)) {
        Some(s) => {
            let (kind, packing) = kind_json(s.kind);
            serde_json::json!({"path": p, "title_id": s.title_id, "title_name": s.title_name, "version": s.version,
                "kind": kind, "packing": packing, "drive": s.drive, "size": s.size})
        }
        None => serde_json::json!({"path": p, "title_id": null, "title_name": null, "version": null,
            "kind": null, "packing": null, "drive": null, "size": null}),
    }).collect()
}

/// One row of the Install screen's lists (GET /api/install); `path` is also its id.
#[derive(Debug, PartialEq)]
struct InstallRow<'a> {
    path: &'a Path,
    title_id: &'a str,
    name: &'a str,
    version: &'a str,
    /// game | patch | dlc
    kind: &'static str,
    size: u64,
    drive: &'a str,
    /// The installed copy's version; None when the title is not installed.
    installed_version: Option<&'a str>,
    /// The installed copy is a retail install.
    retail: bool,
}

/// (installable, kept). installable: patch and DLC packages not installed yet; kept: .pkg duplicates
/// of an installed title, and patch/DLC packages already installed. `installed`: title id ->
/// (installed version, retail); `dlc_in(content id)`: the console reports that DLC installed.
/// A patch counts as installed when the installed version (appmeta, which a patch install updates)
/// is not older than its own; one with no version stays installable.
/// ponytail: no `/user/patch/<TID>` read; a patch whose version the installed appmeta does not
/// show stays installable.
fn install_lists<'a>(packages: &'a [Package], duplicates: &'a [Source], installed: &HashMap<&str, (&'a str, bool)>,
    dlc_in: &dyn Fn(&str) -> bool) -> (Vec<InstallRow<'a>>, Vec<InstallRow<'a>>) {
    let row = |path: &'a Path, title_id: &'a str, name: &'a str, version: &'a str, kind, size, drive: &'a str| {
        let have = installed.get(title_id);
        InstallRow { path, title_id, name, version, kind, size, drive, installed_version: have.map(|h| h.0),
            retail: have.is_some_and(|h| h.1) }
    };
    let mut kept: Vec<InstallRow> = duplicates.iter()
        .filter(|d| d.kind == Kind::Pkg
            && installed.get(d.title_id.as_str()).is_some_and(|h| !ver_less(h.0, &d.version)))
        .map(|d| row(&d.path, &d.title_id, &d.title_name, &d.version, "game", d.size, &d.drive))
        .collect();
    let mut installable = Vec::new();
    for p in packages {
        let (kind, done) = match p.kind {
            PkgKind::Base => continue,
            PkgKind::Patch => ("patch", !p.version.is_empty()
                && installed.get(p.title_id.as_str()).is_some_and(|h| !ver_less(h.0, &p.version))),
            PkgKind::Dlc => ("dlc", dlc_in(&p.content_id)),
        };
        let r = row(&p.path, &p.title_id, &p.title_name, &p.version, kind, p.size, &p.drive);
        if done { kept.push(r) } else { installable.push(r) }
    }
    (installable, kept)
}

/// Dotted versions ("01.010"), compared number by number: `a` is older than `b`.
fn ver_less(a: &str, b: &str) -> bool {
    let num = |s: &str| s.split('.').map(|p| p.trim().parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    let (mut x, mut y) = (num(a), num(b));
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    x < y
}

/// Delete, hide and uninstall refuse the console's own install (register::uninstall would remove it).
fn installed_refusal(src: &Source) -> Option<&'static str> {
    (src.kind == Kind::Installed || crate::scan::installed_pkg(src)).then_some("an installed game: uninstall it from the console's menu")
}

/// Single instance: binds PORT, asking an older copy on it to quit first (POST /api/quit),
/// retrying for 5 s. None: the port stays taken (an older build without /api/quit, or another
/// app), so this copy must not run.
pub fn take_port() -> Option<TcpListener> {
    if let Ok(l) = TcpListener::bind(("0.0.0.0", PORT)) {
        return Some(l);
    }
    for _ in 0..20 {
        // Asked again each try: the older copy refuses while a scan pass (a build) runs.
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", PORT)) {
            let _ = s.write_all(b"POST /api/quit HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n");
        }
        std::thread::sleep(Duration::from_millis(250));
        if let Ok(l) = TcpListener::bind(("0.0.0.0", PORT)) {
            return Some(l);
        }
    }
    None
}

/// Listens on 0.0.0.0:PORT forever, one short-lived thread per connection. Once bound, registers
/// the home-screen tile that opens the UI (once, in its own thread).
pub fn serve(state: Arc<State>, mut listener: TcpListener) {
    log!("web UI on port {PORT}");
    std::thread::spawn(|| crate::register::register_tile(PORT));
    loop {
        match listener.accept() {
            Ok((conn, _)) => {
                let state = state.clone();
                std::thread::spawn(move || {
                    let _ = handle(&state, conn, false);
                });
            }
            Err(e) => {
                // Rest mode can close the listening socket (#110: after a wake the UI was gone and a
                // second copy could bind the port). ponytail: any accept error rebinds (a transient one
                // only drops the backlog); a dead socket whose accept blocks without an error is not seen.
                log!("web: accept on port {PORT} failed: {e}; binding it again");
                drop(listener);
                listener = rebind(&state);
                log!("web UI on port {PORT} again");
            }
        }
    }
}

/// ShadowMount+'s default API port: clients that only know ShadowMount+ (Playden, RecompStore)
/// on the console reach /api/v1 here (#126). Loopback only on purpose (SMP binds 0.0.0.0): ps5upload
/// on a PC probes this port and would see both mounters running.
pub const SMP_PORT: u16 = 10101;

/// Whether a request on SMP_PORT is served: only /api/v1 (the UI, /pkg and the native API stay on PORT).
fn smp_request(head: &str) -> bool {
    head.split_whitespace().nth(1).is_some_and(|t| t.starts_with("/api/v1/"))
}

/// Serves /api/v1 on 127.0.0.1:SMP_PORT forever. The port is busy while ShadowMount+ still runs:
/// binds every 2 s until it is free, and again after an accept error (rest mode, as serve).
pub fn serve_smp(state: Arc<State>) {
    let mut logged = false;
    loop {
        let listener = match TcpListener::bind(("127.0.0.1", SMP_PORT)) {
            Ok(l) => l,
            Err(e) => {
                if !logged {
                    logged = true;
                    log!("web: ShadowMount+ API port {SMP_PORT} busy ({e}); retrying every 2 s");
                }
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        logged = false;
        log!("ShadowMount+ API on 127.0.0.1:{SMP_PORT}");
        loop {
            match listener.accept() {
                Ok((conn, _)) => {
                    let state = state.clone();
                    std::thread::spawn(move || {
                        let _ = handle(&state, conn, true);
                    });
                }
                Err(e) => {
                    log!("web: accept on port {SMP_PORT} failed: {e}; binding it again");
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Binds PORT again, retrying every second. A newer copy that took the port meanwhile wins: this
/// one quits, unless a game runs (its overlay would stay mounted; /api/quit refuses then too).
fn rebind(state: &State) -> TcpListener {
    let mut logged = false;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        match TcpListener::bind(("0.0.0.0", PORT)) {
            Ok(l) => return l,
            // An install does not keep this copy: its /pkg URL is on PORT, which the other copy now
            // answers (404), and two copies would scan at once. The install fails; its marker left
            // behind means not installed (as a payload killed mid-install).
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && !state.paused() => {
                if crate::install::active() {
                    log!("web: port {PORT} taken by another copy; quitting, the running install fails");
                } else {
                    log!("web: port {PORT} taken by another copy; quitting");
                }
                std::process::exit(0)
            }
            Err(e) if !logged => {
                logged = true;
                log!("web: bind port {PORT} failed: {e}; retrying every second");
            }
            Err(_) => {}
        }
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

/// A refused request: HTTP code, FreeBSD errno (the /api/v1 `status`) and the text.
#[derive(Debug, PartialEq)]
struct Fail {
    code: u16,
    errno: i32,
    msg: String,
}

fn fail(code: u16, errno: i32, msg: impl Into<String>) -> Fail {
    Fail { code, errno, msg: msg.into() }
}

impl Fail {
    /// The native routes' plain-text answer.
    fn text(self) -> Reply {
        reply(self.code, "text/plain", self.msg)
    }

    /// The /api/v1 answer (SMP API v1 shape).
    fn json(self) -> Reply {
        reply(self.code, "application/json", serde_json::json!({"status": self.errno, "error": self.msg}).to_string())
    }
}

/// A failed config::save as a Fail.
fn save_fail(e: std::io::Error) -> Fail {
    fail(500, e.raw_os_error().unwrap_or(libc::EIO), e.to_string())
}

/// Puts `item` into `list` (`on`) or takes it out; true when the list changed.
fn set_member(list: &mut Vec<String>, item: &str, on: bool) -> bool {
    let had = list.iter().any(|x| x == item);
    if on && !had {
        list.push(item.to_string());
    } else if !on && had {
        list.retain(|x| x != item);
    }
    on != had
}

/// An `exclude` entry as config.rs stores it: absolute, no trailing `/` (except "/" itself).
fn exclude_entry(path: &str) -> Option<String> {
    // A control character (newline, tab) does not survive config.toml's string syntax.
    if !path.starts_with('/') || path.chars().any(char::is_control) {
        return None;
    }
    let p = path.trim_end_matches('/');
    Some(if p.is_empty() { "/" } else { p }.to_string())
}

/// Changes the saved settings with `f` (on the file's copy: a scan pass may have put an older copy
/// into state.settings), held under the settings lock so a concurrent change is not lost. When `f`
/// reports a change: saves, applies and wakes the scan loop. Ok(changed); a save error is logged.
fn change_settings(state: &State, what: &str, f: impl FnOnce(&mut Settings) -> bool) -> std::io::Result<bool> {
    let mut cur = state.settings.lock().unwrap();
    let mut s = config::try_load().inspect_err(|e| log!("{what}: read {}: {e}", config::CONFIG_PATH))?;
    if !f(&mut s) {
        return Ok(false);
    }
    if let Err(e) = config::save(&s) {
        log!("{what}: save {}: {e}", config::CONFIG_PATH);
        return Err(e);
    }
    *cur = s;
    drop(cur);
    state.wake();
    Ok(true)
}

/// One request; `smp`: it came in on SMP_PORT, which answers only /api/v1.
fn handle(state: &State, mut conn: TcpStream, smp: bool) -> std::io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(5)))?;
    conn.set_write_timeout(Some(Duration::from_secs(10)))?;
    let loopback = conn.peer_addr().map(|a| a.ip().is_loopback()).unwrap_or(false);
    let r = match read_request(&mut conn)? {
        Ok((head, _)) if smp && !smp_request(&head) => fail(404, libc::ENOENT, "unknown route").json(),
        // The installer fetching the package of the running install (install.rs).
        Ok((head, _)) if loopback && is_pkg_request(&head) => {
            crate::install::serve_conn(conn, &head);
            return Ok(());
        }
        Ok((head, body)) => route(state, &head, &body, loopback),
        Err(code) => reply(code, "text/plain", "bad or too large request"),
    };
    let reason = match r.code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        501 => "Not Implemented",
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

/// `GET|HEAD /pkg/...`: a request for install.rs `serve_conn`.
fn is_pkg_request(head: &str) -> bool {
    let mut first = head.split_whitespace();
    matches!(first.next(), Some("GET" | "HEAD")) && first.next().is_some_and(|t| t.starts_with("/pkg/"))
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
        ("POST", p) if p.starts_with("/api/sizes/") => duplicate_size(state, &p["/api/sizes/".len()..], body),
        ("GET", p) if p.starts_with("/api/report/") => report(state, &p["/api/report/".len()..]),
        ("GET", "/api/hidden") => {
            let exclude = state.settings.lock().unwrap().exclude.clone();
            reply(200, "application/json", serde_json::Value::from(hidden_json(&exclude)).to_string())
        }
        ("GET", "/api/settings") => {
            let v = config::to_json(&state.settings.lock().unwrap());
            reply(200, "application/json", v.to_string())
        }
        ("POST", "/api/settings") => post_settings(state, body),
        ("GET", "/api/install") => get_install(state),
        ("POST", "/api/install") => post_install(state, body),
        ("POST", "/api/install/cancel") => {
            if crate::install::cancel() { no_content() } else { reply(404, "text/plain", "no install is running") }
        }
        ("POST", "/api/install/delete-source") => delete_source(state, body),
        ("GET", "/api/ampr") => reply(200, "application/json", crate::ampr_status().to_string()),
        // The UI's "Update now": forced, one attempt, answered when done (one thread per request).
        ("POST", "/api/ampr/update") => match crate::ampr_update(state, true, 1) {
            Ok(m) => reply(200, "text/plain", m),
            Err(e) if e.contains("already running") => reply(409, "text/plain", e),
            Err(e) => reply(502, "text/plain", e),
        },
        ("POST", "/api/self_update") => self_update(state, body),
        // A newer copy taking over the port (loopback, empty body) or the UI's Shut down (body
        // "shutdown", from any host, as Dump Forge's Stop).
        ("POST", "/api/quit") if (loopback || body == b"shutdown") && state.paused() => {
            log!("quit refused: a game is running (its overlay would stay mounted)");
            reply(409, "text/plain", "a game is running")
        }
        ("POST", "/api/quit") if (loopback || body == b"shutdown") && crate::install::active() => {
            log!("quit refused: an install is running or queued");
            reply(409, "text/plain", "an install is running")
        }
        ("POST", "/api/quit") if loopback || body == b"shutdown" => {
            let user = body == b"shutdown";
            // A newer copy waits for the scan pass: quitting mid-build would leave a cache without
            // `.built`, which the next start empties (game-written files included). Held until exit,
            // so no pass starts meanwhile.
            if !user {
                let Ok(work) = state.work.try_lock() else {
                    return reply(409, "text/plain", "a scan or build is in progress");
                };
                std::mem::forget(work);
            }
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
        ("POST", p) if p.starts_with("/api/playgo/") => post_playgo(state, &p["/api/playgo/".len()..], body),
        ("POST", p) if p.starts_with("/api/overlay/") => post_overlay(state, &p["/api/overlay/".len()..], body),
        ("POST", p) if p.starts_with("/api/ng/") => post_ng(state, &p["/api/ng/".len()..], body),
        ("POST", p) if p.starts_with("/api/v1/") => v1(state, &p["/api/v1/".len()..], body),
        ("POST", p) if p.starts_with("/api/rebuild/") => rebuild(state, &p["/api/rebuild/".len()..]),
        ("POST", p) if p.starts_with("/api/delete/") => delete(state, &p["/api/delete/".len()..], body),
        ("POST", p) if p.starts_with("/api/launch/") => launch(state, &p["/api/launch/".len()..]),
        ("POST", p) if p.starts_with("/api/close/") => close(state, &p["/api/close/".len()..]),
        ("GET", "/api/qr") => qr(head),
        ("POST", p) if p.starts_with("/api/hide/") => match uninstall_keep(state, &p["/api/hide/".len()..], Some(body)) {
            Ok(_) => no_content(),
            Err(f) => f.text(),
        },
        ("POST", "/api/unhide") => unhide(state, body),
        ("POST", "/api/hidden/delete") => delete_hidden(state, body),
        ("POST", "/api/unhide-all") => unhide_all(state),
        ("POST", p) if p.starts_with("/api/backport/delete/") => match delete_backport(state, &p["/api/backport/delete/".len()..]) {
            Ok(()) => no_content(),
            Err(f) => f.text(),
        },
        ("POST", "/api/backports/delete-all") => delete_backports(state),
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
    // Held across the save: a concurrent /api/playgo change is not lost.
    let mut cur = state.settings.lock().unwrap();
    let file = match config::try_load() {
        Ok(f) => f,
        Err(e) => {
            log!("settings: read {}: {e}; not saved", config::CONFIG_PATH);
            return reply(500, "text/plain", format!("cannot read {}: {e}", config::CONFIG_PATH));
        }
    };
    // The body's keys over the file's: a client with an older copy (another browser, a hand edit)
    // only changes what it sends.
    let parsed = serde_json::from_slice::<serde_json::Value>(body)
        .map_err(|e| format!("settings: {e}"))
        .and_then(|v| {
            let mut m = config::to_json(&file);
            let (Some(mo), Some(vo)) = (m.as_object_mut(), v.as_object()) else { return Err("settings: expected a JSON object".to_string()) };
            mo.extend(vo.iter().map(|(k, x)| (k.clone(), x.clone())));
            Ok((config::from_json(&m)?, v))
        });
    let (mut s, v) = match parsed {
        Ok(s) => s,
        Err(e) => {
            log!("settings rejected: {e}");
            return reply(400, "text/plain", e);
        }
    };
    // Lists the body did not send stay exactly as in the file: from_json would trim their items.
    if v.get("scanpath").is_none() {
        s.scan_paths.clone_from(&file.scan_paths);
    }
    if v.get("exclude").is_none() {
        s.exclude.clone_from(&file.exclude);
    }
    if v.get("playgo_off").is_none() {
        s.playgo_off.clone_from(&file.playgo_off);
    }
    if v.get("ampr_installed_on").is_none() {
        s.ampr_installed_on.clone_from(&file.ampr_installed_on);
    }
    if v.get("ampr_installed_off").is_none() {
        s.ampr_installed_off.clone_from(&file.ampr_installed_off);
    }
    let sent_overlay = v.get("overlay_off").is_some();
    if !sent_overlay {
        s.overlay_off.clone_from(&file.overlay_off);
    }
    if let Some(t) = s.overlay_off.iter().filter(|_| sent_overlay).find(|t| state.source(t).is_some_and(|src| src.kind == Kind::Pkg && !crate::scan::installed_pkg(&src))) {
        log!("settings rejected: {t}: overlay_off for a pkg game");
        return reply(409, "text/plain", format!("{t}: a pkg game needs its overlay"));
    }
    if let Err(e) = config::save(&s) {
        log!("settings: save {}: {e}", config::CONFIG_PATH);
        return reply(500, "text/plain", e.to_string());
    }
    log!("settings saved");
    let rescan = !config::ui_only_change(&cur, &s);
    *cur = s;
    drop(cur);
    if rescan {
        state.wake();
    }
    no_content()
}

/// POST /api/self_update, body `{"version":"<tag>"}`: downloads the newer release found at start
/// and sends it to elfldr; the new copy then takes over the port. 200 `{}`, else update::install's code.
fn self_update(state: &State, body: &[u8]) -> Reply {
    let v: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let Some(version) = v.as_ref().and_then(|v| v["version"].as_str()) else {
        return reply(400, "text/plain", "expected {\"version\":\"<tag>\"}");
    };
    // The new copy's quit waits for the pass and take_port gives up after 5 s: install holds off new
    // passes (update::holding) and checks that none runs.
    if crate::install::active() {
        return reply(409, "text/plain", "an install is running");
    }
    match crate::update::install(version, &|| state.paused(), &|| state.work.try_lock().is_ok()) {
        Ok(()) => reply(200, "application/json", "{}"),
        Err((code, e)) => reply(code, "text/plain", e),
    }
}

/// POST /api/playgo/<TID>, body `on` or `off`: the bundled PlayGo stub for one game (playgo_off).
fn post_playgo(state: &State, tid: &str, body: &[u8]) -> Reply {
    if !crate::scan::valid_tid(tid) || state.source(tid).is_none() {
        return not_found();
    }
    let off = match body {
        b"on" => false,
        b"off" => true,
        _ => return reply(400, "text/plain", "expected on or off"),
    };
    if let Err(e) = change_settings(state, &format!("{tid}: PlayGo stub"), |s| set_member(&mut s.playgo_off, tid, off)) {
        return reply(500, "text/plain", e.to_string());
    }
    log!("{tid}: PlayGo stub {}", if off { "off" } else { "on" });
    no_content()
}

/// POST /api/ng/<TID>, body `on` or `off`: ampr-emu-ng for one installed game. The value of the
/// global `ampr_installed` clears the title's override (it follows the global switch again).
fn post_ng(state: &State, tid: &str, body: &[u8]) -> Reply {
    let Some(src) = Some(tid).filter(|t| crate::scan::valid_tid(t)).and_then(|t| state.source(t)) else {
        return not_found();
    };
    let on = match body {
        b"on" => true,
        b"off" => false,
        _ => return reply(400, "text/plain", "expected on or off"),
    };
    if src.kind != Kind::Installed && !crate::scan::installed_pkg(&src) {
        return reply(409, "text/plain", "not an installed game");
    }
    let r = change_settings(state, &format!("{tid}: ampr-emu-ng"), |s| {
        let differs = on != s.ampr_installed;
        set_member(&mut s.ampr_installed_on, tid, on && differs) | set_member(&mut s.ampr_installed_off, tid, !on && differs)
    });
    match r {
        Ok(changed) => {
            if changed {
                log!("{tid}: ampr-emu-ng {}", if on { "on" } else { "off" });
            }
            no_content()
        }
        Err(e) => reply(500, "text/plain", e.to_string()),
    }
}

/// The fakelib overlay of one title on (`off` false) or off (overlay_off). Off is refused for a pkg:
/// it needs ampr-emu-ng's pkg mode. `unlisted`: a valid title id not in the library is set too
/// (games/fakelib: a launcher sets it before the title's folder is first scanned, which waits while
/// the launcher runs, so the first build and launch already go without the overlay); else 404.
/// Ok(listed).
fn set_overlay(state: &State, tid: &str, off: bool, unlisted: bool) -> Result<bool, Fail> {
    if !crate::scan::valid_tid(tid) {
        return Err(fail(404, libc::ENOENT, "not found"));
    }
    let src = state.source(tid);
    if src.is_none() && !unlisted {
        return Err(fail(404, libc::ENOENT, "not found"));
    }
    // ponytail: an unlisted title that turns out to be a pkg keeps its overlay anyway (cache.rs
    // overlay_off ignores pkg sources); only a listed pkg is refused here.
    if let Some(src) = src.as_ref().filter(|s| off && s.kind == Kind::Pkg) {
        log!("{tid}: overlay off refused: {} is a pkg (needs ampr-emu-ng's pkg mode)", src.path.display());
        return Err(fail(409, libc::EPERM, "a pkg game needs its overlay (ampr-emu-ng's pkg mode)"));
    }
    if change_settings(state, &format!("{tid}: overlay"), |s| set_member(&mut s.overlay_off, tid, off)).map_err(save_fail)? {
        log!("{tid}: overlay {}{}", if off { "off" } else { "on" }, if src.is_none() { " (not in the library yet)" } else { "" });
    }
    Ok(src.is_some())
}

/// POST /api/overlay/<TID>, body `on` or `off`.
fn post_overlay(state: &State, tid: &str, body: &[u8]) -> Reply {
    let off = match body {
        b"on" => false,
        b"off" => true,
        _ if !crate::scan::valid_tid(tid) || state.source(tid).is_none() => return not_found(),
        _ => return reply(400, "text/plain", "expected on or off"),
    };
    match set_overlay(state, tid, off, false) {
        Ok(_) => no_content(),
        Err(f) => f.text(),
    }
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
    let mut b = Vec::new();
    match std::fs::File::open(path).and_then(|f| f.take(256 * 1024).read_to_end(&mut b)) {
        Ok(_) => {
            reply(200, "text/plain; charset=utf-8", b)
        }
        Err(_) => not_found(),
    }
}

/// kraken.log lines (with the title id) at the end of a game's report.
const REPORT_LOG_LINES: usize = 40;

/// GET /api/report/<TID>: plain text to paste into an issue; 404 for an invalid or absent library title.
fn report(state: &State, tid: &str) -> Reply {
    if !crate::scan::valid_tid(tid) {
        return not_found();
    }
    let Some(g) = state.games.lock().unwrap().iter().find(|g| g.src.title_id == tid && g.status != "duplicate").cloned()
    else {
        return not_found();
    };
    let s = state.settings.lock().unwrap().clone();
    let ampr = crate::ampr_status();
    let crash = crate::crash::load().remove(tid);
    let log = log_tail();
    let log = String::from_utf8_lossy(&log);
    reply(200, "text/plain; charset=utf-8", report_text(&g, &s, &ampr, crash.as_ref(), &log))
}

fn report_text(g: &Game, s: &Settings, ampr: &serde_json::Value, crash: Option<&crate::crash::Crash>, log: &str) -> String {
    let src = &g.src;
    let kind = match src.kind {
        Kind::Pkg => "pkg",
        Kind::Folder(Packing::Plain) => "folder",
        Kind::Folder(Packing::Zstd) => "folder (zstd)",
        Kind::Folder(Packing::Lz4) => "folder (lz4)",
        Kind::Installed => "installed",
    };
    let ampr = format!("{} {}", ampr["kind"].as_str().unwrap_or("?"), ampr["version"].as_str().unwrap_or("")).trim_end().to_string();
    let playgo = if !s.add_emulators { "not added (add_emulators off)" }
        else if s.playgo_off.iter().any(|t| *t == src.title_id) { "off" } else { "on" };
    let crash = crash.map_or("none recorded".to_string(), |c| {
        format!("{} in {}, {} (console dump folder {})", sys::utc(c.at as libc::time_t), c.process, c.status, c.folder)
    });
    let mut out = format!(
        "Kraken Mount {}\nFirmware: (please fill in)\nGame: {} ({}), version {}\nSource: {} ({kind}, {})\n\
         Status: {}{}\nampr-emu-ng: {ampr}\nPlayGo stub: {playgo}\nLast crash: {crash}\n",
        env!("CARGO_PKG_VERSION"), src.title_name, src.title_id, src.version, src.path.display(), src.drive,
        g.status, g.error.as_deref().map_or(String::new(), |e| format!(" ({e})")),
    );
    let lines: Vec<&str> = log.lines().filter(|l| l.contains(src.title_id.as_str())).collect();
    out += &format!("\nkraken.log, last {} lines with {}:\n", REPORT_LOG_LINES, src.title_id);
    for l in &lines[lines.len().saturating_sub(REPORT_LOG_LINES)..] {
        out += l;
        out.push('\n');
    }
    out
}

fn api_state(state: &State) -> Vec<u8> {
    let playgo_off = {
        let s = state.settings.lock().unwrap();
        s.add_emulators.then(|| s.playgo_off.clone())
    };
    let (overlay_off, settings) = {
        let s = state.settings.lock().unwrap();
        (s.overlay_off.clone(), s.clone())
    };
    let crashes = crate::crash::load();
    // A rebuild asked for but not started yet (the pass picks it up, or a game is running).
    let queued = state.forced.lock().unwrap().clone();
    // A copy: presence reads app.db, the mount table and the sources without holding the lock.
    let all_games = state.games.lock().unwrap().clone();
    let installed = installed_map(&all_games);
    let install = install_summary(state, &all_games, &installed);
    let games: Vec<_> = all_games.iter().zip(presence(&all_games)).map(|(g, [registered, mounted, present])| {
        let mut v = game_json(g, playgo_off.as_deref(), crashes.get(&g.src.title_id));
        // The title has an installed copy (that row itself, or a Kraken source it beat).
        let have = installed.get(g.src.title_id.as_str());
        v["installed"] = have.is_some().into();
        // The game sheet's ampr-emu-ng switch: installed rows only (null on the others).
        let installed_row = g.src.kind == Kind::Installed || crate::scan::installed_pkg(&g.src);
        v["ng_off"] = installed_row.then(|| !settings.ng_installed(&g.src.title_id)).into();
        v["installed_version"] = have.map(|h| h.0).into();
        // Only the installed row: the Retail badge is the install's, not a .pkg's.
        v["retail"] = (g.src.kind == Kind::Installed && have.is_some_and(|h| h.1)).into();
        v["rebuild_queued"] = queued.contains(&g.src.title_id).into();
        v["registered"] = registered.into();
        v["mounted"] = mounted.into();
        v["source_present"] = present.into();
        // As cache.rs: a pkg source keeps its fakelib whatever overlay_off says.
        v["overlay_off"] = (g.src.kind != Kind::Pkg && overlay_off.contains(&g.src.title_id)).into();
        v
    }).collect();
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
        "update": crate::update::available(),
        "busy": state.busy.load(Ordering::SeqCst),
        "paused": !running.is_empty(),
        "waiting": *state.waiting.lock().unwrap(),
        "running": running,
        "games": games,
        "install": install,
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

/// Per game: [registered, mounted (/system_ex/app/<TID> is a mount point), source present], from
/// one app.db read and one mount table for all of them.
fn presence(games: &[Game]) -> Vec<[bool; 3]> {
    let tids: Vec<&str> = games.iter().map(|g| g.src.title_id.as_str()).collect();
    let registered = register::registered(&tids);
    static FAILED: AtomicBool = AtomicBool::new(false);
    let mounts: HashSet<String> = match mount_table() {
        Ok(m) => {
            FAILED.store(false, Ordering::Relaxed);
            m.into_iter().map(|(on, _, _)| on).collect()
        }
        Err(e) => {
            // Once until it reads again: /api/state polls this every second or two per open UI.
            if !FAILED.swap(true, Ordering::Relaxed) {
                log!("title status: getfsstat: {e}; titles shown as not mounted");
            }
            HashSet::new()
        }
    };
    games.iter().zip(registered).map(|(g, registered)| {
        [registered, mounts.contains(&format!("/system_ex/app/{}", g.src.title_id)), g.src.path.exists()]
    }).collect()
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

/// The library's installed copies: title id -> (version, retail).
/// ponytail: retail reads 8 bytes of each installed app.pkg on every call (/api/state polls it).
fn installed_map(games: &[Game]) -> HashMap<&str, (&str, bool)> {
    games.iter().filter(|g| (g.src.kind == Kind::Installed || crate::scan::installed_pkg(&g.src)) && g.status != "duplicate").map(|g| {
        let mut head = [0u8; 8];
        let pkg = if g.src.kind == Kind::Installed { g.src.path.join("app.pkg") } else { g.src.path.clone() };
        let retail = fs::File::open(pkg).and_then(|mut f| f.read_exact(&mut head)).is_ok()
            && crate::installed::retail_header(&head);
        (g.src.title_id.as_str(), (g.src.version.as_str(), retail))
    }).collect()
}

/// sceAppInstUtilGetAddcontInstalledStatus of `cid`, remembered for a minute or until the install job
/// records a new result: each call swaps the process credentials (installer.c), and /api/state polls.
/// ponytail: a DLC removed from the console menu still shows as kept for up to a minute.
/// `ask` false: the remembered answer only (false when none), never the native call, which waits
/// behind an install or register's InstallAll on the shared lock. With `ask`, the remembered
/// answer too while such a long call runs (`install::appinst_busy`): no web thread waits on it.
fn dlc_installed(cid: &str, results: &[(String, String)], ask: bool) -> bool {
    /// sceAppInstUtilGetAddcontInstalledStatus: no installed-content record.
    const NOT_FOUND: i32 = 0x80E9_0004_u32 as i32;
    type Memo = (Vec<(String, String)>, HashMap<String, (bool, std::time::Instant)>);
    static MEMO: Mutex<Option<Memo>> = Mutex::new(None);
    {
        let mut memo = MEMO.lock().unwrap_or_else(|e| e.into_inner());
        let (seen, map) = memo.get_or_insert_with(Default::default);
        if seen.as_slice() != results {
            *seen = results.to_vec();
            map.clear();
        }
        let had = map.get(cid);
        if !ask || crate::install::appinst_busy()
            || had.is_some_and(|(_, at)| at.elapsed() < Duration::from_secs(60)) {
            return had.is_some_and(|(on, _)| *on);
        }
    } // MEMO is not held across the native call
    let on = match crate::install::addcont_installed(cid) {
        Ok(on) => on,
        Err(NOT_FOUND) => false,
        Err(rc) => {
            log!("install: DLC {cid}: installed status failed ({:#010x}); listed as not installed", rc as u32);
            false
        }
    };
    let mut memo = MEMO.lock().unwrap_or_else(|e| e.into_inner());
    memo.get_or_insert_with(Default::default).1.insert(cid.to_string(), (on, std::time::Instant::now()));
    on
}

fn row_json(r: &InstallRow) -> serde_json::Value {
    let path = r.path.to_string_lossy();
    serde_json::json!({"id": path, "path": path, "title_id": r.title_id, "name": r.name, "version": r.version,
        "kind": r.kind, "size": r.size, "drive": r.drive, "installed_version": r.installed_version, "retail": r.retail})
}

/// (installable, kept) of GET /api/install, from the last scan (`games`: State.games). `ask`: see
/// dlc_installed.
fn install_rows(state: &State, games: &[Game], installed: &HashMap<&str, (&str, bool)>,
    results: &[(String, String)], ask: bool) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let packages = state.packages.lock().unwrap().clone();
    let dups: Vec<Source> = games.iter().filter(|g| g.status == "duplicate").map(|g| g.src.clone()).collect();
    let (can, kept) = install_lists(&packages, &dups, installed, &|cid| dlc_installed(cid, results, ask));
    (can.iter().map(row_json).collect(), kept.iter().map(row_json).collect())
}

fn job_json(j: &crate::install::Job) -> serde_json::Value {
    serde_json::json!({"id": j.id, "title_id": j.title_id, "name": j.name, "kind": j.kind, "done_bytes": j.done_bytes,
        "total_bytes": j.total_bytes, "phase": j.phase, "error": j.error})
}

/// GET /api/install.
fn get_install(state: &State) -> Reply {
    let (current, queue, results) = crate::install::snapshot();
    let games = state.games.lock().unwrap().clone();
    let (installable, kept) = install_rows(state, &games, &installed_map(&games), &results, true);
    reply(200, "application/json", serde_json::json!({
        "current": current.as_ref().map(job_json),
        "queue": queue.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(),
        "results": results,
        "installable": installable,
        "kept": kept,
    }).to_string())
}

/// The rail item of /api/state: running, percent of the running install, queue + installable + kept.
fn install_summary(state: &State, games: &[Game], installed: &HashMap<&str, (&str, bool)>) -> serde_json::Value {
    let (current, queue, results) = crate::install::snapshot();
    // No native DLC call per /api/state poll: a DLC counts once either way (installable or kept).
    let (can, kept) = install_rows(state, games, installed, &results, false);
    let percent = current.as_ref().filter(|j| j.total_bytes > 0)
        .map(|j| (j.done_bytes.saturating_mul(100) / j.total_bytes).min(100) as u8);
    serde_json::json!({"running": current.is_some(), "percent": percent, "count": queue.len() + can.len() + kept.len()})
}

/// The path a POST body names; Err: 400.
fn body_path(body: &[u8]) -> Result<PathBuf, Reply> {
    match std::str::from_utf8(body).map(str::trim) {
        Ok(p) if !p.is_empty() => Ok(PathBuf::from(p)),
        _ => Err(reply(400, "text/plain", "body: expected the path of a .pkg")),
    }
}

/// POST /api/install, body = a library .pkg game's path (a duplicate too: the update of an installed
/// copy) or a patch/DLC package's path.
fn post_install(state: &State, body: &[u8]) -> Reply {
    let path = match body_path(body) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let game = state.games.lock().unwrap().iter().find(|g| g.src.path == path).map(|g| g.src.clone());
    let item = match game {
        Some(src) => crate::install::Item::Game(src),
        None => match state.packages.lock().unwrap().iter().find(|p| p.path == path) {
            Some(p) => crate::install::Item::Pkg(p.clone()),
            None => return reply(404, "text/plain", "not a game or package of the last scan"),
        },
    };
    match crate::install::enqueue(item) {
        Ok(()) => no_content(),
        Err((code, msg)) => {
            log!("install {}: refused: {msg}", path.display());
            reply(code, "text/plain", msg)
        }
    }
}

/// POST /api/install/delete-source, body = the path of a `kept` row: deletes only that .pkg, after
/// /api/delete's path checks. Never uninstalls, never touches the cache.
fn delete_source(state: &State, body: &[u8]) -> Reply {
    let path = match body_path(body) {
        Ok(p) => p,
        Err(r) => return r,
    };
    let Ok(_work) = state.work.try_lock() else {
        return reply(409, "text/plain", "a scan or build is in progress; try again in a moment");
    };
    let (current, queue, results) = crate::install::snapshot();
    let id = path.to_string_lossy();
    if current.iter().chain(&queue).any(|j| j.id == id) {
        return reply(409, "text/plain", "the package is queued or installing");
    }
    let games = state.games.lock().unwrap().clone();
    let (_, kept) = install_rows(state, &games, &installed_map(&games), &results, true);
    let Some(row) = kept.iter().find(|r| r["path"] == *id) else {
        log!("install: delete of {} refused: not a .pkg of an installed title", path.display());
        return reply(409, "text/plain", "not the .pkg of an installed title");
    };
    let tid = row["title_id"].as_str().unwrap_or_default().to_string();
    // A kept DLC row is already its installed status (dlc_installed asked above); it installs
    // regardless of the base game.
    if row["kind"] != "dlc" && !crate::installed::is_installed(&tid) {
        log!("{tid}: delete of {} refused: the title is not installed", path.display());
        return reply(409, "text/plain", "the title is not installed");
    }
    let roots = {
        let settings = state.settings.lock().unwrap();
        if settings.scan_paths.is_empty() { storage::default_scan_roots() } else { settings.scan_paths.clone() }
    };
    let src = Source { path: path.clone(), kind: Kind::Pkg, title_id: tid.clone(), title_name: String::new(),
        version: String::new(), drive: String::new(), size: 0, backport: None };
    if let Err(reason) = safe_delete_source(&src, &roots) {
        log!("{tid}: delete of the installed .pkg {} rejected: {reason}", path.display());
        return reply(409, "text/plain", reason);
    }
    if let Err(e) = fs::remove_file(&path) {
        log!("{tid}: delete of the installed .pkg {}: {e}", path.display());
        return reply(500, "text/plain", e.to_string());
    }
    log!("{tid}: deleted the installed .pkg {}", path.display());
    state.games.lock().unwrap().retain(|g| g.src.path != path);
    state.packages.lock().unwrap().retain(|p| p.path != path);
    state.wake();
    no_content()
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

/// POST /api/sizes/<TID>, body = a duplicate row's path: `{game}` of that copy only (the .pkg's length,
/// a folder's du); its DLC, saves and cache are the title's, shown on the library copy.
fn duplicate_size(state: &State, tid: &str, body: &[u8]) -> Reply {
    let Some(dup) = Some(tid).filter(|t| crate::scan::valid_tid(t)).and_then(|t| duplicate(state, t, body)) else {
        return not_found();
    };
    let game = match dup.kind {
        Kind::Folder(_) => cache::du(&dup.path),
        Kind::Pkg => fs::metadata(&dup.path).map_or(dup.size, |m| m.len()),
        Kind::Installed => dup.size,
    };
    reply(200, "application/json", serde_json::json!({ "game": game }).to_string())
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
        // app.pkg's size, or the ext copy's du (installed::list).
        Kind::Installed => src.size,
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

pub(crate) fn safe_delete_source(src: &Source, roots: &[String]) -> Result<(), &'static str> {
    let path = &src.path;
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return Err("source path is not a safe absolute path");
    }
    if ["/", "/data", "/user", "/mnt", "/system", "/system_ex"].iter().any(|p| path == Path::new(p))
        || path.parent() == Some(Path::new("/mnt"))
        || storage::drives().iter().any(|d| path == Path::new(&d.mount)) {
        return Err("source path is a protected root");
    }
    // An installed copy's files (/user/app/<TID>/app.pkg, /mnt/extN/user/app/...), even when a
    // custom scan root lists them as a .pkg duplicate.
    if path.to_string_lossy().contains("/user/app/") {
        return Err("source path belongs to an installed game");
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
        // The console's own install: never deleted or uninstalled from here.
        Kind::Installed => Err("an installed game is not deleted here"),
    }
}

/// Starts a ready library title, as its home screen tile does.
fn launch(state: &State, tid: &str) -> Reply {
    if !crate::scan::valid_tid(tid) {
        return not_found();
    }
    let status = state.games.lock().unwrap().iter()
        .find(|g| g.src.title_id == tid && g.status != "duplicate").map(|g| g.status);
    let Some(status) = status else { return not_found() };
    if state.paused() {
        return reply(409, "text/plain", "a game is running");
    }
    if status != "ready" {
        return reply(409, "text/plain", "the game is not ready");
    }
    match sys::launch_app(tid) {
        Ok(()) => no_content(),
        Err(e) => {
            log!("{tid}: launch: {e}");
            reply(502, "text/plain", e)
        }
    }
}

/// The console's LAN address: the source address of a UDP socket connected to a TEST-NET-1 host (a
/// UDP connect sends nothing). None without a route.
fn lan_ip() -> Option<std::net::Ipv4Addr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    match s.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// The request's Host header when it names an IPv4 address other than loopback (a phone or PC opened
/// the page by the console's address).
fn host_ip(head: &str) -> Option<std::net::Ipv4Addr> {
    let host = head.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("host")).map(|(_, v)| v.trim()))?;
    let ip: std::net::Ipv4Addr = host.split(':').next()?.parse().ok()?;
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// GET /api/qr: `{url, svg}`, `http://<LAN IP>:8097/` and its QR code (Settings, for a phone); the
/// route's source address, else the request's Host (a LAN without a default gateway); 404 without either.
/// shortcut: opened on the console itself (127.0.0.1) on a LAN without a gateway, no QR; getifaddrs
/// would cover it if that setup matters.
fn qr(head: &str) -> Reply {
    let Some(ip) = lan_ip().or_else(|| host_ip(head)) else { return reply(404, "text/plain", "no address to show") };
    let url = format!("http://{ip}:{PORT}/");
    let code = match qrcode::QrCode::with_error_correction_level(url.as_bytes(), qrcode::EcLevel::L) {
        Ok(c) => c,
        Err(e) => return reply(500, "text/plain", format!("qr: {e}")),
    };
    // ps5-dump-forge qr_svg: a 2-module quiet zone, one path, scaled by CSS.
    let (w, q) = (code.width(), 2);
    let mut d = String::new();
    for (i, c) in code.to_colors().iter().enumerate() {
        if *c == qrcode::Color::Dark {
            d += &format!("M{},{}h1v1h-1z", i % w + q, i / w + q);
        }
    }
    let n = w + 2 * q;
    let svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {n} {n}\" shape-rendering=\"crispEdges\">\
         <rect width=\"{n}\" height=\"{n}\" fill=\"#fff\"/><path fill=\"#000\" d=\"{d}\"/></svg>");
    reply(200, "application/json", serde_json::json!({ "url": url, "svg": svg }).to_string())
}

/// POST /api/close/<TID>: closes the title while the launch watcher has it running (sys::close_app).
fn close(state: &State, tid: &str) -> Reply {
    if !crate::scan::valid_tid(tid) || state.source(tid).is_none() {
        return not_found();
    }
    if !state.running.lock().unwrap().contains(tid) {
        return reply(409, "text/plain", "the game is not running");
    }
    match sys::close_app(tid) {
        Ok(()) => {
            log!("{tid}: closed from the web UI");
            no_content()
        }
        Err(e) => {
            log!("{tid}: close: {e}");
            reply(502, "text/plain", e)
        }
    }
}

/// The guards of delete and uninstall: `work` (the scan loop holds it while it scans, builds or
/// registers; never wait a build out here), a library title, no game running, the title not building.
fn lock_title<'a>(state: &'a State, tid: &str) -> Result<(std::sync::MutexGuard<'a, ()>, Source), Fail> {
    if !crate::scan::valid_tid(tid) {
        return Err(fail(404, libc::ENOENT, "not found"));
    }
    let Ok(work) = state.work.try_lock() else {
        return Err(fail(409, libc::EBUSY, "a scan or build is in progress; try again in a moment"));
    };
    Ok((work, check_title(state, tid)?))
}

/// lock_title without taking `work` (the caller holds it).
fn check_title(state: &State, tid: &str) -> Result<Source, Fail> {
    let Some(src) = state.source(tid) else { return Err(fail(404, libc::ENOENT, "not found")) };
    if state.paused() || state.running.lock().unwrap().contains(tid) {
        return Err(fail(409, libc::EBUSY, "a game is running"));
    }
    if state.games.lock().unwrap().iter().any(|g| g.src.title_id == tid && matches!(g.status, "building" | "checking")) {
        return Err(fail(409, libc::EBUSY, "the game is being built"));
    }
    Ok(src)
}

/// The duplicate row of `tid` whose source path is `path` (the bytes the UI sent).
fn duplicate(state: &State, tid: &str, path: &[u8]) -> Option<Source> {
    state.games.lock().unwrap().iter()
        .find(|g| g.status == "duplicate" && g.src.title_id == tid && g.src.path.as_os_str().as_encoded_bytes() == path)
        .map(|g| g.src.clone())
}

/// /api/delete of a duplicate: only its source goes (the checks of a library delete). The library
/// copy's registration, mount and cache belong to the title and stay.
fn delete_duplicate(state: &State, tid: &str, dup: &Source) -> Reply {
    if let Some(msg) = installed_refusal(dup) {
        return reply(409, "text/plain", msg);
    }
    let (current, queue, _) = crate::install::snapshot();
    let id = dup.path.to_string_lossy();
    if current.iter().chain(&queue).any(|j| j.id == id) {
        log!("{tid}: delete refused: duplicate {} is queued or installing", dup.path.display());
        return reply(409, "text/plain", "the package is queued or installing; cancel it first");
    }
    let roots = {
        let settings = state.settings.lock().unwrap();
        if settings.scan_paths.is_empty() { storage::default_scan_roots() } else { settings.scan_paths.clone() }
    };
    if let Err(reason) = safe_delete_source(dup, &roots) {
        log!("{tid}: delete rejected: duplicate {}: {reason}", dup.path.display());
        return reply(409, "text/plain", reason);
    }
    let result = match dup.kind {
        Kind::Pkg => fs::remove_file(&dup.path),
        Kind::Folder(_) => fs::remove_dir_all(&dup.path),
        Kind::Installed => return reply(409, "text/plain", "an installed game is not deleted here"),
    };
    if let Err(e) = result {
        // ponytail: a folder remove_dir_all that failed midway leaves a partial duplicate.
        log!("{tid}: delete duplicate {}: {e}", dup.path.display());
        state.wake();
        return reply(500, "text/plain", e.to_string());
    }
    log!("{tid}: deleted the duplicate {}", dup.path.display());
    state.games.lock().unwrap().retain(|g| g.src.path != dup.path);
    state.wake();
    no_content()
}

fn delete(state: &State, tid: &str, body: &[u8]) -> Reply {
    let (_work, src) = match lock_title(state, tid) {
        Ok(x) => x,
        Err(f) => return f.text(),
    };
    if let Some(dup) = duplicate(state, tid, body).filter(|d| d.path != src.path) {
        return delete_duplicate(state, tid, &dup);
    }
    if let Some(msg) = installed_refusal(&src) {
        log!("{tid}: delete refused: {} is the console's install", src.path.display());
        return reply(409, "text/plain", msg);
    }
    // The UI sends the path it showed in the confirmation; a scan may have picked another copy since.
    if body != src.path.as_os_str().as_encoded_bytes() {
        log!("{tid}: delete refused: asked for {:?}, library source is {}", String::from_utf8_lossy(body), src.path.display());
        return reply(409, "text/plain", "the library changed; reopen the game and try again");
    }
    // As delete_source: the install job reads (and serves) a queued or running .pkg.
    // ponytail: a job queued after this check fails at its start on the missing file.
    let (current, queue, _) = crate::install::snapshot();
    let id = src.path.to_string_lossy();
    if current.iter().chain(&queue).any(|j| j.id == id) {
        log!("{tid}: delete refused: {} is queued or installing", src.path.display());
        return reply(409, "text/plain", "the package is queued or installing; cancel it first");
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
        // Refused by safe_delete_source above.
        Kind::Installed => return reply(409, "text/plain", "an installed game is not deleted here"),
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

/// /api/v1/manual/remove (`exclude` true) or add: Kraken Mount sees every folder in its scan roots,
/// so removing a manual source means excluding it, adding one takes it off `exclude`. Idempotent.
fn manual(state: &State, b: &Obj, exclude: bool) -> Result<serde_json::Value, Fail> {
    let path = str_field(b, "path")?;
    let Some(entry) = exclude_entry(path) else {
        return Err(fail(400, libc::EINVAL, "path: expected an absolute path"));
    };
    let changed = change_settings(state, &format!("exclude {entry}"), |s| set_member(&mut s.exclude, &entry, exclude))
        .map_err(save_fail)?;
    if changed {
        log!("{entry}: {} the excluded paths from the web API", if exclude { "added to" } else { "removed from" });
    }
    Ok(serde_json::json!({"changed": changed}))
}

/// /api/v1/games/uninstall: off the home screen and unmounted; the source is excluded so the next
/// scan leaves it alone. Never deletes the source or the cache (game-written files stay).
/// `expect`: the source path the UI showed (/api/hide); empty = any.
fn uninstall_keep(state: &State, tid: &str, expect: Option<&[u8]>) -> Result<serde_json::Value, Fail> {
    let (_work, src) = lock_title(state, tid)?;
    let path = src.path.to_string_lossy().into_owned();
    // /api/hide of a duplicate: only its path joins `exclude`; the library copy stays registered.
    if let Some(dup) = expect.and_then(|b| duplicate(state, tid, b)).filter(|d| d.path != src.path) {
        // An installed copy ignores `exclude` (scan claims installs first): nothing to hide.
        if let Some(msg) = installed_refusal(&dup) {
            return Err(fail(409, libc::EPERM, msg));
        }
        let Some(entry) = exclude_entry(&dup.path.to_string_lossy()) else {
            return Err(fail(409, libc::EINVAL, "the source path is not absolute"));
        };
        change_settings(state, &format!("{tid}: exclude {entry}"), |s| set_member(&mut s.exclude, &entry, true)).map_err(save_fail)?;
        state.games.lock().unwrap().retain(|g| g.src.path != dup.path);
        log!("{tid}: hid the duplicate {}", dup.path.display());
        return Ok(serde_json::json!({}));
    }
    if let Some(msg) = installed_refusal(&src) {
        log!("{tid}: uninstall refused: {path} is the console's install");
        return Err(fail(409, libc::EPERM, msg));
    }
    if let Some(b) = expect.filter(|b| !b.is_empty() && *b != src.path.as_os_str().as_encoded_bytes()) {
        log!("{tid}: hide refused: asked for {:?}, library source is {path}", String::from_utf8_lossy(b));
        return Err(fail(409, libc::EBUSY, "the library changed; reopen the game and try again"));
    }
    // As auto-remove: an installed copy of this title id (or one we did not stage) is never uninstalled.
    // Hide of a title never staged (first build failed or pending) only excludes it.
    let staged = match register::is_ours(tid) {
        Ok(()) => true,
        Err(p) if expect.is_some() && p == Path::new("/user/app").join(tid) => false,
        Err(p) => {
            log!("{tid}: uninstall refused: {} is not a Kraken Mount registration", p.display());
            return Err(fail(409, libc::EPERM, format!("{} is not a Kraken Mount registration", p.display())));
        }
    };
    // Never staged: a mount at /system_ex/app/<TID> is not ours to take down.
    if let Err(e) = if staged { register::unmount_title(tid) } else { Ok(()) } {
        log!("{tid}: unmount {path}: {e}");
        return Err(fail(409, libc::EBUSY, e.to_string()));
    }
    // ponytail: a non-UTF-8 source path is excluded by its lossy text, which then matches nothing.
    let Some(entry) = exclude_entry(&path) else {
        log!("{tid}: uninstall refused: {path} is not an absolute path");
        state.wake();
        return Err(fail(409, libc::EINVAL, "the source path is not absolute"));
    };
    // Every copy of the title (duplicates too): otherwise the next scan picks a duplicate as the
    // winner and registers the title again.
    // ponytail: only the listed copies; one still being copied (scan's `unstable`) is not known here.
    let mut entries = vec![entry];
    let others: Vec<PathBuf> = state.games.lock().unwrap().iter()
        .filter(|g| g.src.title_id == tid && g.src.path != src.path).map(|g| g.src.path.clone()).collect();
    for p in &others {
        match exclude_entry(&p.to_string_lossy()) {
            Some(e) if !entries.contains(&e) => entries.push(e),
            Some(_) => {}
            None => log!("{tid}: duplicate {} is not an absolute path: not excluded", p.display()),
        }
    }
    let what = format!("{tid}: exclude {}", entries.join(", "));
    // Excluded first, so a failed save leaves the title registered and untouched (the next pass
    // remounts it); a failed uninstall takes back what this call added, so the next pass remounts
    // the title that is still on the home screen and the caller can retry.
    let mut added = Vec::new();
    let change = change_settings(state, &what, |s| {
        added = entries.iter().filter(|e| set_member(&mut s.exclude, e, true)).cloned().collect();
        !added.is_empty()
    });
    if let Err(e) = change {
        state.wake();
        return Err(save_fail(e));
    }
    if let Err(e) = if staged { register::uninstall(tid) } else { Ok(()) } {
        log!("{tid}: uninstall {path}: {e}");
        let mut msg = e.to_string();
        let undo = |s: &mut Settings| added.iter().fold(false, |c, e| set_member(&mut s.exclude, e, false) | c);
        if !added.is_empty() && change_settings(state, &what, undo).is_err() {
            msg += &format!("; {} stay excluded (config save failed): take them off with manual/add", added.join(", "));
        }
        state.wake();
        return Err(fail(500, libc::EIO, msg));
    }
    register::forget(tid);
    state.titles.lock().unwrap().remove(tid);
    state.games.lock().unwrap().retain(|g| g.src.title_id != tid);
    state.wake();
    log!("{tid}: uninstalled from the web API, {path} kept and excluded");
    Ok(serde_json::json!({}))
}

/// POST /api/hidden/delete, body = an `exclude` path that is a readable game (scan::read_source): its
/// source is deleted (the checks of a library delete: `work` free, its title not running, not queued
/// or installing, `safe_delete_source`), then the path leaves `exclude`. The title's cache stays (Purge).
fn delete_hidden(state: &State, body: &[u8]) -> Reply {
    let Ok(path) = std::str::from_utf8(body) else { return reply(400, "text/plain", "path: expected UTF-8") };
    if !state.settings.lock().unwrap().exclude.iter().any(|e| e == path) {
        return reply(404, "text/plain", "not hidden");
    }
    let Some(src) = crate::scan::read_source(Path::new(path)) else {
        return reply(409, "text/plain", "not a game Kraken Mount can read");
    };
    let Ok(_work) = state.work.try_lock() else {
        return reply(409, "text/plain", "a scan or build is in progress; try again in a moment");
    };
    let tid = src.title_id.clone();
    // As check_title: paused covers a game that is starting, before the watcher lists it as running.
    if state.paused() || state.running.lock().unwrap().contains(&tid) {
        return reply(409, "text/plain", "a game is running");
    }
    let (current, queue, _) = crate::install::snapshot();
    if current.iter().chain(&queue).any(|j| j.id == path) {
        return reply(409, "text/plain", "the package is queued or installing; cancel it first");
    }
    let roots = {
        let settings = state.settings.lock().unwrap();
        if settings.scan_paths.is_empty() { storage::default_scan_roots() } else { settings.scan_paths.clone() }
    };
    if let Err(reason) = safe_delete_source(&src, &roots) {
        log!("{tid}: delete rejected: hidden {path}: {reason}");
        return reply(409, "text/plain", reason);
    }
    // A source hidden by hand (manual/remove, config) stays registered: as /api/delete, its home-screen
    // entry goes too when this path is the registered source and the registration is ours.
    let registered = cache::stamp_source(&tid).as_deref() == Some(path) && register::is_ours(&tid).is_ok();
    if registered {
        if let Err(e) = register::unmount_title(&tid) {
            log!("{tid}: delete hidden {path}: unmount: {e}");
            return reply(409, "text/plain", e.to_string());
        }
    }
    let result = match src.kind {
        Kind::Pkg => fs::remove_file(path),
        Kind::Folder(_) => fs::remove_dir_all(path),
        Kind::Installed => return reply(409, "text/plain", "an installed game is not deleted here"),
    };
    if let Err(e) = result {
        // ponytail: a folder remove_dir_all that failed midway leaves a partial source (still hidden).
        log!("{tid}: delete hidden {path}: {e}");
        // Still registered: mounted again (an excluded source is not remounted by the scan). shortcut: no backport layer.
        if registered {
            if let Err(m) = register::mount(&tid, &cache::app0(&src), None) {
                log!("{tid}: delete hidden {path}: remount: {m}");
            }
        }
        return reply(500, "text/plain", e.to_string());
    }
    log!("{tid}: deleted the hidden {path}");
    if registered {
        if let Err(e) = register::uninstall(&tid) {
            log!("{tid}: uninstall after deleting the hidden {path}: {e}");
        }
        register::forget(&tid);
        state.titles.lock().unwrap().remove(&tid);
    }
    if let Err(e) = change_settings(state, &format!("{tid}: unexclude {path}"), |s| set_member(&mut s.exclude, path, false)) {
        return reply(500, "text/plain", format!("deleted; still listed as hidden: {e}"));
    }
    no_content()
}

/// POST /api/unhide, body = an `exclude` path: as /api/v1/manual/add; 404 when it was not excluded.
fn unhide(state: &State, body: &[u8]) -> Reply {
    let Ok(path) = std::str::from_utf8(body) else { return reply(400, "text/plain", "path: expected UTF-8") };
    let b = Obj::from_iter([("path".to_string(), path.into())]);
    match manual(state, &b, false) {
        Ok(v) if v["changed"] == true => no_content(),
        Ok(_) => reply(404, "text/plain", "not hidden"),
        Err(f) => f.text(),
    }
}

fn unhide_all(state: &State) -> Reply {
    let mut n = 0;
    let r = change_settings(state, "unhide all", |s| {
        n = s.exclude.len();
        s.exclude.clear();
        n > 0
    });
    match r {
        Ok(true) => {
            log!("{n} paths removed from the excluded paths from the web UI");
            no_content()
        }
        Ok(false) => reply(404, "text/plain", "not hidden"),
        Err(e) => save_fail(e).text(),
    }
}

/// The only dir delete_backport removes: the one the scan recorded, `<TID>*` in a `backports` dir.
fn backport_ok(tid: &str, recorded: Option<&Path>, target: &Path) -> Result<(), &'static str> {
    if recorded != Some(target) {
        return Err("not the backport the scan found");
    }
    if !target.is_absolute() || target.components().any(|c| !matches!(c, Component::RootDir | Component::Normal(_))) {
        return Err("backport path is not a safe absolute path");
    }
    let in_backports = target.parent().and_then(Path::file_name).is_some_and(|n| n.eq_ignore_ascii_case("backports"));
    if !in_backports || !target.file_name().is_some_and(|n| n.to_string_lossy().starts_with(tid)) {
        return Err("backport path is not backports/<TID>*");
    }
    Ok(())
}

/// POST /api/backport/delete/<TID>: deletes the scan's backport dir of the title, then queues a
/// rebuild. A folder game's stack is redone without the unionfs first (busy: nothing deleted).
fn delete_backport(state: &State, tid: &str) -> Result<(), Fail> {
    let (_work, src) = lock_title(state, tid)?;
    delete_backport_locked(state, tid, src)
}

fn delete_backport_locked(state: &State, tid: &str, src: Source) -> Result<(), Fail> {
    let Some(bp) = src.backport.clone() else { return Err(fail(404, libc::ENOENT, "no backport")) };
    let recorded = state.source(tid).and_then(|s| s.backport);
    let not_dir = |p: &Path| fs::symlink_metadata(p).map_or(true, |m| !m.is_dir());
    let checked = backport_ok(tid, recorded.as_deref(), &bp)
        .and_then(|()| if not_dir(&bp) || bp.parent().is_some_and(not_dir) { Err("backport is missing or a symlink") } else { Ok(()) });
    if let Err(reason) = checked {
        log!("{tid}: backport delete rejected {}: {reason}", bp.display());
        return Err(fail(409, libc::EPERM, reason));
    }
    if crate::launch::backport_in_use(&bp) {
        log!("{tid}: backport delete {}: redirects into a sandbox still read from it", bp.display());
        return Err(fail(409, libc::EBUSY, "backport redirects still active"));
    }
    if matches!(src.kind, Kind::Folder(_)) {
        if let Err(e) = register::drop_backport(tid, &cache::app0(&src)) {
            log!("{tid}: backport delete {}: {e}", bp.display());
            state.wake();
            return Err(fail(409, libc::EBUSY, e.to_string()));
        }
    }
    if let Err(e) = fs::remove_dir_all(&bp) {
        // ponytail: a remove that failed midway leaves a partial backport; the next pass uses what is left.
        log!("{tid}: delete backport {}: {e}", bp.display());
        state.wake();
        return Err(fail(500, e.raw_os_error().unwrap_or(libc::EIO), e.to_string()));
    }
    state.update(&src.path, |g| g.src.backport = None);
    state.forced.lock().unwrap().insert(tid.to_string());
    state.wake();
    log!("{tid}: backport {} deleted; rebuild queued", bp.display());
    Ok(())
}

/// POST /api/backports/delete-all: delete_backport per library title with one; failures as
/// "<TID>: <reason>" lines.
fn delete_backports(state: &State) -> Reply {
    let tids: Vec<String> = state.games.lock().unwrap().iter()
        .filter(|g| g.status != "duplicate" && g.src.backport.is_some()).map(|g| g.src.title_id.clone()).collect();
    if tids.is_empty() {
        return reply(404, "text/plain", "no backport");
    }
    // Held for all titles: each delete wakes the scan loop, which would take it in between.
    let Ok(_work) = state.work.try_lock() else {
        return reply(409, "text/plain", "a scan or build is in progress; try again in a moment");
    };
    let failed: Vec<(u16, String)> = tids.iter()
        .filter_map(|t| check_title(state, t).and_then(|src| delete_backport_locked(state, t, src)).err()
            .map(|f| (f.code, format!("{t}: {}", f.msg))))
        .collect();
    match failed.iter().map(|(c, _)| *c).max() {
        None => no_content(),
        Some(code) => reply(code, "text/plain", failed.into_iter().map(|(_, m)| m).collect::<Vec<_>>().join("\n")),
    }
}

type Obj = serde_json::Map<String, serde_json::Value>;

/// An /api/v1 request body: one JSON object; an empty body is {}.
fn v1_body(body: &[u8]) -> Result<Obj, Fail> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Obj::new());
    }
    match serde_json::from_slice(body) {
        Ok(serde_json::Value::Object(o)) => Ok(o),
        Ok(_) => Err(fail(400, libc::EINVAL, "expected a JSON object")),
        Err(e) => Err(fail(400, libc::EINVAL, format!("bad JSON: {e}"))),
    }
}

fn str_field<'a>(b: &'a Obj, key: &str) -> Result<&'a str, Fail> {
    b.get(key).and_then(|v| v.as_str()).ok_or_else(|| fail(400, libc::EINVAL, format!("{key}: expected a string")))
}

/// /api/v1/games/fakelib: true = overlay off. `mode` ("disabled", "full" or "default"; "emulators"
/// is not supported) wins over `ignore_global_fakelib` (true = off).
fn fakelib_off(b: &Obj) -> Result<bool, Fail> {
    match (b.get("mode"), b.get("ignore_global_fakelib")) {
        (Some(m), _) => match m.as_str() {
            Some("disabled") => Ok(true),
            Some("full" | "default") => Ok(false),
            Some("emulators") => Err(fail(501, libc::ENOTSUP, "mode emulators is not supported")),
            _ => Err(fail(400, libc::EINVAL, "mode: expected full, default or disabled")),
        },
        (None, Some(v)) => v.as_bool().ok_or_else(|| fail(400, libc::EINVAL, "ignore_global_fakelib: expected a boolean")),
        (None, None) => Err(fail(400, libc::EINVAL, "expected mode or ignore_global_fakelib")),
    }
}

/// POST /api/v1/<route>: the ShadowMount+ API v1 subset Playden calls (DESIGN.md "Web API").
fn v1(state: &State, route: &str, body: &[u8]) -> Reply {
    use serde_json::json;
    let r = v1_body(body).and_then(|b| match route {
        "version" => Ok(json!({
            "api_version": 1,
            "kraken_mount_version": env!("CARGO_PKG_VERSION"),
            "capabilities": ["list_games", "add_manual_source", "remove_manual_source", "uninstall_game", "rescan", "fakelib",
                "fakelib_unlisted"],
        })),
        "games" => Ok(v1_games(state)),
        "scan" => {
            state.wake();
            Ok(json!({}))
        }
        "manual/add" => manual(state, &b, false),
        "manual/remove" => manual(state, &b, true),
        "games/uninstall" => uninstall_keep(state, str_field(&b, "title_id")?, None),
        "games/fakelib" => {
            let tid = str_field(&b, "title_id")?;
            let off = fakelib_off(&b)?;
            let listed = set_overlay(state, tid, off, true)?;
            Ok(json!({"fakelib_mode": if off { "disabled" } else { "full" }, "saved": true, "applies_on_next_launch": true,
                "listed": listed}))
        }
        _ => Err(fail(404, libc::ENOENT, "unknown route")),
    });
    match r {
        Ok(mut v) => {
            v["status"] = 0.into();
            reply(200, "application/json", v.to_string())
        }
        Err(f) => f.json(),
    }
}

/// Library titles (no duplicates) in SMP's games shape.
fn v1_games(state: &State) -> serde_json::Value {
    let games: Vec<Game> = state.games.lock().unwrap().iter().filter(|g| g.status != "duplicate").cloned().collect();
    let overlay_off = state.settings.lock().unwrap().overlay_off.clone();
    let list: Vec<_> = games.iter().zip(presence(&games)).map(|(g, [registered, mounted, present])| {
        let path = g.src.path.to_string_lossy();
        let folder = matches!(g.src.kind, Kind::Folder(_));
        let installed = g.src.kind == Kind::Installed || crate::scan::installed_pkg(&g.src);
        serde_json::json!({
            "title_id": g.src.title_id,
            "name": g.src.title_name,
            "version": g.src.version,
            "path": path,
            "runtime_path": if folder { &path[..] } else { "" },
            "source_type": if folder { "folder" } else if installed { "installed" } else { "pkg" },
            // The console's own install: Kraken Mount neither registers nor uninstalls it.
            "managed": !installed,
            "installed": registered,
            "mounted": mounted,
            "source_available": present,
            "status": g.status,
            "error": g.error.as_deref().unwrap_or(""),
            // As cache.rs: a pkg source keeps its fakelib whatever overlay_off says.
            "fakelib_mode": if g.src.kind != Kind::Pkg && overlay_off.contains(&g.src.title_id) { "disabled" } else { "full" },
        })
    }).collect();
    serde_json::json!({"count": list.len(), "games": list})
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
            if let Ok(png) = crate::scan::read_capped(&dir.join(name), 32 << 20) {
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
