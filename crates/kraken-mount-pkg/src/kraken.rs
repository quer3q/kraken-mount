//! The Kraken layout descriptor (`naps_pkg_layout.dat`), walked in chunks into a compact block
//! list. Port of `VF/kraken_image.rs:728-1036` (`describe`) and `FP/mount.rs:35-283` (`KBlock`,
//! `Kraken::new`, `stored`). Blocks may be raw or Kraken-compressed; byte shuffles are refused.
//! A sparse extent (a boundary of kind 0x40 before the mount's end) becomes zero blocks with
//! `stored_len` 0, the way ps5-dump-forge's `describe` reads one.

use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use crate::outer::OuterFile;
use crate::pkg::{corrupt, le64, unsupported};
pub(crate) use crate::decoder::Desc;
use crate::{Error, PRead};

/// A mount block is at most this long, and a record's offsets are relative to this window.
const UBLOCK: u64 = 0x4_0000;
const WINDOW: u64 = 0x4_0000;
/// One Kraken half.
const HALF: u64 = 0x2_0000;
const COMP_KRAKEN: u64 = 2;
/// The largest descriptor accepted (`FP/mount.rs` MAX_OPEN).
const MAX_DESCRIPTOR: u64 = 384 << 20;
/// Records (9 B each) and boundary entries (6 B each) read per chunk.
const CHUNK: usize = 4096;
const REPACK: &str = "repack with --kraken stored";

/// One mount block: `len` logical bytes at `logical`, stored in `stored_len` bytes at
/// `stored_at` in `pfs_image.dat`, decoded with `desc`. `stored_len` 0 (with `stored_at` 0 and a
/// default `desc`): a sparse block, all zeros, that nothing stores.
#[derive(Clone, Copy)]
pub(crate) struct KBlock {
    pub logical: u64,
    pub stored_at: u64,
    pub len: u32,
    pub stored_len: u32,
    pub desc: Desc,
}

impl KBlock {
    /// A sparse block: zeros, nothing stored.
    pub fn sparse(&self) -> bool {
        self.stored_len == 0
    }

    /// Stored as a plain copy of its logical bytes (no decoding needed).
    pub fn raw(&self) -> bool {
        self.stored_len == self.len
            && self.desc.mode & 0b1_1011 == 0
            && u64::from(self.desc.even_len) == u64::from(self.len).min(HALF)
    }
}

/// Mount offset -> stored range in `pfs_image.dat`. 32 B per block (~13 MB per 100 GB).
pub(crate) struct Kraken {
    pub blocks: Vec<KBlock>,
    pub mount_size: u64,
}

/// The descriptor's 9-byte records, read `CHUNK` at a time. Lookups move forward and look at
/// most two records ahead, so a reload starts two records back.
struct Records<'a, R: PRead> {
    io: &'a R,
    file: &'a OuterFile,
    at: u64,
    count: usize,
    start: usize,
    loaded: usize,
    buf: Vec<u8>,
}

impl<R: PRead> Records<'_, R> {
    fn get(&mut self, i: usize) -> Result<u128, Error> {
        if i >= self.count {
            return Err(corrupt(format!("record {i} is past the descriptor's {}", self.count)));
        }
        if i < self.start || i >= self.start + self.loaded {
            self.start = i.saturating_sub(2);
            self.loaded = CHUNK.min(self.count - self.start);
            self.buf.resize(self.loaded * 9, 0);
            let off = self.at + self.start as u64 * 9;
            self.file.read(self.io, off, &mut self.buf)?;
        }
        let o = (i - self.start) * 9;
        let mut x = [0u8; 16];
        x[..9].copy_from_slice(&self.buf[o..o + 9]);
        Ok(u128::from_le_bytes(x))
    }
}

impl Kraken {
    /// The blocks overlapping mount bytes `off..off + len`, in mount order.
    pub fn range(&self, off: u64, len: u64) -> &[KBlock] {
        let end = off + len;
        let first = self.blocks.partition_point(|b| b.logical + u64::from(b.len) <= off);
        let last = self.blocks.partition_point(|b| b.logical < end).max(first);
        &self.blocks[first..last]
    }

    /// The stored (`pfs_image.dat`) ranges `(at, len)` behind mount bytes `off..off + len`,
    /// clipped to that range, in mount order.
    pub fn stored(&self, off: u64, len: u64) -> impl Iterator<Item = (u64, u64)> + '_ {
        let end = off + len;
        let first = self.blocks.partition_point(|b| b.logical + u64::from(b.len) <= off);
        let last = self.blocks.partition_point(|b| b.logical < end).max(first);
        self.blocks[first..last].iter().map(move |b| {
            let s = off.max(b.logical);
            let e = end.min(b.logical + u64::from(b.len));
            (b.stored_at + (s - b.logical), e - s)
        })
    }
}

#[cfg(feature = "probe")]
impl Kraken {
    /// Debug: every block as (logical, len, stored_at, stored_len, even_len, mode bits 32..=36).
    pub fn probe_blocks(&self) -> impl Iterator<Item = (u64, u32, u64, u64, u32, u8)> + '_ {
        self.blocks.iter().map(|b| (b.logical, b.len, b.stored_at, u64::from(b.stored_len), b.desc.even_len, b.desc.mode))
    }
}

impl Kraken {
    /// Walk descriptor `naps` (the way `describe` does) for an image of `image_size` stored
    /// bytes. Raw and compressed blocks are kept; byte-shuffled ones are refused.
    pub fn open<R: PRead>(io: &R, naps: &OuterFile, image_size: u64) -> Result<Self, Error> {
        if naps.size < 16 || naps.size > MAX_DESCRIPTOR {
            return Err(corrupt(format!("layout descriptor of {} bytes", naps.size)));
        }
        let mut head = [0u8; 16];
        naps.read(io, 0, &mut head)?;
        let (w0, w1) = (le64(&head, 0), le64(&head, 8));
        let comp = (w0 >> 24) & 3;
        if comp != COMP_KRAKEN {
            return Err(unsupported(format!(
                "the inner image declares compression type {comp} (not Kraken); {REPACK}"
            )));
        }
        let files = (w0 & 0xFF_FFFF) as usize + 1;
        let ublocks = (w0 >> 32) & 0xFF_FFFF;
        let outer = (w1 & 0xFF_FFFF) as usize;
        let shuffles = ((w0 >> 28) & 0xF) as usize;
        let records = ((w1 >> 24) & 0xFF_FFFF) as usize + 2;
        if shuffles != 0 {
            return Err(unsupported(format!(
                "the layout lists {shuffles} PFSv3 byte shuffle(s); {REPACK}"
            )));
        }
        let fidx_at = 16 + outer * 8;
        let u2c_end = fidx_at + files * 6 + (ublocks / 8 + 1) as usize * 10;
        let rec_at = u2c_end.next_multiple_of(8) + 3;
        if naps.size < (rec_at + records * 9) as u64 {
            return Err(corrupt("layout descriptor shorter than its records"));
        }
        let (bounds, mount, sparse) = boundaries(io, naps, fidx_at, files, ublocks)?;
        let next_bound = |x: u64| {
            bounds.get(bounds.partition_point(|&b| b <= x)).copied().unwrap_or(mount)
        };
        // Where the chain of sparse extents starting at `x` ends (`x` itself when none starts there).
        let hole_end = |mut x: u64| {
            while let Ok(i) = sparse.binary_search_by_key(&x, |s| s.0) {
                x = sparse[i].1;
            }
            x
        };
        // Zero blocks for the sparse extents that start at `logical`, up to the next boundary.
        // No record describes them, so they are filled in before the block record that follows.
        let fill = |logical: &mut u64, blocks: &mut Vec<KBlock>| {
            while let Ok(i) = sparse.binary_search_by_key(&*logical, |s| s.0) {
                let end = sparse[i].1;
                while *logical < end {
                    let len = (end - *logical).min(UBLOCK);
                    blocks.push(KBlock { logical: *logical, stored_at: 0, len: len as u32, stored_len: 0, desc: Desc::default() });
                    *logical += len;
                }
            }
        };
        let mut tag = [0u8; 3];
        naps.read(io, (rec_at - 3) as u64, &mut tag)?;
        let strict = tag == [0x00, 0x00, 0x04];
        let mut rec = Records { io, file: naps, at: rec_at as u64, count: records, start: 0, loaded: 0, buf: Vec::new() };
        let mut blocks: Vec<KBlock> = Vec::with_capacity((ublocks as usize + files).min(1 << 22));
        let (mut cursor, mut logical) = (0u64, 0u64);
        let mut next_is_anchor = true;
        let mut carried_nibble = 0u64;
        for i in 0..records.saturating_sub(1) {
            let v = rec.get(i)?;
            let f = |lo: u32, w: u32| ((v >> lo) & ((1u128 << w) - 1)) as u64;
            let nibble = core::mem::replace(&mut carried_nibble, f(68, 4));
            let anchor = next_is_anchor;
            next_is_anchor = f(66, 1) == 1 && !anchor;
            if anchor {
                cursor = (f(26, 22) << 18) | f(48, 18);
                let terminator = i + 2 == records;
                if strict && i > 0 && !terminator && nibble != (cursor >> 16) & 0xF {
                    return Err(corrupt(format!("record {i}: the anchor's position nibble disagrees")));
                }
                continue;
            }
            fill(&mut logical, &mut blocks);
            if strict && i > 0 && nibble != logical & 0xF {
                return Err(corrupt(format!("record {i}: the exact start's low nibble disagrees")));
            }
            if logical >= mount {
                return Err(corrupt(format!("record {i}: a block starts at {logical:#x}, past the mount's end {mount:#x}")));
            }
            if (logical % WINDOW) >> 4 != f(0, 14) {
                return Err(corrupt(format!("record {i}: logical offset disagrees")));
            }
            let cap = logical + (next_bound(logical).min(mount) - logical).min(UBLOCK);
            let next = if f(66, 1) == 1 { i + 2 } else { i + 1 };
            let len = if next + 1 < records {
                let at = rec.get(next)? as u64 & 0x3FFF;
                // A sparse extent starting at `cap`: the next record starts where it ends, and
                // that window offset says nothing about this block (it can wrap into it).
                let after = hole_end(cap);
                if after != cap && (after % WINDOW) >> 4 == at {
                    cap - logical
                } else {
                    let at16 = (logical & !(WINDOW - 1)) | (at << 4);
                    let at16 = if at16 <= logical & !0xF { at16 + WINDOW } else { at16 };
                    let low = if strict { (rec.get(next - 1)? >> 68) as u64 & 0xF } else { logical & 0xF };
                    let exact = at16 | low;
                    if at16 + 16 <= cap || (strict && exact < cap) { exact - logical } else { cap - logical }
                }
            } else {
                cap - logical
            };
            let mut end = (cursor & !(WINDOW - 1)) | f(48, 18);
            if end < cursor {
                end += WINDOW;
            }
            let even_raw = f(33, 1) == 0 && f(14, 17) + 1 == len.min(HALF);
            let odd_raw = f(36, 1) == 0;
            if end == cursor && even_raw && odd_raw && len > HALF {
                end += WINDOW;
            }
            if end == cursor {
                return Err(corrupt(format!("record {i}: a block stored in no bytes")));
            }
            let stored_len = end - cursor;
            if f(44, 4) != 0 {
                return Err(unsupported(format!("block at mount {logical:#x} is byte-shuffled; {REPACK}")));
            }
            if len == 0 || len > UBLOCK || stored_len > 2 * UBLOCK || cursor.checked_add(stored_len).map_or(true, |e| e > image_size) {
                return Err(corrupt(format!("the block at mount {logical:#x} runs past the stored image")));
            }
            blocks.push(KBlock {
                logical,
                stored_at: cursor,
                len: len as u32,
                stored_len: stored_len as u32,
                desc: Desc { even_len: f(14, 17) as u32 + 1, mode: f(32, 5) as u8 },
            });
            cursor = end;
            logical += len;
        }
        fill(&mut logical, &mut blocks);
        if logical != mount {
            return Err(corrupt(format!("blocks tile to {logical:#x}, the mount is {mount:#x}")));
        }
        blocks.shrink_to_fit();
        Ok(Self { blocks, mount_size: mount })
    }
}

/// The descriptor's boundary table (`fidx`, 6 B per entry: 40-bit mount offset, kind byte),
/// read in chunks: the sorted, deduplicated boundaries, the mount's end (the last entry) and the
/// sparse extents `(start, end)`, sorted by start. A kind-0x40 boundary before the end opens a
/// sparse extent that runs to the next boundary; nothing is stored for it.
fn boundaries<R: PRead>(
    io: &R,
    naps: &OuterFile,
    fidx_at: usize,
    files: usize,
    ublocks: u64,
) -> Result<(Vec<u64>, u64, Vec<(u64, u64)>), Error> {
    let mut bounds = Vec::with_capacity(files);
    let mut holes: Vec<u64> = Vec::new();
    let mut buf = vec![0u8; CHUNK * 6];
    let mut done = 0usize;
    while done < files {
        let n = CHUNK.min(files - done);
        let chunk = &mut buf[..n * 6];
        naps.read(io, (fidx_at + done * 6) as u64, chunk)?;
        for r in chunk.chunks_exact(6) {
            let off = u64::from_le_bytes([r[0], r[1], r[2], r[3], r[4], 0, 0, 0]);
            if r[5] == 0x40 {
                holes.push(off);
            }
            bounds.push(off);
        }
        done += n;
    }
    let mount = bounds.last().copied().unwrap_or(0);
    if mount.div_ceil(UBLOCK) != ublocks || bounds.iter().any(|&b| b > mount) {
        return Err(corrupt("the boundary table disagrees with the mount's size"));
    }
    bounds.sort_unstable();
    bounds.dedup();
    bounds.shrink_to_fit();
    // The mount's end carries kind 0x40 too; only one before it is a hole. The mount's size
    // matching the ublock count (above) bounds the zero blocks a forged table can ask for.
    // ponytail: a hole is taken to run to the next boundary (as ps5-dump-forge reads it); a
    // layout where it ends elsewhere would tile wrongly and be refused by the checks in `open`.
    let mut sparse: Vec<(u64, u64)> = holes
        .into_iter()
        .filter(|&a| a < mount)
        .map(|a| (a, bounds.get(bounds.partition_point(|&b| b <= a)).copied().unwrap_or(mount)))
        .collect();
    sparse.sort_unstable();
    sparse.dedup_by_key(|s| s.0);
    Ok((bounds, mount, sparse))
}
