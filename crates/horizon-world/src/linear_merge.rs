//! Puts the chunks of `.linear` regions back into Anvil (`.mca`) files: the way back from the
//! linear conversion. A chunk in a `.linear` file was saved after the conversion, so it wins.
//! Every other chunk comes from the original `.mca`, found in `mca-backup/` or the region
//! folder. Originals are never changed or deleted. Each merged file is written beside the
//! region folder, read back and checked, and only then replaces the region's `.mca`.

use crate::{
    compression::Compression,
    linear::LinearRegionFile,
    region::{ChunkRecord, RegionFile, RegionWriter},
};
use horizon_core::position::ChunkPos;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

/// The folders of a dimension that hold region-style files.
pub const FOLDERS: [&str; 3] = ["region", "entities", "poi"];
/// Where conversion moved the `.mca` originals.
const BACKUP: &str = "mca-backup";
/// Scratch folder for a merged file before it is checked and moved into place.
const SCRATCH: &str = ".merge";

/// What a merge did, counted over the whole world.
#[derive(Debug, Default, Clone, Copy)]
pub struct Report {
    /// Regions written as `.mca` from a `.linear` file.
    pub merged: usize,
    /// Regions with no `.linear` whose original was copied back into the region folder.
    pub restored: usize,
    /// Chunks taken from `.linear` files.
    pub from_linear: usize,
    /// Chunks taken from the original `.mca`.
    pub from_original: usize,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Every dimension of the world, `dimensions/<namespace>/<name>`, in name order.
pub fn dimensions(world: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let Ok(namespaces) = fs::read_dir(world.join("dimensions")) else {
        return Ok(out);
    };
    for namespace in namespaces {
        let namespace = namespace?.path();
        if !namespace.is_dir() {
            continue;
        }
        for dimension in fs::read_dir(&namespace)? {
            let dimension = dimension?.path();
            if dimension.is_dir() {
                out.push(dimension);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Merge every region, entity and poi folder of every dimension in `world`. With `dry_run`,
/// nothing is written and the report says what would be.
pub fn merge_world(world: &Path, dry_run: bool) -> io::Result<Report> {
    let mut report = Report::default();
    for dimension in dimensions(world)? {
        for folder in FOLDERS {
            merge_folder(
                &dimension.join(folder),
                &dimension.join(BACKUP).join(folder),
                dry_run,
                &mut report,
            )?;
        }
    }
    Ok(report)
}

/// The region names (`r.X.Z`) that have files with `extension` in `folder`.
fn names_with(folder: &Path, extension: &str) -> io::Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    let Ok(entries) = fs::read_dir(folder) else {
        return Ok(out);
    };
    for entry in entries {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if let Some(stem) = name.strip_suffix(extension) {
            out.insert(stem.to_owned());
        }
    }
    Ok(out)
}

/// The original `.mca` of region `stem`: the backup's copy, else the region folder's.
fn original_of(dir: &Path, backup: &Path, stem: &str) -> Option<PathBuf> {
    let name = format!("{stem}.mca");
    [backup.join(&name), dir.join(&name)]
        .into_iter()
        .find(|path| path.is_file())
}

fn merge_folder(dir: &Path, backup: &Path, dry_run: bool, report: &mut Report) -> io::Result<()> {
    let linear = names_with(dir, ".linear")?;
    let originals = {
        let mut all = names_with(backup, ".mca")?;
        all.extend(names_with(dir, ".mca")?);
        all
    };

    for stem in &linear {
        let linear_file = LinearRegionFile::open(dir.join(format!("{stem}.linear")))?;
        let mut chunks: BTreeMap<(i32, i32), ChunkRecord> = BTreeMap::new();
        if let Some(original) = original_of(dir, backup, stem) {
            let mut region = RegionFile::open(&original)?;
            let positions: Vec<ChunkPos> = region.chunks().map(|(pos, _)| pos).collect();
            for pos in positions {
                if let Some(record) = region.read(pos)? {
                    chunks.insert((pos.x, pos.z), record);
                }
            }
        }
        let mut from_linear = 0;
        for (pos, _) in linear_file.chunks() {
            let raw = linear_file
                .read(pos)?
                .ok_or_else(|| invalid(format!("{stem}.linear lists a chunk it does not hold")))?;
            let record = ChunkRecord {
                compression: Compression::Zlib,
                compressed: Compression::Zlib.encode(&raw.compressed)?,
                timestamp: raw.timestamp,
            };
            chunks.insert((pos.x, pos.z), record);
            from_linear += 1;
        }

        if dry_run {
            report.merged += 1;
            report.from_linear += from_linear;
            report.from_original += chunks.len() - from_linear;
            continue;
        }

        let target = dir.join(format!("{stem}.mca"));
        let scratch = dir.join(SCRATCH);
        fs::create_dir_all(&scratch)?;
        let temp = scratch.join(format!("{stem}.mca"));
        let _ = fs::remove_file(&temp);
        let mut writer = RegionWriter::create(&temp)?;
        for (&(x, z), record) in &chunks {
            writer.write(ChunkPos::new(x, z), record)?;
        }
        writer.finish()?;
        verify(&temp, &chunks)?;
        fs::rename(&temp, &target)?;
        let _ = fs::remove_dir(&scratch);

        report.merged += 1;
        report.from_linear += from_linear;
        report.from_original += chunks.len() - from_linear;
    }

    // Regions with no .linear: their original goes back into the region folder, if missing.
    for stem in originals.difference(&linear) {
        let target = dir.join(format!("{stem}.mca"));
        if target.is_file() {
            continue;
        }
        let Some(original) = original_of(dir, backup, stem) else {
            continue;
        };
        if !dry_run {
            fs::create_dir_all(dir)?;
            fs::copy(&original, &target)?;
        }
        report.restored += 1;
    }
    Ok(())
}

/// Read `path` back and check it holds exactly `expected`, chunk for chunk, byte for byte.
fn verify(path: &Path, expected: &BTreeMap<(i32, i32), ChunkRecord>) -> io::Result<()> {
    let mut region = RegionFile::open(path)?;
    let count = region.chunks().count();
    if count != expected.len() {
        return Err(invalid(format!(
            "{} holds {count} chunks, expected {}",
            path.display(),
            expected.len()
        )));
    }
    for (&(x, z), record) in expected {
        let pos = ChunkPos::new(x, z);
        let read = region
            .read(pos)?
            .ok_or_else(|| invalid(format!("{} lost chunk {x} {z}", path.display())))?;
        let want = record.compression.decode(&record.compressed, usize::MAX)?;
        let got = read.compression.decode(&read.compressed, usize::MAX)?;
        if want != got {
            return Err(invalid(format!("{} differs at chunk {x} {z}", path.display())));
        }
    }
    Ok(())
}
