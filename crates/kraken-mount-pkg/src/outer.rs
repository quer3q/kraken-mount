//! The outer PFS: superblock, dinode table, `uroot` dirents, and each outer file's block map.
//! Port of `FP/pfs.rs:268-288,440-600`, `VF/outer.rs` (`Dinode`, `parse_dinodes`,
//! `parse_dirents`) and `FP/lib.rs:610-627` (`child`). Plaintext only, no sha3 checks.

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use crate::pkg::{corrupt, le32, le64, pread_exact, unsupported, BLOCK};
use crate::{Error, Extent, PRead};

const DINODE_LEN: usize = 0x2C8;
/// First direct block record (32-byte digest + u32 block = 36-byte stride).
const DIRECT_AT: usize = 0x64;
const INDIRECT_AT: usize = DIRECT_AT + 12 * 36;
const SIG_LEN: usize = 36;
/// `{digest, block}` records per indirect table block.
const PER_INDIRECT: usize = BLOCK as usize / SIG_LEN;
const SUPERBLOCK_MAGIC: u64 = 20_130_315;
const SUPERBLOCK_LEN: usize = 0x5A0;
const PLAINTEXT_MARKER: [u8; 16] = *b"PPRPLAIN-NOAUTH!";

/// Where the outer image sits in the `.pkg`.
#[derive(Clone, Copy)]
pub(crate) struct Geometry {
    pub pfs_offset: u64,
    pub blocks: u64,
}

impl Geometry {
    /// The `.pkg` offset of outer block `index`.
    pub fn block_off(&self, index: u64) -> Result<u64, Error> {
        if index >= self.blocks {
            return Err(corrupt(format!(
                "outer block {index} is past the image's {} blocks",
                self.blocks
            )));
        }
        Ok(self.pfs_offset + index * BLOCK)
    }

    fn block<R: PRead>(&self, io: &R, index: u64) -> Result<Vec<u8>, Error> {
        let mut b = vec![0u8; BLOCK as usize];
        pread_exact(io, self.block_off(index)?, &mut b)?;
        Ok(b)
    }
}

pub(crate) struct Superblock {
    pub dinode_count: u64,
    pub inode_table_block: u32,
}

/// Parse the outer superblock; refuse a native (AES-XTS) image.
pub(crate) fn superblock<R: PRead>(io: &R, geo: &Geometry, index: u64) -> Result<Superblock, Error> {
    let mut sb = [0u8; SUPERBLOCK_LEN];
    pread_exact(io, geo.block_off(index)?, &mut sb)?;
    if le64(&sb, 0) != 2 || le64(&sb, 8) != SUPERBLOCK_MAGIC {
        return Err(corrupt("outer superblock version/magic mismatch"));
    }
    if sb[0x370..0x380] != PLAINTEXT_MARKER {
        return Err(unsupported(
            "the outer image is AES-XTS encrypted; only plaintext (PPRPLAIN-NOAUTH!) \
             dump-forge packages are read",
        ));
    }
    Ok(Superblock { dinode_count: le64(&sb, 0x30), inode_table_block: le32(&sb, 0xD8) })
}

/// The dinode fields this reader uses.
pub(crate) struct Dinode {
    pub size: u64,
    pub direct: [u32; 12],
    pub indirect: [u32; 5],
}

/// The inode table block's first `count` records (as many as fit the block).
pub(crate) fn parse_dinodes(table: &[u8], count: u64) -> Vec<Dinode> {
    let fit = (table.len() / DINODE_LEN) as u64;
    (0..count.min(fit) as usize)
        .map(|j| {
            let o = j * DINODE_LEN;
            Dinode {
                size: le64(table, o + 8),
                direct: core::array::from_fn(|k| le32(table, o + DIRECT_AT + k * SIG_LEN + 32)),
                indirect: core::array::from_fn(|k| le32(table, o + INDIRECT_AT + k * SIG_LEN + 32)),
            }
        })
        .collect()
}

/// The inode `name` names in directory dinode `dir` (its entries fit its first block).
pub(crate) fn child<R: PRead>(
    io: &R,
    geo: &Geometry,
    nodes: &[Dinode],
    dir: usize,
    name: &str,
) -> Result<usize, Error> {
    let node = nodes.get(dir).ok_or_else(|| corrupt(format!("outer dinode {dir} is missing")))?;
    let block = geo.block(io, u64::from(node.direct[0]))?;
    let limit = (node.size as usize).min(block.len());
    let mut o = 0usize;
    while o + 16 <= limit {
        let name_len = le32(&block, o + 8) as usize;
        let ent_size = le32(&block, o + 12) as usize;
        if ent_size == 0 || o + 16 + name_len > limit {
            break;
        }
        let ino = le32(&block, o) as usize;
        if &block[o + 16..o + 16 + name_len] == name.as_bytes() && ino < nodes.len() {
            return Ok(ino);
        }
        o += ent_size;
    }
    Err(corrupt(format!("the outer image has no {name}")))
}

/// One outer file (`pfs_image.dat`, `naps_pkg_layout.dat`): its size and the outer block behind
/// each of its 64 KiB. Four bytes per block.
pub(crate) struct OuterFile {
    pub size: u64,
    geo: Geometry,
    map: Vec<u32>,
}

impl OuterFile {
    /// Map `node`'s blocks: the 12 direct slots, then each indirect level in turn.
    pub fn map<R: PRead>(io: &R, geo: &Geometry, node: &Dinode, what: &str) -> Result<Self, Error> {
        let need = node.size.div_ceil(BLOCK);
        if need > geo.blocks {
            return Err(corrupt(format!("{what} claims {} bytes, more than the outer image", node.size)));
        }
        let mut map = Vec::with_capacity(need as usize);
        for &b in node.direct.iter().take(need.min(12) as usize) {
            push_block(geo, &mut map, b, what)?;
        }
        for (level, &table) in node.indirect.iter().enumerate() {
            if map.len() as u64 >= need || table == 0 {
                break;
            }
            walk_table(io, geo, table, level, need, &mut map, what)?;
        }
        if (map.len() as u64) < need {
            return Err(corrupt(format!("{what}'s block map covers {} of its {need} blocks", map.len())));
        }
        Ok(Self { size: node.size, geo: *geo, map })
    }

    /// Exactly `buf.len()` bytes of this file at `off`.
    pub fn read<R: PRead>(&self, io: &R, off: u64, buf: &mut [u8]) -> Result<(), Error> {
        let mut runs = Vec::new();
        self.pkg_extents(off, buf.len() as u64, &mut runs)?;
        let mut done = 0usize;
        for e in runs {
            let n = e.len as usize;
            pread_exact(io, e.pkg_off, &mut buf[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    /// Append the `.pkg` runs behind this file's bytes `at..at + n` to `out`, merging with the
    /// last run in `out` (and with each other) where they are contiguous in the `.pkg`.
    pub fn pkg_extents(&self, at: u64, n: u64, out: &mut Vec<Extent>) -> Result<(), Error> {
        if at.checked_add(n).map_or(true, |e| e > self.size) {
            return Err(corrupt(format!(
                "a range of {n:#x} bytes at {at:#x} runs past an outer file of {:#x} bytes",
                self.size
            )));
        }
        let mut pos = at;
        let end = at + n;
        while pos < end {
            let within = pos % BLOCK;
            let take = (BLOCK - within).min(end - pos);
            let block = u64::from(self.map[(pos / BLOCK) as usize]);
            let pkg_off = self.geo.pfs_offset + block * BLOCK + within;
            match out.last_mut() {
                Some(last) if last.pkg_off + last.len == pkg_off => last.len += take,
                _ => out.push(Extent { pkg_off, len: take }),
            }
            pos += take;
        }
        Ok(())
    }
}

fn push_block(geo: &Geometry, map: &mut Vec<u32>, block: u32, what: &str) -> Result<(), Error> {
    if u64::from(block) >= geo.blocks {
        return Err(corrupt(format!("{what} points at outer block {block}, past the image")));
    }
    map.push(block);
    Ok(())
}

/// One indirect table (`level` 0 lists data blocks, higher levels list tables).
fn walk_table<R: PRead>(
    io: &R,
    geo: &Geometry,
    block: u32,
    level: usize,
    need: u64,
    map: &mut Vec<u32>,
    what: &str,
) -> Result<(), Error> {
    let table = geo.block(io, u64::from(block))?;
    for slot in 0..PER_INDIRECT {
        if map.len() as u64 >= need {
            break;
        }
        let child = le32(&table, slot * SIG_LEN + 32);
        if child == 0 {
            return Err(corrupt(format!("{what}'s indirect table at block {block} ends early")));
        }
        if level == 0 {
            push_block(geo, map, child, what)?;
        } else {
            walk_table(io, geo, child, level - 1, need, map, what)?;
        }
    }
    Ok(())
}
