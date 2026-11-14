use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

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

pub const MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub struct SkinRefresh {
    last: Option<Instant>,
    viewers_pending: bool,
    own_view_pending: bool,
}

impl SkinRefresh {
    fn request(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < MIN_REFRESH_INTERVAL)
        {
            self.viewers_pending = true;
            return false;
        }
        self.last = Some(now);
        self.viewers_pending = false;
        self.own_view_pending = true;
        true
    }

    fn due(&mut self, now: Instant) -> bool {
        if !self.viewers_pending {
            return false;
        }
        let ready = self
            .last
            .is_none_or(|last| now.saturating_duration_since(last) >= MIN_REFRESH_INTERVAL);
        if ready {
            self.last = Some(now);
            self.viewers_pending = false;
            self.own_view_pending = true;
        }
        ready
    }

    const fn idle(&self) -> bool {
        !self.viewers_pending && !self.own_view_pending
    }
}

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

enum OwnView {
    Now,
    Later,
    Skip,
}

impl Player {
    pub fn refresh_skin(self: &Arc<Self>) {
        let world = self.world();
        let registered = world
            .get_player_by_uuid(self.gameprofile.id)
            .is_some_and(|p| Arc::ptr_eq(&p, self));
        if !registered {
            return;
        }
        let due = self
            .skin_refresh
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .request(Instant::now());
        if due {
            self.refresh_viewers(&world);
            self.try_refresh_own_view(&world);
        }
    }

    pub(super) fn tick_skin_refresh(&self) {
        let (viewers, own) = {
            let Ok(mut state) = self.skin_refresh.try_lock() else {
                return;
            };
            if state.idle() {
                return;
            }
            (state.due(Instant::now()), state.own_view_pending)
        };
        let world = self.world();
        let Some(me) = world.get_player_by_uuid(self.gameprofile.id) else {
            return;
        };
        if !std::ptr::eq(Arc::as_ptr(&me), self) {
            return;
        }
        if viewers {
            me.refresh_viewers(&world);
        }
        if viewers || own {
            me.try_refresh_own_view(&world);
        }
    }

    fn refresh_viewers(self: &Arc<Self>, world: &World) {
        let id = self.gameprofile.id;
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

        if self.living_entity.health.load() <= 0.0 {
            return;
        }
        world
            .entity_tracker
            .respawn_entity(&(self.clone() as Arc<dyn EntityBase>), world);
        if let Some((vehicle_id, passengers)) = self.vehicle_link()
            && let Some(tracked) = world.entity_tracker.get_tracked_entity(vehicle_id)
        {
            tracked.send_to_tracking_players(
                &CSetPassengers::new(VarInt(vehicle_id), &passengers),
                world,
            );
        }
    }

    fn own_view(&self) -> OwnView {
        if !matches!(self.client.as_ref(), ClientPlatform::Java(_))
            || self.living_entity.health.load() <= 0.0
        {
            return OwnView::Skip;
        }
        if !self.has_client_loaded()
            || self.screen_open()
            || self.is_sleeping()
            || self.living_entity.entity.is_fall_flying()
            || self.camera_target_id.load().is_some()
        {
            return OwnView::Later;
        }
        OwnView::Now
    }

    fn try_refresh_own_view(&self, world: &World) {
        let view = self.own_view();
        let mut state = self
            .skin_refresh
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match view {
            OwnView::Later => {}
            OwnView::Skip => state.own_view_pending = false,
            OwnView::Now => {
                state.own_view_pending = false;
                drop(state);
                self.respawn_own_view(world);
                self.resend_own_links();
            }
        }
    }

    fn screen_open(&self) -> bool {
        let current = self
            .current_screen_handler
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::as_ptr(&current).cast::<()>() != Arc::as_ptr(&self.player_screen_handler).cast::<()>()
    }

    fn vehicle_link(&self) -> Option<(i32, Vec<VarInt>)> {
        let vehicle = self
            .living_entity
            .entity
            .vehicle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)?;
        let vehicle_entity = vehicle.get_entity();
        let passengers: Vec<VarInt> = vehicle_entity
            .passengers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|p| VarInt(p.get_entity().entity_id))
            .collect();
        Some((vehicle_entity.entity_id, passengers))
    }

    fn resend_own_links(&self) {
        if let Some((vehicle_id, passengers)) = self.vehicle_link() {
            self.try_send_client_packet(&CSetPassengers::new(VarInt(vehicle_id), &passengers));
        }
        let own: Vec<VarInt> = self
            .living_entity
            .entity
            .passengers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|p| VarInt(p.get_entity().entity_id))
            .collect();
        if !own.is_empty() {
            self.try_send_client_packet(&CSetPassengers::new(VarInt(self.entity_id()), &own));
        }
    }

    fn respawn_own_view(&self, world: &World) {
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

        world.send_level_info(self);
        self.set_client_loaded(false);
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

    #[test]
    fn refreshes_are_throttled_and_coalesced() {
        let start = Instant::now();
        let mut state = SkinRefresh::default();
        assert!(state.idle());
        assert!(state.request(start));
        assert!(state.own_view_pending);
        state.own_view_pending = false;
        assert!(!state.request(start + Duration::from_millis(100)));
        assert!(!state.request(start + Duration::from_millis(200)));
        assert!(state.viewers_pending);
        assert!(!state.due(start + Duration::from_millis(500)));
        assert!(state.due(start + MIN_REFRESH_INTERVAL + Duration::from_millis(100)));
        assert!(!state.viewers_pending);
        assert!(state.own_view_pending);
        assert!(!state.due(start + Duration::from_secs(5)));
        state.own_view_pending = false;
        assert!(state.idle());
        assert!(state.request(start + Duration::from_secs(5)));
    }
}
