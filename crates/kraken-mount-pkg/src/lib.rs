//! Read a ps5-dump-forge debug FPKG (`.pkg`) in place: plaintext (`PPRPLAIN-NOAUTH!`), Kraken
//! layout at any level (`stored` or Kraken-compressed blocks, no byte shuffles), no LIH patch.
//! Every file maps to a list of Kraken blocks; files whose blocks are all raw also map to
//! plaintext byte runs of the `.pkg`.
//!
//! Ported from ps5-dump-forge (`crates/ps5-dump-forge-fpkg` and its vendored fpkg crate),
//! GPL-3.0-or-later. See ps5-dump-forge's fpkg reader for the format.
//!
//! Layers: FIH header + CNT table (`pkg.rs`), outer PFS superblock, dinodes and block maps
//! (`outer.rs`), the Kraken layout descriptor (`kraken.rs`).

#![no_std]

extern crate alloc;

/// The Kraken block decoder.
pub mod decoder;
mod kraken;
mod outer;
mod pkg;
mod tree;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Positional reads from the package file. A short count (0 included) means end of file.
pub trait PRead {
    fn pread(&self, off: u64, buf: &mut [u8]) -> Result<usize, Error>;
}

/// Why a package could not be opened or read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The `PRead` implementation failed; the value is its errno (or any code it chooses).
    Io(i32),
    /// Not a PS5 package at all (no FIH magic, too short, a PS4 package).
    NotPs5(String),
    /// A PS5 package (or a request on one file) this crate does not serve: retail, LIH patch,
    /// encrypted, byte-shuffled; extents of a compressed file; blocks of an unaligned file.
    Unsupported(String),
    /// The package contradicts itself or is truncated.
    Corrupt(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(code) => write!(f, "read error {code}"),
            Error::NotPs5(m) => write!(f, "not a PS5 package: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported package: {m}"),
            Error::Corrupt(m) => write!(f, "corrupt package: {m}"),
        }
    }
}

/// An opened package: its block maps and its sorted file table.
pub struct Pkg {
    #[allow(dead_code)]
    image: pkg::Image,
    /// Every path, back to back; `Node::path_*` index into it.
    arena: String,
    /// Sorted by path (byte order).
    nodes: Vec<Node>,
    /// Every file's extents, back to back; `Node::ext_*` index into it.
    extents: Vec<Extent>,
    /// `ext_pos[i]`: the file offset where `extents[i]` starts.
    ext_pos: Vec<u64>,
    /// Every file's blocks, back to back; `Node::blk_*` index into it.
    blocks: Vec<Block>,
    /// Why `blocks()` refuses a file; `Node::blk_err` indexes into it.
    blk_errs: Vec<String>,
}

const NO_ERR: u32 = u32::MAX;

struct Node {
    path_off: u32,
    path_len: u32,
    size: u64,
    is_dir: bool,
    /// Mount offset of a file's data (0 for a directory or a CNT file).
    logical: u64,
    /// The extents are valid (every block raw, or a CNT file); else reads decode.
    ext_ok: bool,
    ext_start: u32,
    ext_end: u32,
    blk_start: u32,
    blk_end: u32,
    blk_err: u32,
}

/// Container entries that stand in for a `sce_sys` file the inner image lacks (`FP/lib.rs:94-104`).
const CNT_FALLBACKS: [(u32, &str); 3] = [
    (0x1200, "sce_sys/icon0.png"),
    (0x1280, "sce_sys/icon0.dds"),
    (0x2000, "sce_sys/param.json"),
];

fn table(image: pkg::Image, mut walked: Vec<tree::Walked>) -> Result<Pkg, Error> {
    let has = |w: &[tree::Walked], p: &str| w.iter().any(|x| x.path == p);
    for (id, path) in CNT_FALLBACKS {
        let Some(e) = image.cnt.entry(id) else { continue };
        if e.flags1 & 0x8000_0000 != 0 || e.size == 0 || has(&walked, path) {
            continue;
        }
        if !has(&walked, "sce_sys") {
            walked.push(tree::Walked { path: "sce_sys".into(), is_dir: true, size: 0, logical: 0, cnt_at: None });
        }
        walked.push(tree::Walked { path: path.into(), is_dir: false, size: e.size, logical: 0, cnt_at: Some(e.at) });
    }
    walked.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    if let Some(w) = walked.windows(2).find(|w| w[0].path == w[1].path) {
        return Err(pkg::corrupt(alloc::format!("{} is listed twice", w[0].path)));
    }
    let arena_len: usize = walked.iter().map(|w| w.path.len()).sum();
    let mut arena = String::with_capacity(arena_len);
    let mut nodes = Vec::with_capacity(walked.len());
    let mut extents = Vec::new();
    let mut ext_pos = Vec::new();
    let mut tmp = Vec::new();
    let mut blocks = Vec::new();
    let mut blk_errs = Vec::new();
    for w in &walked {
        let ext_start = extents.len() as u32;
        let blk_start = blocks.len() as u32;
        let mut ext_ok = true;
        let mut blk_err = NO_ERR;
        if !w.is_dir {
            tmp.clear();
            match w.cnt_at {
                Some(at) if w.size > 0 => tmp.push(Extent { pkg_off: at, len: w.size }),
                Some(_) => {}
                None => match image.extents(w.logical, w.size, &mut tmp) {
                    Ok(()) => {}
                    Err(Error::Unsupported(_)) => ext_ok = false,
                    Err(e) => return Err(e),
                },
            }
            if ext_ok {
                let mut pos = 0u64;
                for e in &tmp {
                    ext_pos.push(pos);
                    pos += e.len;
                }
                if pos != w.size {
                    return Err(pkg::corrupt(alloc::format!("{}: extents cover {pos} of {} bytes", w.path, w.size)));
                }
                extents.extend_from_slice(&tmp);
            }
            let made = match w.cnt_at {
                Some(at) => {
                    cnt_blocks(at, w.size, &mut blocks);
                    Ok(())
                }
                None => file_blocks(&image, w, &mut tmp, &mut blocks),
            };
            if let Err(m) = made {
                blocks.truncate(blk_start as usize);
                blk_err = blk_errs.len() as u32;
                blk_errs.push(m);
            }
        }
        nodes.push(Node {
            path_off: arena.len() as u32,
            path_len: w.path.len() as u32,
            size: w.size,
            is_dir: w.is_dir,
            logical: if w.cnt_at.is_some() { 0 } else { w.logical },
            ext_ok,
            ext_start,
            ext_end: extents.len() as u32,
            blk_start,
            blk_end: blocks.len() as u32,
            blk_err,
        });
        arena.push_str(&w.path);
    }
    drop(walked);
    extents.shrink_to_fit();
    ext_pos.shrink_to_fit();
    blocks.shrink_to_fit();
    Ok(Pkg { image, arena, nodes, extents, ext_pos, blocks, blk_errs })
}

/// A CNT file as raw pseudo-blocks of at most 256 KiB, back to back from `at`.
fn cnt_blocks(at: u64, size: u64, out: &mut Vec<Block>) {
    let mut done = 0u64;
    while done < size {
        let n = (size - done).min(crate::decoder::BLOCK as u64) as u32;
        let desc = crate::decoder::Desc { even_len: n.min(crate::decoder::HALF as u32), mode: 0 };
        out.push(Block { raw_len: n, pkg_off: at + done, stored_len: n, compressed: false, desc });
        done += u64::from(n);
    }
}

/// A file's Kraken blocks, or why `blocks()` refuses it: its data starts inside a block, or a
/// block's stored bytes are split across `.pkg` runs. `tmp` is scratch.
fn file_blocks(image: &pkg::Image, w: &tree::Walked, tmp: &mut Vec<Extent>, out: &mut Vec<Block>) -> Result<(), String> {
    if w.size == 0 {
        return Ok(());
    }
    let range = image.kraken.range(w.logical, w.size);
    if range.first().map(|b| b.logical) != Some(w.logical) {
        return Err(alloc::format!("{}: its data (mount {:#x}) does not start on a block boundary", w.path, w.logical));
    }
    for b in range {
        tmp.clear();
        image.image.pkg_extents(b.stored_at, u64::from(b.stored_len), tmp).map_err(|e| alloc::format!("{}: {e}", w.path))?;
        let [run] = tmp.as_slice() else {
            return Err(alloc::format!(
                "{}: the block at mount {:#x} is stored in {} separate .pkg runs",
                w.path,
                b.logical,
                tmp.len()
            ));
        };
        out.push(Block { raw_len: b.len, pkg_off: run.pkg_off, stored_len: b.stored_len, compressed: !b.raw(), desc: b.desc });
    }
    Ok(())
}

/// One file or directory of the package's game tree.
#[derive(Debug, Clone, Copy)]
pub struct Entry<'a> {
    pub path: &'a str,
    pub size: u64,
    pub is_dir: bool,
    /// Index into the package's sorted table (not the PFS inode number).
    ino: u32,
}

/// A run of plaintext bytes inside the `.pkg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub pkg_off: u64,
    pub len: u64,
}

/// One Kraken block of a file: `raw_len` logical bytes, stored as `stored_len` contiguous bytes
/// at `pkg_off` in the `.pkg`. A raw block's stored bytes are its logical bytes
/// (`stored_len == raw_len`); a compressed one needs `kraken::decode_block`. Only a file's last
/// block can extend past its end; the bytes beyond the file's size belong to no file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    pub raw_len: u32,
    pub pkg_off: u64,
    pub stored_len: u32,
    pub compressed: bool,
    /// Decode fields for `kraken::decode_block` (meaningful when `compressed`).
    pub desc: crate::decoder::Desc,
}

impl Pkg {
    /// Parse the package's headers, block maps and file tree, and map every file to its
    /// `.pkg` extents. `len` is the package file's length.
    pub fn open<R: PRead>(io: &R, len: u64) -> Result<Pkg, Error> {
        let image = pkg::Image::open(io, len)?;
        let walked = tree::walk(io, &image)?;
        table(image, walked)
    }

    fn entry(&self, i: usize) -> Entry<'_> {
        let n = &self.nodes[i];
        let at = n.path_off as usize;
        Entry { path: &self.arena[at..at + n.path_len as usize], size: n.size, is_dir: n.is_dir, ino: i as u32 }
    }

    /// Every regular file and directory, sorted by path.
    pub fn files(&self) -> impl Iterator<Item = Entry<'_>> {
        (0..self.nodes.len()).map(|i| self.entry(i))
    }

    /// The entry at `path` ("sce_sys/param.json", no leading slash).
    pub fn lookup(&self, path: &str) -> Option<Entry<'_>> {
        let i = self
            .nodes
            .binary_search_by(|n| {
                let at = n.path_off as usize;
                self.arena[at..at + n.path_len as usize].cmp(path)
            })
            .ok()?;
        Some(self.entry(i))
    }

    /// The file node behind `e` (`None` for a directory or a foreign entry).
    fn file(&self, e: &Entry) -> Option<&Node> {
        self.nodes.get(e.ino as usize).filter(|n| !n.is_dir)
    }

    /// The plaintext byte runs of `e` inside the `.pkg`, in file order (empty for a directory).
    /// `Unsupported` when one of its blocks is compressed.
    pub fn extents(&self, e: &Entry) -> Result<&[Extent], Error> {
        match self.file(e) {
            None => Ok(&[]),
            Some(n) if n.ext_ok => Ok(&self.extents[n.ext_start as usize..n.ext_end as usize]),
            Some(_) => Err(pkg::unsupported(alloc::format!("{}: has Kraken-compressed blocks, no plaintext extents", e.path))),
        }
    }

    /// The Kraken blocks of `e`, consecutive, covering file bytes `[0, size)` in order (empty
    /// for a directory or an empty file). `Unsupported` when its data does not start on a block
    /// boundary or a block's stored bytes are not one contiguous `.pkg` range.
    pub fn blocks(&self, e: &Entry) -> Result<&[Block], Error> {
        match self.file(e) {
            None => Ok(&[]),
            Some(n) if n.blk_err == NO_ERR => Ok(&self.blocks[n.blk_start as usize..n.blk_end as usize]),
            Some(n) => Err(Error::Unsupported(self.blk_errs[n.blk_err as usize].clone())),
        }
    }

    /// Up to `buf.len()` bytes of `e` at `off`; 0 at or past its end. A file with only raw
    /// blocks is read in place with no allocation; a compressed one allocates its buffers.
    pub fn read_at<R: PRead>(&self, io: &R, e: &Entry, off: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let Some(n) = self.file(e) else { return Ok(0) };
        if off >= n.size || buf.is_empty() {
            return Ok(0);
        }
        let want = (n.size - off).min(buf.len() as u64) as usize;
        if !n.ext_ok {
            let mut dec = pkg::Decoder::new();
            self.image.read_mount(io, n.logical + off, &mut buf[..want], &mut dec)?;
            return Ok(want);
        }
        let range = n.ext_start as usize..n.ext_end as usize;
        let exts = &self.extents[range.clone()];
        let pos = &self.ext_pos[range];
        // The last extent starting at or before `off` (the first starts at 0).
        let mut i = pos.partition_point(|&p| p <= off) - 1;
        let mut done = 0usize;
        while done < want {
            let cur = off + done as u64;
            let inner = cur - pos[i];
            let k = ((exts[i].len - inner) as usize).min(want - done);
            pkg::pread_exact(io, exts[i].pkg_off + inner, &mut buf[done..done + k])?;
            done += k;
            i += 1;
        }
        Ok(done)
    }
}

/// Debug accessors for a host probe (feature `probe` only; never in a shipped build).
#[cfg(feature = "probe")]
pub mod probe {
    use super::*;

    /// One Kraken block: mount range, stored range in `pfs_image.dat`, record fields.
    #[derive(Debug, Clone, Copy)]
    pub struct Block {
        pub logical: u64,
        pub len: u32,
        pub stored_at: u64,
        pub stored_len: u64,
        pub even_len: u32,
        pub mode: u8,
    }

    /// One walked entry: its mount range, or its CNT offset.
    #[derive(Debug, Clone)]
    pub struct File {
        pub path: String,
        pub is_dir: bool,
        pub logical: u64,
        pub size: u64,
        pub cnt_at: Option<u64>,
    }

    pub struct Probe {
        image: pkg::Image,
        pub blocks: Vec<Block>,
        pub files: Vec<File>,
        pub mount_size: u64,
        /// The inner superblock's mount offset.
        pub meta_base: u64,
        /// Why the tree walk failed (then `files` is empty).
        pub walk_error: Option<Error>,
    }

    impl Probe {
        /// The image (compressed blocks kept) and the unsorted tree walk.
        pub fn open<R: PRead>(io: &R, len: u64) -> Result<Probe, Error> {
            let image = pkg::Image::open(io, len)?;
            let (files, walk_error) = match tree::walk(io, &image) {
                Ok(w) => (
                    w.into_iter()
                        .map(|w| File { path: w.path, is_dir: w.is_dir, logical: w.logical, size: w.size, cnt_at: w.cnt_at })
                        .collect(),
                    None,
                ),
                Err(e) => (Vec::new(), Some(e)),
            };
            let blocks = image
                .kraken
                .probe_blocks()
                .map(|(logical, len, stored_at, stored_len, even_len, mode)| Block { logical, len, stored_at, stored_len, even_len, mode })
                .collect();
            let mount_size = image.mount_size();
            let meta_base = image.meta_base;
            Ok(Probe { image, blocks, files, mount_size, meta_base, walk_error })
        }

        /// `.pkg` runs behind stored (`pfs_image.dat`) bytes `at..at + n`, merged where contiguous.
        pub fn stored_extents(&self, at: u64, n: u64) -> Result<Vec<Extent>, Error> {
            let mut out = Vec::new();
            self.image.image.pkg_extents(at, n, &mut out)?;
            Ok(out)
        }

        /// Walk the tree again over `mount`, the whole decoded mount (for a compressed
        /// metadata region), replacing `files` and `walk_error`.
        pub fn walk_decoded<R: PRead>(&mut self, io: &R, mount: Vec<u8>) {
            self.image.probe_mount = Some(mount);
            match tree::walk(io, &self.image) {
                Ok(w) => {
                    self.files = w
                        .into_iter()
                        .map(|w| File { path: w.path, is_dir: w.is_dir, logical: w.logical, size: w.size, cnt_at: w.cnt_at })
                        .collect();
                    self.walk_error = None;
                }
                Err(e) => self.walk_error = Some(e),
            }
            self.image.probe_mount = None;
        }

        /// `pfs_image.dat` size in bytes.
        pub fn image_size(&self) -> u64 {
            self.image.image.size
        }
    }
}
