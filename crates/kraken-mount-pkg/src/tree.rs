//! The inner filesystem's tree, walked through the mount the way the console walks it: the
//! superblock at the metadata base, the inode table one block after it (390 inodes to a block),
//! and one dirent stream per directory from `uroot` (inode 4) down. Ported from
//! `ps5-dump-forge-fpkg/src/tree.rs` (walk) with the constants of ps5-dump-forge's vendored
//! fpkg crate (`inner.rs`, `plan.rs`).
//!
//! Everything here is untrusted: counts are capped before anything is sized by them, every
//! directory is entered once (a second visit is a cycle), and the walk stops at a fixed number
//! of entries and of path bytes.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::pkg::{corrupt, le32, le64, Decoder, Image, BLOCK};
use crate::{Error, PRead};

const INODE_LEN: usize = 0xA8;
const INODES_PER_BLOCK: u64 = 390;
const FIRST_DIR_INODE: u32 = 4;
const DIRENT_FILE: u32 = 2;
const DIRENT_DIR: u32 = 3;
const DIRENT_DOT: u32 = 4;
const DIRENT_DOTDOT: u32 = 5;
const MAX_INODES: u64 = 4_000_000;
pub(crate) const MAX_ENTRIES: usize = 1_000_000;
const MAX_PATH_BYTES: usize = 256 << 20;
const MAX_DIR_BYTES: u64 = 64 << 20;
const SUPERBLOCK_MAGIC: u64 = 20_130_315;

/// One walked entry, before the table is sorted.
pub(crate) struct Walked {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Logical (mount) offset of a file's data; 0 for a directory.
    pub logical: u64,
    /// A file read from the CNT container instead: its absolute `.pkg` offset.
    pub cnt_at: Option<u64>,
}

struct Inode {
    mode: u16,
    size: u64,
    logical: u64,
}

fn inode_offset(ino: u32) -> u64 {
    let i = u64::from(ino);
    (i / INODES_PER_BLOCK) * BLOCK + (i % INODES_PER_BLOCK) * INODE_LEN as u64
}

fn shown(prefix: &str) -> &str {
    if prefix.is_empty() { "uroot" } else { prefix }
}

/// Walk the inner tree: every file and directory, unsorted, paths without a leading slash.
pub(crate) fn walk<R: PRead>(io: &R, img: &Image) -> Result<Vec<Walked>, Error> {
    let meta_base = img.meta_base;
    let mount_size = img.mount_size();
    if meta_base.checked_add(BLOCK).map_or(true, |e| e > mount_size) {
        return Err(corrupt(format!(
            "the metadata base {meta_base:#x} is past the mount's {mount_size:#x}"
        )));
    }
    let dec = core::cell::RefCell::new(Decoder::new());
    let mut sb = [0u8; 0x40];
    img.read_mount(io, meta_base, &mut sb, &mut dec.borrow_mut())?;
    if le64(&sb, 0) != 2 || le64(&sb, 8) != SUPERBLOCK_MAGIC {
        return Err(corrupt("inner superblock version/magic mismatch"));
    }
    if u64::from(le32(&sb, 0x20)) != BLOCK {
        return Err(corrupt(format!("inner block size {:#x}", le32(&sb, 0x20))));
    }
    let inode_count = le64(&sb, 0x30);
    let table_at = meta_base + BLOCK;
    let table_len = inode_count.div_ceil(INODES_PER_BLOCK).max(1) * BLOCK;
    let table_fits = inode_count <= MAX_INODES
        && table_at.checked_add(table_len).map_or(false, |e| e <= mount_size);
    if inode_count <= u64::from(FIRST_DIR_INODE) || !table_fits {
        return Err(corrupt(format!("implausible inner inode count {inode_count}")));
    }
    let inode = |ino: u32| -> Result<Inode, Error> {
        if u64::from(ino) >= inode_count {
            return Err(corrupt(format!("inode {ino} is past the table's {inode_count}")));
        }
        let mut raw = [0u8; INODE_LEN];
        img.read_mount(io, table_at + inode_offset(ino), &mut raw, &mut dec.borrow_mut())?;
        Ok(Inode {
            mode: u16::from_le_bytes([raw[0], raw[1]]),
            size: le64(&raw, 8),
            logical: le64(&raw, 0x60),
        })
    };

    let mut out: Vec<Walked> = Vec::new();
    let mut path_bytes = 0usize;
    // Directory streams live in the metadata region, so together they cannot be longer than it.
    let mut dir_budget = mount_size - meta_base;
    let mut visited: BTreeSet<u32> = BTreeSet::new();
    let mut pending: Vec<(u32, String)> = vec![(FIRST_DIR_INODE, String::new())];
    while let Some((dir_ino, prefix)) = pending.pop() {
        if !visited.insert(dir_ino) {
            return Err(corrupt(format!("directory inode {dir_ino} is linked twice (a cycle)")));
        }
        let dir = inode(dir_ino)?;
        if dir.mode & 0xF000 != 0x4000 {
            return Err(corrupt(format!("inode {dir_ino} ({}) is not a directory", shown(&prefix))));
        }
        if dir.size > MAX_DIR_BYTES || dir.logical.checked_add(dir.size).map_or(true, |e| e > mount_size) {
            return Err(corrupt(format!(
                "directory {} claims {} bytes at {:#x}",
                shown(&prefix),
                dir.size,
                dir.logical
            )));
        }
        dir_budget = dir_budget
            .checked_sub(dir.size)
            .ok_or_else(|| corrupt("the directories claim more bytes than the metadata region holds"))?;
        let mut bytes = vec![0u8; dir.size as usize];
        img.read_mount(io, dir.logical, &mut bytes, &mut dec.borrow_mut())?;
        let mut o = 0usize;
        while o + 16 <= bytes.len() {
            let ino = le32(&bytes, o);
            let kind = le32(&bytes, o + 4);
            let name_len = le32(&bytes, o + 8) as usize;
            let ent_size = le32(&bytes, o + 12) as usize;
            if ent_size == 0 {
                break; // the zero padding after the last entry
            }
            if ent_size < 16 + name_len || o + 16 + name_len > bytes.len() {
                return Err(corrupt(format!("a directory entry of {} is malformed", shown(&prefix))));
            }
            let raw_name = &bytes[o + 16..o + 16 + name_len];
            o = o.saturating_add(ent_size);
            if kind == DIRENT_DOT || kind == DIRENT_DOTDOT {
                continue;
            }
            let name = core::str::from_utf8(raw_name)
                .ok()
                .filter(|n| !n.is_empty() && *n != "." && *n != ".." && !n.contains(['/', '\0']))
                .ok_or_else(|| {
                    corrupt(format!(
                        "{} holds a name that is not a valid path component ({:?})",
                        shown(&prefix),
                        String::from_utf8_lossy(raw_name)
                    ))
                })?;
            path_bytes = path_bytes.saturating_add(prefix.len() + 1 + name.len());
            if out.len() >= MAX_ENTRIES || path_bytes > MAX_PATH_BYTES {
                return Err(corrupt(format!(
                    "the image lists more than {MAX_ENTRIES} entries or {MAX_PATH_BYTES} path bytes"
                )));
            }
            let path = if prefix.is_empty() { String::from(name) } else { format!("{prefix}/{name}") };
            if kind == DIRENT_DIR {
                out.push(Walked { path: path.clone(), is_dir: true, size: 0, logical: 0, cnt_at: None });
                pending.push((ino, path));
            } else if kind == DIRENT_FILE {
                let f = inode(ino)?;
                if f.mode & 0xF000 != 0x8000 {
                    return Err(corrupt(format!("{path}: inode {ino} is not a regular file")));
                }
                if f.logical.checked_add(f.size).map_or(true, |e| e > meta_base) {
                    return Err(corrupt(format!(
                        "{path}: {} bytes at {:#x} run past the data region",
                        f.size, f.logical
                    )));
                }
                out.push(Walked { path, is_dir: false, size: f.size, logical: f.logical, cnt_at: None });
            } else {
                return Err(corrupt(format!("{path}: unknown directory entry kind {kind}")));
            }
        }
    }
    Ok(out)
}
