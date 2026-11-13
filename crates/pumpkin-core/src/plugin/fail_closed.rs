use std::sync::Arc;

use pumpkin_util::text::TextComponent;

use crate::{
    entity::player::Player,
    net::DisconnectReason,
    plugin::{
        Payload,
        api::events::{player::player_join::PlayerJoinEvent, server::packet::PacketReceivedEvent},
    },
};

const KICK_MESSAGE: &str = "Something went wrong while checking your connection. Please rejoin.";

/// Cancels `event` when it is cancellable and returns the player to disconnect when it is a
/// join or received packet, for a handler of a `fail_closed` plugin that failed.
pub fn contain(event: &mut dyn Payload) -> Option<Arc<Player>> {
    if let Some(cancellable) = event.as_cancellable_mut() {
        cancellable.set_cancelled(true);
    }
    player_to_disconnect(event)
}

/// The player behind a join or received packet event.
#[must_use]
pub fn player_to_disconnect(event: &dyn Payload) -> Option<Arc<Player>> {
    if let Some(join) = event.downcast_ref::<PlayerJoinEvent>() {
        return Some(join.player.clone());
    }
    event
        .downcast_ref::<PacketReceivedEvent>()
        .map(|packet| packet.player.clone())
}

/// Disconnects `player` after a failed handler of the plugin `plugin`.
pub fn disconnect(player: &Player, plugin: &str) {
    tracing::warn!(
        "Disconnecting {} because a handler of fail-closed plugin {plugin} failed",
        player.gameprofile.name
    );
    player.kick(DisconnectReason::Kicked, &TextComponent::text(KICK_MESSAGE));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{
        Cancellable,
        api::events::server::{
            server_command::ServerCommandEvent,
            server_load::{LoadType, ServerLoadEvent},
        },
    };

    #[test]
    fn cancellable_events_are_cancelled() {
        let mut event = ServerCommandEvent::new("stop".to_string());
        assert!(contain(&mut event).is_none());
        assert!(event.cancelled());
    }

    #[test]
    fn other_events_pass_through_untouched() {
        let mut event = ServerLoadEvent::new(LoadType::Startup);
        assert!(event.as_cancellable_mut().is_none());
        assert!(contain(&mut event).is_none());
    }
}
