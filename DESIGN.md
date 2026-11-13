# krakenmount design

A Rust PS5 ELF payload (`src/`). It finds games on the internal SSD and on USB/extended drives,
keeps a per-game cache on the internal SSD, registers each game on the home screen and, at
launch, overlays the game's fakelib and the shared ampr-emu-ng dir on the sandbox's system libs.
The ampr-emu-ng runtime is NOT built here: a prebuilt `libSceAmpr.sprx`
(ampr-emu-ng with pkg mode) goes into `/data/krakenmount/ampr/`.
`crates/kraken-mount-pkg` reads debug FPKGs (the reader plus its Kraken block decoder, module `decoder`).

## Supported sources (nothing else)
- **FPKG**: a dump-forge debug `.pkg` file directly in a scan root.
- **Folder**: a directory directly in a scan root with `eboot.bin` and `sce_sys/param.json`.
  Packing: `ampr_zstd.index` present = zstd, else `ampr_assets.index` = lz4, else plain.
- Any drive. The cache is always on the internal SSD.

## Paths
| Path | What |
|---|---|
| `/data/krakenmount/config.toml` | settings (created with defaults on first start) |
| `/data/krakenmount/caches/<TID>/` | per-game cache |
| `/data/krakenmount/ampr/` | ampr-emu-ng files (`libSceAmpr.sprx`, ...), copied on top of every game's `fakelib/` in its cache (chmod 0777 at start; a `libkernel.sprx` there is never copied and logs a warning) |
| `/data/krakenmount/kraken.log` | log (setting `log`); starts over past 1 MiB, no kept copy |
| `/data/krakenmount/kraken-mount.elf` | self copy the tile starts (Dump Forge style, below) |

## Drives and scan roots (`src/storage.rs`)
Drives: `/data` (label "Internal") always, plus each of `/mnt/usb0..7` ("USB n+1") and
`/mnt/ext0..1` ("Extended n") that is a mount point (statfs `f_mntonname == path`).
Default scan roots: SMP `SM_DEFAULT_SCAN_PATHS_INITIALIZER` order, only for drives present
(`/data/homebrew`, `/data/etaHEN/games`, ext `homebrew`/`etaHEN/games`, usb `homebrew`, usb `etaHEN/games`,
usb roots, ext roots). `scanpath=` lines in config replace them.
A dir named `backports` in a scan root is never a game.

## Settings (`src/config.rs`, file `/data/krakenmount/config.toml`)
A flat TOML subset: `key = value` with strings, `true`/`false`, integers and arrays of strings; `#` comments.
| Key | Default | Meaning |
|---|---|---|
| `scanpath` | (defaults above) | array of paths; replaces the default scan roots |
| `inject_ampr` | true | copy `/data/krakenmount/ampr/*` on top of every game's fakelib (a change only refreshes `fakelib/`) |
| `game_fakelib` | emulators | game's own fakelib: `emulators` (AppContent, GameUpdate, NpEntitlementAccess, PlayGo, Ampr), `full`, `off` |
| `backports` | true | apply `<scan root>/backports/<TID>*` |
| `scan_interval_seconds` | 10 | scan loop period (1..3600) |
| `low_space_gb` | 20 | warn when `/data` free space is below this |
| `notify` | true | system toasts |
| `log` | true | write `/data/krakenmount/kraken.log` (stdout always gets the lines) |
| `debug` | false | klog capture to `/data/krakenmount/klog.txt` and the `/file/data/...` route |

Invalid values are logged and replaced by the default. The web UI edits the same file.

## Cache (`src/cache.rs`)
`/data/krakenmount/caches/<TID>/`, stamp file `.done` written last.
- Folder: `fakelib/` only. `/app0` is the game folder itself.
- FPKG: it IS `/app0`: `sce_sys/**`, `eboot.bin`, every `*.prx`/`*.sprx` extracted (0777),
  `fakelib/`, `kraken/` (mount point for the pkg's folder, see Launch), `kraken.image` (one line, no
  newline: `kraken/<pkg file name>`, relative to `/app0`, the file ng opens; missing means stale), `ampr_emu.index` +
  `ampr_assets.index` (`src/ampr_index.rs`; every non-extracted file packed, fakelib and
  backport files loose). Backport files (except its fakelib) are copied loose over pkg files.
  The dir itself is never deleted: `/system_ex/app/<TID>` is a nullfs of it.
- `/app0` is writable, so a game may write into its cache (pkg) or its dump folder (folder, also on
  USB). `.built` (written before `.done`) lists what the build created; a rebuild removes only those
  paths (a dir only once empty), so files the game wrote stay. A failed build removes what it wrote.
  A cache without `.built` (older build) is emptied. Only Purge deletes the game's files.
- `fakelib/` composition: base = backport `fakelib/` (all files) if present, else the game's
  `fakelib/` filtered by `game_fakelib`; `libkernel.sprx` always dropped. `fakelib2/` (SMP) wins
  over `fakelib/` and is used as-is; it is never indexed or copied as game files. All files 0777.
  Then, with `inject_ampr`, every file of `/data/krakenmount/ampr/` (not `libkernel.sprx`) is copied on top,
  so ours wins over the game's own `libSceAmpr.sprx`. `.ampr` records what was copied (name, size,
  mtime); when the ampr dir or `inject_ampr` changes, `check` says `Fakelib` and only `fakelib/` is
  recomposed (`refresh_fakelib`), no extraction or re-registration. `fakelib/` is not in the AMPR index.
- A folder game's cache is only `fakelib/`, `.ampr`, `.built`, `.done`: nothing of the game is copied.
- Fresh when `.done` exists, its JSON stamp equals the expected one
  (`{"v":1,"kind","source","size","game_fakelib","backport","fakelib"}`), and every input's
  mtime is older than `.done`'s mtime. Inputs: FPKG = the .pkg, backport dir (recursive);
  folder = `eboot.bin`, `sce_sys/param.json`, its lib dir and files (not the folder itself: the game
  writes there). Otherwise rebuilt.
- Purge (settings): delete caches of titles not in the library (`register::unmount_title` first;
  a dir whose unmount fails is skipped and logged), empty and rebuild the rest.

## Flow (`src/main.rs`)
Start: create dirs, load config, take port 8097 (asks an older copy to quit), ampr chmod, web thread,
launch watcher thread, klog thread when `debug`. Loop every `scan_interval_seconds` or on
wake: reload config, scan, then for each game: freshness check, build if stale or forced,
register (once per run, and after every rebuild), status `ready`. A game whose cache is fresh and
which is already registered (`register::is_registered`) is not re-registered: only `register::mount`
redoes the missing app0 nullfs (gone after a reboot); for such a pkg the scan reads title id/name from
the cache's `sce_sys/param.json` and never opens the .pkg. A fresh title registered by this run still
gets `register::mount` every scan (a no-op when app0 is mounted there; redoes it after a USB replug);
a title that leaves the scan is forgotten, so it goes through the full path when it is back.
Running game: while `launch::watch` reports a live session (`running`), the loop does no
scan/build/register/purge (a game starting mid-scan stops the scan before the next title);
rescan/rebuild/purge requests stay queued and run after the game exits (2 s poll).
Toasts: `sceNotificationSend` system toast (dump-forge/SMP JSON, icon `/user/data/krakenmount/notify-icon.png`),
the old `sceKernelSendNotificationRequest` only as fallback. A failed game is retried
only when its inputs change or a rebuild is forced. Duplicate title ids: first scan root wins,
the others show `duplicate`. Low `/data` space (checked before the games, before and after each
build, after the scan): toast once per run and a UI banner.

## Register (`src/register.rs`)
SMP port: copy `sce_sys` to `/user/app/<TID>/sce_sys` + `icon0.png`, appmeta, trophy data,
nullfs `app0 -> /system_ex/app/<TID>` (app0 = cache dir for FPKG, game folder for Folder),
for a Folder with a backport a unionfs of the backport dir on top (SMP
`mount_backport_overlay`), then `sceAppInstUtilAppInstallTitleDir` (fallback InstallAll).

## Launch (`src/launch.rs`)
As before (SMP sm_fakelib port): unionfs of `caches/<TID>/fakelib` (when non-empty) over the sandbox's
`<x>/common/lib` (the game's libs with ampr already copied on top, see Cache); one layer, no
mount of the ampr dir. Also nullfs of `/data` at `<sandbox>/data`, plus a nullfs of every mounted
`/mnt/usbN` / `/mnt/extN` at `<sandbox>/mnt/usbN` (pkg and packs on USB). Then, once the system's
`<sandbox>/app0` is mounted: when it is read-only, a read-write nullfs of `/system_ex/app/<TID>` (the
registered stack, backport unionfs included) over it; for a pkg game, a nullfs of the pkg's folder
(from the `.done` stamp's "source") at `<sandbox>/app0/kraken`; ng opens the .pkg named in `/app0/kraken.image`.
A single-file nullfs of the .pkg is refused by the kernel (ENOTDIR), hence the folder mount.
All removed on exit, newest first.
No overlay when neither layer applies.

## Module interfaces (contract between modules; keep these names)
```rust
// config.rs
pub const CONFIG_PATH: &str = "/data/krakenmount/config.toml";
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum FakelibMode { Emulators, Full, Off }
#[derive(Clone, PartialEq, Debug)]
pub struct Settings { pub scan_paths: Vec<String>, pub inject_ampr: bool, pub game_fakelib: FakelibMode,
  pub backports: bool, pub scan_interval_secs: u64, pub low_space_gb: u64, pub notify: bool, pub log: bool, pub debug: bool }
impl Default for Settings
pub fn load() -> Settings;                       // creates the file with defaults when missing
pub fn save(s: &Settings) -> std::io::Result<()>;
pub fn to_json(s: &Settings) -> serde_json::Value;          // keys = config keys, scanpath = array
pub fn from_json(v: &serde_json::Value) -> Result<Settings, String>;

// storage.rs
pub struct Drive { pub mount: String, pub label: String, pub kind: &'static str /* internal|usb|ext */,
  pub total: u64, pub free: u64 }
pub fn drives() -> Vec<Drive>;
pub fn drive_of(path: &std::path::Path) -> String;          // mount of the drive holding path ("/data", "/mnt/usb0")
pub fn default_scan_roots() -> Vec<String>;
pub fn mounted_aliases() -> Vec<String>;                    // mounted /mnt/usbN and /mnt/extN

// scan.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Packing { Plain, Zstd, Lz4 }
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Kind { Pkg, Folder(Packing) }
#[derive(Clone, Debug)]
pub struct Source { pub path: std::path::PathBuf, pub kind: Kind, pub title_id: String, pub title_name: String,
  pub version: String, /* param.json contentVersion, or empty */
  pub drive: String, pub size: u64 /* pkg file size; folder: 0 */, pub backport: Option<std::path::PathBuf> }
pub struct ScanResult { pub games: Vec<Source>, pub duplicates: Vec<Source>, pub errors: Vec<(std::path::PathBuf, String)> }
pub fn scan(roots: &[String], backports: bool) -> ScanResult;  // memoizes pkg param reads by (path, size, mtime)
pub fn parse_param(json: &[u8]) -> std::io::Result<(String, String, String)>; // (title_id, title_name, version)
pub fn valid_tid(t: &str) -> bool;

// cache.rs
pub const CACHES: &str = "/data/krakenmount/caches";
pub const AMPR_DIR: &str = "/data/krakenmount/ampr";
pub fn dir(tid: &str) -> std::path::PathBuf;
pub fn peek_pkg(pkg: &std::path::Path) -> std::io::Result<(String, String, String)>; // title_id, name, version (via scan::parse_param)
pub fn pkg_file(pkg: &std::path::Path, name: &str) -> std::io::Result<Vec<u8>>; // one file, e.g. "sce_sys/icon0.png"
pub enum Fresh { Fresh, Missing, Stale(String), Fakelib(String) }
pub fn refresh_fakelib(src: &Source, s: &Settings) -> std::io::Result<()>; // Fakelib: fakelib/ only
pub fn check(src: &Source, s: &Settings) -> Fresh;
pub fn build(src: &Source, s: &Settings) -> std::io::Result<()>;   // always rebuilds
pub fn app0(src: &Source) -> std::path::PathBuf;                    // pkg: dir(tid); folder: src.path
pub fn sce_sys(src: &Source) -> std::path::PathBuf;                 // where sce_sys lives for this source
pub fn built_at(tid: &str) -> Option<u64>;                          // .done mtime, unix secs
pub fn size(tid: &str) -> u64;                                      // bytes, recursive
pub fn du(path: &std::path::Path) -> u64;                           // file bytes, recursive; no symlink traversal
pub fn total_size() -> u64;
pub fn purge(keep: &[String]) -> std::io::Result<()>;               // delete caches/<TID> not in keep; empty the kept ones

// register.rs
pub fn register(title_id: &str, app0: &std::path::Path, sce_sys: &std::path::Path, backport: Option<&std::path::Path>) -> std::io::Result<()>;
pub fn register_tile(port: u16);
pub fn is_registered(title_id: &str) -> bool;
pub fn mount(title_id: &str, app0: &std::path::Path, backport: Option<&std::path::Path>) -> std::io::Result<()>;
pub fn unmount_title(tid: &str) -> std::io::Result<()>;   // every layer at /system_ex/app/<TID>, never MNT_FORCE
pub fn uninstall(tid: &str) -> std::io::Result<()>;       // sceAppInstUtilAppUnInstall

// launch.rs (titles: overlay these; running: title ids with a live session, kept by the watcher)
pub fn watch(titles: Arc<Mutex<HashSet<String>>>, running: Arc<Mutex<HashSet<String>>>);
```

## Web API (`src/web.rs`, port 8097, served to `src/ui.html`)
- `GET /` ui.html (`__VERSION__` filled in); `GET /app-icon.png` the app icon (`ps5/icon0.png`);
  `GET /bg.jpg` the background (`src/bg.jpg`); `GET /kraken.appcache` the AppCache manifest
  (version + page hash, with `/bg.jpg` in its `CACHE` section). These four are `no-cache`, the rest `no-store`.
- `GET /api/state`:
  `{version, busy, paused, running:[TID], games:[{title_id, title_name, version, path, kind:"pkg"|"folder", packing:"plain"|"zstd"|"lz4"|null,
  drive, size, status:"pending"|"checking"|"building"|"ready"|"error"|"duplicate", error, cache_size, cache_built, backport}],
  drives:[{mount, label, kind, total, free}], cache:{path, size, free, low_space, ampr_present}, log:[last 100 lines of the last 64 KiB]}`
- `GET /api/settings` -> settings JSON; `POST /api/settings` (JSON body) -> save, apply, rescan.
- `POST /api/rescan`, `POST /api/rebuild/<TID>`, `POST /api/rebuild-all`, `POST /api/purge` -> 204.
- `GET /api/sizes/<TID>` -> `{game: u64, dlc: u64, save: u64|null, cache: u64}` in bytes,
  measured on request; 404 for an invalid or absent library title. Game is the pkg file or folder,
  DLC is summed across `/user/addcont` and both extended drives' `user/addcont` and `ps5/user/addcont`,
  save is summed across eight-hex-digit `/user/home/<id>/<family>/<TID>` directories, where family
  is `savedata_prospero` for a `PPSA` title id and `savedata` otherwise.
  Missing directories count as zero; unreadable `/user/home` gives `save: null`.
- `POST /api/delete/<TID>`, body: the source path the UI showed (the game's `path`) -> 204 on success,
  404 for an invalid or absent library title, 409 while the scan loop is in a pass (it holds
  `State::work` for scan, purge, build and register; delete only try-locks it), when the body is not
  the current library source, while a game runs, this title builds, the source path is unsafe, or
  its title mount cannot be unmounted; 500 on source or cache deletion failure. It validates the
  source is inside a scan root with no symlink from the root down (links above the root are not checked), and is a .pkg file or game
  directory, then unmounts `/system_ex/app/<TID>` without force and deletes the source. If that
  fails, the cache and tile stay and the next pass remounts the title (a folder partly removed stays so).
  Then it uninstalls the home-screen entry (failure logged), removes the title from the launch set
  and its game row, wakes the scan loop, deletes the cache, and logs and optionally notifies.
- `GET /icon/<TID>` icon0.png, `GET /pic/<TID>` pic0.png (or pic1.png): from the source's sce_sys, fallback
  `/user/appmeta/<TID>/`, then (pkg) from the .pkg itself via `cache::pkg_file`; 404 when none. Keyed by title id; the UI must re-request when the title id appears.
- `GET /file/data/...` only with `debug=1`. `POST /api/quit`: loopback with an empty body (a newer copy
  taking over) or body `shutdown` from any host (the UI's Shut down, toast "Kraken Mount stopped"); 409 while a game runs.

## Self copy and start on open (as Dump Forge)
`ps5/build.sh` builds twice: stage 1, then stage 2 (`--features self-copy`, env `KRAKEN_SELF_ELF`) embedding
stage 1. The pushed stage 2 saves stage 1 to `/data/krakenmount/kraken-mount.elf` at start when the bytes differ
(temp + fsync + rename; the last pushed wins). The tile opens the AppCache'd page; on the console's own browser
(127.0.0.1:8097) a failed `/api/state` makes it ask elfldr once per page load:
`GET http://127.0.0.1:9021/data/krakenmount/kraken-mount.elf?args=serve&pipe=0&n=<nonce>`, then it keeps polling.
- Requests are capped at 16 KiB: head too large 431, head + Content-Length too large (or overflowing) 413,
  non-numeric Content-Length 400.

## Build and check
`cargo check --target x86_64-unknown-freebsd` (fast, host). Full payload: `ps5/build.sh` (Docker).
