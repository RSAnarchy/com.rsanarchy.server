//! Linear v2 region buckets containing uncompressed chunk NBT.
use crate::compression::invalid;
use crate::region::RegionFile;
use horizon_core::position::ChunkPos;
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::Path,
    sync::Mutex,
};
use xxhash_rust::xxh64::xxh64;

mod shulkers;

const MAGIC: [u8; 8] = 0xc3ff13183cca9d9au64.to_be_bytes();
const GRID: usize = 8;
const LEVEL: i32 = 1;
const MAX_CHUNK: usize = 64 * 1024 * 1024;
const MAX_BUCKET: usize = 512 * 1024 * 1024;
const MAX_CACHED_BUCKET: usize = 4 * 1024 * 1024;
type BucketEntries = Vec<(u64, Option<Vec<u8>>)>;

pub fn position_from_path(path: &Path) -> io::Result<ChunkPos> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("linear") {
        return Err(invalid("expected a .linear region filename"));
    }
    crate::region::position_from_path(&path.with_extension("mca"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub timestamp: u64,
    pub nbt: Vec<u8>,
}

pub struct Region {
    pub x: i32,
    pub z: i32,
    grid: usize,
    features: Vec<u8>,
    bitmap: [u8; 128],
    newest: u64,
    buckets: Vec<Bucket>,
    last_bucket: Mutex<Option<(usize, BucketEntries)>>,
}

struct Bucket {
    level: u8,
    compressed: Vec<u8>,
}

fn take<const N: usize>(input: &mut &[u8]) -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

impl Region {
    pub fn open(path: &Path, position: ChunkPos) -> io::Result<Self> {
        let region = Self::decode(&fs::read(path)?)?;
        if (region.x, region.z) != (position.x, position.z) {
            return Err(invalid(
                "Linear region coordinates do not match its filename",
            ));
        }
        Ok(region)
    }

    pub fn read(&self, pos: ChunkPos) -> io::Result<Option<Chunk>> {
        if (pos.x.div_euclid(32), pos.z.div_euclid(32)) != (self.x, self.z) {
            return Err(invalid("chunk outside Linear region"));
        }
        let x = pos.x.rem_euclid(32) as usize;
        let z = pos.z.rem_euclid(32) as usize;
        let side = 32 / self.grid;
        let bucket = x / side * self.grid + z / side;
        let index = (x % side) * side + z % side;
        let mut cache = self
            .last_bucket
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some((cached, entries)) = cache.as_ref()
            && *cached == bucket
        {
            let (timestamp, nbt) = &entries[index];
            return Ok(nbt.as_ref().map(|nbt| Chunk {
                timestamp: *timestamp,
                nbt: nbt.clone(),
            }));
        }
        let mut entries = self.read_bucket(bucket)?;
        let (timestamp, nbt) = if entries
            .iter()
            .filter_map(|(_, nbt)| nbt.as_ref())
            .map(Vec::len)
            .sum::<usize>()
            <= MAX_CACHED_BUCKET
        {
            let entry = entries[index].clone();
            *cache = Some((bucket, entries));
            entry
        } else {
            entries.swap_remove(index)
        };
        Ok(nbt.map(|nbt| Chunk { timestamp, nbt }))
    }

    pub fn new(x: i32, z: i32) -> Self {
        Self {
            x,
            z,
            grid: GRID,
            features: vec![0],
            bitmap: [0; 128],
            newest: 0,
            buckets: (0..GRID * GRID)
                .map(|_| Bucket {
                    level: LEVEL as u8,
                    compressed: Vec::new(),
                })
                .collect(),
            last_bucket: Mutex::new(None),
        }
    }

    pub fn decode(mut input: &[u8]) -> io::Result<Self> {
        if take::<8>(&mut input)? != MAGIC || take::<1>(&mut input)? != [2] {
            return Err(invalid("expected a Linear v2 region"));
        }
        let newest = u64::from_be_bytes(take(&mut input)?);
        let grid = usize::from(take::<1>(&mut input)?[0]);
        if !matches!(grid, 1 | 2 | 4 | 8 | 16 | 32) {
            return Err(invalid("invalid Linear v2 grid"));
        }
        let x = i32::from_be_bytes(take(&mut input)?);
        let z = i32::from_be_bytes(take(&mut input)?);
        let bitmap = take(&mut input)?;
        let feature_start = input;
        loop {
            let length = usize::from(take::<1>(&mut input)?[0]);
            if length == 0 {
                break;
            }
            input = input
                .get(length + 4..)
                .ok_or_else(|| invalid("truncated Linear features"))?;
        }
        let features = feature_start[..feature_start.len() - input.len()].to_vec();
        let mut directory = Vec::with_capacity(grid * grid);
        for _ in 0..grid * grid {
            let size = u32::from_be_bytes(take(&mut input)?) as usize;
            let [level] = take(&mut input)?;
            let hash = u64::from_be_bytes(take(&mut input)?);
            directory.push((size, level, hash));
        }
        let mut buckets = Vec::with_capacity(directory.len());
        for (size, level, hash) in directory {
            let (compressed, rest) = input
                .split_at_checked(size)
                .ok_or_else(|| invalid("truncated Linear bucket"))?;
            if size != 0 && xxh64(compressed, 0) != hash {
                return Err(invalid("Linear bucket checksum mismatch"));
            }
            buckets.push(Bucket {
                level,
                compressed: compressed.to_vec(),
            });
            input = rest;
        }
        if input != MAGIC {
            return Err(invalid("invalid Linear v2 footer"));
        }
        Ok(Self {
            x,
            z,
            grid,
            features,
            bitmap,
            newest,
            buckets,
            last_bucket: Mutex::new(None),
        })
    }

    fn slots(&self, bucket: usize) -> impl Iterator<Item = usize> + use<> {
        let side = 32 / self.grid;
        let x = bucket / self.grid * side;
        let z = bucket % self.grid * side;
        (0..side).flat_map(move |dx| (0..side).map(move |dz| x + dx + 32 * (z + dz)))
    }

    fn read_bucket(&self, index: usize) -> io::Result<BucketEntries> {
        let compressed = &self.buckets[index].compressed;
        if compressed.is_empty() {
            return Ok(vec![(0, None); self.slots(index).count()]);
        }
        let mut reader =
            zstd::stream::read::Decoder::new(compressed.as_slice())?.take(MAX_BUCKET as u64 + 1);
        let mut entries = Vec::new();
        let mut remaining = MAX_BUCKET;
        for _ in self.slots(index) {
            let mut header = [0; 12];
            reader.read_exact(&mut header)?;
            let size = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
            let timestamp = u64::from_be_bytes(header[4..].try_into().unwrap());
            let length = match size {
                0 => 0,
                9.. => size - 8,
                _ => return Err(invalid("invalid Linear chunk size")),
            };
            remaining = remaining
                .checked_sub(12 + length)
                .filter(|_| length <= MAX_CHUNK)
                .ok_or_else(|| invalid("Linear bucket exceeds its decoded size limit"))?;
            let nbt = if size == 0 {
                None
            } else {
                let mut nbt = vec![0; length];
                reader.read_exact(&mut nbt)?;
                shulkers::restore(&mut nbt)?;
                remaining = remaining
                    .checked_sub(nbt.len().saturating_sub(length))
                    .ok_or_else(|| invalid("expanded Linear bucket exceeds its size limit"))?;
                Some(nbt)
            };
            entries.push((timestamp, nbt));
        }
        if reader.read(&mut [0])? != 0 {
            return Err(invalid("trailing Linear bucket data"));
        }
        Ok(entries)
    }

    pub fn chunks(&self) -> io::Result<Vec<Option<Chunk>>> {
        let mut chunks = vec![None; 1024];
        for index in 0..self.buckets.len() {
            for (slot, (timestamp, nbt)) in self.slots(index).zip(self.read_bucket(index)?) {
                chunks[slot] = nbt.map(|nbt| Chunk { timestamp, nbt });
            }
        }
        Ok(chunks)
    }

    /// Publish a complete region in a single replacement. The caller serializes
    /// updates to each region and must supply its latest snapshot.
    pub fn save(self, path: &Path, chunks: &[Option<Chunk>]) -> io::Result<()> {
        self.save_slots(path, chunks, None)
    }

    /// Rewrite buckets containing modified slots and preserve the other frames.
    pub fn save_changed(
        self,
        path: &Path,
        chunks: &[Option<Chunk>],
        changed: &[bool; 1024],
    ) -> io::Result<()> {
        self.save_slots(path, chunks, Some(changed))
    }

    fn save_slots(
        self,
        path: &Path,
        chunks: &[Option<Chunk>],
        changed: Option<&[bool; 1024]>,
    ) -> io::Result<()> {
        let bytes = self.encode_slots(chunks, changed)?;
        Self::publish(path, &bytes)
    }

    /// Update only the buckets containing the requested region-local slots.
    pub fn update(self, path: &Path, changes: Vec<(usize, Option<Chunk>)>) -> io::Result<()> {
        let mut chunks = vec![None; 1024];
        let mut changed = [false; 1024];
        for (slot, chunk) in changes {
            if slot >= chunks.len() {
                return Err(invalid("invalid Linear chunk slot"));
            }
            changed[slot] = true;
            chunks[slot] = chunk;
        }
        for index in 0..self.buckets.len() {
            if self.slots(index).any(|slot| changed[slot]) {
                for (slot, (timestamp, nbt)) in self.slots(index).zip(self.read_bucket(index)?) {
                    if !changed[slot] {
                        chunks[slot] = nbt.map(|nbt| Chunk { timestamp, nbt });
                    }
                }
            }
        }
        self.save_slots(path, &chunks, Some(&changed))
    }

    fn publish(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let temporary = path.with_extension("linear.pending");
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }

    pub fn encode_chunks(self, chunks: &[Option<Chunk>]) -> io::Result<Vec<u8>> {
        self.encode_slots(chunks, None)
    }

    fn encode_slots(
        mut self,
        chunks: &[Option<Chunk>],
        changed: Option<&[bool; 1024]>,
    ) -> io::Result<Vec<u8>> {
        if chunks.len() != 1024 {
            return Err(invalid("a Linear region requires 1024 slots"));
        }
        if changed.is_none() {
            self.newest = 0;
        }
        for index in 0..self.buckets.len() {
            if changed.is_some_and(|changed| self.slots(index).all(|slot| !changed[slot])) {
                continue;
            }
            for slot in self.slots(index) {
                let bit = 1 << (7 - slot % 8);
                self.bitmap[slot / 8] &= !bit;
                if let Some(chunk) = &chunks[slot] {
                    self.bitmap[slot / 8] |= bit;
                    self.newest = self.newest.max(chunk.timestamp);
                }
            }
            if self.slots(index).all(|slot| chunks[slot].is_none()) {
                self.buckets[index].compressed.clear();
                continue;
            }
            let mut payload = Vec::new();
            for slot in self.slots(index) {
                if let Some(chunk) = &chunks[slot] {
                    let deduplicated = if changed.is_some() {
                        shulkers::deduplicate(&chunk.nbt)?
                    } else {
                        None
                    };
                    let nbt = deduplicated.as_deref().unwrap_or(&chunk.nbt);
                    if nbt.is_empty()
                        || nbt.len() > MAX_CHUNK
                        || payload.len() + 12 + nbt.len() > MAX_BUCKET
                    {
                        return Err(invalid("Linear chunk or bucket exceeds its size limit"));
                    }
                    payload.extend_from_slice(&((nbt.len() + 8) as u32).to_be_bytes());
                    payload.extend_from_slice(&chunk.timestamp.to_be_bytes());
                    payload.extend_from_slice(nbt);
                } else {
                    payload.extend_from_slice(&[0; 12]);
                }
            }
            if payload.len() > MAX_BUCKET {
                return Err(invalid("Linear bucket exceeds its size limit"));
            }
            self.buckets[index] = Bucket {
                level: LEVEL as u8,
                compressed: zstd::bulk::compress(&payload, LEVEL)?,
            };
        }
        let mut output = Vec::new();
        output.extend_from_slice(&MAGIC);
        output.push(2);
        output.extend_from_slice(&self.newest.to_be_bytes());
        output.push(self.grid as u8);
        output.extend_from_slice(&self.x.to_be_bytes());
        output.extend_from_slice(&self.z.to_be_bytes());
        output.extend_from_slice(&self.bitmap);
        output.extend_from_slice(&self.features);
        for bucket in &self.buckets {
            output.extend_from_slice(&(bucket.compressed.len() as u32).to_be_bytes());
            output.push(bucket.level);
            output.extend_from_slice(&xxh64(&bucket.compressed, 0).to_be_bytes());
        }
        for bucket in self.buckets {
            output.extend_from_slice(&bucket.compressed);
        }
        output.extend_from_slice(&MAGIC);
        Ok(output)
    }
}

/// Convert region files beneath a world into a separate staging directory.
/// With no destination, perform the same conversion and verification in memory.
pub fn convert_world(world: &Path, staging: Option<&Path>) -> io::Result<(usize, usize)> {
    let world = world.canonicalize()?;
    let mut pending = vec![world.clone()];
    let mut sources = Vec::new();
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| matches!(name, "region" | "entities" | "poi"))
            {
                match path.extension().and_then(|extension| extension.to_str()) {
                    Some("mca") => sources.push(path),
                    Some("linear") => {
                        return Err(invalid(format!(
                            "existing Linear file in source world: {}",
                            path.display()
                        )));
                    }
                    _ => {}
                }
            }
        }
    }
    sources.sort();
    if let Some(staging) = staging {
        let parent = staging
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if parent.canonicalize()?.starts_with(&world) {
            return Err(invalid(
                "staging directory must be outside the source world",
            ));
        }
        fs::create_dir(staging)?;
    }
    let mut chunk_count = 0;
    for source in &sources {
        let mut anvil = RegionFile::open(source)?;
        let position = anvil.position();
        let mut chunks = vec![None; 1024];
        for (chunk, _) in anvil.chunks().collect::<Vec<_>>() {
            let record = anvil.read(chunk)?.ok_or_else(|| {
                invalid(format!(
                    "{} has an unreadable chunk at {}, {}",
                    source.display(),
                    chunk.x,
                    chunk.z
                ))
            })?;
            let slot = (chunk.x.rem_euclid(32) + 32 * chunk.z.rem_euclid(32)) as usize;
            chunks[slot] = Some(Chunk {
                timestamp: u64::from(record.timestamp),
                nbt: record.compression.decode(&record.compressed, MAX_CHUNK)?,
            });
            chunk_count += 1;
        }
        let bytes = Region::new(position.x, position.z).encode_chunks(&chunks)?;
        let decoded = if let Some(staging) = staging {
            let relative = source.strip_prefix(&world).expect("source inside world");
            let destination = staging.join(relative).with_extension("linear");
            fs::create_dir_all(destination.parent().expect("staged region has a directory"))?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            Region::decode(&fs::read(destination)?)?
        } else {
            Region::decode(&bytes)?
        };
        if (decoded.x, decoded.z) != (position.x, position.z) || decoded.chunks()? != chunks {
            return Err(invalid(format!(
                "verification failed for {}",
                source.display()
            )));
        }
    }
    Ok((sources.len(), chunk_count))
}

/// Refuse to open a world with legacy region data on the dedicated server.
pub fn check_world(world: &Path) -> io::Result<()> {
    let mut pending = vec![world.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if directory == world && error.kind() == io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let region_folder = directory
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| matches!(name, "region" | "entities" | "poi"));
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                pending.push(path);
            } else if region_folder {
                match path.extension().and_then(|extension| extension.to_str()) {
                    Some("mca" | "mcc") => {
                        return Err(invalid(format!(
                            "legacy region data at {}; migrate the world to Linear v2 before starting",
                            path.display()
                        )));
                    }
                    Some("linear") => {
                        let mut header = [0; 9];
                        fs::File::open(&path)?.read_exact(&mut header)?;
                        if header[..8] != MAGIC || header[8] != 2 {
                            return Err(invalid(format!(
                                "expected Linear v2 at {}",
                                path.display()
                            )));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_save_preserves_untouched_frames() {
        let directory =
            std::env::temp_dir().join(format!("horizon-linear-save-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("r.-2.5.linear");
        let position = ChunkPos::new(-2, 5);
        let mut chunks = vec![None; 1024];
        chunks[0] = Some(Chunk {
            timestamp: 100,
            nbt: vec![10, 0, 0, 0],
        });
        chunks[1023] = chunks[0].clone();
        Region::new(position.x, position.z)
            .save(&path, &chunks)
            .unwrap();
        let region = Region::open(&path, position).unwrap();
        let untouched = region.buckets[63].compressed.clone();
        chunks[0] = None;
        let mut changed = [false; 1024];
        changed[0] = true;
        region.save_changed(&path, &chunks, &changed).unwrap();
        let reopened = Region::open(&path, position).unwrap();
        assert_eq!(reopened.buckets[63].compressed, untouched);
        assert_eq!(reopened.chunks().unwrap(), chunks);
        fs::remove_file(path).unwrap();
        fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn sparse_slots_and_wide_timestamps_survive_a_round_trip() {
        let mut chunks = vec![None; 1024];
        for slot in [0, 4, 31, 32, 160, 1023] {
            chunks[slot] = Some(Chunk {
                timestamp: u64::from(u32::MAX) + slot as u64 + 1,
                nbt: vec![10, 0, 0, 0],
            });
        }
        let mut bytes = Region::new(-2, 5).encode_chunks(&chunks).unwrap();
        let region = Region::decode(&bytes).unwrap();
        assert_eq!((region.x, region.z), (-2, 5));
        assert_eq!(region.chunks().unwrap(), chunks);
        let first_bucket_byte = 26 + 128 + 1 + 64 * 13;
        bytes[first_bucket_byte] ^= 1;
        assert!(Region::decode(&bytes).is_err());
    }
}
