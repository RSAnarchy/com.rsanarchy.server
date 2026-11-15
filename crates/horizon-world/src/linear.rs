//! Linear region file format (by Xymb/StupidCraft).
//!
//! File layout (all big-endian):
//!
//! ```text
//! [32-byte superblock]
//!   u64  signature  = LINEAR_SIGNATURE
//!   u8   version    = LINEAR_VERSION (1)
//!   u64  newest_timestamp
//!   i8   compression_level
//!   i16  chunk_count
//!   u32  complete_region_length  (length of zstd blob below)
//!   u64  hash64 (8 bytes, reserved/zero)
//!
//! [complete_region_length bytes of zstd-compressed payload]
//!   Inside the zstd stream:
//!   [32*32*8 = 8192 bytes chunk header table]
//!     for each of 1024 chunks (row-major: z*32+x):
//!       u32  size       (0 = chunk absent)
//!       u32  timestamp
//!   [chunk data, concatenated]
//!     for each chunk where size > 0: raw NBT bytes (uncompressed)
//!
//! [8-byte footer]
//!   u64  signature  = LINEAR_SIGNATURE
//! ```
//!
//! Files are named `r.X.Z.linear`.

use crate::{
    compression::{Compression, invalid},
    region::{ChunkRecord, RegionFile},
};
use horizon_core::position::ChunkPos;
use std::{
    fs,
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

pub const LINEAR_SIGNATURE: u64 = 0xc3ff13183cca9d9a;
pub const LINEAR_VERSION: u8 = 1;

/// Number of chunks per region axis (32) and total (1024).
const REGION_DIM: usize = 32;
const CHUNK_COUNT: usize = REGION_DIM * REGION_DIM; // 1024
/// Size of the per-chunk entry in the payload header: u32 size + u32 timestamp.
const CHUNK_ENTRY: usize = 8;
/// Total size of the in-payload chunk header table (8192 bytes).
const PAYLOAD_HEADER: usize = CHUNK_COUNT * CHUNK_ENTRY;

/// Superblock size on disk (bytes before the zstd blob).
const SUPERBLOCK: usize = 32;
/// Footer size on disk (bytes after the zstd blob).
const FOOTER: usize = 8;

// ── Superblock layout offsets ─────────────────────────────────────────────────
// 0..8   signature (u64)
// 8      version   (u8)
// 9..17  newest_timestamp (u64)
// 17     compression_level (i8)
// 18..20 chunk_count (i16)
// 20..24 complete_region_length (u32)
// 24..32 hash64 (u64, reserved)

// ─────────────────────────────────────────────────────────────────────────────
// Path helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Parse region coordinates from a `r.X.Z.linear` filename.
pub fn linear_position_from_path(path: &Path) -> io::Result<ChunkPos> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("invalid linear region filename"))?;
    let parts: Vec<_> = name.split('.').collect();
    if parts.len() != 4 || parts[0] != "r" || parts[3] != "linear" {
        return Err(invalid("expected r.<x>.<z>.linear"));
    }
    let x: i32 = parts[1]
        .parse()
        .map_err(|_| invalid("invalid region x coordinate"))?;
    let z: i32 = parts[2]
        .parse()
        .map_err(|_| invalid("invalid region z coordinate"))?;
    if !(-67_108_864..=67_108_863).contains(&x) || !(-67_108_864..=67_108_863).contains(&z) {
        return Err(invalid("region coordinate outside chunk coordinate range"));
    }
    Ok(ChunkPos::new(x, z))
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Translate a chunk position to its slot index (0..1024) within the region.
/// Returns an error if the chunk does not belong to this region.
fn slot(region: ChunkPos, chunk: ChunkPos) -> io::Result<usize> {
    if chunk.region() != region {
        return Err(invalid("chunk does not belong to this region"));
    }
    let (x, z) = chunk.region_local();
    // Row-major: z*32 + x (matches the spec).
    Ok(usize::from(z) * REGION_DIM + usize::from(x))
}

/// Inverse of `slot`: slot index → ChunkPos.
fn chunk_at(region: ChunkPos, index: usize) -> ChunkPos {
    ChunkPos::new(
        region.x * REGION_DIM as i32 + (index % REGION_DIM) as i32,
        region.z * REGION_DIM as i32 + (index / REGION_DIM) as i32,
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// LinearRegionFile — reader
// ─────────────────────────────────────────────────────────────────────────────

/// An in-memory view of a `.linear` region file.
///
/// The whole file is decompressed once on `open`; subsequent `read` calls are
/// pure slice operations (no I/O, no allocation beyond the returned `Vec`).
pub struct LinearRegionFile {
    position: ChunkPos,
    /// sizes[i] = raw chunk byte size (0 → absent).
    sizes: [u32; CHUNK_COUNT],
    timestamps: [u32; CHUNK_COUNT],
    /// Concatenated raw NBT data for every present chunk, in slot order.
    data: Vec<u8>,
}

impl LinearRegionFile {
    /// Open and fully parse a `.linear` file.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let position = linear_position_from_path(path)?;
        let raw = fs::read(path)?;

        if raw.len() < SUPERBLOCK + FOOTER {
            return Err(invalid("linear file too short"));
        }

        // ── Verify header signature ───────────────────────────────────────
        let sig = u64::from_be_bytes(raw[0..8].try_into().unwrap());
        if sig != LINEAR_SIGNATURE {
            return Err(invalid("linear file: bad header signature"));
        }

        let version = raw[8];
        if version != LINEAR_VERSION {
            return Err(invalid(format!(
                "linear file: unsupported version {version}"
            )));
        }

        let blob_len =
            u32::from_be_bytes(raw[20..24].try_into().unwrap()) as usize;

        if SUPERBLOCK + blob_len + FOOTER != raw.len() {
            return Err(invalid("linear file: length does not match superblock"));
        }

        // ── Verify footer signature ───────────────────────────────────────
        let footer_start = SUPERBLOCK + blob_len;
        let footer_sig =
            u64::from_be_bytes(raw[footer_start..footer_start + 8].try_into().unwrap());
        if footer_sig != LINEAR_SIGNATURE {
            return Err(invalid("linear file: bad footer signature"));
        }

        // ── Decompress payload ────────────────────────────────────────────
        let blob = &raw[SUPERBLOCK..SUPERBLOCK + blob_len];
        let payload =
            zstd::decode_all(blob).map_err(|e| invalid(format!("linear zstd: {e}")))?;

        if payload.len() < PAYLOAD_HEADER {
            return Err(invalid("linear payload too short for chunk header table"));
        }

        // ── Parse chunk header table ──────────────────────────────────────
        let mut sizes = [0u32; CHUNK_COUNT];
        let mut timestamps = [0u32; CHUNK_COUNT];
        for i in 0..CHUNK_COUNT {
            let base = i * CHUNK_ENTRY;
            sizes[i] = u32::from_be_bytes(payload[base..base + 4].try_into().unwrap());
            timestamps[i] =
                u32::from_be_bytes(payload[base + 4..base + 8].try_into().unwrap());
        }

        let data = payload[PAYLOAD_HEADER..].to_vec();

        // Sanity-check: data length must equal the sum of sizes.
        let total: u64 = sizes.iter().map(|&s| u64::from(s)).sum();
        if total != data.len() as u64 {
            return Err(invalid(
                "linear payload: chunk data length does not match size table",
            ));
        }

        Ok(Self {
            position,
            sizes,
            timestamps,
            data,
        })
    }

    /// The region's origin chunk position (coordinates of chunk slot 0).
    pub fn position(&self) -> ChunkPos {
        self.position
    }

    /// Return the raw NBT record for `chunk`, or `None` if absent.
    ///
    /// The returned `ChunkRecord` uses `Compression::None`; `compressed` holds
    /// the raw (already uncompressed) NBT bytes as stored in the linear file.
    pub fn read(&self, chunk: ChunkPos) -> io::Result<Option<ChunkRecord>> {
        let i = slot(self.position, chunk)?;
        if self.sizes[i] == 0 {
            return Ok(None);
        }
        // Compute byte offset for slot i.
        let offset: usize = self.sizes[..i].iter().map(|&s| s as usize).sum();
        let len = self.sizes[i] as usize;
        let bytes = self
            .data
            .get(offset..offset + len)
            .ok_or_else(|| invalid("linear: chunk data out of bounds"))?;
        Ok(Some(ChunkRecord {
            compression: Compression::None,
            compressed: bytes.to_vec(),
            timestamp: self.timestamps[i],
        }))
    }

    /// Iterate over `(ChunkPos, timestamp)` for every chunk present in this
    /// region file.
    pub fn chunks(&self) -> impl Iterator<Item = (ChunkPos, u32)> + '_ {
        self.sizes
            .iter()
            .enumerate()
            .filter(|&(_, &sz)| sz != 0)
            .map(|(i, _)| (chunk_at(self.position, i), self.timestamps[i]))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// LinearWriter — writer
// ─────────────────────────────────────────────────────────────────────────────

static NEXT_TEMP: AtomicU32 = AtomicU32::new(0);

/// Collects chunks in memory, then writes a complete `.linear` file atomically
/// (via a `.wip` sibling that is renamed on `finish`).
pub struct LinearWriter {
    destination: PathBuf,
    wip: PathBuf,
    position: ChunkPos,
    compression_level: i32,
    sizes: [u32; CHUNK_COUNT],
    timestamps: [u32; CHUNK_COUNT],
    /// Raw NBT data for each present chunk, in slot order.
    chunks_data: Vec<Option<Vec<u8>>>,
    committed: bool,
}

impl LinearWriter {
    /// Begin writing a new region.  Chunk data is buffered in memory until
    /// `finish` is called.
    ///
    /// `destination` must be a valid `r.X.Z.linear` path.  The file is
    /// **not** created until `finish`.
    pub fn create(destination: impl AsRef<Path>, compression_level: i8) -> io::Result<Self> {
        let destination = destination.as_ref().to_owned();
        let position = linear_position_from_path(&destination)?;

        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;

        let wip = parent.join(format!(
            ".horizon-linear-{}-{}.wip",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));

        // Clamp to zstd's accepted range to avoid panics at finish time.
        let compression_level = i32::from(compression_level);

        let mut chunks_data = Vec::with_capacity(CHUNK_COUNT);
        chunks_data.resize_with(CHUNK_COUNT, || None);

        Ok(Self {
            destination,
            wip,
            position,
            compression_level,
            sizes: [0; CHUNK_COUNT],
            timestamps: [0; CHUNK_COUNT],
            chunks_data,
            committed: false,
        })
    }

    /// Buffer `record` for `chunk`.
    ///
    /// If the record is compressed (gzip / zlib / lz4), the raw NBT bytes are
    /// decoded first, because linear stores uncompressed NBT.
    ///
    /// Writing the same chunk twice is an error.
    pub fn write(&mut self, chunk: ChunkPos, record: &ChunkRecord) -> io::Result<()> {
        let i = slot(self.position, chunk)?;
        if self.sizes[i] != 0 {
            return Err(invalid("linear writer: chunk already written"));
        }

        // Decode to raw NBT if needed.
        let raw = if record.compression == Compression::None {
            record.compressed.clone()
        } else {
            record.compression.decode(&record.compressed, usize::MAX)?
        };

        self.sizes[i] = u32::try_from(raw.len())
            .map_err(|_| invalid("linear writer: chunk too large"))?;
        self.timestamps[i] = record.timestamp;
        self.chunks_data[i] = Some(raw);
        Ok(())
    }

    /// Compress the payload with zstd, write the complete `.linear` file to the
    /// `.wip` sibling, then rename it over `destination`.
    pub fn finish(mut self) -> io::Result<()> {
        // ── Build uncompressed payload ────────────────────────────────────
        let mut payload = Vec::with_capacity(PAYLOAD_HEADER);

        // Chunk header table.
        for i in 0..CHUNK_COUNT {
            payload.extend_from_slice(&self.sizes[i].to_be_bytes());
            payload.extend_from_slice(&self.timestamps[i].to_be_bytes());
        }

        // Chunk data (slot order, only present chunks).
        for chunk in self.chunks_data.iter().flatten() {
            payload.extend_from_slice(chunk);
        }

        // ── Compute superblock fields ─────────────────────────────────────
        let newest_timestamp: u64 = self
            .timestamps
            .iter()
            .map(|&t| u64::from(t))
            .max()
            .unwrap_or(0);
        let chunk_count: i16 = self
            .sizes
            .iter()
            .filter(|&&s| s != 0)
            .count()
            .try_into()
            .unwrap_or(i16::MAX);
        let compression_level_byte = self.compression_level.clamp(i8::MIN as i32, i8::MAX as i32) as i8;

        // ── Compress payload with zstd ────────────────────────────────────
        let blob = zstd::encode_all(payload.as_slice(), self.compression_level)
            .map_err(|e| invalid(format!("linear zstd encode: {e}")))?;
        let blob_len = u32::try_from(blob.len())
            .map_err(|_| invalid("linear: compressed payload too large for u32"))?;

        // ── Assemble file ─────────────────────────────────────────────────
        let total = SUPERBLOCK + blob.len() + FOOTER;
        let mut file_bytes: Vec<u8> = Vec::with_capacity(total);

        // Superblock.
        file_bytes.extend_from_slice(&LINEAR_SIGNATURE.to_be_bytes()); // 0..8
        file_bytes.push(LINEAR_VERSION); // 8
        file_bytes.extend_from_slice(&newest_timestamp.to_be_bytes()); // 9..17
        file_bytes.push(compression_level_byte as u8); // 17  (i8 bit pattern)
        file_bytes.extend_from_slice(&chunk_count.to_be_bytes()); // 18..20
        file_bytes.extend_from_slice(&blob_len.to_be_bytes()); // 20..24
        file_bytes.extend_from_slice(&0u64.to_be_bytes()); // 24..32  hash64 reserved

        // zstd blob.
        file_bytes.extend_from_slice(&blob);

        // Footer.
        file_bytes.extend_from_slice(&LINEAR_SIGNATURE.to_be_bytes());

        debug_assert_eq!(file_bytes.len(), total);

        // ── Write .wip then rename ────────────────────────────────────────
        fs::write(&self.wip, &file_bytes)?;
        fs::rename(&self.wip, &self.destination)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for LinearWriter {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.wip);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// LinearRegion — editable region used by the save layer
// ─────────────────────────────────────────────────────────────────────────────

/// zstd level for region files the server writes.
pub const DEFAULT_LEVEL: i8 = 3;

/// An editable `.linear` region, the counterpart of `RegionFile` for the save
/// layer. Edits are held in memory; `sync` rewrites the whole file atomically
/// (via the `.wip` sibling) when anything changed.
pub struct LinearRegion {
    path: PathBuf,
    position: ChunkPos,
    level: i8,
    /// Per slot: (timestamp, raw NBT bytes), or None for an absent chunk.
    slots: Vec<Option<(u32, Vec<u8>)>>,
    dirty: bool,
}

impl LinearRegion {
    /// Open the region at `path`. A missing file is an empty region when
    /// `create` is set, and `NotFound` otherwise.
    pub fn open(path: impl AsRef<Path>, create: bool) -> io::Result<Self> {
        let path = path.as_ref().to_owned();
        let position = linear_position_from_path(&path)?;
        let mut slots: Vec<Option<(u32, Vec<u8>)>> = vec![None; CHUNK_COUNT];
        if path.exists() {
            let file = LinearRegionFile::open(&path)?;
            for (pos, timestamp) in file.chunks() {
                let i = slot(position, pos)?;
                let record = file
                    .read(pos)?
                    .ok_or_else(|| invalid("linear: listed chunk is missing"))?;
                slots[i] = Some((timestamp, record.compressed));
            }
        } else if !create {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        Ok(Self {
            path,
            position,
            level: DEFAULT_LEVEL,
            slots,
            dirty: false,
        })
    }

    /// The region's origin chunk position.
    pub fn position(&self) -> ChunkPos {
        self.position
    }

    /// The chunk's raw NBT record, or `None` if absent.
    pub fn read(&self, chunk: ChunkPos) -> io::Result<Option<ChunkRecord>> {
        let i = slot(self.position, chunk)?;
        Ok(self.slots[i].as_ref().map(|(timestamp, bytes)| ChunkRecord {
            compression: Compression::None,
            compressed: bytes.clone(),
            timestamp: *timestamp,
        }))
    }

    /// `(position, timestamp)` for every present chunk.
    pub fn chunks(&self) -> impl Iterator<Item = (ChunkPos, u32)> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter_map(move |(i, s)| s.as_ref().map(|(t, _)| (chunk_at(self.position, i), *t)))
    }

    /// Store `record` for `chunk`, decoding it to raw NBT first if compressed.
    pub fn write(&mut self, chunk: ChunkPos, record: &ChunkRecord) -> io::Result<()> {
        let i = slot(self.position, chunk)?;
        let bytes = if record.compression == Compression::None {
            record.compressed.clone()
        } else {
            record.compression.decode(&record.compressed, usize::MAX)?
        };
        self.slots[i] = Some((record.timestamp, bytes));
        self.dirty = true;
        Ok(())
    }

    /// Remove `chunk`. Returns whether it was present. The timestamp is not
    /// stored for removed chunks, since the format has no free-slot table.
    pub fn clear(&mut self, chunk: ChunkPos, _timestamp: u32) -> io::Result<bool> {
        let i = slot(self.position, chunk)?;
        let was_present = self.slots[i].take().is_some();
        self.dirty |= was_present;
        Ok(was_present)
    }

    /// Rewrite the file if any chunk changed since it was opened or last synced.
    pub fn sync(&mut self) -> io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let mut writer = LinearWriter::create(&self.path, self.level)?;
        for (i, s) in self.slots.iter().enumerate() {
            if let Some((timestamp, bytes)) = s {
                writer.write(
                    chunk_at(self.position, i),
                    &ChunkRecord {
                        compression: Compression::None,
                        compressed: bytes.clone(),
                        timestamp: *timestamp,
                    },
                )?;
            }
        }
        writer.finish()?;
        self.dirty = false;
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// One-time conversion from Anvil (.mca)
// ─────────────────────────────────────────────────────────────────────────────

/// Convert one `.mca` region to the `.linear` file at `linear`. Returns the
/// number of chunks written. Any unreadable chunk fails the whole conversion,
/// and nothing is written to `linear`.
pub fn convert_mca(mca: &Path, linear: &Path, level: i8) -> io::Result<usize> {
    let mut region = RegionFile::open(mca)?;
    let present: Vec<ChunkPos> = region.chunks().map(|(pos, _)| pos).collect();
    let mut writer = LinearWriter::create(linear, level)?;
    let mut written = 0;
    for pos in present {
        if let Some(record) = region.read(pos)? {
            writer.write(pos, &record)?;
            written += 1;
        }
    }
    writer.finish()?;
    Ok(written)
}

/// The folders of a dimension that hold region-style files.
pub const REGION_FOLDERS: [&str; 3] = ["region", "entities", "poi"];

/// Name of the folder that keeps the `.mca` originals after conversion.
pub const MCA_BACKUP_DIR: &str = "mca-backup";

/// Convert every `.mca` in one dimension directory to `.linear`, then move the
/// `.mca` into `mca-backup/<folder>/`. A `.mca` whose `.linear` already exists
/// is only moved, so an interrupted run resumes cleanly. Returns the number of
/// regions converted.
///
/// Any failure returns an error and leaves the failing `.mca` in place, so the
/// caller can refuse to open the world rather than run on a partial copy.
pub fn convert_dimension(dimension_dir: &Path, level: i8) -> io::Result<usize> {
    let mut converted = 0;
    for folder in REGION_FOLDERS {
        let dir = dimension_dir.join(folder);
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry?.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".mca") else {
                continue;
            };
            if !path.is_file() || crate::region::position_from_path(&path).is_err() {
                continue;
            }
            let linear = dir.join(format!("{stem}.linear"));
            if !linear.exists() {
                convert_mca(&path, &linear, level).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!("could not convert {}: {e}", path.display()),
                    )
                })?;
                converted += 1;
            }
            let backup = dimension_dir.join(MCA_BACKUP_DIR).join(folder);
            fs::create_dir_all(&backup)?;
            fs::rename(&path, backup.join(name))?;
        }
    }
    Ok(converted)
}
