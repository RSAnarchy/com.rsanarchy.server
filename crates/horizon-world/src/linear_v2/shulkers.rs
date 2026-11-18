use crate::{
    compression::invalid,
    nbt::{self, JavaString, Limits, Tag},
};
use std::{collections::HashMap, io};

const OPTIMIZATION: &str = "anarchyOptimization";

fn shulker_container(item: &mut Tag) -> Option<(&mut nbt::Compound, &'static str)> {
    let Some(Tag::String(id)) = item.get("id") else {
        return None;
    };
    let id = id.to_string_lossy();
    if id != "minecraft:shulker_box" && !id.ends_with("_shulker_box") {
        return None;
    }
    let (parent, key, contents) = if item.get("components").is_some() {
        ("components", "minecraft:container", "contents")
    } else {
        ("tag", "BlockEntityTag", "Items")
    };
    if contents == "contents" {
        match item.get_mut(parent)? {
            Tag::Compound(container) => Some((container, key)),
            _ => None,
        }
    } else {
        match item.get_mut(parent)?.get_mut(key)? {
            Tag::Compound(container) => Some((container, contents)),
            _ => None,
        }
    }
}

pub(super) fn deduplicate(bytes: &[u8]) -> io::Result<Option<Vec<u8>>> {
    if !bytes.windows(11).any(|window| window == b"shulker_box") {
        return Ok(None);
    }
    let limits = Limits::saved();
    let (mut root, consumed) = nbt::read_named(bytes, limits).map_err(io::Error::other)?;
    if consumed != bytes.len() || root.tag.get(OPTIMIZATION).is_some() {
        return Ok(None);
    }
    let Some(Tag::List(block_entities)) = root.tag.get_mut("block_entities") else {
        return Ok(None);
    };
    let mut palette = HashMap::<Tag, (String, usize)>::new();
    for block_entity in block_entities.iter_mut() {
        let Some(Tag::List(items)) = block_entity.get_mut("Items") else {
            continue;
        };
        for item in items {
            let Some((container, key)) = shulker_container(item) else {
                continue;
            };
            if let Some(contents @ Tag::List(items)) = container.get(&JavaString::from(key))
                && !items.is_empty()
            {
                let entry = palette.entry(contents.clone()).or_default();
                if entry.1 == 0 {
                    let encoded = nbt::write_named(
                        &nbt::NamedTag {
                            name: JavaString::from(""),
                            tag: contents.clone(),
                        },
                        limits,
                    )
                    .map_err(io::Error::other)?;
                    let hash = xxhash_rust::xxh3::xxh3_128(&encoded);
                    let lower = (hash as i64).unsigned_abs();
                    let upper = ((hash >> 64) as i64).unsigned_abs();
                    entry.0 = format!("{lower:016x}{upper:016x}");
                }
                entry.1 += 1;
            }
        }
    }
    let mut entries = Vec::new();
    for block_entity in block_entities {
        let Some(Tag::List(items)) = block_entity.get_mut("Items") else {
            continue;
        };
        for item in items {
            let Some((container, key)) = shulker_container(item) else {
                continue;
            };
            let contents_key = JavaString::from(key);
            let hash_key = JavaString::from(if key == "Items" {
                "hash"
            } else {
                "horizon:shulker_hash"
            });
            if container.contains_key(&hash_key) {
                continue;
            }
            let Some((hash, count)) = container
                .get(&contents_key)
                .and_then(|contents| palette.get_mut(contents))
            else {
                continue;
            };
            if *count < 2 {
                continue;
            }
            let contents = container.remove(&contents_key).unwrap();
            container.insert(hash_key, Tag::String(JavaString::from(hash.as_str())));
            entries.push(Tag::compound([
                ("hash", Tag::String(JavaString::from(hash.as_str()))),
                ("item", contents),
            ]));
        }
    }
    if entries.is_empty() {
        return Ok(None);
    }
    let mut seen = std::collections::HashSet::new();
    entries.retain(|entry| seen.insert(entry.get("hash").cloned()));
    let Tag::Compound(chunk) = &mut root.tag else {
        unreachable!()
    };
    chunk.insert(
        JavaString::from(OPTIMIZATION),
        Tag::compound([("itemPalette", Tag::List(entries))]),
    );
    let encoded = nbt::write_named(&root, limits).map_err(io::Error::other)?;
    Ok((encoded.len() < bytes.len()).then_some(encoded))
}

pub(super) fn restore(bytes: &mut Vec<u8>) -> io::Result<()> {
    if !bytes
        .windows(OPTIMIZATION.len())
        .any(|window| window == OPTIMIZATION.as_bytes())
    {
        return Ok(());
    }
    let limits = Limits::saved();
    let (mut root, _) = nbt::read_named(bytes, limits).map_err(io::Error::other)?;
    let Tag::Compound(chunk) = &mut root.tag else {
        return Ok(());
    };
    let Some(optimization) = chunk.remove(&JavaString::from(OPTIMIZATION)) else {
        return Ok(());
    };
    let Some(Tag::List(entries)) = optimization.get("itemPalette") else {
        return Err(invalid("missing shulker item palette"));
    };
    let mut palette = HashMap::new();
    for entry in entries {
        let (Some(Tag::String(hash)), Some(items @ Tag::List(_))) =
            (entry.get("hash"), entry.get("item"))
        else {
            return Err(invalid("invalid shulker item palette entry"));
        };
        let size = nbt::write_named(&nbt::NamedTag {
            name: JavaString::from(""),
            tag: items.clone(),
        }, limits).map_err(io::Error::other)?.len();
        if let Some((previous, _)) = palette.insert(hash, (items, size))
            && previous != items
        {
            return Err(invalid("conflicting shulker item palette entries"));
        }
    }
    let mut remaining = super::MAX_CHUNK.saturating_sub(bytes.len());
    if let Some(Tag::List(block_entities)) = chunk.get_mut(&JavaString::from("block_entities")) {
        for block_entity in block_entities {
            let Some(Tag::List(items)) = block_entity.get_mut("Items") else {
                continue;
            };
            for item in items {
                let Some((container, key)) = shulker_container(item) else {
                    continue;
                };
                let hash_key = JavaString::from(if key == "Items" {
                    "hash"
                } else {
                    "horizon:shulker_hash"
                });
                let Some(Tag::String(hash)) = container.get(&hash_key) else {
                    continue;
                };
                let (contents, size) = palette
                    .get(hash)
                    .ok_or_else(|| invalid("unknown shulker palette hash"))?;
                remaining = remaining.checked_sub(*size)
                    .ok_or_else(|| invalid("expanded shulker palette exceeds chunk size limit"))?;
                container.insert(JavaString::from(key), (**contents).clone());
                container.remove(&hash_key);
            }
        }
    }
    *bytes = nbt::write_named(&root, limits).map_err(io::Error::other)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_modern_and_legacy_shulker_contents_round_trip() {
        for modern in [false, true] {
            let contents = Tag::List(vec![Tag::compound([(
                "payload",
                Tag::ByteArray(vec![42; 512]),
            )])]);
            let (key, data) = if modern {
                (
                    "components",
                    Tag::compound([("minecraft:container", contents)]),
                )
            } else {
                (
                    "tag",
                    Tag::compound([("BlockEntityTag", Tag::compound([("Items", contents)]))]),
                )
            };
            let item = Tag::compound([
                (
                    "id",
                    Tag::String(JavaString::from("minecraft:blue_shulker_box")),
                ),
                (key, data),
            ]);
            let root = nbt::NamedTag {
                name: JavaString::from(""),
                tag: Tag::compound([(
                    "block_entities",
                    Tag::List(vec![Tag::compound([(
                        "Items",
                        Tag::List(vec![item.clone(), item]),
                    )])]),
                )]),
            };
            let original = nbt::write_named(&root, Limits::saved()).unwrap();
            let mut encoded = deduplicate(&original).unwrap().unwrap();
            assert!(encoded.len() < original.len());
            restore(&mut encoded).unwrap();
            assert_eq!(encoded, original);
        }
    }
}
