# krakenmount design

A Rust PS5 ELF payload (`src/`). It finds games on the internal SSD and on USB/extended drives,
keeps a per-game cache on the internal SSD, registers each game on the home screen and, at
launch, overlays the game's fakelib and the shared ampr-emu-ng dir on the sandbox's system libs.
The ampr-emu-ng runtime is NOT built here: its released `libSceAmpr.sprx` is bundled from
`vendor/ampr-emu-ng/` (`VERSION`, `SHA256SUMS` from the GitHub release, checked by `ps5/build.sh`)
and installed into `/data/krakenmount/ampr/` at start.
Two third-party emulators are bundled the same way (released files, `VERSION` and `SHA256SUMS` from
their GitHub releases, checked by `ps5/build.sh`; GPL-3.0, `THIRD_PARTY_NOTICES`): drakmor's dlc_emu
(`vendor/dlc_emu/`: `libSceAppContent.sprx`, `libSceNpEntitlementAccess.sprx`, `libSceGameUpdate.sprx`)
and pgo_stub (`vendor/pgo_stub/`: `libScePlayGo.sprx`). They are embedded in the payload and written
only into a cache's `fakelib/`, never into `/data/krakenmount/ampr/`.
`crates/kraken-mount-pkg` reads debug FPKGs (the reader plus its Kraken block decoder, module `decoder`).

## Supported sources (nothing else)
- **FPKG**: a dump-forge debug `.pkg` file directly in a scan root.
  Sparse layout extents (holes: a kind-0x40 boundary up to the next boundary) are read as zeros.
- **Folder**: a directory directly in a scan root with `eboot.bin` and `sce_sys/param.json`.
  Packing: `ampr_zstd.index` present = zstd, else `ampr_assets.index` = lz4, else plain.
- Any drive. The cache is always on the internal SSD.

## Paths
| Path | What |
|---|---|
| `/data/krakenmount/config.toml` | settings (created with defaults on first start) |
| `/data/krakenmount/caches/<TID>/` | per-game cache |
| `/data/krakenmount/ampr.bundled` | `<version> <len> <crc32>` of the `libSceAmpr.sprx` we last wrote, bundled or downloaded (v0.4.0 builds wrote `<len> <crc32>`: ours, version unknown; outside `ampr/`, whose files all go into fakelib) |
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
Title name (`scan::parse_param`, as Dump Forge's gameTitle): the `localizedParameters` `titleName` in the
console language (`i18n::title_tag`: `SCE_SYSTEM_PARAM_LANG_*` -> its param.json tag, all 31 languages,
en-US when unreadable), else the same language in another region (es-419 -> es-ES), else the default
language's, else en-US, else any, else the title id; blank names skipped. Read at each parse, so a language
change shows on the next scan (an unbuilt pkg's memoized read keeps its first language until it changes).
It is display only: UI, sort order, toasts, logs.
Eject (`POST /api/eject`): `/mnt/usb0..7` and `/mnt/ext0` are ejectable (`/mnt/ext1` is the M.2 slot,
SMP `sm_usb_mount_path`). The request goes to the system automounter over IPMI to `SceAutoMounterIpc`
in `ps5/usb.c` (`km_usb_eject`, a port of SMP `sm_usb.c`), which checks the mount still has the
source and fsid seen at request time; the payload never unmounts the drive itself.

Packages installed by the system (`src/installed.rs`, setting `auto_import_installed`) are found
separately from the scan roots: `/user/app/<TID>/app.pkg` and the same layout under
`/mnt/ext0/user/app`, `/mnt/ext0/ps5/user/app`, `/mnt/ext1/user/app` and `/mnt/ext1/ps5/user/app`
(a regular file under a title-id directory, both matching the pkg's own `param.json` title id).
They join the scan as ordinary pkg sources and are built, registered and launched from their
original path: nothing is copied, moved or deleted, and `/api/delete` refuses them. Before the pkg
is read, a restricted install gets the missing read bits (pkg +0444, its directory +0555; bits are
only added, as `fix_access`); the title id and size are rechecked, and a symlink or another file
name is refused. A copy found in a scan root keeps the usual duplicate rule and wins over them.

## Settings (`src/config.rs`, file `/data/krakenmount/config.toml`)
A flat TOML subset: `key = value` with strings, `true`/`false`, integers and arrays of strings; `#` comments.
| Key | Default | Meaning |
|---|---|---|
| `scanpath` | (defaults above) | array of paths; replaces the default scan roots |
| `auto_import_installed` | true | also list packages the system installed (`/user/app/<TID>/app.pkg` and the extended-storage roots) as games, in place |
| `inject_ampr` | true | copy `/data/krakenmount/ampr/*` on top of a folder game's fakelib; pkg games always get it (pkg mode exists only in ampr-emu-ng) (a change only refreshes `fakelib/`) |
| `add_emulators` | true | add the bundled DLC (dlc_emu) and PlayGo (pgo_stub) emulators to a game's fakelib when it needs them and ships none (a change only refreshes `fakelib/`) |
| `ampr_auto_update` | true | once per start, download a newer ampr-emu-ng release from GitHub (a hand-placed `libSceAmpr.sprx` is kept) |
| `backports` | true | apply `<scan root>/backports/<TID>*` |
| `scan_interval_seconds` | 10 | scan loop period (1..3600) |
| `stability_wait_seconds` | 10 | skip a source changed less than this many seconds ago (still being copied) (0..3600, 0 = off) |
| `low_space_gb` | 20 | warn when `/data` free space is below this |
| `auto_remove_missing` | true | remove a title from the home screen when its source stays missing for `auto_remove_delay_seconds` (the cache stays; see Flow) |
| `auto_remove_delay_seconds` | 300 | how long a source must stay missing before `auto_remove_missing` removes its title (1..86400) |
| `notify` | true | system toasts |
| `log` | true | write `/data/krakenmount/kraken.log` (stdout always gets the lines) |
| `debug` | false | klog capture to `/data/krakenmount/klog.txt` and the `/file/data/...` route |

Invalid values are logged and replaced by the default. The web UI edits the same file.

## Cache (`src/cache.rs`)
`/data/krakenmount/caches/<TID>/`, stamp file `.done` written last.
- Folder: `fakelib/` only. `/app0` is the game folder itself.
- FPKG: it IS `/app0`: `sce_sys/**` (with the files only the pkg container holds: `save_data.png`,
  `pic0`/`pic1`, `snd0.at9`, `trophy00.ucp`, `nptitle.dat`, `npbind.dat`, which save data and
  trophies read from `/app0/sce_sys`), `eboot.bin`, every `*.prx`/`*.sprx`, and a root-level `dlc_emu.ini` / `playgo_stub.dat` (the emulators
  open `/app0/<name>`; an older cache lacks them loose, ng still serves them from the pkg) extracted (0777),
  `fakelib/`, `kraken/` (mount point for the pkg's folder, see Launch), `kraken.image` (one line, no
  newline: `kraken/<pkg file name>`, relative to `/app0`, the file ng opens; missing means stale), `ampr_emu.index` +
  `ampr_assets.index` (`src/ampr_index.rs`; every non-extracted file packed, fakelib and
  backport files loose). Backport files (except its fakelib) are copied loose over pkg files.
  A file with sparse holes stays packed: each hole block is a RAW chunk of pack 1,
  `kraken_zero.dat` (an AMPRDAT3 volume in the cache: one 64 KiB header page + 256 KiB zeros,
  written only when a hole is packed; ng's pkg mode opens packs 1 and up as AMPRDAT3 volumes,
  `ampr-sys` `rt/pack/pkg.rs` `build_pack_path_pkg`). Any file `ampr_index::packable` refuses is
  extracted loose instead: holes that do not start and end on its 256 KiB block grid (logged per
  file with the title id and path), or a short compressed block that decodes past the file's end
  (ps5-dump-forge packs several small files into one block), or data starting inside such a shared
  block (`Pkg::blocks` refuses it; `read_at` decodes it); one line per title: count and bytes.
  The dir itself is never deleted: `/system_ex/app/<TID>` is a nullfs of it.
- `/app0` is writable, so a game may write into its cache (pkg) or its dump folder (folder, also on
  USB). `.built` (written before `.done`) lists what the build created; a rebuild removes only those
  paths (a dir only once empty), so files the game wrote stay. A failed build removes what it wrote.
  A cache without `.built` (older build) is emptied. Only Purge deletes the game's files.
- `fakelib/` composition: base = backport `fakelib/` (all files) if present, else the game's
  `fakelib/` (all files but `libSceSaveData.native.sprx`, which breaks saves; the old `game_fakelib`
  setting is gone and ignored in `config.toml`); `libkernel.sprx` always dropped. A backport's lib dir
  keeps `libSceSaveData.native.sprx` (a backport is deliberate). `fakelib2/` (SMP) wins
  over `fakelib/` and is used as-is (it keeps `libSceSaveData.native.sprx` too); it is never indexed or copied as game files. All files 0777.
  Then, with `add_emulators`, each bundled emulator the game needs is added when `fakelib/` has no file of
  that name (case-insensitive). A file of that name that is a known older release (dlc_emu 0.1a or 0.2, pgo_stub 0.4
  or 0.5, matched by size and crc32, both builds of each) is replaced by the bundled one, whether or not the game
  needs it, logged `<TID>: fakelib: upgraded <name> (<project> <version>)`; any other copy (custom, the bundled
  release, a log build) is kept, so the game's or backport's own copy wins and nothing is downgraded: the dlc_emu set
  (its 3 files, each only if missing) when `dlc_emu.ini` (exact name, as dlc_emu opens it) is at the folder game's root, the backport's root or
  the pkg's root; `libScePlayGo.sprx` when `eboot.bin` (the backport's if it has one, else the game's; pkg:
  the extracted copy) contains the string `libScePlayGo` (an import; an encrypted eboot or an import only in
  another prx is missed). Each add logs `<TID>: fakelib: added <name> (<project> <version>)`.
  The dlc_emu set is all-or-nothing: when any of its 3 files is a copy that is neither a known older release nor the bundled file, none of the 3
  is added or upgraded (no mixed set), logged `<TID>: fakelib: kept the game's dlc_emu files (not a known release)`; pgo_stub goes per file.
  Then, with `inject_ampr` and always for a pkg game (the package's own `libSceAmpr.sprx` cannot read the
  `.pkg` named in `kraken.image`), every file of `/data/krakenmount/ampr/` (not `libkernel.sprx`) is copied on top,
  so ours wins over the game's own `libSceAmpr.sprx`, except a game's or backport's ampr-emu-ng strictly newer than
  ours (the `ampr-emu-ng X.Y.Z` identity in both files; ours without one counts as older), which is kept and logged
  `<TID>: fakelib: kept the game's ampr-emu-ng <v> (newer than <v>)` (no ng downgrade, also over a hand-placed copy). `.ampr` records what fakelib was composed from: `{"ampr": {name: [size, mtime]} or null,
  "emus": null without `add_emulators`, else the dlc_emu and pgo_stub versions, whether a `dlc_emu.ini` is on
  disk (folder or backport root) and the eboot's [size, mtime], "playgo": [eboot size, mtime secs, imports PlayGo]}`
  (a pkg's own `dlc_emu.ini` is not in it, a pkg change rebuilds through the stamp). "playgo" memoizes the PlayGo
  import search: `eboot.bin` is read again only when its (size, mtime) differs (a read error is not memoized), and
  `check` never compares it. An older `.ampr` without `emus` differs, so every cache refreshes `fakelib/` once; that
  one-time refresh also drops a `libSceSaveData.native.sprx` an older build copied (no separate check for such files).
  When "ampr" or "emus" differ (the ampr dir, `inject_ampr` or the emulator inputs changed) or `.done` still has a
  `game_fakelib` other than `full` (a cache built before the setting was removed), `check` says `Fakelib` and only `fakelib/` is
  recomposed (`refresh_fakelib`, which then rewrites `.done` with its own mtime), no extraction or re-registration. `fakelib/` is not in the AMPR index.
- A folder game's cache is only `fakelib/`, `.ampr`, `.built`, `.done`: nothing of the game is copied.
- Fresh when `.done` exists, its JSON stamp equals the expected one
  (`{"v":2,"kind","source","size","backport","fakelib"}`; extra keys of an older stamp are ignored), and every input's
  mtime is older than `.done`'s mtime. Inputs: FPKG = the .pkg, backport dir (recursive);
  folder = `eboot.bin`, `sce_sys/param.json`, its lib dir and files (not the folder itself: the game
  writes there). Otherwise rebuilt. A pkg's `.done` also carries `pkg_id` (FNV-1a 64 of the size,
  the first 64 KiB, the 4 KiB CNT header and the last 64 KiB), a folder's `folder_id` (FNV-1a 64 of
  each input file's path relative to the folder and size, plus the bytes of every input but
  `eboot.bin`: `param.json` and the lib dir's files); neither is
  compared by the freshness check.
- `"v"` is the cache layout. Another value is Stale (`cache layout v1 -> v2`), checked before Moved,
  so every older cache rebuilds once, moved or not. v2: the container-only `sce_sys` files and the
  extraction of every file the index cannot map.
- Moved (`Fresh::Moved(old source)`, `relink`): the stamp differs only in "source" (and "fakelib"
  when it is the folder's own lib dir under the moved path), the old source path is gone, and the
  files are the same: pkg = `pkg_id` equal (the moved .pkg may be newer than `.done`), its other
  inputs older than `.done`; folder = `folder_id` equal (mtimes not compared: a copy-then-delete
  move relinks too). A copy while the old one
  still exists is a duplicate, never a move. Relink: pkg = new stamp, `kraken.image` rewritten when
  the file name changed, `.done` mtime now, extracted files and indexes kept (ng checks the pkg's
  size only); folder = new stamp, `.done` mtime now. Anything else is Stale (a cache stamped before `pkg_id`/`folder_id`
  existed rebuilds once when its game moves).
- Access (`fix_access`, once per run per title from `main.rs`; never fails): games and system
  services (save data, trophies) read the game's files with another uid than the payload. It only
  adds bits, chmods only when some are missing, lstats and never follows symlinks, skips entries
  that are neither file nor dir: the cache dir recursively (dirs 0777, `/app0` is writable; files
  +0444); pkg = the .pkg +0444 and its folder +0555 (the `/app0/kraken` nullfs: ng falls back to
  LZ4 mode when it cannot open the .pkg); folder = the game folder recursively (dirs +0555, files
  +0444). chmod failures (exFAT USB, read-only) are counted: one log line per title with the title
  id, modes fixed, failures and the first failed path and error (none when nothing changed).
- Purge (settings): delete caches of titles not in the library (`register::unmount_title` first;
  a dir whose unmount fails is skipped and logged), empty and rebuild the rest.

## Flow (`src/main.rs`)
Start: `umask(0)` first, so every dir/file the payload creates is 0777/0666 (as SMP, which mkdirs
0777) whatever elfldr passes; then create dirs, load config, take port 8097 (asks an older copy to quit), install the bundled
`libSceAmpr.sprx` when `ampr/libSceAmpr.sprx` is missing or matches `ampr.bundled` (an older bundled
copy; a file put there by hand is kept; see Auto-update below), ampr chmod, web thread, klog thread when `debug`, then wait
(2 s poll, one toast per reason, `waiting` in `/api/state` for the UI banner) until kstuff answers its KEKCALL_CHECK probe (`sys::kstuff_loaded`,
SMP `sm_kstuff_is_loaded`) and no `shadowmountplus.elf` process runs, then the launch watcher thread. Loop every `scan_interval_seconds` or on
wake: reload config, scan, then for each game: freshness check, build if stale or forced,
register (once per run, and after every rebuild), status `ready`. Right before the register (or the
mount-only path below), once per run per title, `cache::fix_access` opens up the cache and the
source for the game and the system services (it only adds missing bits); a relinked moved game
gets it right after `cache::relink`. A game whose cache is fresh and
which is already registered (`register::is_registered`) is not re-registered: only `register::mount`
redoes the missing app0 nullfs (gone after a reboot); for such a pkg the scan reads title id/name from
the cache's `sce_sys/param.json` and never opens the .pkg. A fresh title registered by this run still
gets `register::mount` every scan (a no-op when app0 is mounted there; redoes it after a USB replug);
a title that leaves the scan is forgotten, so it goes through the full path when it is back.
A moved game (`Fresh::Moved`, not forced): a folder's `/system_ex/app/<TID>` stack is redone on the
new folder first (`register::remount`, backport unionfs included, never MNT_FORCE), then
`cache::relink`; logged with the title id and both paths, toast `i18n::moved`; then the usual
fresh path. A failed relink or remount falls back to a rebuild.
Stability (`stability_wait_seconds`, SMP sm_stability.c): a source whose youngest max(mtime, ctime) is
under the wait is still being copied (a pkg: the file; a folder: the folder, `sce_sys/`,
`sce_sys/param.json` and `eboot.bin` only) and is skipped this pass (`ScanResult::unstable`, logged once until stable): not gone, so its entry,
registration and cache stay, and a new one shows up once it is stable. A Purge waits until no
source is being copied (purge empties kept caches, and an unstable one would not rebuild that pass). A clock in the future counts as stable.
An unstable source reserves its title id for duplicate detection (a pkg: the title id of an earlier cache of that path; a
folder: its `param.json`, else that cache), so a copy in flight that would win keeps the other copy out (an unstable loser reserves nothing).
Auto-remove (`auto_remove_missing`, SMP auto_remove_missing_games): after each full scan pass (timers
start only after the first one, so a drive mounted late is not missing; not while paused), a built cache
(`.done`) whose stamp source is gone (ENOENT/ENOTDIR only) starts a timer in memory (a restart starts over).
Excluded, timer reset: a title this scan lists (moved, or still being copied), a running one, one with DLC in
`/user/addcont/<TID>` (a stat error there protects too). Missing for `auto_remove_delay_seconds`: unregister
as `/api/delete` (each title rechecks that no game runs; unmount without force, a busy mount retried next pass;
uninstall, then `register::forget` so a returning source registers in full: `is_registered` reads app.db as bytes); the cache stays for a
return; a failed uninstall is logged once and not retried until the source returns or a restart.
A title with an installed copy (`/user/app/<TID>/app.pkg` or `/mnt/ext{0,1}/user/app/<TID>`; a stat error other than
ENOENT/ENOTDIR counts as one) or no staged `/user/app/<TID>/sce_sys/param.json` is never removed (`register::is_ours`,
rechecked every pass, logged once per run).
Auto-update (ampr-emu-ng): at start, when `ampr_auto_update` is on (whatever `inject_ampr` says: pkg games always use the ampr dir), a background thread asks
`https://api.github.com/repos/quer3q/ampr-emu-ng/releases/latest` (stable releases only; betas are
pre-releases and skipped). When its `tag_name` is newer than the installed version, it downloads the
asset `libSceAmpr.sprx` (size must equal the asset size, max 16 MiB, ELF or SELF magic), writes a temp
file in `/data/krakenmount`, chmod 0777, renames it to `/data/krakenmount/ampr/libSceAmpr.sprx`, records
it in `ampr.bundled` as `<tag> <len> <crc32>` and toasts `i18n::ampr_updated`. A copy we wrote (bundled
or downloaded) is ours: the bundled copy replaces ours only when the bundled version is newer; a
hand-placed file is kept by both, and ownership is checked again after the download. Network errors are
logged and retried up to 3 times 20 s apart, then nothing until the next start. An install wakes the scan
loop (fakelib refresh); one update runs at a time. Settings has "Update ampr-emu-ng now"
(`POST /api/ampr/update`): forced, one attempt, installs the latest stable release whatever is
installed; a hand-placed file is first moved to `/data/krakenmount/libSceAmpr.sprx.hand` (one backup,
outside `ampr/`). HTTPS goes through the console's sceHttp/sceSsl with 4 built-in root CAs
(from SMP).
Running game: while `launch::watch` reports a live session (`running`), the loop does no
scan/build/register/purge (a game starting mid-scan stops the scan before the next title);
rescan/rebuild/purge requests stay queued and run after the game exits (2 s poll).
Toasts: `sceNotificationSend` system toast (dump-forge/SMP JSON, icon `/user/data/krakenmount/notify-icon.png`),
the old `sceKernelSendNotificationRequest` only as fallback. Toast texts (`src/i18n.rs`) follow the console language
(`sceSystemServiceParamGetInt` lang, read per toast; dump-forge's 11: en ru zh de es fr ja pt it ko ar), English when it
cannot be read; "Kraken Mount", title names and paths stay as they are. A failed game is retried
only when its inputs change or a rebuild is forced. Duplicate title ids: a folder beats a pkg, then the first scan root wins;
the others show `duplicate`. Low `/data` space (checked before the games, before and after each
build, after the scan): toast once per run and a UI banner.

## Register (`src/register.rs`)
SMP port: copy `sce_sys` to `/user/app/<TID>/sce_sys` + `icon0.png`, appmeta, trophy data,
nullfs `app0 -> /system_ex/app/<TID>` (app0 = cache dir for FPKG, game folder for Folder),
for a Folder with a backport a unionfs of the backport dir on top (SMP
`mount_backport_overlay`), then `sceAppInstUtilAppInstallTitleDir` (fallback InstallAll).
Every staged copy (sce_sys, `icon0.png`, appmeta, trophy files) is set to 0666 after writing: a
copy keeps the source's mode, and a 0600 file from a game folder would stay unreadable for the
trophy and save services. A copied file named `param.json` gets SMP `copy_param_json_rewrite`: the
first `upgradable` becomes `standard` (`applicationDrmType`), logged `<TID>: param.json patched:
<dst>`; only the staged copies, never the game's own `sce_sys`. Trophy files
(`/system_data/priv/appmeta/<TID>/{trophy2,uds}/npbind.dat`, `param.json`) already there are not
overwritten (SMP), but get +0444 when a read bit is missing.
`/system_ex` is read-only unless etaHEN, SMP or kstuff-lite remounted it (elfldr does not): when
`mkdir -p /system_ex/app/<TID>` fails with EROFS, register remounts it once like SMP `remount_system_ex`
(`nmount` MNT_UPDATE of `/dev/ssd0.system_ex`) and retries. An existing title dir (EEXIST) needs no remount.
Title sound (`set_snd0info`, SMP `update_snd0info`): when `/user/appmeta/<TID>/snd0.at9` exists,
`UPDATE tbl_contentinfo SET snd0info = '/user/appmeta/<TID>/snd0.at9' WHERE titleId = <TID>` (rows
already pointing there are skipped) in `/system_data/priv/mms/app.db`, through `ps5/appdb.c` and the
SQLite 3.46.1 amalgamation (opened read-write, never created, busy timeout 3 s, one mutex for both
threads). Run 200 ms after a successful install call, once per run for a title already registered
(backfills older registrations), and after each game exit (the system rewrites the path); each runs on its own thread
(`spawn_snd0info`, the post-install one after the 200 ms), so a busy app.db (up to 3 s) holds neither the scan nor the watcher. Logged
`<TID>: snd0info set (<n> row(s))` only when a row changed; a failure is logged, never fatal.

## Launch (`src/launch.rs`)
As before (SMP sm_fakelib port): unionfs of `caches/<TID>/fakelib` (when non-empty) over the sandbox's
`<x>/common/lib` (the game's libs with ampr already copied on top, see Cache); one layer, no
mount of the ampr dir. Also nullfs of `/data` at `<sandbox>/data`, plus a nullfs of every mounted
`/mnt/usbN` / `/mnt/extN` at `<sandbox>/mnt/usbN` (pkg and packs on USB). Then, once the system's
`<sandbox>/app0` is mounted: when it is read-only, a read-write nullfs of `/system_ex/app/<TID>` (the
registered stack, backport unionfs included) over it; for a pkg game, a nullfs of the pkg's folder
(from the `.done` stamp's "source") at `<sandbox>/app0/kraken`; ng opens the .pkg named in `/app0/kraken.image`.
A single-file nullfs of the .pkg is refused by the kernel (ENOTDIR), hence the folder mount.
All removed on exit, newest first. When a session's cleanup ends, `register::spawn_snd0info(<TID>, 0)` (own thread).
No overlay when neither layer applies.

## Module interfaces (contract between modules; keep these names)
```rust
// config.rs
pub const CONFIG_PATH: &str = "/data/krakenmount/config.toml";
#[derive(Clone, PartialEq, Debug)]
pub struct Settings { pub scan_paths: Vec<String>, pub auto_import_installed: bool,
  pub inject_ampr: bool, pub add_emulators: bool,
  pub ampr_auto_update: bool,
  pub backports: bool, pub scan_interval_secs: u64, pub stability_wait_secs: u64, pub low_space_gb: u64,
  pub auto_remove_missing: bool, pub auto_remove_delay_secs: u64, pub notify: bool, pub log: bool, pub debug: bool }
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
pub fn ejectable(mount: &str) -> bool;                      // /mnt/usb0..7, /mnt/ext0

// scan.rs
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Packing { Plain, Zstd, Lz4 }
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Kind { Pkg, Folder(Packing) }
#[derive(Clone, Debug)]
pub struct Source { pub path: std::path::PathBuf, pub kind: Kind, pub title_id: String, pub title_name: String,
  pub version: String, /* param.json contentVersion, or empty */
  pub drive: String, pub size: u64 /* pkg file size; folder: 0 */, pub backport: Option<std::path::PathBuf> }
pub struct ScanResult { pub games: Vec<Source>, pub duplicates: Vec<Source>, pub errors: Vec<(std::path::PathBuf, String)>,
  pub unstable: Vec<(std::path::PathBuf, Option<String> /* title id when known */)> } // still being copied: skipped, not gone
pub fn scan(roots: &[String], backports: bool, stability_wait: u64, import_installed: bool) -> ScanResult;  // memoizes pkg param reads by (path, size, mtime)
pub fn installed_source(path: &std::path::Path, expected: &str) -> std::io::Result<Source>; // a system-installed pkg, in place
pub fn still_written(paths: &[std::path::PathBuf], wait: u64) -> Option<u64>; // age in s of the youngest max(mtime, ctime) under wait; None = stable
pub fn parse_param(json: &[u8]) -> std::io::Result<(String, String, String)>; // (title_id, title_name, version); name: see below
pub fn valid_tid(t: &str) -> bool;

// installed.rs (packages the system installed, kept in place)
pub fn package_path(path: &std::path::Path) -> bool;        // <app root>/<TID>/app.pkg, valid title id
pub fn paths() -> Vec<(std::path::PathBuf, String)>;         // every installed package, in title-id order
pub fn prepare(path: &std::path::Path, tid: &str, size: u64) -> std::io::Result<()>; // adds missing read bits only

// cache.rs
pub const CACHES: &str = "/data/krakenmount/caches";
pub const AMPR_DIR: &str = "/data/krakenmount/ampr";
pub fn dir(tid: &str) -> std::path::PathBuf;
pub fn peek_pkg(pkg: &std::path::Path) -> std::io::Result<(String, String, String)>; // title_id, name, version (via scan::parse_param)
pub fn pkg_file(pkg: &std::path::Path, name: &str) -> std::io::Result<Vec<u8>>; // one file, e.g. "sce_sys/icon0.png"
pub enum Fresh { Fresh, Missing, Stale(String), Fakelib(String), Moved(std::path::PathBuf /* old source */) }
pub fn relink(src: &Source, s: &Settings, from: &std::path::Path) -> std::io::Result<()>; // Moved: no rebuild
pub fn pkg_id(pkg: &std::path::Path) -> std::io::Result<String>;   // sampled identity, see Cache
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
pub fn fix_access(src: &Source);                                    // add missing read/exec bits, see Cache "Access"

// register.rs
pub fn register(title_id: &str, app0: &std::path::Path, sce_sys: &std::path::Path, backport: Option<&std::path::Path>) -> std::io::Result<()>;
pub fn register_tile(port: u16);
pub fn is_registered(title_id: &str) -> bool;
pub fn set_snd0info(title_id: &str);                      // app.db snd0info -> /user/appmeta/<TID>/snd0.at9; logs, never fails
pub fn spawn_snd0info(title_id: &str, delay_ms: u64);    // set_snd0info on its own thread after delay_ms
pub fn mount(title_id: &str, app0: &std::path::Path, backport: Option<&std::path::Path>) -> std::io::Result<()>;
pub fn remount(title_id: &str, app0: &std::path::Path, backport: Option<&std::path::Path>) -> std::io::Result<()>; // moved folder: whole stack
pub fn unmount_title(tid: &str) -> std::io::Result<()>;   // every layer at /system_ex/app/<TID>, never MNT_FORCE
pub fn uninstall(tid: &str) -> std::io::Result<()>;       // sceAppInstUtilAppUnInstall
pub fn forget(tid: &str);                                 // after an auto-remove: drop the staged sce_sys/param.json so is_registered is false
pub fn is_ours(title_id: &str) -> Result<(), std::path::PathBuf>; // Err(path) = an installed copy, or no staged param.json: never auto-remove

// main.rs (shared version compare, used by cache.rs for ampr-emu-ng identities)
pub(crate) fn parse_version(v: &str) -> Option<(u32, u32, u32)>; // "X.Y.Z", leading "v" allowed
pub(crate) fn newer(a: Option<&str>, b: Option<&str>) -> bool;   // a strictly newer than b; an unknown version is older than any

// launch.rs (titles: overlay these; running: title ids with a live session, kept by the watcher)
pub fn watch(titles: Arc<Mutex<HashSet<String>>>, running: Arc<Mutex<HashSet<String>>>);

// net.rs (blocking GET over sceHttp/sceSsl, 4 pinned root CAs; Err on non-200, sce error or > max bytes)
pub fn https_get(url: &str, max: usize) -> std::io::Result<Vec<u8>>;
```

## Web API (`src/web.rs`, port 8097, served to `src/ui.html`)
- `GET /` ui.html (`__VERSION__` filled in); `GET /app-icon.png` the app icon (`ps5/icon0.png`);
  `GET /kraken.appcache` the AppCache manifest (version + page hash). These three are `no-cache`,
  the rest `no-store`.
- `GET /api/state`:
  `{version, busy, paused, waiting:"kstuff"|"shadowmount"|null, running:[TID], games:[{title_id, title_name, version, path, kind:"pkg"|"folder", packing:"plain"|"zstd"|"lz4"|null,
  drive, size, installed_pkg: bool, status:"pending"|"checking"|"building"|"ready"|"error"|"duplicate", error, cache_size, cache_built, backport}],
  drives:[{mount, label, kind, total, free, ejectable}], cache:{path, size, free, low_space, ampr_present}, log:[last 100 lines of the last 64 KiB]}`
- `GET /api/settings` -> settings JSON; `POST /api/settings` (JSON body) -> save, apply, rescan.
- `GET /api/ampr` -> `{kind: "bundled"|"downloaded"|"hand"|"missing"|"error", version, updating}` of
  `ampr/libSceAmpr.sprx`; `POST /api/ampr/update` -> forced update, answered when done: 200 + result text,
  409 while one runs, 502 + error text.
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
  its title mount cannot be unmounted, or the source is a package the system installed; 500 on source or cache deletion failure. It validates the
  source is inside a scan root with no symlink from the root down (links above the root are not checked), and is a .pkg file or game
  directory, then unmounts `/system_ex/app/<TID>` without force and deletes the source. If that
  fails, the cache and tile stay and the next pass remounts the title (a folder partly removed stays so).
  Then it uninstalls the home-screen entry (failure logged), removes the title from the launch set
  and its game row, wakes the scan loop, deletes the cache, and logs and optionally notifies.
- `POST /api/eject`, body: the drive's mount path (`/mnt/usb0`) -> 204 once the path is no longer a
  mount point; 400 when not ejectable, 404 when not mounted, 409 while the scan loop is in a pass
  (try-lock of `State::work`, held to the end so no pass builds, registers or remounts meanwhile),
  while a game runs, when another partition of the same disk (`/dev/daN` followed by the end, `p` or
  `s`) is mounted, when a title mount from the drive cannot be unmounted (error text), or when the
  drive changed (`km_usb_eject` ENOENT/ESTALE); 502 with errno and native status in hex when the
  request fails, or when the mount is still there after 5 s (polled every 100 ms). Before the request it
  unmounts, without force, every `/system_ex/app/<TID>` whose source (minus `<above>:`) is the drive
  or under it; other mounts are left to the system (as SMP). Then it wakes the scan loop: the next
  pass forgets the gone drive's games, or after a failure remounts the titles. Every outcome is
  logged as `eject <path>: ...`.
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
