//! The FIH header, the CNT container's entry table, and `Image`: everything `Pkg::open` parses
//! before the tree walk. Port of `FP/pfs.rs:92-257` and `FP/lib.rs:164-290`.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::kraken::{KBlock, Kraken};
use crate::outer::{self, OuterFile};
use crate::{Error, Extent, PRead};

/// Outer PFS block size.
pub(crate) const BLOCK: u64 = 0x10000;
const FIH_LEN: usize = 0x100;
const CNT_HEADER: usize = 0x1000;
const CNT_MAGIC: u32 = 0x7F43_4E54;
const CNT_ENTRY_LEN: usize = 0x20;
const MAX_CNT_ENTRIES: u32 = 4096;

pub(crate) fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub(crate) fn le64(b: &[u8], at: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(x)
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

pub(crate) fn corrupt(m: impl Into<String>) -> Error {
    Error::Corrupt(m.into())
}

pub(crate) fn unsupported(m: impl Into<String>) -> Error {
    Error::Unsupported(m.into())
}

/// Exactly `buf.len()` bytes at `off`, or a `Corrupt` error naming the truncation.
pub(crate) fn pread_exact<R: PRead>(io: &R, off: u64, buf: &mut [u8]) -> Result<(), Error> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = io.pread(off + done as u64, &mut buf[done..])?;
        if n == 0 {
            return Err(corrupt(format!(
                "a read of {} bytes at {off:#x} hit the end of the file (truncated?)",
                buf.len()
            )));
        }
        done += n;
    }
    Ok(())
}

/// Buffers for decoding compressed blocks, and the last block decoded (the tree walk reads
/// many small pieces of the same metadata block).
pub(crate) struct Decoder {
    scratch: Option<Box<crate::decoder::Scratch>>,
    src: Vec<u8>,
    dst: Vec<u8>,
    /// Mount offset of the block whose logical bytes are in `dst`.
    cached: Option<u64>,
}

impl Decoder {
    pub fn new() -> Self {
        Decoder { scratch: None, src: Vec::new(), dst: Vec::new(), cached: None }
    }
}

/// What the FIH header says about where everything is.
pub(crate) struct Header {
    pub pfs_offset: u64,
    pub outer_blocks: u64,
    pub sb_index: u64,
    pub cnt_offset: u64,
    /// The inner mount's metadata base, in bytes.
    pub meta_base: u64,
}

pub(crate) fn header<R: PRead>(io: &R, len: u64) -> Result<Header, Error> {
    if len < FIH_LEN as u64 {
        return Err(Error::NotPs5(format!("{len} bytes, shorter than a package header")));
    }
    let mut head = [0u8; FIH_LEN];
    pread_exact(io, 0, &mut head)?;
    match &head[0..4] {
        b"\x7FFIH" => {}
        b"\x7FCNT" => return Err(Error::NotPs5("a PS4 package (\\x7FCNT at offset 0)".into())),
        b"\x7FLIH" => {
            return Err(unsupported(
                "a LIH patch package; only full dump-forge packages are read",
            ))
        }
        _ => return Err(Error::NotPs5("no \\x7FFIH magic at offset 0".into())),
    }
    if head[5] != 0 {
        return Err(unsupported(format!(
            "a retail package (signed byte {:#04x}, debug packages carry 0x00)",
            head[5]
        )));
    }
    let (pfs_offset, pfs_size) = (le64(&head, 0x10), le64(&head, 0x18));
    if pfs_offset < FIH_LEN as u64 || pfs_offset % BLOCK != 0 {
        return Err(corrupt(format!("outer image offset {pfs_offset:#x}")));
    }
    if pfs_size == 0 || pfs_size % BLOCK != 0 {
        return Err(corrupt(format!("outer image size {pfs_size:#x} is not whole blocks")));
    }
    if pfs_offset.checked_add(pfs_size).map_or(true, |e| e > len) {
        return Err(corrupt(format!(
            "the outer image ({pfs_size:#x} bytes at {pfs_offset:#x}) runs past the end {len:#x} (truncated?)"
        )));
    }
    let outer_blocks = pfs_size / BLOCK;
    if outer_blocks > u64::from(u32::MAX) {
        return Err(corrupt(format!("{outer_blocks} outer blocks")));
    }
    let sb_abs = le64(&head, 0x20);
    let sb_index = sb_abs
        .checked_sub(pfs_offset)
        .filter(|r| r % BLOCK == 0)
        .map(|r| r / BLOCK)
        .filter(|&i| i < outer_blocks)
        .ok_or_else(|| corrupt(format!("superblock offset {sb_abs:#x} is outside the outer image")))?;
    let cnt_offset = le64(&head, 0x58);
    if cnt_offset.checked_add(CNT_HEADER as u64).map_or(true, |e| e > len) {
        return Err(corrupt(format!("container offset {cnt_offset:#x} is past the end {len:#x}")));
    }
    if le64(&head, 0x60) != BLOCK {
        return Err(corrupt(format!("header block size {:#x}", le64(&head, 0x60))));
    }
    Ok(Header {
        pfs_offset,
        outer_blocks,
        sb_index,
        cnt_offset,
        meta_base: u64::from(le32(&head, 0x50)) * BLOCK,
    })
}

/// One container entry, its payload located in the package.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub(crate) struct CntEntry {
    pub id: u32,
    /// `flags1 & 0x8000_0000`: an encrypted entry, not a plain copy of anything.
    pub flags1: u32,
    /// Absolute offset in the package.
    pub at: u64,
    pub size: u64,
}

#[allow(dead_code)]
pub(crate) struct Container {
    pub content_id: String,
    pub entries: Vec<CntEntry>,
}

impl Container {
    #[allow(dead_code)]
    pub fn entry(&self, id: u32) -> Option<CntEntry> {
        self.entries.iter().copied().find(|e| e.id == id)
    }
}

/// The container's header and entry table; payloads stay on disk.
pub(crate) fn container<R: PRead>(io: &R, len: u64, cnt_offset: u64) -> Result<Container, Error> {
    let mut head = vec![0u8; CNT_HEADER];
    pread_exact(io, cnt_offset, &mut head)?;
    if be32(&head, 0) != CNT_MAGIC {
        return Err(corrupt(format!("no \\x7FCNT container at {cnt_offset:#x}")));
    }
    let count = be32(&head, 0x10);
    if count > MAX_CNT_ENTRIES {
        return Err(corrupt(format!("the container claims {count} entries (at most {MAX_CNT_ENTRIES})")));
    }
    let table_at = cnt_offset + u64::from(be32(&head, 0x18));
    let mut rows = vec![0u8; count as usize * CNT_ENTRY_LEN];
    pread_exact(io, table_at, &mut rows)?;
    let mut entries = Vec::with_capacity(count as usize);
    for row in rows.chunks_exact(CNT_ENTRY_LEN) {
        let id = be32(row, 0);
        let at = cnt_offset + u64::from(be32(row, 16));
        let size = u64::from(be32(row, 20));
        if at.checked_add(size).map_or(true, |e| e > len) {
            return Err(corrupt(format!("container entry {id:#06x} runs past the end (truncated?)")));
        }
        entries.push(CntEntry { id, flags1: be32(row, 8), at, size });
    }
    let id = &head[0x40..0x64];
    let id_len = id.iter().position(|&b| b == 0).unwrap_or(id.len());
    let content_id = core::str::from_utf8(&id[..id_len]).unwrap_or("").to_string();
    Ok(Container { content_id, entries })
}

/// The parsed package up to the block mapping: where the inner image's bytes are in the `.pkg`.
#[allow(dead_code)]
pub(crate) struct Image {
    pub pfs_offset: u64,
    /// The inner mount's metadata base (superblock), in mount bytes.
    pub meta_base: u64,
    pub cnt: Container,
    /// `pfs_image.dat`: inner-image (stored) offset -> outer block.
    pub image: OuterFile,
    /// Mount offset -> stored range in `pfs_image.dat`.
    pub kraken: Kraken,
    /// Debug only (`probe`): the whole decoded mount, served by `read_mount` when set.
    #[cfg(feature = "probe")]
    pub probe_mount: Option<Vec<u8>>,
}

impl Image {
    pub fn open<R: PRead>(io: &R, len: u64) -> Result<Image, Error> {
        let h = header(io, len)?;
        let cnt = container(io, len, h.cnt_offset)?;
        let geo = outer::Geometry { pfs_offset: h.pfs_offset, blocks: h.outer_blocks };
        let sb = outer::superblock(io, &geo, h.sb_index)?;
        if u64::from(sb.inode_table_block) == h.sb_index {
            return Err(corrupt("the outer inode table is the superblock"));
        }
        let mut table = vec![0u8; BLOCK as usize];
        pread_exact(io, geo.block_off(u64::from(sb.inode_table_block))?, &mut table)?;
        let nodes = outer::parse_dinodes(&table, sb.dinode_count);
        drop(table);
        let uroot = outer::child(io, &geo, &nodes, 0, "uroot")?;
        let image_ino = outer::child(io, &geo, &nodes, uroot, "pfs_image.dat")?;
        let naps_ino = outer::child(io, &geo, &nodes, uroot, "naps_pkg_layout.dat")?;
        let image = OuterFile::map(io, &geo, &nodes[image_ino], "pfs_image.dat")?;
        let naps = OuterFile::map(io, &geo, &nodes[naps_ino], "naps_pkg_layout.dat")?;
        drop(nodes);
        let kraken = Kraken::open(io, &naps, image.size)?;
        Ok(Image {
            pfs_offset: h.pfs_offset,
            meta_base: h.meta_base,
            cnt,
            image,
            kraken,
            #[cfg(feature = "probe")]
            probe_mount: None,
        })
    }

    /// Mount size in bytes (the inner filesystem's logical size).
    #[allow(dead_code)]
    pub fn mount_size(&self) -> u64 {
        self.kraken.mount_size
    }

    fn check_range(&self, off: u64, len: u64) -> Result<(), Error> {
        if off.checked_add(len).map_or(true, |e| e > self.kraken.mount_size) {
            return Err(corrupt(format!(
                "mount range {off:#x}+{len:#x} runs past the mount's {:#x}",
                self.kraken.mount_size
            )));
        }
        Ok(())
    }

    /// Append the `.pkg` byte runs behind mount bytes `off..off + len` to `out`, in order,
    /// merging a run into the previous one (also one already in `out`) when they touch.
    /// `Unsupported` when a block in the range is compressed or sparse (its bytes are not plaintext).
    pub fn extents(&self, off: u64, len: u64, out: &mut Vec<Extent>) -> Result<(), Error> {
        self.check_range(off, len)?;
        if let Some(b) = self.kraken.range(off, len).iter().find(|b| !b.raw()) {
            let what = if b.sparse() { "sparse" } else { "Kraken-compressed" };
            return Err(unsupported(format!("the block at mount {:#x} is {what}", b.logical)));
        }
        for (at, n) in self.kraken.stored(off, len) {
            self.image.pkg_extents(at, n, out)?;
        }
        Ok(())
    }

    /// Read block `b`'s stored bytes and decode them into `dec.dst` (kept as a one-block cache).
    fn decode<R: PRead>(&self, io: &R, b: &KBlock, dec: &mut Decoder) -> Result<(), Error> {
        if dec.cached == Some(b.logical) {
            return Ok(());
        }
        dec.cached = None;
        let mut runs = Vec::new();
        self.image.pkg_extents(b.stored_at, u64::from(b.stored_len), &mut runs)?;
        dec.src.resize(b.stored_len as usize, 0);
        let mut done = 0usize;
        for r in runs {
            let n = r.len as usize;
            pread_exact(io, r.pkg_off, &mut dec.src[done..done + n])?;
            done += n;
        }
        dec.dst.resize(b.len as usize, 0);
        let scratch = dec.scratch.get_or_insert_with(crate::decoder::Scratch::new_boxed);
        crate::decoder::decode_block(&dec.src, b.desc, &mut dec.dst, scratch)
            .map_err(|e| corrupt(format!("the block at mount {:#x}: {e}", b.logical)))?;
        dec.cached = Some(b.logical);
        Ok(())
    }

    /// Exactly `buf.len()` mount bytes at `off`: raw blocks are read in place, compressed ones
    /// decoded through `dec`.
    pub fn read_mount<R: PRead>(&self, io: &R, off: u64, buf: &mut [u8], dec: &mut Decoder) -> Result<(), Error> {
        #[cfg(feature = "probe")]
        if let Some(m) = &self.probe_mount {
            let src = m.get(off as usize..off as usize + buf.len()).ok_or_else(|| corrupt("past the decoded mount"))?;
            buf.copy_from_slice(src);
            return Ok(());
        }
        let len = buf.len() as u64;
        self.check_range(off, len)?;
        let mut done = 0usize;
        let mut runs = Vec::new();
        for b in self.kraken.range(off, len) {
            let inner = (off + done as u64 - b.logical) as usize;
            let n = (b.len as usize - inner).min(buf.len() - done);
            let out = &mut buf[done..done + n];
            if b.raw() {
                runs.clear();
                self.image.pkg_extents(b.stored_at + inner as u64, n as u64, &mut runs)?;
                let mut at = 0usize;
                for r in &runs {
                    let k = r.len as usize;
                    pread_exact(io, r.pkg_off, &mut out[at..at + k])?;
                    at += k;
                }
            } else {
                self.decode(io, b, dec)?;
                out.copy_from_slice(&dec.dst[inner..inner + n]);
            }
            done += n;
        }
        if done != buf.len() {
            return Err(corrupt(format!("mount range {off:#x}+{len:#x} is not covered by blocks")));
        }
        Ok(())
    }
}
