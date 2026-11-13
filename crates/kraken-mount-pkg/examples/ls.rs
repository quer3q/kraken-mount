//! List a dump-forge debug FPKG: `ls <pkg>` prints every entry with its size, extent count
//! (`-` when it has compressed blocks) and block counts (compressed/raw, `-` when `blocks()`
//! refuses it); `ls <pkg> --cat <path>` writes one file to stdout.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::process::ExitCode;

use kraken_mount_pkg::{Error, PRead, Pkg};

struct Host(File);

impl PRead for Host {
    fn pread(&self, off: u64, buf: &mut [u8]) -> Result<usize, Error> {
        self.0.read_at(buf, off).map_err(|e| Error::Io(e.raw_os_error().unwrap_or(-1)))
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (pkg, cat) = match args.as_slice() {
        [p] => (p, None),
        [p, flag, path] if flag == "--cat" => (p, Some(path)),
        _ => return Err("usage: ls <pkg> [--cat <path>]".into()),
    };
    let file = File::open(pkg).map_err(|e| format!("{pkg}: {e}"))?;
    let len = file.metadata().map_err(|e| format!("{pkg}: {e}"))?.len();
    let io = Host(file);
    let p = Pkg::open(&io, len).map_err(|e| format!("{pkg}: {e}"))?;
    let mut out = std::io::stdout().lock();
    let Some(path) = cat else {
        for e in p.files() {
            let kind = if e.is_dir { "d" } else { "f" };
            let ext = p.extents(&e).map_or("-".to_string(), |x| x.len().to_string());
            let blk = match p.blocks(&e) {
                Ok(b) => {
                    let c = b.iter().filter(|b| b.compressed).count();
                    format!("{c}/{}", b.len() - c)
                }
                Err(_) => "-".to_string(),
            };
            writeln!(out, "{kind} {:>12} {:>4} {:>9} {}", e.size, ext, blk, e.path).map_err(|e| e.to_string())?;
        }
        return Ok(());
    };
    let e = p.lookup(path).ok_or_else(|| format!("{path}: not in the package"))?;
    if e.is_dir {
        return Err(format!("{path}: a directory"));
    }
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    loop {
        let n = p.read_at(&io, &e, off, &mut buf).map_err(|e| format!("{path}: {e}"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        off += n as u64;
    }
    if off != e.size {
        return Err(format!("{path}: read {off} of {} bytes", e.size));
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(m) => {
            eprintln!("ls: {m}");
            ExitCode::FAILURE
        }
    }
}
