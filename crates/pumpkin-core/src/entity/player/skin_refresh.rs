use std::sync::{Arc, Weak};

use pumpkin_inventory::screen_handler::{InventoryPlayer, ScreenHandler};
use pumpkin_protocol::Property;
use pumpkin_protocol::bedrock::client::{
    common::BuildPlatform,
    player_list::{CPlayerList, PlayerListEntry, Skin},
};
use pumpkin_protocol::codec::{var_int::VarInt, var_long::VarLong};
use pumpkin_protocol::java::client::play::Player as InfoEntry;
use pumpkin_protocol::java::client::play::{
    CGameEvent, CPlayerInfoUpdate, CRemovePlayerInfo, CRespawn, CSetExperience, CSetPassengers,
    CSetSelectedSlot, GameEvent, PlayerAction, PlayerInfoFlags, PlayerSpawnData,
};
use pumpkin_util::version::JavaMinecraftVersion;
use pumpkin_world::biome;

use super::Player;
use crate::entity::EntityBase;
use crate::net::ClientPlatform;
use crate::world::World;

#[must_use]
pub fn replace_textures(
    properties: &[Property],
    value: String,
    signature: Option<String>,
) -> Vec<Property> {
    let mut out: Vec<Property> = properties
        .iter()
        .filter(|p| p.name.as_ref() != "textures")
        .cloned()
        .collect();
    if !value.is_empty() {
        out.push(Property {
            name: "textures".into(),
            value: value.into(),
            signature: signature.map(Into::into),
        });
    }
    out
}

impl Player {
    pub fn refresh_skin(self: &Arc<Self>) {
        let world = self.world();
        let id = self.gameprofile.id;
        let Some(registered) = world.get_player_by_uuid(id) else {
            return;
        };
        if !Arc::ptr_eq(&registered, self) {
            return;
        }

        world.broadcast_editioned_all_worlds(
            &CRemovePlayerInfo::new(&[id]),
            &CPlayerList {
                action: CPlayerList::ACTION_REMOVE,
                entries: vec![PlayerListEntry {
                    uuid: id,
                    entity_unique_id: VarLong(i64::from(self.entity_id())),
                    username: self.gameprofile.name.clone(),
                    xuid: String::new(),
                    platform_chat_id: String::new(),
                    build_platform: BuildPlatform::Unknown,
                    skin: Skin::steve(),
                    is_teacher: false,
                    is_host: false,
                    is_sub_client: false,
                    player_color: [0; 4],
                }],
            },
        );
        World::with_player_list_entry(self, |java, bedrock| {
            world.broadcast_editioned_all_worlds(java, bedrock);
        });
        if let Some(name) = self.get_tab_list_name() {
            let actions = [PlayerAction::UpdateDisplayName(Some(&name))];
            world.broadcast_packet_all_worlds(&CPlayerInfoUpdate::new(
                PlayerInfoFlags::UPDATE_DISPLAY_NAME.bits(),
                &[InfoEntry {
                    uuid: id,
                    actions: &actions,
                }],
            ));
        }

        world
            .entity_tracker
            .respawn_entity(&(self.clone() as Arc<dyn EntityBase>), &world);
        world.send_player_equipment(self);
        self.restore_vehicle_link(&world);

        if matches!(self.client.as_ref(), ClientPlatform::Java(_))
            && self.has_client_loaded()
            && self.living_entity.health.load() > 0.0
        {
            self.respawn_own_view(&world);
        }
    }

    fn restore_vehicle_link(&self, world: &World) {
        let vehicle = self
            .living_entity
            .entity
            .vehicle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade);
        let Some(vehicle) = vehicle else {
            return;
        };
        let vehicle_entity = vehicle.get_entity();
        let passengers: Vec<VarInt> = vehicle_entity
            .passengers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|p| VarInt(p.get_entity().entity_id))
            .collect();
        let packet = CSetPassengers::new(VarInt(vehicle_entity.entity_id), &passengers);
        if let Some(tracked) = world
            .entity_tracker
            .get_tracked_entity(vehicle_entity.entity_id)
        {
            tracked.send_to_tracking_players(&packet, world);
        }
        self.try_send_client_packet(&packet);
    }

    fn respawn_own_view(&self, world: &World) {
        let screen_open = {
            let current = self
                .current_screen_handler
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::as_ptr(&current).cast::<()>()
                != Arc::as_ptr(&self.player_screen_handler).cast::<()>()
        };
        if screen_open {
            self.close_handled_screen();
        }

        let gamemode = self.gamemode.load();
        let last_death_location = self
            .last_death_location
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let entity = self.get_entity();
        self.try_send_client_packet(&CRespawn::new(
            PlayerSpawnData::new(
                world.dimension.clone(),
                biome::hash_seed(world.level.seed.0),
                gamemode as u8,
                self.previous_gamemode.load().map_or(-1, |g| g as i8),
                false,
                world.is_flat(),
                last_death_location,
                VarInt(
                    entity
                        .portal_cooldown
                        .load(std::sync::atomic::Ordering::Relaxed) as i32,
                ),
                world.sea_level.into(),
            ),
            CRespawn::KEEP_ALL_DATA,
        ));

        self.send_abilities_update();
        self.send_permission_lvl_update();
        self.enqueue_set_held_item_packet(&CSetSelectedSlot::new(
            self.inventory.get_selected_slot() as i8,
        ));
        self.player_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sync_state();
        self.send_health();
        self.try_send_client_packet(&CSetExperience::new(
            self.experience_progress.load().clamp(0.0, 1.0),
            self.experience_level
                .load(std::sync::atomic::Ordering::Relaxed)
                .into(),
            self.experience_points
                .load(std::sync::atomic::Ordering::Relaxed)
                .into(),
        ));
        let effects: Vec<_> = self
            .living_entity
            .active_effects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for effect in &effects {
            self.send_effect(effect);
        }
        let has_post_effects = !self
            .post_effects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty();
        if has_post_effects {
            self.send_post_effects();
        }
        entity.refresh_synced_data();

        self.send_teleport(entity.pos.load(), entity.yaw.load(), entity.pitch.load());
        if let ClientPlatform::Java(client) = self.client.as_ref()
            && client.version.load() >= JavaMinecraftVersion::V_1_20_2
        {
            self.try_send_client_packet(&CGameEvent::new(GameEvent::StartWaitingChunks, 0.0));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prop(name: &str, value: &str, signature: Option<&str>) -> Property {
        Property {
            name: name.into(),
            value: value.into(),
            signature: signature.map(Into::into),
        }
    }

    #[test]
    fn replaces_only_the_textures_property() {
        let before = vec![
            prop("textures", "old", Some("oldsig")),
            prop("other", "keep", None),
            prop("textures", "dupe", None),
        ];
        let after = replace_textures(&before, "new".into(), Some("sig".into()));
        assert_eq!(after.len(), 2);
        assert_eq!(after[0].name.as_ref(), "other");
        assert_eq!(after[1].value.as_ref(), "new");
        assert_eq!(after[1].signature.as_deref(), Some("sig"));
    }

    #[test]
    fn empty_value_removes_textures() {
        let before = vec![
            prop("textures", "old", Some("s")),
            prop("other", "keep", None),
        ];
        let after = replace_textures(&before, String::new(), None);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].name.as_ref(), "other");
    }

    #[test]
    fn adds_textures_when_missing() {
        let after = replace_textures(&[], "v".into(), None);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].name.as_ref(), "textures");
        assert!(after[0].signature.is_none());
    }
}
