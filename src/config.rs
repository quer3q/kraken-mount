//! Settings from /data/krakenmount/config.toml (DESIGN.md "Settings").
//! ponytail: a flat TOML subset (key = string | bool | integer | array of strings, # comments),
//! no tables; a toml crate when the file needs more.
use serde_json::{json, Value};
pub const CONFIG_PATH: &str = "/data/krakenmount/config.toml";

#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub scan_paths: Vec<String>,
    /// Absolute paths whose sources (the path itself or anything under it) the scan skips.
    pub exclude: Vec<String>,
    pub inject_ampr: bool,
    /// Installed games run through ampr-emu-ng: a debug FPKG install from its app.pkg (pkg mode),
    /// a retail one with /data/krakenmount/ampr/* in its fakelib. Per title: `ng_installed`.
    pub ampr_installed: bool,
    /// Title ids that override `ampr_installed` (on / off) for one installed game.
    pub ampr_installed_on: Vec<String>,
    pub ampr_installed_off: Vec<String>,
    /// Delete the source .pkg after the console reports its install done (install.rs).
    pub install_delete_source: bool,
    pub add_emulators: bool,
    /// Title ids that get no bundled PlayGo stub (pgo_stub): not added, not upgraded.
    pub playgo_off: Vec<String>,
    /// Title ids that get an empty fakelib/ (no overlay at launch: no game fakelib, ampr-emu-ng or emulators).
    pub overlay_off: Vec<String>,
    /// Title ids of plain folder games that get no ampr-emu-ng even with inject_ampr (their own fakelib
    /// and the emulators stay). zstd and LZ4 folders and pkg games always get it: they cannot start
    /// without it. Per title: `ng_folder`.
    pub ampr_off: Vec<String>,
    pub ampr_auto_update: bool,
    pub backports: bool,
    pub scan_interval_secs: u64,
    /// A source changed less than this many seconds ago is still being copied (0 = off).
    pub stability_wait_secs: u64,
    pub low_space_gb: u64,
    /// SMP auto_remove_missing_games: unregister a title whose source is gone this long.
    pub auto_remove_missing: bool,
    pub auto_remove_delay_secs: u64,
    pub notify: bool,
    /// Write /data/krakenmount/kraken.log (the payload's stdout always gets the lines).
    pub log: bool,
    pub debug: bool,
    pub ui_theme: String,
    pub ui_scheme: String,
    pub ui_sidebar_collapsed: bool,
    /// "" = the browser's language.
    pub ui_lang: String,
}

pub const UI_THEMES: &[&str] = &["dark", "light"];
pub const UI_SCHEMES: &[&str] = &["abyss", "ember", "moss", "cobalt", "dusk", "rose", "sand", "volt", "mono"];
/// The LANGS table in ui.html.
pub const UI_LANGS: &[&str] = &["en", "ru", "zh", "de", "es", "fr", "ja", "pt", "it", "ko", "ar"];

impl Settings {
    /// `inject_ampr` for one plain folder title: off when it is in `ampr_off`. zstd and LZ4 folders
    /// do not ask: their packs need ampr-emu-ng.
    pub fn ng_folder(&self, tid: &str) -> bool {
        self.inject_ampr && !self.ampr_off.iter().any(|t| t == tid)
    }
    /// `ampr_installed` for one installed title: its override, else the global switch.
    pub fn ng_installed(&self, tid: &str) -> bool {
        if self.ampr_installed_on.iter().any(|t| t == tid) {
            true
        } else if self.ampr_installed_off.iter().any(|t| t == tid) {
            false
        } else {
            self.ampr_installed
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            scan_paths: Vec::new(),
            exclude: Vec::new(),
            inject_ampr: true,
            ampr_installed: false,
            ampr_installed_on: Vec::new(),
            ampr_installed_off: Vec::new(),
            install_delete_source: false,
            add_emulators: true,
            playgo_off: Vec::new(),
            overlay_off: Vec::new(),
            ampr_off: Vec::new(),
            ampr_auto_update: true,
            backports: true,
            scan_interval_secs: 10,
            stability_wait_secs: 10,
            low_space_gb: 20,
            auto_remove_missing: true,
            auto_remove_delay_secs: 300,
            notify: true,
            log: true,
            debug: false,
            ui_theme: "dark".into(),
            ui_scheme: "abyss".into(),
            ui_sidebar_collapsed: false,
            ui_lang: String::new(),
        }
    }
}


const HELP: &[(&str, &str)] = &[
    ("scanpath", "replaces the default scan roots (/data/homebrew, /data/etaHEN/games, <drive>/homebrew, ...)"),
    ("exclude", "absolute paths the scan skips: a source at one of them or under it is not listed or registered (a registered title stays registered)"),
    ("inject_ampr", "true = copy /data/krakenmount/ampr/* on top of a folder game's fakelib (pkg games always get it; a change only refreshes fakelib/)"),
    ("ampr_installed", "true = installed games run through ampr-emu-ng: a debug FPKG install starts from its app.pkg (pkg mode, rebuilds the cache), a retail one gets /data/krakenmount/ampr/* in its fakelib"),
    ("ampr_installed_on", "title ids of installed games that run through ampr-emu-ng whatever ampr_installed says"),
    ("ampr_installed_off", "title ids of installed games that run without ampr-emu-ng whatever ampr_installed says"),
    ("install_delete_source", "true = delete the source .pkg once the console reports its install done (Install tab)"),
    ("add_emulators", "true = add the bundled DLC (dlc_emu) and PlayGo (pgo_stub) emulators to a game's fakelib when it needs them and ships none (a change only refreshes fakelib/)"),
    ("playgo_off", "title ids that get no bundled PlayGo stub (pgo_stub): not added, not upgraded; a libScePlayGo.sprx the game ships stays (a change only refreshes fakelib/)"),
    ("overlay_off", "title ids (folder and installed games; a pkg game keeps its overlay) that get no fakelib overlay at launch: not the game's fakelib/, not ampr-emu-ng, not the emulators (a change only refreshes fakelib/)"),
    ("ampr_off", "title ids of plain folder games that get no ampr-emu-ng even with inject_ampr; their own fakelib/ and the emulators stay (zstd/LZ4 folders and pkg games always get it; a change only refreshes fakelib/)"),
    ("ampr_auto_update", "true = once per start, download a newer ampr-emu-ng release from GitHub (a hand-placed libSceAmpr.sprx is kept)"),
    ("backports", "true = apply backports/<TID>* from the game's scan root, then the other roots, then /data/homebrew"),
    ("scan_interval_seconds", "scan loop period, 1..3600"),
    ("stability_wait_seconds", "skip a source changed less than this many seconds ago (still being copied), 0..3600, 0 = off"),
    ("low_space_gb", "warn when /data free space is below this"),
    ("auto_remove_missing", "true = remove a title from the home screen when its source is missing for auto_remove_delay_seconds (the cache stays; not while a game runs, not with DLC in /user/addcont/<TID>)"),
    ("auto_remove_delay_seconds", "how long a source must stay missing before auto_remove_missing removes its title, 1..86400"),
    ("notify", "true = system toasts"),
    ("log", "true = write /data/krakenmount/kraken.log (starts over past 1 MiB)"),
    ("debug", "true = klog capture to /data/krakenmount/klog.txt and the /file/data/... route"),
    ("ui_theme", "web UI theme: \"dark\" or \"light\""),
    ("ui_scheme", "web UI color scheme: \"abyss\", \"ember\", \"moss\", \"cobalt\", \"dusk\", \"rose\", \"sand\", \"volt\" or \"mono\""),
    ("ui_sidebar_collapsed", "true = the web UI sidebar shows icons only"),
    ("ui_lang", "web UI language: \"\" = the browser's, or en, ru, zh, de, es, fr, ja, pt, it, ko, ar"),
];

fn flag(v: &str) -> Result<bool, String> {
    match v {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => Err(format!("expected 0 or 1, got {v:?}")),
    }
}

fn one_of(v: &str, allowed: &[&str]) -> Result<String, String> {
    if allowed.contains(&v) {
        Ok(v.to_string())
    } else {
        Err(format!("expected one of {}, got {v:?}", allowed.join(", ")))
    }
}

fn num(v: &str) -> Result<u64, String> {
    v.parse().map_err(|_| format!("expected a number, got {v:?}"))
}

/// Applies one key=value. Unknown keys are an error too.
fn set(s: &mut Settings, key: &str, v: &str) -> Result<(), String> {
    match key {
        "scanpath" if v.starts_with('/') => {
            // A repeated root would list every game in it again as its own duplicate.
            let v = v.trim_end_matches('/');
            let v = if v.is_empty() { "/" } else { v };
            if !s.scan_paths.iter().any(|p| p == v) {
                s.scan_paths.push(v.to_string());
            }
        }
        "scanpath" => return Err(format!("expected an absolute path, got {v:?}")),
        "exclude" if v.starts_with('/') && !v.chars().any(char::is_control) => {
            let v = v.trim_end_matches('/');
            let v = if v.is_empty() { "/" } else { v };
            if !s.exclude.iter().any(|p| p == v) {
                s.exclude.push(v.to_string());
            }
        }
        "exclude" => return Err(format!("expected an absolute path without control characters, got {v:?}")),
        "inject_ampr" => s.inject_ampr = flag(v)?,
        "ampr_installed" => s.ampr_installed = flag(v)?,
        "install_delete_source" => s.install_delete_source = flag(v)?,
        "add_emulators" => s.add_emulators = flag(v)?,
        "ampr_installed_on" | "ampr_installed_off" if crate::scan::valid_tid(v) => {
            let list = if key == "ampr_installed_on" { &mut s.ampr_installed_on } else { &mut s.ampr_installed_off };
            if !list.iter().any(|t| t == v) {
                list.push(v.to_string());
            }
        }
        "ampr_installed_on" | "ampr_installed_off" => return Err(format!("expected a title id like PPSA01556, got {v:?}")),
        "playgo_off" if crate::scan::valid_tid(v) => {
            if !s.playgo_off.iter().any(|t| t == v) {
                s.playgo_off.push(v.to_string());
            }
        }
        "playgo_off" => return Err(format!("expected a title id like PPSA01556, got {v:?}")),
        "overlay_off" if crate::scan::valid_tid(v) => {
            if !s.overlay_off.iter().any(|t| t == v) {
                s.overlay_off.push(v.to_string());
            }
        }
        "overlay_off" => return Err(format!("expected a title id like PPSA01556, got {v:?}")),
        "ampr_off" if crate::scan::valid_tid(v) => {
            if !s.ampr_off.iter().any(|t| t == v) {
                s.ampr_off.push(v.to_string());
            }
        }
        "ampr_off" => return Err(format!("expected a title id like PPSA01556, got {v:?}")),
        "ampr_auto_update" => s.ampr_auto_update = flag(v)?,
        // Removed setting (the game's fakelib is always used in full): ignored, dropped on the next save.
        "game_fakelib" => {}
        "backports" => s.backports = flag(v)?,
        "scan_interval_seconds" => match num(v)? {
            n @ 1..=3600 => s.scan_interval_secs = n,
            n => return Err(format!("{n} not in 1..3600")),
        },
        "stability_wait_seconds" => match num(v)? {
            n @ 0..=3600 => s.stability_wait_secs = n,
            n => return Err(format!("{n} not in 0..3600")),
        },
        "low_space_gb" => s.low_space_gb = num(v)?,
        "auto_remove_missing" => s.auto_remove_missing = flag(v)?,
        "auto_remove_delay_seconds" => match num(v)? {
            n @ 1..=86400 => s.auto_remove_delay_secs = n,
            n => return Err(format!("{n} not in 1..86400")),
        },
        "notify" => s.notify = flag(v)?,
        "log" => s.log = flag(v)?,
        "debug" => s.debug = flag(v)?,
        "ui_theme" => s.ui_theme = one_of(v, UI_THEMES)?,
        "ui_scheme" => s.ui_scheme = one_of(v, UI_SCHEMES)?,
        "ui_sidebar_collapsed" => s.ui_sidebar_collapsed = flag(v)?,
        "ui_lang" if v.is_empty() => s.ui_lang.clear(),
        "ui_lang" => s.ui_lang = one_of(v, UI_LANGS)?,
        _ => return Err("unknown key".into()),
    }
    Ok(())
}

/// A TOML string at the start of `v` ("basic" with \\ \" \n \t escapes, or 'literal') and the rest.
fn toml_str(v: &str) -> Result<(String, &str), String> {
    if let Some(r) = v.strip_prefix('\'') {
        let end = r.find('\'').ok_or("unclosed string")?;
        return Ok((r[..end].to_string(), &r[end + 1..]));
    }
    let r = v.strip_prefix('"').ok_or_else(|| format!("expected a string, got {v:?}"))?;
    let mut out = String::new();
    let mut chars = r.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Ok((out, &r[i + 1..])),
            '\\' => match chars.next().map(|(_, e)| e) {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                e => return Err(format!("unsupported escape \\{}", e.unwrap_or(' '))),
            },
            c => out.push(c),
        }
    }
    Err("unclosed string".into())
}

/// `v` without a trailing `# comment` outside strings.
fn strip_comment(v: &str) -> &str {
    let (mut quote, mut esc) = (None, false);
    for (i, c) in v.char_indices() {
        if esc {
            esc = false;
            continue;
        }
        match (quote, c) {
            (None, '#') => return &v[..i],
            (None, '"' | '\'') => quote = Some(c),
            (Some('"'), '\\') => esc = true,
            (Some(q), c) if c == q => quote = None,
            _ => {}
        }
    }
    v
}

/// A TOML value as the texts `set` takes: bool -> "1"/"0", integer, string, or one text per array item.
fn toml_value(v: &str) -> Result<Vec<String>, String> {
    let v = strip_comment(v).trim();
    if let Some(inner) = v.strip_prefix('[') {
        let mut rest = inner.strip_suffix(']').ok_or("unclosed array")?.trim();
        let mut out = Vec::new();
        while !rest.is_empty() {
            let (item, r) = toml_str(rest)?;
            out.push(item);
            rest = r.trim_start();
            rest = match rest.strip_prefix(',') {
                Some(r) => r.trim_start(),
                None if rest.is_empty() => rest,
                None => return Err(format!("expected , in the array, got {rest:?}")),
            };
        }
        return Ok(out);
    }
    if v.starts_with('"') || v.starts_with('\'') {
        let (text, rest) = toml_str(v)?;
        if !rest.trim().is_empty() {
            return Err(format!("unexpected {:?} after the string", rest.trim()));
        }
        return Ok(vec![text]);
    }
    match v {
        "true" => Ok(vec!["1".into()]),
        "false" => Ok(vec!["0".into()]),
        n if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit() || b == b'_') => Ok(vec![n.replace('_', "")]),
        _ => Err(format!("expected a string, true/false, a number or an array, got {v:?}")),
    }
}

fn toml_quote(v: &str) -> String {
    format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Config text of the last parse; warnings are logged only when the text changed, since
/// load() runs every scan loop.
static LAST_TEXT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

fn parse(text: &str) -> Settings {
    let quiet = {
        let mut last = LAST_TEXT.lock().unwrap_or_else(|e| e.into_inner());
        let same = *last == text;
        if !same {
            *last = text.to_owned();
        }
        same
    };
    let mut s = Settings::default();
    let mut lines = text.lines().enumerate();
    while let Some((n, line)) = lines.next() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            if !quiet {
                log!("config {CONFIG_PATH}:{}: not key = value: {line:?}", n + 1);
            }
            continue;
        };
        let (k, mut v) = (k.trim(), v.trim().to_string());
        // A multi-line array: join lines up to the closing ].
        if v.starts_with('[') {
            while !strip_comment(&v).trim_end().ends_with(']') {
                let Some((_, more)) = lines.next() else { break };
                v = format!("{} {}", strip_comment(&v).trim_end(), more.trim());
            }
        }
        let r = toml_value(&v).and_then(|vals| {
            if k == "scanpath" {
                s.scan_paths.clear(); // the array replaces the defaults (and an earlier scanpath line)
            } else if k == "playgo_off" {
                s.playgo_off.clear(); // a later playgo_off line replaces an earlier one
            } else if k == "overlay_off" {
                s.overlay_off.clear();
            } else if k == "ampr_installed_on" {
                s.ampr_installed_on.clear();
            } else if k == "ampr_installed_off" {
                s.ampr_installed_off.clear();
            } else if k == "ampr_off" {
                s.ampr_off.clear();
            } else if k == "exclude" {
                s.exclude.clear();
            } else if vals.len() != 1 {
                return Err("expected one value, not an array".into());
            } else {
                return set(&mut s, k, &vals[0]);
            }
            // An array item that is wrong is logged and skipped; the others stay, so the next save
            // does not drop them.
            for x in &vals {
                if let (Err(e), false) = (set(&mut s, k, x), quiet) {
                    log!("config {CONFIG_PATH}:{}: {k}: {e}; skipped", n + 1);
                }
            }
            Ok(())
        });
        if let (Err(e), false) = (r, quiet) {
            log!("config {CONFIG_PATH}:{}: {k}: {e}; using the default", n + 1);
        }
    }
    s
}

fn render(s: &Settings) -> String {
    let mut out = String::from("# Kraken Mount settings (TOML). The web UI edits this file.\n");
    for &(k, help) in HELP {
        out += &format!("\n# {help}\n");
        match k {
            // ponytail: no scanpath = default roots, shown as an example only.
            "scanpath" if s.scan_paths.is_empty() => out += "# scanpath = [\"/data/homebrew\"]\n",
            "scanpath" => {
                let items: Vec<String> = s.scan_paths.iter().map(|p| toml_quote(p)).collect();
                out += &format!("scanpath = [{}]\n", items.join(", "));
            }
            "exclude" => {
                let items: Vec<String> = s.exclude.iter().map(|p| toml_quote(p)).collect();
                out += &format!("exclude = [{}]\n", items.join(", "));
            }
            "inject_ampr" => out += &format!("{k} = {}\n", s.inject_ampr),
            "ampr_installed" => out += &format!("{k} = {}\n", s.ampr_installed),
            "ampr_installed_on" | "ampr_installed_off" => {
                let list = if k == "ampr_installed_on" { &s.ampr_installed_on } else { &s.ampr_installed_off };
                let items: Vec<String> = list.iter().map(|t| toml_quote(t)).collect();
                out += &format!("{k} = [{}]\n", items.join(", "));
            }
            "install_delete_source" => out += &format!("{k} = {}\n", s.install_delete_source),
            "add_emulators" => out += &format!("{k} = {}\n", s.add_emulators),
            "playgo_off" => {
                let items: Vec<String> = s.playgo_off.iter().map(|t| toml_quote(t)).collect();
                out += &format!("playgo_off = [{}]\n", items.join(", "));
            }
            "overlay_off" => {
                let items: Vec<String> = s.overlay_off.iter().map(|t| toml_quote(t)).collect();
                out += &format!("overlay_off = [{}]\n", items.join(", "));
            }
            "ampr_off" => {
                let items: Vec<String> = s.ampr_off.iter().map(|t| toml_quote(t)).collect();
                out += &format!("ampr_off = [{}]\n", items.join(", "));
            }
            "ampr_auto_update" => out += &format!("{k} = {}\n", s.ampr_auto_update),
            "backports" => out += &format!("{k} = {}\n", s.backports),
            "scan_interval_seconds" => out += &format!("{k} = {}\n", s.scan_interval_secs),
            "stability_wait_seconds" => out += &format!("{k} = {}\n", s.stability_wait_secs),
            "low_space_gb" => out += &format!("{k} = {}\n", s.low_space_gb),
            "auto_remove_missing" => out += &format!("{k} = {}\n", s.auto_remove_missing),
            "auto_remove_delay_seconds" => out += &format!("{k} = {}\n", s.auto_remove_delay_secs),
            "notify" => out += &format!("{k} = {}\n", s.notify),
            "log" => out += &format!("{k} = {}\n", s.log),
            "ui_theme" => out += &format!("{k} = {}\n", toml_quote(&s.ui_theme)),
            "ui_scheme" => out += &format!("{k} = {}\n", toml_quote(&s.ui_scheme)),
            "ui_sidebar_collapsed" => out += &format!("{k} = {}\n", s.ui_sidebar_collapsed),
            "ui_lang" => out += &format!("{k} = {}\n", toml_quote(&s.ui_lang)),
            _ => out += &format!("{k} = {}\n", s.debug),
        }
    }
    out
}

/// Reads the config; creates it with the defaults when missing.
pub fn load() -> Settings {
    try_load().unwrap_or_else(|e| {
        log!("config {CONFIG_PATH}: cannot read: {e}; using the defaults");
        Settings::default()
    })
}

/// As `load`, but a read error is returned: a save built on the defaults would overwrite the file.
/// Invalid UTF-8 is read lossily (an editor's Latin-1 must not reset every setting).
pub fn try_load() -> std::io::Result<Settings> {
    match std::fs::read(CONFIG_PATH) {
        Ok(b) => Ok(parse(&String::from_utf8_lossy(&b))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let s = Settings::default();
            if let Err(e) = save(&s) {
                log!("config {CONFIG_PATH}: cannot create: {e}");
            }
            Ok(s)
        }
        Err(e) => Err(e),
    }
}

/// Whether `a` and `b` differ only in the web UI's own keys (no rescan needed).
pub fn ui_only_change(a: &Settings, b: &Settings) -> bool {
    let mut b = b.clone();
    b.ui_theme.clone_from(&a.ui_theme);
    b.ui_scheme.clone_from(&a.ui_scheme);
    b.ui_sidebar_collapsed = a.ui_sidebar_collapsed;
    b.ui_lang.clone_from(&a.ui_lang);
    *a == b
}

/// Writes the config atomically (tmp + rename).
pub fn save(s: &Settings) -> std::io::Result<()> {
    let path = std::path::Path::new(CONFIG_PATH);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = format!("{CONFIG_PATH}.tmp");
    std::fs::write(&tmp, render(s))?;
    std::fs::rename(&tmp, path)
}

pub fn to_json(s: &Settings) -> Value {
    json!({
        "scanpath": s.scan_paths,
        "exclude": s.exclude,
        "inject_ampr": s.inject_ampr,
        "ampr_installed": s.ampr_installed,
        "ampr_installed_on": s.ampr_installed_on,
        "ampr_installed_off": s.ampr_installed_off,
        "install_delete_source": s.install_delete_source,
        "add_emulators": s.add_emulators,
        "playgo_off": s.playgo_off,
        "overlay_off": s.overlay_off,
        "ampr_off": s.ampr_off,
        "ampr_auto_update": s.ampr_auto_update,
        "backports": s.backports,
        "scan_interval_seconds": s.scan_interval_secs,
        "stability_wait_seconds": s.stability_wait_secs,
        "low_space_gb": s.low_space_gb,
        "auto_remove_missing": s.auto_remove_missing,
        "auto_remove_delay_seconds": s.auto_remove_delay_secs,
        "notify": s.notify,
        "log": s.log,
        "debug": s.debug,
        "ui_theme": s.ui_theme,
        "ui_scheme": s.ui_scheme,
        "ui_sidebar_collapsed": s.ui_sidebar_collapsed,
        "ui_lang": s.ui_lang,
    })
}

/// Missing keys take the default; bools may be true/false or 0/1.
pub fn from_json(v: &Value) -> Result<Settings, String> {
    let obj = v.as_object().ok_or("settings: expected a JSON object")?;
    let mut s = Settings::default();
    for (k, val) in obj {
        let vals: Vec<&Value> = match (k.as_str(), val) {
            ("scanpath" | "exclude" | "playgo_off" | "overlay_off" | "ampr_installed_on" | "ampr_installed_off" | "ampr_off", Value::Array(a)) => a.iter().collect(),
            ("scanpath" | "exclude" | "playgo_off" | "overlay_off" | "ampr_installed_on" | "ampr_installed_off" | "ampr_off", _) => return Err(format!("{k}: expected an array of strings")),
            _ => vec![val],
        };
        for x in vals {
            let text = match x {
                Value::String(t) => t.trim().to_string(),
                Value::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
                Value::Number(n) => n.to_string(),
                _ => return Err(format!("{k}: unsupported value {x}")),
            };
            set(&mut s, k, &text).map_err(|e| format!("{k}: {e}"))?;
        }
    }
    Ok(s)
}
