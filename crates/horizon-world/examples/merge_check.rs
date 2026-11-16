// TEMP scratch harness (removed after the run).
use horizon_core::position::ChunkPos;
use horizon_world::{compression::Compression, linear::LinearWriter, region::{ChunkRecord, RegionFile, RegionWriter}};
use std::{fs, path::Path};
fn rec(b: &[u8]) -> ChunkRecord { ChunkRecord { compression: Compression::Zlib, compressed: Compression::Zlib.encode(b).unwrap(), timestamp: 5 } }
fn mca(p: &Path, c: &[(ChunkPos, &[u8])]) { fs::create_dir_all(p.parent().unwrap()).unwrap(); let mut w = RegionWriter::create(p).unwrap(); for (q,d) in c { w.write(*q, &rec(d)).unwrap(); } w.finish().unwrap(); }
fn show(label: &str, p: &Path, probes: &[(ChunkPos, &str)]) {
    let mut r = RegionFile::open(p).unwrap();
    let n = r.chunks().count();
    let mut out = String::new();
    for (q, want) in probes {
        let got = r.read(*q).unwrap().map(|x| String::from_utf8_lossy(&x.compression.decode(&x.compressed, usize::MAX).unwrap()).to_string());
        out += &format!(" [{:?}={:?} {}]", (q.x,q.z), got.as_deref(), if got.as_deref()==Some(*want) {"ok"} else {"MISMATCH"});
    }
    println!("{label}: chunks={n}{out}");
}
fn main() {
    let world = std::env::temp_dir().join("merge_world");
    let _ = fs::remove_dir_all(&world);
    let ow = world.join("dimensions/minecraft/overworld");
    let a = ChunkPos::new(1,1); let b = ChunkPos::new(2,2); let c = ChunkPos::new(3,3); let d = ChunkPos::new(4,4); let e = ChunkPos::new(40,1);
    // R1 (r.0.0): original A,B,C in backup; linear has B changed and D new
    mca(&ow.join("mca-backup/region/r.0.0.mca"), &[(a,b"A old"),(b,b"B old"),(c,b"C old")]);
    fs::create_dir_all(ow.join("region")).unwrap();
    let mut lw = LinearWriter::create(ow.join("region/r.0.0.linear"), 3).unwrap();
    lw.write(b, &rec(b"B new")).unwrap(); lw.write(d, &rec(b"D new")).unwrap(); lw.finish().unwrap();
    // R2 (r.1.0): untouched region, original only in backup -> must be restored
    mca(&ow.join("mca-backup/region/r.1.0.mca"), &[(e,b"E old")]);
    // entities: original only in backup -> restored
    mca(&ow.join("mca-backup/entities/r.0.0.mca"), &[(a,b"ent A")]);
    println!("dry run:");
    // run the real CLI later; for now just report the dry run via cargo run below
    let _ = (c, show as fn(&str,&Path,&[(ChunkPos,&str)]));
    println!("world = {}", world.display());
}
