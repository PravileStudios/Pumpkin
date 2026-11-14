use pumpkin_data::block_properties::BLOCK_ENTITY_TYPES;
use pumpkin_data::entity::EntityType;
use pumpkin_nbt::compound::NbtCompound;
use pumpkin_nbt::tag::NbtTag;
use pumpkin_util::math::position::BlockPos;
use pumpkin_util::math::vector2::Vector2;
use pumpkin_world::ProtoChunk;
pub const MAX_NBT_BYTES: usize = 64 * 1024;
pub const MAX_BLOCK_ENTITIES_PER_CHUNK: usize = 8192;
pub const MAX_ENTITIES_PER_CHUNK: usize = 8192;

const POSITION_KEYS: [&str; 3] = ["x", "y", "z"];

pub fn read_nbt(bytes: &[u8]) -> Result<NbtCompound, String> {
    if bytes.len() > MAX_NBT_BYTES {
        return Err(format!(
            "NBT is {} bytes, the limit is {MAX_NBT_BYTES}",
            bytes.len()
        ));
    }
    let mut cursor = std::io::Cursor::new(bytes);
    let mut reader = pumpkin_nbt::deserializer::NbtReadHelperJava::new(
        pumpkin_nbt::deserializer::NbtStreamReader(&mut cursor),
    );
    let nbt = pumpkin_nbt::Nbt::read_unnamed(&mut reader)
        .map_err(|e| format!("invalid NBT: {e}"))?
        .root_tag;
    if cursor.position() != bytes.len() as u64 {
        return Err("trailing bytes after the NBT root compound".to_string());
    }
    Ok(nbt)
}

#[must_use]
pub fn write_nbt(nbt: NbtCompound) -> Vec<u8> {
    pumpkin_nbt::Nbt::from(nbt).write_unnamed().to_vec()
}

fn short_name(id: &str) -> &str {
    id.strip_prefix("minecraft:").unwrap_or(id)
}

fn local_y_in_range(proto: &ProtoChunk, y: i32) -> bool {
    let bottom = i32::from(proto.bottom_y());
    y >= bottom && y < bottom + i32::from(proto.height())
}
pub fn add_block_entity(
    proto: &mut ProtoChunk,
    x: u8,
    y: i32,
    z: u8,
    bytes: &[u8],
) -> Result<(), String> {
    if x >= 16 || z >= 16 {
        return Err(format!("position {x} {y} {z} is outside the chunk"));
    }
    if !local_y_in_range(proto, y) {
        return Err(format!("y {y} is outside the world height"));
    }
    if proto.pending_block_entities.len() >= MAX_BLOCK_ENTITIES_PER_CHUNK {
        return Err(format!(
            "the chunk already has {MAX_BLOCK_ENTITIES_PER_CHUNK} block entities"
        ));
    }
    let mut nbt = read_nbt(bytes)?;
    let id = nbt
        .get_string("id")
        .ok_or_else(|| "the block entity has no id".to_string())?;
    let local_y = y - i32::from(proto.bottom_y());
    let state = proto
        .get_block_state_raw(i32::from(x), local_y, i32::from(z))
        .to_state();
    let expected = BLOCK_ENTITY_TYPES.get(state.block_entity_type as usize);
    if expected.is_none_or(|name| *name != short_name(id)) {
        return Err(format!(
            "the block at {x} {y} {z} does not carry a {id} block entity"
        ));
    }
    let world_x = proto.x * 16 + i32::from(x);
    let world_z = proto.z * 16 + i32::from(z);
    nbt.put_int("x", world_x);
    nbt.put_int("y", y);
    nbt.put_int("z", world_z);
    proto.add_block_entity(nbt);
    Ok(())
}

fn position(nbt: &NbtCompound) -> Option<[f64; 3]> {
    let pos = nbt.get_list("Pos")?;
    if pos.len() != 3 {
        return None;
    }
    let mut out = [0.0; 3];
    for (slot, tag) in out.iter_mut().zip(pos) {
        *slot = tag.extract_double().filter(|v| v.is_finite())?;
    }
    Some(out)
}

fn put_position(nbt: &mut NbtCompound, [x, y, z]: [f64; 3]) {
    nbt.put(
        "Pos",
        NbtTag::List(vec![
            NbtTag::Double(x),
            NbtTag::Double(y),
            NbtTag::Double(z),
        ]),
    );
}
pub fn add_entity(proto: &mut ProtoChunk, bytes: &[u8]) -> Result<(), String> {
    if proto.pending_structure_entity_count() >= MAX_ENTITIES_PER_CHUNK {
        return Err(format!(
            "the chunk already has {MAX_ENTITIES_PER_CHUNK} entities"
        ));
    }
    let mut nbt = read_nbt(bytes)?;
    let id = nbt
        .get_string("id")
        .ok_or_else(|| "the entity has no id".to_string())?;
    let entity_type =
        EntityType::from_name(short_name(id)).ok_or_else(|| format!("unknown entity type {id}"))?;
    if entity_type.id == EntityType::PLAYER.id || !entity_type.summonable {
        return Err(format!("{id} cannot be added to a chunk"));
    }
    if nbt.child_tags.contains_key("Passengers") {
        return Err("passengers are not supported, add them as separate entities".to_string());
    }
    let [x, y, z] = position(&nbt)
        .ok_or_else(|| "the entity needs a Pos list of three finite doubles".to_string())?;
    if !(0.0..16.0).contains(&x) || !(0.0..16.0).contains(&z) {
        return Err(format!("Pos {x} {z} is outside the chunk (0 to 16)"));
    }
    if !local_y_in_range(proto, y.floor() as i32) {
        return Err(format!("Pos y {y} is outside the world height"));
    }
    put_position(
        &mut nbt,
        [
            f64::from(proto.x) * 16.0 + x,
            y,
            f64::from(proto.z) * 16.0 + z,
        ],
    );
    nbt.child_tags.remove("UUID");
    proto.add_structure_entity(nbt);
    Ok(())
}
pub struct ExportedBlockEntity {
    pub x: u8,
    pub y: i32,
    pub z: u8,
    pub nbt: Vec<u8>,
}

#[must_use]
pub fn export_block_entities(entries: Vec<(BlockPos, NbtCompound)>) -> Vec<ExportedBlockEntity> {
    entries
        .into_iter()
        .map(|(pos, mut nbt)| {
            for key in POSITION_KEYS {
                nbt.child_tags.remove(key);
            }
            ExportedBlockEntity {
                x: (pos.0.x & 15) as u8,
                y: pos.0.y,
                z: (pos.0.z & 15) as u8,
                nbt: write_nbt(nbt),
            }
        })
        .collect()
}
#[must_use]
pub fn export_entities(chunk: Vector2<i32>, entries: Vec<NbtCompound>) -> Vec<Vec<u8>> {
    entries
        .into_iter()
        .filter_map(|mut nbt| {
            let [x, y, z] = position(&nbt)?;
            let relative = [
                x - f64::from(chunk.x) * 16.0,
                y,
                z - f64::from(chunk.y) * 16.0,
            ];
            if !(0.0..16.0).contains(&relative[0]) || !(0.0..16.0).contains(&relative[2]) {
                return None;
            }
            put_position(&mut nbt, relative);
            nbt.child_tags.remove("UUID");
            nbt.child_tags.remove("Passengers");
            Some(write_nbt(nbt))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pumpkin_data::Block;
    use pumpkin_data::BlockStateId;
    use pumpkin_data::dimension::Dimension;
    use pumpkin_util::world_seed::Seed;
    use pumpkin_world::generation::generator::{WorldGenerator, flat::FlatGenerator};

    fn proto(x: i32, z: i32) -> ProtoChunk {
        let generator = WorldGenerator::Flat(Box::new(FlatGenerator::new(
            Seed(0),
            Dimension::OVERWORLD,
            Vec::new(),
            "minecraft:plains".to_string(),
        )));
        ProtoChunk::new(x, z, &generator)
    }

    fn nbt(entries: &[(&str, NbtTag)]) -> Vec<u8> {
        let mut nbt = NbtCompound::new();
        for (key, tag) in entries {
            nbt.put(key, tag.clone());
        }
        write_nbt(nbt)
    }

    fn place(proto: &mut ProtoChunk, x: i32, y: i32, z: i32, state: BlockStateId) {
        proto.set_block_state(
            proto.x * 16 + x,
            y,
            proto.z * 16 + z,
            pumpkin_data::BlockState::from_id(state),
        );
    }

    #[test]
    fn block_entity_lands_at_world_position() {
        let mut chunk = proto(3, -2);
        place(
            &mut chunk,
            5,
            70,
            9,
            Block::WHITE_SHULKER_BOX.default_state.id,
        );
        let data = nbt(&[("id", NbtTag::String("minecraft:shulker_box".into()))]);
        add_block_entity(&mut chunk, 5, 70, 9, &data).unwrap();
        let queued = chunk.take_pending_block_entities();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].get_int("x"), Some(53));
        assert_eq!(queued[0].get_int("y"), Some(70));
        assert_eq!(queued[0].get_int("z"), Some(-23));
    }

    #[test]
    fn block_entity_must_match_the_block() {
        let mut chunk = proto(0, 0);
        place(&mut chunk, 1, 64, 1, Block::STONE.default_state.id);
        let data = nbt(&[("id", NbtTag::String("minecraft:chest".into()))]);
        assert!(add_block_entity(&mut chunk, 1, 64, 1, &data).is_err());
        place(&mut chunk, 1, 64, 1, Block::OAK_SIGN.default_state.id);
        assert!(add_block_entity(&mut chunk, 1, 64, 1, &data).is_err());
        let sign = nbt(&[("id", NbtTag::String("sign".into()))]);
        assert!(add_block_entity(&mut chunk, 1, 64, 1, &sign).is_ok());
    }

    #[test]
    fn block_entity_position_and_payload_are_checked() {
        let mut chunk = proto(0, 0);
        let data = nbt(&[("id", NbtTag::String("minecraft:chest".into()))]);
        assert!(add_block_entity(&mut chunk, 16, 64, 0, &data).is_err());
        assert!(add_block_entity(&mut chunk, 0, 64, 16, &data).is_err());
        assert!(add_block_entity(&mut chunk, 0, -65, 0, &data).is_err());
        assert!(add_block_entity(&mut chunk, 0, 320, 0, &data).is_err());
        assert!(add_block_entity(&mut chunk, 0, 64, 0, &[10, 1]).is_err());
        let mut trailing = data.clone();
        trailing.push(0);
        assert!(read_nbt(&trailing).is_err());
        assert!(read_nbt(&vec![0; MAX_NBT_BYTES + 1]).is_err());
        let no_id = nbt(&[("CustomName", NbtTag::String("x".into()))]);
        place(&mut chunk, 0, 64, 0, Block::CHEST.default_state.id);
        assert!(add_block_entity(&mut chunk, 0, 64, 0, &no_id).is_err());
        assert!(chunk.take_pending_block_entities().is_empty());
    }

    fn pos(x: f64, y: f64, z: f64) -> NbtTag {
        NbtTag::List(vec![
            NbtTag::Double(x),
            NbtTag::Double(y),
            NbtTag::Double(z),
        ])
    }

    #[test]
    fn entity_position_is_made_absolute() {
        let mut chunk = proto(-1, 4);
        let data = nbt(&[
            ("id", NbtTag::String("minecraft:block_display".into())),
            ("Pos", pos(2.5, 80.0, 15.75)),
            ("UUID", NbtTag::IntArray(vec![1, 2, 3, 4])),
        ]);
        add_entity(&mut chunk, &data).unwrap();
        let queued = chunk.take_pending_structure_entities();
        assert_eq!(queued.len(), 1);
        assert_eq!(position(&queued[0]), Some([-13.5, 80.0, 79.75]));
        assert!(!queued[0].child_tags.contains_key("UUID"));
    }

    #[test]
    fn entity_is_validated() {
        let mut chunk = proto(0, 0);
        let display = NbtTag::String("minecraft:item_frame".into());
        let cases = [
            nbt(&[("id", display.clone()), ("Pos", pos(16.0, 64.0, 1.0))]),
            nbt(&[("id", display.clone()), ("Pos", pos(1.0, 64.0, -0.1))]),
            nbt(&[("id", display.clone()), ("Pos", pos(1.0, 500.0, 1.0))]),
            nbt(&[("id", display.clone()), ("Pos", pos(f64::NAN, 64.0, 1.0))]),
            nbt(&[("id", display.clone())]),
            nbt(&[
                ("id", NbtTag::String("minecraft:player".into())),
                ("Pos", pos(1.0, 64.0, 1.0)),
            ]),
            nbt(&[
                ("id", NbtTag::String("minecraft:nope".into())),
                ("Pos", pos(1.0, 64.0, 1.0)),
            ]),
            nbt(&[
                ("id", display.clone()),
                ("Pos", pos(1.0, 64.0, 1.0)),
                ("Passengers", NbtTag::List(vec![])),
            ]),
        ];
        for case in &cases {
            assert!(add_entity(&mut chunk, case).is_err());
        }
        assert_eq!(chunk.pending_structure_entity_count(), 0);
        let ok = nbt(&[("id", display), ("Pos", pos(0.0, 64.0, 15.999))]);
        assert!(add_entity(&mut chunk, &ok).is_ok());
    }

    #[test]
    fn export_round_trips_into_add() {
        let chunk_pos = Vector2::new(2, -3);
        let mut entity = NbtCompound::new();
        entity.put_string("id", "minecraft:armor_stand".to_string());
        put_position(&mut entity, [37.25, 70.0, -40.5]);
        entity.put("UUID", NbtTag::IntArray(vec![1, 2, 3, 4]));
        entity.put("Passengers", NbtTag::List(vec![]));
        let exported = export_entities(chunk_pos, vec![entity]);
        let back = read_nbt(&exported[0]).unwrap();
        assert_eq!(position(&back), Some([5.25, 70.0, 7.5]));
        assert!(!back.child_tags.contains_key("UUID"));
        let mut chunk = proto(2, -3);
        add_entity(&mut chunk, &exported[0]).unwrap();
        let queued = chunk.take_pending_structure_entities();
        assert_eq!(position(&queued[0]), Some([37.25, 70.0, -40.5]));

        let mut sign = NbtCompound::new();
        sign.put_string("id", "minecraft:sign".to_string());
        sign.put_int("x", 37);
        sign.put_int("y", 70);
        sign.put_int("z", -41);
        let exported = export_block_entities(vec![(BlockPos::new(37, 70, -41), sign)]);
        assert_eq!((exported[0].x, exported[0].y, exported[0].z), (5, 70, 7));
        assert!(
            !read_nbt(&exported[0].nbt)
                .unwrap()
                .child_tags
                .contains_key("x")
        );
    }
}
