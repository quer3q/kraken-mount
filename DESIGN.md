# Kraken Mount design

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
- **Image**: a `.exfat` (exfatfs), `.ffpkg` (UFS), `.ffpfs` (PFS) or `.ffpfsc` (PFS container holding
  one more image) file, any case, directly in a scan root (or one folder below, as the others), as made by
  SMP's tools. Read first without attaching it (`image::probe`, see Images), then mounted read-only; the game
  folder is the image root when it has `eboot.bin` + `sce_sys/param.json`, else the first such folder directly
  inside it (sorted). From then on it is a
  **Folder** source (`Source.path` = that folder in the mount, `Source.image` = the image file,
  `Source.size` = the image size, `drive` = the image's drive).
- Any drive. The cache is always on the internal SSD.
- **Installed**: a game installed on the console (`/user/app/<TID>` or `/mnt/ext{0,1}/user/app/<TID>`, see
  Installed games). Never registered by Kraken Mount; it keeps a fakelib-only cache for the launch overlay. With
  `ampr_installed` (per title) an internal debug FPKG install is a pkg source instead: built from its `app.pkg` and
  only mounted at `/system_ex/app/<TID>` (see Flow). An FPKG game, or a patch/DLC `.pkg` in a scan root, can be installed from the web UI (Install).

## Images (`src/image.rs`, port of SMP `sm_image.c` / `sm_mount_device.c`, LVD backend only)
- Profile (SMP `get_image_mount_profile` and helpers): *nested* = a `.ffpfsc` container or a PFS image
  inside one; *legacy* = nested, or the image is not under `/data` or `/user`.
  | | LVD type | sector | secondary unit | attach flags | nmount flags |
  |---|---|---|---|---|---|
  | `.exfat` | 5, legacy 0 | 512 | 0x10000 | 0x1c | RDONLY |
  | `.ffpkg` | 5, legacy 7 | 4096 | 0x10000, legacy = sector | 0x1c, legacy 0x1e | RDONLY, legacy +NOATIME |
  | `.ffpfs` | 5 | 4096 | 0x10000, legacy = sector | 0x1c | RDONLY |
  | `.ffpfsc` (outer) | 9 (AcO) | 4096 | sector | 0x1c | RDONLY |
  | PFS inside `.ffpfsc` | 8 (AcI) | 4096 | sector | 0x1c | RDONLY |

  A non-nested image takes the backing filesystem's block size (statfs `f_bsize`, else `f_iosize`) as sector
  when smaller than the default. Attach: `/dev/lvdctl` ioctl `0xC0286D00` (V0, one file layer, no bitmap);
  wait up to 10 s for `/dev/lvdN`.
- nmount options (SMP's, without `errmsg`/`force`): ufs `budgetid=game,async,noatime,automounted`;
  exfatfs `budgetid=game,large=yes,timezone=static,async,noatime,ignoreacl,automounted`; pfs (both PFS
  formats) `sigverify=0,mkeymode=AC,budgetid=game,playgo=0,disc=0,ekpfs=<64 zeros>,async,noatime,automounted`.
  Mount point `/mnt/krakenmount/[pfsc_]<stem>_<fnv1a32(image path)>` (stem: 27 ASCII alnum/`-`/`_` at
  most; `pfsc_` for a `.ffpfsc`), verified by statfs (mounted from `/dev/lvdN`). Any failure detaches the unit.
- Probe (`src/image/probe.rs`, before any attach): ps5-dump-forge's readers open the image file itself:
  `ps5-dump-forge-pfs` for `.ffpfs` (`PfsSource`) and `.ffpfsc` (`open_ffpfsc`: exactly one image in the
  container's root, `.exfat`/`.ffpkg`/`.ufs2`/`.ffpfs` or `pfs_image.dat`), `ps5upload-fpkg` for `.exfat`
  (`exfat::ExFatSource`) and `.ffpkg` (`ufs2_source::Ufs2Source`). From the file list: the game folder (root
  first, else the first folder directly inside, sorted, with `eboot.bin` + `sce_sys/param.json`) and that
  folder's `param.json` (at most `MAX_PARAM_BYTES`). One probe per (path, size, mtime), errors included: an
  image is walked again only when it changes. A probe or `param.json` error is a scan error of the image file;
  it is never attached.
- `.ffpfsc`: the container is mounted, then the inner image the probe named, in that mount (`pfs_image.dat`
  or a name without a known extension is PFS, `.ufs2` is UFS). The game folder is in that inner mount.
- After the mount, the probe's game folder must hold `eboot.bin` + `sce_sys/param.json`; else (the reader
  and the filesystem disagree) the image's mounts are released and it is a scan error. Any failed mount
  (that one, an attach or nmount error, a `.ffpfsc` whose inner image does not mount) releases what it
  attached and is remembered per (path, size, mtime): the same file is not attached again for 10 minutes
  (the same scan error is reported meanwhile); a changed file is tried at once.
- `/data/krakenmount/images/<mount name>` = `<unit>\n<image path>`: a later copy of the payload reuses a
  live mount of the same image; a dead one (drive unplugged) or someone else's at that path is released
  and mounted again.
- Scan: an image still being written (stability) is neither probed nor attached; its title id is that of a
  cache whose stamp source is in its mount point. An image whose title id is already held by a source it
  cannot beat (claim's rule; its packing read from the probe: `ampr_zstd.index` / `ampr_assets.index` in the
  game folder) is a duplicate without being attached (`Source.path` = where its folder would be mounted).
- Release (`image::release_unused`, every pass after the scan, before the games are processed; a mount under a
  running title's `/system_ex/app/<TID>` stack, or holding such an inner image, stays: a running homebrew's title
  is never touched): every
  mount of ours not used by a game (its image mount and the mount of its folder), non-image duplicate, unstable image
  or scan error of this pass, nor holding such a mount, is released by `image::release_tree`: the images
  mounted from inside it first, then the `/system_ex/app/<TID>` stacks on it (`register::unmount_title`,
  never MNT_FORCE), then unmount (never MNT_FORCE), LVD detach (`0xC0286D01`), state file and dir removed.
  The unit detached is the live mount's `/dev/lvdN`, or the recorded one for a dead mount (still in the mount
  table, so attached this boot). Nothing mounted there: the record may predate a reboot (its unit now another
  image's or the system's), so it is dropped without a detach.
  Busy = logged, retried next pass. So a moved image's cache sees its old folder gone and relinks (`Fresh::Moved`),
  and an image that loses its title to a later source is released.
- Eject: the image mounts whose image is on the drive are released (`release_tree`) before the IPMI eject.
- `/app0` of an image game is read-only (writes fail with EROFS), as in SMP's default `read_only`.
  `fix_access` skips the image tree (chmod cannot succeed there). Web: `"image"` in a game's JSON, the
  badge is the image's extension (`EXFAT`, `FFPKG`, `FFPFS`, `FFPFSC`), its pills start on the card's second
  row. The library's Images chip (before Duplicates) lists the image games (every chip but All shows only
  while it has games); the other kind chips leave them out. The game sheet's Path, Hide and Delete name the image file.
  A gameless or unreadable image is a scan error (log only). The game size in `/api/sizes` is the image size.
- Hide, `games/uninstall` and `/api/delete` go by `Source::origin` (the image file, not its mount folder):
  `exclude` gets the image path; Delete releases its mounts (`release_tree`, busy = 500, the file stays),
  then deletes the file.
- ponytail: no MD backend, no per-image sector override, no PFS GDDR5 metadata-cache ioctl (SMP's
  `nested_pfs_index_cache`, off by default there too), one game per `.ffpfsc`, a unit whose detach failed is
  not retried once its mount is gone, and an image replaced in place while mounted keeps serving
  the old file until it is released (unplug, move, or a payload restart after a reboot).

## Paths
| Path | What |
|---|---|
| `/data/krakenmount/config.toml` | settings (created with defaults on first start) |
| `/data/krakenmount/caches/<TID>/` | per-game cache |
| `/data/krakenmount/ampr.bundled` | `<version> <len> <crc32>` of the `libSceAmpr.sprx` we last wrote, bundled or downloaded (v0.4.0 builds wrote `<len> <crc32>`: ours, version unknown; outside `ampr/`, whose files all go into fakelib) |
| `/data/krakenmount/ampr/` | ampr-emu-ng files (`libSceAmpr.sprx`, ...), copied on top of every game's `fakelib/` in its cache (folder: `inject_ampr`; installed: `ampr_installed`, per title `Settings::ng_installed`) (chmod 0777 at start; a `libkernel.sprx` there is never copied and logs a warning) |
| `/data/krakenmount/kraken.log` | log (setting `log`); each line starts with the UTC time and the milliseconds since boot (`2026-11-19 20:51:03Z [10643021] `); starts over past 1 MiB, no kept copy |
| `/data/krakenmount/kraken-mount.elf` | self copy the tile starts (Dump Forge style, below) |
| `/data/krakenmount/installing/<TID>` | empty marker of a base install handed to the console installer and not reported done yet (Installed games) |

## Drives and scan roots (`src/storage.rs`)
Drives: `/data` (label "Internal") always, plus each of `/mnt/usb0..7` ("USB n+1") and
`/mnt/ext0..1` ("Extended n") that is a mount point (statfs `f_mntonname == path`).
Default scan roots: SMP `SM_DEFAULT_SCAN_PATHS_INITIALIZER` order, only for drives present
(`/data/homebrew`, `/data/etaHEN/games`, ext `homebrew`/`etaHEN/games`, usb `homebrew`, usb `etaHEN/games`,
usb roots, ext roots). `scanpath=` lines in config replace them.
A dir named `backports` in a scan root is never a game.
Depth (`scan::candidates`, SMP scan_depth=2): a root's entries are scanned, and also the entries of each subfolder that is
not a game folder, not `backports` (any case: exFAT), not a dot entry and not itself a scan root (one level only, dot
entries and `backports` skipped). A drive root (`/mnt/<x>`) is never descended: its `homebrew` and `etaHEN/games` are
roots of their own. Every root's own entries come first, in root order, then all nested ones; a nested source never
beats a title already held (`claim`), so an old copy in a subfolder cannot take over. A path already listed in this
pass is not listed again.
Installed games (`installed::list`, once per pass) are claimed before any root entry (rank 0, see Flow): a source
`{path: /user/app/<TID> or /mnt/extN/user/app/<TID>, kind: Installed, size, drive}` whose backport is
looked up in `<root>/backports/<TID>*` of the first scan root (or /data/homebrew), then the other roots and
/data/homebrew (`backport_for`). A title with a marker claims nothing as
installed (its FPKG keeps winning), except the title of the base install running now (`scan`'s `installing`, from
`install::busy_tid`): that id is reserved at rank 0 with no source, so its FPKG is `duplicate` and not registered
again mid-install.
Patch and DLC packages: every stable `.pkg` candidate, cached or not, is classified first (`pkg_class`, SMP
`parse_package`, memoized per size+mtime; I/O errors not memoized): `cnt_offset` reads the outer header
(`\x7FFIH` v3: LE u64 at 0x58; `\x7FLIH` v1: LE u64 at 0x30, a delta; `\x7FCNT`: 0), `classify` the CNT header
(content id at 0x40, type BE u32 at 0x74, flags at 0x78, container type at 4): type 0x1B/0x1C/0x21 = DLC; a delta,
category `gp`/`gpd`/`ps5dp` (param.sfo CATEGORY) or the SMP patch flag mask (minus the media-remaster exception)
= patch; else base. The flag mask gives patch only when the CATEGORY is not a base one (`gd`/`gde`/`gdc`/`gdp`): a
base category with patch flags is a base game (SMP marks that kind not confident and skips it; here it stays the FPKG
path, as on main). Delta and category `gp`/`gpd`/`ps5dp` still give patch. A patch or DLC goes to `ScanResult.packages` (never `errors`, never a Library game, a patch cached as
a game by an older build included: its old cache and registration stay, since auto-remove needs the source gone; same
candidate rules, `exclude` and stability wait); title id = content id chars 7..16; name/version from the pkg's
param.json, else sfo TITLE/APP_VER, else the title id and "". A base or unclassified `.pkg` goes the FPKG path as
before. ponytail: SMP's confidence check (known type set, category agreeing with the kind) is not ported.
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

## Settings (`src/config.rs`, file `/data/krakenmount/config.toml`)
A flat TOML subset: `key = value` with strings, `true`/`false`, integers and arrays of strings; `#` comments.
| Key | Default | Meaning |
|---|---|---|
| `scanpath` | (defaults above) | array of paths; replaces the default scan roots |
| `exclude` | [] | array of absolute paths (trailing `/` trimmed, duplicates dropped); a source at an entry or under it (whole path components, `scan::excluded`) is skipped by the scan before duplicate detection, never listed or descended into (an excluded root is not opened): not in games, duplicates, errors or unstable, never built or registered. An excluded title that is still registered stays registered (as SMP removing a manual source). Edited by `/api/v1/manual/{add,remove}` and `/api/v1/games/uninstall` |
| `inject_ampr` | true | copy `/data/krakenmount/ampr/*` on top of a folder game's fakelib; pkg games always get it (pkg mode exists only in ampr-emu-ng) (a change only refreshes `fakelib/`) |
| `ampr_installed` | false | installed games run through ampr-emu-ng (per title: `Settings::ng_installed`, the title's `ampr_installed_on`/`ampr_installed_off` entry, else this). An internal debug FPKG install (`/user/app/<TID>/app.pkg` whose first 8 bytes read as a debug header, `installed::debug_of`; a pass that cannot read them keeps the kind of the cache's stamp, so one bad read never flips the title and rebuilds it; `install::enqueue` refuses its `app.pkg` (409, already installed)) is then a pkg source (`scan::installed_pkg`, see Installed games): ng's pkg mode from its `app.pkg`, our cache mounted at `/system_ex/app/<TID>`, the console's registration kept; a switch rebuilds its cache (stamp `kind` changes). A retail or extended-storage install stays native and gets `/data/krakenmount/ampr/*` on top of its fakelib; off: none copied and the system's `libSceAmpr.sprx` runs: the backport's is dropped from the cache `fakelib/` (ampr state `"drop"`, SMP removes it from an installed PPR package's backport fakelib), and the install's own copy in `/app0` is hidden at launch by an NSFS redirect (see Launch). `inject_ampr` stays folder-only, a Kraken-mounted pkg always gets ng (a change only refreshes `fakelib/`) |
| `ampr_installed_on`, `ampr_installed_off` | [] | arrays of title ids that override `ampr_installed` for one installed game (on wins when listed in both). Per game from `POST /api/ng/<TID>`: the global value clears the title's entry |
| `install_delete_source` | false | delete the source `.pkg` once the console reports its install done (the checks of `/api/delete`'s source delete: inside a scan root, no symlink, a `.pkg`); the Install screen's checkbox and a Settings switch |
| `add_emulators` | true | add the bundled DLC (dlc_emu) and PlayGo (pgo_stub) emulators to a game's fakelib when it needs them and ships none (a change only refreshes `fakelib/`) |
| `playgo_off` | [] | array of title ids (`playgo_off = ["PPSA01556"]`) that get no bundled PlayGo stub: pgo_stub is neither added nor upgraded, a `libScePlayGo.sprx` the game or backport ships stays as shipped; the dlc_emu set is unaffected. Per game from the UI (`POST /api/playgo/<TID>`); a change only refreshes `fakelib/` |
| `overlay_off` | [] | array of title ids whose cache gets an empty `fakelib/`, so launch mounts no overlay: not the game's or backport's fakelib, not ampr-emu-ng, not the emulators; like any empty `fakelib/`, the launch watcher then mounts nothing in its sandbox (no `/data`, USB or writable `/app0` either): the title runs as registered; an installed title's backport redirects into `/app0` still apply (see Launch). Folder and installed games only: a Kraken-mounted pkg needs ampr-emu-ng's pkg mode, so the web refuses it for a pkg (409) and a pkg source of a listed title keeps its fakelib. Per game from `POST /api/overlay/<TID>` or `/api/v1/games/fakelib`; a change only refreshes `fakelib/` |
| `ampr_off` | [] | array of title ids of plain folder games that get no ampr-emu-ng even with `inject_ampr` (a game that dies right after the ampr-emu-ng notice); unlike `overlay_off`, the game's own `fakelib/` and the emulators stay. zstd and LZ4 folders (their packs need ampr-emu-ng) and pkg games always get it, listed or not (`Settings::ng_folder`). Per game from the game sheet's ampr-emu-ng switch (`POST /api/ng/<TID>`); a change only refreshes `fakelib/` |
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
| `ui_theme` | "dark" | web UI theme: `dark` or `light` |
| `ui_scheme` | "abyss" | web UI color scheme: `abyss`, `ember`, `moss`, `cobalt`, `dusk`, `rose`, `sand`, `volt` or `mono` (the last six ported from ps5-dump-forge); Settings shows each as a dot of its accent |
| `ui_sidebar_collapsed` | false | the web UI sidebar shows icons only |
| `ui_lang` | "" | web UI language: `""` = the browser's, or `en`, `ru`, `zh`, `de`, `es`, `fr`, `ja`, `pt`, `it`, `ko`, `ar` |

Invalid values are logged and replaced by the default; a string item of an array (`scanpath`, `exclude`, `playgo_off`, `overlay_off`) that is not
a valid value is logged and skipped, the other items stay (an array that does not parse, such as one with a number in it,
is ignored as a whole). The web UI edits the same file. The `ui_*` keys only steer the web UI: it reads them
from `GET /api/settings` at boot (they are not injected into `GET /`, since the AppCache keeps that page
until the manifest changes). Toasts follow the console language, not `ui_lang`.

## Cache (`src/cache.rs`)
`/data/krakenmount/caches/<TID>/`, stamp file `.done` written last.
- Folder: `fakelib/` only. `/app0` is the game folder itself.
- Installed: `fakelib/` only, composed as for a folder (backport `fakelib2/`/`fakelib/`, then the emulators with
  `add_emulators`/`playgo_off`, then the ampr files when `ampr_installed` for the title); `overlay_off` works per title. Stamp
  `"kind": "installed"`, `source` = `/user/app/<TID>` or `/mnt/extN/user/app/<TID>`, `size` = the source's size.
  Inputs: the install's `app.pkg` (an ext copy without one: its dir) plus the backport lib dir. `app0` =
  `/system_ex/app/<TID>` (the system's mount, only read): eboot.bin and `dlc_emu.ini` lookups. PlayGo eboot: the
  backport's `eboot.bin`, else `/system_ex/app/<TID>/eboot.bin` when it is a file now, else unknown (no pgo_stub,
  not memoized, logged once per run per title). "Not mounted" means `/system_ex/app/<TID>/eboot.bin` is not a file
  (the install's own eboot, not the backport's). While it is not mounted, `compose_fakelib` takes the PlayGo result
  from the `.ampr` memo (`playgo[2]`) only when no eboot is found (a backport's eboot stays live), and `dlc_emu.ini`
  from the last recorded value: with a backport, `emus` also records `app0_dlc_ini` (the system mount's
  `dlc_emu.ini` alone), which falls back while the backport's stays live; without one, `emus.dlc_ini` falls back.
  This is the same fallback as the freshness state; both only while `emus.backport` (an
  installed title's backport path, absent without one) is unchanged, so a removed or swapped backport's results go. `fix_access` touches only the cache dir,
  never the install. A
  former pkg/folder cache of the title goes Stale (kind changed) and the rebuild removes its `.built` files (the
  installed copy is `/app0` now). The stamp's `backport` is also read at launch (`cache::stamp_backport`): its
  loose files are redirected into `/app0` by NSFS (see Launch), not copied; only its lib dir is a cache input.
- FPKG: it IS `/app0`: `sce_sys/**` (with the files only the pkg container holds: `save_data.png`,
  `pic0`/`pic1`, `snd0.at9`, `trophy00.ucp`, `nptitle.dat`, `npbind.dat`, which save data and
  trophies read from `/app0/sce_sys`), `eboot.bin`, every `*.prx`/`*.sprx`, every `*.elf` outside `decrypted/`
  (SceSysCore loads an exit-spawned ELF, which may sit in a subfolder, from disk, never through ng), and a root-level `dlc_emu.ini` / `playgo_stub.dat` (the emulators
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
  paths (a dir only once empty), so files the game wrote stay, then leaves `.built` empty (never deleted, so a
  failed or cut-off build is not taken for an older one). `.done` goes first, so a clear cut off midway is never
  fresh. A failed build removes what it wrote (nothing when clearing the old build failed). Only a missing
  `.built` (older build) empties the cache; an unreadable one fails the build, invalid UTF-8 is read lossily.
  A rebuild keeps `.ampr` even when `.built` lists it (`clear_build` skips it): `compose_fakelib` reads its playgo
  memo and the unmounted `emus` fallback, then rewrites it. Only Purge or an older cache without `.built` removes it.
  Only Purge deletes the game's files. Files read whole are capped (param.json 1 MiB, pkg or source images
  32 MiB): a corrupt size is an error, never an allocation that aborts the payload.
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
  A title in `playgo_off` skips pgo_stub entirely (no add, no upgrade, the eboot is not searched), logged
  `<TID>: fakelib: PlayGo stub off (playgo_off)`; `fakelib/` starts from scratch, so a stub added earlier is gone.
  A folder title in `overlay_off` stops right after `fakelib/` is emptied (no base, no emulators, no ampr), logged
  `<TID>: fakelib: overlay off (overlay_off)`; `.ampr` is written with `"overlay_off": true` and the earlier
  "playgo" memo kept. Launch then mounts no overlay (empty `fakelib/`).
  Then, with `inject_ampr` (folder; a plain one not for a title in `ampr_off`), with `ampr_installed` (installed) and always for a pkg game (the package's own `libSceAmpr.sprx` cannot read the
  `.pkg` named in `kraken.image`), every file of `/data/krakenmount/ampr/` (not `libkernel.sprx`) is copied on top,
  so ours wins over the game's own `libSceAmpr.sprx`, except a game's or backport's ampr-emu-ng strictly newer than
  ours (the `ampr-emu-ng X.Y.Z` identity in both files; ours without one counts as older), which is kept and logged
  `<TID>: fakelib: kept the game's ampr-emu-ng <v> (newer than <v>)` (no ng downgrade, also over a hand-placed copy). `.ampr` records what fakelib was composed from: `{"ampr": {name: [size, mtime]} or null,
  "emus": null without `add_emulators`, else the dlc_emu and pgo_stub versions, whether a `dlc_emu.ini` is on
  disk (folder or backport root), the eboot's [size, mtime] and "playgo_off" (the title is in `playgo_off`), "playgo": [eboot size, mtime secs, imports PlayGo]}`
  (a pkg's own `dlc_emu.ini` is not in it, a pkg change rebuilds through the stamp). "playgo" memoizes the PlayGo
  import search: `eboot.bin` is read again only when its (size, mtime) differs (a read error is not memoized), and
  `check` never compares it. An older `.ampr` without `emus` differs, so every cache refreshes `fakelib/` once; that
  one-time refresh also drops a `libSceSaveData.native.sprx` an older build copied (no separate check for such files).
  `"overlay_off": true` is in `.ampr` only for a title in `overlay_off` (an older `.ampr` stays fresh).
  When "ampr", "emus" or "overlay_off" differ (the ampr dir, `inject_ampr`, the emulator inputs or `overlay_off` changed) or `.done` still has a
  `game_fakelib` other than `full` (a cache built before the setting was removed), `check` says `Fakelib` and only `fakelib/` is
  recomposed (`refresh_fakelib`, which then rewrites `.done` with its own mtime), no extraction or re-registration. `fakelib/` is not in the AMPR index.
- A folder game's cache is only `fakelib/`, `.ampr`, `.built`, `.done`: nothing of the game is copied.
- Fresh when `.done` exists, its JSON stamp equals the expected one
  (`{"v":3,"kind","source","size","backport","fakelib"}`; extra keys of an older stamp are ignored), and every input's
  mtime is older than `.done`'s mtime. Inputs: FPKG = the .pkg, backport dir (recursive);
  folder = `eboot.bin`, `sce_sys/param.json`, its lib dir and files (not the folder itself: the game
  writes there). Otherwise rebuilt. A pkg's `.done` also carries `pkg_id` (FNV-1a 64 of the size,
  the first 64 KiB, the 4 KiB CNT header and the last 64 KiB), a folder's `folder_id` (FNV-1a 64 of
  each input file's path relative to the folder and size, plus the bytes of every input but
  `eboot.bin`: `param.json` and the lib dir's files); neither is
  compared by the freshness check.
- `"v"` is the cache layout. Another value is Stale (`cache layout v<old> -> v3`), checked before Moved,
  so every older cache rebuilds once, moved or not. v2: the container-only `sce_sys` files and the
  extraction of every file the index cannot map. v3: every `*.elf` outside `decrypted/` extracted too.
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
  a dir whose unmount fails is skipped and logged), empty and rebuild the rest. An installed title (`installed::copy_in`
  finds a copy, the `.done` kind is `installed`, or its source is under `/user/app`) is never `unmount_title`d: its `/system_ex/app/<TID>` is the
  system's and stays mounted; only our cache nullfs on top of it (an install run through ng) comes off first
  (`register::drop_cache_layer`, never forced; a failure keeps the cache), then the cache dir is deleted.

## Flow (`src/main.rs`)
Start: `umask(0)` first, so every dir/file the payload creates is 0777/0666 (as SMP, which mkdirs
0777) whatever elfldr passes; then create dirs, load config, take port 8097 (asks an older copy to quit every 250 ms for 5 s; it refuses while its scan pass runs), install the bundled
`libSceAmpr.sprx` when `ampr/libSceAmpr.sprx` is missing or matches `ampr.bundled` (an older bundled
copy; a file put there by hand is kept; see Auto-update below), ampr chmod, self copy thread, self-update check thread
(`update::spawn_check`, see Self-update below), web thread, klog thread when `debug`, then log the kstuff
KEKCALL_CHECK probe once (`sys::kstuff_probe`, SMP `sm_kstuff_is_loaded`: `kstuff probe: answered` or
`kstuff probe: no answer (rax=0x.., carry=..)`) and wait (2 s poll, one toast, `waiting` in `/api/state` for
the UI banner) until no `shadowmountplus.elf` process runs. Start-up never waits for kstuff: scan, register and
mount do not need it, and some kstuff builds run games without answering the probe; a launch is the proof
(see Launch). Then the launch watcher thread (started
again after a `kevent` failure, 2 s or up to 60 s while it keeps failing at once; it adopts every sandbox
with our layers or sandbox mounts, or a running title of ours, and republishes `running`. A game launched in
that gap is adopted without an overlay). Loop every `scan_interval_seconds` or on
wake: reload config, scan (sources in `exclude` skipped, see Settings; a source excluded while the pass runs is skipped before its build/register), then for each game: freshness check, build if stale or forced,
register (once per run, and after every rebuild), status `ready`. Right before the register (or the
mount-only path below), once per run per title, `cache::fix_access` opens up the cache and the
source for the game and the system services (it only adds missing bits); a relinked moved game
gets it right after `cache::relink`. A game whose cache is fresh and
which is already registered (`register::is_registered`) is not re-registered: only `register::mount`
redoes the missing app0 nullfs (gone after a reboot); for such a pkg the scan reads title id/name from
the cache's `sce_sys/param.json` and opens the .pkg only for its memoized class (`pkg_class`, once per run per
size+mtime). A fresh title registered by this run still
gets `register::mount` every scan (a no-op when app0 is mounted there; redoes it after a USB replug);
a title that leaves the scan is forgotten, so it goes through the full path when it is back. So is a title
uninstalled outside Kraken Mount (another tool's `sceAppInstUtilAppUnInstall`, the home screen's Delete): each pass
checks `register::staged` (two stats: `/user/app/<TID>/sce_sys/param.json`, `/user/appmeta/<TID>/param.json`) for a
title in the registered set, logs `<TID>: no longer on the home screen; registering again` when either is gone and
takes it out of the set. ponytail: an app.db row deleted with both files left is not seen.
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
An unstable `.pkg` whose memoized class (this run) is a patch or DLC reserves no title id from an older cache of that
path.
Auto-remove (`auto_remove_missing`, SMP auto_remove_missing_games): after each full scan pass (timers
start only after the first one, so a drive mounted late is not missing; not while paused), a built cache
(`.done`) whose stamp source is gone (ENOENT/ENOTDIR only) starts a timer in memory (a restart starts over).
Excluded, timer reset: a title this scan lists (moved, or still being copied), a running one, one with DLC in
`/user/addcont/<TID>` (a stat error there protects too). Missing for `auto_remove_delay_seconds`: unregister
as `/api/delete` (each title rechecks that no game runs; unmount without force, a busy mount retried next pass;
uninstall, then `register::forget` so a returning source registers in full: `is_registered` reads app.db as bytes); the cache stays for a
return; a failed uninstall is logged once and not retried until the source returns or a restart.
A title with an installed copy (`/user/app/<TID>/app.pkg` or `/mnt/ext{0,1}/user/app/<TID>`; a stat error other than
ENOENT/ENOTDIR counts as one), a stamp source under `/user/app` (a console install run through ng), or no staged `/user/app/<TID>/sce_sys/param.json` is never removed (`register::is_ours`,
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
Self-update (`src/update.rs`): always on, no setting, once per start, on its own thread: GET
`https://api.github.com/repos/quer3q/kraken-mount/releases/latest` (`main::get_retry`, 3 tries 20 s apart, max 1 MiB;
`main::parse_release` with asset `kraken-mount.elf`), logged `update: latest <tag> (running <v>)` or `update: <error>`
(no such asset is an error). The release is kept for `/api/state` `update` and `POST /api/self_update` only when its
`tag_name` is newer than `CARGO_PKG_VERSION` (`main::newer`) and the asset size is > 0.
Running game: while `launch::watch` reports a live session (`running`), the loop does no
scan/build/register/purge (a game starting mid-scan stops the scan before the next title);
rescan/rebuild/purge requests stay queued and run after the game exits (2 s poll).
A running homebrew (`scan::homebrew`: a valid title id not `CUSA*`/`PPSA*`, as SMP 1.7beta6) does not pause the loop
(`State::game_running` = `running` minus homebrew; `State::paused` stays any session): a launcher like Playden gets its
new games on the home screen while it runs. Its own title is skipped by `process` (no check, build, register or
remount; a forced rebuild waits), purge waits for `paused` (it would empty the mounted cache), and everything else
gated on `paused` (self-update, `/api/quit`, the install queue, web delete/hide/uninstall/eject, `/api/state` `paused`)
still waits. ponytail: the UI banner says paused while such a pass runs.
Toasts: `sceNotificationSend` system toast (dump-forge/SMP JSON, icon `/user/data/krakenmount/notify-icon.png`),
the old `sceKernelSendNotificationRequest` only as fallback. Toast texts (`src/i18n.rs`) follow the console language
(`sceSystemServiceParamGetInt` lang, read per toast; dump-forge's 11: en ru zh de es fr ja pt it ko ar), English when it
cannot be read; "Kraken Mount", title names and paths stay as they are. A failed game is retried
only when its inputs change (`inputs_key`: stamp, input mtimes, its `playgo_off` and `overlay_off` membership, its `ng_installed`) or a rebuild is forced. Duplicate title ids
(`scan::rank`, lower wins): installed 0, FPKG 1, zstd folder 2, LZ4 folder 3, plain folder 4; a source wins only
with a lower rank than the holder and when not nested; equal rank: the first scan root. Installed games claim first
and the installing title is reserved (see Drives and scan roots); the others show `duplicate`.
An installed debug FPKG run through ng (`scan::installed_pkg`: a `Kind::Pkg` source at `/user/app/<TID>/app.pkg`,
claimed at rank 0 like any installed game): built as a pkg (cache = `/app0`, `kraken.image` names `app.pkg`,
`fix_access` makes `app.pkg` +0444 and `/user/app/<TID>` +0555 for the sandbox), then only
`register::mount_over_install`: no sce_sys staging, no InstallAll, never `register::register`. The mount alone makes
ShellCore launch our `/app0` without the console's license check (`CheckPrepareProcessLaunchPkgApp`, which refuses
a debug FPKG install on a kstuff without fake-license support: `sceRifManagerEntitlementStatOnHDD` 0x80e90004,
launch 0x80e90017). ponytail: a console-installed patch (`/user/patch/<TID>`) is not read; the cache is the base
`app.pkg` only. Switched off again, the title is `Kind::Installed`: our nullfs comes off first (`register::drop_cache_layer`, before the build empties the cache, so a failed build never leaves it mounted), then the native rebuild. Uninstalled from the console's menu while run through ng: auto-remove only drops our layer (logged once), never an uninstall. No "moved" toast for such a source.
Installed titles (`Kind::Installed`): freshness check, build when stale (`refresh_fakelib` on `Fakelib`), never
`register::register`, `register::mount` or `remount`; status `ready`; added to the launch titles set. Once per run,
when the top mount at `/system_ex/app/<TID>` is Kraken's (a nullfs/unionfs whose source is under `caches/<TID>`,
`/data/homebrew/backports`, a scan root other than `/`, or the folder source or backport dir recorded in the title's
previous non-installed `.done` stamp (remembered just before the installed rebuild replaces it, per run only, so a
folder from a since-removed scan root is dropped too), and not under any `.../user/app/`), it is unmounted top layer first without MNT_FORCE until the top layer is not ours (max 4;
`drop_old_mount`; not `register::unmount_title`, which would also take the system's layer); a failure is logged with
the title id and path and retried next pass. A missing `/system_ex/app/<TID>` (statfs ENOENT/ENOTDIR) counts as
no stack of ours; a title processed as a non-installed source is taken off that once-per-run list, so a later
install of it gets its stack checked again. Nothing is uninstalled: the home-screen entry is the real one now. A
`.done` with kind `installed` never starts an auto-remove timer; Purge treats it as any cache, but never unmounts its `/system_ex/app/<TID>`
(see Cache, Purge). Low `/data` space (checked before the games, before and after each
build, after the scan): toast once per run and a UI banner.

## Register (`src/register.rs`)
SMP port: copy `sce_sys` to `/user/app/<TID>/sce_sys` + `icon0.png`, appmeta, trophy data,
nullfs `app0 -> /system_ex/app/<TID>` (app0 = cache dir for FPKG, game folder for Folder),
for a Folder with a backport a unionfs of the backport dir on top (SMP
`mount_backport_overlay`), then `sceAppInstUtilAppInstallTitleDir`. Where it does not resolve
(FW 12+) the title is only staged: InstallAll registers every folder under `/user/app`, so the scan
pass calls it once after its last game for every title staged in it (SMP install queue). Not
when the pass stopped for a game start: nothing registers while a game runs, and the titles staged
so far are not registered yet, so the pass after the game exits stages them again and installs them. Logged `<TIDs>: <dlsym error>; sceAppInstUtilAppInstallAll:
<rc> (<ms> ms)`; a failure marks each of those titles failed. A batch of more than one title
gets one toast (`<n> games registered` / `<n> games failed`) instead of one per title.
The pass defers InstallAll while a base install runs (`install::busy_tid()` is Some): InstallAll walks every
`/user/app` folder, also the one Sony is still writing. The staged titles stay out of `registered` and are staged
again next pass (as after a running game); a staged tile stays staged. Logged once per wait: `register:
InstallAll for [<TIDs>] waits for the install of <TID>; retried each pass`.
The tile (`KRKN00001`) is staged the same way on FW 12+ (`tile_staged`): the pass calls InstallAll
for it even with no game staged (not while a game runs), and `tile.ok` gets its hash once that succeeds
(a failed call leaves it `pending`, retried at the next start). The tile registers again whenever
`tile.ok` lacks its param.json hash, a tile file changed, or app.db's `tbl_contentinfo` has no `KRKN00001`
row (`SELECT COUNT(*)`; an unreadable app.db trusts `tile.ok`).
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
Title sound (`set_snd0info`, `sync_snd0info_if_changed`, SMP `update_snd0info`): for the titles with a
`/user/appmeta/<TID>/snd0.at9`, one `UPDATE tbl_contentinfo SET snd0info = '/user/appmeta/' || titleId || '/snd0.at9'`
whose `?1` is the comma-joined title ids (`instr` match, rows already pointing there are skipped), so any number of
titles takes one write transaction, in `/system_data/priv/mms/app.db`, through `ps5/appdb.c` (`km_sqlite_exec1`) and the
SQLite 3.46.1 amalgamation (opened read-write, never created, busy timeout 3 s, one mutex across threads). Run 200 ms
after a successful install call, 1500 ms after game launch, 200 ms after `NOTE_EXIT` and again after cleanup ends
(the system rewrites the path), plus once per scan pass via `sync_snd0info_if_changed` when `app.db` or `app.db-wal`
mtime/size changed since the last good sync (the registered titles minus games installed on the console, whose row
is the system's; a failed sync keeps the old stamp, so the next pass retries it); each runs on its own thread
(`spawn_snd0info`, the post-install one after the 200 ms), so a busy app.db holds neither the scan nor the watcher. Logged
`<TIDs>: snd0info set (<n> row(s))` only when a row changed; a failure is logged with the title ids, never fatal.

## Launch (`src/launch.rs`, `src/launch/nsfs.rs`)
As before (SMP sm_fakelib port): unionfs of `caches/<TID>/fakelib` (when non-empty) over the sandbox's
`<x>/common/lib` (the game's libs with ampr already copied on top, see Cache); one layer, no
mount of the ampr dir. Also nullfs of `/data` at `<sandbox>/data`, plus a nullfs of every mounted
`/mnt/usbN` / `/mnt/extN` at `<sandbox>/mnt/usbN` (pkg and packs on USB), and a read-only nullfs of
`/preinst/common/font` at `<sandbox>/preinst/common/font` when it exists (the console's fonts for homebrew text, SMP
`mount_sandbox_fonts`). Then, once the system's
`<sandbox>/app0` is mounted: when it is read-only, a read-write nullfs of `/system_ex/app/<TID>` (the
registered stack, backport unionfs included) over it; for a pkg game, a nullfs of the pkg's folder
(from the `.done` stamp's "source") at `<sandbox>/app0/kraken`; ng opens the .pkg named in `/app0/kraken.image`.
A single-file nullfs of the .pkg is refused by the kernel (ENOTDIR), hence the folder mount.
All removed on exit, newest first: the nullfs mounts at once (left in the sandbox they hang ShellCore's
teardown and the console powers off), the overlay once ShellCore takes it down with common/lib (checked
every 250 ms), at most 3 s later (`EXIT_GRACE_MS`), or when the sandbox dir goes. An exec of the same title
in the same sandbox meanwhile (an exit-spawned ELF; a new overlay would fail with EBUSY)
cancels that and mounts the nullfs mounts again (~10 ms after the exec: the forked child gets its title id
only at the exec, so they cannot go back earlier). When the exec fallback cannot mount the overlay, they are
still mounted. When the NOTE_EXIT watch cannot be added at the exec (or for a pid found in the adopted grace) and
the pid is already gone, the session's cleanup is scheduled at once (no NOTE_EXIT will come). A later copy of the payload (or a restarted watcher) adopts the overlay, `/data`, USB, font, writable `/app0`
and `/app0/kraken` mounts; the writable `/app0` (rw nullfs of `/system_ex/app/<TID>`) only for a title of ours
whose `.done` kind is not `installed`.
An adopted sandbox with no game process but with our `/data`, USB, font or `/app0` nullfs mounts (a game that exited
while the watcher was down) gets the exit cleanup after the exit grace (3 s; a game still starting that shows its title
id in that time is attached instead): those mounts hang the teardown, so the dir would never go. A sandbox with only
our overlay and no pid still waits for its dir to go.
kstuff check (no mount waits on it or is skipped by it): a launch of a fake-signed title (folder and pkg titles;
an installed one unless its app.pkg, from the `.done` stamp's "source", has a retail header,
`installed::retail_of`; ponytail: an ext install without app.pkg counts as fake-signed) is kstuff's proof. Its exec
means kstuff works: when `sys::kstuff_loaded` says no at that moment, logged once per run `kstuff probe got no
answer but <TID> started: this kstuff build does not answer KEKCALL_CHECK (rax=.., carry=..)`, and the probe is
not trusted for the rest of the run. When the sandbox goes, or 11 s (`PENDING_TIMEOUT_MS`) pass after it was seen,
without an exec and the probe does not answer: logged with the raw probe values and toasted (`notify`, read from
the settings file) `Kraken Mount: <TID> did not start: kstuff is not running` (`i18n::kstuff_missing`); only
logged when the probe was found wrong earlier in the run. Adopted sessions get no check.
When a session's cleanup ends, `register::spawn_snd0info(<TID>, 0)` (own thread).
No overlay when neither layer applies, and with an empty `fakelib/` (an `overlay_off` title) no sandbox mount at all
(an installed title still gets its backport redirects).
A title whose `.done` kind is `installed` (`cache::stamp_kind`) gets the fakelib overlay, the `/data`, USB and
font nullfs mounts and the backport redirects below only: no writable `/app0` (there is no `/system_ex/app/<TID>`
of ours), no `/app0/kraken`; `/app0` stays the system's mount, with the backport's loose files redirected into it.

Installed title with a backport (the `.done` stamp's `kind` is `installed` and it has a `backport`,
`cache::stamp_backport`; the `backports` setting off leaves it out): the backport's loose files go into the
sandbox's `/app0` through NSFS redirects (port of SMP 1.7 `sm_pkg_backport.c`). `/dev/nsfsctl`, ioctl
`0xC0406E00` create / `0xC0406E01` delete, 0x40-byte descriptors (scope unconditional, budget bigapp). The
plan (`nsfs::plan`) walks the backport against `<sandbox>/app0`: a regular file is a PLAIN redirect, a
directory app0 lacks (ENOENT) one OVERLAY redirect, a directory app0 has is walked into, a directory where
app0 has a non-directory fails (ENOTDIR); other entries (symlinks) are skipped. Only `fakelib`/`fakelib2` at
the backport's root are left out (they feed the common/lib overlay through the cache); dot files are
redirected. At most 256 redirects (E2BIG) and 64 levels (ELOOP); any failure creates nothing (a create that
fails rolls back every redirect made; one that cannot be deleted stays for the cleanup). No toast on failure,
logged with the title id, backport and app0 path.
When: Kraken has no ShellCore sandbox-ready hook. The redirects are made as soon as the sandbox's own
`/app0` is an nsfs mount (`f_fstypename` nsfs, `f_mntonname` the path) and it is the only nsfs `app0` among
`/mnt/sandbox/<TID>_<n>` (SMP `resolve_pkg_app0_path`; two: EBUSY): checked every 1 ms in the fast-path
wait (up to 3 s, also when the fakelib is empty), then every 10 ms until the exec, then once in the exec
pass (after the eboot.bin is loaded: too late for it, logged as "after exec"). Given up 11 s after the
sandbox was seen without an exec. The log line gives the count, `sandbox`/`poll`/`exec`, the ms since the
sandbox was seen and before/after exec. A missing backport or an app0 that never becomes nsfs is a quiet
skip (one log line at exec). The backport is read live at each launch: its loose files are no cache input.
Namespace identity: app0's `st_dev` + statfs `f_fsid` at creation. Cleanup (the session's `unmount_pass`,
after the exit or when the sandbox dir goes; never forced): the redirects are deleted newest first only while
that same namespace is still at app0 (ENOENT/EINVAL count as deleted); a namespace no longer in the mount
table released them; one still mounted elsewhere (covered) is EBUSY and retried like a busy unmount, then
kept. Other errors keep them, logged. An EBUSY delete is replaced by a later non-EBUSY one (SMP
`cleanup_session_locked`), so a session with one busy and one failed delete keeps its redirects at once instead of
retrying for 10 s. The records live in a static (`nsfs::ACTIVE`, keyed by sandbox dir),
so a restarted watcher's adopted session still releases them; no new redirects after a restart
(ponytail). A new payload process knows none: the sandbox teardown releases them with the namespace.
ampr_installed off (`cache::ng_dropped`: the `.ampr` state's `ampr` is `"drop"`), with or without a backport:
the same session also redirects every `libSceAmpr.sprx` (any case) the install has in `/app0/sce_module`,
`/app0/fakelib` or `/app0/fakelib2` to the system's `/system/common/lib/libSceAmpr.sprx` (PLAIN,
`nsfs::hide_ampr_plan`), and the backport's own `libSceAmpr.sprx` is not redirected; nothing when the system file
is missing. ponytail: a backport dir redirected whole (OVERLAY) keeps a `libSceAmpr.sprx` inside it. A backport
redirect still recorded (live or kept after a failed delete) blocks deleting that backport
(`launch::backport_in_use`, SMP `sm_pkg_backport_source_is_in_use`): `/api/backport/delete` answers 409 EBUSY.
A record counts as in use while its namespace is live or cannot be verified; one whose namespace is verified gone
(app0 no longer that nsfs and no nsfs with its fsid in the mount table) is dropped by the in-use check itself, so
a record kept by a failed cleanup or left by a sandbox that went while no watcher ran stops blocking the delete
once the namespace is gone (SMP `prepare_session_locked` reclaims such slots).
A failed mount-table read is never taken as a gone namespace (EIO, kept).

## Installed games (`src/installed.rs`)
A title is installed when `/user/app/<TID>/app.pkg`, `/mnt/ext0/user/app/<TID>` or `/mnt/ext1/user/app/<TID>`
exists (checked in that order, `copy_in`; only ENOENT/ENOTDIR counts as absent, any other stat error as present;
the same test `register::is_ours` uses) and there is no marker `/data/krakenmount/installing/<TID>`. The marker is
an empty file, written before a fresh base package goes to the console installer and removed only on reported
success. A marker left behind (failed, cancelled, payload killed) means not installed: the FPKG keeps winning and
mounting, and the UI shows "Install failed" with Reinstall.
Listing (`list`, once per scan pass): one `read_dir` of each of `/user/app`, `/mnt/ext0/user/app`,
`/mnt/ext1/user/app`. Skipped: ids failing `scan::valid_tid`, `KRKN00001` (the tile), titles with a marker, and
`/user/app` entries whose `app.pkg` stats ENOENT/ENOTDIR (Kraken's staging, an ext install's stub; the rule of
`copy_in`); any other stat error lists the title with size 0 and logs it. The first base wins for a title
id. Name and version from `/user/appmeta/<TID>/param.json` (`parse_installed` via `scan::parse_param`); only games:
`applicationCategoryType` 0 or missing (apps, 65536 ... skipped); a missing or unparsable param.json still lists
the title (rank 0, so an FPKG of it never registers or mounts over the install), with the title id as its name and
no version, logged once per run per title (ponytail: a non-game app without a readable appmeta is listed as a game). Size: `app.pkg`'s length; an ext copy without `app.pkg`: its du (walked every pass,
ponytail).
Retail badge (`retail_header`): the first 8 bytes of `app.pkg` (ext copy: `<dir>/app.pkg` if present, else not
retail) start with `\x7FFIH` and byte 5 (the signed byte) is not 0x00; debug FPKGs carry 0x00. Unreadable or
another magic: not retail.
Installed titles are never deleted, hidden or uninstalled by Kraken Mount (the console's menu does that):
`/api/delete`, `/api/hide` and `games/uninstall` answer 409 for them; Launch and the backport delete stay allowed.

## Install (`src/install.rs`, `ps5/installer.c`)
What: a library FPKG game (`game`; `update` when the title is already installed: the `.pkg` duplicate of an
installed title), or a patch/DLC package of `ScanResult.packages`. A patch needs its game installed; a DLC installs
regardless. One job thread, started at the first `enqueue`, lives for the run; the queue is in memory (a restart
forgets it; an update, patch or DLC Sony already accepted continues and shows as installed once Sony finishes; a
fresh base game keeps its marker, so it shows "Install failed" until Reinstall clears it, see the marker ponytail below).
Per job: wait while a game runs (phase `wait`); free space `statfs("/user")` available >= size + 10%, else
`not enough space on the internal SSD (<need> GB, <free> GB free)`. A fresh base game (`game`): `busy_tid` = its
title id (the scan reserves it), then under `State::work`: the marker, then, when `register::is_registered` and
`register::is_ours` is Ok (checked first: the console's own copy from an earlier failed install is never unmounted),
`register::unmount_title` (never MNT_FORCE; failure = error) and `register::uninstall` + `register::forget` (failure = error), so the real install gets a clean
title id. On such a failure, or a cancel before the submit, the marker is removed only when this run wrote it: a
marker of an earlier failed install stays, so the title keeps "Install failed" and Reinstall. An `update`, patch or DLC writes no marker (it would make the installed title count as not installed).
Serve: `http://127.0.0.1:8097/pkg/<16 hex token>/<SMP-encoded file name>` (`game.pkg` when the URL would pass 127
bytes, Sony's MetaInfo limit). `web::handle` hands a loopback `GET|HEAD /pkg/...` to `install::serve_conn`:
keep-alive, 200/206/416, `Content-Length`, `Accept-Ranges`, a fixed past `Last-Modified`; a wrong token or method,
or no running job, is 404. The file is opened once per connection; its (dev, ino, size, mtime) is checked at open
and every 5 s: a replacement fails the job `<path> changed while it was being installed`.
Submit (`submit`, installer.c `km_pkg_install`, a port of dump-forge `forge_pkg_install` with the SMP ABI and its
`_Static_assert`s): save authid/caps/attrs; the authid is swapped only when the saved one reads nonzero (else the
note says `can't read the authid (not swapped)` and the ShellCore retry is skipped); SYSTEM authid 0x4801000000000013, caps 0xff x16, attrs[0] 0x80;
`sceAppInstUtilInitialize` once (never terminated; a failed one is retried by the next call; `km_pkg_status` and
`km_addcont_installed` also run it, under the lock with the credentials swapped, when no install has initialized it yet);
`sceAppInstUtilInstallByPackage`; on 0x80B22404 retried as ShellCore 0x3800000000000010 (`info` and `playgo` zeroed again first); creds restored (authid
retried 3x). The native codes and credential failures come back as a note, logged `install <name>: <url>: <note>`.
Status, addcont status and cancel run under the same swap. ponytail: the swap is process-wide (`getpid()`): while
it is held (every 500 ms status poll, every DLC check), the web, scan and launch-watcher threads make their
syscalls (nmount, unmount, app.db writes) with the SYSTEM authid, all caps and attrs 0x80; not seen to matter. One pthread mutex covers every sceAppInstUtil call of
the payload: installer.c takes it itself, register.rs through `install::AppInst::lock()` around its Initialize,
AppUnInstall, AppInstallTitleDir and AppInstallAll (not reentrant; an InstallAll holds status polls for its time).
Progress: bytes served / `.pkg` size (phase `download`), then `sceAppInstUtilGet[Patch]InstallStatus` every 500 ms
(phase `install`). Done (SMP rules): an error code or status `error` fails, an empty status fails (`no status`);
`none` = a DLC with its installed record, or a patch after a full download seen by a poll or every byte served (a
small patch can retire between polls), still only with its base playable/completed and its installed
`app.pkg`/`patch.pkg` stamp changed, else wait (30 s start timeout); a status rc before the first successful
status poll also waits (the 30 s start timeout, error `the installer showed no task for <cid> in 30 s (status rc
0x...)`), after it a status rc fails the job; a task 0 for a non-fresh kind
waits until the shared terminal record changes; `completed`, or `playable` with downloaded >= total (no native
total: every byte served, base only); a DLC only once `addcont_installed` (rc 0x80E90004 = not yet). No request,
byte served, or change of the native status/downloaded for 900 s fails the job (`the installer asked for no
byte and showed no progress in 900 s`).
Success: marker removed (base), the source deleted when `install_delete_source` is on, scan woken. Cancel or
failure: serving stops, `cancel_native` (sceAppInstUtilCancelInstall) only for a fresh base install with no console copy (task type 0,
kind `game`: the call addresses the base application, SMP), the marker stays, the result (`cancelled` or the error
text) is kept per source path: the worker records it and clears `current` under the jobs lock as soon as the job
returns, so a cancel after that finds nothing running. ponytail: a pipelined request's bytes after the first head are lost; a patch's
result is a changed stamp, without SMP's parse of the new file's content id and version; no copy-only check; the
token falls back to the clock and pid when `/dev/urandom` is unreadable; no connection cap. The job kind is checked again
when the job starts (a second copy queued behind a fresh install of its title runs as `update`). A base install over
a marker whose console copy exists (`installed::copy_in`) counts as a reinstall for the status rules: the pre-submit
snapshot is taken, an unchanged terminal record is not read as done, and a failure or cancel does not call
`sceAppInstUtilCancelInstall`. "Fresh" (marker, Busy, prepare_base) stays "kind game and not `is_installed`". `/api/quit` and `/api/self_update` refuse (409) while an install runs or is queued
(`install::active`). ponytail: a marker left by a crash mid-install is never cleared on its own (Reinstall clears it).
Installing a debug FPKG needs kstuff, fpkg-enable and ppr-patch on the console; the installer's error codes are
shown with dump-forge's hint texts.

## Crash report (`src/crash.rs`)
When an app crashes, the console writes `/user/devlog/system/sce_coredumps.0/<TID>_<n>/` (also seen as
`/devlog/...`) with `prosperocore-<n>-0x<pid>-<process>.prosperodmp` inside, and deletes the folder
seconds later. No thread or watch of its own: the launch watcher's NOTE_EXIT for a game we mounted (an
exit-spawned ELF included) logs the exit status from the event's data on the `exit pid=...` line
(`exited with N` or `killed by signal N`), then reads that folder once for `<TID>_*` folders holding a
dump with that pid. If none is there yet, it looks once more when that session's cleanup ends (0.4 to
3 s later), and logs `pid=... killed by signal N; no crash dump ...` when a killed game left none. A
match logs `<TID>: crashed (<process>, <status>), dump found at exit|<ms> ms after exit; the console's
crash dump: <folder path>` and is kept as the title's last crash in `/data/krakenmount/crashes.json`
(`{"<TID>": {at, process, folder, status}}`, temp file + rename), so it survives a payload restart or a
reboot. Not in the game's cache: for a pkg the cache is /app0, so the game would see the file.

## Module interfaces (contract between modules; keep these names)
```rust
// config.rs
pub const CONFIG_PATH: &str = "/data/krakenmount/config.toml";
#[derive(Clone, PartialEq, Debug)]
pub struct Settings { pub scan_paths: Vec<String>, pub exclude: Vec<String>, pub inject_ampr: bool,
  pub ampr_installed: bool, pub ampr_installed_on: Vec<String>, pub ampr_installed_off: Vec<String>, pub install_delete_source: bool, pub add_emulators: bool,
  pub playgo_off: Vec<String>, pub overlay_off: Vec<String>, pub ampr_auto_update: bool,
  pub backports: bool, pub scan_interval_secs: u64, pub stability_wait_secs: u64, pub low_space_gb: u64,
  pub auto_remove_missing: bool, pub auto_remove_delay_secs: u64, pub notify: bool, pub log: bool, pub debug: bool,
  pub ui_theme: String, pub ui_scheme: String, pub ui_sidebar_collapsed: bool, pub ui_lang: String }
pub const UI_THEMES: &[&str]; pub const UI_SCHEMES: &[&str]; pub const UI_LANGS: &[&str]; // the ui.html LANGS codes
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum Kind { Pkg, Folder(Packing), Installed }
pub fn rank(kind: Kind) -> u8;                              // duplicates, lower wins: Installed 0, Pkg 1, Zstd 2, Lz4 3, Plain 4
#[derive(Clone, Debug)]
pub struct Source { pub path: std::path::PathBuf, pub kind: Kind, pub title_id: String, pub title_name: String,
  pub version: String, /* param.json contentVersion, or empty */
  pub drive: String, pub size: u64 /* pkg file size; folder: 0; installed: app.pkg or the ext dir's du */, pub backport: Option<std::path::PathBuf> }
#[derive(Clone, Copy, PartialEq, Eq, Debug)] pub enum PkgKind { Base, Patch, Dlc }
pub struct Package { pub path: std::path::PathBuf, pub kind: PkgKind, pub content_id: String, pub title_id: String,
  pub title_name: String, pub version: String, pub size: u64, pub drive: String } // a patch or DLC .pkg of a scan root
pub struct ScanResult { pub games: Vec<Source>, pub duplicates: Vec<Source>, pub errors: Vec<(std::path::PathBuf, String)>,
  pub unstable: Vec<(std::path::PathBuf, Option<String> /* title id when known */)>, // still being copied: skipped, not gone
  pub packages: Vec<Package> }
pub fn scan(roots: &[String], exclude: &[String], backports: bool, stability_wait: u64,
  installing: Option<&str> /* install::busy_tid: reserved at rank 0 */,
  ng_installed: &dyn Fn(&str) -> bool /* Settings::ng_installed: a debug FPKG install becomes a pkg source */) -> ScanResult;
pub fn installed_pkg(src: &Source) -> bool;                 // Kind::Pkg at /user/app/<TID>/app.pkg (an install run through ng)  // memoizes pkg param reads by (path, size, mtime)
pub fn cnt_offset(outer: &[u8]) -> Option<(u64, bool /* delta (LIH) */)>; // CNT header offset from the outer header
pub fn classify(cnt: &[u8], delta: bool, category: &str) -> Option<(PkgKind, String /* content id */)>;
pub fn excluded(path: &std::path::Path, exclude: &[String]) -> bool; // path is an exclude entry or under one (whole components)
pub fn still_written(paths: &[std::path::PathBuf], wait: u64) -> Option<u64>; // age in s of the youngest max(mtime, ctime) under wait; None = stable
pub fn read_source(path: &std::path::Path) -> Option<Source>; // one path read as the scan does (pkg memo), backport None; None = not a readable game or a patch/DLC .pkg (the hidden list)
pub fn parse_param(json: &[u8]) -> std::io::Result<(String, String, String)>; // (title_id, title_name, version); name: see below
pub fn valid_tid(t: &str) -> bool;
pub fn homebrew(t: &str) -> bool;                          // valid_tid and not CUSA/PPSA: running, it does not pause the scan loop

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
pub fn app0(src: &Source) -> std::path::PathBuf;                    // pkg: dir(tid); folder: src.path; installed: /system_ex/app/<TID>
pub fn sce_sys(src: &Source) -> std::path::PathBuf;                 // where sce_sys lives for this source
pub fn built_at(tid: &str) -> Option<u64>;                          // .done mtime, unix secs
pub fn size(tid: &str) -> u64;                                      // bytes, recursive
pub fn du(path: &std::path::Path) -> u64;                           // file bytes, recursive; no symlink traversal
pub fn total_size() -> u64;
pub fn purge(keep: &[String]) -> std::io::Result<()>;               // delete caches/<TID> not in keep; empty the kept ones
pub fn fix_access(src: &Source);                                    // add missing read/exec bits, see Cache "Access"
pub fn stamp_kind(tid: &str) -> Option<String>;                     // the .done "kind" (pkg|folder|installed); None without a cache
pub fn stamp_backport(tid: &str) -> Option<String>;                 // the .done "backport"; None without a cache or backport
pub fn ng_dropped(tid: &str) -> bool;                               // .ampr "ampr" is "drop": installed, ampr_installed off

// register.rs
pub fn register(title_id: &str, app0: &std::path::Path, sce_sys: &std::path::Path, backport: Option<&std::path::Path>) -> std::io::Result<bool>; // false: staged, waits for the pass's install_all
pub fn register_tile(port: u16);
pub fn tile_staged() -> bool;                             // FW 12+: the tile waits for the pass's install_all
pub fn is_registered(title_id: &str) -> bool;
pub fn staged(title_id: &str) -> bool;                     // both staged param.json files exist (two stats; is_registered's first check)
pub fn registered(tids: &[&str]) -> Vec<bool>;            // is_registered for many, in tids order, one app.db read (all false when unreadable)
pub fn set_snd0info(title_id: &str);                      // app.db snd0info -> /user/appmeta/<TID>/snd0.at9; logs, never fails
pub fn spawn_snd0info(title_id: &str, delay_ms: u64);    // set_snd0info on its own thread after delay_ms
pub fn sync_snd0info_if_changed(tids: &std::collections::HashSet<String>); // one UPDATE for the non-installed titles when app.db/wal mtime or size changed
pub fn mount(title_id: &str, app0: &std::path::Path, backport: Option<&std::path::Path>) -> std::io::Result<()>;
pub fn drop_cache_layer(title_id: &str) -> std::io::Result<()>; // our cache nullfs off the top of /system_ex/app/<TID>, never forced (Purge of an installed title)
pub fn mount_over_install(title_id: &str, app0: &std::path::Path) -> std::io::Result<()>; // scan::installed_pkg: our nullfs; on top of a layer of the system's (never unmounted), a stale one of ours replaced; an unreadable statfs refused
pub fn remount(title_id: &str, app0: &std::path::Path, backport: Option<&std::path::Path>) -> std::io::Result<()>; // moved folder: whole stack
pub fn drop_backport(title_id: &str, app0: &std::path::Path) -> std::io::Result<()>; // remount(app0, None) when /system_ex/app/<TID> is mounted, else Ok; never MNT_FORCE
pub fn unmount_title(tid: &str) -> std::io::Result<()>;   // every layer at /system_ex/app/<TID>, never MNT_FORCE
pub fn uninstall(tid: &str) -> std::io::Result<()>;       // sceAppInstUtilAppUnInstall
pub fn forget(tid: &str);                                 // after an auto-remove: drop the staged sce_sys/param.json so is_registered is false
pub fn is_ours(title_id: &str) -> Result<(), std::path::PathBuf>; // Err(path) = an installed copy (installed::copy_in), or no staged param.json: never auto-remove

// installed.rs (see Installed games; *_in(root, ..) variants for tests)
pub const MARKERS: &str = "/data/krakenmount/installing";
#[derive(Debug, Clone)]
pub struct Game { pub title_id: String, pub title_name: String, pub version: String,
  pub path: std::path::PathBuf /* /user/app/<TID> or /mnt/extN/user/app/<TID> */, pub size: u64, pub retail: bool }
pub fn list() -> Vec<Game>;                                         // list_in(root)
pub fn copy_in(root: &std::path::Path, tid: &str) -> Option<std::path::PathBuf>; // first present copy (internal: its app.pkg)
pub fn is_installed(tid: &str) -> bool;                             // a copy and no marker; installed_in(root, tid)
pub fn mark(tid: &str) -> std::io::Result<()>;
pub fn unmark(tid: &str);                                           // logs any error but NotFound
pub fn marked(tid: &str) -> bool;                                   // marker_in(root, tid) -> PathBuf
pub fn retail_header(head: &[u8]) -> bool;                          // \x7FFIH and byte 5 != 0
pub fn parse_installed(json: &[u8]) -> std::io::Result<Option<(String, String)>>; // (name, version); None: not a game

// install.rs (see Install)
pub struct AppInst(());  impl AppInst { pub fn lock() -> AppInst; } // the payload's sceAppInstUtil lock (RAII); not reentrant
pub struct Native { pub status: String, pub downloaded: u64, pub total: u64, pub error: i32 }
pub fn submit(url: &str, name: &str) -> Result<(String /* content id */, u8 /* task type */), u32>;
pub fn status(content_id: &str, patch: bool) -> Result<Native, i32>;
pub fn addcont_installed(content_id: &str) -> Result<bool, i32>;    // state 4
pub fn cancel_native(content_id: &str) -> i32;                      // fresh base installs only
pub enum Item { Game(scan::Source), Pkg(scan::Package) }
#[derive(Clone, Debug)]
pub struct Job { pub id: String /* source path */, pub title_id: String, pub name: String,
  pub kind: &'static str /* game|update|patch|dlc */, pub done_bytes: u64, pub total_bytes: u64,
  pub phase: &'static str /* wait|download|install */, pub error: Option<String> /* reserved: always None, errors are in results */ }
pub fn init(state: Arc<State>);                                     // main, after State::new; starts nothing
pub fn enqueue(item: Item) -> Result<(), (u16, String)>;            // the job thread starts at the first call
pub fn cancel() -> bool;                                            // false when none runs
pub fn snapshot() -> (Option<Job>, Vec<Job>, Vec<(String, String)> /* last result per id */);
pub fn busy_tid() -> Option<String>;                                // the running fresh base install's title id
pub fn active() -> bool;                                            // an install runs or is queued
pub fn appinst_busy() -> bool;                                      // a long sceAppInstUtil call (AppInst holder or submit) runs or waits
pub fn serve_conn(conn: std::net::TcpStream, head: &str);           // a loopback GET|HEAD /pkg/... from web.rs
// ps5/installer.c: km_appinst_begin/end, km_pkg_install, km_pkg_status, km_addcont_installed, km_pkg_cancel

// main.rs (shared version compare, used by cache.rs for ampr-emu-ng identities)
pub(crate) fn parse_version(v: &str) -> Option<(u32, u32, u32)>; // "X.Y.Z", leading "v" allowed
pub(crate) fn newer(a: Option<&str>, b: Option<&str>) -> bool;   // a strictly newer than b; an unknown version is older than any
// main.rs (GitHub release helpers, shared by the ampr-emu-ng auto-update and update.rs)
pub(crate) struct Release { pub(crate) tag: String, pub(crate) url: String, pub(crate) size: usize }
pub(crate) fn parse_release(body: &[u8], asset: &str) -> Result<Release, String>; // tag_name + that asset of releases/latest
pub(crate) fn get_retry(prefix: &str, url: &str, max: usize, attempts: u32) -> Result<Vec<u8>, String>; // 20 s apart, failures logged after prefix
pub(crate) fn is_module(b: &[u8]) -> bool;                         // ELF or SELF magic

// update.rs (self-update, see Flow "Self-update" and Web API /api/self_update)
pub fn spawn_check();                                              // the start-up release check thread
pub fn holding() -> bool;                                          // an install in flight or sent < 90 s ago: no scan pass
pub fn available() -> Option<serde_json::Value>;                   // {version, size} of the newer release, None when none
pub fn install(version: &str, paused: &dyn Fn() -> bool, pass_idle: &dyn Fn() -> bool) -> Result<(), (u16, String)>; // download + send to elfldr; paused asked before both; Err = (HTTP status, text)

// launch.rs (titles: overlay these; running: title ids with a live session, kept by the watcher)
pub fn watch(titles: Arc<Mutex<HashSet<String>>>, running: Arc<Mutex<HashSet<String>>>);
pub fn backport_in_use(path: &std::path::Path) -> bool; // a backport redirect of ours reads from it
// launch::nsfs is private to launch (apply, has, release, plan, hide_ampr_plan, in_use)

// net.rs (blocking GET over sceHttp/sceSsl, 4 pinned root CAs; Err on non-200, sce error or > max bytes)
pub fn https_get(url: &str, max: usize) -> std::io::Result<Vec<u8>>;

// web.rs (shared with install.rs: the source delete of install_delete_source)
pub(crate) fn safe_delete_source(src: &scan::Source, roots: &[String]) -> Result<(), &'static str>; // inside a scan root, no symlink, the kind's file; Err: the reason
```

## Web API (`src/web.rs`, port 8097, served to `src/ui.html`)
Rest mode can close the listening socket: on an accept error the server binds port 8097 again every second.
If a newer copy took the port meanwhile, this copy quits unless a game runs (then it keeps retrying the bind every
second). An install running or queued does not keep it: its `/pkg` URL is on port 8097, which the other copy now
answers, and two copies would scan at once. The install fails and its marker means not installed. `/api/quit` and
`/api/self_update` still refuse during an install (`install::active()`).
- `GET /` ui.html (`__VERSION__` filled in); `GET /app-icon.png` the app icon (`ps5/icon0.png`);
  `GET /kraken.appcache` the AppCache manifest (version + page hash; the page reloads itself on `updateready`, so a
  new build shows at once instead of on the next open). These three are `no-cache`,
  the rest `no-store`.
- `GET /api/state`:
  `{version, update:{version:"<tag>", size}|null (the installable newer release found at start, see Self-update), busy, paused, waiting:"shadowmount"|null, running:[TID], install:{running, percent: 0..100|null, count: queue + installable + kept (the rail item)},
  games:[{title_id, title_name, version, path, kind:"pkg"|"folder"|"installed", packing:"plain"|"zstd"|"lz4"|null (installed: null),
  drive, size, status:"pending"|"checking"|"building"|"ready"|"error"|"duplicate", error, cache_size, cache_built, backport,
  playgo: bool|null (the memoized "eboot imports PlayGo", null when unknown or `add_emulators` is off), playgo_off: bool (false while `add_emulators` is off),
  crash: {at, process, status}|null (the last crash, from crashes.json, see Crash report),
  rebuild_queued: bool (a rebuild asked for, not started yet; the UI keeps Rebuild disabled through it and while the game is pending, checking or building),
  registered: bool (`register::registered`: staged param.json files and the TID in app.db, one app.db read per request),
  mounted: bool (`/system_ex/app/<TID>` is a mount point, one mount table per request), source_present: bool (the source path exists),
  overlay_off: bool (listed in `overlay_off` and a folder or installed source),
  installed: bool (the title has an installed copy: on the installed row and on the sources it beat), installed_version: that copy's version|null,
  ng_off: bool|null (the game sheet's ampr-emu-ng switch: on installed rows `!ng_installed` (an installed row run through ng reports `kind: "installed"`, its `path` is the `app.pkg`); on plain folders, while `inject_ampr` is on, `!ng_folder` (listed in `ampr_off`); null on the others),
  image: the `.exfat`/`.ffpkg`/`.ffpfs`/`.ffpfsc` file of an image game|null,
  retail: bool (only on the installed row of a retail install; 8 bytes of its app.pkg read per poll, ponytail)}],

  drives:[{mount, label, kind, total, free, ejectable}], cache:{path, size, free, low_space, ampr_present}, log:[last 100 lines of the last 64 KiB]}`
- `GET /api/report/<TID>` -> plain text for an issue: Kraken Mount version, a firmware line to fill in,
  the game (name, title id, version, source, kind, drive, status and error; the image file of an image game), ampr-emu-ng (`/api/ampr`
  kind and version), the PlayGo stub (on, off, or not added), the last crash, and the last 40 lines of
  the log tail (64 KiB) that contain the title id; 404 for an invalid or absent library title.
  The game sheet's Report button opens it.
- `GET /api/hidden` -> `[{path, title_id, title_name, version, kind, packing, image, drive, size}]`, one per `exclude` entry, read by `scan::read_source` on request (the UI asks when the Hidden tab opens and after hide/unhide, never per poll); all but `path` null when it is not a readable game.
- `GET /api/settings` -> settings JSON; `POST /api/settings` (JSON body, any subset of the keys) -> merged
  over the current `config.toml` (a missing key keeps the file's value, lists not sent stay verbatim; the UI
  sends only changed keys), save,
  apply, rescan; 409 when a sent `overlay_off` names a pkg game of the library; 500 when `config.toml` cannot be
  read (every settings save refuses then, so the defaults never overwrite the file; invalid UTF-8 is read
  lossily). A change of `ui_*` keys only does not wake the scan.
- `POST /api/playgo/<TID>`, body exactly `on` or `off` -> removes the title from / adds it to `playgo_off`,
  saves, applies, rescans, logs `<TID>: PlayGo stub on|off`, 204; 404 for an invalid or absent library
  title, 400 for any other body.
- `POST /api/ng/<TID>`, body exactly `on` or `off` -> ampr-emu-ng for one installed game (`Kind::Installed` or
  `scan::installed_pkg`): the title leaves both `ampr_installed_on`/`_off`, then joins the one of its body when that
  differs from `ampr_installed`. For a plain folder game: `off` adds the title to `ampr_off`, `on` removes it; 409
  while `inject_ampr` is off. As `/api/playgo` (`change_settings`), logs `<TID>: ampr-emu-ng on|off` when it
  changed, 204; 404 for an invalid or absent library title, 400 for any other body, 409 for any other title
  (pkg, zstd or LZ4 folder).
- `POST /api/overlay/<TID>`, body exactly `on` or `off` -> removes the title from / adds it to `overlay_off`,
  as `/api/playgo` (shared `change_settings`), logs `<TID>: overlay on|off`, 204; 404 for an invalid or absent
  library title, 400 for any other body (for a library title), 409 for `off` on a pkg game, 500 when the save fails.
- `POST /api/self_update`, body `{"version":"<tag>"}` -> `update::install`: 200 `{}` once the ELF is downloaded
  and sent to elfldr; 400 for a missing version or one that is not the newer release found at start; 409 while an install runs or is queued, while a scan pass holds `State::work` (checked once the hold below is set), while
  a game runs (checked before the download and again before the send: the new copy's `take_port` would be refused) or another update runs or was sent less than 90 s ago; 503 when elfldr does not
  answer on 127.0.0.1:9021 (1 s connect probe before the download, closed at once); 502 + text when the asset
  is over 32 MiB, the download fails (one try), its length is not the asset size or it is not ELF/SELF
  (`main::is_module`), or the send fails. Send: connect 127.0.0.1:9021, `write_all`, `shutdown(Write)`, drop.
  The new copy then takes over the port (asks this copy to quit) and saves its own self copy.
  While an install runs and for 90 s after its send the scan loop starts no pass (`update::holding`, checked after
  taking `State::work`), so a new copy that never takes over only delays the scan. The UI shows Updating... until `/api/state` answers another `version` (then reloads), or an error after 2 min. Logged
  `update: <tag> (<n> bytes) sent to elfldr` or `update: <tag> failed (<code>): <error>`.
- `GET /api/ampr` -> `{kind: "bundled"|"downloaded"|"hand"|"missing"|"error", version, updating}` of
  `ampr/libSceAmpr.sprx`; `POST /api/ampr/update` -> forced update, answered when done: 200 + result text,
  409 while one runs, 502 + error text.
- `POST /api/rescan`, `POST /api/rebuild/<TID>`, `POST /api/rebuild-all`, `POST /api/purge` -> 204.
- `POST /api/sizes/<TID>`, body = a `duplicate` row's path -> `{game}`: that copy's size only (.pkg length, folder
  du); 404 when the body is not a duplicate of the title.
- `GET /api/sizes/<TID>` -> `{game: u64, dlc: u64, save: u64|null, cache: u64}` in bytes,
  measured on request; 404 for an invalid or absent library title. Game is the pkg file or folder (installed: app.pkg, or the ext copy's du),
  DLC is summed across `/user/addcont` and both extended drives' `user/addcont` and `ps5/user/addcont`,
  save is summed across eight-hex-digit `/user/home/<id>/<family>/<TID>` directories, where family
  is `savedata_prospero` for a `PPSA` title id and `savedata` otherwise.
  Missing directories count as zero; unreadable `/user/home` gives `save: null`.
- `POST /api/launch/<TID>` -> starts the title for the foreground user (`sceLncUtilLaunchApp`, then
  `sceSystemServiceLaunchApp`), 204 once a launch is taken; 404 for an invalid or absent library title,
  409 while a game runs or when the title is not `ready`, 502 + error text when both calls fail.
- `POST /api/close/<TID>` -> closes the title as the console's "Close game" does (`sys::close_app`:
  `sceLncUtilKillApp` on the app id of the title's process, `launch::game_app` = sceKernelGetAppInfo; SIGKILL of
  that pid when LncUtil refuses), 204 once taken (the launch watcher then sees the exit); 404 for an invalid or
  absent library title, 409 when the title is not in `running`, 502 + error text when its process is not found
  or the kill fails. The game sheet's Play button is Close game while the title runs (confirmed first).
- `POST /api/delete/<TID>`, body: the source path the UI showed (the game's `path`) -> 204 on success,
  404 for an invalid or absent library title, 409 `an installed game: uninstall it from the console's menu` for an
  installed title (`installed_refusal`; also `/api/hide` and `games/uninstall`), 409 while the scan loop is in a pass (it holds
  `State::work` for scan, purge, build and register; delete only try-locks it), when the body is not
  the current library source, while a game runs, this title builds, the source path is unsafe,
  its title mount cannot be unmounted, or the source is the running or a queued install job (`the package is
  queued or installing; cancel it first`, as `/api/install/delete-source`; ponytail: a job queued after the check
  fails at its start on the missing file); 500 on source or cache deletion failure. It validates the
  source is inside a scan root with no symlink from the root down (links above the root are not checked), is not under any `.../user/app/` (an installed copy's
  files), and is a .pkg file, game directory or (image game) image file, then unmounts `/system_ex/app/<TID>` without force and deletes the source (an image: its mounts released first). If that
  fails, the cache and tile stay and the next pass remounts the title (a folder partly removed stays so).
  Then it uninstalls the home-screen entry (failure logged), removes the title from the launch set
  and its game row, wakes the scan loop, deletes the cache, and logs and optionally notifies.
  A body that is the path of a `duplicate` row of the title (`delete_duplicate`, after `lock_title`): only that source
  is deleted (the queued/installing check, `installed_refusal` and `safe_delete_source` as above), the row dropped,
  204; the library copy's registration, mount and cache stay (they belong to the title), no toast.
- `POST /api/hide/<TID>`, body: the game's `path` (an image game's mount folder; empty = any) -> `games/uninstall` below
  (`uninstall_keep`: off the home screen, source and its duplicates added to `exclude`), 204; also 409 when the
  body is not the current library source. A title never staged (no `/user/app/<TID>/sce_sys/param.json`, e.g.
  its first build failed) is only excluded: no uninstall. Errors as plain text with the same HTTP codes.
  A body that is the path of a `duplicate` row (after `lock_title`): only that path joins `exclude`
  (`change_settings`, scan woken), the row dropped, 204; the library copy stays registered. The Library's
  Duplicates chip (shown while the state lists a `duplicate`; All and the kind chips list them too) lists only these
  rows; their game sheets have Rebuild (per title, so the library copy's cache), Hide and Delete.
- `POST /api/hidden/delete`, body: an `exclude` path that `scan::read_source` reads as a game (an image file through
  `image::probe`, not mounted) -> its source deleted (an image: `remove_image`, its mounts released first; its
  registered source is a game folder in its mount, `image::holds`)
  (the guards of `/api/delete`: `State::work` try-lock, its title not running, not queued or installing,
  `safe_delete_source`), then the path leaves `exclude`, 204; 404 when not excluded, 409 for an unreadable path or a
  guard, 500 when the delete or the save fails. When the path is the title's registered source (`cache::stamp_source`) and the registration is ours (`register::is_ours`; a source hidden by hand stays registered), the title is unmounted first (409 when busy) and uninstalled + forgotten after the delete, as `/api/delete`; a failed delete mounts it again (without a backport layer; the scan does not remount an excluded source). The title's cache stays (Purge). The Hidden screen's Delete.
- `GET /api/qr` -> `{url, svg}`: `http://<LAN IP>:8097/` (the source address of a UDP socket connected to
  192.0.2.1, `lan_ip`; nothing is sent; else the request's Host when it is a LAN IPv4) and its QR code (the `qrcode`
  crate, as ps5-dump-forge), 404 without either. Fetched each time Settings opens. Shown inside Settings > Appearance, after a separator on the right of its rows (hidden under 700px).
- `POST /api/unhide`, body: an `exclude` path -> as `manual/add` (`manual`): off `exclude`, saved, scan woken, 204;
  404 `not hidden` when it was not excluded, 400 for a path that is not absolute UTF-8, 500 when the save fails.
- `POST /api/unhide-all` -> `exclude` emptied in one `change_settings` save, scan woken, 204; 404 when it was empty.
- `POST /api/backport/delete/<TID>` -> deletes the title's `backport` as the last scan recorded it (never a path
  from the request), 204. Guards of `lock_title` (404, 409 while a pass holds `State::work`, a game runs or the
  title builds); 404 `no backport`; 409 EPERM unless the path is that recorded backport, absolute without `..`,
  named `<TID>*` directly in a dir named `backports` (any case), and it and its parent are real dirs (no symlink).
  A folder game's `/system_ex/app/<TID>` stack is first redone without the unionfs (`register::drop_backport`,
  never MNT_FORCE): a failure is 409 and nothing is deleted. Then `remove_dir_all` (failure 500, scan woken;
  a partial delete stays so), the row's backport cleared, a rebuild forced (pkg: the backport was copied into
  the cache; folder: its lib dir fed fakelib/), scan woken. Logged `<TID>: backport <path> deleted; rebuild queued`,
  rejections and errors with the title id and path.
- `POST /api/backports/delete-all` -> the same for every library title with a backport, under one try-lock of
  `State::work` (each delete wakes the scan loop); 204 when all succeed, 404 when none has one, else the highest
  failure code with one `<TID>: <reason>` line per failed title (the others are deleted).
- `POST /api/v1/<route>`: the subset of ShadowMount+'s API v1 (SMP `docs/api.md`) that Playden calls, on port 8097
  and on 127.0.0.1:10101 (ShadowMount+'s default API port), so clients on the console that only know ShadowMount+
  (Playden, RecompStore) reach it (#126). Loopback only on purpose (ShadowMount+ binds 0.0.0.0): ps5upload on a PC
  probes 10101 and would see both mounters running. Port 10101 answers only `/api/v1/` (anything else: the unknown-route 404 below);
  while ShadowMount+ still holds it, Kraken Mount binds it again every 2 s, and again after an accept error.
  Body: one JSON object (empty or whitespace = `{}`; anything else 400). Answer: JSON, HTTP 200 with `status: 0`,
  else `{status: <FreeBSD errno>, error}` with HTTP 400 (EINVAL), 404 (ENOENT; also an unknown route), 409
  (EBUSY, EPERM, EINVAL), 500 (a config save error's errno) or 501 (ENOTSUP). Routes (SMP meaning -> Kraken Mount):
  - `version` -> `{status, api_version: 1, kraken_mount_version, capabilities: ["list_games", "add_manual_source",
    "remove_manual_source", "uninstall_game", "rescan", "fakelib", "fakelib_unlisted", "reregister_missing", "game_info"]}`. A client
    checks a capability, not the version: `fakelib_unlisted` = `games/fakelib` takes a title not in the library yet;
    `reregister_missing` = a title uninstalled outside Kraken Mount registers again at the next pass, with no restart.
  - `games` -> `{status, count, games: [{title_id, name, version, path, runtime_path (folder: path; pkg: ""; an image game: `path` is the image file, `runtime_path` its mount folder),
    source_type: "folder"|"pkg"|"installed", managed: true (installed: false), installed (= registered), mounted, source_available (= source_present),
    status (the Kraken status), error ("" when none), fakelib_mode: "full"|"disabled" (overlay_off, folder and installed sources only)}]}`; library
    titles only (no `duplicate` rows); the three flags as in `/api/state`.
  - `games/info {title_id}` -> that title's `games` entry at the top level, its `status` being the API's 0 (as SMP
    answers it; SMP's size, patch and DLC fields are not sent); 404 ENOENT when the title is not in the library.
    RecompStore's installer polls it for its own tile until `installed` is true.
  - `manual/remove {path}` -> adds `path` to `exclude` (Kraken Mount sees every folder in its scan roots, so
    removing a manual source means not seeing it); `manual/add {path}` -> takes it off `exclude`. `path` must be an
    absolute string without control characters (400 EINVAL), stored as config.rs does (trailing `/` trimmed). Idempotent:
    `{status, changed}`; a change saves the config, wakes the scan loop and logs `<path>: added to|removed from the
    excluded paths from the web API`.
  - `scan` -> wakes the scan loop, `{status}`.
  - `games/uninstall {title_id}` -> off the home screen, keeping the source and the cache (game-written files stay):
    the guards of `/api/delete` (`lock_title`: invalid or absent title 404 ENOENT; `State::work` held, a game running
    or this title building 409 EBUSY), `register::is_ours` (an installed copy or no staged param.json 409 EPERM, as
    auto-remove), `register::unmount_title` without force (failure 409 EBUSY), the source path and the paths of its
    duplicates (else the next scan registers a duplicate) added to `exclude` and saved (not absolute 409 EINVAL; a save error 500, the next pass remounts), `register::uninstall`
    (failure 500 EIO, the exclusions this call added taken back: the next pass remounts it), then `register::forget`, the title out of the launch set and its game rows
    dropped, scan loop woken. Logged `<TID>: uninstalled from the web API, <path> kept and excluded`.
    ponytail: a non-UTF-8 source path is excluded by its lossy text, which then matches nothing.
  - `games/fakelib {title_id, mode}` (`disabled` = overlay off; `full` or `default` = on; `emulators` 501 ENOTSUP;
    other 400 EINVAL) or `{title_id, ignore_global_fakelib: bool}` (true = off; `mode` wins when both) -> as
    `/api/overlay` (`set_overlay`: invalid title id 404 ENOENT, off for a pkg of the library 409 EPERM) ->
    `{status, fakelib_mode, saved: true, applies_on_next_launch: true, listed}`. A valid title id not in the library
    (`listed: false`) is still put in or taken off `overlay_off` and saved: a launcher sets it before its title folder
    is first scanned (scans wait while the launcher runs), so the first build already makes an empty `fakelib/`
    (`inputs_key` holds `overlay_off`). ponytail: such a title that turns out to be a pkg keeps its overlay
    (`cache::overlay_off` ignores pkg sources). `/api/overlay` stays 404 for a title not in the library.
- `GET /api/install` -> `{current: Job|null, queue: [id], results: [[id, text]], installable: [row], kept: [row]}`
  (see Install). Job = `{id, title_id, name, kind: "game"|"update"|"patch"|"dlc", done_bytes, total_bytes,
  phase: "wait"|"download"|"install", error}` (`error` is always null, reserved: a job's error is in `results`); `results` = the last result per source path: `done`, `cancelled` or
  the error text. Row = `{id (= path), path, title_id, name, version, kind: "game"|"patch"|"dlc", size, drive,
  installed_version|null, retail}`. `installable` = patch/DLC packages of the last scan not installed yet; `kept` =
  `.pkg` duplicates of an installed title (kind `game`) plus patch/DLC packages already installed. A patch is
  installed when the installed copy's appmeta version is not older than the patch's (a patch with no version stays
  installable; `/user/patch/<TID>` is not read, ponytail); a DLC when `sceAppInstUtilGetAddcontInstalledStatus`
  says state 4, remembered for 60 s or until the job records a new result (each call swaps the process
  credentials); while a long sceAppInstUtil call runs or waits (`install::appinst_busy`: an `AppInst` holder such
  as InstallAll, or `submit`) the remembered answer is reused with no native call. ponytail: a check, not a
  try-lock; a long call that starts right after the check still blocks that request. The UI polls it with `/api/state` (2 s) while the Install screen is open or an install runs. The Install screen has three tabs: Running (`current` and the queue), Patches and DLC (`installable`, with its count) and Already installed (`kept`, with its count); it opens on Running, which also holds the After install switch (`install_delete_source`, also in Settings).
- `POST /api/install`, body = the path of a library or duplicate `.pkg` game (the duplicate of an installed title is
  its Update) or of a patch/DLC package -> 204 queued; 404 not in the last scan; 400 empty body, a non-pkg source, a
  patch whose game is not installed; 409 already queued or running. No refusal while a game runs (the job waits).
- `POST /api/install/cancel` -> 204; 404 when nothing runs (also once the job returned: its result is recorded and
  `current` cleared at once).
- `POST /api/install/delete-source`, body = a `kept` row's path -> 204 after: `State::work` free (else 409), not
  queued or installing (409), in `kept` now (409; a duplicate `.pkg` newer than the installed version is not kept), `installed::is_installed(tid)` for patch and
  base rows (409; a kept `dlc` row is already the DLC's installed status, since a DLC installs regardless of the base
  game), `safe_delete_source` as a
  `.pkg` (409 with the reason). Removes the file only (no uninstall, no cache change), drops it from the game and
  package rows, wakes the scan.
- `GET|HEAD /pkg/<token>/<name>` (loopback only): the running install's `.pkg` for the console installer (see Install).
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
  taking over) or body `shutdown` from any host (the UI's Stop button, toast "Kraken Mount stopped"); 409 while a game runs or an install runs or is queued, and (a newer copy only) while a scan pass holds `State::work`:
  quitting mid-build would leave a cache without `.built`; once taken, the lock is held until exit. A copy loaded by
  hand or by a payload manager during a build longer than `take_port`'s 5 s therefore exits with the port-busy toast.

## Self copy and start on open (as Dump Forge)
`ps5/build.sh` builds twice: stage 1, then stage 2 (`--features self-copy`, env `KRAKEN_SELF_ELF`) embedding
stage 1. The pushed stage 2 saves stage 1 to `/data/krakenmount/kraken-mount.elf` at start when the bytes differ
(temp + fsync + rename; the last pushed wins). The tile opens the AppCache'd page; on the console's own browser
(127.0.0.1:8097) a failed `/api/state` makes it ask elfldr once per page load:
`GET http://127.0.0.1:9021/data/krakenmount/kraken-mount.elf?args=serve&pipe=0&n=<nonce>`, then it keeps polling.
Self-update (as Dump Forge): `POST /api/self_update` sends the newer release's `kraken-mount.elf` to elfldr
(127.0.0.1:9021); that copy takes over port 8097 (`web::take_port` asks this one to quit) and, being a stage 2 build,
saves its own stage 1 as the self copy, so the tile starts the new version. ps5-payload-manager updates it too:
each release carries `payloads.json` (a custom repository, `https://github.com/quer3q/kraken-mount/releases/latest/download/payloads.json`).
- Requests are capped at 16 KiB: head too large 431, head + Content-Length too large (or overflowing) 413,
  non-numeric Content-Length 400.

## Build and check
`cargo check --target x86_64-unknown-freebsd` (fast, host). Full payload: `ps5/build.sh` (Docker).
Release (`.github/workflows/build.yml`, on a `v*` tag): checkout, the tag must equal `v` + Cargo.toml's version,
`sha256sum -c SHA256SUMS`, then `ps5/make-payloads-json.sh kraken-mount.elf payloads.json`, and a draft release with
`kraken-mount.elf`, `SHA256SUMS` and `payloads.json` (not in SHA256SUMS).
`ps5/make-payloads-json.sh <elf> [out.json]` (default `payloads.json`): the ps5-payload-manager catalog
(schema: itsPLK/ps5-payload-manager `CUSTOM_REPOSITORIES.md`), top-level `name` "Kraken Mount" and one payload: id
`kraken-mount`, name `Kraken Mount`, filename `kraken-mount.elf`, url
`https://github.com/$GITHUB_REPOSITORY/releases/download/v<version>/kraken-mount.elf` (repo default
`quer3q/kraken-mount`), version `v<Cargo.toml version>`, `last_update` (`PAYLOADS_DATE`, default today UTC), a
one-line description, category `Utilities & Tools`, checksum = sha256 of the ELF, author `quer3q`, source and
details_url. The ELF must be named `kraken-mount.elf`. Written with printf, then parsed back (jq, else `python3 -I`).
